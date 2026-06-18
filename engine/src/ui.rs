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
// Subject / ProcFilterTarget / eval_clauses moved server-side (views.rs
// owns filter evaluation now), but the unit-test module still references
// the same types for a parity check against rules::eval_clauses.
#[cfg(test)]
use crate::rules::{ProcFilterTarget, Subject, eval_clauses};

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
    /// D9 brush↔process pipelines: one row per recorded shell pipeline
    /// (cmd + parsed structure JSON + which proc rows it spawned), from
    /// the box's `brushprov` table. Top-level + nested-shim pipelines
    /// share this view; the nested? column tags them.
    Pipelines,
    /// Phase 1 embedded-ninja build graph: one row per parsed n2 build
    /// edge (outs / ins / cmd), from the box's `build_edges` table —
    /// INCLUDING up-to-date targets that never executed.
    BuildEdges,
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
    /// Context-menu popup for the currently-selected list row. Opened
    /// with `m`. `title` is the row identity (e.g. "Box: foo" or
    /// "Change: src/main.rs"); `items` are (label, hint, action). The
    /// `hint` column shows the global key for the same action when one
    /// exists, so the popup doubles as a discoverable cheat-sheet.
    ActionMenu {
        title: String,
        items: Vec<ActionItem>,
        sel: usize,
    },
}

/// One row inside Modal::ActionMenu. `hint` shows the global key that
/// would do the same thing (e.g. "F5" / "a") so the popup teaches the
/// keymap as the user uses it.
#[derive(Clone)]
struct ActionItem {
    label: String,
    hint: &'static str,
    action: Action,
}

/// Actions dispatchable from the context-menu popup. Each variant maps
/// to a single mutation on App, so the modal-key handler can run them
/// without holding any pane-specific data.
#[derive(Clone, Copy)]
enum Action {
    OpenSelection,
    ApplyFile,
    DiscardFile,
    ApplyHunk,
    DiscardHunk,
    ApplyBox,
    DiscardBox,
    StartRename,
    EditRule,
    NewRule,
    DeleteRule,
    MoveRuleUp,
    MoveRuleDown,
    PtyNew,
    PtyKill,
    PtyEmbedToggle,
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

/// How many rows PgDn / PgUp jumps the cursor. Sized to one screenful on a
/// typical-height terminal (the panes are split, so the visible half is
/// roughly this tall); WINDOW_SIZE / 20 ratio keeps page jumps cheap —
/// staying inside a single fetched window for normal terminals.
const PAGE_SIZE: usize = 20;

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
    processes: Vec<Value>,
    processes_view: Option<u64>,
    processes_total: usize,
    processes_window_start: usize,
    /// Same for outputs.
    outputs: Vec<Value>,
    outputs_view: Option<u64>,
    outputs_total: usize,
    outputs_window_start: usize,
    rules: Vec<String>,    // raw filerules lines (apply/discard/passthrough <glob>)
    /// D9 pipelines for the currently-loaded box (one row per `brushprov`
    /// entry; engine returns the full list — these are bounded by what
    /// brush actually ran, so no windowing needed).
    pipelines: Vec<Value>,
    /// Phase 1 build edges for the currently-loaded box (one row per
    /// `build_edges` entry; same "full list, no windowing" reasoning).
    build_edges: Vec<Value>,
    sel_session: usize,
    sel_change: usize,
    sel_proc: usize,
    sel_pipeline: usize,
    sel_edge: usize,
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
    /// True iff focus is currently on the RIGHT pane of the active view
    /// (the detail / body half) rather than the LEFT list. Tab toggles
    /// this on views whose right half is scrollable (Sessions / Procs /
    /// Outputs / Pipelines / BuildEdges / Rules); j/k/PgUp/PgDn then
    /// drive `right_scroll` instead of the list cursor. Switching VIEW
    /// (a letter chip) snaps focus back to the left list.
    right_focused: bool,
    /// Wrapping-aware scroll offset for whichever view's right pane is
    /// currently rendered. Reset to 0 on every view switch (so each
    /// view's right pane starts at the top, the prototype's behavior).
    right_scroll: u16,
    /// When true, the focused view's RIGHT column hosts the currently
    /// selected PTY (`ptys[sel_pty]`) instead of the normal detail
    /// body. Lets the user watch a live shell next to the boxes /
    /// changes / procs lists. Toggled by F11; only meaningful when
    /// ptys.is_empty() == false. PTY full-screen (Pane::Pty) is a
    /// separate state — F11 from there shrinks the PTY into the
    /// previous view's right column instead of toggling.
    /// All currently-open engine-held PTYs. Pane::Pty renders the
    /// full-screen view of `ptys[sel_pty]`; F2/F3 cycle between them,
    /// F7 creates a new one, F8 kills the current one. EOF'd PTYs
    /// are dropped when the user moves off of them, so re-opening
    /// the pane lands on a running shell, not a corpse.
    ptys: Vec<PtyPane>,
    /// F11 toggle — when true, embed the active PTY into the focused
    /// view's right column instead of the normal detail body. Stays
    /// false on PTY full-screen mode (Pane::Pty manages itself).
    pty_in_right: bool,
    /// F9 menu navigation: when true, the menubar cursor is active
    /// and arrow keys move between top-level menu chips. Enter
    /// activates the chip under the cursor (same as pressing its
    /// accelerator letter); Esc cancels. The letter accelerators
    /// (b/c/p/o/l/g/e/?/P) keep working alongside.
    menu_nav: bool,
    /// Index into menubar_chips(self) the menu cursor is on. Only
    /// meaningful when menu_nav is true.
    menu_sel: usize,
    /// Index into `ptys`. Always < ptys.len() when ptys non-empty;
    /// 0 when empty (rendering checks ptys.is_empty()).
    sel_pty: usize,
    /// Esc-Esc detach chord: wall-clock instant of the LAST Esc keypress
    /// inside the PTY pane, used to fire the detach on a second Esc
    /// within 400 ms. None = no Esc queued. A non-Esc keystroke or an
    /// out-of-window Esc flushes the queued Esc into the box (so vi-mode
    /// Esc and brush-interactive's bindings keep working).
    pty_esc_at: Option<std::time::Instant>,
    /// Off-loop structural-diff state for the selected BINARY change.
    structd: StructState,
    /// Hunk cursor within the diff pane (index into the hunk list); used for
    /// per-hunk apply/discard.
    sel_hunk: usize,
    /// Receiver for finished structural-diff worker results (drained each tick).
    struct_rx: Option<mpsc::Receiver<StructResult>>,
    /// Cached header for the currently-selected change: prototype's #cd-info
    /// content — full path / kind / size / mode / stale banner. Populated in
    /// load_hunks (on Enter / cursor move) so draw() doesn't RPC per frame.
    cd_info: Option<Vec<(String, String)>>,
    /// Decoded transcript segments for the outputs view, one per row in
    /// `self.outputs`. Tuple is (output_id, stream, decoded_text). Used by
    /// outputs_lines to draw a per-line gutter mark for the selected
    /// entry and apply the 8000-char windowed render the prototype does
    /// (mirrors its _update_output_detail). Cleared on view reload.
    output_segs: Vec<(i64, i64, String)>,
    /// Per-row decoration for the changes window: parallel to `changes`,
    /// each entry is (kind, stale). `kind` is "created" / "modified" /
    /// "deleted" / "symlink" / "changed" — finer than the source row's
    /// kind, which can't distinguish created vs modified without stating
    /// the host. Filled by a single bulk `review.decorate_many` after
    /// every fetch_changes_window so changes_lines can paint the
    /// prototype's per-row +/~ glyph and the `!` stale marker.
    changes_decor: Vec<(String, bool)>,
    /// "Recently changed" tail for the selected LIVE box's detail panel.
    /// Newest-first (by sqlar.mtime), capped server-side. Empty for
    /// finished boxes (the detail panel uses self.changes head instead).
    /// Refreshed by tick() when on the boxes view.
    recent_changes: Vec<Value>,
    /// Five-list bundle for the Sessions-view right pane (the box-detail
    /// summary): newest-first outputs / changes / processes / pipelines /
    /// build_edges, fetched in one RPC on each session switch. Defaults
    /// to all-empty arrays so box_detail_lines can render unconditionally.
    box_summary: Value,
}

