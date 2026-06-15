// sarun-ui — a Rust interactive ratatui client for the sarun engine. It speaks
// the engine's newline-JSON control protocol over the UI control socket (see
// engine/src/control.rs: {"type":"ui","verb":...,"args":[...]} requests, replies
// wrapped {"ok":true,"r":...}; a {"type":"subscribe"} connection becomes a
// one-way event feed yielding session_added/removed/renamed/pong lines).
//
// It mirrors the core panes of the Python Textual UI (sarun: SessionTable,
// ChangesTable, hunk view, the UI(App)):
//   - Sessions pane  : table of boxes (path/id/status/cmd) from session_dicts
//   - Changes pane   : review.session_changes for the selected box (kind/size)
//   - Hunk/detail    : review.hunks for the selected change, colored unified diff
//
// Modes:
//   sarun-ui --sock PATH          interactive crossterm loop (real terminal)
//   sarun-ui --once --sock PATH   render one frame to a TestBackend, print, exit
//                                 (headless; used by the integration tests)
//
// Keys (mirroring the Textual BINDINGS where the verb exists in the Rust engine
// today): j/k or down/up move within the focused pane; Tab cycles panes; Enter on
// a box loads its changes, Enter on a change loads its hunks; a = apply, x =
// discard (a change if one is selected, else the whole box); K = kill box; D =
// delete box; r prompts rename; R refreshes; q quits. Verbs that a parallel
// agent may still be adding (apply_hunk, decorate, change_mode) are NOT
// hard-depended on — any "unknown verb 'X'" reply is surfaced as a status
// message, never a crash.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine as _;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use serde_json::Value;
use serde_json::json;

// ── wire protocol ───────────────────────────────────────────────────────────

/// One request/reply on the control socket. Returns the unwrapped `r` payload
/// on success. An engine `{"ok":false,"error":...}` reply (e.g. an unknown verb
/// for a not-yet-implemented action) becomes `Err(error_string)` — callers turn
/// that into a status line, never a panic.
fn rpc(sock: &str, verb: &str, args: Value) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    let msg = json!({"type": "ui", "verb": verb, "args": args});
    s.write_all(format!("{msg}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    let rep: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(rep
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("rpc failed")
            .to_string());
    }
    Ok(rep.get("r").cloned().unwrap_or(Value::Null))
}

/// The engine's `rename` is a top-level control type (not a "ui" verb): it takes
/// {"type":"rename","sid":..,"name":..} and replies {"ok":true,...}.
// Driven only by the interactive loop; the headless tests don't exercise rename.
#[cfg_attr(test, allow(dead_code))]
fn rename_rpc(sock: &str, sid: &str, name: &str) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    let msg = json!({"type": "rename", "sid": sid, "name": name});
    s.write_all(format!("{msg}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    let rep: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(rep
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("rename failed")
            .to_string());
    }
    Ok(rep)
}

/// Open a subscribe connection and spawn a reader thread that forwards each
/// event line (parsed JSON) to `tx`. The engine turns a {"type":"subscribe"}
/// connection into a one-way feed (session_added/removed/renamed, pong). The
/// thread ends when the socket closes; failures are silent (the UI simply stops
/// receiving live events and still works via manual refresh).
fn spawn_subscriber(sock: &str, tx: mpsc::Sender<Value>) {
    let sock = sock.to_string();
    std::thread::spawn(move || {
        let Ok(mut s) = UnixStream::connect(&sock) else { return };
        if s.write_all(b"{\"type\":\"subscribe\"}\n").is_err() {
            return;
        }
        let reader = BufReader::new(s.try_clone().expect("clone subscribe conn"));
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
        let _ = s.flush();
    });
}

// ── app state ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Sessions,
    Changes,
    Hunks,
    Processes,
    Outputs,
    Rules,
    Help,
}

/// A transient modal overlaid on the main view. Mirrors the Python Textual
/// modals: Confirm (y/n destructive), SearchModal (substring filter of the
/// active pane), RuleFormModal (add/edit a filerules line).
#[cfg_attr(test, allow(dead_code))]
enum Modal {
    /// A y/n confirmation. `action` names the destructive op to run on 'y'.
    Confirm { prompt: String, action: ConfirmAction },
    /// Substring filter of the focused pane; `buf` is the live query.
    Search { buf: String },
    /// Add or edit a file rule. `editing` is Some(index) when editing an
    /// existing rule, None when adding. `buf` is the "<action> <glob>" line.
    RuleForm { buf: String, editing: Option<usize> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, allow(dead_code))]
enum ConfirmAction {
    Kill,
    Delete,
    Dissolve,
}

struct App {
    sock: String,
    sessions: Vec<Value>,
    changes: Vec<Value>,
    hunks: Value, // raw review.hunks result for the selected change
    processes: Vec<Value>, // processes() rows: [id,tgid,ppid,parent_id,exe,argv]
    outputs: Vec<Value>,   // outputs() rows: {id,ts,process_id,stream,len}
    output_view: String,   // decoded bytes of the captured streams (stdout/stderr)
    rules: Vec<String>,    // raw filerules lines (apply/discard/passthrough <glob>)
    sel_session: usize,
    sel_change: usize,
    sel_proc: usize,
    sel_output: usize,
    sel_rule: usize,
    hunk_scroll: u16,
    out_scroll: u16,
    focus: Pane,
    status: String,
    renaming: Option<String>, // Some(buffer) while editing a new name
    modal: Option<Modal>,
    /// Active substring filter (committed from the Search modal). Empty = none.
    filter: String,
    #[cfg_attr(test, allow(dead_code))]
    should_quit: bool,
}

impl App {
    fn new(sock: String) -> Self {
        let mut a = App {
            sock,
            sessions: vec![],
            changes: vec![],
            hunks: Value::Null,
            processes: vec![],
            outputs: vec![],
            output_view: String::new(),
            rules: vec![],
            sel_session: 0,
            sel_change: 0,
            sel_proc: 0,
            sel_output: 0,
            sel_rule: 0,
            hunk_scroll: 0,
            out_scroll: 0,
            focus: Pane::Sessions,
            status: "ready · j/k move · b/c/p/o boxes/changes/procs/outputs · e rules · ? help · Enter open · a apply · x discard · K kill · D delete · r rename · / search · q quit".into(),
            renaming: None,
            modal: None,
            filter: String::new(),
            should_quit: false,
        };
        a.refresh_sessions();
        a.load_changes();
        a.load_rules();
        a
    }

