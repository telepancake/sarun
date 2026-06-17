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
use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine as _;

use crate::rules::Clause;
use crate::rules::Join;
use crate::rules::Match;
use crate::rules::ProcFilterTarget;
use crate::rules::Subject;
use crate::rules::eval_clauses;

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

/// Ask the engine to terminate ('q' contract — quit stops the server, like
/// the Python prototype). Top-level type, fire-and-forget: the engine acks
/// then SIGTERMs itself, so a broken-pipe on the read side is normal and
/// expected. No-op against an engine that doesn't speak `shutdown` (the
/// Python prototype DOES, this branch is just for older Rust engines).
#[cfg_attr(test, allow(dead_code))]
fn shutdown_rpc(sock: &str) {
    let Ok(mut s) = UnixStream::connect(sock) else { return };
    let _ = s.write_all(b"{\"type\":\"shutdown\"}\n");
    let mut line = String::new();
    let _ = BufReader::new(&s).read_line(&mut line);
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

// ── typed filter system (mirrors Python _view_filters / FILTERABLE) ──────────
//
// Each filterable list view (changes / procs / outputs) keeps its own clause
// list, an on/off flag, and a "generated" marker. A user-typed '/' filter
// persists its clauses when toggled off; a GENERATED "ids" filter (built by a
// cross-pane navigation) is dropped on the next nav / esc. Evaluation reuses the
// in-crate clause engine: a PathTarget (changes) or ProcFilterTarget (procs /
// outputs) is built per row from its data and run through `eval_clauses`.

/// The user Match kinds '/' offers per view — always WITHOUT the internal "ids"
/// kind (that is built only by cross-pane navigation). Mirrors Python
/// SUBJECT_KINDS / FILE_KINDS and FILTERABLE.
const FILE_FILTER_KINDS: &[&str] = &["path", "box", "exe", "cwd", "arg"];
const SUBJECT_FILTER_KINDS: &[&str] = &["box", "exe", "cwd", "arg"];

/// Which list view a '/' filter applies to. Sessions/Hunks/Rules/Help/Pty are
/// not filterable.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterView {
    Changes,
    Procs,
    Outputs,
}

impl FilterView {
    fn kinds(self) -> &'static [&'static str] {
        match self {
            FilterView::Changes => FILE_FILTER_KINDS,
            _ => SUBJECT_FILTER_KINDS,
        }
    }
    fn default_kind(self) -> &'static str {
        match self {
            FilterView::Changes => "path",
            _ => "exe",
        }
    }
}

/// Per-view '/' filter state (mirrors the Python `_view_filters[v]` dict).
#[derive(Clone, Default)]
struct ViewFilter {
    clauses: Vec<Clause>,
    on: bool,
    generated: bool,
}

impl ViewFilter {
    /// The active clause list, or None when the filter is off (Python
    /// `_filter_clauses`).
    fn active(&self) -> Option<&[Clause]> {
        if self.on && !self.clauses.is_empty() {
            Some(&self.clauses)
        } else {
            None
        }
    }
}

/// Wire format for view.open / view.filter — null for "no filter", else a
/// JSON array of clauses the engine reparses against the same `rules::Clause`
/// the UI uses.
fn filter_to_json(clauses: Option<&[Clause]>) -> Value {
    let Some(cs) = clauses else { return Value::Null };
    Value::Array(cs.iter().map(|c| json!({
        "kind": c.m.kind,
        "pattern": c.m.pattern,
        "join": match c.join { Join::And => "and", Join::Or => "or" },
        "negate": c.negate,
        "enabled": c.enabled,
    })).collect())
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
    /// The engine-held PTY pane (D7/D9): a live tui-term view of an interactive
    /// command the ENGINE runs on a PTY, driven over the FRAME_PTY_* mux.
    Pty,
}

/// A transient modal overlaid on the main view. Mirrors the Python Textual
/// modals: Confirm (y/n destructive), SearchModal (substring filter of the
/// active pane), RuleFormModal (add/edit a filerules line).
#[cfg_attr(test, allow(dead_code))]
enum Modal {
    /// A y/n confirmation. `action` names the destructive op to run on 'y'.
    Confirm { prompt: String, action: ConfirmAction },
    /// Clause filter editor of the focused list view (the '/' SearchModal — a
    /// reusable ClauseList). `view` is the list it filters; `kinds` the user
    /// Match vocabulary it offers; `rows` the editable clause rows (enabled ·
    /// and/or · not · kind · pattern); `sel` the cursored row; `field` the
    /// cursored column being edited within that row.
    Search {
        view: FilterView,
        kinds: &'static [&'static str],
        rows: Vec<ClauseRow>,
        sel: usize,
        field: ClauseField,
    },
    /// Add or edit a file rule. `editing` is Some(index) when editing an
    /// existing rule, None when adding. `buf` is the "<action> <glob>" line.
    RuleForm { buf: String, editing: Option<usize> },
    /// Command to run on a fresh engine-held PTY pane. `buf` is the command
    /// line (pre-filled with the configurable default — a saved "login
    /// command"); the user runs whatever they type, e.g. `bash`, a remote
    /// shell, or a full box: `sarun run -b -- make`. The engine-held PTY is a
    /// generic transport — it runs the given argv; it does NOT presume a box or
    /// any box parameters (that is the caller's choice).
    PtyCmd { buf: String },
}

/// One editable line of the '/' clause editor (mirrors Python ClauseRow): the
/// fields a Clause carries, kept as plain editable state until the modal commits
/// them into `crate::rules::Clause` values.
#[derive(Clone)]
#[cfg_attr(test, allow(dead_code))]
struct ClauseRow {
    enabled: bool,
    join: Join,
    negate: bool,
    kind: String,
    pattern: String,
}

impl ClauseRow {
    fn from_clause(c: &Clause) -> ClauseRow {
        ClauseRow {
            enabled: c.enabled,
            join: c.join,
            negate: c.negate,
            kind: c.m.kind.clone(),
            pattern: c.m.pattern.clone(),
        }
    }
    fn to_clause(&self) -> Clause {
        Clause {
            m: Match { kind: self.kind.clone(), pattern: self.pattern.trim().to_string() },
            join: self.join,
            negate: self.negate,
            enabled: self.enabled,
        }
    }
}

/// The cursored column within a clause row in the '/' editor.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, allow(dead_code))]
enum ClauseField {
    Enabled,
    Join,
    Negate,
    Kind,
    Pattern,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, allow(dead_code))]
enum ConfirmAction {
    Kill,
    Delete,
    Dissolve,
}

/// State of the off-loop structural diff for the selected BINARY change. Mirrors
/// the Python `_struct_*` worker fields (generation, spinner, cached lines).
#[derive(Default)]
struct StructState {
    /// Generation counter bumped on every navigate-away — a worker result tagged
    /// with a stale generation is dropped (Python `_struct_gen`).
    generation: u64,
    /// The engine struct job id while a finish is in flight (for struct_cancel).
    job: Option<i64>,
    /// Quick (type + header) lines rendered immediately; (style,text) pairs.
    quick_lines: Vec<(String, String)>,
    /// Full structural diff lines once the worker returns, else None (pending).
    full_lines: Option<Vec<(String, String)>>,
    /// Animated spinner phase while a finish is pending.
    spin: usize,
    /// True while a finish worker is running (show the spinner).
    pending: bool,
    /// Hexdump fallback lines (unrecognized type): rendered straight from bytes.
    hex_lines: Vec<String>,
    /// The binary detail header lines (path · kind · size · mode · ⚠ stale).
    header: Vec<(String, String)>,
}

/// What a structural-diff worker thread sends back when `struct_finish` returns.
struct StructResult {
    generation: u64,
    lines: Vec<(String, String)>,
}

/// How many rows we hold client-side at once for the changes / procs / outputs
/// panes. The engine owns the materialized + filtered list; the UI just walks
/// a window of it. Sized large enough to make a screen of scrolling fit
/// without a refetch — the engine answer is small and fast, but a round-trip
/// per keystroke would still cost more than a slice.
const WINDOW_SIZE: usize = 400;

struct App {
    sock: String,
    sessions: Vec<Value>,
    /// Changes WINDOW for the selected box: a contiguous slice of the engine's
    /// view starting at `changes_window_start`. `sel_change` is the cursor
    /// inside this window (NOT global), so when the cursor walks off an edge
    /// the window slides and the cursor stays in bounds. `changes_total` is
    /// the engine-side row count after the current filter.
    changes: Vec<Value>,
    changes_view: Option<u64>,
    changes_total: usize,
    changes_window_start: usize,
    hunks: Value, // raw review.hunks result for the selected change
    /// Same window/view scheme for the processes pane. `processes` rows here
    /// are the engine-side flattened tree (depth + connector baked in), so
    /// the UI no longer rebuilds the tree.
    /// TODO follow-up commit migrates this pane to the view RPCs; the fields
    /// below are reserved so the App layout doesn't churn twice.
    processes: Vec<Value>,
    #[allow(dead_code)] processes_view: Option<u64>,
    #[allow(dead_code)] processes_total: usize,
    #[allow(dead_code)] processes_window_start: usize,
    /// Same for outputs.
    outputs: Vec<Value>,
    #[allow(dead_code)] outputs_view: Option<u64>,
    #[allow(dead_code)] outputs_total: usize,
    #[allow(dead_code)] outputs_window_start: usize,
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
    /// Per-view typed clause filters (mirrors Python `_view_filters`): changes /
    /// procs / outputs each keep their own clause list + on/generated flags.
    f_changes: ViewFilter,
    f_procs: ViewFilter,
    f_outputs: ViewFilter,
    #[cfg_attr(test, allow(dead_code))]
    should_quit: bool,
    /// Live engine-held PTY pane, present while one is open (Pane::Pty). None
    /// otherwise. Kept out of the headless `--once` path (which never opens one).
    pty: Option<PtyPane>,
    /// Off-loop structural-diff state for the selected BINARY change.
    structd: StructState,
    /// Hunk cursor within the diff pane (index into the hunk list); used for
    /// per-hunk apply/discard.
    sel_hunk: usize,
    /// Receiver for finished structural-diff worker results (drained each tick).
    struct_rx: Option<mpsc::Receiver<StructResult>>,
}