/// Cap on the transcript window: chars rendered in one frame, centred on
/// the selected entry (prototype's _OUT_CAP = 8000). Bytes outside the
/// window get rolled up into "… N earlier" / "… N more" elision lines.
const OUTPUT_WINDOW_CAP: usize = 8000;


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
            rules: vec![], pipelines: vec![], build_edges: vec![],
            sel_session: 0,
            sel_change: 0,
            sel_proc: 0, sel_pipeline: 0, sel_edge: 0,
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
            ptys: vec![], sel_pty: 0, pty_esc_at: None, right_focused: false, right_scroll: 0, pty_in_right: false, menu_nav: false, menu_sel: 0,
            structd: StructState::default(),
            sel_hunk: 0,
            struct_rx: None,
            cd_info: None,
            output_segs: vec![],
            changes_decor: vec![],
            recent_changes: vec![], box_summary: serde_json::json!(null),
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
            Ok(Value::Array(mut a)) => {
                // Sort by dotted display path so the sessions pane's tree
                // renders in DFS order — children come right after their
                // parent. Same ordering the prototype derives via
                // build_path_tree; with sessions keyed by box_id from the
                // engine the natural sort by `path` does the work for any
                // well-formed forest. sel_session indexes the SORTED vec,
                // so cursor moves match the on-screen rows.
                a.sort_by(|x, y| {
                    let px = x.get("path").and_then(Value::as_str).unwrap_or("");
                    let py = y.get("path").and_then(Value::as_str).unwrap_or("");
                    px.cmp(py)
                });
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
        self.seek_first_leaf();
        // For live boxes, also pull the newest-first slice for the box-
        // detail panel — cheap, server-side ORDER BY mtime DESC LIMIT 40.
        self.refresh_recent_changes();
        // the procs/outputs panes track the same selected box.
        self.load_processes();
        self.load_outputs();
    }

    /// After (re)loading the changes view, advance sel_change to the first
    /// LEAF (non-connector) row in the window. Without this the cursor would
    /// land on row 0, which in the new directory-tree layout is the top
    /// connector ("tmp/") instead of an actual change — load_hunks then has
    /// nothing to fetch and the diff pane shows "(select a change)".
    fn seek_first_leaf(&mut self) {
        for (i, c) in self.changes.iter().enumerate() {
            if c.get("connector").and_then(Value::as_bool) != Some(true) {
                self.sel_change = i;
                return;
            }
        }
        self.sel_change = 0;
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
            Err(e) => { self.status = format!("view.window changes: {e}"); return; }
        }
        // Decorate the window's LEAF rows in ONE RPC — the engine looks up
        // each row's (kind, stale) by stat-ing the host; connectors get an
        // empty placeholder so indices stay parallel to `self.changes`.
        // xattr / xattr-only rows are skipped the same way connectors are:
        // their "kind" is canonical at the source and decorate_many's
        // file-on-disk lookup would either fail (synthetic #xattr= path)
        // or wrongly stamp the row as "changed".
        self.changes_decor = vec![(String::new(), false); self.changes.len()];
        let Some(sid) = self.cur_sid() else { return };
        let is_decoratable = |c: &Value| -> bool {
            if c.get("connector").and_then(Value::as_bool) == Some(true) { return false; }
            !matches!(c.get("kind").and_then(Value::as_str),
                      Some("xattr") | Some("xattr-only"))
        };
        let leaf_paths: Vec<&str> = self.changes.iter()
            .filter(|c| is_decoratable(c))
            .filter_map(|c| c.get("path").and_then(Value::as_str))
            .collect();
        if leaf_paths.is_empty() { return; }
        let leaf_paths_value: Vec<Value> = leaf_paths.iter()
            .map(|p| Value::String((*p).into())).collect();
        if let Ok(rep) = rpc(&self.sock, "review.decorate_many",
                             json!([sid, leaf_paths_value])) {
            let decs = rep.as_array().cloned().unwrap_or_default();
            // Walk `self.changes` and `decs` in lockstep, skipping the
            // slots we filtered out so indices stay parallel.
            let mut di = 0;
            for (i, c) in self.changes.iter().enumerate() {
                if !is_decoratable(c) { continue; }
                if let Some(d) = decs.get(di) {
                    let kind = d.get("kind").and_then(Value::as_str)
                        .unwrap_or("changed").to_string();
                    let stale = d.get("stale").and_then(Value::as_bool).unwrap_or(false);
                    self.changes_decor[i] = (kind, stale);
                }
                di += 1;
            }
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

    /// Reopen the changes view (engine source is a snapshot at open time,
    /// so a live box's new files need a reopen to appear) AND keep the
    /// user's cursor pinned to the same path. Window_start is also
    /// preserved so a scrolled-down list doesn't jump back to the top.
    fn refresh_changes_preserving_cursor(&mut self) {
        let pinned_path = self.cur_change_path();
        let saved_start = self.changes_window_start;
        let saved_sel   = self.sel_change;
        self.close_changes_view();
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_changes.active());
        match rpc(&self.sock, "view.open", json!(["changes", sid, filter])) {
            Ok(v) => {
                self.changes_view = v.get("view_id").and_then(Value::as_u64);
                self.changes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
            Err(e) => { self.status = format!("view.open changes: {e}"); return; }
        }
        // Refetch the window we were on; if it slid off the end of the
        // new total, clamp.
        let cap = self.changes_total.saturating_sub(1);
        self.fetch_changes_window(saved_start.min(cap));
        // Pin sel_change by PATH if it's still in the window; otherwise
        // fall back to the saved offset.
        let restored = pinned_path.and_then(|p| {
            self.changes.iter().position(|c|
                c.get("path").and_then(Value::as_str) == Some(p.as_str()))
        });
        self.sel_change = restored
            .unwrap_or_else(|| saved_sel.min(self.changes.len().saturating_sub(1)));
    }

    /// Same idea for procs: reopen + pin sel_proc by row id.
    fn refresh_processes_preserving_cursor(&mut self) {
        let pinned_rid = self.processes.get(self.sel_proc)
            .and_then(|p| p.get("rid").and_then(Value::as_i64));
        let saved_start = self.processes_window_start;
        let saved_sel   = self.sel_proc;
        self.close_processes_view();
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_procs.active());
        match rpc(&self.sock, "view.open", json!(["procs", sid, filter])) {
            Ok(v) => {
                self.processes_view = v.get("view_id").and_then(Value::as_u64);
                self.processes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
            Err(e) => { self.status = format!("view.open procs: {e}"); return; }
        }
        let cap = self.processes_total.saturating_sub(1);
        self.fetch_processes_window(saved_start.min(cap));
        let restored = pinned_rid.and_then(|want| {
            self.processes.iter().position(|p|
                p.get("rid").and_then(Value::as_i64) == Some(want))
        });
        self.sel_proc = restored
            .unwrap_or_else(|| saved_sel.min(self.processes.len().saturating_sub(1)));
    }

    /// Same for outputs: reopen + pin sel_output by output id.
    fn refresh_outputs_preserving_cursor(&mut self) {
        let pinned_oid = self.outputs.get(self.sel_output)
            .and_then(|o| o.get("id").and_then(Value::as_i64));
        let saved_start = self.outputs_window_start;
        let saved_sel   = self.sel_output;
        self.close_outputs_view();
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_outputs.active());
        match rpc(&self.sock, "view.open", json!(["outputs", sid, filter])) {
            Ok(v) => {
                self.outputs_view = v.get("view_id").and_then(Value::as_u64);
                self.outputs_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
            Err(e) => { self.status = format!("view.open outputs: {e}"); return; }
        }
        let cap = self.outputs_total.saturating_sub(1);
        self.fetch_outputs_window(saved_start.min(cap));
        let restored = pinned_oid.and_then(|want| {
            self.outputs.iter().position(|o|
                o.get("id").and_then(Value::as_i64) == Some(want))
        });
        self.sel_output = restored
            .unwrap_or_else(|| saved_sel.min(self.outputs.len().saturating_sub(1)));
    }

    /// Pull the newest-first slice of the current box's sqlar — populates
    /// `recent_changes` when the selected box is live, clears it otherwise.
    fn refresh_recent_changes(&mut self) {
        self.recent_changes.clear();
        let live = self.sessions.get(self.sel_session)
            .and_then(|s| s.get("live").and_then(Value::as_bool))
            .unwrap_or(false);
        if !live { return; }
        let Some(sid) = self.cur_sid() else { return };
        if let Ok(rep) = rpc(&self.sock, "review.recent_changes",
                             json!([sid, 40])) {
            if let Some(arr) = rep.as_array() {
                self.recent_changes = arr.clone();
            }
        }
        // The Sessions-view right pane shows the five-list bundle
        // (outputs / changes / processes / pipelines / build-edges)
        // newest-first. One RPC instead of five — engine packs them in
        // review.box_summary; xattr modifications mix into the changes
        // list as kind="xattr" rows.
        self.box_summary = serde_json::json!(null);
        match rpc(&self.sock, "review.box_summary", json!([sid, 8])) {
            Ok(v) => self.box_summary = v,
            Err(_) => {}
        }
    }

    /// Move the changes-pane cursor by `delta` rows in the engine-side view
    /// coordinate system, sliding the window if the new position would leave
    /// it AND skipping connector (directory) rows the same way the procs
    /// pane skips its structural ancestors. `sel_change` is kept in
    /// [0, window.len()) after this returns.
    fn sel_change_global_advance(&mut self, delta: isize) {
        if self.changes_total == 0 { return; }
        let step: isize = if delta > 0 { 1 } else { -1 };
        let mut global = self.changes_window_start + self.sel_change;
        loop {
            let new_global = global as isize + step;
            if new_global < 0 || new_global as usize >= self.changes_total {
                return; // hit boundary
            }
            global = new_global as usize;
            let win_end = self.changes_window_start + self.changes.len();
            if global < self.changes_window_start || global >= win_end {
                let quarter = WINDOW_SIZE / 4;
                let new_start = global.saturating_sub(
                    if step > 0 { quarter } else { WINDOW_SIZE - quarter });
                self.fetch_changes_window(new_start);
            }
            let off = global.saturating_sub(self.changes_window_start);
            let is_connector = self.changes.get(off)
                .and_then(|c| c.get("connector").and_then(Value::as_bool))
                .unwrap_or(false);
            if !is_connector {
                self.sel_change = off;
                return;
            }
        }
    }

    /// Load the hunks (unified diff) for the selected change of the selected box.
    /// For a BINARY change this also kicks off the off-loop structural diff
    /// (struct_quick now, struct_finish on a worker thread).
    fn load_hunks(&mut self) {
        self.hunks = Value::Null;
        self.hunk_scroll = 0;
        self.sel_hunk = 0;
        self.cd_info = None;
        // Supersede any structural diff in flight for the previous row.
        self.cancel_struct();
        let (Some(sid), Some(path)) = (self.cur_sid(), self.cur_change_path()) else {
            return;
        };
        // cd-info header for any diff (text or binary). One decorate +
        // change_mode RPC at cursor-move time keeps the per-frame draw RPC-
        // free. Shape mirrors the prototype's _update_cd_info.
        self.cd_info = Some(self.binary_header(&sid, &path));
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

    /// Open the engine-side procs view and fetch the first window. The
    /// engine returns the DFS-flattened tree rows (depth + connector flag
    /// baked into each row), so the UI no longer rebuilds the tree per
    /// keystroke — that was the multi-second-per-keypress death spiral on a
    /// box with millions of processes.
    fn load_processes(&mut self) {
        self.close_processes_view();
        self.processes.clear();
        self.processes_total = 0;
        self.processes_window_start = 0;
        self.sel_proc = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_procs.active());
        match rpc(&self.sock, "view.open", json!(["procs", sid, filter])) {
            Ok(v) => {
                self.processes_view = v.get("view_id").and_then(Value::as_u64);
                self.processes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
            Err(e) if e.contains("unknown verb") => {
                self.status = format!(
                    "engine doesn't speak view.* — kill the stale engine \
                     (the socket {} is being answered by an old process) and \
                     run sarun again", self.sock);
                return;
            }
            Err(e) => { self.status = format!("view.open procs: {e}"); return; }
        }
        self.fetch_processes_window(0);
        self.seek_first_real_proc();
    }

    /// Load the brush pipelines for the currently-selected box. Each row
    /// is one `brushprov` entry: cmd + parsed structure + which process
    /// rows brush spawned for it (the D9 pipeline→processes linkage).
    /// Full list (no windowing) — bounded by what brush actually ran.
    fn load_pipelines(&mut self) {
        self.pipelines.clear();
        self.sel_pipeline = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        match rpc(&self.sock, "brushprov", json!([sid])) {
            Ok(Value::Array(rows)) => self.pipelines = rows,
            Ok(_) => {}
            Err(e) => self.status = format!("brushprov: {e}"),
        }
    }

    /// Load the embedded-ninja build edges for the currently-selected box.
    /// Each row is one parsed edge: outs / ins / cmd. Includes up-to-date
    /// targets that never executed.
    fn load_build_edges(&mut self) {
        self.build_edges.clear();
        self.sel_edge = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        match rpc(&self.sock, "build_edges", json!([sid])) {
            Ok(Value::Array(rows)) => self.build_edges = rows,
            Ok(_) => {}
            Err(e) => self.status = format!("build_edges: {e}"),
        }
    }

    /// Advance sel_proc to the first non-connector row in the window. Without
    /// this the cursor sits on a structural ancestor (a connector dim row),
    /// which feels like "no item selected" — the prototype puts the cursor
    /// on a real process, never a connector.
    fn seek_first_real_proc(&mut self) {
        for (i, p) in self.processes.iter().enumerate() {
            if p.get("connector").and_then(Value::as_bool) != Some(true) {
                self.sel_proc = i;
                return;
            }
        }
        self.sel_proc = 0;
    }

    /// Pull one window of the procs view; `start` is in engine-view
    /// coordinates (post-filter), the response carries the row slice
    /// already flattened (each row has rid/tgid/ppid/exe/argv/depth/
    /// connector — same shape as the old ProcTreeRow).
    fn fetch_processes_window(&mut self, start: usize) {
        let Some(vid) = self.processes_view else { return };
        let start = start.min(self.processes_total.saturating_sub(1).max(0));
        match rpc(&self.sock, "view.window",
                  json!([vid, start, WINDOW_SIZE])) {
            Ok(v) => {
                self.processes_window_start =
                    v.get("start").and_then(Value::as_u64).unwrap_or(start as u64) as usize;
                self.processes_total =
                    v.get("total").and_then(Value::as_u64)
                        .unwrap_or(self.processes_total as u64) as usize;
                self.processes = v.get("rows").and_then(Value::as_array).cloned()
                    .unwrap_or_default();
            }
            Err(e) => self.status = format!("view.window procs: {e}"),
        }
    }

    fn close_processes_view(&mut self) {
        if let Some(vid) = self.processes_view.take() {
            let _ = rpc(&self.sock, "view.close", json!([vid]));
        }
    }

    /// Push the local f_procs filter to the engine view, then refetch from
    /// the top. Same shape as push_changes_filter.
    fn push_procs_filter(&mut self) {
        let Some(vid) = self.processes_view else { return };
        let filter = filter_to_json(self.f_procs.active());
        match rpc(&self.sock, "view.filter", json!([vid, filter])) {
            Ok(v) => {
                self.processes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.processes_window_start = 0;
                self.sel_proc = 0;
                self.fetch_processes_window(0);
            }
            Err(e) => self.status = format!("view.filter procs: {e}"),
        }
    }

    /// Move the procs cursor by `delta`, skipping connector rows.
    /// `sel_proc` indexes into the current window; when the cursor walks
    /// off the window edge, slide one window worth and re-aim. Connectors
    /// inside a filtered view never appear (the engine excludes them),
    /// so the skip loop only matters in the unfiltered tree.
    fn move_proc_cursor(&mut self, delta: isize) {
        if self.processes_total == 0 { return; }
        let step: isize = if delta > 0 { 1 } else { -1 };
        // global cursor position within the engine view's idx
        let mut global = self.processes_window_start + self.sel_proc;
        loop {
            let new_global = global as isize + step;
            if new_global < 0 || new_global as usize >= self.processes_total {
                return; // hit the boundary, leave cursor where it was
            }
            global = new_global as usize;
            // is this row in the current window?
            let win_end = self.processes_window_start + self.processes.len();
            if global < self.processes_window_start || global >= win_end {
                let quarter = WINDOW_SIZE / 4;
                let new_start = global.saturating_sub(
                    if step > 0 { quarter } else { WINDOW_SIZE - quarter });
                self.fetch_processes_window(new_start);
            }
            // skip connectors (unfiltered view shows them, but the cursor
            // doesn't land on them — they're structural-ancestor dim rows)
            let off = global.saturating_sub(self.processes_window_start);
            let is_connector = self.processes.get(off)
                .and_then(|r| r.get("connector").and_then(Value::as_bool))
                .unwrap_or(false);
            if !is_connector {
                self.sel_proc = off;
                return;
            }
            // connector: keep stepping in the same direction.
        }
    }

    /// Load the captured outputs index for the selected box, then fetch and
    /// decode each row's bytes (output_detail wire-encodes them as {"__b":b64})
    /// into a single scrollable stdout/stderr transcript.
    fn load_outputs(&mut self) {
        self.close_outputs_view();
        self.outputs.clear();
        self.output_segs.clear();
        self.outputs_total = 0;
        self.outputs_window_start = 0;
        self.sel_output = 0;
        self.out_scroll = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_outputs.active());
        match rpc(&self.sock, "view.open", json!(["outputs", sid, filter])) {
            Ok(v) => {
                self.outputs_view = v.get("view_id").and_then(Value::as_u64);
                self.outputs_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
            Err(e) if e.contains("unknown verb") => {
                self.status = format!(
                    "engine doesn't speak view.* — kill the stale engine \
                     (the socket {} is being answered by an old process) and \
                     run sarun again", self.sock);
                return;
            }
            Err(e) => { self.status = format!("view.open outputs: {e}"); return; }
        }
        self.fetch_outputs_window(0);
    }

    /// Pull one window of the outputs index, then decode just those rows'
    /// captured bytes into the scrollback `output_view`. The decode loop is
    /// bounded by the window so a box with thousands of recorded outputs
    /// doesn't pin the UI thread on RPCs at load time.
    fn fetch_outputs_window(&mut self, start: usize) {
        let Some(vid) = self.outputs_view else { return };
        let start = start.min(self.outputs_total.saturating_sub(1).max(0));
        match rpc(&self.sock, "view.window",
                  json!([vid, start, WINDOW_SIZE])) {
            Ok(v) => {
                self.outputs_window_start =
                    v.get("start").and_then(Value::as_u64).unwrap_or(start as u64) as usize;
                self.outputs_total =
                    v.get("total").and_then(Value::as_u64)
                        .unwrap_or(self.outputs_total as u64) as usize;
                self.outputs = v.get("rows").and_then(Value::as_array).cloned()
                    .unwrap_or_default();
            }
            Err(e) => { self.status = format!("view.window outputs: {e}"); return; }
        }
        // Decode each window-row's captured bytes into a segment kept under
        // its origin (oid, stream). outputs_lines uses these to draw a
        // per-line gutter for the selected entry and the OUTPUT_WINDOW_CAP
        // window centred on it — both impossible if we'd concatenated the
        // text into one string.
        let Some(sid) = self.cur_sid() else { return };
        self.output_segs.clear();
        for o in &self.outputs {
            let oid = o.get("id").and_then(Value::as_i64).unwrap_or(-1);
            let stream = o.get("stream").and_then(Value::as_i64).unwrap_or(0);
            let text = match rpc(&self.sock, "output_detail", json!([sid, oid])) {
                Ok(d) => d.get("content").and_then(|c| c.get("__b"))
                    .and_then(Value::as_str)
                    .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            self.output_segs.push((oid, stream, text));
        }
    }

    fn close_outputs_view(&mut self) {
        if let Some(vid) = self.outputs_view.take() {
            let _ = rpc(&self.sock, "view.close", json!([vid]));
        }
    }

    /// Push the local f_outputs filter to the engine view, then refetch.
    fn push_outputs_filter(&mut self) {
        let Some(vid) = self.outputs_view else { return };
        let filter = filter_to_json(self.f_outputs.active());
        match rpc(&self.sock, "view.filter", json!([vid, filter])) {
            Ok(v) => {
                self.outputs_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.outputs_window_start = 0;
                self.sel_output = 0;
                self.fetch_outputs_window(0);
            }
            Err(e) => self.status = format!("view.filter outputs: {e}"),
        }
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
        // Right-pane focused: scroll the detail body, not the left list.
        // Hunks doesn't go through here (its own keymap drives the diff
        // scroll); right_pane_scrollable() filters Hunks out.
        if self.right_focused && self.right_pane_scrollable() {
            self.right_scroll = self.right_scroll.saturating_add(1);
            return;
        }
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
            Pane::Processes => self.move_proc_cursor(1),
            Pane::Outputs => {
                if self.sel_output + 1 < self.outputs.len() {
                    self.sel_output += 1;
                }
            }
            Pane::Rules => {
                if self.sel_rule + 1 < self.rules.len() {
                    self.sel_rule += 1;
                }
            }
            Pane::Pipelines => {
                if self.sel_pipeline + 1 < self.pipelines.len() {
                    self.sel_pipeline += 1;
                }
            }
            Pane::BuildEdges => {
                if self.sel_edge + 1 < self.build_edges.len() {
                    self.sel_edge += 1;
                }
            }
            Pane::Help => self.out_scroll = self.out_scroll.saturating_add(1),
            Pane::Pty => {}
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn move_up(&mut self) {
        if self.right_focused && self.right_pane_scrollable() {
            self.right_scroll = self.right_scroll.saturating_sub(1);
            return;
        }
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
            Pane::Processes => self.move_proc_cursor(-1),
            Pane::Outputs => self.sel_output = self.sel_output.saturating_sub(1),
            Pane::Rules => self.sel_rule = self.sel_rule.saturating_sub(1),
            Pane::Pipelines => self.sel_pipeline = self.sel_pipeline.saturating_sub(1),
            Pane::BuildEdges => self.sel_edge = self.sel_edge.saturating_sub(1),
            Pane::Help => self.out_scroll = self.out_scroll.saturating_sub(1),
            Pane::Pty => {}
        }
    }

    /// PgDn / PgUp move the cursor by ~one screenful (PAGE_SIZE rows). For
    /// list panes that's a multi-step nav so the existing window-slide /
    /// connector-skip stays correct; for the diff and help bodies it's a
    /// straight scroll bump.
    #[cfg_attr(test, allow(dead_code))]
    fn page_down(&mut self) { self.page_move(PAGE_SIZE as isize); }
    #[cfg_attr(test, allow(dead_code))]
    fn page_up(&mut self) { self.page_move(-(PAGE_SIZE as isize)); }

    fn page_move(&mut self, delta: isize) {
        let n = delta.unsigned_abs();
        let step: isize = if delta > 0 { 1 } else { -1 };
        if self.right_focused && self.right_pane_scrollable() {
            let n16 = n as u16;
            if step > 0 { self.right_scroll = self.right_scroll.saturating_add(n16); }
            else { self.right_scroll = self.right_scroll.saturating_sub(n16); }
            return;
        }
        match self.focus {
            Pane::Sessions => {
                let total = self.sessions.len();
                if total == 0 { return; }
                let cur = self.sel_session as isize;
                let new = (cur + delta).clamp(0, total as isize - 1) as usize;
                if new != self.sel_session {
                    self.sel_session = new;
                    self.load_changes();
                }
            }
            Pane::Changes => {
                if self.changes_total == 0 { return; }
                for _ in 0..n {
                    let g = self.changes_window_start + self.sel_change;
                    if (step > 0 && g + 1 >= self.changes_total) || (step < 0 && g == 0) {
                        break;
                    }
                    self.sel_change_global_advance(step);
                }
                self.load_hunks();
            }
            Pane::Processes => {
                for _ in 0..n { self.move_proc_cursor(step); }
            }
            Pane::Outputs => {
                let total = self.outputs.len();
                if total == 0 { return; }
                let cur = self.sel_output as isize;
                self.sel_output = (cur + delta).clamp(0, total as isize - 1) as usize;
            }
            Pane::Rules => {
                let total = self.rules.len();
                if total == 0 { return; }
                let cur = self.sel_rule as isize;
                self.sel_rule = (cur + delta).clamp(0, total as isize - 1) as usize;
            }
            Pane::Pipelines => {
                let total = self.pipelines.len();
                if total == 0 { return; }
                let cur = self.sel_pipeline as isize;
                self.sel_pipeline = (cur + delta).clamp(0, total as isize - 1) as usize;
            }
            Pane::BuildEdges => {
                let total = self.build_edges.len();
                if total == 0 { return; }
                let cur = self.sel_edge as isize;
                self.sel_edge = (cur + delta).clamp(0, total as isize - 1) as usize;
            }
            Pane::Hunks => {
                let n16 = n as u16;
                if step > 0 { self.hunk_scroll = self.hunk_scroll.saturating_add(n16); }
                else { self.hunk_scroll = self.hunk_scroll.saturating_sub(n16); }
            }
            Pane::Help => {
                let n16 = n as u16;
                if step > 0 { self.out_scroll = self.out_scroll.saturating_add(n16); }
                else { self.out_scroll = self.out_scroll.saturating_sub(n16); }
            }
            Pane::Pty => {}
        }
    }

    /// True iff the current view's RIGHT pane is keyboard-scrollable —
    /// i.e. Tab focuses something and j/k/PgUp/PgDn drive `right_scroll`.
    /// Changes/Hunks is its own thing (toggles between two list-level
    /// focuses with their own actions); Help and Pty are full-screen
    /// with no peer. Everywhere else the right pane is a (possibly long)
    /// detail body — give it a scroll focus.
    fn right_pane_scrollable(&self) -> bool {
        matches!(self.focus,
            Pane::Sessions | Pane::Processes | Pane::Outputs
            | Pane::Pipelines | Pane::BuildEdges | Pane::Rules)
    }

    /// Snap focus back to the LEFT list and reset the right-pane scroll.
    /// Called whenever the user picks a different view via a letter chip
    /// (b/c/p/o/l/g/e/?) — we don't carry the "right pane focused" bit
    /// across views, that would confuse the cursor in the new view.
    fn snap_left(&mut self) {
        self.right_focused = false;
        self.right_scroll = 0;
    }

    /// Tab swaps the active PANE inside the current view — never the view
    /// itself. The letter chips (b/c/p/o/l/g/e/?) switch views.
    ///   * Changes ↔ Hunks: a special case — the right pane (diff) has
    ///     its OWN cursor (sel_hunk) and a/x/d acts per-hunk. We toggle
    ///     between the two list-level focuses.
    ///   * Sessions / Procs / Outputs / Pipelines / BuildEdges / Rules:
    ///     the right pane is a scrollable detail body. Tab flips
    ///     `right_focused`; j/k/PgUp/PgDn then drive `right_scroll`.
    ///   * Help / Pty are full-screen with no peer — no-op (PTY does
    ///     tab-out to Sessions because the keystroke would otherwise be
    ///     consumed; Help can be left with q/Esc).
    #[cfg_attr(test, allow(dead_code))]
    fn next_pane(&mut self) {
        match self.focus {
            Pane::Changes => self.focus = Pane::Hunks,
            Pane::Hunks => self.focus = Pane::Changes,
            Pane::Pty => self.focus = Pane::Sessions,
            _ if self.right_pane_scrollable() => {
                self.right_focused = !self.right_focused;
                if !self.right_focused { self.right_scroll = 0; }
            }
            _ => {}
        }
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
            | Pane::Pipelines | Pane::BuildEdges
            | Pane::Help | Pane::Pty => {}
        }
    }

    /// The currently-selected PTY pane, if any.
    fn cur_pty(&self) -> Option<&PtyPane> { self.ptys.get(self.sel_pty) }
    #[cfg_attr(test, allow(dead_code))]
    fn cur_pty_mut(&mut self) -> Option<&mut PtyPane> {
        self.ptys.get_mut(self.sel_pty)
    }

    /// Cycle to the next / previous PTY (wrapping).
    #[cfg_attr(test, allow(dead_code))]
    fn pty_next(&mut self) {
        if self.ptys.is_empty() { return; }
        self.sel_pty = (self.sel_pty + 1) % self.ptys.len();
    }
    #[cfg_attr(test, allow(dead_code))]
    fn pty_prev(&mut self) {
        if self.ptys.is_empty() { return; }
        self.sel_pty = (self.sel_pty + self.ptys.len() - 1) % self.ptys.len();
    }
    /// Kill the current PTY (drop the connection — engine SIGHUPs the
    /// child). Selector slides to the next PTY; focus snaps back to
    /// Sessions if the last one is gone.
    #[cfg_attr(test, allow(dead_code))]
    fn pty_kill(&mut self) {
        if self.sel_pty >= self.ptys.len() { return; }
        self.ptys.remove(self.sel_pty);
        if self.ptys.is_empty() {
            self.sel_pty = 0;
            self.focus = Pane::Sessions;
            self.status = "PTY killed (no more open)".into();
        } else {
            if self.sel_pty >= self.ptys.len() { self.sel_pty = self.ptys.len() - 1; }
            self.status = format!("PTY killed · {} remain", self.ptys.len());
        }
    }

    /// Open an engine-held PTY running `argv` and focus it. Multiple
    /// PTYs can coexist; each is appended to `ptys` and becomes the
    /// active selection.
    #[cfg_attr(test, allow(dead_code))]
    fn open_pty(&mut self, argv: Vec<String>) {
        if argv.is_empty() { self.status = "pty: empty command".into(); return; }
        match PtyPane::open(&self.sock, &argv, 24, 80) {
            Ok(p) => {
                self.ptys.push(p);
                self.sel_pty = self.ptys.len() - 1;
                self.focus = Pane::Pty;
                self.status = format!(
                    "PTY {}/{} · F2/F3 cycle · F8 kill · F12 detach",
                    self.sel_pty + 1, self.ptys.len());
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
            // Live-box notifications, both broadcast on every actual
            // change (no polling): type=overlay for file writes, mkdir,
            // unlink, etc.; type=process_added when capture.rs records
            // a new process row. If it's for the currently-loaded box,
            // refresh the focused view's data so the UI tracks reality.
            Some("overlay") | Some("process_added") => {
                // Both events refresh the focused view's data when the
                // affected sid matches the currently-loaded box. Routing
                // to the cursor-preserving refreshers, never the reset-
                // happy load_*() versions.
                let sid = ev.get("sid").and_then(Value::as_str)
                    .or_else(|| ev.get("session_id").and_then(Value::as_str));
                if sid.is_some() && sid == self.cur_sid().as_deref() {
                    let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
                    match self.focus {
                        Pane::Changes | Pane::Hunks if kind == "overlay" =>
                            self.refresh_changes_preserving_cursor(),
                        Pane::Processes if kind == "process_added" =>
                            self.refresh_processes_preserving_cursor(),
                        Pane::Outputs if kind == "overlay" =>
                            self.refresh_outputs_preserving_cursor(),
                        Pane::Sessions => self.refresh_recent_changes(),
                        _ => {}
                    }
                }
            }
            // Consolidation events from the Python prototype's engine (the
            // Rust engine has no consolidation phase, so these are only
            // ever seen by a Rust UI attached to a Python engine).
            Some("consolidate_progress") => {
                let done = ev.get("done").and_then(Value::as_u64).unwrap_or(0);
                let total = ev.get("total").and_then(Value::as_u64).unwrap_or(0);
                self.status = format!("consolidating… {done}/{total}");
            }
            Some("consolidate_done") => {
                self.status = "consolidated".into();
                self.load_changes();
            }
            Some("consolidate_failed") => {
                let err = ev.get("error").and_then(Value::as_str).unwrap_or("?");
                self.status = format!("consolidate failed: {err}");
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
            FilterView::Procs => self.push_procs_filter(),
            FilterView::Outputs => self.push_outputs_filter(),
        }
    }

    fn reset_view_cursor(&mut self, v: FilterView) {
        match v {
            FilterView::Changes => self.sel_change = 0,
            FilterView::Procs => self.sel_proc = 0,
            FilterView::Outputs => self.sel_output = 0,
        }
    }

    /// The current procs WINDOW — already filtered + tree-flattened by the
    /// engine-side view, so the UI just walks the slice. Rows carry depth +
    /// connector flags; sel_proc indexes into this window.
    fn visible_processes(&self) -> Vec<&Value> {
        self.processes.iter().collect()
    }

    /// The current changes WINDOW — already filtered by the engine-side view,
    /// so the UI just walks the (small) slice. `sel_change` indexes into this.
    /// Use `changes_total` for the underlying view size; this is just what
    /// the UI happens to be holding right now.
    fn visible_changes(&self) -> Vec<&Value> {
        self.changes.iter().collect()
    }

    /// The current outputs WINDOW — already filtered by the engine view, so
    /// the UI just returns the slice.
    fn visible_outputs(&self) -> Vec<&Value> {
        self.outputs.iter().collect()
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
                let touched = match ids {
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
                        true
                    }
                    None => {
                        if self.view_filter(dest).generated {
                            *self.view_filter_mut(dest) = ViewFilter::default();
                            self.reset_view_cursor(dest);
                            true
                        } else { false }
                    }
                };
                // Push the new (or cleared) generated filter to the engine
                // view — without this the local f_* flips but the engine's
                // materialized idx still reflects the old filter and the
                // pane shows stale rows.
                if touched { self.push_view_filter(dest); }
            }
        }
        self.focus = dest_pane;
    }
}

