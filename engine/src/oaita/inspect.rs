// inspect / read — the paged, keyed structure viewer. The reduction ladder IS
// the protocol: every reduced page ends with a cursor FOOTER (quoted target,
// window, key legend) that the harness re-parses on the model's next call.
// Page keys (next/previous/first/last) resolve against the most recent footer
// found among the current session's own RESULT turns — the context IS the
// cursor, no hidden state.
//
// Locator grammar (matches the Python prototype's parser):
//   path                  the target as-is
//   path lines A..B       jump to file lines A..B
//   path entries A..B     jump to directory entries A..B
//   path around N         jump to a small window centered on line N
//   path symbols          enumerate named definitions (tree-sitter)
//   path symbol <name>    focus on the named definition (tree-sitter)
//   path symbol <name>[N] Nth occurrence — disambiguates name collisions
//   box:<id>              the staged change-set for sub-agent box <id>
//   box:<id>/<file>       page that file's staged diff (sarun BOX patch)
//   next / previous / first / last       — resolve against last cursor

use serde_json::Value;

use crate::oaita::exec::Executor;
use crate::oaita::structural::{find_symbol, parse_symbols, Symbol};
use crate::oaita::tools::RESULT_BUDGET;
use crate::oaita::turns::Turn;

// Footer format the model re-parses:
//   --- inspect: "<target>" <unit> A..B of N — keys: first/previous/next/last
const ENTRIES_PER_PAGE: usize = 40;
pub(crate) const LINES_PER_PAGE: usize = 200;

#[derive(Debug, Clone)]
pub enum Window {
    Default,
    Range(usize, usize), // 1-based, inclusive
    Around(usize),
    PageKey(&'static str),
    /// Enumerate named definitions in the file (tree-sitter).
    Symbols,
    /// Focus on a named definition. `usize` is the 1-based occurrence index
    /// for disambiguating same-name collisions (e.g. `fn new` on two structs).
    Symbol(String, usize),
}

#[derive(Debug, Clone)]
pub struct Locator {
    pub path: String,
    pub window: Window,
}

pub fn parse_locator(s: &str) -> Locator {
    let s = s.trim();
    for key in ["next", "previous", "first", "last"] {
        if s == key {
            let k: &'static str = match key {
                "next" => "next", "previous" => "previous",
                "first" => "first", "last" => "last", _ => "next",
            };
            return Locator { path: String::new(), window: Window::PageKey(k) };
        }
    }
    if let Some(idx) = s.find(" lines ") {
        let path = s[..idx].to_string();
        let range = &s[idx + 7..];
        if let Some((a, b)) = range.split_once("..") {
            if let (Ok(a), Ok(b)) = (a.trim().parse(), b.trim().parse()) {
                return Locator { path, window: Window::Range(a, b) };
            }
        }
    }
    if let Some(idx) = s.find(" entries ") {
        let path = s[..idx].to_string();
        let range = &s[idx + 9..];
        if let Some((a, b)) = range.split_once("..") {
            if let (Ok(a), Ok(b)) = (a.trim().parse(), b.trim().parse()) {
                return Locator { path, window: Window::Range(a, b) };
            }
        }
    }
    if let Some(idx) = s.find(" around ") {
        let path = s[..idx].to_string();
        if let Ok(n) = s[idx + 8..].trim().parse() {
            return Locator { path, window: Window::Around(n) };
        }
    }
    // Structural locators come last because their separators (` symbols`,
    // ` symbol `) are more permissive than the prior ones — they'd never
    // capture a `lines A..B` suffix, but we still check after the numeric
    // forms for tidiness.
    if let Some(idx) = s.find(" symbols") {
        // Accept either "path symbols" exactly or "path symbols<trailing-ws>".
        let path = s[..idx].to_string();
        let rest = s[idx + 8..].trim();
        if rest.is_empty() {
            return Locator { path, window: Window::Symbols };
        }
    }
    if let Some(idx) = s.find(" symbol ") {
        let path = s[..idx].to_string();
        let rest = s[idx + 8..].trim();
        // `name[N]` → (name, N). Bare `name` → (name, 1).
        if let Some(open) = rest.find('[') {
            if let Some(close) = rest.find(']') {
                if close > open {
                    let name = rest[..open].trim().to_string();
                    if let Ok(n) = rest[open + 1..close].trim().parse::<usize>() {
                        if !name.is_empty() && n >= 1 {
                            return Locator { path, window: Window::Symbol(name, n) };
                        }
                    }
                }
            }
        }
        if !rest.is_empty() {
            return Locator { path, window: Window::Symbol(rest.to_string(), 1) };
        }
    }
    Locator { path: s.to_string(), window: Window::Default }
}

