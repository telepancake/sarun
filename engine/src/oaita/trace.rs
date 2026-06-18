// trace — the flight recorder. Every oaita process (top-level AND in-box
// sub-agents) streams newline-JSON events to whatever endpoint `$OAITA_TRACE`
// names; `oaita trace` is the collector that turns those into one human line
// per event plus a JSONL file you can replay into expect-script tests.
//
// Endpoints:
//   @name        abstract unix socket (boxes share the host netns so this
//                crosses the overlay; the filesystem deliberately can't —
//                a box's writes are staged, gone on reject)
//   /path        filesystem unix socket
//   host:port    TCP
//
// Best-effort: no endpoint, missing collector, write errors → silent no-op.
// The trace MUST NEVER affect the run.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixDatagram;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

enum Sink {
    None,
    UnixDgram(UnixDatagram, String), // (sock, target)
    TcpLine(Mutex<TcpStream>),
}

static SINK: std::sync::OnceLock<Sink> = std::sync::OnceLock::new();

fn build_sink() -> Sink {
    let Ok(ep) = std::env::var("OAITA_TRACE") else { return Sink::None; };
    if ep.is_empty() { return Sink::None; }
    if let Some(name) = ep.strip_prefix('@') {
        // Abstract namespace: first byte of the path is NUL on Linux.
        let mut addr = vec![0u8];
        addr.extend_from_slice(name.as_bytes());
        let Ok(sock) = UnixDatagram::unbound() else { return Sink::None; };
        // Stash the target — we send to it on each event (connect would also
        // work but unbound+sendto is simpler for abstract addresses).
        return Sink::UnixDgram(sock, ep);
    }
    if ep.starts_with('/') {
        let Ok(sock) = UnixDatagram::unbound() else { return Sink::None; };
        return Sink::UnixDgram(sock, ep);
    }
    if let Some((host, port)) = ep.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            if let Ok(addrs) = (host, port).to_socket_addrs_helper() {
                for a in addrs {
                    if let Ok(s) = TcpStream::connect_timeout(&a, std::time::Duration::from_millis(250)) {
                        return Sink::TcpLine(Mutex::new(s));
                    }
                }
            }
        }
    }
    Sink::None
}

trait HostPort {
    fn to_socket_addrs_helper(self) -> std::io::Result<Vec<SocketAddr>>;
}
impl HostPort for (&str, u16) {
    fn to_socket_addrs_helper(self) -> std::io::Result<Vec<SocketAddr>> {
        use std::net::ToSocketAddrs;
        Ok((self.0, self.1).to_socket_addrs()?.collect())
    }
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Emit one trace event. Always synchronous (cheap), always swallows errors.
pub fn event(kind: &str, fields: Value) {
    let sink = SINK.get_or_init(build_sink);
    let mut rec = json!({
        "ts": now_secs(),
        "event": kind,
        "pid": std::process::id(),
    });
    if let (Some(obj), Some(f)) = (rec.as_object_mut(), fields.as_object()) {
        for (k, v) in f { obj.insert(k.clone(), v.clone()); }
    }
    let mut line = serde_json::to_string(&rec).unwrap_or_default();
    line.push('\n');
    match sink {
        Sink::None => {}
        Sink::UnixDgram(sock, target) => {
            if let Some(name) = target.strip_prefix('@') {
                let mut path = vec![0u8];
                path.extend_from_slice(name.as_bytes());
                let _ = sock.send_to_unix_abstract(&line, &path);
            } else {
                let _ = sock.send_to(line.as_bytes(), target);
            }
        }
        Sink::TcpLine(mu) => {
            if let Ok(mut s) = mu.lock() { let _ = s.write_all(line.as_bytes()); }
        }
    }
}

trait UnixDgramExt {
    fn send_to_unix_abstract(&self, payload: &str, target: &[u8]) -> std::io::Result<usize>;
}
impl UnixDgramExt for UnixDatagram {
    fn send_to_unix_abstract(&self, payload: &str, target: &[u8]) -> std::io::Result<usize> {
        // Build a sockaddr_un for an abstract namespace path (leading NUL) and
        // sendto via libc — std::os::unix::net::SocketAddr::from_abstract_name
        // is linux-nightly-only, so we go through the C bindings directly.
        unsafe {
            let mut sa: libc::sockaddr_un = std::mem::zeroed();
            sa.sun_family = libc::AF_UNIX as _;
            let max = std::mem::size_of_val(&sa.sun_path);
            if target.len() > max { return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput, "abstract path too long")); }
            for (i, b) in target.iter().enumerate() {
                sa.sun_path[i] = *b as _;
            }
            let len = (std::mem::size_of::<libc::sa_family_t>() + target.len()) as libc::socklen_t;
            let fd = std::os::fd::AsRawFd::as_raw_fd(self);
            let r = libc::sendto(fd, payload.as_ptr().cast(), payload.len(), 0,
                                 (&sa as *const libc::sockaddr_un).cast(), len);
            if r < 0 { Err(std::io::Error::last_os_error()) }
            else { Ok(r as usize) }
        }
    }
}

