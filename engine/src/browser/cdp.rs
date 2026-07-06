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
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

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
}

impl Inner {
    /// Mark closed and drop every pending sender, so blocked callers unblock
    /// with a disconnect rather than waiting out their timeout.
    fn fail_all(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
    }
}

impl Cdp {
    /// Build a client over a reader/writer pair and start the reader thread.
    pub fn new(mut reader: Box<dyn CdpReader>, writer: Box<dyn CdpWriter>) -> Arc<Self> {
        let inner = Arc::new(Inner {
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        });
        let rinner = inner.clone();
        std::thread::Builder::new()
            .name("cdp-reader".into())
            .spawn(move || loop {
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
                        }
                    }
                    Ok(None) | Err(_) => {
                        rinner.fail_all();
                        return;
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

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

// ── websocket transport ─────────────────────────────────────────────────────

/// Connect to a `ws://host:port/path` DevTools endpoint and return a
/// reader/writer pair. A minimal RFC 6455 client: masked client frames,
/// reassembled server messages, control frames handled. We require an HTTP
/// 101 but do not validate the `Sec-WebSocket-Accept` hash (no sha1 dep; the
/// endpoint is localhost-trusted).
pub fn ws_connect(url: &str) -> io::Result<(Box<dyn CdpReader>, Box<dyn CdpWriter>)> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a ws:// url"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host_port = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    let stream = TcpStream::connect(&host_port)?;
    stream.set_nodelay(true).ok();

    // Handshake.
    let mut key = [0u8; 16];
    rand::Rng::fill(&mut rand::thread_rng(), &mut key[..]);
    use base64::Engine as _;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(key);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key_b64}\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    (&stream).write_all(req.as_bytes())?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let status = read_http_headers(&mut reader)?;
    if !status.contains(" 101") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("websocket upgrade rejected: {status}"),
        ));
    }

    let ws_reader = WsReader { r: reader };
    let ws_writer = WsWriter { w: stream };
    Ok((Box::new(ws_reader), Box::new(ws_writer)))
}

/// Read the status line + headers up to the blank line; return the status line.
fn read_http_headers<R: Read>(r: &mut R) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let n = r.read(&mut one)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof in headers"));
        }
        buf.push(one[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "header too large"));
        }
    }
    let text = String::from_utf8_lossy(&buf);
    Ok(text.lines().next().unwrap_or("").to_string())
}

struct WsReader {
    r: BufReader<TcpStream>,
}

struct WsWriter {
    w: TcpStream,
}

impl CdpReader for WsReader {
    fn recv(&mut self) -> io::Result<Option<String>> {
        read_ws_message(&mut self.r)
    }
}

impl CdpWriter for WsWriter {
    fn send(&mut self, msg: &str) -> io::Result<()> {
        let frame = encode_ws_frame(0x1, msg.as_bytes());
        self.w.write_all(&frame)?;
        self.w.flush()
    }
}

/// Encode one masked client frame (FIN set) with the given opcode.
fn encode_ws_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= 0xffff {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mut mask = [0u8; 4];
    rand::Rng::fill(&mut rand::thread_rng(), &mut mask[..]);
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    out
}

/// Read and reassemble one application message (text/binary), skipping and
/// handling control frames. `Ok(None)` on a close frame or clean EOF.
fn read_ws_message<R: Read>(r: &mut R) -> io::Result<Option<String>> {
    let mut payload: Vec<u8> = Vec::new();
    loop {
        let mut h = [0u8; 2];
        match r.read_exact(&mut h) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let fin = h[0] & 0x80 != 0;
        let opcode = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            len = u64::from_be_bytes(b);
        }
        let mut mask = [0u8; 4];
        if masked {
            r.read_exact(&mut mask)?;
        }
        let mut data = vec![0u8; len as usize];
        r.read_exact(&mut data)?;
        if masked {
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        match opcode {
            0x8 => return Ok(None),         // close
            0x9 | 0xa => continue,          // ping/pong — ignore (localhost)
            0x0 | 0x1 | 0x2 => {
                payload.extend_from_slice(&data);
                if fin {
                    return Ok(Some(String::from_utf8_lossy(&payload).into_owned()));
                }
            }
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, Sender};

    #[test]
    fn ws_frame_roundtrip() {
        // Encode as a client (masked), decode with the server-side reader.
        for payload in [&b""[..], b"hi", &vec![b'x'; 130][..], &vec![b'y'; 70000][..]] {
            let frame = encode_ws_frame(0x1, payload);
            let mut cursor = std::io::Cursor::new(frame);
            let msg = read_ws_message(&mut cursor).unwrap().unwrap();
            assert_eq!(msg.as_bytes(), payload);
        }
    }

    #[test]
    fn ws_close_frame_ends_stream() {
        let frame = encode_ws_frame(0x8, b"");
        let mut cursor = std::io::Cursor::new(frame);
        assert!(read_ws_message(&mut cursor).unwrap().is_none());
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
    #[ignore]
    fn e2e_evaluate_against_chromium() {
        use std::io::Read;
        let bin = std::env::var("CELLULOSE_BROWSER").unwrap_or_else(|_| {
            "/opt/pw-browsers/chromium-1194/chrome-linux/chrome".to_string()
        });
        let mut child = std::process::Command::new(bin)
            .args([
                "--headless",
                "--no-sandbox",
                "--disable-gpu",
                "--remote-debugging-port=0",
                "--disable-features=EncryptedClientHello",
                "about:blank",
            ])
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn chromium");
        // Parse the DevTools ws url off stderr.
        let mut err = child.stderr.take().unwrap();
        let mut buf = Vec::new();
        let mut one = [0u8; 256];
        let ws = loop {
            let n = err.read(&mut one).expect("read stderr");
            assert!(n > 0, "chromium exited without a DevTools line");
            buf.extend_from_slice(&one[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(i) = text.find("ws://") {
                let end = text[i..].find(char::is_whitespace).map(|e| i + e).unwrap_or(text.len());
                break text[i..end].to_string();
            }
        };
        let (r, w) = ws_connect(&ws).expect("ws connect");
        let cdp = Cdp::new(r, w);
        let targets = cdp
            .call("Target.getTargets", json!({}), None, Duration::from_secs(5))
            .expect("getTargets");
        let page = targets["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["type"] == "page")
            .expect("a page target");
        let attached = cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": page["targetId"], "flatten": true }),
                None,
                Duration::from_secs(5),
            )
            .expect("attach");
        let sid = attached["sessionId"].as_str().unwrap().to_string();
        let res = cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "1+2", "returnByValue": true }),
                Some(&sid),
                Duration::from_secs(5),
            )
            .expect("evaluate");
        assert_eq!(res["result"]["value"], 3);
        let _ = child.kill();
        let _ = child.wait();
    }

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