/// Cursor footer for a PARTIAL page — the model needs to know more pages
/// exist and which keys to use. Mid-stream only; the end-of-stuff banner
/// below handles the last-page case.
fn footer(target: &str, unit: &str, a: usize, b: usize, n: usize) -> String {
    format!("\n--- inspect: {target:?} {unit} {a}..{b} of {n} — \
             keys: first/previous/next/last\n")
}

/// END-OF-STUFF banner: when the page IS the full listing (a..b covers
/// 1..n), we used to just OMIT the footer. The model then couldn't tell
/// whether "no `next` key shown" meant "you've seen everything" or "the
/// harness forgot to emit one" — and on small directories it would
/// often try `next` anyway and waste a step. State the end loudly: the
/// model gets an unambiguous signal it has the complete listing.
fn end_banner(target: &str, unit: &str, n: usize) -> String {
    format!("\n--- END of {target:?}: {n} {unit} total, no more pages ---\n")
}

/// Run the inspect tool with a parsed locator. `turns` is the current
/// session's turn list (used to resolve page keys against the last cursor).
/// `box_id` + `executor` together route file IO through the box overlay so
/// the tool sees what a shell-inside-the-box would see (lower=host ⊕ upper=
/// staged) — never the bare host fs.
pub fn inspect(locator: &Locator, turns: &[Turn],
               box_id: &str, executor: &dyn Executor) -> String {
    let (target, window) = match &locator.window {
        Window::PageKey(k) => match resolve_page_key(k, turns) {
            Some((t, w)) => (t, w),
            None => return format!("inspect: no recent cursor to resolve {k:?}"),
        },
        _ => (locator.path.clone(), locator.window.clone()),
    };
    if let Some(rest) = target.strip_prefix("box:") {
        return inspect_box(rest, &window, turns);
    }
    // Structural windows need file bytes regardless of "kind"; route them
    // BEFORE the dir/file dispatch so a stale path_kind cache or a fresh
    // box with no upper yet doesn't false-negative the user's symbol query.
    if matches!(window, Window::Symbols | Window::Symbol(_, _)) {
        return inspect_structural(box_id, executor, &target, &window, turns);
    }
    match executor.path_kind(box_id, &target) {
        'd' => inspect_dir(box_id, executor, &target, &window, turns),
        'f' => inspect_file(box_id, executor, &target, &window, turns),
        'l' => format!("inspect: {target}: symlink"),
        's' => format!("inspect: {target}: special file"),
        _ => format!("inspect: not found: {target}"),
    }
}

fn inspect_structural(box_id: &str, executor: &dyn Executor,
                      target: &str, window: &Window, turns: &[Turn]) -> String {
    let bytes = match executor.read_file(box_id, target) {
        Ok(b) => b,
        Err(e) => return format!("inspect: {e}"),
    };
    let Some(symbols) = parse_symbols(target, &bytes) else {
        return format!(
            "inspect: {target}: no tree-sitter grammar for this extension \
             (currently: .rs, .py, .sh, .bash). Use `path lines A..B` for a \
             line window instead.");
    };
    let body = match window {
        Window::Symbols => render_symbol_list(target, &symbols),
        Window::Symbol(name, n) => match find_symbol(&symbols, name, *n) {
            Some(sym) => render_symbol_focus(target, sym, &bytes),
            None => {
                let matches: Vec<&Symbol> = symbols.iter()
                    .filter(|s| s.name == *name).collect();
                if matches.is_empty() {
                    format!("inspect: {target}: no symbol named {name:?} \
                             (use `{target} symbols` to list)")
                } else {
                    format!("inspect: {target}: symbol {name:?} has only \
                             {} occurrence(s); asked for #{n}",
                            matches.len())
                }
            }
        },
        _ => format!("inspect: internal: non-structural window in structural path"),
    };
    body + &crate::oaita::hints::append(turns, &["inspect-syntax"])
}

