// trace — the flight recorder. Every oaita process (top-level AND in-box
// sub-agents) streams newline-JSON events over the ENGINE'S control socket
// using the `trace.emit` verb. The engine broadcasts them to every conn
// that has issued `trace.subscribe`; `oaita trace --jsonl FILE` is that
// subscriber + jsonl writer.
//
// One socket — the engine's. In-box emitters reach it through the FD
// broker (no /tmp/oaita-trace.sock bind-mount, no extra path to bwrap
// through the box's mount namespace). Host emitters use the filesystem
// UDS like every other engine client.
//
// Best-effort throughout: no engine reachable, write errors → silent
// no-op. The trace MUST NEVER affect the run.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

enum Sink {
    None,
    Stream(Mutex<UnixStream>),
}

static SINK: std::sync::OnceLock<Sink> = std::sync::OnceLock::new();

/// Acquire an engine connection: broker (in-box) or filesystem (host). The
/// broker is preferred when SARUN_BROKER is set — that's the same dispatch
/// the runner + oaita::exec::ctrl_rpc use, so the trace path doesn't need
/// its own discovery logic.
fn dial_engine() -> Option<UnixStream> {
    if let Ok(name) = std::env::var("SARUN_BROKER") {
        if !name.is_empty() {
            return crate::runner::broker_dial(&name).ok();
        }
    }
    UnixStream::connect(crate::paths::sock_path()).ok()
}

fn build_sink() -> Sink {
    // OAITA_TRACE is the master on/off switch for this process — the value
    // itself is ignored (the destination is always the engine). Unset or
    // empty means "do not emit", matching the prior contract.
    let Ok(ep) = std::env::var("OAITA_TRACE") else { return Sink::None; };
    if ep.is_empty() { return Sink::None; }
    let Some(mut conn) = dial_engine() else { return Sink::None; };
    if writeln!(conn, "{{\"type\":\"trace.emit\"}}").is_err() {
        return Sink::None;
    }
    if conn.flush().is_err() { return Sink::None; }
    Sink::Stream(Mutex::new(conn))
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Emit one trace event. Always swallows errors.
pub fn event(kind: &str, fields: Value) {
    let sink = SINK.get_or_init(build_sink);
    let Sink::Stream(mu) = sink else { return; };
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
    if let Ok(mut s) = mu.lock() {
        let _ = s.write_all(line.as_bytes());
        let _ = s.flush();
    }
}

// ── Collector — the `oaita trace` subcommand ─────────────────────────────────

/// Subscribe to the engine's trace stream, render each event to stdout, and
/// optionally tee to a jsonl file. The `_endpoint` arg is accepted for
/// backwards-compatible CLI shape (callers pass `/tmp/...sock` from habit)
/// but ignored — the engine is the sole source.
pub fn run_collector(_endpoint: &str, jsonl: Option<&str>) -> i32 {
    let Some(mut conn) = dial_engine() else {
        eprintln!("oaita: no engine running");
        return 1;
    };
    if writeln!(conn, "{{\"type\":\"trace.subscribe\"}}").is_err() {
        eprintln!("oaita: subscribe write failed");
        return 1;
    }
    let _ = conn.flush();
    let reader_conn = match conn.try_clone() {
        Ok(c) => c,
        Err(e) => { eprintln!("oaita: try_clone: {e}"); return 1; }
    };
    let mut reader = BufReader::new(reader_conn);
    let mut ack = String::new();
    if reader.read_line(&mut ack).is_err() {
        eprintln!("oaita: subscribe ack read failed");
        return 1;
    }
    eprintln!("oaita: tracing engine stream (Ctrl-C to stop)");
    let mut log = jsonl.and_then(|p| std::fs::OpenOptions::new()
        .create(true).append(true).open(p).ok());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return 0,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches('\n');
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            println!("{}", render_event(&v));
        } else {
            println!("{trimmed}");
        }
        if let Some(f) = log.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
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
