// Tool registry — one declarative ROW per tool, two-faced: an outward
// `tools[]` schema entry the LLM sees, and an inward `run_location` tag the
// dispatcher uses to know whether the call runs IN-BOX (a script in the
// session's sarun overlay) or as a MANAGER operation on the box itself
// (apply/reject/backtrack — fixed templates, never free-form).
//
// The seven rows match the Python prototype (oaita branch):
//   ask       — recursive sub-agent (the meta-tool; processes in BOXES, not
//               in-process recursion); exhausted form at MAX_DEPTH
//   shell     — sh -c SCRIPT in the persistent box (or throwaway if discard=true)
//   inspect   — paged structure of the thing at `path` (cursor-keyed)
//   read      — raw bytes of a file/slice (use after inspect to quote precisely)
//   apply     — fold a sub-agent's staged changes INTO this plane
//   reject    — discard a sub-agent's staged changes
//   backtrack — rewind this conversation; waypoint OR finished answer

use serde_json::{json, Value};

pub const META_TOOL_NAME: &str = "ask";
pub const DEFAULT_CAPABILITIES: &str =
    "general assistance (shell, inspect, delegation)";

/// Delegation depth cap — a top-level conversation is depth 0; each `ask`
/// sub-agent is one deeper. Past it `ask` stays VISIBLE in the schema but
/// returns "too deep" so the model is told the capability exists, just
/// exhausted, and does the work itself instead of spinning.
pub const MAX_DEPTH: u32 = 3;