fn render_symbol_list(target: &str, symbols: &[Symbol]) -> String {
    if symbols.is_empty() {
        return format!("inspect: {target}: file has no named definitions.");
    }
    // Tally same-name collisions so the listing tells the model when it
    // needs the `[N]` disambiguator. First pass: count names.
    let mut counts = std::collections::HashMap::<&str, usize>::new();
    for s in symbols { *counts.entry(s.name.as_str()).or_insert(0) += 1; }
    let mut by_name_seen = std::collections::HashMap::<&str, usize>::new();
    let mut lines = Vec::with_capacity(symbols.len() + 1);
    lines.push(format!("file {target}: {} named definitions", symbols.len()));
    lines.push(String::new());
    for s in symbols {
        let seen = by_name_seen.entry(s.name.as_str()).or_insert(0);
        *seen += 1;
        let occ = *seen;
        let total = *counts.get(s.name.as_str()).unwrap_or(&1);
        let disambig = if total > 1 { format!("[{occ}]") } else { String::new() };
        let indent = "  ".repeat(s.depth);
        lines.push(format!(
            "{indent}{kind:>6}  {name}{disambig}   (lines {a}..{b})",
            kind = s.kind, name = s.name,
            a = s.start_line, b = s.end_line,
        ));
    }
    lines.push(end_banner(target, "symbols", symbols.len()));
    lines.join("\n")
}

fn render_symbol_focus(target: &str, sym: &Symbol, bytes: &[u8]) -> String {
    // Render with 1-based line numbers (same convention as inspect_file).
    let slice = &bytes[sym.start_byte..sym.end_byte];
    let text = String::from_utf8_lossy(slice);
    let mut out = String::new();
    out.push_str(&format!(
        "{kind} {name} in {target} (lines {a}..{b})\n\n",
        kind = sym.kind, name = sym.name, target = target,
        a = sym.start_line, b = sym.end_line,
    ));
    for (i, line) in text.lines().enumerate() {
        out.push_str(&format!("{:>6}  {line}\n", sym.start_line + i));
    }
    out.push_str(&format!(
        "\n--- inspect: {target:?} symbol {:?}[1..{a}..{b}] of file ---\n",
        sym.name, a = sym.start_line, b = sym.end_line,
    ));
    out
}

fn kind_label(k: char) -> &'static str {
    match k { 'd' => "dir", 'l' => "link", 'f' => "file", _ => "other" }
}

fn inspect_dir(box_id: &str, executor: &dyn Executor,
               target: &str, window: &Window, turns: &[Turn]) -> String {
    let mut entries: Vec<(String, String)> = match executor.list_dir(box_id, target) {
        Ok(es) => es.into_iter().map(|(name, k)| (kind_label(k).to_string(), name))
                                .collect(),
        Err(e) => return format!("inspect: {e}"),
    };
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    let n = entries.len();
    let (a, b) = window_indices(window, n, ENTRIES_PER_PAGE);
    let slice = &entries[a.saturating_sub(1)..b.min(n)];
    let body: String = slice.iter()
        .map(|(k, name)| format!("{k:>5}  {name}"))
        .collect::<Vec<_>>()
        .join("\n");
    let head = format!("dir {target}: {n} entries ({} dirs, {} files, {} other)\n\n",
                       entries.iter().filter(|(k, _)| k == "dir").count(),
                       entries.iter().filter(|(k, _)| k == "file").count(),
                       entries.iter().filter(|(k, _)| !matches!(k.as_str(), "dir"|"file")).count());
    let mut text = format!("{head}{body}");
    let paged = a > 1 || b < n;
    if paged {
        text.push_str(&footer(target, "entries", a, b.min(n), n));
    } else {
        text.push_str(&end_banner(target, "entries", n));
    }
    let mut hint_ids: Vec<&str> = vec!["inspect-dir"];
    if paged { hint_ids.push("inspect-cursor"); }
    text + &crate::oaita::hints::append(turns, &hint_ids)
}

