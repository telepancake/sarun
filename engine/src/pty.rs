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

// The engine PTY proxy is a "good citizen" terminal as far as the child is
// concerned: it answers DSR (cursor-position) and Device-Attribute queries
// the same way a real terminal would. Without this, reedline-based
// interactive shells (brush -i, anything else built on crossterm) time out
// on init waiting for ESC[6n to come back. The UI's tui-term front end
// renders the bytes but can't reply across the mux, so the engine does it.
struct ProxyCallbacks {
    /// Bytes queued for the input loop to write back to the PTY master
    /// (DSR / DA1 replies). Drained after every process() call.
    pending: Vec<u8>,
}

impl tui_term::vt100::Callbacks for ProxyCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut tui_term::vt100::Screen,
        intro: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // CSI 6 n  →  ESC [ row ; col R   (DSR — cursor position report).
        //   vt100's (row,col) is 0-based; the wire reply is 1-based.
        // CSI 5 n  →  ESC [ 0 n           (DSR — terminal status OK).
        // CSI c    →  ESC [ ? 1 ; 0 c     (DA1 — VT100, no options) — the
        //   minimum reedline / crossterm need to stop waiting.
        // Anything else with a private intro ('?', '>', '=', '!') is left
        // unanswered; same for unknown final bytes.
        if intro.is_some() { return; }
        match c {
            'n' => match params.first().and_then(|p| p.first()).copied().unwrap_or(0) {
                6 => {
                    let (r, col) = screen.cursor_position();
                    let s = format!("\x1b[{};{}R", r + 1, col + 1);
                    self.pending.extend_from_slice(s.as_bytes());
                }
                5 => self.pending.extend_from_slice(b"\x1b[0n"),
                _ => {}
            }
            'c' => self.pending.extend_from_slice(b"\x1b[?1;0c"),
            _ => {}
        }
    }
}

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
/// `cwd` is the working directory the child is launched in (the UI's $PWD
/// when it sent pty_spawn). `env` is a list of (KEY, VAL) pairs piled on
/// top of the engine's own environment, so the child sees the UI user's
/// SHELL / HOME / PATH instead of the daemon's minimal env.
pub fn serve_pty<C>(argv: &[String], rows: u16, cols: u16,
                    mut client: C, sink: Option<Sink>,
                    cwd: Option<&std::path::Path>,
                    env: &[(String, String)]) -> i32
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
    if let Some(d) = cwd { cmd.cwd(d); }
    for (k, v) in env { cmd.env(k, v); }
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

    let master_writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return 1,
    };
    let mut master_reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return 1,
    };
    // Shared between the input loop (writes raw keystrokes) and the output
    // thread (writes DSR / DA1 replies via the ProxyCallbacks queue).
    let master_writer = Arc::new(Mutex::new(master_writer));

    // The master must outlive both directions; keep it owned by the resizer so we
    // can apply FRAME_PTY_RESIZE. portable-pty's MasterPty::resize takes &self.
    let master = Arc::new(Mutex::new(pair.master));

    let done = Arc::new(AtomicBool::new(false));

    // ── OUTPUT direction: master → client (FRAME_PTY_DATA) + tee to sink ──────
    let mut out_client = client.clone_stream();
    let done_out = done.clone();
    let mw_out = master_writer.clone();
    let out = std::thread::spawn(move || {
        // Shadow vt100 parser so we know the cursor when a DSR query arrives.
        let mut parser = tui_term::vt100::Parser::new_with_callbacks(
            rows, cols, 0, ProxyCallbacks { pending: vec![] });
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
            // Track cursor / catch DSR + DA queries. The callbacks queue
            // their replies; we drain them straight back to the master so
            // the child sees a real terminal answering.
            parser.process(data);
            let replies = std::mem::take(&mut parser.callbacks_mut().pending);
            if !replies.is_empty() {
                if let Ok(mut w) = mw_out.lock() {
                    let _ = w.write_all(&replies);
                    let _ = w.flush();
                }
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
            apply_input(&frames_v, &master_writer, &master);
            break;
        }
        let n = match client.read(&mut rbuf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        acc.extend_from_slice(&rbuf[..n]);
        let (frames_v, used) = frames::decode(&acc);
        acc.drain(..used);
        apply_input(&frames_v, &master_writer, &master);
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
fn apply_input(frames_v: &[(u8, Vec<u8>)],
               master_writer: &Arc<Mutex<Box<dyn Write + Send>>>,
               master: &Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>) {
    for (ft, payload) in frames_v {
        match *ft {
            x if x == frames::FRAME_PTY_DATA => {
                if let Ok(mut w) = master_writer.lock() {
                    let _ = w.write_all(payload);
                    let _ = w.flush();
                }
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
                24, 80, engine_end, None, None, &[])
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
                24, 80, engine_end, None, None, &[])
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
                24, 80, engine_end, None, None, &[])
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
                24, 80, engine_end, None, None, &[])
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

    // TERMINAL QUERIES: a child that writes CSI 6 n (DSR — cursor position) or
    // CSI c (DA1 — device attributes) must receive an actual reply, or
    // reedline-based interactive shells time out on init. The shim asks via
    // tty, then `read` echoes whatever came back; the child sees the reply
    // because the engine writes it onto the master.
    #[test]
    fn engine_pty_answers_cursor_and_device_queries() {
        let (engine_end, client_end) = UnixStream::pair().expect("socketpair");
        let h = std::thread::spawn(move || {
            // Put the tty in raw mode so reads aren't buffered to a newline,
            // emit DSR + DA1, then read EXACTLY 13 bytes back (CSI 1;1 R is
            // 6, CSI ?1;0 c is 7). od dumps the bytes octal-escaped so the
            // test can find them on the muxed output stream. A backgrounded
            // alarm kills the read if no reply arrives — without this a
            // broken engine would deadlock the test instead of failing it.
            serve_pty(
                &["sh".into(), "-c".into(),
                  "stty raw -echo 2>/dev/null; \
                   (sleep 3; kill -ALRM $$) & \
                   printf '\\033[6n\\033[c'; \
                   dd ibs=1 count=13 2>/dev/null | od -An -c | tr -d '\\n' | \
                       awk '{print \"REPLY:\" $0}'; \
                   stty sane 2>/dev/null".into()],
                24, 80, engine_end, None, None, &[])
        });
        let stream = drain_to_eof(client_end);
        let _ = h.join();
        let (data, _) = collect_pty_data(&stream);
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("REPLY:"), "shim never produced readback marker; got {text:?}");
        // od -An -c renders the DSR reply ESC[1;1R as " 033   [   1   ;   1   R",
        // and DA1 reply ESC[?1;0c as " 033   [   ?   1   ;   0   c".
        assert!(text.contains("033   [   1   ;   1   R"),
                "missing DSR cursor-position reply in {text:?}");
        assert!(text.contains("033   [   ?   1   ;   0   c"),
                "missing DA1 device-attribute reply in {text:?}");
    }

    // SPAWN FAILURE: argv[0] not on PATH. The mux must send a DATA frame with
    // the human-readable error and an EOF frame — without this the client sees
    // an empty pane labeled "(exited)" and can't tell what went wrong.
    #[test]
    fn engine_pty_surfaces_spawn_error() {
        let (engine_end, client_end) = UnixStream::pair().expect("socketpair");
        let h = std::thread::spawn(move || {
            serve_pty(&["definitely-not-a-real-binary-xyzzy".into()],
                      24, 80, engine_end, None, None, &[])
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
                24, 80, engine_end, Some(sink_dyn), None, &[])
        });
        let _ = drain_to_eof(client_end);
        let _ = h.join();
        let recorded = sink.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&recorded);
        assert!(text.contains("RECORDED-MARKER"),
                "sink did not record the session; got=\n{text}");
    }
}