pub fn max_depth() -> u32 { MAX_DEPTH }

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
            "ASK A SUB-AGENT to do something for you. Your context is \
             SHORT — every byte of intermediate thinking you do here is \
             a byte you don't have for the next move. `ask` sends the \
             noisy work to a fresh sub-agent whose multi-step \
             exploration stays in ITS conversation, not yours. You get \
             back ONE clean result text plus a list of files it staged. \
             The grind happens elsewhere; you see only the answer. \
             \
             Use it for: anything multi-step (build-and-test loops, \
             pattern-search across files, debugging cycles), anything \
             you want to try \
             several variants of (issue several `ask`s in the same \
             assistant turn, each one with a different `request` — they \
             run as independent sub-agents), or tasks whose result is \
             small but whose work is large. If you can't fit the task's \
             intermediate state in your head, don't fit it in your \
             context — ask. \
             \
             AFTER the sub-agent returns, resolve its box: \
             `apply(target=<id>)` keeps its files, \
             `reject(target=<id>)` tosses the files but keeps the result \
             text, `delete(session=<id>)` drops both. Unresolved \
             sub-agents are announced to you at the start of each turn. \
             \
             To CONTINUE a previous sub-agent (its context persists), \
             set `follow_up` to that earlier `ask` call's turn-id — \
             you'll address the same sub-agent again. \
             \
             OPTIONAL `max_steps`: cap THIS sub-agent at N LLM-call \
             turns. If you omit it, the sub-agent is uncapped — it \
             only stops when YOUR pool runs out (whatever turns YOU \
             have left, it draws from). Set max_steps when you want a \
             specific budget for a sub-task (`max_steps=10` for a \
             quick lookup, leave unset for a deep investigation that \
             should run until your own budget gives out). On a \
             `follow_up`, max_steps EXTENDS the existing sub-agent's \
             pool by N more turns. \
             \
             Put the natural-language request in `request` and any \
             input data in `data`. Your capabilities are: {capabilities}.")
    };
    ToolSpec {
        name: META_TOOL_NAME,
        description,
        parameters: json!({
            "type": "object",
            "properties": {
                "request": {"type": "string",
                            "description": "What you want the sub-agent to do, in natural language."},
                "data":    {"type": "string",
                            "description": "Input data the sub-agent operates on (optional)."},
                "follow_up": {"type": "string",
                              "description": "To address an existing sub-agent again, pass that earlier `ask` call's turn-id."},
                "max_steps": {"type": "integer",
                              "description": "Optional cap: this sub-agent stops after at most N LLM-call turns. Omit for uncapped (draws from your own pool)."},
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
            "Run a shell script for ACTIONS and RUNTIME work — building, \
             compiling, installing packages, running tests, invoking other \
             binaries, anything that changes state or needs a real process. \
             For READING files or BROWSING the filesystem, use `inspect` \
             and `read` instead — they're faster, paged, cursor-keyed, and \
             don't burn a fresh sandbox box per call. Don't use shell to \
             `cat foo.txt` or `ls /etc` — those are inspect/read's job. \
             \
             Mechanics: the script is executed with `sh -c` in this \
             conversation's persistent sandbox box. stdout/stderr are \
             captured and returned, followed by a summary of any file \
             changes the run staged in the box (changes stay STAGED until \
             you resolve them — they do not touch the host). Set `discard` \
             true for a read-only look: the script runs in a throwaway box \
             discarded right after. \
             \
             IMPORTANT: /tmp is a FRESH tmpfs per shell call (not part of \
             the overlay) — files written to /tmp in one call are GONE by \
             the next. To persist state across calls, write to /root, \
             $HOME, /var, or any other path under the overlay.".to_string(),
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
            "Use this for FILESYSTEM NAVIGATION and STRUCTURE — anything \
             you'd normally do with `ls`, `find`, `head`, line-numbered \
             `cat`, or `grep -n`. inspect is PAGED so you won't get an \
             8KB blob back, KEYED so you can ask for `next` page instead \
             of repeating the path, and TYPE-AWARE so it formats a dir \
             entry list as kind+name, a text file as numbered lines, and \
             a box:<id> as the staged change set (the one view shell \
             literally cannot give you). \
             \
             Locators: `<path>` for the whole thing, `<path> lines A..B` \
             to jump to file lines A..B, `<path> entries A..B` to jump to \
             directory entries A..B, `<path> around N` for a small window \
             centred on line N, `<path> symbols` to list the named \
             definitions in a source file (tree-sitter; .rs/.py/.sh/.bash), \
             `<path> symbol <name>[N]` to focus on the Nth occurrence of a \
             definition by name, or `box:<id>[/<file>]` for staged diffs. \
             A reduced page ends with a cursor footer; continue it by \
             calling inspect with just `next`, `previous`, `first`, or \
             `last` (no path needed — the cursor lives in the result \
             turns). The cursor footer says either END (you've seen \
             everything) or shows the available page keys — read it \
             and decide whether you have what you need before paginating \
             further.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string",
                         "description": "Locator: path | path lines A..B | path around N | path symbols | path symbol <name>[N] | box:<id>[/<file>] | next | previous | first | last."},
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
            "Use this when you need to QUOTE FILE CONTENT VERBATIM — the \
             raw bytes, no line numbers, no framing. inspect shows you \
             structure and numbered lines (use it first to find WHERE in \
             the file you want); read gives you the exact text from there \
             so you can include it in your reply unaltered. \
             \
             `path` takes inspect's locator grammar: a file path, \
             optionally with `lines A..B` or `around N` for a line slice, \
             `symbol <name>[N]` for a named definition's source (.rs/.py/\
             .sh/.bash), or a page key (next/previous/first/last) \
             returning the last paged window raw. Use this instead of \
             `shell` + `cat`/`sed -n`/`awk` — those add line numbers, \
             framing, or formatting noise.".to_string(),
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

fn write_spec() -> ToolSpec {
    ToolSpec {
        name: "write",
        description:
            "Use this to MODIFY a file by NAMING THE THING TO REPLACE — the \
             same locator grammar `inspect` uses to ENUMERATE things, `write` \
             uses to REPLACE them. inspect says \"here are the lines / \
             entries / sections of this thing\"; read quotes one of them \
             verbatim; write swaps one out for new content. The whole file, \
             a line range, or the window around a line — anything inspect \
             can name. \
             \
             `path` takes inspect's locator grammar: a bare path replaces \
             the whole file; `path lines A..B` replaces lines A..B \
             (line counts may differ — the file grows/shrinks); `path \
             around N` replaces the same window inspect would have shown \
             around line N; `path symbol <name>[N]` replaces the Nth \
             named definition in a source file (.rs/.py/.sh/.bash — \
             tree-sitter resolves it); a page key (next/previous/first/\
             last) replaces the slice the most recent inspect/read paged \
             to. `content` is the replacement bytes (no line numbers, no \
             framing — write accepts what read returns). \
             \
             Optimistic concurrency: if you `read` (or `inspect`) the slice \
             earlier in this session and the on-disk contents changed since \
             then, write returns a conflict — re-read, reconcile, then \
             write again. Pass `force=true` to overwrite anyway. First-time \
             writes to a slice you haven't read aren't gated (no baseline \
             to compare). \
             \
             Use this instead of `shell` + `sed`/`cat >`/`tee` for in-place \
             edits — write is locator-driven, has the conflict guard, and \
             doesn't burn a sandbox box. Use shell when you need a real \
             process to compute the new content (running a formatter, \
             code-gen tool, etc.) and let it write the file directly. \
             \
             box: locators are inspect-only (staged change sets are \
             read-only here). Sequence-insertion locators (before/after a \
             named symbol) are a planned extension.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string",
                            "description": "Locator (inspect's grammar): path | path lines A..B | path around N | path symbol <name>[N] | next/previous/first/last."},
                "content": {"type": "string",
                            "description": "Replacement bytes for the named slice."},
                "force":   {"type": "boolean",
                            "description": "Skip the optimistic-concurrency check and overwrite regardless of drift. Default false."},
            },
            "required": ["path", "content"],
        }),
        run_location: RunLocation::InBox,
    }
}