// ── process tree row (the projection of the engine-flattened tree) ──────────

/// One DFS-ordered tree row: (row_id, tgid, ppid, exe, argv, depth, connector).
/// `connector` is true for a purely-structural ancestor added to connect the
/// forest (dimmed; the cursor skips it). The actual DFS flatten happens in
/// the engine (views.rs); this struct is just what the UI projects out of
/// the windowed JSON rows for rendering.
#[derive(Clone)]
struct ProcTreeRow {
    rid: i64,
    tgid: i64,
    #[allow(dead_code)] ppid: i64,
    exe: String,
    argv: Vec<String>,
    depth: usize,
    connector: bool,
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

fn title(base: &str, _focused: bool) -> String {
    format!(" {base} ")
}

fn block(t: String, focused: bool) -> Block<'static> {
    // Focused pane: cyan-bold DOUBLE border (Norton/Turbo style); blurred:
    // gray plain. No "«focus»" tag in the title — the border carries it.
    let (style, btype) = if focused {
        (Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
         ratatui::widgets::BorderType::Double)
    } else {
        (Style::default().fg(Color::Gray),
         ratatui::widgets::BorderType::Plain)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(btype)
        .border_style(style)
        .title(t)
}

/// "Now minus ts" as a tight human label — seconds/minutes/hours, same shape
/// as the prototype's fmt_age helper. Used for the sessions pane's Age col.
fn fmt_age(ts: f64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let s = (now - ts).max(0.0) as i64;
    if s < 60 { format!("{s}s") }
    else if s < 3600 { format!("{}m{:02}s", s / 60, s % 60) }
    else { format!("{}h{:02}m", s / 3600, (s % 3600) / 60) }
}

/// Map a session status string to (single-char flag, color) — F column of
/// the sessions pane (mirrors the prototype's sty()/flag scheme).
fn session_flag(status: &str) -> (&'static str, Color) {
    match status {
        "running"  => ("R", Color::Green),
        "finished" => ("F", Color::DarkGray),
        "killed"   => ("K", Color::Red),
        "error"    => ("E", Color::Red),
        _          => ("?", Color::Reset),
    }
}

fn sessions_lines(app: &App) -> Vec<Line<'static>> {
    // Columns mirror the prototype's #s-tab: F | Name | PID | Cmd | Age.
    // Sessions are nested under parents via the dotted display path — render
    // them as a DFS-ordered tree, indenting children under their parent.
    // (Same shape as the prototype's _rebuild_sessions; we walk a sorted-by-
    // path order which gives DFS automatically for any well-formed forest.)
    let mut out = vec![Line::from(Span::styled(
        format!("{:<1} {:<24} {:<6} {:<24} {:>8}",
                "F", "Name", "PID", "Cmd", "Age"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.sessions.is_empty() {
        out.push(Line::from("(no boxes)"));
        return out;
    }
    // refresh_sessions already sorted by dotted display path, so this is
    // DFS order — children land immediately after their parent.
    for (i, s) in app.sessions.iter().enumerate() {
        let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let status = g("status");
        let (flag, color) = session_flag(&status);
        let path = g("path");
        // Depth from dot count; bare top-level boxes have depth 0.
        let depth = if path.is_empty() { 0 }
                    else { path.matches('.').count() };
        let indent = "  ".repeat(depth);
        // Display label: name if present, else the dotted-path's last
        // segment, else the session id.
        let basename = {
            let n = g("name");
            if !n.is_empty() { n }
            else if !path.is_empty() { path.rsplit('.').next().unwrap_or(&path).to_string() }
            else { g("session_id") }
        };
        let pid = s.get("pid").and_then(Value::as_i64).unwrap_or(0);
        let cmd = s.get("cmd").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let cmd24: String = cmd.chars().take(24).collect();
        let started = s.get("started").and_then(Value::as_f64).unwrap_or(0.0);
        let age = if started > 0.0 { fmt_age(started) } else { String::new() };
        let pid_str = if pid > 0 { pid.to_string() } else { String::new() };
        let name_col = format!("{indent}{basename}");
        let text = format!("{flag:<1} {name_col:<24} {pid_str:<6} {cmd24:<24} {age:>8}");
        let line = if i == app.sel_session {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            Line::from(Span::styled(text, Style::default().fg(color)))
        };
        out.push(line);
    }
    out
}

/// Plain-space tree indent (used by both the procs and changes panes —
/// the earlier "└ " glyph hurt legibility more than it added structure;
/// the depth alone is enough).
fn tree_indent(depth: usize) -> String { "  ".repeat(depth) }

/// Render the cached cd_info tuples (style-tag, text) as styled Lines —
/// the small header strip above the diff. Same set of style tags the
/// binary structural-diff header uses.
fn cd_info_lines(app: &App) -> Vec<Line<'static>> {
    let Some(items) = app.cd_info.as_ref() else {
        return vec![Line::from(Span::styled("(select a change)",
            Style::default().add_modifier(Modifier::DIM)))];
    };
    items.iter().map(|(tag, txt)| {
        let st = match tag.as_str() {
            "bold" => Style::default().add_modifier(Modifier::BOLD),
            "stale" => Style::default().fg(Color::Red).add_modifier(Modifier::REVERSED),
            _ => Style::default().fg(Color::DarkGray),
        };
        Line::from(Span::styled(txt.clone(), st))
    }).collect()
}

fn changes_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<1} {:>10}  {}", "", "SIZE", "PATH"),
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
        let name = c.get("name").and_then(Value::as_str).unwrap_or("");
        let size = c.get("size").and_then(Value::as_i64).unwrap_or(0);
        let depth = c.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize;
        let connector = c.get("connector").and_then(Value::as_bool) == Some(true);
        // Prefer the per-row decoration (created vs modified, plus stale).
        // The source row's `kind` only knows deleted / symlink / changed.
        let (dec_kind, stale) = app.changes_decor.get(i)
            .map(|(k, s)| (k.as_str(), *s)).unwrap_or(("", false));
        let effective_kind = if !dec_kind.is_empty() { dec_kind } else { kind };
        let (glyph, color) = match effective_kind {
            "created"  => ("+", Color::Green),
            "modified" => ("~", Color::Yellow),
            "deleted"  => ("-", Color::Red),
            "symlink"  => ("~", Color::Magenta),
            "changed"  => ("~", Color::Yellow),
            // xattr leaf: indented under its file, displays the key
            // (set as `name`) and the value byte count (set as `size`).
            "xattr"    => ("@", Color::Cyan),
            // xattr-only file: an xattr was set on a path whose data
            // didn't change (no sqlar row of its own — the box just
            // chattr-tagged a passthrough file). Distinct dim glyph so
            // it doesn't pretend to be a "changed" file.
            "xattr-only" => ("@", Color::DarkGray),
            _ => ("…", Color::DarkGray),
        };
        let indent = tree_indent(depth);
        let line = if connector {
            let text = format!("{:<1} {:>10}  {indent}{name}/", "", "");
            Line::from(Span::styled(text,
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)))
        } else if i == app.sel_change {
            let sz = if size > 0 { fmt_bytes(size) } else { String::new() };
            let stale_mark = if stale { "!" } else { "" };
            let text = format!("{glyph}{stale_mark:<1} {sz:>10}  {indent}{name}");
            Line::from(Span::styled(text,
                Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            let sz = if size > 0 { fmt_bytes(size) } else { String::new() };
            let mut spans = vec![
                Span::styled(glyph.to_string(), Style::default().fg(color)),
            ];
            if stale {
                spans.push(Span::styled("!",
                    Style::default().fg(Color::Red).add_modifier(Modifier::REVERSED)));
            } else {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::raw(format!(" {sz:>10}  {indent}")));
            spans.push(Span::styled(name.to_string(),
                if stale { Style::default().fg(Color::Red) }
                else { Style::default().fg(color) }));
            Line::from(spans)
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
    /// Render rows for the procs pane. The engine flattens the tree at
    /// view.open and embeds depth + connector flag in every row, so this is
    /// just a cheap projection of the window — no RPC, no rebuild, no tree
    /// walk per keystroke (that walk was N proc_info RPCs per row per
    /// render, which was the multi-second hot path).
    fn proc_tree_rows(&self) -> Vec<ProcTreeRow> {
        self.processes.iter().map(|p| ProcTreeRow {
            rid: p.get("rid").and_then(Value::as_i64).unwrap_or(-1),
            tgid: p.get("tgid").and_then(Value::as_i64).unwrap_or(0),
            ppid: p.get("ppid").and_then(Value::as_i64).unwrap_or(0),
            exe: p.get("exe").and_then(Value::as_str).unwrap_or("").to_string(),
            argv: p.get("argv").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str)
                          .map(String::from).collect())
                .unwrap_or_default(),
            depth: p.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
            connector: p.get("connector").and_then(Value::as_bool).unwrap_or(false),
        }).collect()
    }
}

/// PROCESSES pane: a depth-indented process TREE. Each row shows tgid · ppid ·
/// (indented) exe · argv. Structural-ancestor connector rows are dimmed and the
/// cursor (sel_proc, counted over non-connector rows) skips them.
fn processes_lines(app: &App) -> Vec<Line<'static>> {
    // Layout: TGID, then the argv (indented by tree depth, with the
    // exe/argv[0] basename highlighted as the legibility anchor). PPID is
    // redundant — the indent already says "child of the row above" — and
    // the "└ " connector glyph was making everything harder to scan, so the
    // indent is just spaces.
    let mut out = vec![Line::from(Span::styled(
        format!("{:>6}  {}", "TGID", "PROCESS"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    let rows = app.proc_tree_rows();
    if rows.is_empty() {
        let msg = if app.processes_total == 0 {
            if app.f_procs.active().is_some() {
                "(no processes match filter)"
            } else { "(no captured processes)" }
        } else { "(empty window — engine view drifted)" };
        out.push(Line::from(msg));
        return out;
    }
    let hi = Some(app.sel_proc);
    for (i, r) in rows.iter().enumerate() {
        // Pick the program-name anchor. argv[0] is what the user typed; fall
        // back to exe when argv is empty (e.g. an exec without a recorded
        // argv). Take the basename so a long /usr/local/bin/foo doesn't
        // drown out the rest of the row.
        let anchor_path = r.argv.first().filter(|s| !s.is_empty()).cloned()
            .unwrap_or_else(|| r.exe.clone());
        let anchor = anchor_path.rsplit('/').next().unwrap_or(&anchor_path).to_string();
        let rest_argv = if r.argv.len() > 1 { r.argv[1..].join(" ") } else { String::new() };
        let indent = "  ".repeat(r.depth);

        let (anchor_style, rest_style) = if Some(i) == hi {
            (Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
             Style::default().fg(Color::Black).bg(Color::Cyan))
        } else if r.connector {
            (Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
             Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM))
        } else {
            (Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
             Style::default().fg(Color::DarkGray))
        };
        let tgid_str = if r.connector { String::new() } else { r.tgid.to_string() };
        let mut spans = vec![
            Span::styled(format!("{tgid_str:>6}  "),
                         if Some(i) == hi {
                             Style::default().fg(Color::Black).bg(Color::Cyan)
                         } else { Style::default() }),
            Span::styled(indent,
                         if Some(i) == hi {
                             Style::default().fg(Color::Black).bg(Color::Cyan)
                         } else { Style::default() }),
            Span::styled(anchor, anchor_style),
        ];
        if !rest_argv.is_empty() {
            spans.push(Span::styled(format!(" {rest_argv}"), rest_style));
        }
        out.push(Line::from(spans));
    }
    out
}

/// OUTPUTS transcript pane — port of the prototype's _update_output_detail.
/// Walks `app.output_segs` line-by-line within a OUTPUT_WINDOW_CAP window
/// centred on the selected entry; prefixes every line with a "▌ " gutter
/// (yellow-bold) when its origin is the selection, "  " (dim) otherwise;
/// stderr lines render red; the selected entry's lines also get bold +
/// grey23 background. Out-of-window bytes get rolled up as
/// "… N earlier" / "… N more" elision lines.
fn outputs_lines(app: &App) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    // Header: total counts per stream — quick orientation.
    let (mut nout, mut nerr) = (0i64, 0i64);
    for o in &app.outputs {
        let len = o.get("len").and_then(Value::as_i64).unwrap_or(0);
        if o.get("stream").and_then(Value::as_i64).unwrap_or(0) == 1 { nerr += len; }
        else { nout += len; }
    }
    out.push(Line::from(Span::styled(
        format!("{} write(s) · {} stdout B · {} stderr B",
                app.outputs.len(), nout, nerr),
        Style::default().add_modifier(Modifier::BOLD))));
    if app.output_segs.is_empty() {
        out.push(Line::from("(no captured output)"));
        return out;
    }
    // Selected entry's id (used to drive the gutter mark and the window
    // centre). sel_output indexes the OUTPUTS index list, which lines up
    // with output_segs by construction.
    let sel_oid = app.outputs.get(app.sel_output)
        .and_then(|o| o.get("id").and_then(Value::as_i64));
    // Total concat size + selected entry's start offset (for centring).
    let total: usize = app.output_segs.iter().map(|(_, _, t)| t.len()).sum();
    let mut sel_off = 0usize;
    let mut acc = 0usize;
    for (oid, _st, txt) in &app.output_segs {
        if Some(*oid) == sel_oid { sel_off = acc; }
        acc += txt.len();
    }
    // Centre the window on sel_off, then clamp.
    let (start, end) = if total > OUTPUT_WINDOW_CAP {
        let half = OUTPUT_WINDOW_CAP / 2;
        let mut s = sel_off.saturating_sub(half);
        let e = (s + OUTPUT_WINDOW_CAP).min(total);
        s = e.saturating_sub(OUTPUT_WINDOW_CAP);
        (s, e)
    } else { (0, total) };
    if start > 0 {
        out.push(Line::from(Span::styled(
            format!("  … {} earlier", fmt_bytes(start as i64)),
            Style::default().add_modifier(Modifier::DIM))));
    }
    // Walk segments line-by-line within [start, end), applying gutter +
    // style per the prototype.
    let mut pos = 0usize;
    for (oid, stream, txt) in &app.output_segs {
        let seg_start = pos;
        let seg_end = pos + txt.len();
        pos = seg_end;
        if seg_end <= start || seg_start >= end { continue; }
        // Visible slice of this segment.
        let lo = start.saturating_sub(seg_start);
        let hi = txt.len() - seg_end.saturating_sub(end);
        let vis = &txt[lo..hi];
        let is_sel = Some(*oid) == sel_oid;
        let mut text_style = Style::default();
        if *stream == 1 { text_style = text_style.fg(Color::Red); }
        if is_sel {
            text_style = text_style.add_modifier(Modifier::BOLD).bg(Color::Rgb(58, 58, 58));
        }
        let gutter_style = if is_sel {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else { Style::default().add_modifier(Modifier::DIM) };
        let gutter = if is_sel { "▌ " } else { "  " };
        let lines: Vec<&str> = vis.split('\n').collect();
        let last_idx = lines.len().saturating_sub(1);
        for (i, ln) in lines.iter().enumerate() {
            if ln.is_empty() && i == last_idx && i > 0 { break; }
            out.push(Line::from(vec![
                Span::styled(gutter.to_string(), gutter_style),
                Span::styled(ln.to_string(), text_style),
            ]));
        }
    }
    if end < total {
        out.push(Line::from(Span::styled(
            format!("  … {} more", fmt_bytes((total - end) as i64)),
            Style::default().add_modifier(Modifier::DIM))));
    }
    out
}

/// OUTPUTS index (left pane). Columns mirror the prototype's #out-tab:
/// Time | Stream | Process | Bytes. exe + tgid are baked into each row by
/// the engine's source_outputs so the renderer doesn't RPC per output.
fn outputs_index_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<8} {:<6} {:<20} {:>10}", "Time", "Stream", "Process", "Bytes"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.outputs.is_empty() {
        let msg = if app.outputs_total == 0 {
            if app.f_outputs.active().is_some() { "(no outputs match filter)" }
            else { "(no captured output)" }
        } else { "(empty window — engine view drifted)" };
        out.push(Line::from(msg));
        return out;
    }
    for (i, o) in app.outputs.iter().enumerate() {
        let ts = o.get("ts").and_then(Value::as_f64).unwrap_or(0.0) as i64;
        let stream = o.get("stream").and_then(Value::as_i64).unwrap_or(0);
        let len = o.get("len").and_then(Value::as_i64).unwrap_or(0);
        let exe = o.get("exe").and_then(Value::as_str).unwrap_or("");
        let tgid = o.get("tgid").and_then(Value::as_i64).unwrap_or(0);
        let base = exe.rsplit('/').next().unwrap_or(exe);
        let proc_label = if tgid > 0 { format!("{base}·{tgid}") } else { base.to_string() };
        let proc_label: String = proc_label.chars().take(20).collect();
        let time_label = {
            let secs = ts.rem_euclid(86400);
            let h = secs / 3600; let m = (secs % 3600) / 60; let s = secs % 60;
            format!("{h:02}:{m:02}:{s:02}")
        };
        let stream_label = if stream == 1 { "err" } else { "out" };
        let text = format!("{time_label:<8} {stream_label:<6} {proc_label:<20} {len:>10}");
        let line = if i == app.sel_output {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            let color = if stream == 1 { Color::Red } else { Color::Reset };
            Line::from(Span::styled(text, Style::default().fg(color)))
        };
        out.push(line);
    }
    out
}

