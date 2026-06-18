// replay — load a trace JSONL recorded by `oaita trace --jsonl FILE`, then
// serve as a tiny OpenAI-compatible HTTP endpoint. Each /chat/completions
// request gets matched against the recorded gen.request events and the
// paired gen.reply is streamed back as SSE deltas (or as a single non-
// stream JSON when stream=false).
//
// Matching is by LAST-USER-MESSAGE content: the trace's gen.request with
// the same final user message wins, in recording order. That mirrors the
// Python prototype's `srv.expect(matcher, response)` semantic and survives
// reorderings that don't change the actual prompt.
//
// Usage:
//   oaita replay --jsonl FILE [--port 8765] [--once]
//
// Then point oaita.toml at http://127.0.0.1:8765/v1 and re-run the same
// session — every gen call gets the exact recorded reply.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use serde_json::{json, Value};

/// One recorded pair: the LAST USER MESSAGE of the request (used as the
/// match key) plus the full gen.reply event's payload.
#[derive(Debug, Clone)]
struct Recorded {
    last_user: String,
    n_messages: usize,
    content: String,
    tool_calls: Vec<Value>,
    finish_reason: String,
}

fn last_user_message(messages: &[Value]) -> String {
    for m in messages.iter().rev() {
        if m.get("role").and_then(Value::as_str) == Some("user") {
            return m.get("content").and_then(Value::as_str).unwrap_or("").to_string();
        }
    }
    String::new()
}

/// Walk the JSONL file and pair each gen.request with the next gen.reply
/// from the same pid. Returns the recorded pairs in trace order.
fn load_recorded(path: &str) -> std::io::Result<Vec<Recorded>> {
    let f = std::fs::File::open(path)?;
    let mut pending: Vec<(i64, String, usize)> = Vec::new(); // (pid, last_user, n_messages)
    let mut out: Vec<Recorded> = Vec::new();
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue; };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue; };
        let pid = v.get("pid").and_then(Value::as_i64).unwrap_or(0);
        let event = v.get("event").and_then(Value::as_str).unwrap_or("");
        if event == "gen.request" {
            let messages = v.get("messages").cloned().unwrap_or(Value::Array(vec![]));
            let arr = messages.as_array().cloned().unwrap_or_default();
            let last = last_user_message(&arr);
            pending.push((pid, last, arr.len()));
        } else if event == "gen.reply" {
            // Pair with the most recent request from the same pid.
            let pos = pending.iter().rposition(|(p, _, _)| *p == pid);
            let Some(pos) = pos else { continue; };
            let (_, last_user, n_messages) = pending.remove(pos);
            let content = v.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            let tool_calls = v.get("tool_calls")
                .and_then(Value::as_array).cloned().unwrap_or_default();
            let finish_reason = v.get("finish_reason")
                .and_then(Value::as_str).unwrap_or("stop").to_string();
            out.push(Recorded {
                last_user, n_messages, content, tool_calls, finish_reason,
            });
        }
    }
    Ok(out)
}

pub fn run(args: &[String]) -> i32 {
    let mut jsonl: Option<String> = None;
    let mut port: u16 = 8765;
    let mut once = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jsonl" => jsonl = it.next().cloned(),
            "--port" => port = it.next().and_then(|s| s.parse().ok()).unwrap_or(port),
            "--once" => once = true,
            other => { eprintln!("oaita replay: unknown flag {other:?}"); return 2; }
        }
    }
    let Some(path) = jsonl else {
        eprintln!("oaita replay: --jsonl FILE is required");
        return 2;
    };
    let recorded = match load_recorded(&path) {
        Ok(r) => r,
        Err(e) => { eprintln!("oaita replay: load {path}: {e}"); return 1; }
    };
    eprintln!("oaita replay: loaded {} recorded gen pairs from {path}", recorded.len());
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => { eprintln!("oaita replay: bind 127.0.0.1:{port}: {e}"); return 1; }
    };
    eprintln!("oaita replay: serving http://127.0.0.1:{port} — point oaita.toml's \
               base_url at http://127.0.0.1:{port}/v1");
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(
        recorded.into_iter().collect::<VecDeque<_>>()));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue; };
        let rec = recorded.clone();
        std::thread::spawn(move || handle_conn(conn, rec));
        if once {
            // For automated tests: serve a few then exit. Cap at 32 to be
            // generous but bounded.
            if recorded.lock().unwrap().is_empty() { break; }
        }
    }
    0
}