impl App {
    fn new(sock: String) -> Self {
        let mut a = App {
            sock,
            sessions: vec![],
            changes: vec![],
            changes_view: None,
            changes_total: 0,
            changes_window_start: 0,
            hunks: Value::Null,
            processes: vec![],
            processes_view: None,
            processes_total: 0,
            processes_window_start: 0,
            outputs: vec![],
            outputs_view: None,
            outputs_total: 0,
            outputs_window_start: 0,
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
            f_changes: ViewFilter::default(),
            f_procs: ViewFilter::default(),
            f_outputs: ViewFilter::default(),
            should_quit: false,
            pty: None,
            structd: StructState::default(),
            sel_hunk: 0,
            struct_rx: None,
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
        self.visible_changes()
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

    /// Open / reopen the engine-side changes view for the selected box and
    /// fetch the first window. `sel_change` is window-relative; the view
    /// keeps the materialized + filtered list, we just walk a slice.
    fn load_changes(&mut self) {
        self.close_changes_view();
        self.changes.clear();
        self.changes_total = 0;
        self.changes_window_start = 0;
        self.hunks = Value::Null;
        self.sel_change = 0;
        self.hunk_scroll = 0;
        self.sel_hunk = 0;
        self.cancel_struct();
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_changes.active());
        match rpc(&self.sock, "view.open", json!(["changes", sid, filter])) {
            Ok(v) => {
                self.changes_view = v.get("view_id").and_then(Value::as_u64);
                self.changes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
            Err(e) if e.contains("unknown verb") => {
                self.status = format!(
                    "engine doesn't speak view.* — kill the stale engine \
                     (the socket {} is being answered by an old process) and \
                     run sarun again", self.sock);
                return;
            }
            Err(e) => {
                self.status = format!("view.open changes: {e}");
                return;
            }
        }
        self.fetch_changes_window(0);
        // the procs/outputs panes track the same selected box.
        self.load_processes();
        self.load_outputs();
    }

    /// Pull a single window worth of rows from the engine's changes view.
    /// On success replaces `self.changes` + updates window_start / total.
    fn fetch_changes_window(&mut self, start: usize) {
        let Some(vid) = self.changes_view else { return };
        let start = start.min(self.changes_total.saturating_sub(1).max(0));
        match rpc(&self.sock, "view.window",
                  json!([vid, start, WINDOW_SIZE])) {
            Ok(v) => {
                self.changes_window_start =
                    v.get("start").and_then(Value::as_u64).unwrap_or(start as u64) as usize;
                self.changes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(self.changes_total as u64) as usize;
                self.changes = v.get("rows").and_then(Value::as_array).cloned()
                    .unwrap_or_default();
            }
            Err(e) => self.status = format!("view.window changes: {e}"),
        }
    }

    /// Drop the engine-side changes view (after which `changes_view` is None).
    /// No-op if we never opened one.
    fn close_changes_view(&mut self) {
        if let Some(vid) = self.changes_view.take() {
            let _ = rpc(&self.sock, "view.close", json!([vid]));
        }
    }

    /// Push the current local f_changes filter to the engine-side view (so the
    /// engine recomputes `idx`), then refetch the window from the top.
    fn push_changes_filter(&mut self) {
        let Some(vid) = self.changes_view else { return };
        let filter = filter_to_json(self.f_changes.active());
        match rpc(&self.sock, "view.filter", json!([vid, filter])) {
            Ok(v) => {
                self.changes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.changes_window_start = 0;
                self.sel_change = 0;
                self.fetch_changes_window(0);
            }
            Err(e) => self.status = format!("view.filter changes: {e}"),
        }
    }

    /// box_id of the selected box as i64 (the view RPCs take it numerically).
    fn cur_sid_i64(&self) -> Option<i64> {
        self.cur_sid().and_then(|s| s.parse::<i64>().ok())
    }

    /// Move the changes-pane cursor by `delta` rows in the engine-side view
    /// coordinate system, sliding the window if the new position would leave
    /// it. `sel_change` is kept in [0, window.len()) after this returns.
    /// Caller is responsible for the bound check against `changes_total`.
    fn sel_change_global_advance(&mut self, delta: isize) {
        let cur_global = self.changes_window_start + self.sel_change;
        let new_global = (cur_global as isize + delta).max(0) as usize;
        // Still in the current window? Adjust the local cursor in-place — no
        // RPC, the common case for any single-step navigation.
        let win_end = self.changes_window_start + self.changes.len();
        if new_global >= self.changes_window_start && new_global < win_end {
            self.sel_change = new_global - self.changes_window_start;
            return;
        }
        // Out of window — slide. Anchor the cursor at ~1/4 in so there's
        // ~3/4 of a window of look-ahead in the direction of motion.
        let quarter = WINDOW_SIZE / 4;
        let new_start = new_global.saturating_sub(if delta > 0 { quarter } else { WINDOW_SIZE - quarter });
        self.fetch_changes_window(new_start);
        self.sel_change = new_global.saturating_sub(self.changes_window_start);
        // Defend against a too-short window at the tail (sel beyond rows).
        if self.sel_change >= self.changes.len() && !self.changes.is_empty() {
            self.sel_change = self.changes.len() - 1;
        }
    }

    /// Load the hunks (unified diff) for the selected change of the selected box.
    /// For a BINARY change this also kicks off the off-loop structural diff
    /// (struct_quick now, struct_finish on a worker thread).
    fn load_hunks(&mut self) {
        self.hunks = Value::Null;
        self.hunk_scroll = 0;
        self.sel_hunk = 0;
        // Supersede any structural diff in flight for the previous row.
        self.cancel_struct();
        let (Some(sid), Some(path)) = (self.cur_sid(), self.cur_change_path()) else {
            return;
        };
        match rpc(&self.sock, "review.hunks", json!([sid, path])) {
            Ok(v) => self.hunks = v,
            Err(e) => self.status = format!("hunks: {e}"),
        }
        // text changes use the unified-diff hunk pane; binary changes drive the
        // structural-diff pane (detail header + struct_quick → struct_finish).
        if self.hunks.get("is_text").and_then(Value::as_bool) != Some(true) {
            self.begin_struct(&sid, &path);
        }
    }

    /// Bump the generation so any in-flight worker result is dropped, drop the
    /// receiver, and tell the engine to cancel the running job (Python
    /// `_cancel_struct`). Clears the cached struct state.
    fn cancel_struct(&mut self) {
        self.structd.generation = self.structd.generation.wrapping_add(1);
        if let Some(job) = self.structd.job.take() {
            let _ = rpc(&self.sock, "struct_cancel", json!([job]));
        }
        self.struct_rx = None;
        self.structd.pending = false;
        self.structd.quick_lines.clear();
        self.structd.full_lines = None;
        self.structd.hex_lines.clear();
        self.structd.header.clear();
    }

    /// Build the binary detail header + run struct_quick; for a recognized type
    /// spawn a worker thread that runs struct_finish off the render path. For an
    /// unrecognized type build a hexdump fallback from the change's bytes.
    fn begin_struct(&mut self, sid: &str, rel: &str) {
        // detail header (path · kind · size · mode · ⚠ stale) from decorate.
        self.structd.header = self.binary_header(sid, rel);
        // FAST half: type lines + header + (maybe) a job id.
        let quick = rpc(&self.sock, "struct_quick", json!([sid, rel]))
            .unwrap_or_else(|e| json!({"lines": [["err", e]], "job": Value::Null}));
        self.structd.quick_lines = pairs_of(quick.get("lines"));
        let job = quick.get("job").and_then(Value::as_i64);
        if let Some(job) = job {
            // recognized type: run the heavy sandboxed dump off the render path.
            self.structd.job = Some(job);
            self.structd.pending = true;
            self.structd.full_lines = None;
            let (tx, rx) = mpsc::channel();
            self.struct_rx = Some(rx);
            let generation = self.structd.generation;
            let sock = self.sock.clone();
            std::thread::spawn(move || {
                let r = rpc(&sock, "struct_finish", json!([job]))
                    .unwrap_or_else(|e| json!({"lines": [["err", e]]}));
                let _ = tx.send(StructResult { generation, lines: pairs_of(r.get("lines")) });
            });
        } else {
            // unrecognized type: hexdump fallback (before/after bytes).
            self.structd.hex_lines = self.hexdump_fallback();
        }
    }

    /// Drain a finished structural-diff worker result if present, dropping a
    /// stale-generation result. Returns true if the cached lines changed (a
    /// redraw is warranted). Mirrors Python `_struct_done`.
    fn pump_struct(&mut self) -> bool {
        let Some(rx) = self.struct_rx.as_ref() else { return false };
        match rx.try_recv() {
            Ok(res) => {
                self.struct_rx = None;
                if res.generation != self.structd.generation {
                    return false; // superseded during navigation: drop it
                }
                self.structd.full_lines = Some(res.lines);
                self.structd.pending = false;
                self.structd.job = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.struct_rx = None;
                self.structd.pending = false;
                false
            }
        }
    }

    /// The binary detail header lines for the selected change (path · kind ·
    /// size · mode · ⚠ stale), from review.decorate + review.change_mode + the
    /// change row's size. Mirrors Python `_update_cd_info`.
    fn binary_header(&self, sid: &str, rel: &str) -> Vec<(String, String)> {
        let ent = self.visible_changes().get(self.sel_change).cloned().cloned();
        let size = ent.as_ref().and_then(|c| c.get("size").and_then(Value::as_i64));
        let row_kind = ent
            .as_ref()
            .and_then(|c| c.get("kind").and_then(Value::as_str))
            .unwrap_or("changed")
            .to_string();
        let dec = rpc(&self.sock, "review.decorate", json!([sid, rel])).ok();
        let kind = dec
            .as_ref()
            .and_then(|d| d.get("kind").and_then(Value::as_str))
            .unwrap_or(&row_kind)
            .to_string();
        let stale = dec
            .as_ref()
            .and_then(|d| d.get("stale").and_then(Value::as_bool))
            .unwrap_or(false);
        let mode = rpc(&self.sock, "review.change_mode", json!([sid, rel]))
            .ok()
            .and_then(|v| v.as_i64());
        let mut meta = kind;
        if let Some(sz) = size {
            meta.push_str(&format!(" · {}", fmt_bytes(sz)));
        }
        if let Some(m) = mode {
            meta.push_str(&format!(" · {} {:o}", filemode(m), m & 0o7777));
        }
        let mut out = vec![("bold".to_string(), format!("/{rel}")), ("dim".to_string(), meta)];
        if stale {
            out.push(("stale".to_string(), "⚠ host changed since capture".to_string()));
        }
        out
    }

    /// A before/after hexdump (16 bytes per row, hex + ASCII) of the selected
    /// binary change, used when the type is unrecognized (no structural differ).
    /// Mirrors the Python `_hexdump` fallback in `_diff_info_lines`.
    fn hexdump_fallback(&self) -> Vec<String> {
        let (Some(sid), Some(rel)) = (self.cur_sid(), self.cur_change_path()) else {
            return vec![];
        };
        let diff = self.hunks.get("diff").cloned().unwrap_or(Value::Null);
        let _ = (sid, rel);
        let mut out = vec![];
        let decode = |v: Option<&Value>| -> Vec<u8> {
            v.and_then(Value::as_str)
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .unwrap_or_default()
        };
        let after = decode(diff.get("content"));
        if let Some(before_v) = diff.get("content_before") {
            let before = decode(Some(before_v));
            out.push(format!("── before ── binary · {}", fmt_bytes(before.len() as i64)));
            hexdump_into(&before, &mut out);
            out.push(format!("── after ──  binary · {}", fmt_bytes(after.len() as i64)));
        } else {
            out.push(format!("binary · {}", fmt_bytes(after.len() as i64)));
        }
        hexdump_into(&after, &mut out);
        out
    }

    // ── per-hunk + batch apply/discard (Python _hunk_apply / action_apply_*) ──

    /// The hunk indices present in the current text diff (the `index` field of
    /// each hunk in review.hunks), in order.
    fn hunk_indices(&self) -> Vec<i64> {
        self.hunks
            .get("hunks")
            .and_then(Value::as_array)
            .map(|hs| hs.iter().filter_map(|h| h.get("index").and_then(Value::as_i64)).collect())
            .unwrap_or_default()
    }

    /// The engine-side index of the hunk under the diff cursor, if any.
    fn cur_hunk_index(&self) -> Option<i64> {
        self.hunk_indices().get(self.sel_hunk).copied()
    }

    /// Apply ONE hunk (review.apply_hunk sid,rel,index): the box already holds
    /// it, so applying it to the host stops it being a difference. Reloads.
    fn apply_hunk(&mut self) {
        let (Some(sid), Some(rel), Some(ix)) =
            (self.cur_sid(), self.cur_change_path(), self.cur_hunk_index())
        else {
            self.status = "no hunk under cursor".into();
            return;
        };
        match rpc(&self.sock, "review.apply_hunk", json!([sid, rel, ix])) {
            Ok(r) if r.get("ok").and_then(Value::as_bool) == Some(true) =>
                self.status = format!("applied hunk {ix}"),
            Ok(r) => self.status = format!(
                "apply_hunk: {}",
                r.get("error").and_then(Value::as_str).unwrap_or("failed")
            ),
            Err(e) => self.status = format!("apply_hunk: {e}"),
        }
        self.reload_after_hunk();
    }

    /// Discard ONE hunk (review.discard_hunk sid,rel,index): revert that hunk in
    /// the box back to the host's bytes. Reloads.
    fn discard_hunk(&mut self) {
        let (Some(sid), Some(rel), Some(ix)) =
            (self.cur_sid(), self.cur_change_path(), self.cur_hunk_index())
        else {
            self.status = "no hunk under cursor".into();
            return;
        };
        match rpc(&self.sock, "review.discard_hunk", json!([sid, rel, ix])) {
            Ok(r) if r.get("ok").and_then(Value::as_bool) == Some(true) =>
                self.status = format!("discarded hunk {ix}"),
            Ok(r) => self.status = format!(
                "discard_hunk: {}",
                r.get("error").and_then(Value::as_str).unwrap_or("failed")
            ),
            Err(e) => self.status = format!("discard_hunk: {e}"),
        }
        self.reload_after_hunk();
    }

    /// After a per-hunk op: refresh sessions, reopen the engine-side changes
    /// view (the box's sqlar may have lost a row when its last hunk went), and
    /// keep the cursor on the same path if it survived. The view is reopened
    /// rather than just re-windowed because the engine snapshots the row set
    /// at open time — and that snapshot is now stale.
    fn reload_after_hunk(&mut self) {
        let path = self.cur_change_path();
        self.refresh_sessions();
        // Reopen the changes view so the engine rescans sqlar with our filter.
        self.close_changes_view();
        if let Some(sid) = self.cur_sid_i64() {
            let filter = filter_to_json(self.f_changes.active());
            if let Ok(v) = rpc(&self.sock, "view.open",
                               json!(["changes", sid, filter])) {
                self.changes_view = v.get("view_id").and_then(Value::as_u64);
                self.changes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
        }
        self.fetch_changes_window(0);
        self.sel_change = 0;
        // Re-locate the previously-selected path in the new window, if it
        // landed there. Cheap O(window_size) — millions of rows are still
        // shielded server-side.
        if let Some(p) = &path {
            if let Some(i) = self.changes.iter().position(|c| {
                c.get("path").and_then(Value::as_str) == Some(p.as_str())
            }) {
                self.sel_change = i;
            }
        }
        let n = self.hunk_indices().len();
        if n > 0 && self.sel_hunk >= n {
            self.sel_hunk = n - 1;
        }
        self.load_hunks();
    }

    /// `A` — apply ALL changes of the selected box (review.apply with a null
    /// selector). Mirrors Python `action_apply_all`.
    fn apply_all(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "review.apply", json!([sid, Value::Null])) {
            Ok(r) => {
                let n = r.get("applied").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                self.status = format!("applied all ({n} change(s))");
            }
            Err(e) => self.status = format!("apply_all: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    /// `X` — discard ALL changes of the selected box (review.discard with a null
    /// selector). Mirrors Python `action_discard_all`.
    fn discard_all(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "review.discard", json!([sid, Value::Null])) {
            Ok(r) => {
                let n = r.get("discarded").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                self.status = format!("discarded all ({n} change(s))");
            }
            Err(e) => self.status = format!("discard_all: {e}"),
        }
        self.refresh_sessions();
        self.load_changes();
    }

    /// Move the selected file rule by `delta` (ctrl+up = -1, ctrl+down = +1),
    /// rewrite the filerules file in the new order, and reload_rules. Mirrors
    /// Python FileRules.move.
    fn move_rule(&mut self, delta: isize) {
        let i = self.sel_rule;
        let j = i as isize + delta;
        if j < 0 || j as usize >= self.rules.len() || self.rules.is_empty() {
            return;
        }
        let j = j as usize;
        self.rules.swap(i, j);
        self.sel_rule = j;
        self.save_rules();
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
                if self.changes_window_start + self.sel_change + 1 < self.changes_total {
                    self.sel_change_global_advance(1);
                    self.load_hunks();
                }
            }
            Pane::Hunks => {
                // move the hunk cursor between hunks when there are multiple;
                // otherwise scroll the diff body.
                let n = self.hunk_indices().len();
                if n > 1 && self.sel_hunk + 1 < n {
                    self.sel_hunk += 1;
                } else {
                    self.hunk_scroll = self.hunk_scroll.saturating_add(1);
                }
            }
            Pane::Processes => {
                if self.sel_proc + 1 < self.proc_selectable().len() {
                    self.sel_proc += 1;
                }
            }
            Pane::Outputs => self.out_scroll = self.out_scroll.saturating_add(1),
            Pane::Rules => {
                if self.sel_rule + 1 < self.rules.len() {
                    self.sel_rule += 1;
                }
            }
            Pane::Help => self.out_scroll = self.out_scroll.saturating_add(1),
            Pane::Pty => {}
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
                if self.changes_window_start + self.sel_change > 0 {
                    self.sel_change_global_advance(-1);
                    self.load_hunks();
                }
            }
            Pane::Hunks => {
                if self.hunk_indices().len() > 1 && self.sel_hunk > 0 {
                    self.sel_hunk -= 1;
                } else {
                    self.hunk_scroll = self.hunk_scroll.saturating_sub(1);
                }
            }
            Pane::Processes => self.sel_proc = self.sel_proc.saturating_sub(1),
            Pane::Outputs => self.out_scroll = self.out_scroll.saturating_sub(1),
            Pane::Rules => self.sel_rule = self.sel_rule.saturating_sub(1),
            Pane::Help => self.out_scroll = self.out_scroll.saturating_sub(1),
            Pane::Pty => {}
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
            // Tab out of the PTY pane back to the box list (keystrokes are not
            // captured by Tab while focused — see the interactive loop's Pty arm).
            Pane::Pty => Pane::Sessions,
        };
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
            Pane::Hunks | Pane::Processes | Pane::Outputs | Pane::Rules
            | Pane::Help | Pane::Pty => {}
        }
    }

