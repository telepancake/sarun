// Engine-held PTY (D7/D9). The engine spawns a command on a pseudo-terminal it
// OWNS (portable-pty), and muxes the PTY master ↔ a client over the typed
// FRAME_PTY_* frames defined in frames.rs. This is the OTHER half of the
// runner-held PTY (`sarun run -p`): there the launching terminal holds the tty;
// here the ENGINE holds it and a remote ratatui UI client renders/drives it as a
// tui-term pane over the control socket.
//
// SCOPE (honest): this first cut spawns the command DIRECTLY on the PTY (no
// bwrap/overlay box around it) and proves the full mux+render+input loop end to
// end. Wrapping the PTY child in a bwrap'd overlay box (so it is a real captured
// box like a normal `run`) reuses the SAME frame mux unchanged and is the
// documented follow-on (see DESIGN.md D9 / the test file header). Even now the
// master is TEE'd to an optional sink writer so a session can be recorded.
//
// The loop is deliberately fd/poll based (no async runtime) to match the rest of
// the engine, and is unit-tested headlessly in `#[cfg(test)]` against a real
// child process.

use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use portable_pty::CommandBuilder;
use portable_pty::PtySize;
use portable_pty::native_pty_system;

use crate::frames;

/// A sink the PTY master bytes are tee'd into (recording the session). Any
/// `Write` works; the control layer passes the box stdout sink, tests pass a
/// shared `Vec<u8>` buffer.
pub type Sink = Arc<Mutex<dyn Write + Send>>;

/// Drive an engine-held PTY for `argv` to EOF, muxing it over `client`.
///
/// Behavior:
///   * spawns argv on a fresh PTY (rows×cols), engine holds the master;
///   * a reader thread pumps master → FRAME_PTY_DATA frames to `client` AND, if
///     present, tees the raw bytes to `sink` (the recording);
///   * the calling thread reads frames FROM `client`: FRAME_PTY_DATA → write the
///     bytes to the master (keystrokes reach the child); FRAME_PTY_RESIZE →
///     resize the PTY;
///   * on child exit the master hits EOF, a FRAME_PTY_EOF is sent, both
///     directions wind down.
///
/// `client` must be readable AND writable (a UnixStream, or in tests a
/// socketpair end). Returns the child's exit code (best-effort; 0 if unknown).
pub fn serve_pty<C>(argv: &[String], rows: u16, cols: u16,
                    mut client: C, sink: Option<Sink>) -> i32
where
    C: Read + Write + Send + 'static + CloneStream,
{
    if argv.is_empty() {
        return 2;
    }
    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize {
        rows, cols, pixel_width: 0, pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => return 1,
    };
    let mut cmd = CommandBuilder::new(&argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            // Surface the real reason to the client (most often "executable
            // not found in PATH"): one DATA frame with the human-readable
            // error, then EOF. Without this the client just sees "(exited)"
            // on an empty pane and can't tell whether it was a missing
            // binary, a permission error, or anything else.
            let msg = format!("pty: spawn {:?} failed: {}\r\n", argv[0], e);
            let _ = client.write_all(&frames::encode(
                frames::FRAME_PTY_DATA, msg.as_bytes()));
            let _ = client.write_all(&frames::encode(frames::FRAME_PTY_EOF, &[]));
            let _ = client.flush();
            return 127;
        }
    };
    // Drop the slave handle so the master sees EOF once the child closes its tty
    // (otherwise the reader never ends).
    drop(pair.slave);

    let mut master_writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return 1,
    };
    let mut master_reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return 1,
    };

    // The master must outlive both directions; keep it owned by the resizer so we
    // can apply FRAME_PTY_RESIZE. portable-pty's MasterPty::resize takes &self.
    let master = Arc::new(Mutex::new(pair.master));

    let done = Arc::new(AtomicBool::new(false));

    // ── OUTPUT direction: master → client (FRAME_PTY_DATA) + tee to sink ──────
    let mut out_client = client.clone_stream();
    let done_out = done.clone();
    let out = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match master_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let data = &buf[..n];
            if let Some(s) = &sink {
                let _ = s.lock().unwrap().write_all(data);
            }
            let frame = frames::encode(frames::FRAME_PTY_DATA, data);
            if out_client.write_all(&frame).is_err() {
                break;
            }
            let _ = out_client.flush();
        }
        // Child closed the tty → tell the client and signal the input loop.
        done_out.store(true, Ordering::SeqCst);
        let _ = out_client.write_all(&frames::encode(frames::FRAME_PTY_EOF, &[]));
        let _ = out_client.flush();
        // Unblock the input loop: shut down the read side of the shared socket so
        // its blocking `read` returns 0 (the client never closes first — it's
        // waiting on US). Harmless if the client already closed.
        out_client.shutdown_read();
    });

    // ── INPUT direction: client → master (keystrokes) + resize ───────────────
    // We read frames from the client until the child is gone (done) and the
    // client closes, applying DATA (write to master) and RESIZE (resize PTY).
    let mut acc: Vec<u8> = vec![];
    let mut rbuf = [0u8; 8192];
    loop {
        if done.load(Ordering::SeqCst) {
            // The child has exited; drain anything already buffered, then stop —
            // no point blocking on client reads for a dead PTY.
            let (frames_v, used) = frames::decode(&acc);
            acc.drain(..used);
            apply_input(&frames_v, &mut master_writer, &master);
            break;
        }
        let n = match client.read(&mut rbuf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        acc.extend_from_slice(&rbuf[..n]);
        let (frames_v, used) = frames::decode(&acc);
        acc.drain(..used);
        apply_input(&frames_v, &mut master_writer, &master);
    }

    let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(0);
    // Drop the master write side; the read side closed when the child closed its
    // slave (that EOF is what ended the output thread). Join it to be tidy.
    drop(master_writer);
    let _ = out.join();
    let _ = master; // keep the master alive until here (resize needs it)
    code
}

