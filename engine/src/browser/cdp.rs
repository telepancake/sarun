// A synchronous Chrome DevTools Protocol client (DESIGN-cellulose.md C1/C2).
//
// Ported from the `cellulose/` Python prototype's `CDP` class. The transport
// is abstracted behind a reader/writer pair so the websocket path used today
// can be swapped for `--remote-debugging-pipe` later with no client change.
//
// The client owns a writer (behind a mutex) and spawns a reader thread that
// correlates responses to blocked callers by message id and queues everything
// else as an event. `call` blocks with a timeout; once the transport closes,
// every pending and future `call` fails fast instead of hanging to timeout.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// The read half of a CDP transport: blocking, one whole message per call,
/// `Ok(None)` on clean close.
pub trait CdpReader: Send {
    fn recv(&mut self) -> io::Result<Option<String>>;
}

/// The write half of a CDP transport.
pub trait CdpWriter: Send {
    fn send(&mut self, msg: &str) -> io::Result<()>;
}

// ── the client ────────────────────────────────────────────────────────────

pub struct Cdp {
    writer: Mutex<Box<dyn CdpWriter>>,
    next_id: AtomicU64,
    inner: Arc<Inner>,
}

struct Inner {
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    events: Mutex<Vec<Value>>,
    closed: AtomicBool,
    /// A fd the reader pokes (one byte) whenever an event lands or the
    /// connection closes, so an event loop can `poll` on it and wake the
    /// instant something arrives instead of timing out. -1 = unset.
    wake_fd: std::sync::atomic::AtomicI32,
}

impl Inner {
    /// Mark closed and drop every pending sender, so blocked callers unblock
    /// with a disconnect rather than waiting out their timeout.
    fn fail_all(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
        self.poke();
    }

    /// Wake a registered event-loop fd (non-blocking; a full pipe already
    /// means "pending", so a dropped byte is harmless).
    fn poke(&self) {
        let fd = self.wake_fd.load(Ordering::Relaxed);
        if fd >= 0 {
            let b = [1u8];
            unsafe { libc::write(fd, b.as_ptr() as *const _, 1) };
        }
    }
}