    /// box_id (the engine's session_id, a stringified i64) of the selected box.
    fn cur_sid(&self) -> Option<String> {
        self.sessions
            .get(self.sel_session)
            .and_then(|s| s.get("session_id"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    fn cur_change_path(&self) -> Option<String> {
        self.changes
            .get(self.sel_change)
            .and_then(|c| c.get("path"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    fn refresh_sessions(&mut self) {
        match rpc(&self.sock, "session_dicts", json!([])) {
            Ok(Value::Array(a)) => {
                self.sessions = a;
                if self.sel_session >= self.sessions.len() {
                    self.sel_session = self.sessions.len().saturating_sub(1);
                }
            }
            Ok(_) => self.sessions.clear(),
            Err(e) => self.status = format!("session_dicts: {e}"),
        }
    }

    /// Load the changes for the selected box and reset the change cursor.
    fn load_changes(&mut self) {
        self.changes.clear();
        self.hunks = Value::Null;
        self.sel_change = 0;
        self.hunk_scroll = 0;
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "review.session_changes", json!([sid])) {
            Ok(Value::Array(a)) => self.changes = a,
            Ok(_) => {}
            Err(e) => self.status = format!("session_changes: {e}"),
        }
        // the procs/outputs panes track the same selected box.
        self.load_processes();
        self.load_outputs();
    }

    /// Load the hunks (unified diff) for the selected change of the selected box.
    fn load_hunks(&mut self) {
        self.hunks = Value::Null;
        self.hunk_scroll = 0;
        let (Some(sid), Some(path)) = (self.cur_sid(), self.cur_change_path()) else {
            return;
        };
        match rpc(&self.sock, "review.hunks", json!([sid, path])) {
            Ok(v) => self.hunks = v,
            Err(e) => self.status = format!("hunks: {e}"),
        }
    }

    /// Load the captured process tree for the selected box (rows are
    /// [id,tgid,ppid,parent_id,exe,argv]).
    fn load_processes(&mut self) {
        self.processes.clear();
        self.sel_proc = 0;
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "processes", json!([sid])) {
            Ok(Value::Array(a)) => self.processes = a,
            Ok(_) => {}
            Err(e) => self.status = format!("processes: {e}"),
        }
    }

    /// Load the captured outputs index for the selected box, then fetch and
    /// decode each row's bytes (output_detail wire-encodes them as {"__b":b64})
    /// into a single scrollable stdout/stderr transcript.
    fn load_outputs(&mut self) {
        self.outputs.clear();
        self.output_view.clear();
        self.sel_output = 0;
        self.out_scroll = 0;
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "outputs", json!([sid])) {
            Ok(Value::Array(a)) => self.outputs = a,
            Ok(_) => return,
            Err(e) => {
                self.status = format!("outputs: {e}");
                return;
            }
        }
        let mut view = String::new();
        for o in &self.outputs {
            let oid = o.get("id").and_then(Value::as_i64).unwrap_or(-1);
            let stream = o.get("stream").and_then(Value::as_i64).unwrap_or(0);
            let tag = if stream == 1 { "err" } else { "out" };
            if let Ok(d) = rpc(&self.sock, "output_detail", json!([sid, oid])) {
                if let Some(b64) = d
                    .get("content")
                    .and_then(|c| c.get("__b"))
                    .and_then(Value::as_str)
                {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        let text = String::from_utf8_lossy(&bytes);
                        for chunk in text.split_inclusive('\n') {
                            view.push_str(&format!("[{tag}] {}", chunk.trim_end_matches('\n')));
                            view.push('\n');
                        }
                    }
                }
            }
        }
        self.output_view = view;
    }

    /// The on-disk filerules path for the active namespace, computed the same
    /// way the engine's paths::config_home() does (XDG_CONFIG_HOME or
    /// ~/.config, then slopbox[.NS]).
    fn rules_path(&self) -> PathBuf {
        let app_dir = match std::env::var("SLOPBOX_NS") {
            Ok(ns) if !ns.is_empty() => format!("slopbox.{ns}"),
            _ => "slopbox".into(),
        };
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
                .join(".config"),
        };
        base.join(app_dir).join("filerules")
    }

    /// Read the filerules file into `self.rules` (one line per rule; blank and
    /// comment lines are kept so an edit round-trips the file faithfully).
    fn load_rules(&mut self) {
        let text = std::fs::read_to_string(self.rules_path()).unwrap_or_default();
        self.rules = text.lines().map(String::from).collect();
        if self.sel_rule >= self.rules.len() {
            self.sel_rule = self.rules.len().saturating_sub(1);
        }
    }

    /// Persist `self.rules` back to disk and tell the engine to reload them.
    fn save_rules(&mut self) {
        let path = self.rules_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = if self.rules.is_empty() {
            String::new()
        } else {
            format!("{}\n", self.rules.join("\n"))
        };
        if let Err(e) = std::fs::write(&path, body) {
            self.status = format!("write rules: {e}");
            return;
        }
        match rpc(&self.sock, "reload_rules", json!([])) {
            Ok(_) => self.status = format!("saved {} rule(s) · reloaded", self.rules.len()),
            Err(e) => self.status = format!("reload_rules: {e}"),
        }
    }

    // ── navigation ── (driven by the interactive loop; not by headless tests)

    #[cfg_attr(test, allow(dead_code))]
    fn move_down(&mut self) {
        match self.focus {
            Pane::Sessions => {
                if self.sel_session + 1 < self.sessions.len() {
                    self.sel_session += 1;
                    self.load_changes();
                }
            }
            Pane::Changes => {
                if self.sel_change + 1 < self.changes.len() {
                    self.sel_change += 1;
                    self.load_hunks();
                }
            }
            Pane::Hunks => self.hunk_scroll = self.hunk_scroll.saturating_add(1),
            Pane::Processes => {
                if self.sel_proc + 1 < self.visible_processes().len() {
                    self.sel_proc += 1;
                }
            }
            Pane::Outputs => self.out_scroll = self.out_scroll.saturating_add(1),
            Pane::Rules => {
                if self.sel_rule + 1 < self.rules.len() {
                    self.sel_rule += 1;
                }
            }
            Pane::Help => {}
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn move_up(&mut self) {
        match self.focus {
            Pane::Sessions => {
                if self.sel_session > 0 {
                    self.sel_session -= 1;
                    self.load_changes();
                }
            }
            Pane::Changes => {
                if self.sel_change > 0 {
                    self.sel_change -= 1;
                    self.load_hunks();
                }
            }
            Pane::Hunks => self.hunk_scroll = self.hunk_scroll.saturating_sub(1),
            Pane::Processes => self.sel_proc = self.sel_proc.saturating_sub(1),
            Pane::Outputs => self.out_scroll = self.out_scroll.saturating_sub(1),
            Pane::Rules => self.sel_rule = self.sel_rule.saturating_sub(1),
            Pane::Help => {}
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn next_pane(&mut self) {
        self.focus = match self.focus {
            Pane::Sessions => Pane::Changes,
            Pane::Changes => Pane::Hunks,
            Pane::Hunks => Pane::Processes,
            Pane::Processes => Pane::Outputs,
            Pane::Outputs => Pane::Rules,
            Pane::Rules => Pane::Sessions,
            Pane::Help => Pane::Sessions,
        };
    }

    /// Processes after applying the active substring filter (matches exe/argv).
    fn visible_processes(&self) -> Vec<&Value> {
        if self.filter.is_empty() {
            return self.processes.iter().collect();
        }
        let f = self.filter.to_lowercase();
        self.processes
            .iter()
            .filter(|p| proc_text(p).to_lowercase().contains(&f))
            .collect()
    }

    /// Enter: open the selected row into the next pane.
    fn open(&mut self) {
        match self.focus {
            Pane::Sessions => {
                self.load_changes();
                self.focus = Pane::Changes;
            }
            Pane::Changes => {
                self.load_hunks();
                self.focus = Pane::Hunks;
            }
            Pane::Hunks | Pane::Processes | Pane::Outputs | Pane::Rules | Pane::Help => {}
        }
    }

    // ── verbs ──

    /// The selector for an apply/discard: a single change path when the Changes
    /// pane is focused and a change is selected; otherwise null (the engine
    /// treats null as "the whole box").
    fn review_selector(&self) -> Value {
        if self.focus == Pane::Changes {
            if let Some(p) = self.cur_change_path() {
                return json!([p]);
            }
        }
        Value::Null
    }

    fn apply(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        let sel = self.review_selector();
        match rpc(&self.sock, "review.apply", json!([sid, sel])) {
            Ok(r) => {
                let n = r.get("applied").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                self.status = format!("applied {n} change(s)");
            }
            Err(e) => self.status = format!("apply: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    fn discard(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        let sel = self.review_selector();
        match rpc(&self.sock, "review.discard", json!([sid, sel])) {
            Ok(r) => {
                let n = r.get("discarded").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                self.status = format!("discarded {n} change(s)");
            }
            Err(e) => self.status = format!("discard: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    #[cfg_attr(test, allow(dead_code))]
    fn kill(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "kill", json!([sid])) {
            Ok(_) => self.status = format!("sent SIGTERM to box {sid}"),
            Err(e) => self.status = format!("kill: {e}"),
        }
        self.refresh_sessions();
    }

    #[cfg_attr(test, allow(dead_code))]
    fn delete(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "delete", json!([sid])) {
            Ok(_) => self.status = format!("deleted box {sid}"),
            Err(e) => self.status = format!("delete: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    #[cfg_attr(test, allow(dead_code))]
    fn dissolve(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "dissolve", json!([sid])) {
            Ok(_) => self.status = format!("dissolved box {sid}"),
            Err(e) => self.status = format!("dissolve: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    /// Run the destructive op a Confirm modal was guarding (after a 'y').
    #[cfg_attr(test, allow(dead_code))]
    fn run_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Kill => self.kill(),
            ConfirmAction::Delete => self.delete(),
            ConfirmAction::Dissolve => self.dissolve(),
        }
    }

    /// Commit the RuleForm modal buffer: append a new rule or replace an
    /// existing one, then persist + reload.
    #[cfg_attr(test, allow(dead_code))]
    fn commit_rule(&mut self, buf: String, editing: Option<usize>) {
        let line = buf.trim().to_string();
        if line.is_empty() {
            self.status = "empty rule discarded".into();
            return;
        }
        match editing {
            Some(i) if i < self.rules.len() => self.rules[i] = line,
            _ => self.rules.push(line),
        }
        self.save_rules();
    }

    /// Delete the selected file rule and persist + reload.
    #[cfg_attr(test, allow(dead_code))]
    fn delete_rule(&mut self) {
        if self.sel_rule < self.rules.len() {
            self.rules.remove(self.sel_rule);
            if self.sel_rule >= self.rules.len() {
                self.sel_rule = self.rules.len().saturating_sub(1);
            }
            self.save_rules();
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn commit_rename(&mut self) {
        let Some(name) = self.renaming.take() else { return };
        let Some(sid) = self.cur_sid() else { return };
        match rename_rpc(&self.sock, &sid, &name) {
            Ok(_) => self.status = format!("renamed box {sid} -> {name}"),
            Err(e) => self.status = format!("rename: {e}"),
        }
        self.refresh_sessions();
    }

    /// Apply an incoming subscribe event. Structural events (added/removed/
    /// renamed) re-read session_dicts so the list reflects the new world; pong
    /// is shown in the status line. Unknown event types are ignored.
    fn on_event(&mut self, ev: &Value) {
        match ev.get("type").and_then(Value::as_str) {
            Some("session_added") | Some("session_removed") | Some("session_renamed") => {
                self.refresh_sessions();
                self.load_changes();
                self.status = format!(
                    "event: {}",
                    ev.get("type").and_then(Value::as_str).unwrap_or("?")
                );
            }
            Some("pong") => self.status = "pong".into(),
            _ => {}
        }
    }
}

// ── rendering ───────────────────────────────────────────────────────────────

/// "exe argv0 argv1 …" for a processes() row [id,tgid,ppid,parent_id,exe,argv].
fn proc_text(p: &Value) -> String {
    let arr = p.as_array();
    let exe = arr.and_then(|a| a.get(4)).and_then(Value::as_str).unwrap_or("");
    let argv = arr
        .and_then(|a| a.get(5))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    format!("{exe} {argv}").trim().to_string()
}

fn title(base: &str, focused: bool) -> String {
    if focused {
        format!(" {base} «focus» ")
    } else {
        format!(" {base} ")
    }
}

fn block(t: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Block::default().borders(Borders::ALL).border_style(style).title(t)
}

fn sessions_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<14} {:<4} {:<9} {}", "PATH", "ID", "STATUS", "CMD"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.sessions.is_empty() {
        out.push(Line::from("(no boxes)"));
        return out;
    }
    for (i, s) in app.sessions.iter().enumerate() {
        let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let path = g("path");
        let id = g("session_id");
        let status = g("status");
        let name = g("name");
        let cmd = s
            .get("cmd")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let label = if path.is_empty() { name } else { path };
        let text = format!("{label:<14} {id:<4} {status:<9} {cmd}");
        let line = if i == app.sel_session {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            Line::from(text)
        };
        out.push(line);
    }
    out
}

fn changes_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<9} {:>10}  {}", "KIND", "SIZE", "PATH"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.changes.is_empty() {
        out.push(Line::from("(no changes)"));
        return out;
    }
    for (i, c) in app.changes.iter().enumerate() {
        let kind = c.get("kind").and_then(Value::as_str).unwrap_or("");
        let path = c.get("path").and_then(Value::as_str).unwrap_or("");
        let size = c.get("size").and_then(Value::as_i64).unwrap_or(0);
        let kcolor = match kind {
            "deleted" => Color::Red,
            "symlink" => Color::Magenta,
            _ => Color::Green,
        };
        let text = format!("{kind:<9} {size:>10}  {path}");
        let line = if i == app.sel_change {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            Line::from(Span::styled(text, Style::default().fg(kcolor)))
        };
        out.push(line);
    }
    out
}

/// Render review.hunks into a colored unified diff. Text diffs come as
/// {"is_text":true,"hunks":[{"lines":[[tag,text],...]}]} where tag is one of
/// "hdr"/" "/"-"/"+". Non-text/binary/symlink/deleted come as
/// {"is_text":false,"diff":{kind,...}}.
fn hunk_lines(app: &App) -> Vec<Line<'static>> {
    let h = &app.hunks;
    if h.is_null() {
        return vec![Line::from("(select a change and press Enter)")];
    }
    if h.get("is_text").and_then(Value::as_bool) == Some(true) {
        let mut out = vec![];
        if let Some(hunks) = h.get("hunks").and_then(Value::as_array) {
            if hunks.is_empty() {
                out.push(Line::from("(no textual differences)"));
            }
            for hunk in hunks {
                if let Some(lines) = hunk.get("lines").and_then(Value::as_array) {
                    for l in lines {
                        let arr = l.as_array();
                        let tag = arr.and_then(|a| a.first()).and_then(Value::as_str).unwrap_or(" ");
                        let txt = arr.and_then(|a| a.get(1)).and_then(Value::as_str).unwrap_or("");
                        let (prefix, color) = match tag {
                            "hdr" => ("", Color::Cyan),
                            "+" => ("+", Color::Green),
                            "-" => ("-", Color::Red),
                            _ => (" ", Color::Gray),
                        };
                        out.push(Line::from(Span::styled(
                            format!("{prefix}{txt}"),
                            Style::default().fg(color),
                        )));
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(Line::from("(no hunks)"));
        }
        out
    } else {
        let diff = h.get("diff").cloned().unwrap_or(Value::Null);
        let kind = diff.get("kind").and_then(Value::as_str).unwrap_or("binary");
        let mut out = vec![Line::from(Span::styled(
            format!("[{kind}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))];
        if let Some(d) = diff.get("diff").and_then(Value::as_str) {
            out.push(Line::from(d.to_string()));
        } else if let Some(e) = diff.get("error").and_then(Value::as_str) {
            out.push(Line::from(format!("error: {e}")));
        } else if diff.get("content").is_some() {
            out.push(Line::from("(binary content — not rendered)"));
        }
        out
    }
}

/// PROCESSES pane: tgid · ppid · exe · argv, one row per captured process.
fn processes_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:>6} {:>6}  {}", "TGID", "PPID", "EXE · ARGV"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    let vis = app.visible_processes();
    if vis.is_empty() {
        out.push(Line::from("(no captured processes)"));
        return out;
    }
    for (i, p) in vis.iter().enumerate() {
        let a = p.as_array();
        let tgid = a.and_then(|x| x.get(1)).and_then(Value::as_i64).unwrap_or(0);
        let ppid = a.and_then(|x| x.get(2)).and_then(Value::as_i64).unwrap_or(0);
        let text = format!("{tgid:>6} {ppid:>6}  {}", proc_text(p));
        let line = if i == app.sel_proc {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            Line::from(text)
        };
        out.push(line);
    }
    out
}

/// OUTPUTS pane: an index header (count + per-stream byte tally) followed by the
/// decoded stdout/stderr transcript, each line tagged [out]/[err].
fn outputs_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![];
    let (mut nout, mut nerr) = (0i64, 0i64);
    for o in &app.outputs {
        let len = o.get("len").and_then(Value::as_i64).unwrap_or(0);
        if o.get("stream").and_then(Value::as_i64).unwrap_or(0) == 1 {
            nerr += len;
        } else {
            nout += len;
        }
    }
    out.push(Line::from(Span::styled(
        format!("{} write(s) · {} stdout B · {} stderr B", app.outputs.len(), nout, nerr),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if app.output_view.is_empty() {
        out.push(Line::from("(no captured output)"));
        return out;
    }
    for l in app.output_view.lines() {
        let color = if l.starts_with("[err]") { Color::Red } else { Color::Gray };
        out.push(Line::from(Span::styled(l.to_string(), Style::default().fg(color))));
    }
    out
}

/// FILE RULES pane: the ordered filerules lines (first match wins).
fn rules_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        "apply/discard/passthrough <glob> — top → bottom, first match wins",
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.rules.is_empty() {
        out.push(Line::from("(no rules — press n to add)"));
        return out;
    }
    for (i, r) in app.rules.iter().enumerate() {
        let color = if r.trim_start().starts_with("discard") {
            Color::Red
        } else if r.trim_start().starts_with("passthrough") {
            Color::Yellow
        } else if r.trim_start().starts_with('#') || r.trim().is_empty() {
            Color::DarkGray
        } else {
            Color::Green
        };
        let line = if i == app.sel_rule {
            Line::from(Span::styled(r.clone(), Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            Line::from(Span::styled(r.clone(), Style::default().fg(color)))
        };
        out.push(line);
    }
    out
}

/// HELP pane: a static cheatsheet of the keybindings and the run→inspect→
/// apply/discard loop.
fn help_lines() -> Vec<Line<'static>> {
    let h = |s: &str| Line::from(Span::styled(s.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let t = |s: &str| Line::from(s.to_string());
    vec![
        h("sarun — sandboxed run → inspect → apply/discard"),
        t(""),
        t("A box runs your command over a copy-on-write overlay of the"),
        t("filesystem. Its writes, processes, and stdout/stderr are captured"),
        t("for review. You then apply (materialize on the host) or discard."),
        t(""),
        h("Panes"),
        t("  b  boxes/sessions     c  changes (files)     (Enter→diff)"),
        t("  p  processes          o  outputs (stdout/err)"),
        t("  e  file rules         ?  this help            Tab cycles"),
        t(""),
        h("Navigation"),
        t("  j/k or ↓/↑  move      Enter  open selection in next pane"),
        t("  R  refresh            /      filter active pane (substring)"),
        t(""),
        h("Actions"),
        t("  a  apply change/box   x  discard change/box"),
        t("  K  kill box (y/n)     D  delete box (y/n)   X  dissolve (y/n)"),
        t("  r  rename box"),
        t(""),
        h("File rules (e)"),
        t("  n  new rule           Enter  edit selected   d  delete selected"),
        t("  Rules are 'apply|discard|passthrough <glob>', first match wins;"),
        t("  saving reloads them in the engine."),
        t(""),
        t("  q  quit"),
    ]
}

/// Render the active modal centered over the body. Returns the area consumed.
fn draw_modal(f: &mut ratatui::Frame, area: Rect, modal: &Modal) {
    let w = (area.width * 7 / 10).clamp(20, area.width);
    let hgt = 7u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(hgt)) / 2;
    let rect = Rect { x, y, width: w, height: hgt };
    // clear behind the modal
    f.render_widget(ratatui::widgets::Clear, rect);
    let (title_s, body): (&str, Vec<Line>) = match modal {
        Modal::Confirm { prompt, .. } => (
            " confirm ",
            vec![Line::from(prompt.clone()), Line::from(""), Line::from("y = yes · n / Esc = cancel")],
        ),
        Modal::Search { buf } => (
            " search / filter ",
            vec![
                Line::from(format!("filter: {buf}_")),
                Line::from(""),
                Line::from("type to filter the active pane · Enter apply · Esc clear"),
            ],
        ),
        Modal::RuleForm { buf, editing } => (
            if editing.is_some() { " edit rule " } else { " new rule " },
            vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("e.g.  discard **/*.log   ·   Enter save · Esc cancel"),
            ],
        ),
    };
    let p = Paragraph::new(Text::from(body))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                .title(title_s.to_string()),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, rect);
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());
    let body = root[0];
    let status_area = root[1];

    // The Help pane takes the whole body.
    if app.focus == Pane::Help {
        let help = Paragraph::new(Text::from(help_lines()))
            .block(block(title("help", true), true))
            .wrap(Wrap { trim: false });
        f.render_widget(help, body);
    } else {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(body);

        // left column: sessions on top, a context list below.
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(cols[0]);

        let sessions = Paragraph::new(Text::from(sessions_lines(app))).block(block(
            title("sarun · boxes", app.focus == Pane::Sessions),
            app.focus == Pane::Sessions,
        ));
        f.render_widget(sessions, left[0]);

        // Bottom-left list + right detail depend on which pane group is focused.
        match app.focus {
            Pane::Processes => {
                let procs = Paragraph::new(Text::from(processes_lines(app)))
                    .block(block(title("PROCESSES", true), true))
                    .wrap(Wrap { trim: false });
                f.render_widget(procs, left[1]);
                let detail = Paragraph::new(Text::from(proc_detail_lines(app)))
                    .block(block(title("ENVIRONMENT · DETAIL", false), false))
                    .wrap(Wrap { trim: false });
                f.render_widget(detail, cols[1]);
            }
            Pane::Outputs => {
                let idx = Paragraph::new(Text::from(changes_lines(app)))
                    .block(block(title("changes", false), false));
                f.render_widget(idx, left[1]);
                let out = Paragraph::new(Text::from(outputs_lines(app)))
                    .block(block(title("OUTPUT · stdout/stderr", true), true))
                    .scroll((app.out_scroll, 0))
                    .wrap(Wrap { trim: false });
                f.render_widget(out, cols[1]);
            }
            Pane::Rules => {
                let rules = Paragraph::new(Text::from(rules_lines(app)))
                    .block(block(title("FILE RULES", true), true))
                    .wrap(Wrap { trim: false });
                f.render_widget(rules, left[1]);
                let hint = Paragraph::new(Text::from(vec![
                    Line::from("n new · Enter edit · d delete"),
                    Line::from(""),
                    Line::from(format!("file: {}", app.rules_path().display())),
                    Line::from(""),
                    Line::from("Rules decide each captured write: apply (keep),"),
                    Line::from("discard (drop), or passthrough. First match wins."),
                ]))
                .block(block(title("WHAT IT MATCHES", false), false))
                .wrap(Wrap { trim: false });
                f.render_widget(hint, cols[1]);
            }
            _ => {
                // Sessions / Changes / Hunks group: changes list + diff.
                let changes = Paragraph::new(Text::from(changes_lines(app))).block(block(
                    title("changes", app.focus == Pane::Changes),
                    app.focus == Pane::Changes,
                ));
                f.render_widget(changes, left[1]);

                let hunks = Paragraph::new(Text::from(hunk_lines(app)))
                    .block(block(title("diff", app.focus == Pane::Hunks), app.focus == Pane::Hunks))
                    .scroll((app.hunk_scroll, 0))
                    .wrap(Wrap { trim: false });
                f.render_widget(hunks, cols[1]);
            }
        }
    }

    let status_text = if let Some(buf) = &app.renaming {
        format!("rename -> {buf}_  (Enter to commit, Esc to cancel)")
    } else {
        app.status.clone()
    };
    let status = Paragraph::new(Line::from(Span::styled(
        status_text,
        Style::default().fg(Color::Black).bg(Color::Gray),
    )));
    f.render_widget(
        status,
        Rect { x: status_area.x, y: status_area.y, width: status_area.width, height: 1 },
    );

    if let Some(m) = &app.modal {
        draw_modal(f, body, m);
    }
}

/// Detail for the selected process: full exe + argv + the deduped env (via the
/// process_env verb), keyed off the processes() row id.
fn proc_detail_lines(app: &App) -> Vec<Line<'static>> {
    let vis = app.visible_processes();
    let Some(p) = vis.get(app.sel_proc) else {
        return vec![Line::from("(no process selected)")];
    };
    let a = p.as_array();
    let rid = a.and_then(|x| x.first()).and_then(Value::as_i64).unwrap_or(-1);
    let mut out = vec![
        Line::from(Span::styled(proc_text(p), Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
    ];
    if let (Some(sid), true) = (app.cur_sid(), rid >= 0) {
        if let Ok(env) = rpc(&app.sock, "process_env", json!([sid, rid])) {
            if let Some(obj) = env.as_object() {
                if obj.is_empty() {
                    out.push(Line::from("(no recorded environment)"));
                }
                for (k, v) in obj {
                    out.push(Line::from(format!("{k}={}", v.as_str().unwrap_or(""))));
                }
            }
        }
    }
    out
}

// ── headless one-frame render (tests / --once) ──────────────────────────────

/// Render the current app state to a TestBackend and return the buffer as text.
/// Used by `--once` and the integration tests.
fn render_to_string(app: &App, w: u16, h: u16) -> Result<String, String> {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;
    term.draw(|f| draw(f, app)).map_err(|e| e.to_string())?;
    Ok(format!("{}", term.backend()))
}

// ── interactive loop (real terminal) ────────────────────────────────────────

/// Handle one keypress while a modal is open. Mirrors the Python Confirm /
/// SearchModal / RuleFormModal interactions.
#[cfg(not(test))]
fn handle_modal_key(app: &mut App, code: crossterm::event::KeyCode) {
    use crossterm::event::KeyCode;
    let Some(modal) = app.modal.take() else { return };
    match modal {
        Modal::Confirm { prompt, action } => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => app.run_confirm(action),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.status = "cancelled".into();
            }
            _ => app.modal = Some(Modal::Confirm { prompt, action }),
        },
        Modal::Search { mut buf } => match code {
            KeyCode::Enter => {
                app.filter = buf.trim().to_string();
                app.sel_proc = 0;
                app.status = format!("filter: '{}'", app.filter);
            }
            KeyCode::Esc => {
                app.filter.clear();
                app.sel_proc = 0;
                app.status = "filter cleared".into();
            }
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::Search { buf });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::Search { buf });
            }
            _ => app.modal = Some(Modal::Search { buf }),
        },
        Modal::RuleForm { mut buf, editing } => match code {
            KeyCode::Enter => app.commit_rule(buf, editing),
            KeyCode::Esc => app.status = "rule edit cancelled".into(),
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::RuleForm { buf, editing });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::RuleForm { buf, editing });
            }
            _ => app.modal = Some(Modal::RuleForm { buf, editing }),
        },
    }
}

#[cfg(not(test))]
fn run_interactive(sock: &str) -> Result<(), String> {
    use crossterm::event;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::execute;
    use crossterm::terminal;
    use ratatui::backend::CrosstermBackend;

    let mut app = App::new(sock.to_string());
    let (tx, rx) = mpsc::channel();
    spawn_subscriber(sock, tx);

    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen).map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;

    let res = (|| -> Result<(), String> {
        loop {
            // drain live events
            while let Ok(ev) = rx.try_recv() {
                app.on_event(&ev);
            }
            term.draw(|f| draw(f, &app)).map_err(|e| e.to_string())?;
            if app.should_quit {
                break;
            }
            if !event::poll(Duration::from_millis(200)).map_err(|e| e.to_string())? {
                continue;
            }
            if let Event::Key(k) = event::read().map_err(|e| e.to_string())? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                // modal captures keys (Confirm / Search / RuleForm).
                if app.modal.is_some() {
                    handle_modal_key(&mut app, k.code);
                    continue;
                }
                // rename input mode captures keys
                if let Some(buf) = app.renaming.as_mut() {
                    match k.code {
                        KeyCode::Enter => app.commit_rename(),
                        KeyCode::Esc => {
                            app.renaming = None;
                            app.status = "rename cancelled".into();
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                        }
                        KeyCode::Char(c) => buf.push(c),
                        _ => {}
                    }
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') => app.should_quit = true,
                    KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                    KeyCode::Tab => app.next_pane(),
                    KeyCode::Enter => {
                        if app.focus == Pane::Rules {
                            // edit selected rule
                            let cur = app.rules.get(app.sel_rule).cloned().unwrap_or_default();
                            app.modal = Some(Modal::RuleForm { buf: cur, editing: Some(app.sel_rule) });
                        } else {
                            app.open();
                        }
                    }
                    // pane switches
                    KeyCode::Char('b') => app.focus = Pane::Sessions,
                    KeyCode::Char('c') => app.focus = Pane::Changes,
                    KeyCode::Char('p') => app.focus = Pane::Processes,
                    KeyCode::Char('o') => app.focus = Pane::Outputs,
                    KeyCode::Char('e') => app.focus = Pane::Rules,
                    KeyCode::Char('?') => app.focus = Pane::Help,
                    KeyCode::Char('a') => app.apply(),
                    KeyCode::Char('x') => app.discard(),
                    KeyCode::Char('K') => {
                        app.modal = Some(Modal::Confirm {
                            prompt: "Kill (SIGTERM) the selected box?".into(),
                            action: ConfirmAction::Kill,
                        })
                    }
                    KeyCode::Char('D') => {
                        app.modal = Some(Modal::Confirm {
                            prompt: "Delete the selected box and its captures?".into(),
                            action: ConfirmAction::Delete,
                        })
                    }
                    KeyCode::Char('X') => {
                        app.modal = Some(Modal::Confirm {
                            prompt: "Dissolve the selected box (unmount/cleanup)?".into(),
                            action: ConfirmAction::Dissolve,
                        })
                    }
                    KeyCode::Char('n') if app.focus == Pane::Rules => {
                        app.modal = Some(Modal::RuleForm { buf: String::new(), editing: None });
                    }
                    KeyCode::Char('d') if app.focus == Pane::Rules => app.delete_rule(),
                    KeyCode::Char('/') => {
                        app.modal = Some(Modal::Search { buf: app.filter.clone() });
                    }
                    KeyCode::Char('r') => app.renaming = Some(String::new()),
                    KeyCode::Char('R') => {
                        app.refresh_sessions();
                        app.load_changes();
                        app.load_rules();
                        app.status = "refreshed".into();
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    terminal::disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen).map_err(|e| e.to_string())?;
    term.show_cursor().map_err(|e| e.to_string())?;
    res
}

// ── entrypoint ──────────────────────────────────────────────────────────────

#[cfg(not(test))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut once = false;
    let mut sock = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--once" => once = true,
            "--sock" => sock = it.next().cloned().unwrap_or_default(),
            "-h" | "--help" => {
                println!(
                    "sarun-ui — Rust ratatui client for the sarun engine\n\
                     \n\
                     usage:\n  \
                     sarun-ui --sock PATH          interactive UI\n  \
                     sarun-ui --once --sock PATH   render one frame and exit (headless)\n"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    if sock.is_empty() {
        sock = std::env::var("SARUN_SOCK").unwrap_or_default();
    }
    if sock.is_empty() {
        eprintln!("sarun-ui: no socket (pass --sock PATH or set SARUN_SOCK)");
        std::process::exit(2);
    }
    if once {
        let app = App::new(sock);
        match render_to_string(&app, 100, 30) {
            Ok(buf) => {
                print!("{buf}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("sarun-ui: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = run_interactive(&sock) {
        eprintln!("sarun-ui: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
fn main() {}

// ── integration tests against a LIVE engine ─────────────────────────────────
//
// These boot the real sarun-engine `serve` in an isolated XDG/SLOPBOX_NS, create
// boxes via box_new, write real files through the FUSE mount, then drive the App
// (the same state machine the interactive loop drives) and assert the rendered
// TestBackend buffer CONTAINS the real box ids / changed-file names / diff text.
// They require a working FUSE + the engine binary; if the engine can't be found
// or fails to come up, the test self-skips with an explanatory message.

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Child;
    use std::process::Command;

    struct Engine {
        child: Child,
        sock: String,
        ns: String,
        xdg: PathBuf,
        _tmp: PathBuf,
    }

    impl Drop for Engine {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn engine_bin() -> Option<PathBuf> {
        // ui/ is a sibling of engine/; the release binary lives there.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let rel = here.join("../engine/target/release/sarun-engine");
        if rel.exists() {
            return Some(rel);
        }
        let dbg = here.join("../engine/target/debug/sarun-engine");
        dbg.exists().then_some(dbg)
    }

    /// Boot a private engine instance. Returns None (skip) if the binary is
    /// missing or the control socket never appears (no FUSE/permissions here).
    fn boot() -> Option<Engine> {
        let bin = engine_bin()?;
        // unique NS per test (pid + atomic) so parallel cargo tests don't collide.
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static N: AtomicU64 = AtomicU64::new(0);
        let ns = format!("uit{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
        let tmp = std::env::temp_dir().join(format!("sarun-ui-{ns}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).ok()?;
        let xdg = tmp.join("xdg");
        std::fs::create_dir_all(&xdg).ok()?;

        let child = Command::new(&bin)
            .arg("serve")
            .env("SLOPBOX_NS", &ns)
            .env("HOME", &tmp)
            .env("XDG_DATA_HOME", &xdg)
            .env("XDG_STATE_HOME", &xdg)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("XDG_RUNTIME_DIR", &xdg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        let sock = xdg.join(format!("slopbox.{ns}")).join("ui.sock");
        for _ in 0..60 {
            if sock.exists() && UnixStream::connect(&sock).is_ok() {
                return Some(Engine {
                    child,
                    sock: sock.to_string_lossy().into_owned(),
                    ns: ns.clone(),
                    xdg: xdg.clone(),
                    _tmp: tmp,
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut e = Engine {
            child,
            sock: String::new(),
            ns: ns.clone(),
            xdg: xdg.clone(),
            _tmp: tmp,
        };
        let _ = e.child.kill();
        None
    }

    /// Create a box via box_new and return (sid, mount_root).
    fn make_box(sock: &str) -> (String, PathBuf) {
        let r = rpc(sock, "box_new", json!([])).expect("box_new");
        let sid = r.get("sid").and_then(Value::as_str).unwrap().to_string();
        let root = PathBuf::from(r.get("root").and_then(Value::as_str).unwrap());
        (sid, root)
    }

    /// Run a real command in a box against the booted engine (so there are
    /// captured processes + outputs to show), reusing the engine's XDG env so
    /// the runner finds the same control socket. Blocks until the box exits.
    fn run_cmd(eng: &Engine, cmd: &[&str]) -> bool {
        let bin = engine_bin().expect("engine bin");
        let mut args: Vec<String> = vec!["run".into(), "--".into()];
        args.extend(cmd.iter().map(|s| s.to_string()));
        Command::new(&bin)
            .args(&args)
            .env("SLOPBOX_NS", &eng.ns)
            .env("HOME", &eng._tmp)
            .env("XDG_DATA_HOME", &eng.xdg)
            .env("XDG_STATE_HOME", &eng.xdg)
            .env("XDG_CONFIG_HOME", &eng.xdg)
            .env("XDG_RUNTIME_DIR", &eng.xdg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn sessions_pane_shows_real_box() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (sid, _root) = make_box(&eng.sock);
        let app = App::new(eng.sock.clone());
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains(&sid), "frame should contain box id {sid}; got:\n{buf}");
        assert!(buf.contains("boxes"), "sessions pane title missing");
        assert!(buf.contains("changes"), "changes pane title missing");
        assert!(buf.contains("diff"), "diff pane title missing");
    }

    #[test]
    fn changes_pane_shows_written_file() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        // write a real file through the FUSE mount → a captured change.
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir through mount");
        std::fs::write(dir.join("hello_ui_marker.txt"), b"hello from ui test\n")
            .expect("write through mount");

        // drive the App as the loop would: select box (Enter) → Changes pane.
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        app.open();
        // render wide so the full path fits (a narrow pane truncates it).
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(
            buf.contains("hello_ui_marker.txt"),
            "changes pane should list the written file; got:\n{buf}"
        );
        assert!(buf.contains("changed"), "kind 'changed' missing:\n{buf}");
        // the 19-byte write's size must show in the pane.
        assert!(buf.contains("19"), "size column missing:\n{buf}");
    }

    #[test]
    fn hunks_pane_shows_diff_content() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("diffme.txt"), b"UNIQUE_DIFF_LINE_xyzzy\n").expect("write");

        let mut app = App::new(eng.sock.clone());
        // scripted keys: Enter (box→changes), Enter (change→hunks).
        app.open();
        assert!(!app.changes.is_empty(), "expected at least one change");
        app.open();
        let buf = render_to_string(&app, 120, 40).unwrap();
        assert!(
            buf.contains("xyzzy") || buf.contains("UNIQUE_DIFF"),
            "diff pane should contain the new file's unique line; got:\n{buf}"
        );
    }

    #[test]
    fn discard_removes_the_change() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("to_discard.txt"), b"junk\n").expect("write");

        let mut app = App::new(eng.sock.clone());
        app.open(); // focus Changes, changes loaded
        let present = |a: &App| {
            a.changes.iter().any(|c| {
                c.get("path")
                    .and_then(Value::as_str)
                    .map(|p| p.contains("to_discard.txt"))
                    .unwrap_or(false)
            })
        };
        assert!(present(&app), "change should be present before discard");
        app.discard();
        assert!(!present(&app), "change should be gone after discard; status={}", app.status);
    }

    #[test]
    fn unknown_verb_is_graceful() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        // a verb a parallel agent may still be adding — must NOT panic, just Err.
        let r = rpc(&eng.sock, "apply_hunk", json!(["1", "x", 0]));
        assert!(r.is_err(), "unknown verb should be an Err, not a crash");
        assert!(r.unwrap_err().contains("unknown verb"), "error should name the unknown verb");
    }

    #[test]
    fn live_event_feed_delivers_pong_and_removed() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (tx, rx) = mpsc::channel();
        spawn_subscriber(&eng.sock, tx);
        std::thread::sleep(Duration::from_millis(300));
        // ping triggers a 'pong' broadcast on the subscribe feed.
        let _ = rpc(&eng.sock, "ping", json!([]));
        let mut saw_pong = false;
        for _ in 0..5 {
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(ev) => {
                    if ev.get("type").and_then(Value::as_str) == Some("pong") {
                        saw_pong = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(saw_pong, "expected a 'pong' live event on the subscribe feed");

        // a structural event (delete) should arrive and refresh the App.
        let (sid, _root) = make_box(&eng.sock);
        let mut app = App::new(eng.sock.clone());
        let _ = rpc(&eng.sock, "delete", json!([sid.clone()]));
        let mut saw_removed = false;
        for _ in 0..10 {
            if let Ok(ev) = rx.recv_timeout(Duration::from_secs(2)) {
                if ev.get("type").and_then(Value::as_str) == Some("session_removed") {
                    app.on_event(&ev);
                    saw_removed = true;
                    break;
                }
            }
        }
        assert!(saw_removed, "expected a session_removed live event");
    }

    #[test]
    fn apply_then_materializes_on_host() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // a path under /tmp the host can actually accept on apply.
        let fname = format!("applied_marker_{sid}.txt");
        std::fs::write(dir.join(&fname), b"applied content\n").expect("write");
        let host_path = PathBuf::from("/tmp").join(&fname);
        let _ = std::fs::remove_file(&host_path);

        let mut app = App::new(eng.sock.clone());
        app.open(); // changes pane
        assert!(!app.changes.is_empty(), "expected a change before apply");
        app.apply();
        assert!(
            app.status.starts_with("applied"),
            "apply status should report success; got {}",
            app.status
        );
        assert!(host_path.exists(), "applied file should materialize on host at {host_path:?}");
        let _ = std::fs::remove_file(&host_path);
    }

    #[test]
    fn processes_pane_shows_real_exe_and_argv() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        // run a real command in a box; its process tree is captured.
        if !run_cmd(&eng, &["/bin/echo", "PROC_MARKER_zzz"]) {
            eprintln!("SKIP: could not run a box command (bwrap unavailable?)");
            return;
        }
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        // select the box that actually has captured processes.
        let idx = (0..app.sessions.len()).find(|&i| {
            app.sel_session = i;
            app.load_processes();
            !app.processes.is_empty()
        });
        assert!(idx.is_some(), "expected at least one box with captured processes");
        app.focus = Pane::Processes;
        let buf = render_to_string(&app, 160, 40).unwrap();
        assert!(buf.contains("PROCESSES"), "processes pane title missing:\n{buf}");
        // the real exe of the command we ran must appear.
        assert!(
            buf.contains("echo"),
            "processes pane should show the real exe 'echo'; got:\n{buf}"
        );
        // and its argv marker.
        assert!(
            buf.contains("PROC_MARKER_zzz"),
            "processes pane should show the real argv; got:\n{buf}"
        );
    }

    #[test]
    fn outputs_pane_shows_real_captured_bytes() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        if !run_cmd(&eng, &["/bin/echo", "OUTPUT_MARKER_qqq"]) {
            eprintln!("SKIP: could not run a box command (bwrap unavailable?)");
            return;
        }
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        let found = (0..app.sessions.len()).any(|i| {
            app.sel_session = i;
            app.load_outputs();
            app.output_view.contains("OUTPUT_MARKER_qqq")
        });
        assert!(found, "expected the echoed bytes in some box's outputs; status={}", app.status);
        app.focus = Pane::Outputs;
        let buf = render_to_string(&app, 160, 40).unwrap();
        assert!(buf.contains("OUTPUT"), "outputs pane title missing:\n{buf}");
        assert!(
            buf.contains("OUTPUT_MARKER_qqq"),
            "outputs pane should show the captured stdout bytes; got:\n{buf}"
        );
    }

    #[test]
    fn rules_editor_writes_file_and_reloads() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let mut app = App::new(eng.sock.clone());
        app.focus = Pane::Rules;
        assert!(app.rules.is_empty(), "fresh instance should have no rules");
        // add a rule the way the RuleForm modal commit does.
        app.commit_rule("discard **/*.RULEMARKER_log".into(), None);
        // it must have been persisted to the on-disk filerules file...
        let on_disk = std::fs::read_to_string(app.rules_path()).unwrap_or_default();
        assert!(
            on_disk.contains("discard **/*.RULEMARKER_log"),
            "rule should be persisted to {:?}; got: {on_disk:?}",
            app.rules_path()
        );
        // ...reload_rules must have been called (status reflects success)...
        assert!(
            app.status.contains("reloaded"),
            "save should call reload_rules; status={}",
            app.status
        );
        // ...and the rules pane must render it.
        let buf = render_to_string(&app, 160, 40).unwrap();
        assert!(buf.contains("FILE RULES"), "rules pane title missing:\n{buf}");
        assert!(
            buf.contains("RULEMARKER_log"),
            "rules pane should show the added rule; got:\n{buf}"
        );

        // edit then delete round-trips the file.
        app.sel_rule = 0;
        app.commit_rule("apply src/**".into(), Some(0));
        let edited = std::fs::read_to_string(app.rules_path()).unwrap_or_default();
        assert!(edited.contains("apply src/**"), "edit should replace the rule: {edited:?}");
        assert!(!edited.contains("RULEMARKER_log"), "old rule should be gone: {edited:?}");
        app.sel_rule = 0;
        app.delete_rule();
        let after = std::fs::read_to_string(app.rules_path()).unwrap_or_default();
        assert!(after.trim().is_empty(), "delete should empty the file: {after:?}");
    }

    #[test]
    fn confirm_modal_guards_destructive_delete() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (sid, _root) = make_box(&eng.sock);
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        // open a Confirm for delete; the box must still be present.
        app.modal = Some(Modal::Confirm {
            prompt: "Delete?".into(),
            action: ConfirmAction::Delete,
        });
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("confirm"), "confirm modal title missing:\n{buf}");
        assert!(buf.contains("Delete?"), "confirm prompt missing:\n{buf}");
        assert!(
            app.sessions.iter().any(|s| s.get("session_id").and_then(Value::as_str) == Some(&sid)),
            "box should still exist while only the modal is open"
        );
        // running the guarded action actually deletes it.
        app.run_confirm(ConfirmAction::Delete);
        assert!(
            !app.sessions.iter().any(|s| s.get("session_id").and_then(Value::as_str) == Some(&sid)),
            "box should be gone after the confirmed delete; status={}",
            app.status
        );
    }

    #[test]
    fn search_filter_narrows_processes_pane() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        if !run_cmd(&eng, &["/bin/echo", "FILTER_KEEP_marker"]) {
            eprintln!("SKIP: could not run a box command");
            return;
        }
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        let ok = (0..app.sessions.len()).any(|i| {
            app.sel_session = i;
            app.load_processes();
            !app.processes.is_empty()
        });
        assert!(ok, "expected captured processes");
        let total = app.processes.len();
        // a filter that matches nothing hides every row.
        app.filter = "NO_SUCH_PROC_zzzz".into();
        assert!(app.visible_processes().is_empty(), "bogus filter should hide all rows");
        // a filter on the real exe keeps the echo row.
        app.filter = "echo".into();
        let vis = app.visible_processes();
        assert!(!vis.is_empty() && vis.len() <= total, "exe filter should keep ≥1, ≤all rows");
        app.focus = Pane::Processes;
        let buf = render_to_string(&app, 160, 40).unwrap();
        assert!(buf.contains("echo"), "filtered processes pane should still show echo:\n{buf}");
    }

    #[test]
    fn help_pane_lists_keybindings() {
        // pure render; no engine needed.
        let mut app = App {
            sock: String::new(),
            sessions: vec![],
            changes: vec![],
            hunks: Value::Null,
            processes: vec![],
            outputs: vec![],
            output_view: String::new(),
            rules: vec![],
            sel_session: 0,
            sel_change: 0,
            sel_proc: 0,
            sel_output: 0,
            sel_rule: 0,
            hunk_scroll: 0,
            out_scroll: 0,
            focus: Pane::Help,
            status: String::new(),
            renaming: None,
            modal: None,
            filter: String::new(),
            should_quit: false,
        };
        app.focus = Pane::Help;
        let buf = render_to_string(&app, 100, 40).unwrap();
        assert!(buf.contains("help"), "help pane title missing:\n{buf}");
        assert!(buf.contains("apply") && buf.contains("discard"), "help should mention apply/discard:\n{buf}");
        assert!(buf.contains("processes"), "help should mention the processes pane:\n{buf}");
    }
}
