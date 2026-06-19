// gen / call / run — the one-step primitives plus the driver loop. Matches
// the Python prototype's semantics turn-for-turn (and file-for-file): each
// turn is one file in the session folder; gen writes a `p`-flagged partial
// while streaming and drops the flag on clean completion; tool calls are
// persisted as `c`-flagged assistant turns and answered by `call`.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::oaita::client::Client;
use crate::oaita::config::Config;
use crate::oaita::exec::{box_name, Executor};
use crate::oaita::ids::{is_adoptable_slug, new_turn_id};
use crate::oaita::inspect::{inspect, parse_locator, read_path, write_at_locator};
use crate::oaita::tools::{tools_array, ExecResult};
use crate::oaita::trace;
use crate::oaita::turns::{
    append_turn, assign_slugs, build_messages, load_stitched, load_turns,
    next_number, parse_stitch, session_dir, strip_emitted_turn_id,
    target_segment, turn_filename, Turn,
};

#[derive(Clone, Debug)]
pub struct Settings {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub capabilities: Option<String>,
    pub tool_context: Option<String>,
    pub depth: u32,
    pub sarun_override: Option<String>,
    pub no_sandbox: bool,
}

impl Settings {
    pub fn resolve(model: Option<String>, base_url: Option<String>,
                   api_key: Option<String>,
                   capabilities: Option<String>,
                   tool_context: Option<String>,
                   sarun_override: Option<String>,
                   no_sandbox: bool)
        -> Result<Self, String>
    {
        let cfg = Config::load();
        let (model_d, base_d, key_d) = cfg.resolve()?;
        let depth = std::env::var("OAITA_DEPTH").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok(Settings {
            model: model.unwrap_or(model_d),
            base_url: base_url.unwrap_or(base_d),
            api_key: api_key.unwrap_or(key_d),
            capabilities,
            tool_context,
            depth,
            sarun_override,
            no_sandbox,
        })
    }
}