fn apply_spec() -> ToolSpec {
    ToolSpec {
        name: "apply",
        description:
            "Call this AFTER a successful `ask` to commit the sub-agent's \
             work. The sub-agent ran in its own sandbox box; its file \
             writes are STAGED there, not yet folded into your plane. \
             apply takes the change summary you saw in the ask result \
             and merges every staged file into your conversation's \
             working state, then removes the box. \
             \
             ALWAYS review the change summary in the sub-agent's result \
             turn before applying — once applied you cannot un-apply. \
             If the staged changes look wrong: call `reject` instead, or \
             call `ask` again to fix them in a new sub-agent. \
             \
             `target` is the sub-agent's session id — find it as the \
             `from` field on the ask result turn (`{\"turn-id\":\"...\", \
             \"from\":\"<target>\"}` header).".to_string(),
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
            "Call this when a sub-agent's STAGED FILE CHANGES are wrong \
             or unwanted, but its RESULT TEXT is still useful. Discards \
             everything the sub-agent's sandbox box accumulated and \
             removes the box — but the ask tool result stays in your \
             conversation for reasoning. Use this when the model wrote \
             experimental files you don't want to keep, or wrote to the \
             wrong paths, but its conclusion is still meaningful. \
             \
             If you also want to drop the result text (the sub-agent's \
             session was a dead-end, no useful conclusion), use `delete` \
             instead. \
             \
             `target` is the sub-agent's session id — find it as the \
             `from` field on the ask result turn.".to_string(),
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
            "USE THIS TO SHIP YOUR FINAL ANSWER cleanly. When you've \
             worked through tool calls, dead ends, retries, and now know \
             the answer — DO NOT just emit prose. Call backtrack with \
             final=true and your answer as the summary. The harness will \
             erase every messy derivation turn from `turn_id` onward and \
             plant your clean answer in their place. The settled \
             conversation reads `<question> → <your-clean-answer>`. \
             No tool calls, no half-formed paragraphs, no walk-throughs \
             of failed attempts — just the result. \
             \
             This is also the right move for compacting MID-DERIVATION \
             when one branch dead-ended. Call backtrack with the bad \
             branch's first turn_id and final=false (default); the \
             summary becomes a WAYPOINT (`tried X; dead end: Y, moving \
             on`) and the run keeps going from the rewound state. \
             \
             Pick turn_id by reading the {\"turn-id\":\"…\"} header at the \
             top of each turn in your context. The rewind point itself \
             is PRESERVED by default (inclusive=false); pass \
             inclusive=true to discard it too. User-role turns (the \
             original question; `ask` delegation seeds) are immutable \
             and the harness rejects any backtrack that would erase one. \
             \
             You cannot use this to edit your CALLER's context — only \
             your own. Sub-agents must call backtrack(final=true) to \
             cleanly ship a result, otherwise the messy derivation \
             flows back to the caller.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "turn_id":   {"type": "string", "description": "Rewind point: turns from here onward are discarded. The turn itself is preserved unless inclusive=true."},
                "summary":   {"type": "string", "description": "The condensed record of the discarded branch — all that is carried forward."},
                "inclusive": {"type": "boolean", "description": "Default false: the named turn is preserved. True discards it too. User turns are always preserved regardless."},
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
            "Call this when a sub-agent's WORK was a dead end — you've \
             already incorporated whatever signal you got into your own \
             reasoning (or there was no useful signal), and now you want \
             to free the harness from tracking that sub-agent at all. \
             Drops the sub-agent's session folder AND its sandbox box \
             completely. \
             \
             Use this instead of `reject` when even the result TEXT \
             isn't worth keeping — the sub-agent contributed nothing \
             you'll cite. Use `apply` (not delete) when the staged \
             changes ARE wanted; use `reject` (not delete) when the \
             changes are unwanted but the result text is. \
             \
             For rewinding your OWN context — collapsing your derivation \
             into a clean answer — use `backtrack(final=true)`, NOT \
             delete.".to_string(),
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
/// `depth` (this context's delegation depth) flattens `ask` to its exhausted
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
    r.insert("write", write_spec());
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