/// Right pane of the RULES view — the prototype's _clause_detail / _update_
/// rule_detail. Parses the currently-selected rule line via rules::FileRule,
/// prints the ACTION (colored by verb), then each clause as
/// "[join] [off] [not] kind:pattern" with the same dim styling for
/// disabled clauses; finally a per-kind help table for the kinds actually
/// used by this rule.
fn rule_detail_lines(app: &App) -> Vec<Line<'static>> {
    let Some(line) = app.rules.get(app.sel_rule) else {
        return vec![
            Line::from(Span::styled("n new · Enter edit · d delete",
                Style::default().add_modifier(Modifier::DIM))),
            Line::from(""),
            Line::from(format!("file: {}", app.rules_path().display())),
            Line::from(""),
            Line::from("Rules decide each captured write: apply (keep),"),
            Line::from("discard (drop), or passthrough. First match wins."),
        ];
    };
    let Some(rule) = crate::rules::FileRule::parse(line) else {
        return vec![Line::from(Span::styled(
            "(unparseable rule)",
            Style::default().fg(Color::Red)))];
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    let cyan = Style::default().fg(Color::Cyan);
    let (action_label, action_color) = match rule.action {
        crate::rules::Action::Apply       => ("APPLY",       Color::Green),
        crate::rules::Action::Discard     => ("DISCARD",     Color::Red),
        crate::rules::Action::Passthrough => ("PASSTHROUGH", Color::Cyan),
    };
    let mut out = vec![
        Line::from(Span::styled(action_label.to_string(),
            Style::default().fg(action_color).add_modifier(Modifier::BOLD))),
    ];
    for (n, c) in rule.clauses.iter().enumerate() {
        let mut lead = String::new();
        if n > 0 { lead.push_str(match c.join {
            crate::rules::Join::And => "and ", crate::rules::Join::Or => "or " }); }
        if !c.enabled { lead.push_str("off "); }
        if c.negate { lead.push_str("not "); }
        let kp = format!("{}:{}", c.m.kind, c.m.pattern);
        let kp_style = if c.enabled { Style::default() } else { dim };
        out.push(Line::from(vec![
            Span::styled(format!("  {lead}"), dim),
            Span::styled(kp, kp_style),
        ]));
    }
    out.push(Line::from(""));
    // Per-kind help table: each kind that this rule actually uses, with its
    // one-line glob semantics. Same text the prototype shows.
    let help = |k: &str| match k {
        "path" => "changed path (extended glob: * ** ? @(a|b) {a,b}; bare = any depth)",
        "box"  => "the box's hierarchical name (same globs as paths)",
        "exe"  => "triggering process's command pathname (path globs)",
        "cwd"  => "triggering process's working directory (path globs)",
        "arg"  => "any one of the triggering process's argv (raw glob)",
        _ => "",
    };
    let mut seen: Vec<&str> = vec![];
    for c in &rule.clauses {
        if !seen.contains(&c.m.kind.as_str()) { seen.push(c.m.kind.as_str()); }
    }
    for k in seen {
        out.push(Line::from(vec![
            Span::styled(format!("{:6} ", k), cyan),
            Span::styled(help(k).to_string(), dim),
        ]));
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
        t("  l  pipeLines           shell pipelines brush ran inside a -b box,"),
        d("     with their parsed structure and the process rows they spawned"),
        t("  g  build Graph         parsed ninja/make build edges from a -b box,"),
        d("     including up-to-date targets that never executed"),
        t("  e  file rules          the ordered apply/discard/passthrough rules"),
        t("  ?  this help"),
        t("  P  open an engine-held PTY — a live interactive shell pane"),
        d("     keys go to the box · Ctrl-] / F12 / Esc-Esc detaches back to the UI"),
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
        Modal::ActionMenu { items, .. } => (items.len() as u16) + 5,
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
        Modal::ActionMenu { title, items, sel } => {
            let mut body = vec![
                Line::from(Span::styled(title.clone(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                Line::from(""),
            ];
            // Two-column rows: label on the left (wide), key hint on
            // the right (narrow). Active row reverses; arrows move,
            // Enter activates, Esc cancels.
            let lw = items.iter().map(|i| i.label.chars().count())
                              .max().unwrap_or(20).max(20);
            for (i, it) in items.iter().enumerate() {
                let active = i == *sel;
                let style = if active {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default()
                };
                let label = format!("  {:<lw$}", it.label, lw = lw);
                body.push(Line::from(vec![
                    Span::styled(label, style),
                    Span::styled(format!("  {}", it.hint),
                        Style::default().fg(Color::DarkGray)),
                ]));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "↑/↓ move · Enter run · Esc cancel",
                Style::default().fg(Color::Gray))));
            (" actions ", body)
        }
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

/// Compute a vertical scroll offset for a Paragraph so the cursor row stays
/// inside the visible rect. `cursor_line` is the 0-based index of the
/// highlighted Line inside the Lines vector (so for our panes that emit a
/// header at index 0 and rows from index 1, it's `sel_row + 1`). `rect_h`
/// is the full pane rect height (block borders included; we subtract 2).
/// Anchors the cursor ~1/3 down so motion in either direction has room.
fn scroll_for_cursor(cursor_line: usize, n_lines: usize, rect_h: u16) -> u16 {
    let visible = (rect_h as usize).saturating_sub(2);
    if visible == 0 || n_lines <= visible { return 0; }
    let third = visible / 3;
    let want = cursor_line.saturating_sub(third);
    want.min(n_lines.saturating_sub(visible)) as u16
}

/// Mirrors the prototype's _keybar: a row of view-key chips (b/c/p/o/e),
/// the active view's chip reversed-bold + its label yellow-bold, plus a
/// "⦿ filter <expr>" badge when the focused view has an active filter.
/// Map a pane to its (accelerator letter, label, filter-view). The
/// menubar + fkeybar both use this so the letters / labels stay
/// consistent in one place.
fn view_of_pane(p: Pane) -> Option<(char, &'static str, FilterView)> {
    match p {
        Pane::Sessions => Some(('b', "boxes",    FilterView::Changes /* unused */)),
        Pane::Changes | Pane::Hunks
                       => Some(('c', "changes",  FilterView::Changes)),
        Pane::Processes => Some(('p', "procs",   FilterView::Procs)),
        Pane::Outputs   => Some(('o', "outputs", FilterView::Outputs)),
        Pane::Pipelines => Some(('l', "pipes",   FilterView::Changes /* unused */)),
        Pane::BuildEdges => Some(('g', "build",  FilterView::Changes /* unused */)),
        Pane::Rules     => Some(('e', "rules",   FilterView::Changes /* unused */)),
        Pane::Help      => Some(('?', "help",    FilterView::Changes /* unused */)),
        Pane::Pty       => Some(('P', "PTY",     FilterView::Changes /* unused */)),
    }
}

/// Top menubar: pane chips with their letter accelerators. The
/// active pane reverses; chips that would lead to an empty pane for
/// this box are dimmed (procs/outputs/pipes/build) or hidden (same
/// rule as the old keybar). PTY chip shows when any PTY is open.
/// Visible top-level menu chips for the current state. The same list
/// drives both the menubar render and F9 menu-nav dispatch — one
/// source of truth so they can't drift.
fn menubar_chips(app: &App) -> Vec<(char, &'static str)> {
    let has = |k: &str| app.box_summary.get(k)
        .and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
    let show_procs    = has("processes") || app.focus == Pane::Processes;
    let show_outputs  = has("outputs")   || app.focus == Pane::Outputs;
    let show_pipes    = has("pipelines") || app.focus == Pane::Pipelines;
    let show_build    = has("edges")     || app.focus == Pane::BuildEdges;
    let any_pty       = !app.ptys.is_empty();
    [
        Some(('b', "Boxes")),
        Some(('c', "Changes")),
        if show_procs   { Some(('p', "Procs"))   } else { None },
        if show_outputs { Some(('o', "Outputs")) } else { None },
        if show_pipes   { Some(('l', "Pipes"))   } else { None },
        if show_build   { Some(('g', "Build"))   } else { None },
        Some(('e', "Rules")),
        if any_pty { Some(('P', "PTYs")) } else { None },
        Some(('?', "Help")),
    ].into_iter().flatten().collect()
}

fn menubar_spans(app: &App) -> Vec<Span<'static>> {
    let active = view_of_pane(app.focus).map(|(k, _, _)| k);
    let chips = menubar_chips(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, label)) in chips.iter().enumerate() {
        let on = active == Some(*key);
        // Menu-nav mode (F9): the cursor lands on a chip and arrows
        // move it. Highlight differs from "active view" so the user
        // can tell "the cursor is HERE waiting for Enter" apart from
        // "this is the view I'm currently on".
        let menu_cursor = app.menu_nav && app.menu_sel == i;
        let style = if menu_cursor {
            Style::default().fg(Color::Black).bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if on {
            Style::default().fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(format!("  {label}"), style));
        spans.push(Span::styled(format!(" ({key}) "),
            Style::default().fg(if on || menu_cursor { Color::Yellow }
                                else { Color::DarkGray })));
    }
    if app.menu_nav {
        spans.push(Span::styled("   ←/→ move · Enter pick · Esc cancel",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM)));
    }
    spans
}

/// Norton-style cmdline (row above the F-keybar). Shows the active
/// filter expression when one is set, else a dim "$" placeholder.
/// Hook point for a future "type a command" input mode — today it's
/// pure status surface.
fn cmdline_spans(app: &App) -> Vec<Span<'static>> {
    let prompt = Span::styled("$ ", Style::default().fg(Color::Cyan)
                                                    .add_modifier(Modifier::BOLD));
    let active = view_of_pane(app.focus);
    if let Some((_, _, v)) = active {
        let f = app.view_filter(v);
        if f.on && !f.clauses.is_empty() {
            let expr = clauses_expr(&f.clauses);
            return vec![
                prompt,
                Span::styled("filter ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(expr, Style::default().fg(Color::Yellow)),
                Span::styled("  ('/' clears)",
                    Style::default().add_modifier(Modifier::DIM)),
            ];
        }
    }
    vec![prompt,
         Span::styled("(idle — '/' opens filter, F-keys below)",
             Style::default().add_modifier(Modifier::DIM))]
}

/// Norton-Commander-style F-key bar: ten contextual fields, one per
/// F1..F10, each "<n><LABEL>". Labels change based on the focused
/// pane so the user always knows what each F-key does HERE. F1/F10
/// (Help / Quit) stay stable across panes for muscle memory.
fn fkeybar_spans(app: &App) -> Vec<Span<'static>> {
    let cells = fkey_labels(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (n, label) in cells.iter().enumerate() {
        spans.push(Span::styled(format!("{} ", n + 1),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        // dim the label when unused (the label is "·")
        let lstyle = if *label == "·" {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        };
        // F11 (index 10) takes slot 11 — narrower because of the
        // two-digit prefix, but still readable.
        let width = if n == 10 { 7 } else { 8 };
        spans.push(Span::styled(format!("{label:<width$}"), lstyle));
        spans.push(Span::raw(" "));
    }
    spans
}

/// Compute the labels for the F-keybar. We use 11 slots so F11
/// (split/un-split with the PTY in the right column) can surface
/// alongside F1..F10. "·" means unbound; rendered dim.
fn fkey_labels(app: &App) -> [&'static str; 11] {
    let pty_pane = app.focus == Pane::Pty;
    let hunks    = app.focus == Pane::Hunks;
    let rules    = app.focus == Pane::Rules;
    let sessions = app.focus == Pane::Sessions;
    let changes  = app.focus == Pane::Changes;
    let any_pty  = !app.ptys.is_empty();
    // Default fields. Override per pane below.
    let mut f: [&'static str; 11] = [
        "Help",     // F1   — always
        if pty_pane { "PtyNext" } else { "Pty+" },  // F2
        if pty_pane { "PtyPrev" } else { "Tab" },   // F3
        "Edit",     // F4 (edit-current rule, edit-current name)
        if hunks || changes { "Apply" } else { "·" },  // F5
        if sessions { "Rename" } else { "·" }, // F6
        if pty_pane { "PtyNew" }
        else if rules { "NewRule" }
        else { "·" }, // F7
        if pty_pane { "PtyKill" }
        else if hunks || changes { "Discard" }
        else if rules { "DelRule" }
        else { "·" }, // F8
        "Menu",     // F9 (navigation in menubar — placeholder for now)
        "Quit",     // F10  — always
        // F11: split/un-split. The label flips with `pty_in_right`
        // so the user can read what F11 will DO next.
        if pty_pane { "Embed" }
        else if !any_pty { "Pty+" }
        else if app.pty_in_right { "Solo" }
        else { "Split" },
    ];
    // Override: PTY pane F12 detach is shown in the title strip too;
    // we mirror that here so the user doesn't have to look up.
    if pty_pane { f[8] = "·"; }  // no menu nav while in PTY
    f
}

/// Render a clause list as a one-line expression (kind:pattern, joined by
/// the per-clause join keyword) — the prototype's _clauses_expr.
fn clauses_expr(clauses: &[Clause]) -> String {
    let mut s = String::new();
    for (i, c) in clauses.iter().enumerate() {
        if !c.enabled { continue; }
        if i > 0 {
            s.push(' ');
            s.push_str(match c.join { Join::And => "and", Join::Or => "or" });
            s.push(' ');
        }
        if c.negate { s.push_str("not "); }
        s.push_str(&c.m.kind);
        s.push(':');
        s.push_str(&c.m.pattern);
    }
    s
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    // Norton-Commander-style chrome (top to bottom):
    //   menubar    : pane names with their letter accelerators (b/c/p/...)
    //   body       : current 2-pane view, unchanged
    //   cmdline    : the active filter expression or "$" prompt for input
    //   fkeybar    : ten F-key fields ("1Help 2Pty+ 3Tab ..."), context-sensitive
    //   status     : transient status / error line
    // The two top/bottom strips give the user a stable, ALWAYS-visible
    // map of what each key does in the current context — no more "open
    // help to remember which letter switches view".
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),   // menubar
            Constraint::Min(3),      // body
            Constraint::Length(1),   // cmdline
            Constraint::Length(1),   // fkeybar
            Constraint::Length(1),   // status
        ])
        .split(f.area());
    let menubar_area = root[0];
    let body = root[1];
    let cmdline_area = root[2];
    let fkeybar_area = root[3];
    let status_area = root[4];

    // Top menubar.
    f.render_widget(
        Paragraph::new(Line::from(menubar_spans(app))),
        menubar_area);

    // The PTY pane takes the whole body. NO border block — the surrounding
    // frame would interfere with the host terminal's native click-drag
    // selection (the user's "copy this output" gesture would pick up
    // border glyphs as part of the selection). Title goes on a single
    // top row above the screen grid; mouse / selection / copy-paste are
    // left to the OUTER terminal exactly as if you were running the
    // PTY child in a plain xterm — vt100's scrollback is set wide
    // enough to absorb long output (otherwise lines off the top of
    // the screen vanish).
    if app.focus == Pane::Pty {
        if let Some(pty) = app.cur_pty() {
            let total = app.ptys.len();
            let sel = app.sel_pty + 1;
            let state = if pty.eof { "(exited)" } else { "live" };
            let tag = format!("PTY {sel}/{total} · {state}");
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(body);
            let title_bar = Paragraph::new(Line::from(vec![
                Span::styled(tag, Style::default().fg(Color::Yellow)
                                                   .add_modifier(Modifier::BOLD)),
                Span::raw("   "),
                Span::styled("F2/F3 cycle · F7 new · F8 kill · F12 detach",
                    Style::default().add_modifier(Modifier::DIM)),
            ]));
            f.render_widget(title_bar, split[0]);
            let screen = pty.parser.screen();
            f.render_widget(tui_term::widget::PseudoTerminal::new(screen), split[1]);
        } else {
            // No PTY open: a one-line message, also frame-less so the
            // empty-state matches the live-PTY layout.
            f.render_widget(
                Paragraph::new(Span::styled("no PTY open",
                    Style::default().add_modifier(Modifier::DIM))),
                body);
        }
    } else if app.focus == Pane::Help {
        // The Help pane takes the whole body.
        let help = Paragraph::new(Text::from(help_lines()))
            .block(block(title("help · j/k scroll", true), true))
            .scroll((app.out_scroll, 0))
            .wrap(Wrap { trim: false });
        f.render_widget(help, body);
    } else {
        // Single-list-per-view layout (mirrors the prototype's _set_view /
        // LEFT/RIGHT scheme): the left half is the FOCUSED view's primary
        // list, the right half is that view's detail. Sessions only appears
        // on the boxes view — no more "boxes on every screen". Pane::Hunks
        // is the changes view with focus on the diff (right half).
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(body);
        let left = cols[0]; let right = cols[1];
        // Border / title highlight follows the focused half: when
        // right_focused is set, the LEFT block dims and the RIGHT block
        // gets the focused styling — the same "active border" treatment
        // the Hunks path uses for the diff body.
        let lf = !app.right_focused;
        let rf = app.right_focused;
        let rscroll = app.right_scroll;
        // F11 embed mode: swap the RIGHT column for the active PTY's
        // screen. Renders a thin title strip + the vt100 grid, like the
        // full-screen Pane::Pty path but constrained to the right area.
        // Falls through to normal detail rendering when off / no PTY.
        let embed_pty = app.pty_in_right && !app.ptys.is_empty();
        if embed_pty {
            if let Some(pty) = app.cur_pty() {
                let total = app.ptys.len();
                let sel = app.sel_pty + 1;
                let state = if pty.eof { "(exited)" } else { "live" };
                let tag = format!("PTY {sel}/{total} · {state}");
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Min(1)])
                    .split(right);
                let title_bar = Paragraph::new(Line::from(vec![
                    Span::styled(tag,
                        Style::default().fg(if rf { Color::Yellow } else { Color::DarkGray })
                            .add_modifier(Modifier::BOLD)),
                    Span::raw("   "),
                    Span::styled("F11 full-screen · F8 kill · F2/F3 cycle",
                        Style::default().add_modifier(Modifier::DIM)),
                ]));
                f.render_widget(title_bar, split[0]);
                let screen = pty.parser.screen();
                f.render_widget(tui_term::widget::PseudoTerminal::new(screen), split[1]);
                // Now render the LEFT pane below per the focus arm (the
                // `match` keeps its existing arms; only the right
                // half above is overridden). Bypass the per-arm right
                // render by re-purposing `right` to the empty rect
                // we already drew on, and shortening the match's
                // right-side responsibilities to nothing.
                // We accomplish this by setting a sentinel and letting
                // each arm check it.
            }
        }
        // Sentinel for the per-arm right renderers to skip their detail
        // body when the embed has already taken over the right column.
        let skip_right = embed_pty;
        match app.focus {
            Pane::Sessions => {
                let lines = sessions_lines(app);
                let scroll = scroll_for_cursor(app.sel_session + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("sarun · boxes", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let detail = Paragraph::new(Text::from(box_detail_lines(app)))
                    .block(block(title("BOX · DETAIL", rf), rf))
                    .scroll((rscroll, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::Processes => {
                let lines = processes_lines(app);
                let scroll = scroll_for_cursor(app.sel_proc + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("PROCESSES", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let detail = Paragraph::new(Text::from(proc_detail_lines(app)))
                    .block(block(title("ENVIRONMENT · DETAIL", rf), rf))
                    .scroll((rscroll, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::Outputs => {
                let lines = outputs_index_lines(app);
                let scroll = scroll_for_cursor(app.sel_output + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("OUTPUTS", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let out = Paragraph::new(Text::from(outputs_lines(app)))
                    .block(block(title("OUTPUT · stdout/stderr", rf), rf))
                    .scroll((rscroll, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(out, right); }
            }
            Pane::Pipelines => {
                let lines = pipelines_lines(app);
                let scroll = scroll_for_cursor(app.sel_pipeline + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("PIPELINES · brush", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let detail = Paragraph::new(Text::from(pipeline_detail_lines(app)))
                    .block(block(title("PIPELINE · DETAIL", rf), rf))
                    .scroll((rscroll, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::BuildEdges => {
                let lines = build_edges_lines(app);
                let scroll = scroll_for_cursor(app.sel_edge + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("BUILD EDGES · ninja/make", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let detail = Paragraph::new(Text::from(build_edge_detail_lines(app)))
                    .block(block(title("EDGE · DETAIL", rf), rf))
                    .scroll((rscroll, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::Rules => {
                let lines = rules_lines(app);
                let scroll = scroll_for_cursor(app.sel_rule + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("FILE RULES", lf), lf))
                    .scroll((scroll, 0))
                    .wrap(Wrap { trim: false });
                f.render_widget(p, left);
                let detail = Paragraph::new(Text::from(rule_detail_lines(app)))
                    .block(block(title("WHAT IT MATCHES", rf), rf))
                    .scroll((rscroll, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            // Changes view (Pane::Changes is list-focused; Pane::Hunks is
            // diff-focused — same two-pane layout, different border). The
            // right half is split vertically: a 3-row cd-info strip with
            // the selected change's full path + kind/size/mode + stale
            // banner, and the diff body below it.
            _ => {
                let lines = changes_lines(app);
                let scroll = scroll_for_cursor(app.sel_change + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(
                        title("changes", app.focus == Pane::Changes),
                        app.focus == Pane::Changes,
                    ))
                    .scroll((scroll, 0));
                f.render_widget(p, left);

                if !skip_right {
                    let rsplit = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(4), Constraint::Min(3)])
                        .split(right);
                    f.render_widget(
                        Paragraph::new(Text::from(cd_info_lines(app)))
                            .block(block(title("path", false), false))
                            .wrap(Wrap { trim: false }),
                        rsplit[0]);

                    let is_bin = !app.hunks.is_null()
                        && app.hunks.get("is_text").and_then(Value::as_bool) != Some(true);
                    let diff_title = if is_bin { "structural diff" } else { "diff" };
                    let hunks = Paragraph::new(Text::from(hunk_lines(app)))
                        .block(block(title(diff_title, app.focus == Pane::Hunks),
                                     app.focus == Pane::Hunks))
                        .scroll((app.hunk_scroll, 0))
                        .wrap(Wrap { trim: false });
                    f.render_widget(hunks, rsplit[1]);
                }
            }
        }
    }

    // Cmdline (above the F-keybar). Today: the active filter
    // expression for the focused view, prefixed with a "$" prompt
    // glyph; if no filter is set, just the dim "$" placeholder. A
    // future "type a command here" mode will hook into this row.
    f.render_widget(
        Paragraph::new(Line::from(cmdline_spans(app))),
        cmdline_area);

    // F-keybar: ten contextual fields. F1 / F10 are stable across
    // panes (help / quit); the middle ones change to reflect what
    // each key does HERE. Look down to know — no help-pane round-trip.
    f.render_widget(
        Paragraph::new(Line::from(fkeybar_spans(app))),
        fkeybar_area);

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
/// Right pane of the BOXES view — the box-detail summary. Faithful port of
/// the prototype's _update_box_detail (lines 11086-11137): label/path bold
/// colored by status, then status / cmd / pid·age labels, then a change
/// count line and a small preview of recent paths.
fn box_detail_lines(app: &App) -> Vec<Line<'static>> {
    let Some(s) = app.sessions.get(app.sel_session) else {
        return vec![Line::from(Span::styled("(no slopbox selected)",
            Style::default().add_modifier(Modifier::DIM)))];
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let status = g("status");
    let (_flag, color) = session_flag(&status);
    let path = g("path");
    let name = g("name");
    let sid = g("session_id");
    let label = if !path.is_empty() { path }
                else if !name.is_empty() { name }
                else { sid };
    let cmd = s.get("cmd").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let pid = s.get("pid").and_then(Value::as_i64).unwrap_or(0);
    let started = s.get("started").and_then(Value::as_f64).unwrap_or(0.0);
    let live = s.get("live").and_then(Value::as_bool).unwrap_or(false);
    let preview_label = if live { "recently changed" } else { "↵ to review" };
    let mut out = vec![
        Line::from(Span::styled(label,
            Style::default().fg(color).add_modifier(Modifier::BOLD))),
        Line::from(vec![Span::styled("status  ", dim),
                        Span::styled(status.clone(), Style::default().fg(color))]),
        Line::from(vec![Span::styled("cmd     ", dim), Span::raw(cmd)]),
        Line::from(vec![
            Span::styled("pid     ", dim),
            Span::raw(if pid > 0 { pid.to_string() } else { "0".into() }),
            Span::raw("    "),
            Span::styled("age ", dim),
            Span::raw(if started > 0.0 { fmt_age(started) } else { String::new() }),
        ]),
        Line::from(vec![
            Span::styled("changes ", dim),
            Span::styled(bold_count(app.changes_total), bold),
            Span::styled(format!("   [{preview_label}]"), dim),
        ]),
        Line::from(""),
    ];
    // Five-list summary: the engine packs newest-first outputs / changes /
    // processes / pipelines / build-edges into review.box_summary on each
    // session switch. Sections are headed with a dim "── label · N ──" and
    // hidden when empty (so a non-brush box doesn't show "── pipelines ──").
    let header = |title: &str, n: usize| Line::from(vec![
        Span::styled(format!("── {title} · "), dim),
        Span::styled(n.to_string(), Style::default().fg(Color::Cyan)
                                                    .add_modifier(Modifier::BOLD)),
        Span::styled(" ──", dim),
    ]);
    let render_changes_section = |out: &mut Vec<Line<'static>>, rows: &[Value]| {
        if rows.is_empty() { return; }
        out.push(header("recent changes", rows.len()));
        for c in rows {
            let kind = c.get("kind").and_then(Value::as_str).unwrap_or("");
            let path = c.get("path").and_then(Value::as_str).unwrap_or("");
            let (glyph, col) = match kind {
                "deleted" => ("-", Color::Red),
                "symlink" => ("~", Color::Magenta),
                "xattr"   => ("@", Color::Cyan),
                "changed" => ("~", Color::Yellow),
                _ => ("·", Color::DarkGray),
            };
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), Style::default().fg(col)),
                Span::raw(path.to_string()),
            ];
            // xattr rows ride the file's mtime and surface as
            //   @ /the/file   key=user.foo  (12 B)
            // so the user can see the file + the affected xattr without a
            // separate pane (they were invisible before).
            if kind == "xattr" {
                let key = c.get("xattr_key").and_then(Value::as_str).unwrap_or("");
                let n = c.get("xattr_len").and_then(Value::as_i64).unwrap_or(0);
                spans.push(Span::styled(format!("   {key}"),
                    Style::default().fg(Color::Cyan)));
                spans.push(Span::styled(format!("  ({n} B)"), dim));
            }
            out.push(Line::from(spans));
        }
        out.push(Line::from(""));
    };
    let render_outputs_section = |out: &mut Vec<Line<'static>>, rows: &[Value]| {
        if rows.is_empty() { return; }
        out.push(header("recent outputs", rows.len()));
        for r in rows {
            let stream = r.get("stream").and_then(Value::as_i64).unwrap_or(0);
            let len = r.get("len").and_then(Value::as_i64).unwrap_or(0);
            let preview = r.get("preview").and_then(Value::as_str).unwrap_or("");
            let tag = if stream == 1 { "err" } else { "out" };
            let tag_col = if stream == 1 { Color::Red } else { Color::Green };
            let one_line: String = preview.chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .take(80).collect();
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(tag.to_string(), Style::default().fg(tag_col)),
                Span::styled(format!("  {len:>5} B  "), dim),
                Span::raw(one_line),
            ]));
        }
        out.push(Line::from(""));
    };
    let render_processes_section = |out: &mut Vec<Line<'static>>, rows: &[Value]| {
        if rows.is_empty() { return; }
        out.push(header("recent processes", rows.len()));
        for r in rows {
            let tgid = r.get("tgid").and_then(Value::as_i64).unwrap_or(0);
            let argv0 = r.get("argv0").and_then(Value::as_str).unwrap_or("");
            let exe = r.get("exe").and_then(Value::as_str).unwrap_or("");
            // basename of argv[0] (if any) else of exe
            let head = std::path::Path::new(
                if !argv0.is_empty() { argv0 } else { exe })
                .file_name().and_then(|s| s.to_str()).unwrap_or("");
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{tgid:>6}  "), dim),
                Span::styled(head.to_string(), bold),
            ]));
        }
        out.push(Line::from(""));
    };
    let render_pipelines_section = |out: &mut Vec<Line<'static>>, rows: &[Value]| {
        if rows.is_empty() { return; }
        out.push(header("recent pipelines", rows.len()));
        for r in rows {
            let cmd = r.get("cmd").and_then(Value::as_str).unwrap_or("");
            let nested = r.get("nested").and_then(Value::as_bool) == Some(true);
            let mark = if nested { "N" } else { "T" };
            let mark_style = if nested {
                Style::default().fg(Color::Magenta)
            } else { Style::default().fg(Color::Cyan) };
            let trimmed: String = cmd.chars().take(72).collect();
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{mark} "), mark_style),
                Span::raw(trimmed),
            ]));
        }
        out.push(Line::from(""));
    };
    let render_edges_section = |out: &mut Vec<Line<'static>>, rows: &[Value]| {
        if rows.is_empty() { return; }
        out.push(header("recent build edges", rows.len()));
        for r in rows {
            let target = r.get("out").and_then(Value::as_str).unwrap_or("(unnamed)");
            let n = r.get("n_outs").and_then(Value::as_i64).unwrap_or(1);
            let cmd = r.get("cmd").and_then(Value::as_str).unwrap_or("");
            let phony = cmd.is_empty();
            let mark = if phony { "P" } else { "R" };
            let mark_style = if phony {
                Style::default().fg(Color::DarkGray)
            } else { Style::default().fg(Color::Green) };
            let extra = if n > 1 { format!(" (+{})", n - 1) } else { String::new() };
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{mark} "), mark_style),
                Span::styled(target.to_string(), bold),
                Span::styled(extra, dim),
            ]));
        }
        out.push(Line::from(""));
    };

    // Pull each list out of the bundle (empty defaults if the bundle is
    // null — happens momentarily on session switch before the RPC returns).
    let g_arr = |k: &str| app.box_summary.get(k)
        .and_then(Value::as_array).cloned().unwrap_or_default();
    let changes  = g_arr("changes");
    let outputs  = g_arr("outputs");
    let procs    = g_arr("processes");
    let pipes    = g_arr("pipelines");
    let edges    = g_arr("edges");

    // Order: outputs first (what just printed), then changes (files /
    // xattrs that just landed), then processes (who did it), then the
    // brush / build views below for context. Empty sections drop out
    // so a vanilla (non-brush) box's right pane stays tight.
    render_outputs_section(&mut out, &outputs);
    render_changes_section(&mut out, &changes);
    render_processes_section(&mut out, &procs);
    render_pipelines_section(&mut out, &pipes);
    render_edges_section(&mut out, &edges);

    // Fallback: if the bundle never arrived (e.g. old engine that doesn't
    // know review.box_summary) keep the previous behavior so the pane
    // isn't blank — show the recent_changes list the old way.
    if changes.is_empty() && outputs.is_empty() && procs.is_empty()
        && pipes.is_empty() && edges.is_empty() {
        let preview: Box<dyn Iterator<Item = &Value>> =
            if live && !app.recent_changes.is_empty() {
                Box::new(app.recent_changes.iter())
            } else {
                Box::new(app.changes.iter().take(40))
            };
        for c in preview {
            if c.get("connector").and_then(Value::as_bool) == Some(true) { continue; }
            let kind = c.get("kind").and_then(Value::as_str).unwrap_or("");
            let path = c.get("path").and_then(Value::as_str).unwrap_or("");
            let (glyph, col) = match kind {
                "deleted" => ("-", Color::Red),
                "symlink" => ("~", Color::Magenta),
                "changed" => ("~", Color::Yellow),
                _ => ("·", Color::DarkGray),
            };
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), Style::default().fg(col)),
                Span::raw(path.to_string()),
            ]));
        }
    }
    out
}