impl Cdp {
    /// Build a client over a reader/writer pair and start the reader thread.
    pub fn new(mut reader: Box<dyn CdpReader>, writer: Box<dyn CdpWriter>) -> Arc<Self> {
        let inner = Arc::new(Inner {
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            wake_fd: std::sync::atomic::AtomicI32::new(-1),
        });
        let rinner = inner.clone();
        std::thread::Builder::new()
            .name("cdp-reader".into())
            .spawn(move || {
                loop {
                    match reader.recv() {
                        Ok(Some(text)) => {
                            let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                                continue; // ignore non-JSON noise
                            };
                            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                                let tx = rinner.pending.lock().unwrap().remove(&id);
                                if let Some(tx) = tx {
                                    let _ = tx.send(msg);
                                }
                            } else {
                                rinner.events.lock().unwrap().push(msg);
                                rinner.poke();
                            }
                        }
                        Ok(None) | Err(_) => {
                            rinner.fail_all();
                            return;
                        }
                    }
                }
            })
            .expect("spawn cdp reader");
        Arc::new(Self {
            writer: Mutex::new(writer),
            next_id: AtomicU64::new(1),
            inner,
        })
    }

    /// Issue a CDP command and block for its result (or error/timeout). A
    /// `session` routes the command to an attached target (flat sessions).
    pub fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        if self.inner.closed.load(Ordering::SeqCst) {
            anyhow::bail!("{method}: connection closed");
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx): (Sender<Value>, Receiver<Value>) = mpsc::channel();
        let mut req = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            req["sessionId"] = json!(s);
        }
        // Register before sending so a fast reply can never race ahead of us.
        self.inner.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.writer.lock().unwrap().send(&req.to_string()) {
            self.inner.pending.lock().unwrap().remove(&id);
            self.inner.fail_all();
            anyhow::bail!("{method}: send failed: {e}");
        }
        match rx.recv_timeout(timeout) {
            Ok(msg) => {
                if let Some(err) = msg.get("error") {
                    let m = err.get("message").and_then(Value::as_str).unwrap_or("?");
                    anyhow::bail!("{method}: {m}");
                }
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.inner.pending.lock().unwrap().remove(&id);
                anyhow::bail!("{method}: timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("{method}: connection closed")
            }
        }
    }

    /// Take and clear the queued events (methods without an id).
    pub fn drain_events(&self) -> Vec<Value> {
        std::mem::take(&mut *self.inner.events.lock().unwrap())
    }

    /// Register a fd to be poked (one byte) whenever an event arrives or the
    /// connection closes — for an event loop that wants to `poll` and wake
    /// immediately rather than spin on a timeout.
    pub fn set_wake_fd(&self, fd: std::os::fd::RawFd) {
        self.inner.wake_fd.store(fd, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

// ── pipe transport (--remote-debugging-pipe) ────────────────────────────────

/// CDP over two fds, framed as NUL-delimited JSON — Chromium's
/// `--remote-debugging-pipe` protocol. This is the box transport
/// (DESIGN-cellulose.md C1): the engine passes a socketpair/pipe into the box
/// exactly like the existing conn-fd, so no port and no netns dial is needed.
/// `read_fd` receives from Chromium (its fd 4), `write_fd` sends to it (fd 3).
pub fn pipe_transport(
    read_fd: std::os::fd::RawFd,
    write_fd: std::os::fd::RawFd,
) -> (Box<dyn CdpReader>, Box<dyn CdpWriter>) {
    use std::os::fd::FromRawFd;
    let r = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let w = unsafe { std::fs::File::from_raw_fd(write_fd) };
    (
        Box::new(PipeReader {
            r: BufReader::new(r),
        }),
        Box::new(PipeWriter { w }),
    )
}

struct PipeReader {
    r: BufReader<std::fs::File>,
}
struct PipeWriter {
    w: std::fs::File,
}

impl CdpReader for PipeReader {
    fn recv(&mut self) -> io::Result<Option<String>> {
        use std::io::BufRead;
        let mut buf = Vec::new();
        let n = self.r.read_until(0, &mut buf)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }
}

impl CdpWriter for PipeWriter {
    fn send(&mut self, msg: &str) -> io::Result<()> {
        self.w.write_all(msg.as_bytes())?;
        self.w.write_all(&[0])?;
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, Sender};

    #[test]
    fn pipe_frames_nul_delimited_json() {
        use std::os::fd::IntoRawFd;
        // A connected socketpair: bytes written to the writer end surface on
        // the reader end. pipe_transport(read=a, write=b) → reading a returns
        // what was sent to b.
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (mut r, mut w) = pipe_transport(a.into_raw_fd(), b.into_raw_fd());
        w.send("{\"id\":1}").unwrap();
        w.send("{\"id\":2}").unwrap();
        assert_eq!(r.recv().unwrap().as_deref(), Some("{\"id\":1}"));
        assert_eq!(r.recv().unwrap().as_deref(), Some("{\"id\":2}"));
    }

    // A mock transport backed by channels, so the client's correlation logic
    // is testable without a browser: the "server" reads what the client sent
    // and pushes replies/events back.
    struct MockReader {
        rx: Receiver<String>,
    }
    struct MockWriter {
        tx: Sender<String>,
    }
    impl CdpReader for MockReader {
        fn recv(&mut self) -> io::Result<Option<String>> {
            match self.rx.recv() {
                Ok(s) => Ok(Some(s)),
                Err(_) => Ok(None),
            }
        }
    }
    impl CdpWriter for MockWriter {
        fn send(&mut self, msg: &str) -> io::Result<()> {
            self.tx
                .send(msg.to_string())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
    }

    fn mock_pair() -> (Arc<Cdp>, Sender<String>, Receiver<String>) {
        let (to_client, from_server) = mpsc::channel::<String>(); // server -> client reader
        let (to_server, from_client) = mpsc::channel::<String>(); // client -> server
        let cdp = Cdp::new(
            Box::new(MockReader { rx: from_server }),
            Box::new(MockWriter { tx: to_server }),
        );
        (cdp, to_client, from_client)
    }

    #[test]
    fn call_correlates_result_by_id() {
        let (cdp, to_client, from_client) = mock_pair();
        let c2 = cdp.clone();
        // Fake server: reply to each request's id with a result echoing method.
        let server = std::thread::spawn(move || {
            let sent = from_client.recv().unwrap();
            let v: Value = serde_json::from_str(&sent).unwrap();
            let id = v["id"].as_u64().unwrap();
            let reply = json!({ "id": id, "result": { "ok": v["method"] } });
            to_client.send(reply.to_string()).unwrap();
        });
        let res = c2
            .call("Foo.bar", json!({}), None, Duration::from_secs(2))
            .unwrap();
        assert_eq!(res["ok"], "Foo.bar");
        server.join().unwrap();
    }

    #[test]
    fn call_surfaces_protocol_error() {
        let (cdp, to_client, from_client) = mock_pair();
        let c2 = cdp.clone();
        std::thread::spawn(move || {
            let sent = from_client.recv().unwrap();
            let v: Value = serde_json::from_str(&sent).unwrap();
            let id = v["id"].as_u64().unwrap();
            let reply = json!({ "id": id, "error": { "message": "boom" } });
            to_client.send(reply.to_string()).unwrap();
        });
        let err = c2
            .call("Bad.method", json!({}), None, Duration::from_secs(2))
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn events_are_queued() {
        let (cdp, to_client, _from_client) = mock_pair();
        to_client
            .send(json!({ "method": "Page.loadEventFired", "params": {} }).to_string())
            .unwrap();
        // Give the reader thread a moment to enqueue.
        std::thread::sleep(Duration::from_millis(100));
        let evs = cdp.drain_events();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["method"], "Page.loadEventFired");
        assert!(cdp.drain_events().is_empty());
    }

    // End-to-end against a real headless Chromium: proves the Rust websocket
    // client handshakes (Chromium's origin check accepts our no-Origin
    // request) and the CDP round-trip works. Ignored by default — needs a
    // browser; run with `--ignored`. Path via $CELLULOSE_BROWSER.
    #[test]
    fn call_fails_fast_once_closed() {
        let (cdp, to_client, _from_client) = mock_pair();
        drop(to_client); // server closes → reader sees None → fail_all
        std::thread::sleep(Duration::from_millis(100));
        let err = cdp
            .call("X.y", json!({}), None, Duration::from_secs(5))
            .unwrap_err();
        assert!(err.to_string().contains("closed"));
    }
}