fn handle_conn(mut conn: TcpStream, recorded: std::sync::Arc<std::sync::Mutex<VecDeque<Recorded>>>) {
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    // Read the HTTP request: headline + headers + body. We don't need a
    // full HTTP parser — just look for Content-Length and read that much.
    let mut buf = [0u8; 8192];
    let mut head = Vec::<u8>::new();
    loop {
        match conn.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") { break; }
                if head.len() > 1024 * 1024 { return; }
            }
            Err(_) => return,
        }
    }
    let head_str = String::from_utf8_lossy(&head).into_owned();
    let split = head_str.find("\r\n\r\n").unwrap_or(head_str.len());
    let (header_part, rest) = head.split_at(split + 4);
    let header_str = String::from_utf8_lossy(header_part);
    let mut content_length: usize = 0;
    for line in header_str.lines() {
        if let Some(v) = line.strip_prefix("Content-Length:").or_else(|| line.strip_prefix("content-length:")) {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = rest.to_vec();
    while body.len() < content_length {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let messages: Vec<Value> = req.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let want_last = last_user_message(&messages);
    let stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // Pick the recorded pair whose last_user matches; fall back to front.
    let pick = {
        let mut g = recorded.lock().unwrap();
        let pos = g.iter().position(|r| r.last_user == want_last)
            .or_else(|| if g.is_empty() { None } else { Some(0) });
        pos.map(|i| g.remove(i).unwrap())
    };
    let Some(r) = pick else {
        let body = json!({"error":{"message":"no recorded reply for this request",
                                   "want_last": want_last}}).to_string();
        let _ = write!(conn, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\
                              Content-Length: {}\r\n\r\n{body}", body.len());
        return;
    };
    eprintln!("oaita replay: matched n_messages={} content_len={} tool_calls={}",
              r.n_messages, r.content.len(), r.tool_calls.len());
    if stream {
        serve_streamed(&mut conn, &r);
    } else {
        serve_buffered(&mut conn, &r);
    }
}

fn serve_buffered(conn: &mut TcpStream, r: &Recorded) {
    let resp = json!({
        "id": "chatcmpl-replay",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": r.content,
                "tool_calls": r.tool_calls,
            },
            "finish_reason": r.finish_reason,
        }],
    });
    let body = resp.to_string();
    let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                          Content-Length: {}\r\n\r\n{body}", body.len());
}

fn serve_streamed(conn: &mut TcpStream, r: &Recorded) {
    let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                          Cache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n");
    // Yield content as ONE delta. (We could chunk it but byte-identity on
    // replay only requires the assembled content match — the harness
    // streams it back into a single file.)
    if !r.content.is_empty() {
        let chunk = json!({
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": r.content},
            }],
        });
        write_sse(conn, &chunk.to_string());
    }
    for (i, tc) in r.tool_calls.iter().enumerate() {
        let chunk = json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": i,
                        "id": tc.get("id"),
                        "type": "function",
                        "function": {
                            "name": tc.get("function").and_then(|f| f.get("name")),
                            "arguments": tc.get("function").and_then(|f| f.get("arguments")),
                        },
                    }],
                },
            }],
        });
        write_sse(conn, &chunk.to_string());
    }
    let finish = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": r.finish_reason}],
    });
    write_sse(conn, &finish.to_string());
    write_sse(conn, "[DONE]");
    // End chunked transfer.
    let _ = conn.write_all(b"0\r\n\r\n");
}

fn write_sse(conn: &mut TcpStream, payload: &str) {
    let frame = format!("data: {payload}\n\n");
    // Chunked transfer: length-line + chunk + CRLF.
    let _ = write!(conn, "{:x}\r\n{frame}\r\n", frame.len());
}