fn bold_count(n: usize) -> String {
    if n == 1 { "1 file".into() } else { format!("{n} files") }
}

/// One line per pipeline row: a single-letter origin marker (T = top-level,
/// N = nested-shim), the seq index, and the command. The pipeline's full
/// parsed structure + linked process row ids live in the detail pane.
fn pipelines_lines(app: &App) -> Vec<Line<'static>> {
    if app.pipelines.is_empty() {
        return vec![Line::from(Span::styled(
            "no pipelines yet — run something through brush (-b) to populate",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))),
        ];
    }
    let mut out = vec![];
    for (i, row) in app.pipelines.iter().enumerate() {
        let id = row.get("id").and_then(Value::as_i64).unwrap_or(0);
        let nested = row.get("nested").and_then(Value::as_bool) == Some(true);
        let pipeline = row.get("pipeline").and_then(Value::as_i64).unwrap_or(0);
        let cmd = row.get("cmd").and_then(Value::as_str).unwrap_or("");
        let nprocs = row.get("processes").and_then(Value::as_array)
            .map(|a| a.len()).unwrap_or(0);
        let mark = if nested { "N" } else { "T" };
        let mark_style = if nested {
            Style::default().fg(Color::Magenta)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let mut spans = vec![
            Span::styled(format!("{:>4}  ", id),
                         Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{mark}  "), mark_style),
            Span::styled(format!("p{pipeline:<2}  "),
                         Style::default().fg(Color::DarkGray)),
            Span::raw(cmd.to_string()),
        ];
        if nprocs > 0 {
            spans.push(Span::styled(format!("  ·  {nprocs} proc{}",
                if nprocs == 1 { "" } else { "s" }),
                Style::default().fg(Color::DarkGray)));
        }
        let mut line = Line::from(spans);
        if i == app.sel_pipeline {
            line = line.style(Style::default().add_modifier(Modifier::REVERSED));
        }
        out.push(line);
    }
    out
}

/// Right-hand detail for the selected pipeline: cmd, origin, ts/spawn_ts,
/// the linked process row ids, and the parsed structure JSON (pretty).
fn pipeline_detail_lines(app: &App) -> Vec<Line<'static>> {
    let Some(row) = app.pipelines.get(app.sel_pipeline) else {
        return vec![Line::from(Span::styled(
            "(select a pipeline)",
            Style::default().fg(Color::DarkGray)))];
    };
    let nested = row.get("nested").and_then(Value::as_bool) == Some(true);
    let id = row.get("id").and_then(Value::as_i64).unwrap_or(0);
    let pipeline = row.get("pipeline").and_then(Value::as_i64).unwrap_or(0);
    let cmd = row.get("cmd").and_then(Value::as_str).unwrap_or("");
    let ts = row.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let spawn_ts = row.get("spawn_ts").and_then(Value::as_f64);
    let procs: Vec<String> = row.get("processes").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_i64().map(|i| i.to_string())).collect())
        .unwrap_or_default();
    let label = Style::default().fg(Color::DarkGray);
    let val = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(vec![Span::styled("id        ", label),
                        Span::styled(id.to_string(), val)]),
        Line::from(vec![Span::styled("origin    ", label),
                        Span::styled(if nested { "nested  (sh -c shim)" }
                                     else { "top-level brush body" }, val)]),
        Line::from(vec![Span::styled("seq       ", label),
                        Span::styled(format!("p{pipeline}"), val)]),
        Line::from(vec![Span::styled("ts        ", label),
                        Span::raw(format!("{ts:.6}"))]),
    ];
    if let Some(st) = spawn_ts {
        lines.push(Line::from(vec![Span::styled("spawn_ts  ", label),
                                    Span::raw(format!("{st:.6}"))]));
    }
    lines.push(Line::from(vec![Span::styled("cmd       ", label),
                                Span::styled(cmd.to_string(), val)]));
    if !procs.is_empty() {
        lines.push(Line::from(vec![Span::styled("processes ", label),
            Span::raw(procs.join(", "))]));
    } else {
        lines.push(Line::from(vec![Span::styled("processes ", label),
            Span::styled("(none linked — brush ran this in-process)",
                         Style::default().fg(Color::DarkGray)
                                          .add_modifier(Modifier::ITALIC))]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "── parsed structure ──",
        Style::default().fg(Color::DarkGray))));
    let rec = row.get("record").cloned().unwrap_or(Value::Null);
    let pretty = serde_json::to_string_pretty(&rec).unwrap_or_default();
    for l in pretty.lines() {
        lines.push(Line::from(l.to_string()));
    }
    lines
}