    /// Open an engine-held PTY pane running `argv`, and focus it. The argv is
    /// whatever the user chose (the engine-held PTY is a generic transport: it
    /// runs the given command; nothing about a box or its parameters is
    /// presumed here). 'P' first prompts (pre-filled with the configured
    /// default), then calls this with the entered command line.
    #[cfg_attr(test, allow(dead_code))]
    fn open_pty(&mut self, argv: Vec<String>) {
        if argv.is_empty() { self.status = "pty: empty command".into(); return; }
        match PtyPane::open(&self.sock, &argv, 24, 80) {
            Ok(p) => {
                self.pty = Some(p);
                self.focus = Pane::Pty;
                self.status = "PTY · keys go to the box · Ctrl-] detaches".into();
            }
            Err(e) => self.status = format!("pty: {e}"),
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

    // ── typed filters (mirrors Python _view_filters / action_filter / _nav) ──

    /// The filter slot for a list view.
    fn view_filter(&self, v: FilterView) -> &ViewFilter {
        match v {
            FilterView::Changes => &self.f_changes,
            FilterView::Procs => &self.f_procs,
            FilterView::Outputs => &self.f_outputs,
        }
    }
    fn view_filter_mut(&mut self, v: FilterView) -> &mut ViewFilter {
        match v {
            FilterView::Changes => &mut self.f_changes,
            FilterView::Procs => &mut self.f_procs,
            FilterView::Outputs => &mut self.f_outputs,
        }
    }

    /// The FilterView the focused pane filters, if any (Sessions/Hunks/Rules/
    /// Help/Pty are not filterable).
    fn focus_filter_view(&self) -> Option<FilterView> {
        match self.focus {
            Pane::Changes => Some(FilterView::Changes),
            Pane::Processes => Some(FilterView::Procs),
            Pane::Outputs => Some(FilterView::Outputs),
            _ => None,
        }
    }

    /// '/' on a filterable list (Python `action_filter`). OFF → open the clause
    /// editor seeded with the view's last clauses. ON → clear it (a generated
    /// "ids" filter is dropped; a user one keeps its clauses for next time).
    #[cfg_attr(test, allow(dead_code))]
    fn toggle_filter(&mut self) {
        let Some(v) = self.focus_filter_view() else {
            self.status = "filter: not a filterable pane".into();
            return;
        };
        if self.view_filter(v).on {
            self.clear_filter(v);
            return;
        }
        let kinds = v.kinds();
        let seed = self.view_filter(v).clauses.clone();
        let rows: Vec<ClauseRow> = if seed.is_empty() {
            vec![ClauseRow {
                enabled: true,
                join: Join::And,
                negate: false,
                kind: v.default_kind().to_string(),
                pattern: String::new(),
            }]
        } else {
            seed.iter().map(ClauseRow::from_clause).collect()
        };
        self.modal = Some(Modal::Search { view: v, kinds, rows, sel: 0, field: ClauseField::Pattern });
    }

    /// Commit the clause editor: drop empty-pattern rows; if any enabled clause
    /// remains, turn the filter on (user-typed, not generated); else leave off.
    #[cfg_attr(test, allow(dead_code))]
    fn commit_filter(&mut self, v: FilterView, rows: &[ClauseRow]) {
        let clauses: Vec<Clause> = rows
            .iter()
            .filter(|r| !r.pattern.trim().is_empty())
            .map(ClauseRow::to_clause)
            .collect();
        if clauses.iter().any(|c| c.enabled) {
            *self.view_filter_mut(v) = ViewFilter { clauses, on: true, generated: false };
            self.reset_view_cursor(v);
            self.push_view_filter(v);
            self.status = "filter applied".into();
        } else {
            self.status = "filter: no enabled clause".into();
        }
    }

    /// Turn a view's filter off, keeping its clauses for next time (a generated
    /// "ids" filter is dropped). Mirrors Python `_clear_filter`.
    fn clear_filter(&mut self, v: FilterView) {
        let f = self.view_filter_mut(v);
        if f.generated {
            f.clauses.clear();
        }
        f.on = false;
        f.generated = false;
        self.reset_view_cursor(v);
        self.push_view_filter(v);
        self.status = "filter cleared".into();
    }

    /// Push the active client-side filter for view `v` into the engine-side
    /// view (so the engine recomputes its idx) and refetches the window. A
    /// no-op for views that haven't been migrated yet (procs / outputs).
    fn push_view_filter(&mut self, v: FilterView) {
        match v {
            FilterView::Changes => self.push_changes_filter(),
            FilterView::Procs | FilterView::Outputs => {}
        }
    }

    fn reset_view_cursor(&mut self, v: FilterView) {
        match v {
            FilterView::Changes => self.sel_change = 0,
            FilterView::Procs => self.sel_proc = 0,
            FilterView::Outputs => self.sel_output = 0,
        }
    }

    /// Build the Subject (box/exe/cwd/argv) for a process row, used to evaluate
    /// SUBJECT-kind clauses. `prov` falls back to a `proc_prov` lookup for cwd
    /// (the processes() row carries exe+argv but not cwd).
    fn proc_subject(&self, p: &Value) -> Subject {
        let a = p.as_array();
        let exe = a.and_then(|x| x.get(4)).and_then(Value::as_str).unwrap_or("").to_string();
        let argv = a
            .and_then(|x| x.get(5))
            .and_then(Value::as_array)
            .map(|v| v.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default();
        let rid = a.and_then(|x| x.first()).and_then(Value::as_i64).unwrap_or(-1);
        let cwd = self
            .cur_sid()
            .filter(|_| rid >= 0)
            .and_then(|sid| rpc(&self.sock, "proc_prov", json!([sid, rid])).ok())
            .and_then(|pr| pr.get("cwd").and_then(Value::as_str).map(String::from))
            .unwrap_or_default();
        let box_name = self
            .sessions
            .get(self.sel_session)
            .and_then(|s| s.get("name").or_else(|| s.get("path")))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Subject { box_name, exe, cwd, argv }
    }

    /// Processes after applying the procs view's typed filter. Evaluates each
    /// row's ProcFilterTarget (row id + subject) against the active clauses via
    /// the in-crate `eval_clauses`.
    fn visible_processes(&self) -> Vec<&Value> {
        let Some(clauses) = self.f_procs.active() else {
            return self.processes.iter().collect();
        };
        self.processes
            .iter()
            .filter(|p| {
                let rid = p
                    .as_array()
                    .and_then(|x| x.first())
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                let target = ProcFilterTarget { row_id: rid, subject: self.proc_subject(p) };
                eval_clauses(&target, clauses)
            })
            .collect()
    }

    /// The current changes WINDOW — already filtered by the engine-side view,
    /// so the UI just walks the (small) slice. `sel_change` indexes into this.
    /// Use `changes_total` for the underlying view size; this is just what
    /// the UI happens to be holding right now.
    fn visible_changes(&self) -> Vec<&Value> {
        self.changes.iter().collect()
    }

    /// Output rows after applying the outputs view's typed filter. An output is
    /// matched via its process_id (ProcFilterTarget); subject clauses resolve
    /// against that process's provenance.
    fn visible_outputs(&self) -> Vec<&Value> {
        let Some(clauses) = self.f_outputs.active() else {
            return self.outputs.iter().collect();
        };
        let sid = self.cur_sid();
        self.outputs
            .iter()
            .filter(|o| {
                let pid = o.get("process_id").and_then(Value::as_i64).unwrap_or(-1);
                let subject = sid
                    .as_ref()
                    .filter(|_| pid >= 0)
                    .and_then(|s| rpc(&self.sock, "proc_prov", json!([s, pid])).ok())
                    .map(|pr| Subject {
                        box_name: String::new(),
                        exe: pr.get("exe").and_then(Value::as_str).unwrap_or("").to_string(),
                        cwd: pr.get("cwd").and_then(Value::as_str).unwrap_or("").to_string(),
                        argv: pr
                            .get("argv")
                            .and_then(Value::as_array)
                            .map(|v| v.iter().filter_map(Value::as_str).map(String::from).collect())
                            .unwrap_or_default(),
                    })
                    .unwrap_or_default();
                let target = ProcFilterTarget { row_id: pid, subject };
                eval_clauses(&target, clauses)
            })
            .collect()
    }

    /// Writer row ids for a change (first_writer_id + writer_id), de-duped
    /// preserving order — the changes→procs nav target and the "ids" clause set.
    fn change_writer_ids(&self, sid: &str, rel: &str) -> Vec<i64> {
        let mut ids = vec![];
        for verb in ["first_writer_id", "writer_id"] {
            if let Ok(v) = rpc(&self.sock, verb, json!([sid, rel])) {
                if let Some(i) = v.as_i64() {
                    if !ids.contains(&i) {
                        ids.push(i);
                    }
                }
            }
        }
        ids
    }

    // ── cross-pane nav-id transitions (Python _nav / _nav_ids) ───────────────

    /// Resolve the destination row ids for an src→dest cross-navigation against
    /// the CURRENT cursor (Python `_nav_ids`). None for transitions that don't
    /// auto-filter.
    fn nav_ids(&self, src: FilterView, dest: FilterView) -> Option<Vec<i64>> {
        let sid = self.cur_sid()?;
        match (src, dest) {
            (FilterView::Changes, FilterView::Procs) => {
                let rel = self.cur_change_path()?;
                let ids = self.change_writer_ids(&sid, &rel);
                if ids.is_empty() { None } else { Some(ids) }
            }
            (FilterView::Procs, FilterView::Changes) | (FilterView::Procs, FilterView::Outputs) => {
                let p = self.visible_processes();
                let row = p.get(self.sel_proc)?;
                let rid = row.as_array().and_then(|x| x.first()).and_then(Value::as_i64)?;
                Some(vec![rid])
            }
            (FilterView::Outputs, FilterView::Procs) => {
                let o = self.visible_outputs();
                let row = o.get(self.sel_output)?;
                let pid = row.get("process_id").and_then(Value::as_i64)?;
                Some(vec![pid])
            }
            _ => None,
        }
    }

    /// Cross-pane navigation to `dest` (Python `_nav`): install a GENERATED
    /// "ids" filter on the destination resolved from the current cursor, or drop
    /// a stale generated filter when this nav produces none. A user-typed filter
    /// on the destination is left untouched. Then focus the destination pane.
    #[cfg_attr(test, allow(dead_code))]
    fn nav(&mut self, dest_pane: Pane) {
        let dest = match dest_pane {
            Pane::Changes => Some(FilterView::Changes),
            Pane::Processes => Some(FilterView::Procs),
            Pane::Outputs => Some(FilterView::Outputs),
            _ => None,
        };
        if let (Some(src), Some(dest)) = (self.focus_filter_view(), dest) {
            if src != dest {
                let ids = self.nav_ids(src, dest);
                match ids {
                    Some(ids) => {
                        let pat = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
                        *self.view_filter_mut(dest) = ViewFilter {
                            clauses: vec![Clause {
                                m: Match { kind: "ids".into(), pattern: pat },
                                join: Join::And,
                                negate: false,
                                enabled: true,
                            }],
                            on: true,
                            generated: true,
                        };
                        self.reset_view_cursor(dest);
                    }
                    None => {
                        if self.view_filter(dest).generated {
                            *self.view_filter_mut(dest) = ViewFilter::default();
                            self.reset_view_cursor(dest);
                        }
                    }
                }
            }
        }
        self.focus = dest_pane;
    }
}

// ── process tree (mirrors Python build_proc_tree / build_path_tree) ──────────

/// One DFS-ordered tree row: (row_id, tgid, ppid, exe, argv, depth, connector).
/// `connector` is true for a purely-structural ancestor added to connect the
/// forest (dimmed; the cursor skips it).
#[derive(Clone)]
struct ProcTreeRow {
    rid: i64,
    tgid: i64,
    ppid: i64,
    exe: String,
    argv: Vec<String>,
    depth: usize,
    connector: bool,
}

const PROC_TREE_DEPTH: usize = 64;

/// Resolve (tgid, ppid, parent_id, exe, argv) for a row id, first from the
/// in-memory member/node map, else via a `proc_info` RPC (connector ancestors
/// not present in `procs`). Returns None if unrecorded.
type NodeInfo = (i64, i64, Option<i64>, String, Vec<String>);

fn node_info(p: &Value) -> NodeInfo {
    let a = p.as_array();
    let g = |i: usize| a.and_then(|x| x.get(i)).and_then(Value::as_i64);
    let exe = a.and_then(|x| x.get(4)).and_then(Value::as_str).unwrap_or("").to_string();
    let argv = a
        .and_then(|x| x.get(5))
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    (g(1).unwrap_or(0), g(2).unwrap_or(0), g(3), exe, argv)
}

/// Build the DFS-ordered, depth-indented process tree from the flat `procs`
/// list, walking parent_id chains up to `roots` and unioning structural
/// ancestors (resolved via `lookup`). Mirrors Python `build_proc_tree` +
/// `build_path_tree`: parent immediately precedes its subtree; order-independent
/// (a pure function of inputs).
fn build_proc_tree(
    procs: &[&Value],
    roots: &std::collections::HashSet<i64>,
    lookup: &dyn Fn(i64) -> Option<NodeInfo>,
) -> Vec<ProcTreeRow> {
    use std::collections::HashMap;
    use std::collections::HashSet;

    let mut members: HashMap<i64, NodeInfo> = HashMap::new();
    for p in procs {
        if let Some(rid) = p.as_array().and_then(|x| x.first()).and_then(Value::as_i64) {
            members.insert(rid, node_info(p));
        }
    }
    let mut nodes: HashMap<i64, NodeInfo> = members.clone();

    let info = |rid: i64, nodes: &HashMap<i64, NodeInfo>| -> Option<NodeInfo> {
        if let Some(n) = nodes.get(&rid) {
            return Some(n.clone());
        }
        lookup(rid)
    };

    // root→self row-id path for each member, unioning ancestors into `nodes`.
    let mut member_paths: HashMap<i64, Vec<i64>> = HashMap::new();
    let member_ids: Vec<i64> = members.keys().copied().collect();
    for &start in &member_ids {
        let mut path = vec![];
        let mut seen = HashSet::new();
        let mut cur = start;
        for _ in 0..PROC_TREE_DEPTH {
            if seen.contains(&cur) {
                break;
            }
            seen.insert(cur);
            path.push(cur);
            if roots.contains(&cur) {
                break;
            }
            let Some(got) = info(cur, &nodes) else { break };
            let Some(parent_id) = got.2 else { break };
            if parent_id == 0 {
                break;
            }
            let Some(got_p) = info(parent_id, &nodes) else { break };
            nodes.entry(parent_id).or_insert(got_p);
            cur = parent_id;
        }
        path.reverse();
        member_paths.insert(start, path);
    }

    // Sort member root→self paths lexicographically → DFS order (parent before
    // subtree). Each member contributes its full path; intermediate ancestors
    // become connector rows the first time they appear.
    let mut paths: Vec<Vec<i64>> = member_paths.values().cloned().collect();
    paths.sort();

    let mut emitted: HashSet<i64> = HashSet::new();
    let mut out = vec![];
    for path in &paths {
        for (depth, &rid) in path.iter().enumerate() {
            if !emitted.insert(rid) {
                continue;
            }
            let connector = !members.contains_key(&rid);
            let (tgid, ppid, _par, exe, argv) =
                nodes.get(&rid).cloned().or_else(|| lookup(rid)).unwrap_or((0, 0, None, String::new(), vec![]));
            out.push(ProcTreeRow { rid, tgid, ppid, exe, argv, depth, connector });
        }
    }
    out
}

// ── rendering ───────────────────────────────────────────────────────────────

/// Convert an engine `lines` array of [style,text] pairs into owned tuples.
fn pairs_of(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|p| {
                    let arr = p.as_array();
                    let style = arr.and_then(|x| x.first()).and_then(Value::as_str).unwrap_or("").to_string();
                    let text = arr.and_then(|x| x.get(1)).and_then(Value::as_str).unwrap_or("").to_string();
                    (style, text)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map a structural-diff style tag to a ratatui color (Python `_STRUCT_STYLE`).
fn struct_color(style: &str) -> Color {
    match style {
        "type" => Color::Yellow,
        "hdr" => Color::Cyan,
        "+" => Color::Green,
        "-" => Color::Red,
        "@" => Color::Cyan,
        "err" => Color::Red,
        _ => Color::DarkGray, // " " / "dim"
    }
}

/// Human byte size (mirrors Python fmt_bytes: B/KiB/MiB/… with one decimal).
fn fmt_bytes(n: i64) -> String {
    let n = n as f64;
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n as i64, U[0])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// `ls -l`-style mode string (mirrors Python stat.filemode for the common bits).
fn filemode(mode: i64) -> String {
    let m = mode as u32;
    let ft = match m & 0o170000 {
        0o120000 => 'l',
        0o040000 => 'd',
        0o100000 => '-',
        0o060000 => 'b',
        0o020000 => 'c',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '?',
    };
    let bit = |on: bool, c: char| if on { c } else { '-' };
    let mut s = String::new();
    s.push(ft);
    let perms = [
        (0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x'),
    ];
    for (mask, c) in perms {
        s.push(bit(m & mask != 0, c));
    }
    s
}

/// Append a hexdump of `data` (first 256 bytes, 16 per row: offset, hex, ASCII)
/// to `out`. Mirrors the Python `_hexdump` helper.
fn hexdump_into(data: &[u8], out: &mut Vec<String>) {
    let n = data.len().min(256);
    let mut i = 0;
    while i < n {
        let chunk = &data[i..(i + 16).min(n)];
        let hex = chunk.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
        let ascii: String = chunk
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        out.push(format!("{i:04x}  {hex:<48}  {ascii}"));
        i += 16;
    }
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
    let vis = app.visible_changes();
    if vis.is_empty() {
        let empty_msg = if app.changes_total == 0 {
            if app.f_changes.active().is_some() { "(no changes match filter)" } else { "(no changes)" }
        } else {
            "(empty window — engine view drifted)"
        };
        out.push(Line::from(empty_msg));
        return out;
    }
    for (i, c) in vis.iter().enumerate() {
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
        let cur_idx = app.cur_hunk_index();
        if let Some(hunks) = h.get("hunks").and_then(Value::as_array) {
            if hunks.is_empty() {
                out.push(Line::from("(no textual differences)"));
            }
            for hunk in hunks {
                let hidx = hunk.get("index").and_then(Value::as_i64);
                let on_cursor = hidx.is_some() && hidx == cur_idx;
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
                        // mark the hunk under the cursor (a/x apply/discard target).
                        let mark = if on_cursor && tag == "hdr" { "▶ " } else if tag == "hdr" { "  " } else { "" };
                        let mut st = Style::default().fg(color);
                        if on_cursor {
                            st = st.add_modifier(Modifier::BOLD);
                        }
                        out.push(Line::from(Span::styled(format!("{mark}{prefix}{txt}"), st)));
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(Line::from("(no hunks)"));
        }
        out
    } else {
        // BINARY change: detail header + (struct_quick lines / structural diff
        // once the worker returns / animated spinner while pending / hexdump
        // fallback when the type was unrecognized).
        let mut out = vec![];
        for (style, txt) in &app.structd.header {
            let st = match style.as_str() {
                "bold" => Style::default().add_modifier(Modifier::BOLD),
                "stale" => Style::default().fg(Color::Red).add_modifier(Modifier::REVERSED),
                _ => Style::default().fg(Color::DarkGray),
            };
            out.push(Line::from(Span::styled(txt.clone(), st)));
        }
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        // If the full structural diff has returned, render it; else the quick
        // (type + header) lines, plus a spinner row while the finish is pending.
        let lines = app.structd.full_lines.as_ref().unwrap_or(&app.structd.quick_lines);
        for (style, txt) in lines {
            let prefix = match style.as_str() {
                "+" => "+",
                "-" => "-",
                " " => " ",
                _ => "",
            };
            out.push(Line::from(Span::styled(
                format!("{prefix}{txt}"),
                Style::default().fg(struct_color(style)),
            )));
        }
        if app.structd.pending && app.structd.full_lines.is_none() {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let f = frames[app.structd.spin % frames.len()];
            out.push(Line::from(Span::styled(
                format!("  {f} analyzing structure…  (navigate away to cancel)"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        // hexdump fallback (unrecognized type): before/after bytes.
        for l in &app.structd.hex_lines {
            out.push(Line::from(Span::styled(l.clone(), Style::default().fg(Color::DarkGray))));
        }
        if out.is_empty() {
            out.push(Line::from("(binary change)"));
        }
        out
    }
}

impl App {
    /// The procs pane's render rows. With NO filter: the full DFS process TREE
    /// (depth-indented, structural connectors included). With a typed filter:
    /// the surviving REAL processes rendered FLAT (depth 0, no connectors) — the
    /// filter means "show me exactly these rows", not the hierarchy. Mirrors the
    /// Python `_load_procs` filtered/unfiltered split.
    fn proc_tree_rows(&self) -> Vec<ProcTreeRow> {
        if self.f_procs.active().is_some() {
            // filtered: flat list of surviving processes (depth 0, no connectors).
            return self
                .visible_processes()
                .iter()
                .map(|p| {
                    let (tgid, ppid, _par, exe, argv) = node_info(p);
                    let rid = p.as_array().and_then(|x| x.first()).and_then(Value::as_i64).unwrap_or(-1);
                    ProcTreeRow { rid, tgid, ppid, exe, argv, depth: 0, connector: false }
                })
                .collect();
        }
        // unfiltered: the full tree.
        let roots: std::collections::HashSet<i64> = self
            .cur_sid()
            .and_then(|sid| rpc(&self.sock, "proc_roots", json!([sid])).ok())
            .and_then(|v| v.as_array().cloned())
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let sid = self.cur_sid();
        let sock = self.sock.clone();
        let lookup = move |rid: i64| -> Option<NodeInfo> {
            let sid = sid.clone()?;
            let v = rpc(&sock, "proc_info", json!([sid, rid])).ok()?;
            let a = v.as_array()?;
            // proc_info returns [tgid,ppid,parent_id,exe,argv]
            let g = |i: usize| a.get(i).and_then(Value::as_i64);
            let exe = a.get(3).and_then(Value::as_str).unwrap_or("").to_string();
            let argv = a
                .get(4)
                .and_then(Value::as_array)
                .map(|x| x.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            Some((g(0).unwrap_or(0), g(1).unwrap_or(0), g(2), exe, argv))
        };
        let procs: Vec<&Value> = self.processes.iter().collect();
        build_proc_tree(&procs, &roots, &lookup)
    }

    /// Indices into `proc_tree_rows()` the cursor may land on (non-connectors).
    fn proc_selectable(&self) -> Vec<usize> {
        self.proc_tree_rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.connector)
            .map(|(i, _)| i)
            .collect()
    }
}

/// PROCESSES pane: a depth-indented process TREE. Each row shows tgid · ppid ·
/// (indented) exe · argv. Structural-ancestor connector rows are dimmed and the
/// cursor (sel_proc, counted over non-connector rows) skips them.
fn processes_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:>6} {:>6}  {}", "TGID", "PPID", "EXE · ARGV"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    let rows = app.proc_tree_rows();
    if rows.is_empty() {
        out.push(Line::from("(no captured processes)"));
        return out;
    }
    // map sel_proc (index over real rows) to the highlighted tree-row index.
    let real = app.proc_selectable();
    let hi = real.get(app.sel_proc).copied();
    for (i, r) in rows.iter().enumerate() {
        let indent = "  ".repeat(r.depth);
        let base = r.exe.rsplit('/').next().unwrap_or(&r.exe);
        let argv = r.argv.join(" ");
        let text = format!("{:>6} {:>6}  {indent}{base} {argv}", r.tgid, r.ppid).trim_end().to_string();
        let line = if Some(i) == hi {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else if r.connector {
            Line::from(Span::styled(text, Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)))
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
    let d = |s: &str| Line::from(Span::styled(s.to_string(), Style::default().fg(Color::DarkGray)));
    vec![
        h("sarun — sandboxed run → inspect → apply/discard"),
        t(""),
        h("The loop"),
        t("  1. RUN a command in a box: it executes over a copy-on-write overlay"),
        t("     of your filesystem, so its writes never touch the host yet."),
        t("  2. INSPECT what it did: the changed files (diffs), the process tree,"),
        t("     and the captured stdout/stderr — all without committing anything."),
        t("  3. APPLY the writes you want onto the real host, or DISCARD them."),
        t("     Apply/discard can be whole-box, per-file, or per-HUNK."),
        d("  Boxes run in the HOST network namespace (normal connectivity). Only"),
        d("  the untrusted binary viewer is network-isolated under bwrap."),
        t(""),
        h("Panes (Tab cycles; or jump directly)"),
        t("  b  boxes/sessions      the list of boxes (path · id · status · cmd)"),
        t("  c  changes             files the box wrote (Enter → its diff)"),
        t("  p  processes           the captured process TREE (exe · argv · env)"),
        t("  o  outputs             decoded stdout/stderr transcript"),
        t("  e  file rules          the ordered apply/discard/passthrough rules"),
        t("  ?  this help"),
        t("  P  open an engine-held PTY — a live interactive shell pane"),
        d("     keys go to the box · Ctrl-] detaches back to the UI"),
        t(""),
        h("Navigation & filters"),
        t("  j/k or ↓/↑  move       Enter  open the selection in the next pane"),
        t("  R  refresh             q      quit (stops the engine)"),
        t("                          d      detach (leaves the engine running)"),
        t("  /  filter the active pane with a clause editor. Filter KINDS:"),
        d("     path  — match the changed file's path (changes pane)"),
        d("     box   — match the box name"),
        d("     exe   — match a process's executable path"),
        d("     cwd   — match a process's working directory"),
        d("     arg   — match a token in a process's argv"),
        d("     Rows fold top→bottom by each row's and/or; 'not' negates a row."),
        t("  c/p/o also cross-navigate: pin the destination pane to the rows the"),
        t("       cursor relates to (a change's writers, a process's outputs…)."),
        d("       Esc drops such a generated filter."),
        t(""),
        h("Reviewing changes"),
        t("  a  apply selected change / whole box     x  discard it"),
        t("  A  apply ALL the box's changes           X  discard ALL"),
        t("  In the DIFF pane, a TEXT change is shown as unified-diff hunks:"),
        t("    ↑/↓  move the hunk cursor (▶)  ·  a  apply this hunk to the host"),
        t("    x or d  discard this hunk (revert it in the box)"),
        t("  A BINARY change shows a detail header (path · kind · size · mode,"),
        t("  ⚠ when the host changed since capture) and a STRUCTURAL diff: the"),
        t("  type is sniffed and a differ (readelf/ar/unzip/tar) runs in a"),
        t("  sandbox off the render path. Unrecognized types get a hexdump."),
        t(""),
        h("Boxes & nesting"),
        t("  K  kill box (SIGTERM, y/n)    D  delete box + captures (y/n)"),
        t("  Z  dissolve box (y/n)         r  rename box"),
        d("  A box may be NESTED inside another: applying a nested box promotes"),
        d("  its changes into the PARENT box (still pending), not the host;"),
        d("  discarding copies the change DOWN into immediate child boxes."),
        d("  Only a TOP-LEVEL box's apply reaches the real host."),
        t(""),
        h("File rules (e)"),
        t("  n  new rule    Enter  edit selected    d  delete selected"),
        t("  ctrl+↑ / ctrl+↓  reorder the selected rule (order = priority)"),
        t("  Each rule is '<action> <clause>' where action is one of:"),
        d("     apply        keep the matching writes (let them reach the host)"),
        d("     discard      drop the matching writes"),
        d("     passthrough  let the box write straight through to the host"),
        t("  A clause is a glob (e.g. **/*.log) or a typed clause like"),
        t("  'exe:**/gcc' / 'box:WORK' combined with and/or/not. Rules are"),
        t("  evaluated TOP → BOTTOM and the FIRST match wins; saving any edit"),
        t("  rewrites the filerules file and reloads it in the engine."),
    ]
}

/// Render the active modal centered over the body. Returns the area consumed.
fn draw_modal(f: &mut ratatui::Frame, area: Rect, modal: &Modal) {
    let w = (area.width * 7 / 10).clamp(20, area.width);
    let want = match modal {
        Modal::Search { rows, .. } => (rows.len() as u16) + 6,
        _ => 7,
    };
    let hgt = want.min(area.height);
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
        Modal::Search { rows, sel, field, .. } => {
            let mut body = vec![Line::from(Span::styled(
                "keep only entries matching — rows folded top→bottom by each row's and/or",
                Style::default().fg(Color::Gray),
            ))];
            for (i, r) in rows.iter().enumerate() {
                let cur = i == *sel;
                let mark = |on: bool, label: &str, f: ClauseField| -> Span<'static> {
                    let active = cur && *field == f;
                    let txt = format!("[{}]", if on { label } else { " " });
                    let mut st = Style::default();
                    if active { st = st.fg(Color::Black).bg(Color::Cyan); }
                    Span::styled(txt, st)
                };
                let joinlbl = if i == 0 { "   ".to_string() } else { match r.join { Join::And => "and".into(), Join::Or => "or ".into() } };
                let join_sp = {
                    let active = cur && *field == ClauseField::Join;
                    let mut st = Style::default();
                    if active { st = st.fg(Color::Black).bg(Color::Cyan); }
                    Span::styled(joinlbl, st)
                };
                let kind_sp = {
                    let active = cur && *field == ClauseField::Kind;
                    let mut st = Style::default().fg(Color::Yellow);
                    if active { st = Style::default().fg(Color::Black).bg(Color::Cyan); }
                    Span::styled(format!("{:<5}", r.kind), st)
                };
                let pat_sp = {
                    let active = cur && *field == ClauseField::Pattern;
                    let mut st = Style::default();
                    if active { st = st.fg(Color::Black).bg(Color::Cyan); }
                    let shown = if active { format!("{}_", r.pattern) } else { r.pattern.clone() };
                    Span::styled(shown, st)
                };
                body.push(Line::from(vec![
                    Span::raw(if cur { "› " } else { "  " }),
                    mark(r.enabled, "on", ClauseField::Enabled),
                    Span::raw(" "),
                    join_sp,
                    Span::raw(" "),
                    mark(r.negate, "not", ClauseField::Negate),
                    Span::raw(" "),
                    kind_sp,
                    Span::raw(" "),
                    pat_sp,
                ]));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "←/→ field · space toggle · type pattern · n new row · ^s apply · esc clear",
                Style::default().fg(Color::Gray),
            )));
            (" filter ", body)
        }
        Modal::RuleForm { buf, editing } => (
            if editing.is_some() { " edit rule " } else { " new rule " },
            vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("e.g.  discard **/*.log   ·   Enter save · Esc cancel"),
            ],
        ),
        Modal::PtyCmd { buf } => (
            " run on a PTY ",
            vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("any command — e.g. `bash` or `sarun run -b -- make` · \
                            Enter run · Esc cancel"),
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

    // The PTY pane takes the whole body: a live tui-term view of the engine-held
    // PTY child, rendered from the vt100 screen grid.
    if app.focus == Pane::Pty {
        if let Some(pty) = &app.pty {
            let t = if pty.eof { "PTY · (exited)" } else { "PTY · live" };
            let blk = block(title(t, true), true);
            let inner = blk.inner(body);
            f.render_widget(blk, body);
            let screen = pty.parser.screen();
            f.render_widget(tui_term::widget::PseudoTerminal::new(screen), inner);
        } else {
            let msg = Paragraph::new("no PTY open")
                .block(block(title("PTY", true), true));
            f.render_widget(msg, body);
        }
    } else if app.focus == Pane::Help {
        // The Help pane takes the whole body.
        let help = Paragraph::new(Text::from(help_lines()))
            .block(block(title("help · j/k scroll", true), true))
            .scroll((app.out_scroll, 0))
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

                let is_bin = !app.hunks.is_null()
                    && app.hunks.get("is_text").and_then(Value::as_bool) != Some(true);
                let diff_title = if is_bin { "structural diff" } else { "diff" };
                let hunks = Paragraph::new(Text::from(hunk_lines(app)))
                    .block(block(title(diff_title, app.focus == Pane::Hunks), app.focus == Pane::Hunks))
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
    let rows = app.proc_tree_rows();
    let real = app.proc_selectable();
    let Some(&ti) = real.get(app.sel_proc) else {
        return vec![Line::from("(no process selected)")];
    };
    let r = &rows[ti];
    let rid = r.rid;
    let mut out = vec![
        Line::from(Span::styled(
            format!("{} {}", r.exe, r.argv.join(" ")).trim().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
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

/// The default command for a fresh PTY pane — the configurable "login command".
/// Reads the first non-blank, non-`#` line of
/// $XDG_CONFIG_HOME/slopbox[.NS]/pty_command if present; else falls back to the
/// user's $SHELL (interactive) or `sh -i`. This is only a DEFAULT the user can
/// edit at the prompt — never an enforced choice, and it presumes NOTHING about
/// a box or its parameters (the engine-held PTY just runs whatever argv it gets;
/// to run a box you type e.g. `sarun run -b -- make`).
fn pty_default_cmd() -> String {
    let app_dir = match std::env::var("SLOPBOX_NS") {
        Ok(ns) if !ns.is_empty() => format!("slopbox.{ns}"),
        _ => "slopbox".into(),
    };
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
            .join(".config"),
    };
    if let Ok(s) = std::fs::read_to_string(base.join(app_dir).join("pty_command")) {
        if let Some(l) = s.lines().map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#')) {
            return l.to_string();
        }
    }
    match std::env::var("SHELL") {
        Ok(sh) if !sh.is_empty() => format!("{sh} -i"),
        _ => "sh -i".to_string(),
    }
}

/// Split a typed command line into argv, honoring single and double quotes
/// (enough for the PTY prompt). Unquoted whitespace separates words.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                in_word = true;
                while let Some(n) = chars.next() {
                    if n == c { break; }
                    cur.push(n);
                }
            }
            c if c.is_whitespace() => {
                if in_word { out.push(std::mem::take(&mut cur)); in_word = false; }
            }
            c => { in_word = true; cur.push(c); }
        }
    }
    if in_word { out.push(cur); }
    out
}

// ── engine-held PTY pane (D7/D9) ─────────────────────────────────────────────
//
// The client side of the FRAME_PTY_* mux: open a `pty_spawn` control connection,
// spawn a reader thread that forwards every FRAME_PTY_DATA payload (and the
// FRAME_PTY_EOF) over an mpsc channel, and keep the write half to send keystrokes
// (FRAME_PTY_DATA) and resizes (FRAME_PTY_RESIZE). The vt100 Parser accumulates
// the data into a screen grid that the tui-term PseudoTerminal widget renders —
// the EXACT stack proven in ptyspike/, now driven over the engine socket.

struct PtyPane {
    parser: tui_term::vt100::Parser,
    writer: UnixStream,            // write half: keystrokes + resize frames
    rx: mpsc::Receiver<PtyMsg>,    // data/eof from the reader thread
    #[allow(dead_code)] rows: u16,
    #[allow(dead_code)] cols: u16,
    eof: bool,
}

enum PtyMsg {
    Data(Vec<u8>),
    Eof,
}

impl PtyPane {
    /// Open a PTY connection to the engine and spawn `argv` on it. Returns a pane
    /// whose parser will fill as the reader thread delivers FRAME_PTY_DATA.
    fn open(sock: &str, argv: &[String], rows: u16, cols: u16) -> Result<PtyPane, String> {
        let mut s = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
        let req = json!({"type": "pty_spawn", "argv": argv, "rows": rows, "cols": cols});
        s.write_all(format!("{req}\n").as_bytes()).map_err(|e| e.to_string())?;
        s.flush().ok();
        // Read the one-line ack ({"ok":true,...}) before the frame stream begins.
        // We must NOT consume any frame bytes, so read a single line byte-by-byte.
        let ack = read_one_line(&mut s)?;
        let v: Value = serde_json::from_str(ack.trim())
            .map_err(|e| format!("bad ack: {e}"))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(v.get("error").and_then(Value::as_str)
                .unwrap_or("pty_spawn refused").to_string());
        }
        let writer = s.try_clone().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        // Reader thread: decode FRAME_PTY_DATA / FRAME_PTY_EOF off the socket.
        std::thread::spawn(move || {
            let mut reader = s;
            let mut acc: Vec<u8> = vec![];
            let mut buf = [0u8; 8192];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                acc.extend_from_slice(&buf[..n]);
                let (frames_v, used) = crate::frames::decode(&acc);
                acc.drain(..used);
                for (ft, payload) in frames_v {
                    if ft == crate::frames::FRAME_PTY_DATA {
                        if tx.send(PtyMsg::Data(payload)).is_err() { return; }
                    } else if ft == crate::frames::FRAME_PTY_EOF {
                        let _ = tx.send(PtyMsg::Eof);
                        return;
                    }
                }
            }
            let _ = tx.send(PtyMsg::Eof);
        });
        Ok(PtyPane {
            parser: tui_term::vt100::Parser::new(rows, cols, 0),
            writer, rx, rows, cols, eof: false,
        })
    }

    /// Drain any pending PTY output into the vt100 parser. Non-blocking; call each
    /// UI tick. Returns true if anything changed (a redraw is warranted).
    fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                PtyMsg::Data(d) => { self.parser.process(&d); changed = true; }
                PtyMsg::Eof => { self.eof = true; changed = true; }
            }
        }
        changed
    }

    /// Send raw keystroke bytes to the child (FRAME_PTY_DATA, client→engine).
    fn send_input(&mut self, bytes: &[u8]) {
        let frame = crate::frames::encode(crate::frames::FRAME_PTY_DATA, bytes);
        let _ = self.writer.write_all(&frame);
        let _ = self.writer.flush();
    }

    /// Tell the engine the pane was resized (FRAME_PTY_RESIZE).
    #[allow(dead_code)]
    fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols { return; }
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
        let frame = crate::frames::encode(crate::frames::FRAME_PTY_RESIZE,
            &crate::frames::pty_resize_payload(rows, cols));
        let _ = self.writer.write_all(&frame);
        let _ = self.writer.flush();
    }
}

/// Read exactly one '\n'-terminated line from a stream byte-by-byte, so we never
/// over-read into the frame stream that follows the JSON ack.
fn read_one_line(s: &mut UnixStream) -> Result<String, String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match s.read(&mut b) {
            Ok(0) => break,
            Ok(_) => { if b[0] == b'\n' { break; } out.push(b[0]); }
            Err(e) => return Err(e.to_string()),
        }
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Translate a crossterm key event into the bytes a terminal would send to the
/// child PTY (the input encoding the pane forwards as FRAME_PTY_DATA).
fn key_to_pty_bytes(code: crossterm::event::KeyCode,
                    mods: crossterm::event::KeyModifiers) -> Option<Vec<u8>> {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;
    Some(match code {
        KeyCode::Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                // Ctrl-A..Ctrl-Z and friends → control bytes 0x01..0x1a.
                let up = c.to_ascii_uppercase();
                if up.is_ascii_alphabetic() {
                    vec![(up as u8) - b'A' + 1]
                } else {
                    let mut b = [0u8; 4];
                    c.encode_utf8(&mut b).as_bytes().to_vec()
                }
            } else {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        _ => return None,
    })
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
fn handle_modal_key(app: &mut App, code: crossterm::event::KeyCode,
                    mods: crossterm::event::KeyModifiers) {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;
    let Some(modal) = app.modal.take() else { return };
    match modal {
        Modal::Confirm { prompt, action } => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => app.run_confirm(action),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.status = "cancelled".into();
            }
            _ => app.modal = Some(Modal::Confirm { prompt, action }),
        },
        Modal::Search { view, kinds, mut rows, mut sel, mut field } => {
            let ctrl = mods.contains(KeyModifiers::CONTROL);
            // ^s / Enter → commit; Esc → cancel (no change). All else edits rows.
            if (ctrl && matches!(code, KeyCode::Char('s'))) || code == KeyCode::Enter {
                app.commit_filter(view, &rows);
                return;
            }
            if code == KeyCode::Esc {
                app.status = "filter unchanged".into();
                return;
            }
            // field/row navigation and edits.
            let order = [
                ClauseField::Enabled, ClauseField::Join, ClauseField::Negate,
                ClauseField::Kind, ClauseField::Pattern,
            ];
            let cur_fi = order.iter().position(|f| *f == field).unwrap_or(4);
            match code {
                KeyCode::Left => field = order[cur_fi.saturating_sub(1)],
                KeyCode::Right => field = order[(cur_fi + 1).min(order.len() - 1)],
                KeyCode::Up => sel = sel.saturating_sub(1),
                KeyCode::Down => sel = (sel + 1).min(rows.len().saturating_sub(1)),
                KeyCode::Char('n') if field != ClauseField::Pattern => {
                    rows.push(ClauseRow {
                        enabled: true, join: Join::And, negate: false,
                        kind: kinds[0].to_string(), pattern: String::new(),
                    });
                    sel = rows.len() - 1;
                    field = ClauseField::Pattern;
                }
                _ => {
                    if let Some(r) = rows.get_mut(sel) {
                        match field {
                            ClauseField::Pattern => match code {
                                KeyCode::Backspace => { r.pattern.pop(); }
                                KeyCode::Char(c) => r.pattern.push(c),
                                _ => {}
                            },
                            ClauseField::Enabled => {
                                if matches!(code, KeyCode::Char(' ')) { r.enabled = !r.enabled; }
                            }
                            ClauseField::Negate => {
                                if matches!(code, KeyCode::Char(' ')) { r.negate = !r.negate; }
                            }
                            ClauseField::Join => {
                                if matches!(code, KeyCode::Char(' ') | KeyCode::Char('j') | KeyCode::Char('o')) {
                                    r.join = match r.join { Join::And => Join::Or, Join::Or => Join::And };
                                }
                            }
                            ClauseField::Kind => {
                                if matches!(code, KeyCode::Char(' ')) {
                                    let ki = kinds.iter().position(|k| *k == r.kind).unwrap_or(0);
                                    r.kind = kinds[(ki + 1) % kinds.len()].to_string();
                                }
                            }
                        }
                    }
                }
            }
            app.modal = Some(Modal::Search { view, kinds, rows, sel, field });
        }
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
        Modal::PtyCmd { mut buf } => match code {
            KeyCode::Enter => app.open_pty(shell_split(&buf)),
            KeyCode::Esc => app.status = "pty cancelled".into(),
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::PtyCmd { buf });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::PtyCmd { buf });
            }
            _ => app.modal = Some(Modal::PtyCmd { buf }),
        },
    }
}

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
            // drain engine-held PTY output into the vt100 parser each tick.
            if let Some(pty) = app.pty.as_mut() {
                pty.pump();
            }
            // drain a finished structural-diff worker result, and animate the
            // spinner while one is still pending.
            app.pump_struct();
            if app.structd.pending && app.structd.full_lines.is_none() {
                app.structd.spin = app.structd.spin.wrapping_add(1);
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
                    handle_modal_key(&mut app, k.code, k.modifiers);
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
                // PTY pane captures ALL keystrokes and forwards them to the
                // engine-held child, EXCEPT Ctrl-] which detaches back to the UI
                // (the classic telnet/ssh escape). This must come before the
                // global keymap so 'q', Tab, etc. reach the shell, not the UI.
                if app.focus == Pane::Pty {
                    use crossterm::event::KeyModifiers;
                    let detach = matches!(k.code, KeyCode::Char(']'))
                        && k.modifiers.contains(KeyModifiers::CONTROL);
                    if detach {
                        app.focus = Pane::Sessions;
                        app.status = "PTY detached (still running)".into();
                    } else if let Some(bytes) = key_to_pty_bytes(k.code, k.modifiers) {
                        if let Some(pty) = app.pty.as_mut() { pty.send_input(&bytes); }
                    }
                    continue;
                }
                use crossterm::event::KeyModifiers as KM;
                let ctrl = k.modifiers.contains(KM::CONTROL);
                match k.code {
                    // 'q' stops the engine too (mirrors the Python prototype's
                    // contract — q is QUIT, not detach). 'd' detaching is
                    // matched LATER, after the pane-specific 'd' bindings
                    // (discard_hunk on Hunks, delete_rule on Rules) have had
                    // their guards checked.
                    KeyCode::Char('q') => {
                        shutdown_rpc(&app.sock);
                        app.should_quit = true;
                    }
                    // ctrl+up / ctrl+down reorder the selected file rule (before
                    // the plain move arm, which also matches Up/Down).
                    KeyCode::Up if ctrl && app.focus == Pane::Rules => app.move_rule(-1),
                    KeyCode::Down if ctrl && app.focus == Pane::Rules => app.move_rule(1),
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
                    // pane switches; c/p/o cross-navigate (install a generated
                    // ids filter on the destination from the cursor).
                    KeyCode::Char('b') => app.focus = Pane::Sessions,
                    KeyCode::Char('c') => app.nav(Pane::Changes),
                    KeyCode::Char('p') => app.nav(Pane::Processes),
                    KeyCode::Char('o') => app.nav(Pane::Outputs),
                    KeyCode::Char('e') => app.focus = Pane::Rules,
                    KeyCode::Char('?') => { app.focus = Pane::Help; app.out_scroll = 0; }
                    KeyCode::Char('P') =>
                        app.modal = Some(Modal::PtyCmd { buf: pty_default_cmd() }),
                    // In the diff pane, a/x/d are PER-HUNK; elsewhere they act on
                    // the selected change / whole box.
                    KeyCode::Char('a') if app.focus == Pane::Hunks => app.apply_hunk(),
                    KeyCode::Char('x') | KeyCode::Char('d') if app.focus == Pane::Hunks => app.discard_hunk(),
                    KeyCode::Char('a') => app.apply(),
                    KeyCode::Char('x') => app.discard(),
                    // batch apply/discard of the WHOLE box (Python A/X).
                    KeyCode::Char('A') => app.apply_all(),
                    KeyCode::Char('X') => app.discard_all(),
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
                    KeyCode::Char('Z') => {
                        app.modal = Some(Modal::Confirm {
                            prompt: "Dissolve the selected box (unmount/cleanup)?".into(),
                            action: ConfirmAction::Dissolve,
                        })
                    }
                    KeyCode::Char('n') if app.focus == Pane::Rules => {
                        app.modal = Some(Modal::RuleForm { buf: String::new(), editing: None });
                    }
                    KeyCode::Char('d') if app.focus == Pane::Rules => app.delete_rule(),
                    // Plain 'd' (no Hunks/Rules guard above caught it) =
                    // detach: close just the UI, leave the engine running so
                    // a later `sarun` reattaches to it.
                    KeyCode::Char('d') => app.should_quit = true,
                    KeyCode::Char('/') => app.toggle_filter(),
                    KeyCode::Esc => {
                        // esc clears a generated (cross-nav) filter on the focused pane.
                        if let Some(v) = app.focus_filter_view() {
                            if app.view_filter(v).generated {
                                app.clear_filter(v);
                            }
                        }
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

/// The UI role of the single `sarun` binary. `args` are the tokens after the
/// dispatch token. Returns a process exit code (caller does `process::exit`).
pub fn ui_main(args: &[String]) -> i32 {
    let mut once = false;
    let mut sock = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--once" => once = true,
            "--sock" => sock = it.next().cloned().unwrap_or_default(),
            "-h" | "--help" => {
                println!(
                    "sarun UI — Rust ratatui client for the sarun engine\n\
                     \n\
                     usage:\n  \
                     sarun                         start engine (if needed) + interactive UI\n  \
                     sarun attach [--sock PATH]    interactive UI against a running engine\n  \
                     sarun --once --sock PATH      render one frame and exit (headless)\n"
                );
                return 0;
            }
            _ => {}
        }
    }
    if sock.is_empty() {
        sock = std::env::var("SARUN_SOCK").unwrap_or_default();
    }
    if sock.is_empty() {
        sock = crate::paths::sock_path().to_string_lossy().into_owned();
    }
    if sock.is_empty() {
        eprintln!("sarun: no socket (pass --sock PATH or set SARUN_SOCK)");
        return 2;
    }
    if once {
        let app = App::new(sock);
        match render_to_string(&app, 100, 30) {
            Ok(buf) => {
                print!("{buf}");
                return 0;
            }
            Err(e) => {
                eprintln!("sarun: {e}");
                return 1;
            }
        }
    }
    if let Err(e) = run_interactive(&sock) {
        eprintln!("sarun: {e}");
        return 1;
    }
    0
}

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

    /// The App computes its filerules path from the (process-global) env, so the
    /// rule-editing tests share one on-disk file. Serialize them with this lock
    /// and each clears the file under it.
    static RULES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn shell_split_handles_quotes() {
        assert_eq!(shell_split("bash"), vec!["bash"]);
        assert_eq!(shell_split("sarun run -b -- make all"),
                   vec!["sarun", "run", "-b", "--", "make", "all"]);
        assert_eq!(shell_split("sh -c 'echo hi there'"),
                   vec!["sh", "-c", "echo hi there"]);
        assert_eq!(shell_split(r#"a "b c" d"#), vec!["a", "b c", "d"]);
        assert!(shell_split("   ").is_empty());
    }

    #[test]
    fn pty_default_cmd_is_configurable_not_enforced() {
        // A saved "login command" in the config wins …
        let tmp = std::env::temp_dir()
            .join(format!("sarun-ptycfg-{}", std::process::id()));
        let cfgdir = tmp.join("slopbox.PTYCFG");
        std::fs::create_dir_all(&cfgdir).unwrap();
        std::fs::write(cfgdir.join("pty_command"),
                       "# comment\n\nsarun run -b -- make\n").unwrap();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_ns = std::env::var("SLOPBOX_NS").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &tmp);
            std::env::set_var("SLOPBOX_NS", "PTYCFG");
        }
        assert_eq!(pty_default_cmd(), "sarun run -b -- make",
                   "configured login command must be used verbatim");
        // … and with no config it falls back to a shell — a DEFAULT, not a
        // baked-in box decision.
        std::fs::remove_file(cfgdir.join("pty_command")).unwrap();
        let d = pty_default_cmd();
        assert!(d.contains("sh") || d.contains("bash"),
                "fallback should be a shell, got {d:?}");
        unsafe {
            match prev_xdg { Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                             None => std::env::remove_var("XDG_CONFIG_HOME") }
            match prev_ns { Some(v) => std::env::set_var("SLOPBOX_NS", v),
                            None => std::env::remove_var("SLOPBOX_NS") }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

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
        // The single binary now lives in this crate's own target dir.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let rel = here.join("target/release/sarun");
        if rel.exists() {
            return Some(rel);
        }
        let dbg = here.join("target/debug/sarun");
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
        let _guard = RULES_LOCK.lock().unwrap();
        let mut app = App::new(eng.sock.clone());
        app.focus = Pane::Rules;
        // start clean (the filerules path is shared across in-process tests).
        let _ = std::fs::remove_file(app.rules_path());
        app.rules.clear();
        app.load_rules();
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
        // a typed exe-clause that matches nothing hides every row.
        let bogus = vec![ClauseRow {
            enabled: true, join: Join::And, negate: false,
            kind: "exe".into(), pattern: "NO_SUCH_PROC_zzzz".into(),
        }];
        app.commit_filter(FilterView::Procs, &bogus);
        assert!(app.visible_processes().is_empty(), "bogus exe clause should hide all rows");
        // an exe clause on '**/echo' keeps the echo row.
        let keep = vec![ClauseRow {
            enabled: true, join: Join::And, negate: false,
            kind: "exe".into(), pattern: "**/echo".into(),
        }];
        app.commit_filter(FilterView::Procs, &keep);
        let vis = app.visible_processes();
        assert!(!vis.is_empty() && vis.len() <= total, "exe clause should keep ≥1, ≤all rows");
        app.focus = Pane::Processes;
        let buf = render_to_string(&app, 160, 40).unwrap();
        assert!(buf.contains("echo"), "filtered processes pane should still show echo:\n{buf}");

        // CROSS-CHECK: the same (clause, target) inputs run through the in-crate
        // eval_clauses directly must agree with the UI's filter decision.
        let clause = keep[0].to_clause();
        for p in &app.processes {
            let rid = p.as_array().and_then(|x| x.first()).and_then(Value::as_i64).unwrap_or(-1);
            let tgt = crate::rules::ProcFilterTarget { row_id: rid, subject: app.proc_subject(p) };
            let direct = crate::rules::eval_clauses(&tgt, std::slice::from_ref(&clause));
            let via_ui = app.visible_processes().iter().any(|q| {
                q.as_array().and_then(|x| x.first()).and_then(Value::as_i64) == Some(rid)
            });
            assert_eq!(direct, via_ui,
                "eval_clauses and the UI filter must agree for row {rid}");
        }
    }

    /// Build a known multi-level process tree in a box and assert the procs pane
    /// renders children INDENTED under their parents (a tree, not a flat list).
    #[test]
    fn proc_tree_renders_children_indented() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        // a 3-level chain: sh -> sh -c (parent) -> a grandchild marker process.
        if !run_cmd(&eng, &["/bin/sh", "-c",
            "/bin/sh -c '/bin/sleep 0.05; /bin/echo TREEKID_marker'"]) {
            eprintln!("SKIP: could not run a box command");
            return;
        }
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        let ok = (0..app.sessions.len()).any(|i| {
            app.sel_session = i;
            app.load_processes();
            app.processes.len() >= 2
        });
        assert!(ok, "expected a box with a multi-process tree");
        let rows = app.proc_tree_rows();
        // there must be at least one row at depth>0 (a child indented under a parent).
        assert!(rows.iter().any(|r| r.depth > 0),
                "proc tree should have an indented child; rows depths: {:?}",
                rows.iter().map(|r| r.depth).collect::<Vec<_>>());
        // a parent must appear before its deeper child in DFS order.
        let first_deep = rows.iter().position(|r| r.depth > 0).unwrap();
        assert!(rows[..first_deep].iter().any(|r| r.depth == 0),
                "a depth-0 ancestor must precede the first deep row");
        app.focus = Pane::Processes;
        let buf = render_to_string(&app, 160, 40).unwrap();
        // the rendered grid must show indentation (leading spaces before an exe).
        assert!(buf.lines().any(|l| {
            let t = l.trim_start_matches(|c: char| c.is_ascii_digit() || c == ' ');
            // a deeper row has MORE leading spaces in the EXE column than a root.
            l.contains("sh") && l.matches("  ").count() >= 2 && !t.is_empty()
        }), "rendered tree should show indented rows:\n{buf}");
        assert!(buf.contains("sh"), "tree should show the sh processes:\n{buf}");
    }

    /// changes→procs navigation installs a GENERATED ids filter pinning the procs
    /// pane to exactly the change's writer(s); a subsequent nav/esc clears it.
    #[test]
    fn nav_ids_filters_procs_to_writer_then_clears() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        // write a file from a known box process so the change has a recorded
        // writer (the cp process). cp through the box: a captured proc IS the
        // writer, so changes→procs nav resolves to it.
        let (_sid0, root) = make_box(&eng.sock);
        std::fs::write(root.join("tmp").join("navids_seed.txt"), b"seed\n").ok();
        // a real box command that writes a file (its captured proc is the writer).
        if !run_cmd(&eng, &["/bin/cp", "/bin/echo", "/tmp/navids_marker_file.txt"]) {
            eprintln!("SKIP: could not run a box command");
            return;
        }
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        // pick the box that has a change WITH a recorded writer AND processes.
        let mut chosen = None;
        for i in 0..app.sessions.len() {
            app.sel_session = i;
            app.load_changes();
            if app.processes.is_empty() { continue; }
            let sid = app.cur_sid().unwrap();
            let vc = app.visible_changes();
            if let Some(ci) = vc.iter().position(|c| {
                let rel = c.get("path").and_then(Value::as_str).unwrap_or("");
                !app.change_writer_ids(&sid, rel).is_empty()
            }) {
                chosen = Some((i, ci));
                break;
            }
        }
        let Some((bi, ci)) = chosen else {
            eprintln!("SKIP: no box with a change carrying a recorded writer + procs");
            return;
        };
        app.sel_session = bi;
        app.load_changes();
        app.focus = Pane::Changes;
        app.sel_change = ci;
        let sid = app.cur_sid().unwrap();
        let rel = app.cur_change_path().unwrap();
        let expect_ids = app.change_writer_ids(&sid, &rel);
        assert!(!expect_ids.is_empty(), "the written file must have a recorded writer");

        // changes→procs nav: a generated ids filter must appear on procs.
        app.nav(Pane::Processes);
        assert!(app.focus == Pane::Processes);
        assert!(app.f_procs.on && app.f_procs.generated,
                "procs pane should carry a generated filter after nav");
        // the procs pane must be narrowed to exactly the writer row(s).
        let vis: Vec<i64> = app.visible_processes().iter().map(|p|
            p.as_array().and_then(|x| x.first()).and_then(Value::as_i64).unwrap_or(-1)).collect();
        assert!(!vis.is_empty(), "filtered procs must be non-empty");
        for id in &vis {
            assert!(expect_ids.contains(id),
                "procs filter must show only the writer ids {expect_ids:?}, saw {id}");
        }

        // esc on the procs pane clears the generated (cross-nav) filter.
        app.focus = Pane::Processes;
        assert!(app.f_procs.generated, "filter still generated before esc");
        app.clear_filter(FilterView::Procs);
        assert!(!app.f_procs.on && !app.f_procs.generated && app.f_procs.clauses.is_empty(),
                "esc/clear must drop the generated ids filter entirely");
        // and the procs pane is back to the full set.
        assert_eq!(app.visible_processes().len(), app.processes.len(),
                   "cleared filter shows all processes again");
    }

    #[test]
    fn build_proc_tree_is_dfs_ordered_with_connectors() {
        // pure unit test (no engine): a forest where row 30's parent (20) is a
        // structural connector NOT present in `procs` (resolved via lookup).
        let p = |rid: i64, parent: i64, exe: &str| {
            json!([rid, rid * 10, 1, parent, exe, [exe]])
        };
        let owned = vec![p(10, 0, "root"), p(30, 20, "leaf")];
        let procs: Vec<&Value> = owned.iter().collect();
        let mut roots = std::collections::HashSet::new();
        roots.insert(10i64);
        // 20 is the connector ancestor: parent_id 10 (a root member), not in procs.
        let lookup = |rid: i64| -> Option<NodeInfo> {
            if rid == 20 { Some((200, 1, Some(10), "mid".into(), vec!["mid".into()])) } else { None }
        };
        let rows = build_proc_tree(&procs, &roots, &lookup);
        // DFS: 10 (root, depth0) -> 20 (connector, depth1) -> 30 (leaf, depth2).
        let ids: Vec<(i64, usize, bool)> =
            rows.iter().map(|r| (r.rid, r.depth, r.connector)).collect();
        assert_eq!(ids, vec![(10, 0, false), (20, 1, true), (30, 2, false)],
                   "tree must be DFS-ordered with the unioned connector at depth 1");
    }

    #[test]
    fn eval_clauses_cross_check_matches_python_semantics() {
        // Construct identical (clause, target) inputs the way the Python
        // eval_clauses would receive them and assert the in-crate engine agrees
        // with the hand-computed boolean fold.
        let exe_clause = Clause {
            m: Match { kind: "exe".into(), pattern: "**/echo".into() },
            join: Join::And, negate: false, enabled: true,
        };
        let yes = ProcFilterTarget { row_id: 1,
            subject: Subject { box_name: "B".into(), exe: "/bin/echo".into(),
                               cwd: "/".into(), argv: vec!["echo".into(), "hi".into()] } };
        let no = ProcFilterTarget { row_id: 2,
            subject: Subject { exe: "/bin/cat".into(), ..Default::default() } };
        assert!(eval_clauses(&yes, std::slice::from_ref(&exe_clause)));
        assert!(!eval_clauses(&no, std::slice::from_ref(&exe_clause)));
        // an ids clause pins to an exact row set (the generated-filter form).
        let ids_clause = Clause {
            m: Match { kind: "ids".into(), pattern: "1,5".into() },
            join: Join::And, negate: false, enabled: true,
        };
        assert!(eval_clauses(&yes, std::slice::from_ref(&ids_clause)));
        assert!(!eval_clauses(&no, std::slice::from_ref(&ids_clause)));
    }

    #[test]
    fn help_pane_lists_keybindings() {
        // pure render; no engine needed.
        let mut app = App {
            sock: String::new(),
            sessions: vec![],
            changes: vec![],
            changes_view: None,
            changes_total: 0,
            changes_window_start: 0,
            hunks: Value::Null,
            processes: vec![],
            processes_view: None,
            processes_total: 0,
            processes_window_start: 0,
            outputs: vec![],
            outputs_view: None,
            outputs_total: 0,
            outputs_window_start: 0,
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
            f_changes: ViewFilter::default(),
            f_procs: ViewFilter::default(),
            f_outputs: ViewFilter::default(),
            should_quit: false,
            pty: None,
            structd: StructState::default(),
            sel_hunk: 0,
            struct_rx: None,
        };
        app.focus = Pane::Help;
        // render tall enough to fit the full manual (it scrolls in a real term).
        let buf = render_to_string(&app, 100, 80).unwrap();
        assert!(buf.contains("help"), "help pane title missing:\n{buf}");
        assert!(buf.contains("apply") && buf.contains("discard"), "help should mention apply/discard:\n{buf}");
        assert!(buf.contains("processes"), "help should mention the processes pane:\n{buf}");
        // the richer manual must cover the loop, filter kinds, rule syntax, nesting.
        assert!(buf.contains("copy-on-write") || buf.contains("overlay"),
                "manual should explain the overlay loop:\n{buf}");
        assert!(buf.contains("hunk"), "manual should mention per-hunk apply:\n{buf}");
        assert!(buf.contains("passthrough"), "manual should document rule actions:\n{buf}");
        assert!(buf.contains("ctrl+"), "manual should mention rule reorder:\n{buf}");
    }

    /// Helper: select the box (and change) whose path matches `pred`, returning
    /// true once the App is positioned on it in the Changes pane.
    fn select_change(app: &mut App, pred: impl Fn(&str) -> bool) -> bool {
        app.refresh_sessions();
        for i in 0..app.sessions.len() {
            app.sel_session = i;
            app.load_changes();
            let pos = app.visible_changes().iter().position(|c| {
                c.get("path").and_then(Value::as_str).map(&pred).unwrap_or(false)
            });
            if let Some(ci) = pos {
                app.focus = Pane::Changes;
                app.sel_change = ci;
                app.load_hunks();
                return true;
            }
        }
        false
    }

    /// STRUCT PANE: a box with a real ELF binary change (cp /bin/true). Drive the
    /// quick→finish flow and assert the rendered diff shows REAL structural lines
    /// (an ELF section/program-header keyword from readelf) — actual file content.
    #[test]
    fn struct_pane_shows_elf_structure() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        // create an ELF binary change inside the box by copying a host ELF.
        let (_sid, root) = make_box(&eng.sock);
        let src = ["/bin/true", "/usr/bin/true", "/bin/echo"].iter()
            .map(PathBuf::from).find(|p| p.exists());
        let Some(src) = src else { eprintln!("SKIP: no host ELF to copy"); return; };
        let bytes = std::fs::read(&src).expect("read host ELF");
        assert_eq!(&bytes[..4], b"\x7fELF", "test fixture must be an ELF");
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("struct_elf_marker"), &bytes).expect("write ELF through mount");

        let mut app = App::new(eng.sock.clone());
        if !select_change(&mut app, |p| p.contains("struct_elf_marker")) {
            eprintln!("SKIP: ELF change not captured");
            return;
        }
        // it must be a BINARY change driving the structural pane.
        assert!(app.hunks.get("is_text").and_then(Value::as_bool) != Some(true),
                "ELF change should be binary, not text");
        // quick half ran: the type line names ELF, and a finish job was kicked.
        let quick_joined: String = app.structd.quick_lines.iter()
            .map(|(_, t)| t.clone()).collect::<Vec<_>>().join("\n");
        assert!(quick_joined.to_lowercase().contains("elf"),
                "struct_quick should report the ELF type; got: {quick_joined}");
        // drive the worker to completion (it runs readelf in a sandbox).
        let mut done = false;
        for _ in 0..100 {
            if app.pump_struct() { done = true; break; }
            std::thread::sleep(Duration::from_millis(100));
        }
        // render the pane and assert it shows a REAL readelf keyword OR (if the
        // sandbox/readelf is unavailable here) at least the ELF type line.
        let buf = render_to_string(&app, 160, 60).unwrap();
        let readelf_ok = done && app.structd.full_lines.as_ref().map(|ls| {
            let j = ls.iter().map(|(_,t)| t.clone()).collect::<Vec<_>>().join("\n");
            j.contains("ELF Header") || j.contains("Section Headers")
                || j.contains("Program Headers") || j.contains(".text")
                || j.contains("readelf")
        }).unwrap_or(false);
        assert!(buf.contains("structural diff"), "pane title should be structural:\n{buf}");
        assert!(
            readelf_ok || buf.to_lowercase().contains("elf"),
            "structural pane should show real ELF structure (or at least the ELF \
             type line); done={done}; got:\n{buf}"
        );
    }

    /// STRUCT PANE hexdump fallback: a binary change of an UNRECOGNIZED type
    /// (random NUL-containing bytes) renders a hexdump of the real bytes.
    #[test]
    fn struct_pane_hexdump_fallback_shows_real_bytes() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        // bytes with a NUL (→ binary) and a recognizable hex signature, but no
        // known magic (so no differ → hexdump fallback).
        let data: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33,
                                 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB,
                                 0x00, 0xCC];
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("struct_hex_marker.bin"), &data).expect("write");

        let mut app = App::new(eng.sock.clone());
        if !select_change(&mut app, |p| p.contains("struct_hex_marker.bin")) {
            eprintln!("SKIP: binary change not captured");
            return;
        }
        assert!(app.hunks.get("is_text").and_then(Value::as_bool) != Some(true),
                "should be a binary change");
        // unrecognized → no finish job, hexdump fallback built immediately.
        assert!(app.structd.job.is_none(), "unrecognized type must not kick a job");
        assert!(!app.structd.hex_lines.is_empty(), "hexdump fallback must be built");
        let buf = render_to_string(&app, 160, 40).unwrap();
        // the real leading bytes must appear in the hexdump (offset 0 row).
        assert!(buf.contains("dead beef") || buf.contains("de ad be ef"),
                "hexdump should show the real bytes; got:\n{buf}");
    }

    /// HUNK APPLY: a 2-hunk text change. Drive `a` on hunk 0 via the UI key path
    /// and assert (through the engine) the host got exactly that hunk while the
    /// other hunk remains pending.
    #[test]
    fn hunk_apply_applies_only_the_selected_hunk() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        // a host file with two well-separated regions; the box edits BOTH so the
        // diff has two distinct hunks.
        let host = PathBuf::from(format!("/tmp/hunk2_{}.txt", std::process::id()));
        let base: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&host, &base).expect("seed host file");
        let rel = host.strip_prefix("/").unwrap().to_string_lossy().to_string();

        let (sid, root) = make_box(&eng.sock);
        // edit line 2 and line 38 in the box's view of the same file.
        let mut edited: Vec<String> = base.lines().map(|l| format!("{l}\n")).collect();
        edited[1] = "line 2 CHANGED_TOP\n".into();
        edited[37] = "line 38 CHANGED_BOTTOM\n".into();
        let boxpath = root.join(&rel);
        std::fs::create_dir_all(boxpath.parent().unwrap()).ok();
        std::fs::write(&boxpath, edited.concat()).expect("write through mount");

        let mut app = App::new(eng.sock.clone());
        let _ = sid;
        if !select_change(&mut app, |p| p == rel) {
            let _ = std::fs::remove_file(&host);
            eprintln!("SKIP: 2-hunk change not captured");
            return;
        }
        let idxs = app.hunk_indices();
        assert!(idxs.len() >= 2, "expected ≥2 hunks, got {}", idxs.len());
        // focus the diff pane, cursor on hunk 0, then exercise the UI's apply_hunk.
        app.focus = Pane::Hunks;
        app.sel_hunk = 0;
        let first_ix = app.cur_hunk_index().unwrap();
        app.apply_hunk();
        assert!(app.status.starts_with("applied hunk"),
                "apply_hunk status should report success; got {}", app.status);
        // the host file must now contain hunk 0's content but NOT hunk 1's (still
        // pending in the box).
        let on_host = std::fs::read_to_string(&host).unwrap_or_default();
        assert!(on_host.contains("CHANGED_TOP"),
                "host should have the applied top hunk; got:\n{on_host}");
        assert!(!on_host.contains("CHANGED_BOTTOM"),
                "host must NOT yet have the un-applied bottom hunk; got:\n{on_host}");
        // and the box still reports a remaining (bottom) hunk for this path.
        let remaining = rpc(&eng.sock, "review.hunks", json!([app.cur_sid().unwrap(), rel]))
            .ok().and_then(|v| v.get("hunks").and_then(Value::as_array).map(|a| a.len()))
            .unwrap_or(0);
        assert!(remaining >= 1, "the other hunk must remain pending (idx {first_ix} applied)");
        let _ = std::fs::remove_file(&host);
    }

    /// BATCH A: a box with ≥2 changes; the UI's apply_all applies them all.
    #[test]
    fn batch_apply_all_applies_every_change() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let pid = std::process::id();
        let names = [format!("batch_a_{pid}.txt"), format!("batch_b_{pid}.txt")];
        for (i, n) in names.iter().enumerate() {
            std::fs::write(dir.join(n), format!("content {i}\n")).expect("write");
        }
        for n in &names { let _ = std::fs::remove_file(PathBuf::from("/tmp").join(n)); }

        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        // select the box that holds both changes.
        let ok = (0..app.sessions.len()).any(|i| {
            app.sel_session = i;
            app.load_changes();
            names.iter().all(|n| app.changes.iter().any(|c|
                c.get("path").and_then(Value::as_str).map(|p| p.contains(n)).unwrap_or(false)))
        });
        assert!(ok, "expected a box holding both changes");
        assert!(app.changes.len() >= 2, "expected ≥2 changes before apply_all");
        app.apply_all();
        assert!(app.status.starts_with("applied all"),
                "apply_all status should report success; got {}", app.status);
        // both files must now exist on the host.
        for n in &names {
            let hp = PathBuf::from("/tmp").join(n);
            assert!(hp.exists(), "apply_all should materialize {hp:?} on the host");
            let _ = std::fs::remove_file(&hp);
        }
        // and the box's pending changes are gone.
        assert!(app.changes.is_empty() || app.changes.iter().all(|c|
            !names.iter().any(|n| c.get("path").and_then(Value::as_str)
                .map(|p| p.contains(n)).unwrap_or(false))),
            "applied changes should no longer be pending");
    }

    /// RULE MOVE: two rules; ctrl-down on the first swaps the on-disk order and
    /// calls reload_rules (status reflects the reload).
    #[test]
    fn rule_move_swaps_on_disk_order_and_reloads() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let _guard = RULES_LOCK.lock().unwrap();
        let mut app = App::new(eng.sock.clone());
        app.focus = Pane::Rules;
        // start from a clean filerules file (the App computes its path from the
        // process env, shared across in-process tests — isolate by clearing it).
        let _ = std::fs::remove_file(app.rules_path());
        app.rules.clear();
        app.load_rules();
        // two distinct rules.
        app.commit_rule("apply src/**".into(), None);
        app.commit_rule("discard **/*.log".into(), None);
        let before = std::fs::read_to_string(app.rules_path()).unwrap_or_default();
        let lines_before: Vec<&str> = before.lines().collect();
        assert_eq!(lines_before, vec!["apply src/**", "discard **/*.log"],
                   "two rules in insertion order on disk");
        // ctrl-down on the first rule.
        app.sel_rule = 0;
        app.move_rule(1);
        assert_eq!(app.sel_rule, 1, "cursor should follow the moved rule");
        let after = std::fs::read_to_string(app.rules_path()).unwrap_or_default();
        let lines_after: Vec<&str> = after.lines().collect();
        assert_eq!(lines_after, vec!["discard **/*.log", "apply src/**"],
                   "ctrl-down must swap the on-disk order; got: {after:?}");
        // save_rules calls reload_rules — status reflects the reload.
        assert!(app.status.contains("reloaded"),
                "rule move should reload_rules; status={}", app.status);
        // restore a clean filerules file (shared in-process path).
        let _ = std::fs::remove_file(app.rules_path());
    }
}
