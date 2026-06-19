// pretty — render an `oaita trace --jsonl FILE` recording as a narrative
// suitable for HUMAN READING and for asking a subagent "is this working
// or do we need to fix something?". Lossy by design:
//
//   * tool schemas are listed ONCE at the top of each session (they repeat
//     identically in every gen.request);
//   * messages already shown are NOT re-listed each gen — only the new
//     turns the model received since the last gen are surfaced;
//   * tool result blobs are head-clamped to a digest (full bytes are still
//     in the JSONL).
//
// The output is meant to be read top-to-bottom like a conversation log,
// with each gen showing what the model was given that's NEW + what it
// decided, plus a per-session stats footer.
//
// Usage:
//   oaita pretty --jsonl FILE [--session NAME]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};

use serde_json::Value;

#[derive(Debug, Clone)]
struct Event {
    ts: f64,
    pid: i64,
    kind: String,
    session: String,
    payload: Value,
}

fn load(path: &str) -> std::io::Result<Vec<Event>> {
    let f = std::fs::File::open(path)?;
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue; };
        out.push(Event {
            ts: v.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
            pid: v.get("pid").and_then(Value::as_i64).unwrap_or(0),
            kind: v.get("event").and_then(Value::as_str).unwrap_or("").to_string(),
            session: v.get("session").and_then(Value::as_str).unwrap_or("").to_string(),
            payload: v,
        });
    }
    out.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

pub fn run(args: &[String]) -> i32 {
    let mut jsonl: Option<String> = None;
    let mut filter_session: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jsonl" => jsonl = it.next().cloned(),
            "--session" => filter_session = it.next().cloned(),
            other => { eprintln!("oaita pretty: unknown flag {other:?}"); return 2; }
        }
    }
    let Some(path) = jsonl else {
        eprintln!("oaita pretty: --jsonl FILE is required");
        return 2;
    };
    let events = match load(&path) {
        Ok(e) => e,
        Err(e) => { eprintln!("oaita pretty: load {path}: {e}"); return 1; }
    };
    if events.is_empty() {
        eprintln!("oaita pretty: no events in {path}");
        return 0;
    }
    // Group by session (in event order; preserves chronological narration).
    let mut by_session: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for e in events {
        if e.session.is_empty() { continue; }
        if let Some(f) = &filter_session {
            if e.session != *f { continue; }
        }
        by_session.entry(e.session.clone()).or_default().push(e);
    }
    for (name, evs) in by_session {
        render_session(&name, &evs);
    }
    0
}

