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
               `act`(request=…) delegates to a sub-agent in its own box; \
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

    // ── shell composition reminders ───────────────────────────────────────
    Hint {
        id: "shell",
        marker: "--- hint: shell ---",
        body: "shell runs scripts inside your persistent box (writes stage \
               for review — see `box:<id>` via inspect). Related moves you \
               may not have considered: \
               `act`(request=…) spawns a SUB-AGENT in a fresh box for \
               parallel or one-off work whose intermediate state you don't \
               want to clutter this conversation — it returns only a \
               result; \
               `backtrack`(turn_id=…, inclusive=false, final=false, \
               summary=…) compresses a long thinking session into a one-\
               line waypoint and continues from there with the original \
               question intact; `backtrack` with `final=true` ships the \
               summary as the answer and collapses the derivation.",
    },
];

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