/// Apply a batch of client→engine input frames to the PTY master.
fn apply_input(frames_v: &[(u8, Vec<u8>)], master_writer: &mut Box<dyn Write + Send>,
               master: &Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>) {
    for (ft, payload) in frames_v {
        match *ft {
            x if x == frames::FRAME_PTY_DATA => {
                let _ = master_writer.write_all(payload);
                let _ = master_writer.flush();
            }
            x if x == frames::FRAME_PTY_RESIZE => {
                if let Some((rows, cols)) = frames::pty_resize_parse(payload) {
                    if let Ok(m) = master.lock() {
                        let _ = m.resize(PtySize {
                            rows, cols, pixel_width: 0, pixel_height: 0,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// A stream we can clone so the output thread and input loop share one socket,
/// and whose READ side we can shut down to unblock a blocked reader. (When the
/// PTY child exits, the output thread shuts down the client read side so the
/// input loop's blocking `read` returns instead of deadlocking — neither side of
/// the muxed connection would otherwise close first.)
pub trait CloneStream {
    fn clone_stream(&self) -> Self;
    fn shutdown_read(&self);
}

impl CloneStream for std::os::unix::net::UnixStream {
    fn clone_stream(&self) -> Self {
        self.try_clone().expect("UnixStream::try_clone")
    }
    fn shutdown_read(&self) {
        let _ = self.shutdown(std::net::Shutdown::Read);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// Build a vt100 parser + render the accumulated PTY-DATA stream through
    /// tui-term into a ratatui TestBackend, returning the flattened screen text.
    /// This is exactly the client-side render path the UI pane uses.
    fn render(bytes: &[u8], rows: u16, cols: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use tui_term::vt100;
        use tui_term::widget::PseudoTerminal;
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        let backend = TestBackend::new(cols, rows);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            f.render_widget(PseudoTerminal::new(parser.screen()), f.area());
        })
        .expect("draw");
        format!("{}", term.backend())
    }

    /// Collect FRAME_PTY_DATA payloads from a raw frame stream (the bytes a UI
    /// client would receive), concatenated — i.e. reconstruct the PTY output.
    fn collect_pty_data(stream: &[u8]) -> (Vec<u8>, bool) {
        let (frames_v, _) = frames::decode(stream);
        let mut out = vec![];
        let mut eof = false;
        for (ft, p) in frames_v {
            if ft == frames::FRAME_PTY_DATA {
                out.extend_from_slice(&p);
            } else if ft == frames::FRAME_PTY_EOF {
                eof = true;
            }
        }
        (out, eof)
    }

    fn drain_to_eof(mut s: UnixStream) -> Vec<u8> {
        use std::io::Read;
        let mut buf = vec![];
        let _ = s.read_to_end(&mut buf);
        buf
    }

    // OUTPUT: a real child's stdout, muxed as FRAME_PTY_DATA, reconstructed and
    // rendered through vt100+tui-term, must CONTAIN the child's marker.
    #[test]
    fn engine_pty_output_renders_marker() {
        let (engine_end, client_end) = UnixStream::pair().expect("socketpair");
        let h = std::thread::spawn(move || {
            serve_pty(
                &["sh".into(), "-c".into(),
                  "echo MARKER-ENGINE-PTY; echo second-row".into()],
                24, 80, engine_end, None)
        });
        let stream = drain_to_eof(client_end);
        let _ = h.join();
        let (data, eof) = collect_pty_data(&stream);
        assert!(eof, "engine sent FRAME_PTY_EOF on child exit");
        let screen = render(&data, 24, 80);
        assert!(screen.contains("MARKER-ENGINE-PTY"),
                "rendered pane missing child stdout marker; screen=\n{screen}");
        assert!(screen.contains("second-row"),
                "rendered pane missing second line; screen=\n{screen}");
    }

    // CONTROL SEQUENCES: vt100 truly emulates (no raw escape codes on the grid).
    #[test]
    fn engine_pty_emulates_escapes() {
        let (engine_end, client_end) = UnixStream::pair().expect("socketpair");
        let h = std::thread::spawn(move || {
            serve_pty(
                &["printf".into(),
                  "\\033[2J\\033[H\\033[1mBOLD-ENGINE\\033[0m end".into()],
                24, 80, engine_end, None)
        });
        let stream = drain_to_eof(client_end);
        let _ = h.join();
        let (data, _) = collect_pty_data(&stream);
        let screen = render(&data, 24, 80);
        assert!(screen.contains("BOLD-ENGINE"), "screen=\n{screen}");
        assert!(!screen.contains("\u{1b}["), "raw escape leaked: screen=\n{screen}");
    }

    // INPUT: a FRAME_PTY_DATA keystroke frame written to the engine reaches the
    // child (it reads a line and echoes a readback marker we then find).
    #[test]
    fn engine_pty_input_reaches_child() {
        let (engine_end, mut client_end) = UnixStream::pair().expect("socketpair");
        let h = std::thread::spawn(move || {
            serve_pty(
                &["sh".into(), "-c".into(),
                  "read line; echo GOT:$line".into()],
                24, 80, engine_end, None)
        });
        // Drive a keystroke as the client would: a FRAME_PTY_DATA frame.
        let key = frames::encode(frames::FRAME_PTY_DATA, b"ping-from-ui\n");
        client_end.write_all(&key).expect("send key");
        client_end.flush().ok();
        let stream = drain_to_eof(client_end);
        let _ = h.join();
        let (data, _) = collect_pty_data(&stream);
        let screen = render(&data, 24, 80);
        assert!(screen.contains("GOT:ping-from-ui"),
                "child never received the keystroke; screen=\n{screen}");
    }

    // RESIZE: a FRAME_PTY_RESIZE frame changes the child's tty size — the child
    // reports its rows via `stty size`, and the new value lands on screen.
    #[test]
    fn engine_pty_resize_applies() {
        let (engine_end, mut client_end) = UnixStream::pair().expect("socketpair");
        let h = std::thread::spawn(move || {
            // Start at 24 rows, then we resize to 40 before stty reads it.
            serve_pty(
                &["sh".into(), "-c".into(),
                  // small sleep so the resize frame is applied before stty runs
                  "sleep 0.3; stty size".into()],
                24, 80, engine_end, None)
        });
        let rz = frames::encode(frames::FRAME_PTY_RESIZE,
                                &frames::pty_resize_payload(40, 100));
        client_end.write_all(&rz).expect("send resize");
        client_end.flush().ok();
        let stream = drain_to_eof(client_end);
        let _ = h.join();
        let (data, _) = collect_pty_data(&stream);
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("40 100"),
                "resize did not reach the child tty; stty output=\n{text}");
    }

    // SPAWN FAILURE: argv[0] not on PATH. The mux must send a DATA frame with
    // the human-readable error and an EOF frame — without this the client sees
    // an empty pane labeled "(exited)" and can't tell what went wrong.
    #[test]
    fn engine_pty_surfaces_spawn_error() {
        let (engine_end, client_end) = UnixStream::pair().expect("socketpair");
        let h = std::thread::spawn(move || {
            serve_pty(&["definitely-not-a-real-binary-xyzzy".into()],
                      24, 80, engine_end, None)
        });
        let stream = drain_to_eof(client_end);
        let rc = h.join().unwrap();
        assert_eq!(rc, 127, "spawn-failure should return 127");
        let (data, eof) = collect_pty_data(&stream);
        assert!(eof, "EOF frame must follow a failed spawn");
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("definitely-not-a-real-binary-xyzzy"),
                "error text should name the bad executable; got {text:?}");
        assert!(text.contains("pty: spawn"),
                "error text should be tagged; got {text:?}");
    }

    // TEE: the optional sink records the session bytes (so an engine PTY box can
    // still capture its output), identical to what the client receives.
    #[test]
    fn engine_pty_tees_to_sink() {
        let (engine_end, client_end) = UnixStream::pair().expect("socketpair");
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(vec![]));
        // Adapt Vec<u8> behind the Sink trait object.
        let sink_dyn: Sink = {
            struct W(Arc<Mutex<Vec<u8>>>);
            impl Write for W {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(b);
                    Ok(b.len())
                }
                fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
            }
            Arc::new(Mutex::new(W(sink.clone())))
        };
        let h = std::thread::spawn(move || {
            serve_pty(
                &["sh".into(), "-c".into(), "echo RECORDED-MARKER".into()],
                24, 80, engine_end, Some(sink_dyn))
        });
        let _ = drain_to_eof(client_end);
        let _ = h.join();
        let recorded = sink.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&recorded);
        assert!(text.contains("RECORDED-MARKER"),
                "sink did not record the session; got=\n{text}");
    }
}
