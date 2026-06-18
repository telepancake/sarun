// Tool registry — one declarative ROW per tool, two-faced: an outward
// `tools[]` schema entry the LLM sees, and an inward `run_location` tag the
// dispatcher uses to know whether the call runs IN-BOX (a script in the
// session's sarun overlay) or as a MANAGER operation on the box itself
// (apply/reject/backtrack — fixed templates, never free-form).
//
// The seven rows match the Python prototype (oaita branch):
//   act       — recursive sub-agent (the meta-tool; processes in BOXES, not
//               in-process recursion); exhausted form at MAX_DEPTH
//   shell     — sh -c SCRIPT in the persistent box (or throwaway if discard=true)
//   inspect   — paged structure of the thing at `path` (cursor-keyed)
//   read      — raw bytes of a file/slice (use after inspect to quote precisely)
//   apply     — fold a sub-agent's staged changes INTO this plane
//   reject    — discard a sub-agent's staged changes
//   backtrack — rewind this conversation; waypoint OR finished answer

use serde_json::{json, Value};

pub const META_TOOL_NAME: &str = "act";
pub const DEFAULT_CAPABILITIES: &str =
    "general assistance (shell, inspect, delegation)";

/// Delegation depth cap — a top-level conversation is depth 0; each `act`
/// sub-agent is one deeper. Past it `act` stays VISIBLE in the schema but
/// returns "too deep" so the model is told the capability exists, just
/// exhausted, and does the work itself instead of spinning.
pub fn max_depth() -> u32 {
    std::env::var("OAITA_MAX_DEPTH").ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3)
}

/// Hard ceiling on a tool RESULT turn's size, in bytes. Rendering ladders
/// fall back to terser forms until one fits; the FULL stream/diff stays in
/// the box so nothing is lost — only what flows back into the LLM context
/// is bounded.
pub const RESULT_BUDGET: usize = 8 * 1024;
/// Within result-budget, how much may a CHANGES summary take vs the OUTPUT
/// it accompanies. The output is the model's read-through; the changes
/// summary fills whatever remains.
pub const CHANGES_BUDGET: usize = 2 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLocation { InBox, Manager }

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    pub parameters: Value,
    pub run_location: RunLocation,
}

impl ToolSpec {
    pub fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

fn act_spec(capabilities: &str, exhausted: bool) -> ToolSpec {
    let description = if exhausted {
        "Delegation depth is exhausted here. You can still call this, but it \
         will return 'too deep' — so just do the task yourself.".to_string()
    } else {
        format!(
            "Use your capabilities by describing what you want. Put a \
             natural-language description in `request` and any data in `data`. \
             Your capabilities: {capabilities}. To follow up on an earlier \
             result, set `follow_up` to that call's turn-id.")
    };
    ToolSpec {
        name: META_TOOL_NAME,
        description,
        parameters: json!({
            "type": "object",
            "properties": {
                "request": {"type": "string",
                            "description": "Natural-language description of what you want done."},
                "data":    {"type": "string",
                            "description": "Any data or payload the request operates on."},
                "follow_up": {"type": "string",
                              "description": "Turn-id of a previous `act` call to continue as a follow-up."},
            },
            "required": ["request"],
        }),
        run_location: RunLocation::InBox,
    }
}

fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell",
        description:
            "Run a shell script in this conversation's persistent sandbox box. \
             The script is executed with `sh -c`; stdout/stderr are captured \
             and returned, followed by a summary of any file changes the run \
             staged in the box (changes stay STAGED until resolved — they do \
             not touch the host). Set `discard` true for a read-only look: \
             the script runs in a throwaway box discarded right after — \
             output comes back, nothing stays staged. \
             IMPORTANT: /tmp is a FRESH tmpfs on every shell call (it is not \
             part of the overlay) — files written there in one call are GONE \
             by the next call. To persist state ACROSS shell calls, write to \
             /root, $HOME, /var, or any other path (those go through the \
             overlay and persist for the session).".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "script":  {"type": "string", "description": "The shell script to run (no shebang)."},
                "discard": {"type": "boolean", "description": "Run read-only: a throwaway box, discarded after the run. Default false."},
            },
            "required": ["script"],
        }),
        run_location: RunLocation::InBox,
    }
}