// ── Collector — the `oaita trace` subcommand ─────────────────────────────────

pub fn run_collector(endpoint: &str, jsonl: Option<&str>) -> i32 {
    use std::os::unix::net::UnixDatagram;
    let sock = if let Some(name) = endpoint.strip_prefix('@') {
        let mut path = vec![0u8];
        path.extend_from_slice(name.as_bytes());
        match bind_abstract(&path) {
            Ok(s) => s,
            Err(e) => { eprintln!("oaita: bind {endpoint}: {e}"); return 1; }
        }
    } else if endpoint.starts_with('/') {
        let _ = std::fs::remove_file(endpoint);
        match UnixDatagram::bind(endpoint) {
            Ok(s) => s,
            Err(e) => { eprintln!("oaita: bind {endpoint}: {e}"); return 1; }
        }
    } else {
        eprintln!("oaita: only @abstract and /path unix endpoints supported in collector");
        return 1;
    };
    eprintln!("oaita: tracing on {endpoint} — export OAITA_TRACE={endpoint} (Ctrl-C to stop)");
    let mut log = jsonl.and_then(|p| std::fs::OpenOptions::new()
        .create(true).append(true).open(p).ok());
    let mut buf = [0u8; 65536];
    loop {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) => { eprintln!("oaita: recv: {e}"); continue; }
        };
        let line = String::from_utf8_lossy(&buf[..n]);
        let line = line.trim_end_matches('\n');
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            println!("{}", render_event(&v));
        } else {
            println!("{line}");
        }
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn bind_abstract(path: &[u8]) -> std::io::Result<UnixDatagram> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 { return Err(std::io::Error::last_os_error()); }
        let mut sa: libc::sockaddr_un = std::mem::zeroed();
        sa.sun_family = libc::AF_UNIX as _;
        for (i, b) in path.iter().enumerate() {
            sa.sun_path[i] = *b as _;
        }
        let len = (std::mem::size_of::<libc::sa_family_t>() + path.len()) as libc::socklen_t;
        if libc::bind(fd, (&sa as *const libc::sockaddr_un).cast(), len) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        use std::os::fd::FromRawFd;
        Ok(UnixDatagram::from_raw_fd(fd))
    }
}

fn render_event(v: &Value) -> String {
    let ts = v.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let ev = v.get("event").and_then(Value::as_str).unwrap_or("?");
    let pid = v.get("pid").and_then(Value::as_i64).unwrap_or(0);
    let session = v.get("session").and_then(Value::as_str).unwrap_or("");
    let detail: String = match ev {
        "run.start" => format!("{}", v.get("session").and_then(Value::as_str).unwrap_or("")),
        "gen.request" => {
            let n = v.get("n_messages").and_then(Value::as_i64).unwrap_or(0);
            let model = v.get("model").and_then(Value::as_str).unwrap_or("");
            format!("{model} ({n} msgs)")
        }
        "gen.reply" => {
            let len = v.get("content_len").and_then(Value::as_i64).unwrap_or(0);
            let calls = v.get("n_tool_calls").and_then(Value::as_i64).unwrap_or(0);
            format!("len={len} calls={calls}")
        }
        "call.eval" => v.get("tool").and_then(Value::as_str).unwrap_or("").to_string(),
        "call.result" => format!("{} {}",
            v.get("tool").and_then(Value::as_str).unwrap_or(""),
            v.get("bytes").and_then(Value::as_i64).unwrap_or(0)),
        "exec.run" => v.get("box").and_then(Value::as_str).unwrap_or("").to_string(),
        "exec.done" => format!("rc={} bytes={}",
            v.get("rc").and_then(Value::as_i64).unwrap_or(0),
            v.get("bytes").and_then(Value::as_i64).unwrap_or(0)),
        _ => String::new(),
    };
    format!("{ts:.3} [{pid}] {session:>8} {ev:<14} {detail}")
}