// ── gen ─────────────────────────────────────────────────────────────────────
/// One model generation. The streamed reply is written incrementally to a
/// `p`-flagged target file; on clean completion the flag drops (rename). If
/// the model emits tool calls instead of prose (or alongside it), each call
/// is persisted as a `c`-flagged assistant turn and gen STOPS without
/// evaluating them (that is `call`'s job).
pub fn generate(spec: &str, set: &Settings) -> Result<Vec<PathBuf>, String> {
    let segs = parse_stitch(spec)?;
    let target = segs.last().unwrap().clone();
    fs::create_dir_all(session_dir(&target))
        .map_err(|e| format!("create session dir: {e}"))?;

    // Ensure every turn has a slug; turn-ids stay unique across the stitched
    // context.
    let stitched = load_stitched(spec)?;
    // The TARGET session's current turn list (for resume / append logic).
    let mut target_turns = load_turns(&target);
    let mut existing: HashSet<String> = stitched.iter()
        .filter_map(|t| t.slug.clone()).collect();
    target_turns = assign_slugs(target_turns, &mut existing)
        .map_err(|e| format!("assign slugs: {e}"))?;

    // Resume rules: if the LAST target turn is `p`-flagged, regenerate IN
    // PLACE and EXCLUDE it from the prompt. Otherwise append a new
    // `p`-flagged turn.
    let resume = target_turns.last()
        .map(|t| t.kind == "assistant" && t.flags.contains('p'))
        .unwrap_or(false);

    let (target_path, target_slug) = if resume {
        let last = target_turns.last().unwrap().clone();
        (last.path, last.slug.unwrap())
    } else {
        let n = next_number(&target_turns);
        let slug = new_turn_id(&existing);
        existing.insert(slug.clone());
        let name = turn_filename(n, "assistant", Some(&slug), None, "p");
        let path = session_dir(&target).join(name);
        fs::write(&path, "").map_err(|e| format!("create target: {e}"))?;
        (path, slug)
    };

    // The prompt: stitched context up to and excluding our target if resuming.
    let prompt_turns: Vec<Turn> = if resume {
        stitched.into_iter()
            .filter(|t| t.path != target_path).collect()
    } else { stitched };

    let mut messages = build_messages(&prompt_turns);
    // Baseline harness guide: if the session doesn't already have a system
    // turn (user-authored via a dot-stitched guide context), prepend one
    // that primes the model on tool preference. Without this, ds4-flash /
    // mimo / and most open models default to "I'll just use shell+python"
    // because that's what training reinforced — they bypass inspect/read
    // even when those would be cheaper and cleaner.
    let has_system = messages.iter().any(|m|
        m.get("role").and_then(Value::as_str) == Some("system")
        || m.get("role").and_then(Value::as_str) == Some("developer"));
    if !has_system {
        messages.insert(0, serde_json::json!({
            "role": "system",
            "content": HARNESS_GUIDE,
        }));
    }
    // Announcement: surface unhandled completed sub-tasks (act sub-agents
    // that have settled but whose box hasn't been apply/reject'd). Without
    // this, a model can move on and leave staged work — and the orphan box
    // pile up. Injected as a system message AFTER the existing context so
    // it's the freshest thing the model sees before its next turn.
    if let Some(note) = unhandled_subtasks_announcement(&target) {
        messages.push(serde_json::json!({
            "role": "system",
            "content": note,
        }));
    }
    // Behavioural nudge — INJECTED AS A USER TURN with an explicit
    // "Automated harness notification:" prefix. Two earlier attempts didn't
    // work out:
    //   * `role: "system"` — strong-prior models (ds4-flash) ignored it.
    //   * `role: "assistant"` (trailing or appended to a past turn) —
    //     mimo's reasoning trace called it "a weird hidden instruction"
    //     and refused to act on it; the puppet-your-own-voice trick
    //     reads as a prompt-injection attempt.
    // A labelled user message is honest about WHO is talking (the harness),
    // doesn't trigger the prompt-injection refusal path, and reaches every
    // provider's chat template intact. Trailing user is the normal "model
    // generates an assistant reply next" position — no template edge cases.
    if let Some(note) = backtrack_behavioural_announcement(&target) {
        // Rate-limit. Without a stamp we'd re-inject the same notification
        // every gen the conditions hold, and the model would see the same
        // boilerplate every other turn. Once per ~5-turn window is enough.
        if announcement_rate_limit_ok(&target) {
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!("Automated harness notification: {note}"),
            }));
            record_announcement(&target);
        }
    }
    let tools = tools_array(set.capabilities.as_deref(), set.depth);

    trace::event("gen.request", json!({
        "session": &target, "model": &set.model,
        // The FULL request — what we'd POST to /chat/completions if not
        // for the trace. Recorded as the replayable record: a fakeserver
        // can pair this with the matching gen.reply event and serve them
        // back byte-identical for byte-replay testing.
        "messages": messages,
        "tools": tools,
        "n_messages": messages.len(),
    }));

    let body = json!({
        "model": set.model,
        "messages": messages,
        "tools": tools,
        "stream": true,
    });

    let client = Client::from_resolved(&set.base_url, &set.api_key)?;
    let mut content = String::new();
    let mut tool_calls: Vec<AssembledToolCall> = Vec::new();
    let mut finish_reason: String = String::new();

    crate::oaita::client::block_on(async {
        client.post_stream("/chat/completions", body, |payload| {
            let Ok(v) = serde_json::from_str::<Value>(payload) else { return; };
            let Some(choices) = v.get("choices").and_then(Value::as_array)
                else { return; };
            for choice in choices {
                if let Some(d) = choice.get("delta").and_then(Value::as_object) {
                    if let Some(c) = d.get("content").and_then(Value::as_str) {
                        if !c.is_empty() {
                            content.push_str(c);
                            // Stream-into-target: write the partial whenever
                            // content grows. Resilient resume needs the file
                            // to reflect what we have RIGHT NOW.
                            let _ = fs::write(&target_path, &content);
                            print!("{c}");
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                    }
                    if let Some(tcs) = d.get("tool_calls").and_then(Value::as_array) {
                        assemble_tool_calls(&mut tool_calls, tcs);
                    }
                }
                if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                    if !fr.is_empty() { finish_reason = fr.to_string(); }
                }
            }
        }).await
    })?;

    // Strip an emitted turn-id header and possibly ADOPT it as our slug.
    let (emitted, body) = strip_emitted_turn_id(&content);
    let mut produced: Vec<PathBuf> = Vec::new();
    let mut new_slug = target_slug.clone();
    if let Some(eid) = emitted {
        if is_adoptable_slug(&eid) && !existing.contains(&eid) {
            new_slug = eid;
        }
    }

    // Tool-call-as-content rescue: some open models (ds4-flash, mimo, …)
    // emit tool calls as plain JSON in the content delta instead of via
    // the OpenAI `tool_calls` field. Without this rescue the harness banks
    // them as settled answers and the run stops. Pattern: content (after
    // stripping the turn-id header) parses as a JSON object with `tool`
    // (string) and `arguments` (object) at top level — exactly the
    // c.assistant envelope shape we already use on disk.
    if tool_calls.is_empty() {
        if let Some(rescued) = rescue_content_tool_call(&body) {
            tool_calls.push(rescued);
            // Drop the content so we don't bank it as a duplicate answer.
            let _ = std::fs::write(&target_path, "");
        }
    }

    // Decide the assistant turn's final filename:
    // — kept `p` if finish_reason=="length" (a token-cut reply remains a
    //   truthful resumable partial; gen will continue it next round).
    // — `b`-flagged WAYPOINT NOT here (only backtrack(waypoint=true) sets it).
    // — clean otherwise.
    let kept_partial = finish_reason == "length";
    let final_flags = if kept_partial { "p" } else { "" };
    let number = parse_existing_number(&target_path)
        .unwrap_or_else(|| next_number(&load_turns(&target)));
    let final_name = turn_filename(number, "assistant", Some(&new_slug),
                                   None, final_flags);
    let final_path = target_path.parent().unwrap().join(final_name);
    // Tool-call-ONLY reply (no prose): the streamed content is empty and we
    // already persist each call as its own c-flagged turn below. Drop the
    // empty assistant placeholder rather than banking a 0-byte turn that
    // would look like a settled answer to `run`'s settle check.
    if body.is_empty() && !tool_calls.is_empty() {
        let _ = fs::remove_file(&target_path);
    } else {
        if body != content || !kept_partial || new_slug != target_slug {
            let _ = fs::write(&target_path, &body);
        }
        if final_path != target_path {
            let _ = fs::rename(&target_path, &final_path);
        }
        if !body.is_empty() { produced.push(final_path.clone()); }
    }

    // Persist tool calls as `c`-flagged assistant turns. ONE turn per call.
    let mut taken: HashSet<String> = existing.clone();
    taken.insert(new_slug.clone());
    for tc in &tool_calls {
        let call_slug = adopt_call_id(tc.id.as_deref(), &mut taken);
        let envelope = json!({"tool": tc.name, "arguments": tc.arguments_json()});
        let n = next_number(&load_turns(&target));
        let name = turn_filename(n, "assistant", Some(&call_slug), None, "c");
        let path = session_dir(&target).join(name);
        fs::write(&path, envelope.to_string()).map_err(|e| format!("write call: {e}"))?;
        produced.push(path);
    }

    // Full reply for byte-replay: content + assembled tool_calls + finish.
    // The fakeserver pairs this with the preceding gen.request and serves
    // it back as a streamed SSE response when the same prompt comes in.
    let tool_calls_json: Vec<Value> = tool_calls.iter().map(|t| json!({
        "id": t.id,
        "type": "function",
        "function": {
            "name": t.name,
            "arguments": t.arguments,
        },
    })).collect();
    trace::event("gen.reply", json!({
        "session": &target,
        "content": &body,
        "tool_calls": tool_calls_json,
        "finish_reason": finish_reason,
        "kept_partial": kept_partial,
    }));
    Ok(produced)
}

fn parse_existing_number(path: &PathBuf) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn adopt_call_id(wire_id: Option<&str>, taken: &mut HashSet<String>) -> String {
    if let Some(id) = wire_id {
        // Sanitize: lowercase, [a-z0-9]+, length-bound.
        let sane: String = id.chars().filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase()).take(16).collect();
        if !sane.is_empty() && !taken.contains(&sane) {
            taken.insert(sane.clone());
            return sane;
        }
    }
    let id = new_turn_id(taken);
    taken.insert(id.clone());
    id
}