fn inspect_file(box_id: &str, executor: &dyn Executor,
                target: &str, window: &Window, turns: &[Turn]) -> String {
    let bytes = match executor.read_file(box_id, target) {
        Ok(b) => b, Err(e) => return format!("inspect: {e}"),
    };
    if bytes.iter().any(|&b| b == 0) {
        return format!("inspect: {target}: binary ({} bytes)", bytes.len());
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let (a, b) = window_indices(window, n, LINES_PER_PAGE);
    let slice = &lines[a.saturating_sub(1)..b.min(n)];
    let body: String = slice.iter().enumerate()
        .map(|(i, l)| format!("{:>6}  {l}", a + i)).collect::<Vec<_>>().join("\n");
    let head = format!("file {target}: {n} lines\n\n");
    let mut text = format!("{head}{body}");
    let paged = a > 1 || b < n;
    if paged {
        text.push_str(&footer(target, "lines", a, b.min(n), n));
    } else {
        text.push_str(&end_banner(target, "lines", n));
    }
    let mut hint_ids: Vec<&str> = vec!["inspect-file-lines"];
    if paged { hint_ids.push("inspect-cursor"); }
    text + &crate::oaita::hints::append(turns, &hint_ids)
}

fn inspect_box(rest: &str, _window: &Window, turns: &[Turn]) -> String {
    // box:<id>           → list the change set as one entry per file
    // box:<id>/<file>    → page that file's staged diff
    let (box_id, sub) = match rest.split_once('/') {
        Some((b, s)) => (b, Some(s)),
        None => (rest, None),
    };
    // Defer to sarun CLI: `sarun BOX patch` emits the unified diff (the same
    // verb SarunExecutor uses for staged-change summaries). We don't expose
    // direct sqlar access here — the box id is the one truthful source.
    let out = match std::process::Command::new(default_sarun())
        .args([box_id, "patch"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => return format!("inspect: box:{box_id}: cannot reach sarun: {e}"),
    };
    let body = if let Some(file) = sub {
        // Crude per-file slice — find the `+++ b/<file>` chunk and follow it
        // until the next `diff --git` header.
        let needle = format!("+++ b/{file}\n");
        if let Some(start) = out.find(&needle) {
            let tail = &out[start..];
            let end = tail[needle.len()..].find("diff --git ")
                .map(|i| i + needle.len()).unwrap_or(tail.len());
            tail[..end].to_string()
        } else {
            return format!("inspect: box:{box_id}/{file} not in change set");
        }
    } else {
        // List filenames touched.
        let mut files: Vec<String> = Vec::new();
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("+++ b/") {
                files.push(rest.to_string());
            }
        }
        if files.is_empty() {
            return format!("box:{box_id}: empty change set");
        }
        format!("box:{box_id}: {} changed files\n{}", files.len(), files.join("\n"))
    };
    body + &crate::oaita::hints::append(turns, &["inspect-box"])
}

fn default_sarun() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(stem) = exe.file_name().and_then(|s| s.to_str()) {
            if stem == "sarun" || stem == "sarun-engine" {
                return exe.to_string_lossy().into_owned();
            }
        }
    }
    "sarun".to_string()
}

pub(crate) fn window_indices(window: &Window, n: usize, default_page: usize) -> (usize, usize) {
    match window {
        Window::Default => (1, n.min(default_page)),
        Window::Range(a, b) => (*a.max(&1), *b.min(&n)),
        // Structural windows route through their own (file-parsing) path
        // before reaching window_indices — if one reaches here it's a
        // programming error, but return a safe default rather than panic.
        Window::Symbols | Window::Symbol(_, _) => (1, n.min(default_page)),
        Window::Around(c) => {
            let half = default_page / 2;
            let a = c.saturating_sub(half).max(1);
            (a, (a + default_page - 1).min(n))
        }
        Window::PageKey(_) => (1, n.min(default_page)),
    }
}