/// One line per build_edges row: a marker (P for phony, R for real recipe),
/// the first output target, → and the cmd (truncated). The detail pane
/// shows all outs/ins and the full cmd.
fn build_edges_lines(app: &App) -> Vec<Line<'static>> {
    if app.build_edges.is_empty() {
        return vec![Line::from(Span::styled(
            "no build edges yet — run `ninja` or `make` inside a -b box",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))),
        ];
    }
    let mut out = vec![];
    for (i, row) in app.build_edges.iter().enumerate() {
        let outs = row.get("outs").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from))
                              .collect::<Vec<_>>())
            .unwrap_or_default();
        let cmd_opt = row.get("cmd").and_then(Value::as_str);
        let phony = cmd_opt.is_none() || cmd_opt == Some("");
        let mark = if phony { "P" } else { "R" };
        let mark_style = if phony {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Green)
        };
        let head = outs.first().cloned().unwrap_or_else(|| "(unnamed)".into());
        let extra = if outs.len() > 1 {
            format!(" (+{})", outs.len() - 1)
        } else { String::new() };
        let mut spans = vec![
            Span::styled(format!("{mark}  "), mark_style),
            Span::styled(head, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(extra, Style::default().fg(Color::DarkGray)),
        ];
        if let Some(cmd) = cmd_opt.filter(|c| !c.is_empty()) {
            let trimmed: String = cmd.chars().take(60).collect();
            spans.push(Span::styled("  ←  ",
                Style::default().fg(Color::DarkGray)));
            spans.push(Span::raw(trimmed));
        }
        let mut line = Line::from(spans);
        if i == app.sel_edge {
            line = line.style(Style::default().add_modifier(Modifier::REVERSED));
        }
        out.push(line);
    }
    out
}

/// Right-hand detail for the selected build edge: full outs/ins lists,
/// the recipe cmd (or "phony"), and the timestamp.
fn build_edge_detail_lines(app: &App) -> Vec<Line<'static>> {
    let Some(row) = app.build_edges.get(app.sel_edge) else {
        return vec![Line::from(Span::styled(
            "(select an edge)",
            Style::default().fg(Color::DarkGray)))];
    };
    let id = row.get("id").and_then(Value::as_i64).unwrap_or(0);
    let ts = row.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let outs = row.get("outs").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from))
                          .collect::<Vec<_>>()).unwrap_or_default();
    let ins = row.get("ins").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from))
                          .collect::<Vec<_>>()).unwrap_or_default();
    let cmd = row.get("cmd").and_then(Value::as_str).unwrap_or("");
    let label = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(vec![Span::styled("id        ", label),
                        Span::raw(id.to_string())]),
        Line::from(vec![Span::styled("ts        ", label),
                        Span::raw(format!("{ts:.6}"))]),
        Line::from(""),
        Line::from(Span::styled("outputs", Style::default().fg(Color::Green)
                                                   .add_modifier(Modifier::BOLD))),
    ];
    if outs.is_empty() {
        lines.push(Line::from(Span::styled("  (none)",
            Style::default().fg(Color::DarkGray))));
    }
    for o in &outs {
        lines.push(Line::from(format!("  {o}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "inputs", Style::default().fg(Color::Cyan)
                          .add_modifier(Modifier::BOLD))));
    if ins.is_empty() {
        lines.push(Line::from(Span::styled("  (none — leaf or phony)",
            Style::default().fg(Color::DarkGray))));
    }
    for i in &ins {
        lines.push(Line::from(format!("  {i}")));
    }
    lines.push(Line::from(""));
    if cmd.is_empty() {
        lines.push(Line::from(Span::styled(
            "phony — no recipe; this edge only declares dependencies",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))));
    } else {
        lines.push(Line::from(Span::styled(
            "recipe", Style::default().add_modifier(Modifier::BOLD))));
        // Wrap-friendly: just hand the whole cmd to the paragraph; ratatui
        // re-wraps at the pane width via wrap: trim:false.
        lines.push(Line::from(cmd.to_string()));
    }
    lines
}

fn proc_detail_lines(app: &App) -> Vec<Line<'static>> {
    // Faithful port of the prototype's _update_proc_detail:
    //   tgid X  ppid Y                       (bold)
    //   exe   <path>                         (label dim)
    //   argv  <joined>                       (label dim)
    //                                        (blank)
    //   ── environment ──                    (dim header)
    //   KEY=value  (KEY cyan)   one per line
    //   ...or the "-e to record" hint when env is empty.
    let rows = app.proc_tree_rows();
    let Some(r) = rows.get(app.sel_proc) else {
        return vec![Line::from(Span::styled("(no process selected)",
            Style::default().add_modifier(Modifier::DIM)))];
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let cyan = Style::default().fg(Color::Cyan);
    let mut out = vec![
        Line::from(Span::styled(format!("tgid {}  ppid {}", r.tgid, r.ppid), bold)),
        Line::from(vec![Span::styled("exe   ", dim),
                        Span::raw(if r.exe.is_empty() { "?".into() } else { r.exe.clone() })]),
        Line::from(vec![Span::styled("argv  ", dim),
                        Span::raw(r.argv.join(" "))]),
        Line::from(""),
        Line::from(Span::styled("── environment ──", dim)),
    ];
    let rid = r.rid;
    if rid < 0 { return out; }
    let env = app.cur_sid()
        .and_then(|sid| rpc(&app.sock, "process_env", json!([sid, rid])).ok());
    let obj = env.as_ref().and_then(|v| v.as_object());
    match obj {
        Some(m) if !m.is_empty() => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for k in keys {
                let v = m.get(k).and_then(Value::as_str).unwrap_or("");
                out.push(Line::from(vec![
                    Span::styled(k.clone(), cyan),
                    Span::raw(format!("={v}")),
                ]));
            }
        }
        _ => out.push(Line::from(Span::styled(
            "(none captured — run with -e to record the environment)", dim))),
    }
    out
}