#[derive(Default, Debug)]
struct AssembledToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: String, // JSON string assembled from streamed fragments
}
impl AssembledToolCall {
    fn arguments_json(&self) -> Value {
        serde_json::from_str(&self.arguments).unwrap_or_else(|_| Value::String(self.arguments.clone()))
    }
}

/// Rescue a tool call the model emitted as PLAIN CONTENT instead of via
/// the OpenAI `tool_calls` field. Models like ds4-flash and xiaomi/mimo
/// frequently do this — they understand the tool schema but encode the
/// call in the content delta as a JSON object `{"tool":"X","arguments":{…}}`
/// (exactly the shape we already use on disk for c.assistant envelopes).
///
/// Returns the equivalent AssembledToolCall when the content parses as
/// that shape AND the tool name is one we recognise; None otherwise so
/// content that legitimately starts with a JSON object is not eaten.
fn rescue_content_tool_call(body: &str) -> Option<AssembledToolCall> {
    let trimmed = body.trim();
    if !trimmed.starts_with('{') { return None; }
    let v: Value = serde_json::from_str(trimmed).ok()?;
    let obj = v.as_object()?;
    let tool = obj.get("tool").and_then(Value::as_str)?;
    // Only rescue tools we actually dispatch — `tool` could legitimately
    // appear in a free-form answer that happens to be JSON.
    if !matches!(tool, "ask" | "shell" | "inspect" | "read" | "write" |
                       "apply" | "reject" | "backtrack" | "delete") {
        return None;
    }
    let args = obj.get("arguments").cloned().unwrap_or(Value::Null);
    Some(AssembledToolCall {
        id: None,
        name: tool.to_string(),
        arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
    })
}

fn assemble_tool_calls(acc: &mut Vec<AssembledToolCall>, frags: &[Value]) {
    for frag in frags {
        let idx = frag.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while acc.len() <= idx { acc.push(AssembledToolCall::default()); }
        let row = &mut acc[idx];
        if row.id.is_none() {
            if let Some(s) = frag.get("id").and_then(Value::as_str) {
                row.id = Some(s.to_string());
            }
        }
        if let Some(func) = frag.get("function") {
            if row.name.is_empty() {
                if let Some(n) = func.get("name").and_then(Value::as_str) {
                    row.name = n.to_string();
                }
            }
            if let Some(a) = func.get("arguments").and_then(Value::as_str) {
                row.arguments.push_str(a);
            }
        }
    }
}

/// Baseline guide prepended as the first system message when the session
/// has no user-authored system turn. Primes the model on tool preference
/// — without this guidance most models default to their training-shaped
/// "shell + python" reflex even for tasks where inspect/read/backtrack
/// are the correct gesture.
const HARNESS_GUIDE: &str = "\
You are running on a model with VERY LIMITED USABLE CONTEXT. Every byte \
you keep in this conversation is a byte you don't have for the next \
move. Two consequences:

1. DELEGATE noisy work. `ask` sends a sub-task to a sub-agent running \
   in its own conversation. The sub-agent's intermediate steps stay in \
   ITS context, not yours. You see one clean result plus a list of \
   files it staged.

2. CURATE this conversation. You can write notes inline as an \
   assistant turn — a running TODO, a result table you're building, a \
   list of files you've examined. The conversation IS your workspace; \
   you don't need scratch files for this. Then collapse stale \
   derivation via `backtrack` once the bits you need are in your \
   notes.

The tools, with concrete examples.