/// Resolve a page key against the most recent cursor footer in the session's
/// result turns. Returns (target, window) to apply.
pub(crate) fn resolve_page_key(key: &str, turns: &[Turn]) -> Option<(String, Window)> {
    // Walk the .tool turns in reverse looking for the footer line.
    let re = regex::Regex::new(
        r#"--- inspect: "(?P<target>[^"]+)" (?P<unit>\w+) (?P<a>\d+)\.\.(?P<b>\d+) of (?P<n>\d+)"#
    ).unwrap();
    for t in turns.iter().rev() {
        if t.kind != "tool" { continue; }
        let Ok(content) = t.read() else { continue; };
        let Some(caps) = re.captures(&content) else { continue; };
        let target = caps["target"].to_string();
        let _unit = caps["unit"].to_string();
        let a: usize = caps["a"].parse().ok()?;
        let b: usize = caps["b"].parse().ok()?;
        let n: usize = caps["n"].parse().ok()?;
        let page = b.saturating_sub(a) + 1;
        let window = match key {
            "first" => Window::Range(1, page),
            "last" => Window::Range(n.saturating_sub(page) + 1, n),
            "next" => Window::Range(b + 1, (b + page).min(n)),
            "previous" => Window::Range(a.saturating_sub(page).max(1),
                                        a.saturating_sub(1).max(1)),
            _ => Window::Default,
        };
        return Some((target, window));
    }
    None
}