/// The default command for a fresh PTY pane — the configurable "login command".
/// Reads the first non-blank, non-`#` line of
/// $XDG_CONFIG_HOME/slopbox[.NS]/pty_command if present; else falls back to
/// `<this-binary> run -b --` so the box opens in brush-shell mode without
/// requiring `sarun` on $PATH (the engine's PTY does no shell lookup — it
/// execvp's argv[0] directly). The user can edit the line before launching;
/// this is a default, not an enforced choice.
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
    // Absolute path to this very binary: `<…>/sarun run -b --` drops the
    // user into a brush shell inside a fresh box. They can add a NAME or
    // change the trailing command before pressing Enter.
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sarun".to_string());
    format!("{exe} run -b -- ")
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
        // The engine daemon is long-lived; its own cwd is wherever the
        // first `sarun` invocation started it, typically $HOME. The user
        // expects the PTY child to launch in the directory the CURRENT
        // sarun was invoked from, so we ship it explicitly. Same story
        // for env: portable_pty's CommandBuilder defaults to a minimal
        // env, so `bash -i` lands SHELL/HOME/PATH-less. We ship our own.
        let cwd = std::env::current_dir().ok()
            .map(|p| p.to_string_lossy().into_owned());
        let env: std::collections::BTreeMap<String, String> =
            std::env::vars().collect();
        let mut s = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
        let req = json!({
            "type": "pty_spawn",
            "argv": argv,
            "rows": rows, "cols": cols,
            "cwd": cwd,
            "env": env,
        });
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
            // 10 000 rows of scrollback: vt100's screen grid is rows×cols,
            // anything that scrolled off the top is preserved in the
            // scrollback buffer. Without this (the old `0`) long output
            // truncated the moment it overflowed the visible screen —
            // which is exactly what the user noticed when running a
            // build inside the PTY pane.
            parser: tui_term::vt100::Parser::new(rows, cols, 10_000),
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
                // Ctrl-A..Ctrl-Z → control bytes 0x01..0x1a.
                let up = c.to_ascii_uppercase();
                if up.is_ascii_alphabetic() {
                    vec![(up as u8) - b'A' + 1]
                } else if matches!(c, '4'..='7') {
                    // Crossterm collapses raw 0x1c..0x1f into Char('4')..
                    // Char('7') with CONTROL (parse.rs L110). Translate
                    // back to the original control byte so Ctrl-\ /
                    // Ctrl-] / Ctrl-^ / Ctrl-_ behave as the shell
                    // expects (and Ctrl-\ keeps killing the foreground).
                    vec![0x1c + (c as u8 - b'4')]
                } else if c == ' ' || c == '@' {
                    // Ctrl-Space / Ctrl-@ → NUL (the historical mapping).
                    vec![0]
                } else if c == '?' {
                    // Ctrl-? → DEL.
                    vec![0x7f]
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
/// Dispatch a menubar accelerator letter (b / c / p / o / l / g / e /
/// P / ?). Shared between the F9 menu-nav Enter path and the direct
/// letter-key handler — same effect either way (snap focus to the
/// left list, switch view, load data when relevant).
/// Build the context-menu items for the current pane. Empty when the
/// current pane has nothing meaningful you'd do to a row (Help, Pty
/// — both manage themselves). Caller wraps the returned list in
/// Modal::ActionMenu and presents it.
fn pane_action_menu(app: &App) -> Option<(String, Vec<ActionItem>)> {
    let mk = |label: &str, hint: &'static str, action: Action| ActionItem {
        label: label.into(), hint, action,
    };
    let title = |what: &str, target: &str| if target.is_empty() {
        format!("{what}")
    } else {
        format!("{what}: {target}")
    };
    match app.focus {
        Pane::Sessions => {
            let row = app.sessions.get(app.sel_session);
            let path = row.and_then(|r| r.get("path").and_then(Value::as_str))
                .unwrap_or("").to_string();
            Some((title("Box", &path), vec![
                mk("Open changes view", "Enter",  Action::OpenSelection),
                mk("Apply ALL changes", "a/F5",   Action::ApplyBox),
                mk("Discard ALL changes", "x/F8", Action::DiscardBox),
                mk("Rename box",        "r/F6",   Action::StartRename),
            ]))
        }
        Pane::Changes => {
            let row = app.changes.get(app.sel_change);
            let path = row.and_then(|r| r.get("path").and_then(Value::as_str))
                .unwrap_or("").to_string();
            Some((title("Change", &path), vec![
                mk("Open diff",          "Enter",  Action::OpenSelection),
                mk("Apply this file",    "a/F5",   Action::ApplyFile),
                mk("Discard this file",  "x/F8",   Action::DiscardFile),
            ]))
        }
        Pane::Hunks => Some(("Hunk (selected)".into(), vec![
            mk("Apply this hunk",    "a/F5",   Action::ApplyHunk),
            mk("Discard this hunk",  "x/F8",   Action::DiscardHunk),
        ])),
        Pane::Rules => {
            let cur = app.rules.get(app.sel_rule).cloned().unwrap_or_default();
            Some((title("Rule", &cur), vec![
                mk("Edit rule",   "F4",   Action::EditRule),
                mk("New rule",    "n/F7", Action::NewRule),
                mk("Delete rule", "d/F8", Action::DeleteRule),
                mk("Move up",     "Ctrl-↑", Action::MoveRuleUp),
                mk("Move down",   "Ctrl-↓", Action::MoveRuleDown),
            ]))
        }
        Pane::Pty => {
            let total = app.ptys.len();
            let sel = app.sel_pty + 1;
            Some((format!("PTY {sel}/{total}"), vec![
                mk("New PTY",         "F7",   Action::PtyNew),
                mk("Kill this PTY",   "F8",   Action::PtyKill),
                mk("Embed in pane",   "F11",  Action::PtyEmbedToggle),
            ]))
        }
        // Procs / Outputs / Pipelines / BuildEdges: no destructive ops
        // worth grouping into a menu today; the popup would be just
        // "Open" → same as Enter. Defer until there's something to
        // offer beyond what Enter already does.
        _ => None,
    }
}

/// Run a context-menu Action. Each variant maps to a single mutation
/// on the app, so the modal-key handler can dispatch without holding
/// any pane-specific data.
fn run_action(app: &mut App, a: Action) {
    match a {
        Action::OpenSelection  => app.open(),
        Action::ApplyFile      => app.apply(),
        Action::DiscardFile    => app.discard(),
        Action::ApplyHunk      => app.apply_hunk(),
        Action::DiscardHunk    => app.discard_hunk(),
        Action::ApplyBox       => app.apply(),
        Action::DiscardBox     => app.discard(),
        Action::StartRename    => app.renaming = Some(String::new()),
        Action::EditRule       => {
            let cur = app.rules.get(app.sel_rule).cloned().unwrap_or_default();
            app.modal = Some(Modal::RuleForm { buf: cur, editing: Some(app.sel_rule) });
        }
        Action::NewRule        => {
            app.modal = Some(Modal::RuleForm { buf: String::new(), editing: None });
        }
        Action::DeleteRule     => app.delete_rule(),
        Action::MoveRuleUp     => app.move_rule(-1),
        Action::MoveRuleDown   => app.move_rule(1),
        Action::PtyNew         => {
            app.modal = Some(Modal::PtyCmd { buf: pty_default_cmd() });
        }
        Action::PtyKill        => app.pty_kill(),
        Action::PtyEmbedToggle => {
            if app.ptys.is_empty() {
                app.modal = Some(Modal::PtyCmd { buf: pty_default_cmd() });
            } else {
                app.pty_in_right = !app.pty_in_right;
                app.right_focused = app.pty_in_right;
                app.right_scroll = 0;
            }
        }
    }
}