ASKING A SUB-AGENT

    ask(request=\"Implement a 512-point FFT in pure Bash. Write \
                 fft.sh, validate with a Python reference, save \
                 the validator as validate.py.\")

You get back:

    [sub-agent session id: kxabc — pass this as `target` to apply/reject/delete]
    Implementation done. Validates within 1e-9 tolerance.
    === changes ===
    root/fft.sh: +234 -0
    root/validate.py: +89 -0

To keep its files:    apply(target=\"kxabc\")
To toss its files but keep the result text:  reject(target=\"kxabc\")
To drop both:         delete(session=\"kxabc\")

MULTIPLE INDEPENDENT SUB-TASKS — issue several `ask`s in the SAME \
assistant turn; each runs in its own sub-agent:

    ask(request=\"Implement bubble sort in pure Bash, save as sort_bubble.sh\")
    ask(request=\"Implement insertion sort in pure Bash, save as sort_insertion.sh\")
    ask(request=\"Implement quicksort in pure Bash, save as sort_quick.sh\")

Three sub-agents work in parallel; you see only the three small results.

FOLLOWING UP on an earlier sub-agent (keeps its context, addresses the \
same agent):

    ask(request=\"Now also benchmark fft.sh against numpy's FFT.\", \
        follow_up=\"kxabc\")

READING + EDITING in YOUR own context (no sub-agent):

    inspect(path=\"/root/fft.sh\")              # paged structure
    read(path=\"/root/fft.sh lines 40..60\")    # exact bytes for quoting
    write(path=\"/root/fft.sh lines 50..50\", content=\"set -x\\n\")

RUNNING actions in your persistent box (compile, test, run binaries):

    shell(script=\"cd /root && bash fft.sh < nums.txt > out.txt\")

NOTE-TAKING — write an assistant turn with no tool call. The text \
becomes part of your context; the next gen sees it. Use this for a \
running TODO, intermediate findings, file inventories. Example:

    Notes so far:
      - fft.sh    : working, validates to 1e-9
      - bench     : 23ms (mine) vs 0.8ms (numpy)
      - TODO      : try radix-4 variant
      - applied   : kxabc, mzdee
      - rejected  : qrwwt (wrong /tmp paths)

SHIPPING the final answer cleanly (collapses derivation; the settled \
conversation reads `<question> → <your answer>` with the messy \
middle gone):

    backtrack(turn_id=\"<the-original-user-turn-id>\", final=true, \
              summary=\"The pure-Bash FFT matches numpy within 1e-9. \
                       Code in fft.sh, validator in validate.py.\")

COMPRESSING a dead-end mid-run (you keep going from the rewound state):

    backtrack(turn_id=\"<the-branch's-first-turn-id>\", final=false, \
              summary=\"Tried radix-4 — bash arithmetic precision too \
                       coarse. Sticking with radix-2.\")

RULES

- USE SHELL FOR ACTIONS (build/compile/install/run/test). Do NOT use \
  shell to `cat` or `ls` something — that's inspect/read's job.
- AFTER `ask` returns, ALWAYS resolve its box (apply / reject / \
  delete). Unresolved sub-agents are announced at the start of each \
  turn — handle them before you settle.
- Pick the tool that matches the gesture, not the one most familiar \
  from training.
";

// ── announcements & sub-agent cleanup ───────────────────────────────────────

/// Walk every oaita session folder, pick out the ones that look like
/// sub-agents spawned BY `outer` — their 0010 user turn carries
/// `from=<outer>` in the filename (`NNNN-id-from.user`). Returns
/// `Vec<inner_session_name>`.
fn spawned_by(outer: &str) -> Vec<String> {
    let mut out = Vec::new();
    let root = crate::paths::oaita_state_home();
    let Ok(rd) = std::fs::read_dir(&root) else { return out; };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() { continue; }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else { continue; };
        if name == outer { continue; }
        let inner_turns = crate::oaita::turns::load_turns(name);
        let Some(first) = inner_turns.first() else { continue; };
        if first.sender.as_deref() == Some(outer) {
            out.push(name.to_string());
        }
    }
    out
}

/// True iff the LAST turn of `session` is a clean assistant turn (no `p`,
/// `c`, or `b` flags) — the "settled" predicate.
fn session_settled(session: &str) -> bool {
    let ts = crate::oaita::turns::load_turns(session);
    ts.last()
        .map(|t| t.kind == "assistant"
            && !t.flags.contains('p')
            && !t.flags.contains('c')
            && !t.flags.contains('b'))
        .unwrap_or(false)
}

/// True iff `outer` has called one of apply/reject/delete on `inner`'s id.
/// We scan only the OUTER session's c.assistant tool-call envelopes.
fn already_resolved(outer: &str, inner: &str) -> bool {
    let turns = crate::oaita::turns::load_turns(outer);
    for t in &turns {
        if t.kind != "assistant" || !t.flags.contains('c') { continue; }
        let Ok(content) = t.read() else { continue; };
        let Ok(v) = serde_json::from_str::<Value>(&content) else { continue; };
        let tool = v.get("tool").and_then(Value::as_str).unwrap_or("");
        let args = v.get("arguments").cloned().unwrap_or(Value::Null);
        let target_match = matches!(tool, "apply" | "reject")
            && args.get("target").and_then(Value::as_str) == Some(inner);
        let session_match = tool == "delete"
            && args.get("session").and_then(Value::as_str) == Some(inner);
        if target_match || session_match { return true; }
    }
    false
}

/// Cheap textual screen for tool-result content that looks like a failure
/// the model would naturally retry — non-zero exit, common error tokens,
/// shell complaints. Used by the behavioural announcement heuristics to
/// decide "productive spin" (clean) vs "unproductive spin" (errory).
fn tool_result_looks_failed(content: &str) -> bool {
    // The shell tool's result text BEGINS with stdout+stderr; if rc was
    // non-zero, common messages appear early. We check the FIRST 2KB —
    // long successful outputs may incidentally contain "error" later
    // (e.g. log lines), but the leader is normally a clean run's data.
    let head: String = content.chars().take(2048).collect();
    let lc = head.to_lowercase();
    let markers = [
        "no such file or directory",
        "permission denied",
        "command not found",
        "syntax error",
        "traceback (most recent call last)",
        "error: unknown tool",
        "fatal error",
        "segfault",
        "core dumped",
        "exited with status",
    ];
    if markers.iter().any(|m| lc.contains(m)) { return true; }
    // sh `set -e` aborts emit lines like "line 12: foo: ...". A bare
    // "error" or "failed" token is too noisy; require a prefix.
    if lc.contains(": error:") || lc.contains(": failed:") { return true; }
    false
}

/// Walk recent turns and decide whether the conversation looks like a
/// productive spin (consecutive successful tool calls — the model has
/// what it needs and should ship) or an unproductive spin (recent tool
/// results are failing — model is in a loop and should rewind).
///
/// Returns the announcement to inject, or None if neither pattern is
/// strong enough. Both reminders are written so the model is told
/// EXACTLY which gesture is correct (backtrack(final=true) to ship,
/// backtrack with a waypoint summary to rewind) — backtrack overloads
/// "ship" and "compact" under one tool, so the reminder has to spell
/// out the right form.
fn backtrack_behavioural_announcement(target: &str) -> Option<String> {
    let turns = crate::oaita::turns::load_turns(target);
    // We need a few rounds of activity before either pattern fires —
    // otherwise the announcement looks gratuitous in short tasks.
    let n_tool = turns.iter().filter(|t| t.kind == "tool").count();
    if n_tool < 3 { return None; }
    // Find the first user turn — that's where backtrack(turn_id=..., final=true,
    // inclusive=false) should rewind TO so the original question stays.
    let first_user_id: Option<String> = turns.iter()
        .find(|t| t.kind == "user")
        .and_then(|t| t.slug.clone());

    // Walk the LAST 5 tool turns. Tally clean vs errory.
    let recent_tools: Vec<&crate::oaita::turns::Turn> =
        turns.iter().filter(|t| t.kind == "tool").rev().take(5).collect();
    let mut errs = 0;
    let mut cleans = 0;
    for t in &recent_tools {
        let content = t.read().unwrap_or_default();
        if tool_result_looks_failed(&content) { errs += 1; } else { cleans += 1; }
    }
    // The productive-cluster case is now handled by the hint mechanism
    // (see hints::productive_cluster_append). evaluate_call appends the
    // marker-deduped body to the tool result that completes the streak,
    // so it fires exactly once per context and is purged naturally on
    // backtrack. Only the unproductive case remains here, since "errors
    // piling up" is a state we want surfaced at gen-time when no fresh
    // tool result is being produced.

    // Errors piling up: list tools NOT used yet (factual inventory) plus
    // both backtrack variants. No suggestion to "step back" — just an
    // enumeration of available levers.
    if errs >= 2 {
        let tid = first_user_id.as_deref().unwrap_or("<first-user-turn-id>");
        let used = used_tools_so_far(&turns);
        let candidates = ["ask", "inspect", "read", "shell", "write"];
        let unused: Vec<&str> = candidates.iter().copied()
            .filter(|n| !used.contains(*n)).collect();
        let mut parts: Vec<String> = Vec::new();
        for t in &unused {
            let blurb = match *t {
                "ask" => "ask(request=…) — send the task to a sub-agent \
                          that works in its own conversation",
                "inspect" => "inspect(path=…) — paged structural view of a \
                              file or directory",
                "read" => "read(path=…) — byte-faithful slice of a file",
                "shell" => "shell(script=…) — run a script in this box",
                "write" => "write(path=…, content=…) — overlay write to a \
                            file or named range",
                _ => continue,
            };
            parts.push(blurb.to_string());
        }
        parts.push(format!(
            "backtrack(turn_id={tid:?}, final=false, summary=…) — rewind \
             to a clean state, keeping `summary` as a waypoint note"));
        parts.push(format!(
            "backtrack(turn_id={tid:?}, final=true, summary=<answer>) \
             — ship; the harness collapses the session to \
             {{question, summary}}"));
        return Some(format!(
            "{errs} of the last {total} tool calls returned error markers. \
             Available levers not yet used in this run, listed for \
             reference:\n  • {bul}",
            total = recent_tools.len(),
            bul = parts.join("\n  • "),
        ));
    }
    None
}

/// Rate-limit gate for the behavioural announcement. A sidecar stamp file
/// in the session dir holds the turn-number at which we last fired; the gate
/// opens once enough turn-numbers have elapsed since that mark. Turn numbers
/// step by `NUM_STEP` (=10), so MIN_GAP_TURN_NUMS=50 ≈ 5 turns.
const ANNOUNCEMENT_STAMP: &str = ".last_announcement";
const MIN_GAP_TURN_NUMS: u32 = 50;

fn announcement_rate_limit_ok(target: &str) -> bool {
    let cur = crate::oaita::turns::load_turns(target).iter()
        .map(|t| t.number).max().unwrap_or(0);
    let stamp = crate::oaita::turns::session_dir(target).join(ANNOUNCEMENT_STAMP);
    let last: u32 = std::fs::read_to_string(&stamp).ok()
        .and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if last == 0 { return true; }
    cur.saturating_sub(last) >= MIN_GAP_TURN_NUMS
}

fn record_announcement(target: &str) {
    let cur = crate::oaita::turns::load_turns(target).iter()
        .map(|t| t.number).max().unwrap_or(0);
    let stamp = crate::oaita::turns::session_dir(target).join(ANNOUNCEMENT_STAMP);
    let _ = std::fs::write(stamp, cur.to_string());
}

/// Tool names the model has called in this session so far (used to compute
/// "unused levers" for the options-inventory announcement).
fn used_tools_so_far(turns: &[crate::oaita::turns::Turn]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for t in turns {
        if t.kind != "assistant" || !t.flags.contains('c') { continue; }
        let Ok(body) = t.read() else { continue; };
        let Ok(v) = serde_json::from_str::<Value>(&body) else { continue; };
        if let Some(name) = v.get("tool").and_then(Value::as_str) {
            out.insert(name.to_string());
        }
    }
    out
}

/// Build the system-message announcement for `outer`'s next generation:
/// a list of completed sub-agents whose result hasn't been resolved.
/// Returns None when nothing's pending — we don't waste tokens on a no-op
/// announcement.
fn unhandled_subtasks_announcement(outer: &str) -> Option<String> {
    let mut unhandled: Vec<String> = Vec::new();
    for inner in spawned_by(outer) {
        if !session_settled(&inner) { continue; }
        if already_resolved(outer, &inner) { continue; }
        unhandled.push(inner);
    }
    if unhandled.is_empty() { return None; }
    Some(format!(
        "HARNESS ANNOUNCEMENT: {n} sub-agent task(s) you launched have \
         completed and are awaiting your resolution. Each holds staged \
         changes in its box and a settled answer. You must now call \
         exactly one of:\n  \
         apply(target=<id>)    fold the sub-agent's staged changes into \
         this plane (commit);\n  \
         reject(target=<id>)   discard the staged changes, keep the result \
         text in conversation;\n  \
         delete(session=<id>)  drop the sub-agent entirely (result already \
         incorporated, no staging needed).\n\
         Unhandled sub-agent ids: {ids}",
        n = unhandled.len(),
        ids = unhandled.join(", "),
    ))
}

/// Sub-agent shutdown sweep. When THIS run settled and we are a sub-agent
/// (depth > 0), every descendant we launched is by definition no longer
/// reachable — the only thing they could ever do is return results to us,
/// and we already shipped ours upward. Kill (in case anything's still
/// running — background sub-agents, partial gens), discard (drop the
/// box's overlay), delete (drop the session folder). Recurse FIRST so
/// the deepest unreachable boxes go first; this also catches unsettled
/// descendants whose own cleanup never ran (because they never settled).
fn cleanup_spawned_subagents(outer: &str) {
    let sarun = crate::oaita::exec::SarunExecutor::new(None).sarun;
    for inner in spawned_by(outer) {
        // Recurse first: clean up anything `inner` itself spawned
        // (settled OR unsettled — settled descendants already cleaned
        // when they settled, but an unsettled inner never ran that
        // sweep, so its children are still live).
        cleanup_spawned_subagents(&inner);
        let inner_box = crate::oaita::exec::box_name(&inner);
        // Kill any live runner inside the box first — for foreground
        // sub-agents this is a no-op (Command::output blocked until
        // exit), but a future background mode would leave processes.
        let _ = std::process::Command::new(&sarun)
            .args([&inner_box, "kill"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        // Discard the box's overlay + sqlar.
        let _ = std::process::Command::new(&sarun)
            .args([&inner_box, "discard"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        // Drop the session folder.
        let _ = std::fs::remove_dir_all(crate::oaita::turns::session_dir(&inner));
        trace::event("run.subagent_cleaned", json!({
            "outer": outer, "inner": inner,
        }));
    }
}

// ── call ────────────────────────────────────────────────────────────────────
/// Evaluate the first UNANSWERED tool call (positional pairing: k-th result
/// answers k-th call in the trailing call/result block). The result is
/// posted back as a `.tool` turn carrying from=<inner-session> when the
/// caller is delegated; otherwise just a plain result turn.
pub fn evaluate_call(spec: &str, set: &Settings,
                     executor: Option<&dyn Executor>) -> Result<Vec<PathBuf>, String> {
    let target = target_segment(spec)?;
    let turns = load_turns(&target);
    let pending = first_pending_call(&turns)
        .ok_or_else(|| "no unanswered tool calls".to_string())?;

    let (tool, arguments) = parse_call_envelope(&pending.read().map_err(|e| e.to_string())?)?;
    trace::event("call.eval", json!({"session": &target, "tool": &tool}));

    // backtrack is special: it REWRITES this session's turn history in
    // place (removes turns, plants the summary). The pending c.assistant
    // call itself is among the turns the rewind will sweep up — writing
    // a tool result for it would resurrect the call/result pair the user
    // just asked us to purge. Dispatch it without emitting a `.tool`
    // turn — the planted summary IS the result, and for final=true it's
    // the settled answer.
    if tool == "backtrack" {
        let _ = dispatch_backtrack(&arguments, &target);
        return Ok(vec![]);
    }

    let mut result_text = dispatch(&tool, &arguments, &target, set, executor, &turns);

    // Productive-cluster hint: streak-driven append. The hint fires when
    // this result is clean AND the four prior tool results were clean too;
    // its marker stays in context so subsequent results don't repeat it.
    // Backtrack purges the carrying turn naturally on rewind.
    let first_user_id = turns.iter().find(|t| t.kind == "user")
        .and_then(|t| t.slug.clone());
    let current_clean = !tool_result_looks_failed(&result_text);
    let extra = crate::oaita::hints::productive_cluster_append(
        &turns, current_clean, first_user_id.as_deref());
    if !extra.is_empty() { result_text.push_str(&extra); }

    // Write the result turn. Sender = self by default; for an inner sub-agent
    // case the executor is the box, and `act` builds its own machinery — for
    // a directly-dispatched call we simply post under the session's own name.
    let n = next_number(&load_turns(&target));
    let mut existing: HashSet<String> = turns.iter()
        .filter_map(|t| t.slug.clone()).collect();
    let slug = new_turn_id(&existing);
    existing.insert(slug.clone());
    let name = turn_filename(n, "tool", Some(&slug), None, "");
    let path = session_dir(&target).join(name);
    fs::write(&path, &result_text).map_err(|e| format!("write result: {e}"))?;
    trace::event("call.result", json!({"session": &target, "tool": &tool, "bytes": result_text.len()}));
    Ok(vec![path])
}

fn first_pending_call(turns: &[Turn]) -> Option<&Turn> {
    // Walk backwards collecting trailing assistant `c` turns and matching
    // them positionally to trailing `.tool` turns.
    let mut calls: Vec<&Turn> = Vec::new();
    let mut results = 0usize;
    for t in turns.iter().rev() {
        if t.kind == "tool" { results += 1; continue; }
        if t.kind == "assistant" && t.flags.contains('c') {
            calls.push(t);
            continue;
        }
        break;
    }
    calls.reverse();
    calls.into_iter().nth(results)
}

fn parse_call_envelope(content: &str) -> Result<(String, Value), String> {
    let v: Value = serde_json::from_str(content)
        .map_err(|e| format!("bad call envelope: {e}"))?;
    let tool = v.get("tool").and_then(Value::as_str)
        .ok_or_else(|| "call envelope missing `tool`".to_string())?.to_string();
    let args = v.get("arguments").cloned().unwrap_or(Value::Null);
    Ok((tool, args))
}

fn dispatch(tool: &str, arguments: &Value, target: &str, set: &Settings,
            executor: Option<&dyn Executor>, turns: &[Turn]) -> String {
    match tool {
        "ask" => dispatch_act(arguments, target, set, executor),
        "shell" => dispatch_shell(arguments, target, executor, turns),
        "inspect" => dispatch_inspect(arguments, target, executor, turns),
        "read" => dispatch_read(arguments, target, executor, turns),
        "write" => dispatch_write(arguments, target, executor, turns),
        "apply" | "reject" => dispatch_box_resolve(tool, arguments, target),
        "backtrack" => dispatch_backtrack(arguments, target),
        "delete" => dispatch_delete(arguments),
        other => format!("error: unknown tool {other:?}{}",
                         crate::oaita::hints::append(turns, &["unknown-tool"])),
    }
}

fn args_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}
fn args_bool(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn dispatch_act(args: &Value, outer: &str, set: &Settings,
                executor: Option<&dyn Executor>) -> String {
    if set.depth >= crate::oaita::tools::max_depth() {
        return "ask: too deep — do the task yourself".to_string();
    }
    let Some(request) = args_str(args, "request") else {
        return "ask: missing required `request`".to_string();
    };
    let data = args_str(args, "data").unwrap_or_default();
    let follow_up = args_str(args, "follow_up").unwrap_or_default();
    // Inner session: if follow_up names a previous act call, reuse its
    // sub-agent id; otherwise mint a new one (folder name is the slug too).
    let inner = if !follow_up.is_empty() {
        // The Python prototype looks up the inner session via the previous
        // call's id; we lean on the model passing the same id forward.
        follow_up
    } else {
        let existing: HashSet<String> = HashSet::new();
        new_turn_id(&existing)
    };
    let _ = fs::create_dir_all(session_dir(&inner));
    // Seed: a user turn from the outer session.
    let seed_content = if data.is_empty() { request.clone() }
                       else { format!("{request}\n\n{data}") };
    let _ = append_turn(&inner, "user", &seed_content, None, Some(outer.to_string()), "", None);
    // Run the inner session in its own box, parented under the agent's
    // OWN persistent shell box (`OAITA-<outer>.OAITA-<inner>` — dotted
    // form, parsed by control.rs register's `rsplit_once('.')`).
    //
    // Why nest instead of letting the sub-agent box be top-level: when
    // the model later calls `apply(target=<inner>)`, sarun lifts the
    // sub-agent's overlay INTO ITS PARENT. With a top-level sub-agent,
    // the parent IS the host filesystem — the agent's own inspect/read
    // (which targets `OAITA-<outer>`) doesn't see the lifted files and
    // returned "not found" right after a successful apply. Nesting under
    // the agent's box makes apply lift into the agent's overlay, which
    // is exactly where the agent's later inspect/read will look.
    let outer_box = box_name(outer);
    let inner_box = box_name(&inner);
    let dotted = format!("{outer_box}.{inner_box}");
    let script = act_script(&inner, set.depth + 1);
    let Some(exe) = executor else {
        return "ask: no executor (sandbox disabled) — cannot delegate".to_string();
    };
    // Make sure the parent box exists FIRST: dotted-name register errors
    // if the prefix segment can't be resolved (see control.rs:644). One
    // no-op materialization is cheap and idempotent.
    let _ = exe.run(&outer_box, "true", /*discard=*/false, /*api_access=*/false);
    // act sub-agents are `oaita run` PROCESSES IN A BOX — they need the
    // engine binary on PATH and proxy access for the LLM call. Pass --api
    // so the runner binds /usr/local/bin/{oaita,sarun} AND admits the
    // box on the proxy gate.
    let r = exe.run(&dotted, &script, false, /*api_access=*/true);
    format_act_result(&r, &inner)
}

fn act_script(inner: &str, child_depth: u32) -> String {
    // OAITA_DEPTH rides into the child process so its `act` calls see the
    // bumped depth (and the exhausted form fires at MAX_DEPTH).
    format!(
        "set -e\n\
         export OAITA_DEPTH={child_depth}\n\
         oaita run {inner}\n\
         oaita tail {inner}\n"
    )
}

fn format_act_result(r: &ExecResult, inner: &str) -> String {
    // The header puts the SESSION ID front and centre — that's what
    // apply/reject/delete want. The box-name form (OAITA-…) was a
    // double-prefix trap when included naively; dispatch_box_resolve now
    // also accepts the box-name form defensively, but framing the result
    // with the session id removes the temptation.
    let mut text = String::new();
    text.push_str(&format!(
        "[sub-agent session id: {inner} — pass this as `target` to \
         apply/reject/delete]\n\n"));
    text.push_str(&r.text);
    text
}

fn dispatch_shell(args: &Value, target: &str, executor: Option<&dyn Executor>,
                  turns: &[Turn]) -> String {
    let Some(script) = args_str(args, "script") else {
        return "shell: missing required `script`".to_string();
    };
    let discard = args_bool(args, "discard");
    let Some(exe) = executor else {
        return "shell: no executor (sandbox disabled) — pass --sarun to enable".to_string();
    };
    // Plain shell tool calls: no API proxy access (the script is user code,
    // not an oaita sub-agent). Cf. dispatch_act which sets api_access=true.
    let r = exe.run(&box_name(target), &script, discard, /*api_access=*/false);
    r.text + &crate::oaita::hints::append(turns, &["shell"])
}

fn dispatch_inspect(args: &Value, target: &str, executor: Option<&dyn Executor>,
                    turns: &[Turn]) -> String {
    let Some(path) = args_str(args, "path") else {
        return "inspect: missing required `path`".to_string();
    };
    let Some(exe) = executor else {
        return "inspect: no executor (sandbox disabled) — file IO requires a box".to_string();
    };
    let loc = parse_locator(&path);
    inspect(&loc, turns, &box_name(target), exe)
}

fn dispatch_read(args: &Value, target: &str, executor: Option<&dyn Executor>,
                 turns: &[Turn]) -> String {
    let Some(path) = args_str(args, "path") else {
        return "read: missing required `path`".to_string();
    };
    let Some(exe) = executor else {
        return "read: no executor (sandbox disabled) — file IO requires a box".to_string();
    };
    let loc = parse_locator(&path);
    read_path(&loc, turns, &box_name(target), exe)
}

fn dispatch_write(args: &Value, target: &str, executor: Option<&dyn Executor>,
                  turns: &[Turn]) -> String {
    let Some(path) = args_str(args, "path") else {
        return "write: missing required `path`".to_string();
    };
    let Some(content) = args_str(args, "content") else {
        return "write: missing required `content`".to_string();
    };
    let Some(exe) = executor else {
        return "write: no executor (sandbox disabled) — file IO requires a box".to_string();
    };
    let force = args_bool(args, "force");
    let loc = parse_locator(&path);
    write_at_locator(&loc, &content, force, turns, &box_name(target), exe)
}

fn dispatch_box_resolve(verb: &str, args: &Value, outer: &str) -> String {
    let Some(target) = args_str(args, "target") else {
        return format!("{verb}: missing required `target`");
    };
    // verb = apply / reject (== discard in sarun terminology). Defer to the
    // sarun CLI which already implements both as control verbs.
    //
    // Accept BOTH a bare session id (`uydur`) AND the OAITA-prefixed box
    // name (`OAITA-UYDUR`). The model has seen both in the act result —
    // the JSON header carries the session id but the human-readable
    // "[from box OAITA-… of …]" line tempts it to grab the box-name form.
    // Double-prefixing was the observed failure mode (`OAITA-OAITA-UYDUR`
    // → "no slopbox"); the case-insensitive prefix check covers it.
    let cmd = if verb == "apply" { "apply" } else { "discard" };
    let sarun = crate::oaita::exec::SarunExecutor::new(None).sarun;
    let inner_box = if target.to_uppercase().starts_with("OAITA-") {
        target.to_uppercase()
    } else {
        box_name(&target)
    };
    // Sub-agent boxes spawned via `act` are nested under the agent's own
    // OAITA-<outer> box (see dispatch_act). Resolve the apply/reject
    // target against the dotted display path so a stale top-level box
    // with the same leaf name doesn't shadow ours, and so apply lifts
    // into the right parent (the agent's box, which inspect/read see).
    let dotted = format!("{}.{inner_box}", box_name(outer));
    let r = std::process::Command::new(&sarun)
        .args([&dotted, cmd]).output();
    match r {
        Ok(o) if o.status.success() => {
            // Stdout looks like `OAITA-X: <count> apply` (or `discard`).
            // Surface the count so the model knows whether anything
            // actually landed — a bare "ok" misled past traces where
            // the sub-agent's writes were under a bwrap-private path
            // (e.g. /tmp) so the box captured nothing, apply reported
            // zero, and the model assumed its sub-agent's work was
            // present and went looking for it.
            let out = String::from_utf8_lossy(&o.stdout);
            let count = out.split_whitespace()
                .find_map(|w| w.parse::<usize>().ok())
                .unwrap_or(usize::MAX);
            match (verb, count) {
                ("apply", 0) => format!(
                    "apply({inner_box}): 0 changes — the sub-agent's box \
                     captured no writes. Common cause: the sub-agent wrote \
                     under /tmp (a bwrap-private tmpfs the overlay doesn't \
                     capture) or under another transient path. Use \
                     /root/… or /home/… for changes that survive apply."),
                ("apply", n) if n != usize::MAX => format!(
                    "apply({inner_box}): {n} file(s) folded into the parent."),
                ("reject", n) if n != usize::MAX => format!(
                    "reject({inner_box}): {n} staged change(s) discarded."),
                _ => format!("{verb}({inner_box}) ok"),
            }
        }
        Ok(o) => format!("{verb}({inner_box}) failed: {}",
                         String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("{verb}: cannot run sarun: {e}"),
    }
}

fn dispatch_backtrack(args: &Value, target: &str) -> String {
    let Some(turn_id) = args_str(args, "turn_id") else {
        return "backtrack: missing `turn_id`".to_string();
    };
    let Some(summary) = args_str(args, "summary") else {
        return "backtrack: missing `summary`".to_string();
    };
    // Default `inclusive=false`: PRESERVE the rewind point. The destructive
    // form (inclusive=true) is opt-in. Earlier the default was true and a
    // model that omitted the argument silently erased the user's original
    // question along with the derivation; that's the wrong default.
    let inclusive = args.get("inclusive").and_then(Value::as_bool).unwrap_or(false);
    let final_answer = args_bool(args, "final");
    let turns = load_turns(target);
    let cut = turns.iter().position(|t| t.slug.as_deref() == Some(turn_id.as_str()));
    let Some(mut cut) = cut else {
        return format!("backtrack: no turn with id {turn_id:?}");
    };
    if !inclusive { cut += 1; }
    // User-kind turns are immutable to backtrack. The first user turn is
    // the original question (the conversation's reason for being); a
    // later user turn carrying `from=<outer>` is a delegation seed from
    // an `act` caller and equally not the model's to destroy. If the cut
    // would sweep up any user turn, COERCE the cut forward past every
    // user turn in the history — silently. The model picked an
    // approximate rewind point; the harness picks the safe one.
    if let Some(last_user) = turns.iter().rposition(|t| t.kind == "user") {
        if cut <= last_user {
            cut = last_user + 1;
        }
    }
    // Remove every turn from `cut` onward.
    let mut removed = 0usize;
    for t in &turns[cut..] {
        if fs::remove_file(&t.path).is_ok() { removed += 1; }
    }
    // Plant the summary as an assistant turn — `b`-flagged unless final.
    let kept = &turns[..cut];
    let n = kept.iter().map(|t| t.number).max().unwrap_or(0) + crate::oaita::turns::NUM_STEP;
    let mut existing: HashSet<String> = kept.iter().filter_map(|t| t.slug.clone()).collect();
    let slug = new_turn_id(&existing);
    existing.insert(slug.clone());
    let flags = if final_answer { "" } else { "b" };
    let name = turn_filename(n, "assistant", Some(&slug), None, flags);
    let path = session_dir(target).join(name);
    let _ = fs::write(&path, &summary);
    format!("backtrack: removed {removed} turns; {} planted at {}",
            if final_answer { "answer" } else { "waypoint" },
            path.display())
}

fn dispatch_delete(args: &Value) -> String {
    let Some(session) = args_str(args, "session") else {
        return "delete: missing `session`".to_string();
    };
    let dir = session_dir(&session);
    let _ = fs::remove_dir_all(&dir);
    // Also discard the session's box (if any).
    let sarun = crate::oaita::exec::SarunExecutor::new(None).sarun;
    let _ = std::process::Command::new(&sarun)
        .args([&box_name(&session), "discard"]).output();
    format!("delete: dropped session {session}")
}

// ── run ─────────────────────────────────────────────────────────────────────
/// run = drive call/gen until the tail is a CLEAN assistant turn (no `p`, no
/// `c`, no `b`). Termination is engine-managed: the budget pool (see
/// `oaita::budget`) is debited on every `api.proxy` conn, and the
/// upstream LLM call returns HTTP 503 when the session or any ancestor
/// has run out — `generate` surfaces that as `Err("budget exhausted...")`
/// and we exit cleanly. The box stays alive in intermediate state; a
/// follow-up `oaita run <session> --max-steps N` adds to the same pool
/// and resumes.
pub fn run_to_completion(spec: &str, set: &Settings,
                         executor: Option<&dyn Executor>)
                         -> Result<Vec<PathBuf>, String> {
    let target = target_segment(spec)?;
    let mut produced: Vec<PathBuf> = Vec::new();
    trace::event("run.start", json!({"session": &target}));
    loop {
        let turns = load_turns(&target);
        // Settled? — last turn is assistant with no p/c/b flags.
        if let Some(last) = turns.last() {
            if last.kind == "assistant"
                && !last.flags.contains('p')
                && !last.flags.contains('c')
                && !last.flags.contains('b') {
                trace::event("run.settled", json!({"session": &target}));
                cleanup_spawned_subagents(&target);
                return Ok(produced);
            }
            if first_pending_call(&turns).is_some() {
                let mut r = evaluate_call(spec, set, executor)?;
                produced.append(&mut r);
                continue;
            }
        }
        // Generate. If the engine refuses (budget chain hit zero), the
        // upstream HTTP call returns 503 and `generate` propagates a
        // matching error — exit cleanly so the user can grant more.
        match generate(spec, set) {
            Ok(mut r) => produced.append(&mut r),
            Err(e) if e.contains("budget exhausted") => {
                trace::event("run.budget_exhausted",
                    json!({"session": &target, "detail": &e}));
                cleanup_spawned_subagents(&target);
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
}
