// First-time hints — additive, additive only, dedup-by-context.
//
// Each tool result kind (a directory listing, a numbered-line file view, a
// syntax-axis enumeration, an unknown-tool error, etc.) carries a short hint
// explaining what the output is and what it composes with. The hint is
// EMBEDDED INTO THE OUTPUT once per context: a literal marker line stays in
// the conversation, the next call's scan finds the marker and skips re-
// emitting the body. Costs no system-prompt budget and degrades gracefully
// after a backtrack (the rewound turns take the marker with them, hint fires
// fresh on the next call — exactly the "context is fresh, re-teach" path).
//
// Hint bodies are FACTUAL, not directive. They describe what the model is
// looking at and which other tools relate to it; they don't push an action.
// The wording style is the same as the inspect cursor footer convention —
// `--- hint: <id> ---` — so it reads as part of the output, not as out-of-
// band scaffolding the model has to skip.
//
// Size budget: hints are appended AFTER any RESULT_BUDGET clamping of the
// main payload. The payload keeps its full quota; hints are extra bytes
// from the harness, not the user's question, so they don't compete.

use std::collections::HashSet;

use crate::oaita::turns::Turn;

pub struct Hint {
    pub id: &'static str,
    /// Literal marker that gets embedded in the output. The marker IS the
    /// signature: a future turn's content containing this string means the
    /// hint already lives in context, so the next call skips it.
    pub marker: &'static str,
    /// The human-readable body — appended after the marker on a new line.
    pub body: &'static str,
}

const HINTS: &[Hint] = &[
    // ── inspect output shapes ──────────────────────────────────────────────
    Hint {
        id: "inspect-dir",
        marker: "--- hint: inspect-dir ---",
        body: "Each row is `<kind>  <name>` (kind ∈ dir/file/link/other). \
               Address a child entry by calling `inspect` on the full path \
               (current path + `/` + name). `inspect <path> entries A..B` \
               pages a slice of a large directory.",
    },
    Hint {
        id: "inspect-file-lines",
        marker: "--- hint: inspect-file-lines ---",
        body: "Lines are 1-based. `inspect <path> lines A..B` jumps to a \
               specific range, `inspect <path> around N` centres on line N. \
               `read <path>` returns the same bytes without line numbers, \
               framing-free, when you want to quote the file verbatim or \
               feed it to `write`.",
    },
    Hint {
        id: "inspect-syntax",
        marker: "--- hint: inspect-syntax ---",
        body: "These are tree-sitter items. Address one with the locator \
               `<path> symbol <name>[N]` — N is the 1-based occurrence and \
               disambiguates same-name collisions across nested scopes. The \
               same locator works in `inspect` (focus the source), `read` \
               (raw bytes), and `write` (splice the symbol's byte range with \
               new content). For deeper edits inside one symbol, fall back \
               to `<path> lines A..B` using the line range shown.",
    },
    Hint {
        id: "inspect-cursor",
        marker: "--- hint: inspect-cursor ---",
        body: "The cursor footer (`--- inspect: ... — keys: first/previous/\
               next/last`) means more pages exist. Continue with \
               `inspect first|previous|next|last` (no path needed — the \
               cursor lives in the result turns). An `--- END of ... ---` \
               line means you've seen everything.",
    },
    Hint {
        id: "inspect-box",
        marker: "--- hint: inspect-box ---",
        body: "`box:<id>` shows the change set staged inside sub-agent box \
               <id> — read-only here. `box:<id>/<file>` pages one file's \
               staged diff. To commit the box's writes into your plane, \
               call `apply(target=<id>)`; to drop them call \
               `reject(target=<id>)`.",
    },

    // ── tool meta ─────────────────────────────────────────────────────────
    Hint {
        id: "unknown-tool",
        marker: "--- hint: unknown-tool ---",
        body: "Tools available in this harness: \
               `ask`(request=…) sends the task to a sub-agent in its own \
               conversation; \
               `shell`(script=…) runs a script in your box (writes stage \
               for review); `inspect`/`read`/`write` address files via a \
               shared locator grammar (path, `lines A..B`, `around N`, \
               `symbol <name>[N]`, or page keys); `apply`/`reject`(target=\
               <id>) commit or drop a sub-agent box; `backtrack`(turn_id=…, \
               inclusive=…, final=…, summary=…) rewinds with a waypoint \
               note OR ships the final answer; `delete`(session=…) drops a \
               sub-agent session entirely. Names outside this set raise \
               this error.",
    },

    // ── conditional, streak-driven ───────────────────────────────────────
    //
    // productive-cluster fires from inside evaluate_call when the current
    // tool result and at least four preceding tool results are all clean
    // (no error markers). The body's `<TID>` placeholder is replaced with
    // the session's first-user-turn id before emission. Same marker-in-
    // context dedup as the static hints — fires once per context, then
    // suppresses until backtrack drops the carrying turn.
    Hint {
        id: "productive-cluster",
        marker: "--- hint: productive-cluster ---",
        body: "The last 5 tool calls have all returned without error \
               markers. If you now have the answer the original request \
               asked for, the closing gesture is `backtrack(turn_id=<TID>, \
               final=true, summary=<your-answer>)` — it ships the summary \
               as the settled answer and collapses the intermediate \
               derivation. (User turns stay in place; the derivation turns \
               between go.)",
    },

    // ── shell composition reminders ───────────────────────────────────────
    Hint {
        id: "shell",
        marker: "--- hint: shell ---",
        body: "shell runs scripts inside your persistent box (writes stage \
               for review — see `box:<id>` via inspect). Related moves you \
               may not have considered: \
               `ask`(request=…) sends the task to a SUB-AGENT in a fresh \
               box for parallel or one-off work whose intermediate state \
               you don't want to clutter this conversation — it returns \
               only a result; \
               `backtrack`(turn_id=…, final=false, summary=…) compresses \
               a long thinking session into a one-line waypoint and \
               continues from there with the original question intact; \
               `backtrack` with `final=true` ships the summary as the \
               answer and collapses the derivation.",
    },
];

