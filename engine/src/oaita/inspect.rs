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
const LINES_PER_PAGE: usize = 200;

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

fn window_indices(window: &Window, n: usize, default_page: usize) -> (usize, usize) {
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
fn resolve_page_key(key: &str, turns: &[Turn]) -> Option<(String, Window)> {
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