/// `read` — raw bytes of a file/slice, using inspect's locator grammar.
/// Routes through the executor's box overlay (same view a shell-inside-the-
/// box gets).
pub fn read_path(locator: &Locator, turns: &[Turn],
                 box_id: &str, executor: &dyn Executor) -> String {
    let (target, window) = match &locator.window {
        Window::PageKey(k) => match resolve_page_key(k, turns) {
            Some((t, w)) => (t, w),
            None => return format!("read: no recent cursor to resolve {k:?}"),
        },
        _ => (locator.path.clone(), locator.window.clone()),
    };
    if target.starts_with("box:") {
        return "read: box: locators are inspect-only — use shell in that box \
                to read STAGED file contents".to_string();
    }
    let bytes = match executor.read_file(box_id, &target) {
        Ok(b) => b, Err(e) => return format!("read: {e}"),
    };
    // Symbol-targeted read returns the raw source of the named definition —
    // the byte-faithful counterpart of `inspect ... symbol foo`.
    if let Window::Symbol(name, occurrence) = &window {
        let Some(symbols) = parse_symbols(&target, &bytes) else {
            return format!(
                "read: {target}: no tree-sitter grammar for this extension \
                 (currently: .rs, .py, .sh, .bash).");
        };
        let Some(sym) = find_symbol(&symbols, name, *occurrence) else {
            return format!("read: {target}: no symbol named {name:?}");
        };
        let mut out = String::from_utf8_lossy(
            &bytes[sym.start_byte..sym.end_byte]).into_owned();
        if out.len() > RESULT_BUDGET {
            out = out.chars().take(RESULT_BUDGET).collect();
        }
        return out;
    }
    if matches!(window, Window::Symbols) {
        return "read: `path symbols` is inspect-only — use \
                `inspect <path> symbols` to enumerate, then \
                `read <path> symbol <name>` for the bytes".to_string();
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let (a, b) = window_indices(&window, n, LINES_PER_PAGE);
    let slice = &lines[a.saturating_sub(1)..b.min(n)];
    let mut out = slice.join("\n");
    if out.len() > RESULT_BUDGET {
        out = out.chars().take(RESULT_BUDGET).collect();
    }
    out
}

// Hush an unused-import warning when `Value` is wired in later.
#[allow(dead_code)] fn _v(_: Value) {}

// ── write — replace the slice named by a locator ─────────────────────────────
//
// V1 scope: whole-file replace; line-range replace (`path lines A..B`); around-
// N replace (`path around N`); page-key replace (resolves the most recent
// inspect/read window from this session's turns); tree-sitter symbol replace
// (`path symbol <name>[N]`). New content's line count may differ from the
// slice's — the file grows or shrinks. `box:<id>` locators are read-only and
// rejected here.
//
// Postponed for a later pass:
//   - `path/before` and `path/after` sequence-insertion locators — fake paths
//     that splice an element ahead of / after the named one in a sequence
//     (lines, list items, JSON array members, etc.). The line-range case is
//     already expressible as `path lines A..A-1` (empty range = pure
//     insertion); the structural case (insert before/after a symbol) is the
//     interesting one and wants more design.
//
// Optimistic concurrency: when the model previously `read` (or `inspect`ed)
// this same path+window in the current session, the saved tool-result
// content is the "expected" content. If today's on-disk slice differs, the
// model is editing stale bytes — return a conflict unless `force=true`. If
// no prior read of this slice exists in-session, we don't have an expected
// baseline to compare against, so the write proceeds (first-time edits of
// fresh paths must not get a phantom conflict).

/// Tool entry point — match-by-locator write. Returns a human-readable status
/// line; conflicts/errors are also returned as text (no Result) to match how
/// the other dispatch_* functions hand their bodies back to the call result.
pub fn write_at_locator(locator: &Locator, content: &str, force: bool,
                        turns: &[Turn], box_id: &str,
                        executor: &dyn Executor) -> String {
    let (target, window) = match &locator.window {
        Window::PageKey(k) => match resolve_page_key(k, turns) {
            Some((t, w)) => (t, w),
            None => return format!("write: no recent cursor to resolve {k:?}"),
        },
        _ => (locator.path.clone(), locator.window.clone()),
    };
    if target.starts_with("box:") {
        return "write: box: locators are inspect-only — staged box contents \
                are reachable only through shell inside the box".to_string();
    }
    let kind = executor.path_kind(box_id, &target);

    // Whole-file replace (Window::Default on either a missing path or an
    // existing file) — easy path, no slicing.
    if matches!(window, Window::Default) {
        let exists = kind == 'f';
        let prev_n = if exists {
            executor.read_file(box_id, &target)
                .map(|b| String::from_utf8_lossy(&b).lines().count())
                .unwrap_or(0)
        } else { 0 };
        if exists && !force {
            if let Some(conflict) = check_conflict_whole_file(
                box_id, executor, &target, turns) {
                return conflict;
            }
        }
        if let Err(e) = executor.write_file(box_id, &target, content.as_bytes()) {
            return format!("write: {e}");
        }
        let new_n = if content.is_empty() { 0 } else { content.lines().count().max(1) };
        return format!("write: {target}: replaced whole file ({prev_n} -> {new_n} lines)");
    }

    // From here on, we need an existing file to slice into.
    if kind != 'f' {
        return format!("write: not a file: {target}");
    }
    let original_bytes = match executor.read_file(box_id, &target) {
        Ok(b) => b,
        Err(e) => return format!("write: {e}"),
    };

    // Structural splice — parse the file, find the named symbol, replace its
    // byte range. Falls back to a clean error when tree-sitter doesn't cover
    // the extension (so the model can switch to `path lines A..B`).
    if let Window::Symbol(name, occurrence) = &window {
        let Some(symbols) = parse_symbols(&target, &original_bytes) else {
            return format!(
                "write: {target}: no tree-sitter grammar for this extension \
                 (currently: .rs, .py, .sh, .bash). Use `path lines A..B` \
                 instead.");
        };
        let Some(sym) = find_symbol(&symbols, name, *occurrence) else {
            let matches: Vec<&Symbol> = symbols.iter()
                .filter(|s| s.name == *name).collect();
            return if matches.is_empty() {
                format!("write: {target}: no symbol named {name:?} (use \
                         `{target} symbols` to list)")
            } else {
                format!("write: {target}: symbol {name:?} has only {} \
                         occurrence(s); asked for #{occurrence}",
                        matches.len())
            };
        };
        if !force {
            if let Some(conflict) = check_conflict_symbol(
                &target, &original_bytes, sym, turns) {
                return conflict;
            }
        }
        let mut out = Vec::with_capacity(
            original_bytes.len() - (sym.end_byte - sym.start_byte) + content.len() + 1);
        out.extend_from_slice(&original_bytes[..sym.start_byte]);
        out.extend_from_slice(content.as_bytes());
        // Preserve a trailing newline if the original symbol's slice ended
        // with one (most language definitions do — a `}` followed by `\n`).
        let original_had_trailing_nl = original_bytes
            .get(sym.end_byte.saturating_sub(1)) == Some(&b'\n');
        if original_had_trailing_nl && !content.ends_with('\n') {
            out.push(b'\n');
        }
        out.extend_from_slice(&original_bytes[sym.end_byte..]);
        if let Err(e) = executor.write_file(box_id, &target, &out) {
            return format!("write: {e}");
        }
        let was = sym.end_line.saturating_sub(sym.start_line) + 1;
        let now = content.lines().count()
            + if content.is_empty() || content.ends_with('\n') { 0 } else { 1 };
        return format!(
            "write: {target}: replaced {kind} {name} ({was} lines -> {now})",
            kind = sym.kind, name = sym.name,
        );
    }

    let original = String::from_utf8_lossy(&original_bytes).into_owned();
    let lines: Vec<&str> = original.lines().collect();
    let n = lines.len();
    let (a, b) = window_indices(&window, n, LINES_PER_PAGE);
    // 1-based inclusive a..b; treat empty / inverted as pure insertion at `a`.
    let (lo, hi) = if a == 0 { (1, 0) } else { (a, b) };

    // Concurrency check: compare the current slice against the last in-session
    // read/inspect of the SAME path+window. If they differ and !force, abort.
    if !force {
        if let Some(conflict) = check_conflict_slice(&target, &lines, lo, hi, turns) {
            return conflict;
        }
    }

    // Splice: lines[0..lo-1]  +  new_content_lines  +  lines[hi..n]
    let prefix_end = lo.saturating_sub(1).min(n);
    let suffix_start = hi.min(n);
    let mut out = String::new();
    for ln in lines[..prefix_end].iter() {
        out.push_str(ln);
        out.push('\n');
    }
    if !content.is_empty() {
        out.push_str(content);
        if !content.ends_with('\n') { out.push('\n'); }
    }
    for ln in &lines[suffix_start..] {
        out.push_str(ln);
        out.push('\n');
    }
    // If the original lacked a trailing newline, drop ours so we don't grow
    // the file by a phantom byte.
    if !original.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    if let Err(e) = executor.write_file(box_id, &target, out.as_bytes()) {
        return format!("write: {e}");
    }
    let new_lines: Vec<&str> = out.lines().collect();
    let was = hi.saturating_sub(lo) + 1;
    let was = if hi < lo { 0 } else { was };
    let now = content.lines().count()
        + if content.is_empty() || content.ends_with('\n') { 0 } else { 1 };
    format!(
        "write: {target}: replaced lines {lo}..{hi} ({was} lines -> {now}); \
         file now {} lines",
        new_lines.len(),
    )
}

/// Symbol-grain conflict check. If the model previously inspected/read this
/// SAME (target, symbol-name, occurrence), compare the captured bytes to the
/// symbol's current bytes — return a conflict if they differ.
fn check_conflict_symbol(target: &str, current_bytes: &[u8], sym: &Symbol,
                         turns: &[Turn]) -> Option<String> {
    // Walk back to find the most recent (read or inspect) of the SAME
    // structural locator. We don't compare against line-range baselines
    // here — the symbol-grain edit doesn't promise anything about lines.
    let (call_args, captured) = last_read_for_path(target, turns)?;
    let prev = parse_locator(&call_args);
    if let Window::Symbol(prev_name, prev_n) = prev.window {
        if prev_name == sym.name && prev_n == 1 /* current Symbol.depth-aware
                                                   matching is a later pass */
        {
            let now = String::from_utf8_lossy(
                &current_bytes[sym.start_byte..sym.end_byte]).into_owned();
            // The captured `read`/`inspect` result includes our own framing
            // (line-numbered prefix for inspect, plain for read). Bail when
            // the captured content doesn't byte-contain the current source —
            // a coarse but safe check. False-positive a model can clear with
            // `force=true`.
            if !captured.contains(&now) {
                return Some(format!(
                    "write: conflict — {target} {kind} {name} differs from \
                     what was last read in this session. Re-read it, \
                     reconcile, then write again (or pass force=true to \
                     overwrite anyway).",
                    kind = sym.kind, name = sym.name,
                ));
            }
        }
    }
    None
}

/// Walk the session's turn list backwards for the most recent `read` or
/// `inspect` tool-result whose CALL was on this same target path. If we find
/// one, compare its captured content to what the file holds now and return a
/// conflict string when they differ. Returns None when no prior read exists
/// (=> nothing to compare, write proceeds) or when contents still match.
fn check_conflict_whole_file(box_id: &str, executor: &dyn Executor,
                             target: &str, turns: &[Turn]) -> Option<String> {
    let (call_args, captured) = last_read_for_path(target, turns)?;
    // Only treat WHOLE-FILE prior reads as a baseline for whole-file writes.
    // A prior `read(path lines 1..3)` doesn't certify the rest of the file
    // hasn't drifted.
    let prev_loc = parse_locator(&call_args);
    if !matches!(prev_loc.window, Window::Default) {
        return None;
    }
    let bytes = executor.read_file(box_id, target).ok()?;
    let now = String::from_utf8_lossy(&bytes).into_owned();
    if now == captured {
        None
    } else {
        Some(format!(
            "write: conflict — {target} differs from what was last read in \
             this session. Re-read it, reconcile, then write again (or pass \
             force=true to overwrite anyway)."
        ))
    }
}

fn check_conflict_slice(target: &str, lines: &[&str], lo: usize, hi: usize,
                        turns: &[Turn]) -> Option<String> {
    let (call_args, captured) = last_read_for_path(target, turns)?;
    let prev_loc = parse_locator(&call_args);
    // Compute the prior read's effective window so we can intersect it.
    let n = lines.len();
    let (pa, pb) = match prev_loc.window {
        Window::Default => (1usize, n.min(LINES_PER_PAGE)),
        Window::Range(a, b) => (a.max(1), b.min(n)),
        Window::Around(c) => {
            let half = LINES_PER_PAGE / 2;
            let a = c.saturating_sub(half).max(1);
            (a, (a + LINES_PER_PAGE - 1).min(n))
        }
        Window::PageKey(_) => return None, // can't compare against a page key
        // Structural prior reads don't certify a line range — no baseline.
        Window::Symbols | Window::Symbol(_, _) => return None,
    };
    // Require the prior read to fully cover the write window — otherwise we
    // have no baseline for some of the bytes we're about to replace.
    if pa > lo || pb < hi { return None; }
    // The prior captured content is `read`'s output: the raw slice lines
    // joined by '\n' (no trailing newline). Reconstruct the current
    // equivalent for the same window and compare.
    let cur_slice: String = lines[pa.saturating_sub(1)..pb.min(n)].join("\n");
    if cur_slice == captured {
        None
    } else {
        Some(format!(
            "write: conflict — {target} lines {pa}..{pb} differ from what was \
             last read in this session. Re-read it, reconcile, then write \
             again (or pass force=true to overwrite anyway)."
        ))
    }
}

/// Walk turns backwards looking for the most recent (`read` call → tool
/// result) pair whose call's `path` argument resolves to `target`. Returns
/// the call's raw `path` string (for window inspection) and the captured
/// tool-result content. Inspect results are skipped — their numbered-line
/// rendering doesn't byte-equal the raw on-disk text, so we'd never get a
/// clean comparison; `read` is the byte-faithful baseline.
fn last_read_for_path(target: &str, turns: &[Turn])
    -> Option<(String, String)>
{
    // The turn list is in chronological order; pair a c.assistant call turn
    // with the immediately following .tool result turn.
    let pairs: Vec<(&Turn, &Turn)> = {
        let mut out = Vec::new();
        let mut i = 0;
        while i < turns.len() {
            let t = &turns[i];
            if t.kind == "assistant" && t.flags.contains('c') {
                if let Some(next) = turns.get(i + 1) {
                    if next.kind == "tool" {
                        out.push((t, next));
                        i += 2;
                        continue;
                    }
                }
            }
            i += 1;
        }
        out
    };
    for (call, result) in pairs.iter().rev() {
        let Ok(envelope) = call.read() else { continue; };
        let Ok(v) = serde_json::from_str::<Value>(&envelope) else { continue; };
        let tool = v.get("tool").and_then(Value::as_str).unwrap_or("");
        if tool != "read" { continue; }
        let path_arg = v.get("arguments").and_then(|a| a.get("path"))
            .and_then(Value::as_str).unwrap_or("").to_string();
        let prev_loc = parse_locator(&path_arg);
        // Resolve page-key locators too — they refer to the cursor that
        // existed when the call ran; we approximate by using the current
        // turns list (the cursor is still latest-wins for the same key).
        let resolved_target = match &prev_loc.window {
            Window::PageKey(k) => {
                match resolve_page_key(k, turns) {
                    Some((t, _)) => t,
                    None => continue,
                }
            }
            _ => prev_loc.path.clone(),
        };
        if resolved_target != target { continue; }
        let Ok(captured) = result.read() else { continue; };
        return Some((path_arg, captured));
    }
    None
}