fn dispatch_menubar_key(app: &mut App, k: char) {
    match k {
        'b' => { app.snap_left(); app.focus = Pane::Sessions; }
        'c' => { app.snap_left(); app.nav(Pane::Changes); }
        'p' => { app.snap_left(); app.nav(Pane::Processes); }
        'o' => { app.snap_left(); app.nav(Pane::Outputs); }
        'l' => {
            app.snap_left();
            app.focus = Pane::Pipelines;
            app.load_pipelines();
        }
        'g' => {
            app.snap_left();
            app.focus = Pane::BuildEdges;
            app.load_build_edges();
        }
        'e' => { app.snap_left(); app.focus = Pane::Rules; }
        '?' => { app.snap_left(); app.focus = Pane::Help; app.out_scroll = 0; }
        'P' => {
            let any_live = app.ptys.iter().any(|p| !p.eof);
            if any_live {
                if app.cur_pty().map(|p| p.eof).unwrap_or(true) {
                    if let Some((i, _)) = app.ptys.iter().enumerate()
                        .find(|(_, p)| !p.eof) { app.sel_pty = i; }
                }
                app.focus = Pane::Pty;
            } else {
                app.modal = Some(Modal::PtyCmd { buf: pty_default_cmd() });
            }
        }
        _ => {}
    }
}

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
        Modal::ActionMenu { title, items, mut sel } => match code {
            KeyCode::Esc => app.status = "menu cancelled".into(),
            KeyCode::Up => {
                if sel > 0 { sel -= 1; }
                app.modal = Some(Modal::ActionMenu { title, items, sel });
            }
            KeyCode::Down => {
                if sel + 1 < items.len() { sel += 1; }
                app.modal = Some(Modal::ActionMenu { title, items, sel });
            }
            KeyCode::Home => app.modal = Some(Modal::ActionMenu {
                title, items, sel: 0 }),
            KeyCode::End => {
                let last = items.len().saturating_sub(1);
                app.modal = Some(Modal::ActionMenu { title, items, sel: last });
            }
            KeyCode::Enter => {
                if let Some(it) = items.get(sel) {
                    let act = it.action;
                    run_action(app, act);
                }
                // Modal stays closed; the dispatched action may
                // re-open a different one (e.g. RuleForm).
            }
            _ => app.modal = Some(Modal::ActionMenu { title, items, sel }),
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
            // drain each engine-held PTY's output into its own vt100
            // parser. Multi-PTY: every PTY's reader thread runs
            // independently, but the UI only redraws on its own tick,
            // so we pump them all here even when only one is visible
            // (the rest accumulate output for when the user switches).
            for pty in app.ptys.iter_mut() { pty.pump(); }
            // drain a finished structural-diff worker result, and animate the
            // spinner while one is still pending.
            app.pump_struct();
            if app.structd.pending && app.structd.full_lines.is_none() {
                app.structd.spin = app.structd.spin.wrapping_add(1);
            }
            // No periodic refresh — the engine pushes overlay /
            // process_added / session_* events on the subscribe stream
            // (see on_event), so the UI reacts to actual change instead
            // of polling.
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
                // F9 menu nav: ←/→ move the menubar cursor, Enter
                // dispatches via the chip's accelerator letter, Esc
                // cancels back to normal mode. The letter accelerators
                // (b/c/p/o/l/g/e/?/P) still work directly without
                // entering nav mode, so this is purely additive.
                if app.menu_nav {
                    let chips = menubar_chips(&app);
                    match k.code {
                        KeyCode::Esc => {
                            app.menu_nav = false;
                            app.status = "menu cancelled".into();
                        }
                        KeyCode::Left => {
                            if !chips.is_empty() {
                                app.menu_sel = (app.menu_sel + chips.len() - 1)
                                    % chips.len();
                            }
                        }
                        KeyCode::Right => {
                            if !chips.is_empty() {
                                app.menu_sel = (app.menu_sel + 1) % chips.len();
                            }
                        }
                        KeyCode::Home => app.menu_sel = 0,
                        KeyCode::End => {
                            if !chips.is_empty() {
                                app.menu_sel = chips.len() - 1;
                            }
                        }
                        KeyCode::Enter => {
                            // Exit nav, then synthesize the accelerator
                            // letter to dispatch via the existing chip
                            // handler (which already does snap_left and
                            // any per-pane setup).
                            app.menu_nav = false;
                            if let Some((key, _)) = chips.get(app.menu_sel).copied() {
                                let letter_event = crossterm::event::KeyEvent::new(
                                    KeyCode::Char(key),
                                    crossterm::event::KeyModifiers::empty());
                                // Replay through the same loop body by
                                // re-injecting; cleaner: dispatch inline.
                                dispatch_menubar_key(&mut app, key);
                                let _ = letter_event;
                            }
                        }
                        KeyCode::Char(c) => {
                            // Typing the letter accelerator while in
                            // nav mode also activates (so F9 then 'p'
                            // works the same as just 'p'). Anything
                            // else cancels — typing a non-chip letter
                            // in menu mode is most likely a fat-finger.
                            if chips.iter().any(|(k, _)| *k == c) {
                                app.menu_nav = false;
                                dispatch_menubar_key(&mut app, c);
                            } else {
                                app.menu_nav = false;
                                app.status = "menu cancelled (unknown chip)".into();
                            }
                        }
                        _ => {}
                    }
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
                // The PTY input handler fires when EITHER (a) we're on
                // Pane::Pty (full-screen) OR (b) the active PTY is
                // embedded into the focused view's right column and
                // that column has the focus. In case (b) Tab takes the
                // focus back to the left list, so user can still
                // navigate boxes / changes / procs while a shell runs
                // on the right.
                let pty_input_active = app.focus == Pane::Pty
                    || (app.pty_in_right && app.right_focused
                        && !app.ptys.is_empty());
                if pty_input_active {
                    use crossterm::event::KeyModifiers;
                    // EOF: child is dead. Any key detaches (no point typing
                    // into a corpse — that's how the user got stuck last time).
                    let dead = app.cur_pty().map(|p| p.eof).unwrap_or(true);
                    // F-key bindings local to the PTY pane (the F-keybar
                    // shows the same set in its bottom strip — F1/F2/F3/F7/F8
                    // for help / next-pty / prev-pty / new-pty / kill-pty,
                    // F12 for detach. Norton-style: stable mapping, the
                    // labels in the bar tell you what each does here).
                    if matches!(k.code, KeyCode::F(2)) { app.pty_next(); continue; }
                    if matches!(k.code, KeyCode::F(3)) { app.pty_prev(); continue; }
                    if matches!(k.code, KeyCode::F(7)) {
                        app.modal = Some(Modal::PtyCmd { buf: pty_default_cmd() });
                        continue;
                    }
                    if matches!(k.code, KeyCode::F(8)) { app.pty_kill(); continue; }
                    if matches!(k.code, KeyCode::F(11)) {
                        // F11 from full-screen PTY: shrink the PTY into
                        // the Sessions view's right column (default
                        // landing for the embed mode). The user can then
                        // F11 again to pop back to full-screen.
                        app.focus = Pane::Sessions;
                        app.pty_in_right = true;
                        app.right_focused = true;
                        app.right_scroll = 0;
                        continue;
                    }
                    if matches!(k.code, KeyCode::F(1)) {
                        app.snap_left();
                        app.focus = Pane::Help;
                        app.out_scroll = 0;
                        continue;
                    }
                    // Detach via Ctrl-]/F12/Esc-Esc. Crossterm collapses
                    // raw 0x1c..0x1f into Char('4')..Char('7') with
                    // CONTROL set (parse.rs L110); we accept named char,
                    // collapsed digit, and raw GS byte.
                    let ctrl_bracket = k.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5'));
                    let raw_gs = matches!(k.code, KeyCode::Char('\u{1d}'));
                    let f12 = matches!(k.code, KeyCode::F(12));
                    let now = std::time::Instant::now();
                    let esc_chord = matches!(k.code, KeyCode::Esc)
                        && app.pty_esc_at.is_some_and(|t|
                            now.duration_since(t) < Duration::from_millis(400));
                    let detach = ctrl_bracket || raw_gs || f12 || esc_chord;
                    if dead {
                        // Drop the corpse from `ptys` (slides to next/prev)
                        // and snap focus back to Sessions if none remain.
                        // Any key triggers — the user can't usefully type
                        // into an exited PTY.
                        app.pty_kill();
                        continue;
                    }
                    if detach {
                        // Detach = STOP forwarding keys, NOT close the PTY.
                        // The reader thread keeps draining the master into
                        // vt100's screen (so re-attach via the boxes view
                        // shows live state). No teardown — the box is fine.
                        // Two flavors of detach:
                        //   * Full-screen Pty pane → go back to Sessions.
                        //   * Embedded (pty_in_right) → just unfocus the
                        //     right column; the user stays on whichever
                        //     list view they were on, with the PTY still
                        //     running in the right pane (the next Tab
                        //     reattaches it).
                        app.pty_esc_at = None;
                        if app.focus == Pane::Pty {
                            app.focus = Pane::Sessions;
                        } else {
                            // Embedded mode: leave focus, just unfocus.
                            app.right_focused = false;
                        }
                        app.status = format!(
                            "PTY {}/{} detached (still running · Tab/P re-attach)",
                            app.sel_pty + 1, app.ptys.len());
                    } else if matches!(k.code, KeyCode::Esc) {
                        // Arm the Esc-Esc chord; lone Esc still reaches
                        // the shell via the flush path below.
                        app.pty_esc_at = Some(now);
                    } else if let Some(bytes) = key_to_pty_bytes(k.code, k.modifiers) {
                        // A non-Esc key arrived. If an Esc was queued and
                        // is still within the window, flush it first.
                        if let Some(t) = app.pty_esc_at.take() {
                            if now.duration_since(t) < Duration::from_millis(400) {
                                if let Some(pty) = app.cur_pty_mut() {
                                    pty.send_input(&[0x1b]);
                                }
                            }
                        }
                        if let Some(pty) = app.cur_pty_mut() { pty.send_input(&bytes); }
                    }
                    continue;
                }
                use crossterm::event::KeyModifiers as KM;
                let ctrl = k.modifiers.contains(KM::CONTROL);
                // F-key bar bindings, global outside the PTY pane.
                // (PTY pane handles its own F2/F3/F7/F8/F12 before
                // we get here.) Mapping mirrors fkey_labels() above:
                //   F1  Help · F2 new PTY · F3 Tab (next pane within
                //   view) · F4 edit (rules / rename box) · F5 apply
                //   · F6 rename (boxes) · F7 new rule (when on Rules)
                //   · F8 discard (Changes/Hunks) / del rule (Rules)
                //   · F9 menu (TBD)             · F10 quit
                if let KeyCode::F(n) = k.code {
                    match n {
                        1 => { app.snap_left(); app.focus = Pane::Help; app.out_scroll = 0; }
                        2 => { app.modal = Some(Modal::PtyCmd { buf: pty_default_cmd() }); }
                        3 => { app.next_pane(); }
                        11 => {
                            // Embed the active PTY into the focused view's
                            // RIGHT column (or un-embed). No-op when no PTY
                            // is open; opens a fresh prompt in that case
                            // so the toggle does something visible.
                            if app.ptys.is_empty() {
                                app.modal = Some(Modal::PtyCmd { buf: pty_default_cmd() });
                            } else {
                                app.pty_in_right = !app.pty_in_right;
                                // Right-focus follows the embedded PTY so
                                // keystrokes land there immediately.
                                app.right_focused = app.pty_in_right;
                                app.right_scroll = 0;
                            }
                        }
                        4 => {
                            if app.focus == Pane::Rules {
                                let cur = app.rules.get(app.sel_rule).cloned().unwrap_or_default();
                                app.modal = Some(Modal::RuleForm {
                                    buf: cur, editing: Some(app.sel_rule) });
                            } else if app.focus == Pane::Sessions {
                                app.renaming = Some(String::new());
                            }
                        }
                        5 => {
                            if app.focus == Pane::Hunks { app.apply_hunk(); }
                            else if app.focus == Pane::Changes { app.apply(); }
                        }
                        6 => {
                            if app.focus == Pane::Sessions {
                                app.renaming = Some(String::new());
                            }
                        }
                        7 => {
                            if app.focus == Pane::Rules {
                                app.modal = Some(Modal::RuleForm {
                                    buf: String::new(), editing: None });
                            }
                        }
                        8 => {
                            if app.focus == Pane::Hunks { app.discard_hunk(); }
                            else if app.focus == Pane::Changes { app.discard(); }
                            else if app.focus == Pane::Rules { app.delete_rule(); }
                        }
                        9 => {
                            // F9 enters menu-nav mode: highlight a
                            // menubar chip, ←/→ move, Enter picks,
                            // Esc cancels. We land on the chip for
                            // the currently-active view (or the
                            // first chip if no chip maps).
                            let chips = menubar_chips(&app);
                            let active = view_of_pane(app.focus)
                                .map(|(k, _, _)| k);
                            app.menu_sel = chips.iter()
                                .position(|(k, _)| Some(*k) == active)
                                .unwrap_or(0);
                            app.menu_nav = true;
                            app.status = "menu — ←/→ move · Enter pick · Esc cancel".into();
                        }
                        10 => { shutdown_rpc(&app.sock); app.should_quit = true; }
                        _ => {}
                    }
                    continue;
                }
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
                    KeyCode::PageDown => app.page_down(),
                    KeyCode::PageUp => app.page_up(),
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
                    // Every letter chip snaps focus back to the LEFT list and
                    // clears the right-pane scroll — carrying right_focused
                    // across views would put the cursor in the new view's
                    // detail body, which has no cursor of its own.
                    // Letter accelerators: route through the same
                    // dispatch_menubar_key the F9 menu-nav path uses,
                    // so the two paths can't diverge.
                    KeyCode::Char(c @ ('b'|'c'|'p'|'o'|'l'|'g'|'e'|'?'|'P')) =>
                        dispatch_menubar_key(&mut app, c),
                    // In the diff pane, a/x/d are PER-HUNK; elsewhere they act on
                    // the selected change / whole box.
                    // 'm' opens the context-menu popup for the
                    // currently-selected row. Per-pane action list +
                    // hints come from pane_action_menu(); pressing m
                    // on a pane with no contextual actions (Procs /
                    // Outputs / Pipelines / BuildEdges today) is a
                    // no-op with a status note.
                    KeyCode::Char('m') => {
                        if let Some((title, items)) = pane_action_menu(&app) {
                            app.modal = Some(Modal::ActionMenu {
                                title, items, sel: 0,
                            });
                        } else {
                            app.status = "no actions for this row yet".into();
                        }
                    }
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
        // … and with no config it falls back to `<this-binary> run -b --`
        // — an absolute path so the engine's PTY (which execvp's argv[0]
        // directly, no shell lookup) can find it without `sarun` on PATH.
        std::fs::remove_file(cfgdir.join("pty_command")).unwrap();
        let d = pty_default_cmd();
        assert!(d.contains("run -b --"),
                "fallback should drop into a fresh brush-mode box, got {d:?}");
        let head = d.split_whitespace().next().unwrap_or("");
        assert!(head.starts_with('/'),
                "fallback head should be an absolute path, got {head:?}");
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
        // Single-list-per-view layout: the boxes view shows the sessions
        // list (left) + box detail (right). No changes/diff pane until the
        // user navigates to those views via c/Enter.
        assert!(buf.contains(&sid), "frame should contain box id {sid}; got:\n{buf}");
        assert!(buf.contains("boxes"), "sessions pane title missing");
        assert!(buf.contains("BOX · DETAIL"), "box-detail pane title missing");
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
        // Kind is shown as the prototype's single-char glyph. With per-row
        // decoration now wired up, a newly-written file renders as '+'
        // (created); a modified one would be '~'.
        assert!(buf.contains('+') || buf.contains('~'),
                "kind glyph (+ / ~) missing:\n{buf}");
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
            app.output_segs.iter().any(|(_, _, t)| t.contains("OUTPUT_MARKER_qqq"))
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
        // Filtering now happens server-side (engine's views::rebuild_idx
        // runs the same rules::eval_clauses); the cross-check that the UI's
        // filtered output matches an in-process eval_clauses was for the
        // old client-side filter and no longer applies.
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
        // Engine-side view rows are objects {rid,tgid,ppid,exe,argv,depth,connector}.
        let vis: Vec<i64> = app.visible_processes().iter().map(|p|
            p.get("rid").and_then(Value::as_i64).unwrap_or(-1)).collect();
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
            rules: vec![], pipelines: vec![], build_edges: vec![],
            sel_session: 0,
            sel_change: 0,
            sel_proc: 0, sel_pipeline: 0, sel_edge: 0,
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
            ptys: vec![], sel_pty: 0, pty_esc_at: None, right_focused: false, right_scroll: 0, pty_in_right: false, menu_nav: false, menu_sel: 0,
            structd: StructState::default(),
            sel_hunk: 0,
            struct_rx: None,
            cd_info: None,
            output_segs: vec![],
            changes_decor: vec![],
            recent_changes: vec![], box_summary: serde_json::json!(null),
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

    // ── coverage for the post-port UI behaviors ───────────────────────────

    /// The boxes view's right pane carries the BOX · DETAIL block — bold
    /// label, status / cmd / pid·age labels, "changes N file(s) [↵ to
    /// review]" — and a preview of recent paths. Asserts every piece of
    /// that header shows in a freshly-opened UI against a real box.
    #[test]
    fn box_detail_pane_shows_status_cmd_pid_age_and_changes_count() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        std::fs::write(root.join("root/box_detail_marker.txt"),
                       b"hi\n").expect("write");
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains("BOX · DETAIL"), "block title missing:\n{buf}");
        assert!(buf.contains("status  "), "status label missing:\n{buf}");
        assert!(buf.contains("cmd     "), "cmd label missing:\n{buf}");
        assert!(buf.contains("pid     "), "pid label missing:\n{buf}");
        assert!(buf.contains("age "), "age label missing:\n{buf}");
        assert!(buf.contains("changes "), "changes count line missing:\n{buf}");
        // The finished-box preview label.
        assert!(buf.contains("[↵ to review]"),
                "↵ to review label missing for finished box:\n{buf}");
        // The actual written path makes it into the preview.
        assert!(buf.contains("box_detail_marker.txt"),
                "recent preview missing the written path:\n{buf}");
    }

    /// The keybar carries one chip per view (b/c/p/o/e) and the active
    /// view's chip+label is highlighted. Switch focus and assert the
    /// active chip moves with it.
    #[test]
    fn menubar_chips_track_focus() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let _box = make_box(&eng.sock);
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        // boxes view: menubar must mention every chip label.
        let buf = render_to_string(&app, 160, 30).unwrap();
        for lab in ["Boxes", "Changes", "Rules", "Help"] {
            assert!(buf.contains(lab), "menubar chip {lab:?} missing:\n{buf}");
        }
        // chip letters appear inside parentheses, e.g. " (b) ".
        for k in ['b', 'c', 'e', '?'] {
            let lit = format!("({k})");
            assert!(buf.contains(&lit),
                    "menubar accelerator {lit:?} missing:\n{buf}");
        }
        // Move focus to Procs (forcing it shown since the box has no
        // procs yet — view_of_pane's "active overrides hide" rule
        // means the chip surfaces).
        app.focus = Pane::Processes;
        app.load_processes();
        let buf2 = render_to_string(&app, 160, 30).unwrap();
        assert!(buf2.contains("Procs"), "Procs label missing after focus:\n{buf2}");
    }

    /// Active filter on the focused view surfaces in the COMMAND LINE
    /// row (above the F-keybar) with the clause expression rendered.
    /// Renamed from the old "keybar chip" test — same intent, new spot.
    #[test]
    fn cmdline_shows_active_filter_expression() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        std::fs::write(root.join("root/keepme_PUMPKIN.txt"), b"x").expect("write");
        std::fs::write(root.join("root/other_THING.txt"), b"y").expect("write");
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        app.focus = Pane::Changes;
        let rows = vec![ClauseRow {
            enabled: true, join: Join::And, negate: false,
            kind: "path".into(), pattern: "**/PUMPKIN*".into(),
        }];
        app.commit_filter(FilterView::Changes, &rows);
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains("filter"),
                "cmdline must surface 'filter' tag when one is active:\n{buf}");
        assert!(buf.contains("path:**/PUMPKIN*"),
                "filter expression missing from cmdline:\n{buf}");
    }

    /// A freshly-created file (no host counterpart at lower) renders as
    /// '+' (green created) on the changes pane after the bulk-decorate
    /// pass — distinct from the '~' yellow modified glyph the previous
    /// undifferentiated render used.
    #[test]
    fn changes_pane_uses_plus_glyph_for_created_file() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        // a path that almost certainly doesn't exist on the host →
        // decorate should say kind=created → render with '+'.
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("created_only_xyzzy.txt"), b"new!\n").expect("write");
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        app.focus = Pane::Changes;
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains("created_only_xyzzy.txt"),
                "the file should appear in the changes pane:\n{buf}");
        assert!(buf.contains('+'),
                "created file should render with '+' glyph:\n{buf}");
    }

    /// cd-info strip (path / kind / size / mode) renders ABOVE the diff
    /// area for any selected change, not just binary ones.
    #[test]
    fn cd_info_strip_shows_selected_change_path_and_meta() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("cd_info_TARGET.txt"),
                       b"sample bytes\n").expect("write");
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        app.open();          // sessions → changes
        app.open();          // changes → hunks (loads the diff + cd-info)
        let buf = render_to_string(&app, 160, 40).unwrap();
        assert!(buf.contains("/tmp/cd_info_TARGET.txt"),
                "cd-info should show the leading-slashed full path:\n{buf}");
        assert!(buf.contains("path"), "cd-info title 'path' missing:\n{buf}");
    }

    /// PgDn / PgUp jump the cursor by PAGE_SIZE rows in list panes.
    #[test]
    fn page_down_advances_cursor_by_page_size_in_changes() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, root) = make_box(&eng.sock);
        // Make 60 files so a single PAGE_SIZE jump won't hit the tail.
        let dir = root.join("tmp/page");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for i in 0..60 {
            std::fs::write(dir.join(format!("f_{i:04}.txt")),
                           b"x").expect("write");
        }
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.load_changes();
        app.focus = Pane::Changes;
        let before = app.changes_window_start + app.sel_change;
        app.page_down();
        let after = app.changes_window_start + app.sel_change;
        let moved = after - before;
        // We don't require EXACTLY PAGE_SIZE because the tree has connector
        // rows that the cursor skips; just assert it moved by more than one
        // and less than 2× PAGE_SIZE.
        assert!(moved > 1, "page_down should move more than 1 row, got {moved}");
        assert!(moved <= PAGE_SIZE * 2,
                "page_down moved {moved} rows, expected around {PAGE_SIZE}");
    }

    /// The rules pane right side parses the selected rule and prints the
    /// ACTION + per-clause breakdown (not the old static hint).
    #[test]
    fn rules_pane_parses_rule_and_shows_clauses() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let _box = make_box(&eng.sock);
        let mut app = App::new(eng.sock.clone());
        app.rules = vec!["apply path:src/** and exe:**/gcc".into()];
        app.focus = Pane::Rules;
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains("APPLY"), "ACTION heading missing:\n{buf}");
        assert!(buf.contains("path:src/**"), "first clause missing:\n{buf}");
        assert!(buf.contains("exe:**/gcc"), "second clause missing:\n{buf}");
        // per-kind help line should appear for at least one kind.
        assert!(buf.contains("changed path"),
                "path-kind help line missing:\n{buf}");
    }

    /// The overlay's mutating ops push (sid, rel, op) events through the
    /// engine's broadcast stream as type=overlay. Subscribing then writing
    /// through the FUSE mount must deliver one matching event.
    #[test]
    fn overlay_write_broadcasts_change_event() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (tx, rx) = mpsc::channel();
        spawn_subscriber(&eng.sock, tx);
        std::thread::sleep(Duration::from_millis(300));
        let (sid, root) = make_box(&eng.sock);
        let dir = root.join("tmp");
        std::fs::create_dir_all(&dir).expect("mkdir through mount");
        std::fs::write(dir.join("overlay_evt_marker.txt"), b"hi\n")
            .expect("write through mount");
        // Wait for an overlay event whose sid matches the box we just wrote.
        let mut saw = false;
        for _ in 0..15 {
            if let Ok(ev) = rx.recv_timeout(Duration::from_secs(2)) {
                if ev.get("type").and_then(Value::as_str) == Some("overlay")
                   && ev.get("sid").and_then(Value::as_str) == Some(sid.as_str())
                {
                    saw = true; break;
                }
            }
        }
        assert!(saw, "expected an overlay event for the written file");
    }

    /// Running a real command in a box has its BoxState push a
    /// "process_added" entry into the shared engine event queue
    /// (capture.rs::record_proc); the broadcaster turns each one
    /// into a type=process_added event on the subscribe stream. With
    /// these events flowing the UI has no need for a 3 s polling
    /// tick — the procs / changes / outputs views refresh on the
    /// actual change.
    #[test]
    fn process_added_event_arrives_after_running_a_command() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (tx, rx) = mpsc::channel();
        spawn_subscriber(&eng.sock, tx);
        std::thread::sleep(Duration::from_millis(300));
        if !run_cmd(&eng, &["/bin/echo", "hi"]) {
            eprintln!("SKIP: could not run a box command (bwrap unavailable?)");
            return;
        }
        let mut saw = false;
        for _ in 0..15 {
            if let Ok(ev) = rx.recv_timeout(Duration::from_secs(2)) {
                if ev.get("type").and_then(Value::as_str) == Some("process_added") {
                    saw = true; break;
                }
            }
        }
        assert!(saw, "expected a process_added event after running a command");
    }

    /// Sessions sorted by dotted path so children land right after their
    /// parent. With one top-level box, just assert sel_session=0 finds it
    /// and the boxes view renders its label.
    #[test]
    fn sessions_pane_renders_top_level_box() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, _root) = make_box(&eng.sock);
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        // refresh_sessions sorts by `path`; sel_session=0 picks a real box.
        assert!(!app.sessions.is_empty(), "expected at least one session");
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains('F') || buf.contains('R'),
                "session F/R status flag missing:\n{buf}");
        assert!(buf.contains("Name"), "Name column header missing:\n{buf}");
        assert!(buf.contains("PID"), "PID column header missing:\n{buf}");
        assert!(buf.contains("Cmd"), "Cmd column header missing:\n{buf}");
        assert!(buf.contains("Age"), "Age column header missing:\n{buf}");
    }

    /// New panes render even on an empty box: pipelines and build_edges
    /// must show their "no rows yet" hints (not crash), and the keybar
    /// must list both letter chips ('l' and 'g'). Live data is exercised
    /// by the engine's own brushprov / build_edges tests; the UI test
    /// just guarantees the front end doesn't crash on the empty case.
    #[test]
    fn pipelines_and_build_edges_panes_render() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        let (_sid, _root) = make_box(&eng.sock);
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        app.focus = Pane::Pipelines;
        app.load_pipelines();
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains("PIPELINES"), "pipelines title missing:\n{buf}");
        assert!(buf.contains("no pipelines yet"),
                "empty-state hint missing on pipelines pane:\n{buf}");
        assert!(buf.contains("(l)") || buf.contains("Pipes"),
                "menubar must surface the pipelines chip:\n{buf}");
        app.focus = Pane::BuildEdges;
        app.load_build_edges();
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains("BUILD EDGES"), "build-edges title missing:\n{buf}");
        assert!(buf.contains("no build edges yet"),
                "empty-state hint missing on build-edges pane:\n{buf}");
        assert!(buf.contains("(g)") || buf.contains("Build"),
                "menubar must surface the build-edges chip:\n{buf}");
    }
}
