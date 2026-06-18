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
//   box:<id>              the staged change-set for sub-agent box <id>
//   box:<id>/<file>       page that file's staged diff (sarun BOX patch)
//   next / previous / first / last       — resolve against last cursor

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

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
pub fn inspect(locator: &Locator, turns: &[Turn]) -> String {
    let (target, window) = match &locator.window {
        Window::PageKey(k) => match resolve_page_key(k, turns) {
            Some((t, w)) => (t, w),
            None => return format!("inspect: no recent cursor to resolve {k:?}"),
        },
        _ => (locator.path.clone(), locator.window.clone()),
    };
    if let Some(rest) = target.strip_prefix("box:") {
        return inspect_box(rest, &window);
    }
    let p = PathBuf::from(&target);
    if p.is_dir() {
        inspect_dir(&p, &target, &window)
    } else if p.is_file() {
        inspect_file(&p, &target, &window)
    } else {
        format!("inspect: not found: {target}")
    }
}

fn entry_kind(p: &Path) -> &'static str {
    let m = match fs::symlink_metadata(p) { Ok(m) => m, Err(_) => return "missing" };
    let ft = m.file_type();
    if ft.is_dir() { "dir" }
    else if ft.is_symlink() { "link" }
    else if ft.is_file() { "file" }
    else { "other" }
}

fn inspect_dir(p: &Path, target: &str, window: &Window) -> String {
    let mut entries: Vec<(String, String)> = match fs::read_dir(p) {
        Ok(rd) => rd.filter_map(|e| e.ok())
            .map(|e| (entry_kind(&e.path()).to_string(),
                      e.file_name().to_string_lossy().into_owned()))
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
    if a > 1 || b < n {
        text.push_str(&footer(target, "entries", a, b.min(n), n));
    } else {
        text.push_str(&end_banner(target, "entries", n));
    }
    text
}

fn inspect_file(p: &Path, target: &str, window: &Window) -> String {
    let bytes = match fs::read(p) { Ok(b) => b, Err(e) => return format!("inspect: {e}") };
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
    if a > 1 || b < n {
        text.push_str(&footer(target, "lines", a, b.min(n), n));
    } else {
        text.push_str(&end_banner(target, "lines", n));
    }
    text
}

fn inspect_box(rest: &str, _window: &Window) -> String {
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
    if let Some(file) = sub {
        // Crude per-file slice — find the `+++ b/<file>` chunk and follow it
        // until the next `diff --git` header.
        let needle = format!("+++ b/{file}\n");
        if let Some(start) = out.find(&needle) {
            let tail = &out[start..];
            let end = tail[needle.len()..].find("diff --git ")
                .map(|i| i + needle.len()).unwrap_or(tail.len());
            return tail[..end].to_string();
        }
        return format!("inspect: box:{box_id}/{file} not in change set");
    }
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
pub fn read_path(locator: &Locator, turns: &[Turn]) -> String {
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
    let p = PathBuf::from(&target);
    let bytes = match fs::read(&p) { Ok(b) => b, Err(e) => return format!("read: {e}") };
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
// inspect/read window from this session's turns). New content's line count may
// differ from the slice's — the file grows or shrinks. `box:<id>` locators are
// read-only and rejected here.
//
// Postponed for a later pass:
//   - tree-sitter-driven STRUCTURAL locators (`path:fn_name`, `path:para[3]`,
//     etc.) — the design's "lenses". Needs a parser plug-in per file type;
//     keeping the V1 surface small until that's its own change.
//   - `path/before` and `path/after` sequence-insertion locators — fake paths
//     that splice an element ahead of / after the named one in a sequence
//     (lines, list items, JSON array members, etc.). Postponed alongside the
//     structural locators: "before/after WHAT" is most useful for structural
//     things, and the line-range case is already expressible as
//     `path lines A..A-1` (empty range = pure insertion).
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
                        turns: &[Turn]) -> String {
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
    let p = PathBuf::from(&target);

    // Whole-file replace (Window::Default on either a missing path or an
    // existing file) — easy path, no slicing.
    if matches!(window, Window::Default) {
        let exists = p.is_file();
        let prev_n = if exists {
            fs::read_to_string(&p).map(|s| s.lines().count()).unwrap_or(0)
        } else { 0 };
        if exists && !force {
            if let Some(conflict) = check_conflict_whole_file(&target, &p, turns) {
                return conflict;
            }
        }
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        if let Err(e) = fs::write(&p, content) {
            return format!("write: {e}");
        }
        let new_n = content.lines().count()
            + if content.ends_with('\n') || content.is_empty() { 0 } else { 0 };
        let new_n = if content.is_empty() { 0 } else { new_n.max(1) };
        return format!("write: {target}: replaced whole file ({prev_n} -> {new_n} lines)");
    }

    // From here on, we need an existing file to slice into.
    if !p.is_file() {
        return format!("write: not a file: {target}");
    }
    let original = match fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) => return format!("write: {e}"),
    };
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
    for (i, ln) in lines[..prefix_end].iter().enumerate() {
        out.push_str(ln);
        out.push('\n');
        let _ = i;
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
    if let Err(e) = fs::write(&p, &out) {
        return format!("write: {e}");
    }
    let new_lines: Vec<&str> = out.lines().collect();
    let was = hi.saturating_sub(lo) + 1;
    let was = if hi < lo { 0 } else { was };
    let now = content.lines().count()
        + if content.is_empty() || content.ends_with('\n') { 0 } else { 1 };
    // Inspect-style footer so the model knows what range it just touched.
    format!(
        "write: {target}: replaced lines {lo}..{hi} ({was} lines -> {now}); \
         file now {} lines",
        new_lines.len(),
    )
}

/// Walk the session's turn list backwards for the most recent `read` or
/// `inspect` tool-result whose CALL was on this same target path. If we find
/// one, compare its captured content to what the file holds now and return a
/// conflict string when they differ. Returns None when no prior read exists
/// (=> nothing to compare, write proceeds) or when contents still match.
fn check_conflict_whole_file(target: &str, p: &Path, turns: &[Turn]) -> Option<String> {
    let (call_args, captured) = last_read_for_path(target, turns)?;
    // Only treat WHOLE-FILE prior reads as a baseline for whole-file writes.
    // A prior `read(path lines 1..3)` doesn't certify the rest of the file
    // hasn't drifted.
    let prev_loc = parse_locator(&call_args);
    if !matches!(prev_loc.window, Window::Default) {
        return None;
    }
    let now = fs::read_to_string(p).ok()?;
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