fn inspect_spec() -> ToolSpec {
    ToolSpec {
        name: "inspect",
        description:
            "Page through the structure of the thing at `path`: a directory \
             (entries, kind + name), a text file (numbered lines), or \
             box:<sub-agent id> — its STAGED change set (box:<id>/<file> \
             pages that file's staged diff; this is the one thing shell \
             cannot show). Append \"lines A..B\" / \"entries A..B\" or \
             \"around N\" to a path to jump. A reduced page ends with a \
             cursor footer; continue it by calling inspect with just next, \
             previous, first or last.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string",
                         "description": "Locator: path | path lines A..B | path around N | box:<id>[/<file>] | next | previous | first | last."},
            },
            "required": ["path"],
        }),
        run_location: RunLocation::InBox,
    }
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read",
        description:
            "The RAW text of a file or slice — exactly the content, no line \
             numbers, no framing; use after inspect to quote precisely. `path` \
             takes inspect's locators: a file path, optionally with \"lines \
             A..B\" or \"around N\", or a page key (next/previous/first/last) \
             returning the last paged window raw.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Locator (inspect's grammar)."},
            },
            "required": ["path"],
        }),
        run_location: RunLocation::InBox,
    }
}

fn apply_spec() -> ToolSpec {
    ToolSpec {
        name: "apply",
        description:
            "APPLY a sub-agent's staged changes: fold everything its sandbox \
             box accumulated (files, session edits) into this plane, then \
             remove the box. `target` is the sub-agent id (the from-sender of \
             its result turn). Review the change summary first — applying is \
             the commit.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "description": "Sub-agent session id whose box to apply."},
            },
            "required": ["target"],
        }),
        run_location: RunLocation::Manager,
    }
}

fn reject_spec() -> ToolSpec {
    ToolSpec {
        name: "reject",
        description:
            "REJECT a sub-agent's staged changes: discard everything its \
             sandbox box accumulated and remove the box. `target` is the \
             sub-agent id (the from-sender of its result turn). Its result \
             text stays in this conversation; only the staged changes vanish.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "description": "Sub-agent session id whose box to discard."},
            },
            "required": ["target"],
        }),
        run_location: RunLocation::Manager,
    }
}

fn backtrack_spec() -> ToolSpec {
    ToolSpec {
        name: "backtrack",
        description:
            "Rewind this conversation: discard every turn from `turn_id` \
             onward (this very call included; inclusive=false keeps the named \
             turn itself) and put `summary` — your condensed record of the \
             discarded branch, e.g. \"tried X; dead end: Y\" — in its place. \
             By default the summary is a WAYPOINT and work CONTINUES from the \
             rewound context (shed a failed approach, compact a stretch you \
             no longer need verbatim); with final=true it is your FINISHED \
             ANSWER and the run settles on it (collapse a messy arc into the \
             clean result).".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "turn_id":   {"type": "string", "description": "Rewind point: turns from here onward are discarded."},
                "summary":   {"type": "string", "description": "The condensed record of the discarded branch — all that is carried forward."},
                "inclusive": {"type": "boolean", "description": "Default true: the named turn is discarded too. False keeps it and discards only what follows."},
                "final":     {"type": "boolean", "description": "Default false: the summary is a waypoint and you keep working. True: the summary is your finished answer."},
            },
            "required": ["turn_id", "summary"],
        }),
        run_location: RunLocation::Manager,
    }
}

fn delete_spec() -> ToolSpec {
    ToolSpec {
        name: "delete",
        description:
            "Delete a finished sub-agent session entirely (its turns and its \
             sandbox box) once its result is banked. For rewinding your OWN \
             context, use backtrack.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "session": {"type": "string", "description": "Sub-agent session name to delete."},
            },
            "required": ["session"],
        }),
        run_location: RunLocation::Manager,
    }
}

/// The v1 tool rows — keyed by name so the dispatcher can resolve a call.
/// `depth` (this context's delegation depth) flattens `act` to its exhausted
/// form at `MAX_DEPTH` — the row stays so the model sees the capability, but
/// a call returns "too deep".
pub fn tool_registry(capabilities: Option<&str>, depth: u32)
    -> std::collections::BTreeMap<&'static str, ToolSpec>
{
    let caps = capabilities.unwrap_or(DEFAULT_CAPABILITIES);
    let mut r = std::collections::BTreeMap::new();
    r.insert(META_TOOL_NAME, act_spec(caps, depth >= max_depth()));
    r.insert("shell", shell_spec());
    r.insert("inspect", inspect_spec());
    r.insert("read", read_spec());
    r.insert("apply", apply_spec());
    r.insert("reject", reject_spec());
    r.insert("backtrack", backtrack_spec());
    r.insert("delete", delete_spec());
    r
}