fn render_session(name: &str, evs: &[Event]) {
    let t0 = evs.first().map(|e| e.ts).unwrap_or(0.0);
    let t_end = evs.last().map(|e| e.ts).unwrap_or(0.0);
    println!("══════════════════════════════════════════════════════════════════");
    println!("Session: {name}    Duration: {:.2}s    Events: {}",
             t_end - t0, evs.len());
    println!("══════════════════════════════════════════════════════════════════");
    // Print tool inventory ONCE if any gen.request had tools.
    if let Some(first_req) = evs.iter().find(|e| e.kind == "gen.request") {
        if let Some(tools) = first_req.payload.get("tools").and_then(Value::as_array) {
            let names: Vec<String> = tools.iter().filter_map(|t|
                t.get("function").and_then(|f| f.get("name"))
                 .and_then(Value::as_str).map(String::from)).collect();
            if !names.is_empty() {
                println!("Tools available: {}", names.join(", "));
            }
        }
        if let Some(model) = first_req.payload.get("model").and_then(Value::as_str) {
            println!("Model: {model}");
        }
        println!();
    }
    // Track what messages the model has seen so we only print NEW ones
    // at each gen.request. Identity is content-hash for simplicity.
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut gen_n = 0usize;
    let mut stats_gens = 0usize;
    let mut stats_tools: BTreeMap<String, usize> = BTreeMap::new();
    let mut stats_subagents = 0usize;
    let mut last_gen_ts = t0;

    for e in evs {
        let rel = e.ts - t0;
        match e.kind.as_str() {
            "run.start" => {
                let max = e.payload.get("max_steps").and_then(Value::as_i64).unwrap_or(0);
                println!("[+{rel:>6.2}s] ▶ run.start  max_steps={max}");
            }
            "gen.request" => {
                gen_n += 1;
                stats_gens += 1;
                println!("\n[+{rel:>6.2}s] ▶ gen #{gen_n}");
                let arr = e.payload.get("messages")
                    .and_then(Value::as_array).cloned().unwrap_or_default();
                for m in &arr {
                    let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
                    let content = m.get("content").and_then(Value::as_str).unwrap_or("");
                    let key = hash_string(&format!("{role}\0{content}"));
                    if seen.insert(key) {
                        print_message_block(role, content);
                    }
                }
                last_gen_ts = e.ts;
            }
            "gen.reply" => {
                let content = e.payload.get("content")
                    .and_then(Value::as_str).unwrap_or("");
                let finish = e.payload.get("finish_reason")
                    .and_then(Value::as_str).unwrap_or("");
                let tcs = e.payload.get("tool_calls")
                    .and_then(Value::as_array).cloned().unwrap_or_default();
                let latency = e.ts - last_gen_ts;
                println!("[+{rel:>6.2}s] ◀ gen.reply  ({latency:.2}s, finish={finish})");
                if !content.trim().is_empty() {
                    print_message_block("assistant", content);
                }
                for tc in &tcs {
                    print_tool_call_summary(tc);
                }
                // Also count any rescued content-as-tool-call so the
                // stats line below makes sense.
                if tcs.is_empty() && !content.trim().is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(content.trim()) {
                        if let Some(tn) = v.get("tool").and_then(Value::as_str) {
                            println!("           ⚠  reply IS a tool call in plain content (rescued): {tn}");
                        }
                    }
                }
                // Mark the assistant turn as "seen" so future gens don't repeat.
                if !content.is_empty() {
                    seen.insert(hash_string(&format!("assistant\0{content}")));
                }
            }
            "call.eval" => {
                let tool = e.payload.get("tool").and_then(Value::as_str).unwrap_or("?");
                println!("[+{rel:>6.2}s]   · dispatch  {tool}");
                *stats_tools.entry(tool.to_string()).or_default() += 1;
            }
            "call.result" => {
                let tool = e.payload.get("tool").and_then(Value::as_str).unwrap_or("?");
                let bytes = e.payload.get("bytes").and_then(Value::as_i64).unwrap_or(0);
                println!("[+{rel:>6.2}s]   · result    {tool} ({bytes} bytes)");
            }
            "exec.run" => {
                let bx = e.payload.get("box").and_then(Value::as_str).unwrap_or("?");
                let discard = e.payload.get("discard").and_then(Value::as_bool).unwrap_or(false);
                let len = e.payload.get("script_len").and_then(Value::as_i64).unwrap_or(0);
                println!("[+{rel:>6.2}s]     · exec    box={bx} {} script_len={len}",
                         if discard { "discard=true" } else { "persistent" });
            }
            "exec.done" => {
                let rc = e.payload.get("rc").and_then(Value::as_i64).unwrap_or(0);
                let bytes = e.payload.get("bytes").and_then(Value::as_i64).unwrap_or(0);
                println!("[+{rel:>6.2}s]     · exec.done rc={rc} bytes={bytes}");
            }
            "run.subagent_cleaned" => {
                let inner = e.payload.get("inner").and_then(Value::as_str).unwrap_or("?");
                println!("[+{rel:>6.2}s]   ✂ subagent_cleaned {inner}");
                stats_subagents += 1;
            }
            "run.settled" => {
                println!("[+{rel:>6.2}s] ✔ run.settled");
            }
            other => {
                println!("[+{rel:>6.2}s]   ({other})");
            }
        }
    }
    // Stats footer.
    let tool_line: Vec<String> = stats_tools.iter()
        .map(|(t, n)| format!("{t}×{n}")).collect();
    println!();
    println!("── stats ──────────────────────────────────────────────────────────");
    println!("  gens: {stats_gens}    tools: {}    subagents cleaned: {stats_subagents}    duration: {:.2}s",
             if tool_line.is_empty() { "none".into() }
             else { tool_line.join(", ") },
             t_end - t0);
    println!();
}

fn print_message_block(role: &str, content: &str) {
    let banner = match role {
        "user" => "USER",
        "system" => "SYSTEM",
        "developer" => "DEVELOPER",
        "assistant" => "ASSISTANT",
        "tool" => "TOOL",
        _ => role,
    };
    println!("  ┌─ {banner} ─────");
    let trimmed = content.trim_end();
    for line in trimmed.lines().take(20) {
        // Clip very long lines so the narrative stays scannable.
        let l = if line.len() > 180 { format!("{}…", clip_at_char(line, 180)) }
                else { line.to_string() };
        println!("  │ {l}");
    }
    let nlines = trimmed.lines().count();
    if nlines > 20 {
        println!("  │ …[{} more lines]", nlines - 20);
    }
    println!("  └──");
}

/// Truncate `s` at the longest char-boundary ≤ `max_bytes`. `&s[..n]` panics
/// when `n` falls mid-character — a real concern here because tool args carry
/// arbitrary file content (box-drawing chars, accented letters, etc. all
/// multi-byte). Walks char_indices and stops at the last index that fits.
fn clip_at_char(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    let mut last = 0;
    for (i, _) in s.char_indices() {
        if i > max_bytes { break; }
        last = i;
    }
    &s[..last]
}

fn print_tool_call_summary(tc: &Value) {
    let name = tc.get("function").and_then(|f| f.get("name"))
        .and_then(Value::as_str).unwrap_or("?");
    let args_raw = tc.get("function").and_then(|f| f.get("arguments"))
        .and_then(Value::as_str).unwrap_or("{}");
    // Parse and re-print as one-liner with key=value pairs, clipping long ones.
    let args: Value = serde_json::from_str(args_raw).unwrap_or(Value::Null);
    let mut bits: Vec<String> = Vec::new();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            let vs = match v {
                Value::String(s) => {
                    if s.len() > 80 { format!("{:?}…", clip_at_char(s, 80)) }
                    else { format!("{s:?}") }
                }
                _ => v.to_string(),
            };
            bits.push(format!("{k}={vs}"));
        }
    }
    println!("           → tool_call  {name}({})", bits.join(", "));
}

/// Deterministic 64-bit hash for message dedup. Per-run RandomState would
/// make seen-keys collide between gens (different seed each call); use a
/// FxHash-style folded multiply so the dedup actually works.
fn hash_string(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