/// Streak-driven productive-cluster hint. Returns the templated body to
/// append to the CURRENT tool result iff:
///   * `current_clean` is true (the result we're about to emit is itself
///     clean — no error markers);
///   * at least 5 SUBSTANTIVE clean tool turns exist in the last 8 — a
///     non-trivial result, not a 1-line "ok" or empty body — and at most
///     2 of those last 8 errored. The "non-trivial result" gate stops the
///     hint from arming after a write/write/shell streak that hasn't yet
///     verified anything; the "≤2 errors" relaxation stops it from being
///     locked out forever by a long-running task with occasional misses;
///   * the productive-cluster marker isn't already in any prior turn.
/// `first_user_id` templates into the suggested backtrack invocation. When
/// any condition fails, returns an empty string.
pub fn productive_cluster_append(turns: &[Turn], current_clean: bool,
                                 first_user_id: Option<&str>) -> String {
    if !current_clean { return String::new(); }
    let recent_tools: Vec<&Turn> = turns.iter()
        .filter(|t| t.kind == "tool").rev().take(8).collect();
    if recent_tools.len() < 5 { return String::new(); }
    let mut substantive_clean = 0usize;
    let mut errs = 0usize;
    for t in &recent_tools {
        let body = t.read().unwrap_or_default();
        if looks_failed(&body) {
            errs += 1;
        } else if is_substantive(&body) {
            substantive_clean += 1;
        }
    }
    if substantive_clean < 5 || errs > 2 { return String::new(); }
    // Marker dedup against the full session.
    let marker = HINTS.iter().find(|h| h.id == "productive-cluster")
        .map(|h| h.marker).unwrap_or("");
    for t in turns {
        if let Ok(content) = t.read() {
            if content.contains(marker) { return String::new(); }
        }
    }
    let Some(h) = HINTS.iter().find(|h| h.id == "productive-cluster") else {
        return String::new();
    };
    let tid = first_user_id.unwrap_or("<first-user-turn-id>");
    let body = h.body.replace("<TID>", &format!("{tid:?}"));
    format!("\n\n{}\n{body}", h.marker)
}

/// A tool result is "substantive" when there's something to chew on —
/// at least a few non-trivial lines that aren't just status framing. We
/// use this to gate the productive-cluster hint so it doesn't arm after
/// a write/write/shell streak that produced nothing the model could
/// verify. The thresholds are deliberately conservative: short status
/// lines like `write: foo.py: replaced whole file (0 -> 83 lines)` or
/// the bare `0` rc message from a side-effecty shell call shouldn't
/// count as "I've verified my work."
fn is_substantive(content: &str) -> bool {
    // Strip the harness-injected hint tail (everything after a `--- hint:`
    // marker line) before counting — those are our own additions, not
    // signal the model produced.
    let core = content.split("\n\n--- hint:").next().unwrap_or(content);
    let trimmed = core.trim();
    if trimmed.len() < 80 { return false; }
    let line_count = trimmed.lines().filter(|l| !l.trim().is_empty()).count();
    line_count >= 3
}

/// The same "tool result looks like an error" predicate the unproductive
/// announcement uses. Inlined here to keep the hint module self-contained.
fn looks_failed(content: &str) -> bool {
    let lc = content.to_ascii_lowercase();
    let markers = &[
        "no such file or directory", "permission denied", "command not found",
        "syntax error", "traceback (most recent call last)",
        "error: unknown tool", "fatal error", "segfault", "core dumped",
        "exited with status",
    ];
    if markers.iter().any(|m| lc.contains(m)) { return true; }
    if lc.contains(": error:") || lc.contains(": failed:") { return true; }
    false
}

/// Append all hints in `ids` whose marker isn't already in any session turn.
/// Returns an empty string when every requested hint has been shown already
/// (or the ids are unknown). The returned string starts with a blank line so
/// callers can concatenate it directly to a tool result.
pub fn append(turns: &[Turn], ids: &[&str]) -> String {
    let mut seen: HashSet<&str> = HashSet::new();
    for t in turns {
        let Ok(content) = t.read() else { continue; };
        for h in HINTS {
            if content.contains(h.marker) {
                seen.insert(h.id);
            }
        }
    }
    let mut out = String::new();
    for id in ids {
        if seen.contains(*id) { continue; }
        let Some(h) = HINTS.iter().find(|h| h.id == *id) else { continue; };
        out.push_str("\n\n");
        out.push_str(h.marker);
        out.push('\n');
        out.push_str(h.body);
    }
    out
}