/// Render the tools[] array the OpenAI API expects.
pub fn tools_array(capabilities: Option<&str>, depth: u32) -> Value {
    let reg = tool_registry(capabilities, depth);
    Value::Array(reg.values().map(|s| s.schema()).collect())
}

/// Result of executing a tool call. The on-disk turn carries the rendered
/// `text`; `raw_output` and `patch` are the FULL versions kept for inspect.
#[derive(Debug, Default, Clone)]
pub struct ExecResult {
    pub text: String,
    pub raw_output: String,
    pub patch: String,
    pub rc: i32,
}

// ── result budget — fit a list of (label, body) renderings into RESULT_BUDGET
//    by trying them in order until one fits. ───────────────────────────────
pub fn fit_to_budget(renderings: &[String], budget: usize) -> String {
    for r in renderings {
        if r.len() <= budget { return r.clone(); }
    }
    // None fit — clamp the LAST (presumed terse-most) rendering to the budget.
    let last = renderings.last().cloned().unwrap_or_default();
    let s: String = last.chars().take(budget).collect();
    s
}

/// Trim raw output down to budget bytes using a head+tail-around-elision
/// ladder. The shell `text` is `output\n\n=== changes ===\n<changes>` —
/// callers concat afterwards.
pub fn fit_output(output: &str, budget: usize) -> String {
    if output.len() <= budget { return output.to_string(); }
    let head_n = budget / 2;
    let tail_n = budget / 2 - 80; // room for the elision marker
    let head: String = output.chars().take(head_n).collect();
    let tail: String = output.chars().rev().take(tail_n).collect::<String>()
        .chars().rev().collect();
    format!("{head}\n…[{} bytes elided]…\n{tail}",
            output.len().saturating_sub(head.len() + tail.len()))
}

/// Summarise a unified diff into per-file then per-dir then totals lines.
pub fn summarize_patch(patch: &str, budget: usize) -> String {
    if patch.len() <= budget { return patch.to_string(); }
    // Per-file +/- summary.
    let mut files: Vec<(String, usize, usize)> = Vec::new(); // (path, +, -)
    let mut cur_path: Option<String> = None;
    let mut adds = 0usize;
    let mut dels = 0usize;
    let mut total_adds = 0usize;
    let mut total_dels = 0usize;
    let push_cur = |files: &mut Vec<(String, usize, usize)>,
                    cur: &mut Option<String>, a: &mut usize, d: &mut usize| {
        if let Some(p) = cur.take() {
            files.push((p, *a, *d));
            *a = 0; *d = 0;
        }
    };
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            push_cur(&mut files, &mut cur_path, &mut adds, &mut dels);
            cur_path = Some(rest.to_string());
        } else if line.starts_with("+") && !line.starts_with("+++") {
            adds += 1; total_adds += 1;
        } else if line.starts_with("-") && !line.starts_with("---") {
            dels += 1; total_dels += 1;
        }
    }
    push_cur(&mut files, &mut cur_path, &mut adds, &mut dels);
    // Try the per-file rendering first.
    let mut per_file: Vec<String> = files.iter()
        .map(|(p, a, d)| format!("{p}: +{a} -{d}"))
        .collect();
    per_file.sort();
    let per_file_text = per_file.join("\n");
    if per_file_text.len() <= budget { return per_file_text; }
    // Per-directory rollup.
    let mut by_dir: std::collections::BTreeMap<String, (usize, usize, usize)> =
        Default::default();
    for (p, a, d) in &files {
        let dir = std::path::Path::new(p).parent()
            .map(|x| x.display().to_string()).unwrap_or_else(|| ".".into());
        let e = by_dir.entry(dir).or_insert((0, 0, 0));
        e.0 += 1; e.1 += a; e.2 += d;
    }
    let per_dir_text = by_dir.iter()
        .map(|(d, (n, a, m))| format!("{d}/: {n} files +{a} -{m}"))
        .collect::<Vec<_>>().join("\n");
    if per_dir_text.len() <= budget { return per_dir_text; }
    format!("{} files +{total_adds} -{total_dels}", files.len())
}
