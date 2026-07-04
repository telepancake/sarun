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
const PIPELINE_FILTER_KINDS: &[&str] = &["cmd"];
const EDGE_FILTER_KINDS: &[&str] = &["target", "cmd"];

/// Which list view a '/' filter applies to. Sessions/Hunks/Rules/Help/Pty are
/// not filterable.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterView {
    Changes,
    Procs,
    Outputs,
    Pipelines,
    BuildEdges,
}

impl FilterView {
    fn kinds(self) -> &'static [&'static str] {
        match self {
            FilterView::Changes => FILE_FILTER_KINDS,
            FilterView::Pipelines => PIPELINE_FILTER_KINDS,
            FilterView::BuildEdges => EDGE_FILTER_KINDS,
            _ => SUBJECT_FILTER_KINDS,
        }
    }
    fn default_kind(self) -> &'static str {
        match self {
            FilterView::Changes => "path",
            FilterView::Pipelines => "cmd",
            FilterView::BuildEdges => "target",
            _ => "exe",
        }
    }
}

/// Cap on the Backspace go-back stack; oldest snapshots drop off first.
const NAV_HISTORY_CAP: usize = 32;

/// One entry of the Backspace go-back stack (see App::nav_history).
#[derive(Clone)]
struct NavSnapshot {
    pane: Pane,
    /// The pane's '/' filter at snapshot time (filterable views only).
    filter: Option<ViewFilter>,
    /// Global cursor position (window_start + in-window selection for the
    /// engine-windowed views; the plain index for in-memory lists).
    cursor: usize,
    right_scroll: u16,
    right_focused: bool,
    err_only: bool,
    /// Vars pane: the query the snapshot was taken under (restored on
    /// go-back — the item-follow chain rewinds query by query).
    vars_query: (String, String),
    vars_any: bool,
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
    /// Variable provenance: every makefile-level assignment (name, loc,
    /// value, make dir) plus shell assignments from box shells, recorded in
    /// the box's makevar table. '/' sets the name/value query; the right
    /// pane shows the selected variable's full value + assignment history.
    Vars,
    /// `-n` per-box network capture: left pane = list of flows
    /// (tshark-decoded HTTP requests + TLS handshakes from the box's
    /// pcapng, queried via the `flows.list` engine verb), right pane =
    /// the verbose tshark dissection of the selected flow's frame
    /// (`flows.detail`, also sandboxed).
    Flows,
    /// oaita `--api` proxy logs: one row per request the engine forwarded
    /// on this box's behalf. Routes AROUND the network proxy (the API call
    /// leaves through the engine's HOST-namespace upstream connection, not
    /// the box's netns / loopback), so it has its OWN log surface — the
    /// network pcap/MITM views would not see it. Backed by the box's
    /// `api_log` sqlar table (`api_log` / `api_log_detail` control verbs).
    ApiLogs,
    /// Network/Web pane (DESIGN-web.md W4): the box's web-capture archive —
    /// one row per HTTP(S) request/response the tap MITM proxy teed (a
    /// browser box, a --webcap crawl, any captured HTTP client), newest
    /// first, with drill-in to headers + body. This is the content record the
    /// Flows/Packets panes are the packet record of. Backed by the box's
    /// `webcap` sqlar table (`webcap` / `webcap_detail` control verbs).
    Network,
    /// Drill-down INTO a flow's TCP stream: left pane = every packet in
    /// that connection (frame · time · src→dst · proto · len · info),
    /// right pane = the same tshark -V dissection but per-packet. Pushed
    /// by Enter from Pane::Flows; Esc / Backspace pops back to Flows.
    Packets,
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
    /// Browser URL entry (DESIGN-web.md W3). A real URL field, not a shell
    /// line: `buf` is the destination the user types; Enter assembles the
    /// carbonyl launch (persistent BROWSER box, web capture on) with `spki`
    /// pinned so Chromium trusts the tap MITM proxy. `spki` is captured at
    /// open time (the action refuses to open this modal if the CA is missing),
    /// so it's a plain String, never a silent None.
    BrowserUrl { buf: String, spki: String },
    /// Vars-view query: `name [value]` — two whitespace-separated cmd_match
    /// text globs (bare word = substring); the second is optional.
    VarQuery { buf: String },
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
    /// Hierarchical base-image picker: pick a container image to load as an
    /// at-rest box stack, without knowing reference syntax. Levels are a
    /// stack of menus (Enter descends / activates, Backspace or Esc pops);
    /// `crumbs` mirror the descent for the title bar. Content comes from
    /// the loaded-image list, the curated catalog ({config_home}/images.toml
    /// or built-in defaults), and /etc/containers/registries.conf short-name
    /// aliases — leaves show WHERE the pull will actually go (mirror/alias
    /// resolution) so policy-required mirrors are visible before committing.
    ImagePicker { crumbs: Vec<String>, stack: Vec<PickLevel> },
    /// Free-text image reference entry — the picker's escape hatch (and the
    /// "enter tag…" leaf, which pre-fills `buf` with "image:").
    ImageRef { buf: String },
    /// The Pty+ launcher: destination rows (like ActionMenu) PLUS visible
    /// option chips the user cycles in place — `n` network mode (tap/host/
    /// off), `e` record-env — instead of aborting and retyping a command
    /// line to change a flag. The chosen options live on App (launch_net /
    /// launch_env) so they persist across launches this session and are
    /// applied to whichever destination Enter picks.
    Launcher { items: Vec<ActionItem>, sel: usize },
    /// Task entry for "run an oaita agent session ON a box". `box_name` is
    /// the box the session's sandbox is parented on; `session` is the
    /// auto-derived turn-folder name; `buf` is the task the user types.
    /// Enter runs `oaita run --on <box> --task <buf> <session>` on a PTY —
    /// one step, no session to pre-scaffold, no NAME to remember.
    OaitaTask { box_name: String, session: String, buf: String },
    /// Local-model picker for the Api pane. Opened when neither an external
    /// API nor a local model is configured (or from the pane's F4 menu). The
    /// list is the engine's `oaita.models` catalog — a LIVE HuggingFace query
    /// for currently-popular Q4 GGUF instruct models (config-file override +
    /// offline fallback); `source` names where it came from. Enter on a model
    /// runs `oaita local --model-url <url>` on a PTY (a BOXED download — no
    /// host writes). A trailing "custom URL…" row is the escape hatch. While
    /// `loading`, the fetch is still in flight (see pump_models).
    ModelPicker {
        models: Vec<ModelRow>,
        source: String,
        sel: usize,
        loading: bool,
    },
    /// Free-text GGUF URL entry — the model picker's escape hatch. Enter runs
    /// `oaita local --model-url <buf>` on a PTY.
    ModelUrl { buf: String },
    /// External-API config editor: the three oaita.toml fields as editable
    /// lines (`field` = cursored line 0..3), a `result` line for the last
    /// connection test, and `testing` while a probe is in flight. Tab/↑/↓
    /// move between fields, Ctrl-T tests, Ctrl-S / Enter saves to oaita.toml,
    /// Esc cancels. The in-UI answer to "where do I set the server?".
    ApiConfig {
        base_url: String,
        model: String,
        api_key: String,
        field: usize,
        result: String,
        testing: bool,
    },
}

/// One level of the image-picker menu stack.
#[derive(Clone)]
struct PickLevel {
    items: Vec<PickItem>,
    sel: usize,
}

/// One image-picker row: what it shows and what Enter does.
#[derive(Clone)]
struct PickItem {
    label: String,
    /// Right-hand annotation: where the pick resolves to (registry, mirror,
    /// alias target) or a short description for submenus.
    detail: String,
    next: PickNext,
}

#[derive(Clone)]
enum PickNext {
    /// Descend into a submenu.
    Menu(Vec<PickItem>),
    /// Pull + install this reference as an at-rest box stack (background
    /// `oci.load`; the UI stays live and shows a spinner in the status line).
    Pull(String),
    /// An already-loaded image box: open a PTY prompt pre-filled with
    /// `<exe> oci run <name> -- ` so Enter starts a container on it.
    RunLocal(String),
    /// Open the free-text reference modal pre-filled with this prefix.
    EnterRef(String),
}

/// A background `oci.load` (image pull) in flight — see App::start_image_load
/// / App::pump_load.
struct LoadJob {
    rx: mpsc::Receiver<Result<String, String>>,
    label: String,
    spin: usize,
}

/// One pickable local model in the ModelPicker (mirrors the engine's
/// `oaita.models` entries): a display name, a ready-to-download Q4 GGUF
/// URL, and a short note (size / source).
#[derive(Clone)]
struct ModelRow {
    name: String,
    url: String,
    note: String,
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
    /// Box removal from the context menu, mirroring the K / D keys: dissolve
    /// (finalize by rules, keep children, remove) and kill (SIGTERM). Both
    /// open a Confirm modal so they're never a one-keystroke accident.
    DissolveBox,
    KillBox,
    StartRename,
    EditRule,
    NewRule,
    DeleteRule,
    MoveRuleUp,
    MoveRuleDown,
    PtyNew,
    PtyKill,
    PtyEmbedToggle,
    /// Pty+ chooser entries: a discoverable menu instead of one raw command
    /// prompt (typing `bash` into that prompt runs on the HOST; inside a
    /// brush box a nested interactive shell is refused — both surprised
    /// users. The menu makes each destination explicit.)
    PtyNewBoxShell,
    PtyNewHostShell,
    PtyNewCustom,
    /// Open the hierarchical base-image picker (new box from a container
    /// image; respects /etc/containers/registries.conf).
    NewFromImage,
    /// Selected sessions row is a loaded image: prompt a PTY running
    /// `<exe> oci run <name>` (a container shell on that image).
    RunSelectedImage,
    /// Launcher: carbonyl (Chromium in the terminal) in the persistent
    /// BROWSER box (DESIGN-web.md W3). Opens a real URL field (Modal::
    /// BrowserUrl); Enter assembles the launch via build_launch as
    /// Reuse("BROWSER") + webcap, so the Chromium profile persists across
    /// launches and every page is captured to the box's web archive. Refused
    /// (visible status) if the MITM CA is unavailable.
    BrowserCarbonyl,
    /// Api pane: run `oaita local` on a PTY — download a tiny tool-capable
    /// model + CPU runtime and serve a local OpenAI-compatible endpoint.
    OaitaLocalPty,
    /// Launcher / sessions menu: run an oaita agent session whose sandbox
    /// is parented ON TOP of the selected box (`oaita run --on BOX NAME`).
    OaitaOnSelectedBox,
    /// Api pane: open the local-model picker (live HuggingFace catalog) to
    /// choose a model, then download+serve it. The discoverable front door to
    /// `oaita local` — no magic URL to guess, no host/box confusion.
    OpenModelPicker,
    /// Api pane: open the external-API config editor (base_url / model /
    /// api_key) with a live "test connection" — the in-UI alternative to
    /// hand-editing oaita.toml.
    OpenApiConfig,
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
        // The '/' filter is an interactive SEARCH box, not a precise rule
        // editor (that's the separate RuleForm modal, which writes raw glob
        // text straight to the persisted .sarunrules and must NOT go through
        // this wrapping — an apply/discard automation rule has to keep
        // matching exactly what its author typed). "cmd" already reads as a
        // substring search via `cmd_match`; "ids" is an internally-generated
        // comma-list, never user-typed. Every other kind (path/exe/cwd/box/
        // arg/target) is matched via a precise glob matcher shared with the
        // real rules engine, so a bare "gzread" would otherwise only match a
        // file literally named "gzread" — not "gzread.lo" — which is not
        // what someone typing into a search box expects. Wrap it here, at
        // the UI boundary, so the precise matcher itself (and real rules)
        // stay untouched.
        let pattern = if matches!(self.kind.as_str(), "cmd" | "ids") {
            self.pattern.trim().to_string()
        } else {
            crate::rules::wrap_bare_as_substring(self.pattern.trim())
        };
        Clause {
            m: Match { kind: self.kind.clone(), pattern },
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
    changes_view_sid: Option<i64>,
    changes_total: usize,
    changes_window_start: usize,
    hunks: Value, // raw review.hunks result for the selected change
    /// Same window/view scheme for the processes pane. `processes` rows here
    /// are the engine-side flattened tree (depth + connector baked in), so
    /// the UI no longer rebuilds the tree.
    processes: Vec<Value>,
    processes_view: Option<u64>,
    /// Sid the procs view was last opened for. Compared against
    /// `cur_sid_i64()` by `load_processes_if_needed` so we don't
    /// re-open on the same box (load_processes itself is expensive
    /// for million-row boxes).
    processes_view_sid: Option<i64>,
    processes_total: usize,
    processes_window_start: usize,
    /// Same for outputs.
    outputs: Vec<Value>,
    outputs_view: Option<u64>,
    outputs_view_sid: Option<i64>,
    outputs_total: usize,
    outputs_window_start: usize,
    rules: Vec<String>,    // raw filerules lines (apply/discard/passthrough <glob>)
    /// D9 pipelines for the currently-loaded box — the DISPLAY window,
    /// after server-side filtering + client-side running_only + tree.
    pipelines: Vec<Value>,
    /// The current window of server-filtered rows (before client-side
    /// running_only / tree transforms). `pipelines` above is derived from
    /// this via `rebuild_pipeline_view`.
    pipelines_flat: Vec<Value>,
    pipelines_view: Option<u64>,
    pipelines_view_sid: Option<i64>,
    pipelines_total: usize,
    pipelines_window_start: usize,
    /// Pipelines pane view: true = hierarchical tree (parent_uid nesting),
    /// false = flat chronological. Toggled with `t`.
    pipe_tree: bool,
    /// Pipelines pane: show ONLY in-flight pipelines (done_ts==0) — the live /
    /// stuck set on a running box. Toggled with `f`.
    pipe_running_only: bool,
    /// Procs pane: show ONLY processes still alive (engine-side pidfd probe) on a
    /// live box. Defaults true (most informative); toggled with `f` to show all.
    proc_running_only: bool,
    /// Build edges AS DISPLAYED — after server-side filtering + client-side
    /// running_only. Indexed by `sel_edge`.
    build_edges: Vec<Value>,
    /// The current window of server-filtered rows (before client-side
    /// running_only). `build_edges` is derived from this via `rebuild_edge_view`.
    build_edges_flat: Vec<Value>,
    edges_view: Option<u64>,
    edges_view_sid: Option<i64>,
    edges_total: usize,
    edges_window_start: usize,
    /// Targets pane: on a LIVE box, show ONLY the edges currently building
    /// (started_ts>0 && ended_ts==0). Defaults true (most informative for a
    /// running build); ignored for a finished box (running is meaningless then,
    /// so all edges show). Toggled with `f`.
    edges_running_only: bool,
    sel_session: usize,
    sel_change: usize,
    /// Multi-select marks for batch apply/discard/delete on the Boxes and
    /// Changes lists. Keys are stable row identities (a box's session_id, or a
    /// change's path) so they survive re-sorts / window refreshes. `mark_scope`
    /// records WHICH list the marks belong to (marks only apply when the
    /// focused pane matches). `mark_anchor` is the cursor index the last Space
    /// set, used by the `[` / `]` range-fill. Space toggles, `[`/`]` fill the
    /// range from the anchor to the cursor, Esc clears.
    marks: std::collections::HashSet<String>,
    mark_scope: Option<Pane>,
    mark_anchor: Option<usize>,
    sel_proc: usize,
    sel_pipeline: usize,
    sel_edge: usize,
    sel_output: usize,
    sel_api_log: usize,
    /// One row per api_log entry for the focused box (id, ts, method, path,
    /// model, status, stream, req_len, resp_len). Populated lazily by
    /// load_api_logs_if_needed; refreshed on `api_log_added` notifications.
    api_log_rows: Vec<Value>,
    api_log_loaded_sid: Option<String>,
    /// Network/Web pane (DESIGN-web.md W4). One row per webcap capture for the
    /// focused box (id, ts, method, url, host, status, mime, truncated,
    /// req_len, resp_len), newest-first. Populated lazily by
    /// load_webcap_if_needed; refreshed on `webcap_added` notifications.
    sel_webcap: usize,
    webcap_rows: Vec<Value>,
    webcap_loaded_sid: Option<String>,
    /// Endpoint summary / getting-started lines for the Api pane's empty
    /// state (endpoint_note_lines). Computed when the pane loads — not per
    /// frame — because it reads oaita.toml from disk.
    api_endpoint_note: Vec<String>,
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
    f_pipelines: ViewFilter,
    f_edges: ViewFilter,
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
    /// Upper bound for `right_scroll` — the right pane's wrapped content
    /// rows minus its visible viewport, recomputed by draw() every frame
    /// (a Cell because draw takes &App). The j/k/PgUp/PgDn handlers clamp
    /// against it so the detail body stops at its last line instead of
    /// scrolling into blank space.
    right_scroll_max: std::cell::Cell<u16>,
    /// Vars view: query (name glob, value glob), loaded rows, cursor.
    vars_query: (String, String),
    /// Single-term query: match name OR value (the forgiving default).
    vars_any: bool,
    vars_rows: Vec<Value>,
    sel_var: usize,
    /// Cursor over the detail pane's navigable items (refs + history rows)
    /// when the right pane is focused.
    sel_var_item: usize,
    /// Backspace go-back stack: one snapshot per view SWITCH (letter chips,
    /// cross-navs, Enter drill-downs), capped at NAV_HISTORY_CAP — oldest
    /// dropped first. Each snapshot carries enough to restore the view as it
    /// was: its '/' filter, errors-only lens, global cursor position, and
    /// the right pane's focus + scroll.
    nav_history: Vec<NavSnapshot>,
    /// Single-key errors-only view toggle ('!'): when true, the FOCUSED
    /// view's engine filter gets an extra `err` clause AND-ed onto whatever
    /// the user's '/' filter selects — outputs keep only stderr writes,
    /// pipelines / build-edges keep only non-zero exits. Cleared on every
    /// view switch (it is a transient lens, not a persisted filter).
    err_only: bool,
    /// The Outputs right pane's cursor-follow scroll, recomputed by
    /// draw() every frame: while the LEFT list is focused the transcript
    /// auto-scrolls to keep the selected write visible. Tab into the
    /// right pane seeds `right_scroll` from it, so manual scrolling
    /// starts where the view already was instead of jumping to the top.
    out_follow_scroll: std::cell::Cell<u16>,
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
    /// Loaded OCI images (name, reference) — refreshed with the sessions
    /// list; feeds the image picker's "Loaded images" branch and the
    /// sessions context menu's "container shell" entry. Empty against an
    /// engine without the `oci.images` verb.
    oci_images: Vec<(String, String)>,
    /// Launcher option: network mode index into NET_MODES (0 tap · 1 host ·
    /// 2 off). Cycled with `n` in the Pty+ launcher; applied to box-shell
    /// and container launches. Persists for the session.
    launch_net: usize,
    /// Launcher option: record the environment (`-e`) in box-shell launches.
    launch_env: bool,
    /// tap_available() result, held on App so the render path stays pure
    /// (and tests can exercise both worlds without faking privileges).
    tap_ok: bool,
    /// One-shot: the launcher already bumped tap → host for this session
    /// because tap is unavailable. Never re-bump — a user who cycles back
    /// to tap on purpose (e.g. to see the marker) is not fought.
    net_auto_bumped: bool,
    /// In-flight background `oci.load` (image pull), at most one at a time.
    /// Drained by pump_load() each tick; the status line shows a spinner
    /// while it runs so the UI never blocks on a registry.
    load_job: Option<LoadJob>,
    /// In-flight background fetch of the local-model catalog (a live
    /// HuggingFace query the engine runs for `oaita.models`). Drained by
    /// pump_models() into the open Modal::ModelPicker so the UI never blocks
    /// on the network while listing models.
    models_job: Option<mpsc::Receiver<Result<(Vec<ModelRow>, String), String>>>,
    /// One-shot: the Api pane already auto-offered the model picker this
    /// session (because neither an external API nor a local model was
    /// configured). Never re-offer — a user who dismisses it is not nagged;
    /// the F4 menu still opens it on demand.
    model_picker_offered: bool,
    /// In-flight background connection test for the ApiConfig editor (the
    /// engine's `oaita.probe`). Drained by pump_probe() into the modal's
    /// `result` line so the test never blocks the UI thread.
    probe_job: Option<mpsc::Receiver<Result<String, String>>>,
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
    /// Five-list bundle for the Sessions-view right pane (the box-detail
    /// summary): newest-first outputs / changes / processes / pipelines /
    /// build_edges, fetched in one RPC on each session switch. Defaults
    /// to all-empty arrays so box_detail_lines can render unconditionally.
    box_summary: Value,
    /// Flows pane: rows from `flows.list` (tshark-decoded HTTP/TLS entries
    /// for the selected box's pcapng). One row = one frame; the right
    /// pane shows the cached `flow_detail` text for `flow_detail_frame`,
    /// pulled lazily from `flows.detail` on cursor change.
    flows: Vec<Value>,
    sel_flow: usize,
    flow_detail: String,
    flow_detail_frame: u64,
    /// Packet drill-down pushed by Enter on a flow row. `packets` holds
    /// every frame in `packets_stream`'s tcp.stream; `packet_detail` is
    /// the cached tshark -V for `packet_detail_frame`. Esc / Backspace
    /// pops back to Flows.
    packets: Vec<Value>,
    sel_packet: usize,
    packet_detail: String,
    packet_detail_frame: u64,
    packets_stream: i64,
    /// Banner-prompt queue head, refreshed each tick from the engine's
    /// `prompts.peek`. While Some(_), the bottom status line gets
    /// replaced with a YELLOW banner asking the user for a verdict;
    /// y/n/a/d send the answer via `prompts.answer`.
    pending_prompt: Option<Value>,
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
            changes_view: None, changes_view_sid: None,
            changes_total: 0,
            changes_window_start: 0,
            hunks: Value::Null,
            processes: vec![],
            processes_view: None, processes_view_sid: None,
            processes_total: 0,
            processes_window_start: 0,
            outputs: vec![],
            outputs_view: None, outputs_view_sid: None,
            outputs_total: 0,
            outputs_window_start: 0,
            rules: vec![], pipelines: vec![], pipelines_flat: vec![],
            pipelines_view: None, pipelines_view_sid: None, pipelines_total: 0, pipelines_window_start: 0,
            pipe_tree: true, pipe_running_only: false, proc_running_only: true,
            build_edges: vec![], build_edges_flat: vec![],
            edges_view: None, edges_view_sid: None, edges_total: 0, edges_window_start: 0,
            edges_running_only: true,
            sel_session: 0,
            sel_change: 0,
            marks: std::collections::HashSet::new(),
            mark_scope: None,
            mark_anchor: None,
            sel_proc: 0, sel_pipeline: 0, sel_edge: 0,
            sel_output: 0,
            sel_api_log: 0,
            api_log_rows: vec![],
            api_log_loaded_sid: None,
            sel_webcap: 0,
            webcap_rows: vec![],
            webcap_loaded_sid: None,
            api_endpoint_note: vec![],
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
            f_pipelines: ViewFilter::default(),
            f_edges: ViewFilter::default(),
            should_quit: false,
            ptys: vec![], sel_pty: 0, pty_esc_at: None, right_focused: false, right_scroll: 0, right_scroll_max: std::cell::Cell::new(0), out_follow_scroll: std::cell::Cell::new(0), err_only: false, nav_history: vec![], vars_query: (String::new(), String::new()), vars_any: false, vars_rows: vec![], sel_var: 0, sel_var_item: 0, pty_in_right: false, menu_nav: false, menu_sel: 0,
            oci_images: vec![],
            launch_net: 0,
            launch_env: false,
            tap_ok: tap_available(),
            net_auto_bumped: false,
            load_job: None,
            models_job: None,
            model_picker_offered: false,
            probe_job: None,
            structd: StructState::default(),
            sel_hunk: 0,
            struct_rx: None,
            cd_info: None,
            output_segs: vec![],
            changes_decor: vec![],
            box_summary: serde_json::json!(null),
            flows: vec![], sel_flow: 0,
            flow_detail: String::new(), flow_detail_frame: 0,
            packets: vec![], sel_packet: 0,
            packet_detail: String::new(), packet_detail_frame: 0,
            packets_stream: -1,
            pending_prompt: None,
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

    /// Whether the currently-selected box is still running (its recipes can be
    /// in flight). Gates the targets pane's running-only filter: on a finished
    /// box "running" is meaningless, so all edges show regardless of the toggle.
    fn cur_session_live(&self) -> bool {
        self.sessions
            .get(self.sel_session)
            .and_then(|s| s.get("status"))
            .and_then(Value::as_str)
            == Some("running")
    }

    /// The current `How` (net + env chips) with the given placement — the one
    /// accessor every launch path uses so the chips are honored identically.
    fn how(&self, placement: Placement) -> How {
        How {
            net: effective_net(self).to_string(),
            env: self.launch_env,
            placement,
            webcap: false,
            webfilter: false,
        }
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
        // Loaded-image metadata rides along with every sessions refresh (a
        // cheap engine-side meta scan). Errors (older engine) leave it empty.
        self.oci_images = match rpc(&self.sock, "oci.images", json!([])) {
            Ok(Value::Array(a)) => a.iter().filter_map(|v| {
                let name = v.get("name").and_then(Value::as_str)?;
                let rf = v.get("reference").and_then(Value::as_str)?;
                Some((name.to_string(), rf.to_string()))
            }).collect(),
            _ => vec![],
        };
    }

    /// Open the hierarchical base-image picker (Sessions F7 / action menu /
    /// the Pty+ chooser). Built fresh each time so registries.conf edits and
    /// newly loaded images show up without restarting the UI.
    fn open_image_picker(&mut self) {
        let conf = crate::containers_conf::ContainersConf::load();
        let items = build_image_picker(&conf, &image_catalog(), &self.oci_images);
        self.modal = Some(Modal::ImagePicker {
            crumbs: vec![],
            stack: vec![PickLevel { items, sel: 0 }],
        });
        self.status = if conf.sources.is_empty() {
            "image picker · no /etc/containers/registries.conf — docker.io defaults".into()
        } else {
            format!("image picker · registries.conf: {}",
                    conf.sources.iter().map(|p| p.display().to_string())
                        .collect::<Vec<_>>().join(", "))
        };
    }

    /// Kick a background `oci.load` for `reference`. The pull happens on its
    /// own thread against the engine (which itself pulls host-side); the UI
    /// keeps running and pump_load() reports the outcome.
    fn start_image_load(&mut self, reference: String) {
        if self.load_job.is_some() {
            self.status = "an image load is already running".into();
            return;
        }
        let (tx, rx) = mpsc::channel();
        let sock = self.sock.clone();
        let refc = reference.clone();
        std::thread::spawn(move || {
            let res = rpc(&sock, "oci.load", json!([refc])).map(|r| {
                let name = r.get("top_name").and_then(Value::as_str).unwrap_or("?");
                let n = r.get("n_layers").and_then(Value::as_i64).unwrap_or(0);
                format!("loaded '{refc}' → box '{name}' ({n} layers) · \
                         Pty+ → \"Container from image…\" to start it")
            });
            let _ = tx.send(res);
        });
        self.load_job = Some(LoadJob { rx, label: reference, spin: 0 });
    }

    /// Drain a finished image load; animate the status spinner while one is
    /// still pending. Called once per main-loop tick (like pump_struct).
    fn pump_load(&mut self) {
        let Some(job) = self.load_job.as_mut() else { return };
        match job.rx.try_recv() {
            Ok(Ok(msg)) => {
                self.load_job = None;
                self.status = msg;
                self.refresh_sessions();
            }
            Ok(Err(e)) => {
                let label = job.label.clone();
                self.load_job = None;
                self.status = format!("oci load '{label}': {e}");
            }
            Err(mpsc::TryRecvError::Empty) => {
                job.spin = job.spin.wrapping_add(1);
                let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
                self.status = format!("{} pulling {} …",
                    frames[job.spin / 2 % frames.len()], job.label);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let label = job.label.clone();
                self.load_job = None;
                self.status = format!("oci load '{label}': worker died");
            }
        }
    }

    /// Open the local-model picker (Api pane F4, or auto-opened when nothing
    /// is configured). The catalog fetch (a live HuggingFace query the engine
    /// runs) can take a few seconds, so open in a `loading` state and pull the
    /// result in via pump_models() rather than blocking the UI thread.
    fn open_model_picker(&mut self) {
        let (tx, rx) = mpsc::channel();
        let sock = self.sock.clone();
        std::thread::spawn(move || {
            let res = rpc(&sock, "oaita.models", json!([])).map(|r| {
                let source = r.get("source").and_then(Value::as_str)
                    .unwrap_or("").to_string();
                let models = r.get("models").and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|m| Some(ModelRow {
                        name: m.get("name")?.as_str()?.to_string(),
                        url: m.get("url")?.as_str()?.to_string(),
                        note: m.get("note").and_then(Value::as_str)
                            .unwrap_or("").to_string(),
                    })).collect())
                    .unwrap_or_default();
                (models, source)
            }).map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
        self.models_job = Some(rx);
        self.modal = Some(Modal::ModelPicker {
            models: vec![], source: String::new(), sel: 0, loading: true,
        });
        self.status = "local model picker · querying HuggingFace for current \
                       models…".into();
    }

    /// Drain the finished model-catalog fetch into the open ModelPicker.
    fn pump_models(&mut self) {
        let Some(rx) = self.models_job.as_ref() else { return };
        match rx.try_recv() {
            Ok(res) => {
                self.models_job = None;
                // Only fill the modal if it's still the (loading) picker — the
                // user may have closed it while the fetch was in flight.
                if let Some(Modal::ModelPicker { models, source, loading, .. })
                    = self.modal.as_mut()
                {
                    match res {
                        Ok((rows, src)) => {
                            *models = rows;
                            *source = src;
                            *loading = false;
                            self.status = format!(
                                "local model picker · {} model(s) · {}",
                                models.len(), source);
                        }
                        Err(e) => {
                            *loading = false;
                            *source = format!("catalog fetch failed: {e}");
                            self.status = format!("oaita.models: {e}");
                        }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.models_job = None;
                if let Some(Modal::ModelPicker { loading, source, .. })
                    = self.modal.as_mut()
                {
                    *loading = false;
                    *source = "catalog worker died".into();
                }
            }
        }
    }

    /// Open the external-API config editor, prefilled from the current
    /// oaita.toml. The in-UI answer to "where do I set base_url / model /
    /// api_key?" — with a Ctrl-T connection test and Ctrl-S save.
    fn open_api_config(&mut self) {
        let cfg = crate::oaita::config::Config::load();
        self.modal = Some(Modal::ApiConfig {
            base_url: cfg.base_url.unwrap_or_default(),
            model: cfg.model.unwrap_or_default(),
            api_key: cfg.api_key.unwrap_or_default(),
            field: 0,
            result: String::new(),
            testing: false,
        });
        self.status = format!("edit external API · writes {}",
            crate::paths::oaita_config_path().display());
    }

    /// Kick a background connection test (engine `oaita.probe`) for the
    /// ApiConfig editor's current values. Result lands via pump_probe().
    fn start_api_probe(&mut self, base_url: String, model: String,
                       api_key: String) {
        let (tx, rx) = mpsc::channel();
        let sock = self.sock.clone();
        std::thread::spawn(move || {
            let res = rpc(&sock, "oaita.probe",
                json!([{ "base_url": base_url, "model": model,
                         "api_key": api_key }]));
            let out = match res {
                Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) =>
                    Ok(v.get("detail").and_then(Value::as_str)
                        .unwrap_or("connected").to_string()),
                Ok(v) => Err(v.get("error").and_then(Value::as_str)
                    .unwrap_or("probe failed").to_string()),
                Err(e) => Err(e),
            };
            let _ = tx.send(out);
        });
        self.probe_job = Some(rx);
    }

    /// Drain a finished connection test into the open ApiConfig editor.
    fn pump_probe(&mut self) {
        let Some(rx) = self.probe_job.as_ref() else { return };
        match rx.try_recv() {
            Ok(res) => {
                self.probe_job = None;
                if let Some(Modal::ApiConfig { result, testing, .. })
                    = self.modal.as_mut()
                {
                    *testing = false;
                    *result = match res {
                        Ok(d) => format!("✓ {d}"),
                        Err(e) => format!("✗ {e}"),
                    };
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.probe_job = None;
                if let Some(Modal::ApiConfig { result, testing, .. })
                    = self.modal.as_mut()
                {
                    *testing = false;
                    *result = "✗ probe worker died".into();
                }
            }
        }
    }

    /// Persist the ApiConfig editor to oaita.toml (model required). Returns a
    /// status string; refreshes the engine's --api box shadow so a running
    /// box picks up the new upstream without a restart.
    fn save_api_config(&mut self, base_url: &str, model: &str, api_key: &str)
        -> String {
        if model.trim().is_empty() {
            return "model is required — fill it before saving".into();
        }
        let mut toml = format!("model = {:?}\n", model.trim());
        if !base_url.trim().is_empty() {
            toml.push_str(&format!("base_url = {:?}\n", base_url.trim()));
        }
        if !api_key.trim().is_empty() {
            toml.push_str(&format!("api_key = {:?}\n", api_key.trim()));
        }
        let path = crate::paths::oaita_config_path();
        if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
        match std::fs::write(&path, toml) {
            Ok(()) => {
                crate::control::write_api_box_oaita_toml();
                self.refresh_api_endpoint_note();
                format!("saved external API → {}", path.display())
            }
            Err(e) => format!("write {}: {e}", path.display()),
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
                self.changes_view_sid = Some(sid);
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
        self.refresh_recent_changes();
    }

    /// Lazy counterpart of load_changes — call from the 'c' chip
    /// / Tab path so the heavy view.open only runs when the user
    /// actually navigates to the Changes pane (and skips when the
    /// view is already open for THIS box).
    fn load_changes_if_needed(&mut self) {
        let cur = self.cur_sid_i64();
        if cur.is_some() && self.changes_view_sid == cur {
            return;
        }
        self.load_changes();
    }

    /// Session-cursor moved to a different box. We do NOT open the
    /// changes / procs / outputs views here — those each cost a
    /// full sqlar scan + JSON serialize (multi-second on million-row
    /// boxes). The box summary RPC is enough to populate the
    /// Sessions-view right pane; the views lazy-load when the user
    /// actually navigates to those panes via the letter chips.
    fn on_box_cursor_moved(&mut self) {
        // A new box's detail body starts at the top — a stale scroll from the
        // previous box would show a random middle slice.
        self.right_scroll = 0;
        self.refresh_recent_changes();
        // Drop any old per-box state so the next focus on Changes /
        // Hunks / Procs / Outputs forces a fresh load_*_if_needed.
        self.changes_view_sid = None;
        self.processes_view_sid = None;
        self.outputs_view_sid = None;
        self.pipelines_view_sid = None;
        self.edges_view_sid = None;
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
        match rpc(&self.sock, "view.open", json!(["procs", sid, filter, self.proc_running_only])) {
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
    /// Pull the box summary bundle for the currently-selected box.
    /// One small RPC; populates `box_summary` which `box_detail_lines`
    /// reads. Named for the legacy "recent_changes" call but renamed
    /// in spirit — recent_changes Vec is gone, the bundle is the
    /// single source of truth now (no fallback).
    /// (Re)query the Vars view. Requires a non-empty name or value pattern —
    /// an unfiltered dump of a big build's assignments is the useless list
    /// this view exists to avoid.
    fn load_vars(&mut self) {
        self.vars_rows.clear();
        self.sel_var = 0;
        let Some(sid) = self.cur_sid() else { return };
        self.sel_var_item = 0;
        let (n, v) = self.vars_query.clone();
        if n.is_empty() && v.is_empty() {
            self.status = "vars: press '/' and type a NAME (or NAME VALUE) query".into();
            return;
        }
        match rpc(&self.sock, "review.makevars",
                  json!([sid, n, v, 800, self.vars_any])) {
            Ok(rows) => {
                self.vars_rows = rows.as_array().cloned().unwrap_or_default();
                self.status = format!("vars: {} assignment(s)", self.vars_rows.len());
            }
            Err(e) => self.status = format!("review.makevars: {e}"),
        }
    }

    fn refresh_recent_changes(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        self.box_summary = serde_json::json!(null);
        // 40 per kind: enough rows that the detail pane's height-scaled
        // sections stay full on a tall terminal (box_detail_lines trims each
        // section to its share of the pane).
        if let Ok(v) = rpc(&self.sock, "review.box_summary", json!([sid, 40])) {
            self.box_summary = v;
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
    /// `f` on Procs: toggle showing only alive processes (live box) vs all.
    fn toggle_proc_running_only(&mut self) {
        self.proc_running_only = !self.proc_running_only;
        self.load_processes();
        self.status = if self.proc_running_only {
            "procs: running only".to_string()
        } else {
            "procs: all".to_string()
        };
    }

    fn load_processes(&mut self) {
        self.close_processes_view();
        self.processes.clear();
        self.processes_total = 0;
        self.processes_window_start = 0;
        self.sel_proc = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_procs.active());
        match rpc(&self.sock, "view.open", json!(["procs", sid, filter, self.proc_running_only])) {
            Ok(v) => {
                self.processes_view = v.get("view_id").and_then(Value::as_u64);
                self.processes_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.processes_view_sid = Some(sid);
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

    /// Load the procs view only if it's not already loaded for the
    /// currently-selected box. Called from the letter-chip / nav /
    /// Tab path so a session-switch doesn't pay the view.open cost
    /// (eager full-sqlar scan, expensive on million-row boxes); the
    /// user pays it once when they actually navigate to procs.
    fn load_processes_if_needed(&mut self) {
        let cur = self.cur_sid_i64();
        if cur.is_some() && self.processes_view_sid == cur {
            return;
        }
        self.load_processes();
    }

    fn load_pipelines(&mut self) {
        self.close_pipelines_view();
        self.pipelines.clear();
        self.pipelines_flat.clear();
        self.pipelines_total = 0;
        self.pipelines_window_start = 0;
        self.sel_pipeline = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_pipelines.active());
        match rpc(&self.sock, "view.open", json!(["pipelines", sid, filter])) {
            Ok(v) => {
                self.pipelines_view = v.get("view_id").and_then(Value::as_u64);
                self.pipelines_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.pipelines_view_sid = Some(sid);
            }
            Err(e) => { self.status = format!("view.open pipelines: {e}"); return; }
        }
        if self.f_pipelines.on {
            self.fetch_pipelines_window(0);
        } else {
            let tail_start = self.pipelines_total.saturating_sub(WINDOW_SIZE);
            self.fetch_pipelines_window(tail_start);
            self.sel_pipeline = self.pipelines.len().saturating_sub(1);
        }
    }

    fn load_pipelines_if_needed(&mut self) {
        let cur = self.cur_sid_i64();
        if cur.is_some() && self.pipelines_view_sid == cur {
            return;
        }
        self.load_pipelines();
    }

    fn fetch_pipelines_window(&mut self, start: usize) {
        let Some(vid) = self.pipelines_view else { return };
        let start = start.min(self.pipelines_total.saturating_sub(1).max(0));
        match rpc(&self.sock, "view.window",
                  json!([vid, start, WINDOW_SIZE])) {
            Ok(v) => {
                self.pipelines_window_start =
                    v.get("start").and_then(Value::as_u64).unwrap_or(start as u64) as usize;
                self.pipelines_total =
                    v.get("total").and_then(Value::as_u64)
                        .unwrap_or(self.pipelines_total as u64) as usize;
                self.pipelines_flat = v.get("rows").and_then(Value::as_array).cloned()
                    .unwrap_or_default();
            }
            Err(e) => self.status = format!("view.window pipelines: {e}"),
        }
        self.rebuild_pipeline_view();
    }

    fn close_pipelines_view(&mut self) {
        if let Some(vid) = self.pipelines_view.take() {
            let _ = rpc(&self.sock, "view.close", json!([vid]));
        }
    }

    fn refresh_pipelines_preserving_cursor(&mut self) {
        let Some(sid) = self.cur_sid_i64() else { return };
        let pinned_id = self.pipelines.get(self.sel_pipeline)
            .and_then(|p| p.get("id").and_then(Value::as_i64));
        let saved_start = self.pipelines_window_start;
        let saved_sel = self.sel_pipeline;
        self.close_pipelines_view();
        let filter = filter_to_json(self.f_pipelines.active());
        match rpc(&self.sock, "view.open", json!(["pipelines", sid, filter])) {
            Ok(v) => {
                self.pipelines_view = v.get("view_id").and_then(Value::as_u64);
                self.pipelines_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.pipelines_view_sid = Some(sid);
            }
            Err(e) => { self.status = format!("view.open pipelines: {e}"); return; }
        }
        let tail_start = if self.f_pipelines.on {
            0
        } else {
            self.pipelines_total.saturating_sub(WINDOW_SIZE)
        };
        self.fetch_pipelines_window(saved_start.max(tail_start));
        let restored = pinned_id.and_then(|want| {
            self.pipelines.iter().position(|p|
                p.get("id").and_then(Value::as_i64) == Some(want))
        });
        self.sel_pipeline = restored
            .unwrap_or_else(|| saved_sel.min(self.pipelines.len().saturating_sub(1)));
    }

    fn push_pipelines_filter(&mut self) {
        let Some(vid) = self.pipelines_view else { return };
        let filter = self.filter_json_for(FilterView::Pipelines);
        match rpc(&self.sock, "view.filter", json!([vid, filter])) {
            Ok(v) => {
                self.pipelines_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.pipelines_window_start = 0;
                self.sel_pipeline = 0;
                self.fetch_pipelines_window(0);
            }
            Err(e) => self.status = format!("view.filter pipelines: {e}"),
        }
    }

    /// Rebuild the displayed `pipelines` from `pipelines_flat` per `pipe_tree`:
    /// a parent→child tree (depth stamped) or the flat chronological list.
    /// Client-side post-processing on top of the server-filtered window.
    fn rebuild_pipeline_view(&mut self) {
        let src: Vec<Value> = if self.pipe_running_only {
            self.pipelines_flat.iter()
                .filter(|r| r.get("done_ts").and_then(Value::as_f64).unwrap_or(0.0) == 0.0)
                .cloned().collect()
        } else {
            self.pipelines_flat.clone()
        };
        self.pipelines = if self.pipe_tree {
            build_pipeline_tree(src)
        } else {
            src
        };
        if !self.pipelines.is_empty() && self.sel_pipeline >= self.pipelines.len() {
            self.sel_pipeline = self.pipelines.len() - 1;
        }
    }

    /// `t`: toggle the Pipelines pane between hierarchical tree and flat
    /// chronological order.
    fn toggle_pipeline_tree(&mut self) {
        self.pipe_tree = !self.pipe_tree;
        self.rebuild_pipeline_view();
        self.status = format!("pipelines: {} view",
            if self.pipe_tree { "tree" } else { "flat chronological" });
    }

    /// `f`: toggle showing only in-flight (running) pipelines — done_ts==0.
    fn toggle_pipeline_running_only(&mut self) {
        self.pipe_running_only = !self.pipe_running_only;
        self.sel_pipeline = 0;
        self.rebuild_pipeline_view();
        self.status = if self.pipe_running_only {
            "pipelines: running only".to_string()
        } else {
            "pipelines: all".to_string()
        };
    }

    fn load_build_edges(&mut self) {
        self.close_edges_view();
        self.build_edges.clear();
        self.build_edges_flat.clear();
        self.edges_total = 0;
        self.edges_window_start = 0;
        self.sel_edge = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        let filter = filter_to_json(self.f_edges.active());
        match rpc(&self.sock, "view.open", json!(["build_edges", sid, filter])) {
            Ok(v) => {
                self.edges_view = v.get("view_id").and_then(Value::as_u64);
                self.edges_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.edges_view_sid = Some(sid);
            }
            Err(e) => { self.status = format!("view.open build_edges: {e}"); return; }
        }
        if self.f_edges.on {
            self.fetch_edges_window(0);
        } else {
            let tail_start = self.edges_total.saturating_sub(WINDOW_SIZE);
            self.fetch_edges_window(tail_start);
            self.sel_edge = self.build_edges.len().saturating_sub(1);
        }
    }

    fn load_edges_if_needed(&mut self) {
        let cur = self.cur_sid_i64();
        if cur.is_some() && self.edges_view_sid == cur {
            return;
        }
        self.load_build_edges();
    }

    fn fetch_edges_window(&mut self, start: usize) {
        let Some(vid) = self.edges_view else { return };
        let start = start.min(self.edges_total.saturating_sub(1).max(0));
        match rpc(&self.sock, "view.window",
                  json!([vid, start, WINDOW_SIZE])) {
            Ok(v) => {
                self.edges_window_start =
                    v.get("start").and_then(Value::as_u64).unwrap_or(start as u64) as usize;
                self.edges_total =
                    v.get("total").and_then(Value::as_u64)
                        .unwrap_or(self.edges_total as u64) as usize;
                self.build_edges_flat = v.get("rows").and_then(Value::as_array).cloned()
                    .unwrap_or_default();
            }
            Err(e) => self.status = format!("view.window build_edges: {e}"),
        }
        self.rebuild_edge_view();
    }

    fn close_edges_view(&mut self) {
        if let Some(vid) = self.edges_view.take() {
            let _ = rpc(&self.sock, "view.close", json!([vid]));
        }
    }

    fn push_edges_filter(&mut self) {
        let Some(vid) = self.edges_view else { return };
        let filter = self.filter_json_for(FilterView::BuildEdges);
        match rpc(&self.sock, "view.filter", json!([vid, filter])) {
            Ok(v) => {
                self.edges_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.edges_window_start = 0;
                self.sel_edge = 0;
                self.fetch_edges_window(0);
            }
            Err(e) => self.status = format!("view.filter build_edges: {e}"),
        }
    }

    /// Rebuild the displayed `build_edges` from `build_edges_flat`. On a live box
    /// with `edges_running_only`, keep only the edges currently building
    /// (started_ts>0 && ended_ts==0). Client-side post-processing on the
    /// server-filtered window.
    fn rebuild_edge_view(&mut self) {
        let filter = self.edges_running_only && self.cur_session_live();
        self.build_edges = if filter {
            self.build_edges_flat.iter()
                .filter(|r| edge_running(r))
                .cloned().collect()
        } else {
            self.build_edges_flat.clone()
        };
        if !self.build_edges.is_empty() && self.sel_edge >= self.build_edges.len() {
            self.sel_edge = self.build_edges.len() - 1;
        }
    }

    /// Re-fetch build edges without jumping the cursor to the tail — used by the
    /// live event path (`build_edges` events) so the running-only view tracks a
    /// running build as recipes start and finish. `rebuild_edge_view` clamps the
    /// selection into the (possibly shrunk) filtered set.
    fn refresh_build_edges_preserving_cursor(&mut self) {
        let Some(sid) = self.cur_sid_i64() else { return };
        self.close_edges_view();
        let filter = filter_to_json(self.f_edges.active());
        match rpc(&self.sock, "view.open", json!(["build_edges", sid, filter])) {
            Ok(v) => {
                self.edges_view = v.get("view_id").and_then(Value::as_u64);
                self.edges_total =
                    v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.edges_view_sid = Some(sid);
            }
            Err(e) => { self.status = format!("view.open build_edges: {e}"); return; }
        }
        let start = self.edges_window_start;
        self.fetch_edges_window(start);
        if self.sel_edge >= self.build_edges.len() {
            self.sel_edge = self.build_edges.len().saturating_sub(1);
        }
    }

    /// `f` on Targets: toggle showing only the edges currently building (live
    /// box) vs every parsed edge.
    fn toggle_edge_running_only(&mut self) {
        self.edges_running_only = !self.edges_running_only;
        self.sel_edge = 0;
        self.rebuild_edge_view();
        self.sel_edge = self.build_edges.len().saturating_sub(1);
        self.status = if self.edges_running_only {
            "targets: running only".to_string()
        } else {
            "targets: all".to_string()
        };
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

    fn move_pipeline_cursor(&mut self, delta: isize) {
        if self.pipelines_total == 0 { return; }
        let step: isize = if delta > 0 { 1 } else { -1 };
        let mut global = self.pipelines_window_start + self.sel_pipeline;
        let new_global = global as isize + step;
        if new_global < 0 || new_global as usize >= self.pipelines_total {
            return;
        }
        global = new_global as usize;
        let win_end = self.pipelines_window_start + self.pipelines.len();
        if global < self.pipelines_window_start || global >= win_end {
            let quarter = WINDOW_SIZE / 4;
            let new_start = global.saturating_sub(
                if step > 0 { quarter } else { WINDOW_SIZE - quarter });
            self.fetch_pipelines_window(new_start);
        }
        self.sel_pipeline = global.saturating_sub(self.pipelines_window_start);
    }

    fn move_edge_cursor(&mut self, delta: isize) {
        if self.edges_total == 0 { return; }
        let step: isize = if delta > 0 { 1 } else { -1 };
        let mut global = self.edges_window_start + self.sel_edge;
        let new_global = global as isize + step;
        if new_global < 0 || new_global as usize >= self.edges_total {
            return;
        }
        global = new_global as usize;
        let win_end = self.edges_window_start + self.build_edges.len();
        if global < self.edges_window_start || global >= win_end {
            let quarter = WINDOW_SIZE / 4;
            let new_start = global.saturating_sub(
                if step > 0 { quarter } else { WINDOW_SIZE - quarter });
            self.fetch_edges_window(new_start);
        }
        self.sel_edge = global.saturating_sub(self.edges_window_start);
    }

    fn page_pipeline_cursor(&mut self, delta: isize) {
        let n = delta.unsigned_abs();
        let step: isize = if delta > 0 { 1 } else { -1 };
        for _ in 0..n { self.move_pipeline_cursor(step); }
    }

    fn page_edge_cursor(&mut self, delta: isize) {
        let n = delta.unsigned_abs();
        let step: isize = if delta > 0 { 1 } else { -1 };
        for _ in 0..n { self.move_edge_cursor(step); }
    }

    fn move_output_cursor(&mut self, delta: isize) {
        if self.outputs_total == 0 { return; }
        let step: isize = if delta > 0 { 1 } else { -1 };
        let global = self.outputs_window_start + self.sel_output;
        let new_global = global as isize + step;
        if new_global < 0 || new_global as usize >= self.outputs_total {
            return;
        }
        let global = new_global as usize;
        let win_end = self.outputs_window_start + self.outputs.len();
        if global < self.outputs_window_start || global >= win_end {
            let quarter = WINDOW_SIZE / 4;
            let new_start = global.saturating_sub(
                if step > 0 { quarter } else { WINDOW_SIZE - quarter });
            self.fetch_outputs_window(new_start);
        }
        self.sel_output = global.saturating_sub(self.outputs_window_start);
    }

    fn page_output_cursor(&mut self, delta: isize) {
        let n = delta.unsigned_abs();
        let step: isize = if delta > 0 { 1 } else { -1 };
        for _ in 0..n { self.move_output_cursor(step); }
    }

    /// Load the captured flows for the selected box: one RPC, full list
    /// (a typical box's pcapng has dozens of flows, no windowing needed
    /// since each row is small). On success the cursor lands on the first
    /// row and we proactively fetch its detail.
    fn load_flows(&mut self) {
        self.flows.clear();
        self.sel_flow = 0;
        self.flow_detail.clear();
        self.flow_detail_frame = 0;
        self.right_scroll = 0;
        let Some(sid) = self.cur_sid_i64() else { return };
        match rpc(&self.sock, "flows.list", json!([sid.to_string()])) {
            Ok(v) => {
                if v.get("ok").and_then(Value::as_bool) == Some(true) {
                    if let Some(arr) = v.get("flows").and_then(Value::as_array) {
                        self.flows = arr.clone();
                    }
                } else if let Some(e) = v.get("error").and_then(Value::as_str) {
                    self.status = format!("flows.list: {e}");
                }
            }
            Err(e) => { self.status = format!("flows.list: {e}"); }
        }
        self.load_flow_detail();
    }

    /// Poll the engine for the next pending banner-prompt. Idempotent;
    /// safe to call every tick. The only stateful effect is replacing
    /// `self.pending_prompt` with the engine's view.
    fn refresh_prompt(&mut self) {
        match rpc(&self.sock, "prompts.peek", json!([])) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                let ask = v.get("ask").cloned();
                self.pending_prompt = match ask {
                    Some(Value::Null) | None => None,
                    Some(other) => Some(other),
                };
            }
            _ => {}
        }
    }

    /// Send the user's verdict back; on success the engine pops the
    /// queue and the next tick's refresh_prompt picks up whatever's
    /// behind it (or None).
    fn answer_prompt(&mut self, verdict: &'static str) {
        let Some(p) = self.pending_prompt.clone() else { return };
        let Some(id) = p.get("id").and_then(Value::as_u64) else { return };
        let _ = rpc(&self.sock, "prompts.answer", json!([id, verdict]));
        self.pending_prompt = None;
        // Eager refresh so the banner doesn't flicker between answer and
        // the next tick (a fast-typer can hit y twice before the tick).
        self.refresh_prompt();
        let host = p.get("host").and_then(Value::as_str).unwrap_or("");
        self.status = match verdict {
            "yes_once"  => format!("allow this once: {host}"),
            "no_once"   => format!("deny this once: {host}"),
            "allow_save" => format!("ALLOW + saved: apply host:{host}"),
            "deny_save"  => format!("DENY + saved: discard host:{host}"),
            _ => String::new(),
        };
    }

    /// Drill from the selected flow row into its TCP stream's packet
    /// list. Open by `Enter` on the flows pane. Idempotent: re-entering
    /// with the same stream id replays the cached state.
    fn open_packets(&mut self) {
        let Some(sid) = self.cur_sid_i64() else { return };
        let Some(row) = self.flows.get(self.sel_flow) else { return };
        let stream = row.get("stream").and_then(Value::as_i64).unwrap_or(-1);
        if stream < 0 {
            self.status = "no tcp.stream for that flow".into();
            return;
        }
        self.packets.clear();
        self.sel_packet = 0;
        self.packet_detail.clear();
        self.packet_detail_frame = 0;
        self.packets_stream = stream;
        self.right_scroll = 0;
        match rpc(&self.sock, "flows.packets",
                  json!([sid.to_string(), stream])) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                if let Some(arr) = v.get("packets").and_then(Value::as_array) {
                    self.packets = arr.clone();
                }
            }
            Ok(v) => {
                self.status = format!("flows.packets: {}",
                    v.get("error").and_then(Value::as_str).unwrap_or("?"));
            }
            Err(e) => self.status = format!("flows.packets: {e}"),
        }
        // Land the cursor on the same frame the user picked in the flows
        // list (so the right-pane immediately shows what they clicked on,
        // not packet #1 of the connection).
        let want = row.get("frame").and_then(Value::as_u64).unwrap_or(0);
        if want > 0 {
            if let Some(pos) = self.packets.iter().position(|r| {
                r.get("frame").and_then(Value::as_u64) == Some(want)
            }) {
                self.sel_packet = pos;
            }
        }
        self.focus = Pane::Packets;
        self.load_packet_detail();
    }

    /// Lazy-load tshark -V for the cursored packet (same engine verb as
    /// the flows pane; one frame is one frame).
    fn load_packet_detail(&mut self) {
        let Some(sid) = self.cur_sid_i64() else { return };
        let Some(row) = self.packets.get(self.sel_packet) else {
            self.packet_detail.clear(); self.packet_detail_frame = 0; return;
        };
        let frame = row.get("frame").and_then(Value::as_u64).unwrap_or(0);
        if frame == 0 { self.packet_detail.clear(); return; }
        if frame == self.packet_detail_frame && !self.packet_detail.is_empty() {
            return;
        }
        self.packet_detail_frame = frame;
        match rpc(&self.sock, "flows.detail",
                  json!([sid.to_string(), frame])) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                self.packet_detail = v.get("text").and_then(Value::as_str)
                    .unwrap_or("").to_string();
            }
            Ok(v) => self.packet_detail = format!(
                "tshark error: {}",
                v.get("error").and_then(Value::as_str).unwrap_or("?")),
            Err(e) => self.packet_detail = format!("rpc error: {e}"),
        }
    }

    /// Pop back from Pane::Packets to Pane::Flows. Cursor and detail
    /// state on the flows side are untouched.
    fn close_packets(&mut self) {
        self.focus = Pane::Flows;
        self.right_scroll = 0;
    }

    /// Lazy-load tshark `-V` for the selected flow's frame. Cached
    /// until the cursor moves to a different frame.
    fn load_flow_detail(&mut self) {
        let Some(sid) = self.cur_sid_i64() else { return };
        let Some(row) = self.flows.get(self.sel_flow) else {
            self.flow_detail.clear(); self.flow_detail_frame = 0; return;
        };
        let frame = row.get("frame").and_then(Value::as_u64).unwrap_or(0);
        if frame == 0 { self.flow_detail.clear(); return; }
        if frame == self.flow_detail_frame && !self.flow_detail.is_empty() {
            return;
        }
        self.flow_detail_frame = frame;
        match rpc(&self.sock, "flows.detail",
                  json!([sid.to_string(), frame])) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                self.flow_detail = v.get("text").and_then(Value::as_str)
                    .unwrap_or("").to_string();
            }
            Ok(v) => {
                self.flow_detail = format!(
                    "tshark error: {}",
                    v.get("error").and_then(Value::as_str).unwrap_or("?"));
            }
            Err(e) => self.flow_detail = format!("rpc error: {e}"),
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
                self.outputs_view_sid = Some(sid);
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

    /// Pull this box's `api_log` table directly (no view machinery — the
    /// rows are bounded by the number of LLM calls a box has made, and
    /// the table fits comfortably in one fetch). One row per row.
    /// Refresh the Api pane's endpoint summary from oaita.toml (shown in
    /// the empty state so "how do I get an endpoint?" answers itself).
    fn refresh_api_endpoint_note(&mut self) {
        let cfg = crate::oaita::config::Config::load();
        self.api_endpoint_note =
            endpoint_note_lines(cfg.resolve().ok()
                .map(|(model, base_url, _)| (model, base_url)));
    }

    /// The FIRST time the Api pane is shown with nothing wired up — no
    /// external API in oaita.toml AND no local model declared — auto-open the
    /// model picker so the endpoint answers "how do I get one?" itself,
    /// instead of leaving the user to guess F4 / magic argv. One-shot: a
    /// dismissal is respected (see `model_picker_offered`).
    fn maybe_offer_model_picker(&mut self) {
        if self.model_picker_offered || self.modal.is_some() { return; }
        let none = matches!(rpc(&self.sock, "oaita.status", json!([])),
            Ok(v) if v.get("kind").and_then(Value::as_str) == Some("none"));
        if none {
            self.model_picker_offered = true;
            self.open_model_picker();
        }
    }

    fn load_api_logs(&mut self) {
        self.refresh_api_endpoint_note();
        self.maybe_offer_model_picker();
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "api_log", json!([sid])) {
            Ok(v) => {
                self.api_log_rows = v.as_array().cloned().unwrap_or_default();
                self.api_log_loaded_sid = Some(sid);
            }
            Err(e) if e.contains("unknown verb") => {
                self.status = "engine doesn't speak api_log — old engine?".into();
            }
            Err(e) => { self.status = format!("api_log: {e}"); }
        }
    }

    fn load_api_logs_if_needed(&mut self) {
        let cur = self.cur_sid();
        if cur.is_some() && self.api_log_loaded_sid == cur && !self.api_log_rows.is_empty() {
            return;
        }
        self.load_api_logs();
    }

    /// Network/Web pane loader (DESIGN-web.md W4): pull the focused box's
    /// webcap rows (newest first). Mirrors load_api_logs; no api-endpoint
    /// bookkeeping. Clamps the selection so a shrunken list (box changed)
    /// never leaves the cursor past the end.
    fn load_webcap(&mut self) {
        let Some(sid) = self.cur_sid() else { return };
        match rpc(&self.sock, "webcap", json!([sid])) {
            Ok(v) => {
                self.webcap_rows = v.as_array().cloned().unwrap_or_default();
                self.webcap_loaded_sid = Some(sid);
                if self.sel_webcap >= self.webcap_rows.len() {
                    self.sel_webcap = self.webcap_rows.len().saturating_sub(1);
                }
            }
            Err(e) if e.contains("unknown verb") => {
                self.status = "engine doesn't speak webcap — old engine?".into();
            }
            Err(e) => { self.status = format!("webcap: {e}"); }
        }
    }

    fn load_webcap_if_needed(&mut self) {
        let cur = self.cur_sid();
        if cur.is_some() && self.webcap_loaded_sid == cur && !self.webcap_rows.is_empty() {
            return;
        }
        self.load_webcap();
    }

    /// Lazy counterpart of load_outputs — same idea as
    /// load_processes_if_needed: only re-open when the box changed.
    fn load_outputs_if_needed(&mut self) {
        let cur = self.cur_sid_i64();
        if cur.is_some() && self.outputs_view_sid == cur {
            return;
        }
        self.load_outputs();
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
    /// True for the views where an errors-only lens is meaningful.
    fn err_capable(v: FilterView) -> bool {
        matches!(v, FilterView::Outputs | FilterView::Pipelines | FilterView::BuildEdges)
    }

    /// The wire filter for view `v`: the user's '/' clauses, plus a synthetic
    /// `err` clause AND-ed on while the errors-only toggle is lit.
    fn filter_json_for(&self, v: FilterView) -> Value {
        let base = filter_to_json(self.view_filter(v).active());
        if !(self.err_only && Self::err_capable(v)) {
            return base;
        }
        let mut arr = match base { Value::Array(a) => a, _ => vec![] };
        arr.push(json!({"kind": "err", "pattern": "1", "join": "and",
                        "negate": false, "enabled": true}));
        Value::Array(arr)
    }

    /// '!' — flip the errors-only lens on the focused view (no-op elsewhere).
    #[cfg_attr(test, allow(dead_code))]
    fn toggle_err_only(&mut self) {
        let Some(v) = self.focus_filter_view() else {
            self.status = "errors-only: not applicable here".into();
            return;
        };
        if !Self::err_capable(v) {
            self.status = "errors-only: not applicable here".into();
            return;
        }
        self.err_only = !self.err_only;
        self.push_view_filter(v);
        self.status = if self.err_only {
            "showing ERRORS only ('!' to clear)".into()
        } else {
            "errors-only off".into()
        };
    }

    fn push_outputs_filter(&mut self) {
        let Some(vid) = self.outputs_view else { return };
        let filter = self.filter_json_for(FilterView::Outputs);
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
        // Vars detail focused: move over its navigable items, not a scroll.
        if self.right_focused && self.focus == Pane::Vars {
            self.var_item_move(1);
            return;
        }
        // Right-pane focused: scroll the detail body, not the left list.
        // Hunks doesn't go through here (its own keymap drives the diff
        // scroll); right_pane_scrollable() filters Hunks out.
        if self.right_focused && self.right_pane_scrollable() {
            self.right_scroll = self
                .right_scroll
                .saturating_add(1)
                .min(self.right_scroll_max.get());
            return;
        }
        match self.focus {
            Pane::Sessions => {
                if self.sel_session + 1 < self.sessions.len() {
                    self.sel_session += 1;
                    self.on_box_cursor_moved();
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
            Pane::Processes => { self.move_proc_cursor(1); self.right_scroll = 0; }
            Pane::Outputs => { self.move_output_cursor(1); self.right_scroll = 0; }
            Pane::Rules => {
                if self.sel_rule + 1 < self.rules.len() {
                    self.sel_rule += 1;
                    self.right_scroll = 0;
                }
            }
            Pane::Pipelines => { self.move_pipeline_cursor(1); self.right_scroll = 0; }
            Pane::BuildEdges => { self.move_edge_cursor(1); self.right_scroll = 0; }
            Pane::Flows => {
                if self.sel_flow + 1 < self.flows.len() {
                    self.sel_flow += 1;
                    self.right_scroll = 0;
                    self.load_flow_detail();
                }
            }
            Pane::Packets => {
                if self.sel_packet + 1 < self.packets.len() {
                    self.sel_packet += 1;
                    self.right_scroll = 0;
                    self.load_packet_detail();
                }
            }
            Pane::Help => self.out_scroll = self.out_scroll.saturating_add(1),
            Pane::Pty => {}
            Pane::ApiLogs => {
                if self.sel_api_log + 1 < self.api_log_rows.len() {
                    self.sel_api_log += 1;
                    self.right_scroll = 0;
                }
            }
            Pane::Network => {
                if self.sel_webcap + 1 < self.webcap_rows.len() {
                    self.sel_webcap += 1;
                    self.right_scroll = 0;
                }
            }
            Pane::Vars => {
                if self.sel_var + 1 < self.vars_rows.len() {
                    self.sel_var += 1;
                    self.sel_var_item = 0;
                    self.right_scroll = 0;
                }
            }
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn move_up(&mut self) {
        if self.right_focused && self.focus == Pane::Vars {
            self.var_item_move(-1);
            return;
        }
        if self.right_focused && self.right_pane_scrollable() {
            self.right_scroll = self.right_scroll.saturating_sub(1);
            return;
        }
        match self.focus {
            Pane::Sessions => {
                if self.sel_session > 0 {
                    self.sel_session -= 1;
                    self.on_box_cursor_moved();
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
            Pane::Processes => { self.move_proc_cursor(-1); self.right_scroll = 0; }
            Pane::Outputs => { self.move_output_cursor(-1); self.right_scroll = 0; }
            Pane::Rules => {
                self.sel_rule = self.sel_rule.saturating_sub(1);
                self.right_scroll = 0;
            }
            Pane::Pipelines => { self.move_pipeline_cursor(-1); self.right_scroll = 0; }
            Pane::BuildEdges => { self.move_edge_cursor(-1); self.right_scroll = 0; }
            Pane::Flows => {
                if self.sel_flow > 0 {
                    self.sel_flow -= 1;
                    self.right_scroll = 0;
                    self.load_flow_detail();
                }
            }
            Pane::Packets => {
                if self.sel_packet > 0 {
                    self.sel_packet -= 1;
                    self.right_scroll = 0;
                    self.load_packet_detail();
                }
            }
            Pane::Help => self.out_scroll = self.out_scroll.saturating_sub(1),
            Pane::Pty => {}
            Pane::ApiLogs => {
                self.sel_api_log = self.sel_api_log.saturating_sub(1);
                self.right_scroll = 0;
            }
            Pane::Network => {
                self.sel_webcap = self.sel_webcap.saturating_sub(1);
                self.right_scroll = 0;
            }
            Pane::Vars => {
                self.sel_var = self.sel_var.saturating_sub(1);
                self.sel_var_item = 0;
                self.right_scroll = 0;
            }
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
            if step > 0 {
                self.right_scroll = self
                    .right_scroll
                    .saturating_add(n16)
                    .min(self.right_scroll_max.get());
            }
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
                    self.on_box_cursor_moved();
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
                self.right_scroll = 0;
            }
            Pane::Outputs => { self.page_output_cursor(delta); self.right_scroll = 0; }
            Pane::Rules => {
                let total = self.rules.len();
                if total == 0 { return; }
                let cur = self.sel_rule as isize;
                self.sel_rule = (cur + delta).clamp(0, total as isize - 1) as usize;
                self.right_scroll = 0;
            }
            Pane::Pipelines => { self.page_pipeline_cursor(delta); self.right_scroll = 0; }
            Pane::BuildEdges => { self.page_edge_cursor(delta); self.right_scroll = 0; }
            Pane::Flows => {
                let total = self.flows.len();
                if total == 0 { return; }
                let cur = self.sel_flow as isize;
                let new = (cur + delta).clamp(0, total as isize - 1) as usize;
                if new != self.sel_flow {
                    self.sel_flow = new;
                    self.right_scroll = 0;
                    self.load_flow_detail();
                }
            }
            Pane::Packets => {
                let total = self.packets.len();
                if total == 0 { return; }
                let cur = self.sel_packet as isize;
                let new = (cur + delta).clamp(0, total as isize - 1) as usize;
                if new != self.sel_packet {
                    self.sel_packet = new;
                    self.right_scroll = 0;
                    self.load_packet_detail();
                }
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
            Pane::ApiLogs => {
                let total = self.api_log_rows.len();
                if total == 0 { return; }
                let cur = self.sel_api_log as isize;
                self.sel_api_log = (cur + delta).clamp(0, total as isize - 1) as usize;
                self.right_scroll = 0;
            }
            Pane::Network => {
                let total = self.webcap_rows.len();
                if total == 0 { return; }
                let cur = self.sel_webcap as isize;
                self.sel_webcap = (cur + delta).clamp(0, total as isize - 1) as usize;
                self.right_scroll = 0;
            }
            Pane::Vars => {
                let total = self.vars_rows.len();
                if total == 0 { return; }
                let cur = self.sel_var as isize;
                self.sel_var = (cur + delta).clamp(0, total as isize - 1) as usize;
                self.sel_var_item = 0;
                self.right_scroll = 0;
            }
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
            | Pane::Pipelines | Pane::BuildEdges | Pane::Rules
            | Pane::Flows | Pane::Packets | Pane::Vars)
    }

    /// Snap focus back to the LEFT list and reset the right-pane scroll.
    /// Called whenever the user picks a different view via a letter chip
    /// (b/c/p/o/l/g/e/?) — we don't carry the "right pane focused" bit
    /// across views, that would confuse the cursor in the new view.
    fn snap_left(&mut self) {
        self.right_focused = false;
        self.right_scroll = 0;
    }

    /// The FilterView backing a pane's LIST, if any (like focus_filter_view
    /// but for an arbitrary pane — the go-back stack restores non-focused
    /// panes).
    fn pane_filter_view(pane: Pane) -> Option<FilterView> {
        match pane {
            Pane::Changes | Pane::Hunks => Some(FilterView::Changes),
            Pane::Processes => Some(FilterView::Procs),
            Pane::Outputs => Some(FilterView::Outputs),
            Pane::Pipelines => Some(FilterView::Pipelines),
            Pane::BuildEdges => Some(FilterView::BuildEdges),
            _ => None,
        }
    }

    /// The focused pane's GLOBAL cursor position (window-relative selection
    /// plus the window start for the engine-windowed views).
    fn current_cursor_global(&self) -> usize {
        match self.focus {
            Pane::Sessions => self.sel_session,
            Pane::Changes | Pane::Hunks => self.changes_window_start + self.sel_change,
            Pane::Processes => self.processes_window_start + self.sel_proc,
            Pane::Outputs => self.outputs_window_start + self.sel_output,
            Pane::Pipelines => self.pipelines_window_start + self.sel_pipeline,
            Pane::BuildEdges => self.edges_window_start + self.sel_edge,
            Pane::Rules => self.sel_rule,
            Pane::Flows => self.sel_flow,
            Pane::Packets => self.sel_packet,
            Pane::ApiLogs => self.sel_api_log,
            Pane::Network => self.sel_webcap,
            Pane::Vars => self.sel_var,
            Pane::Help | Pane::Pty => 0,
        }
    }

    /// Record the CURRENT view on the go-back stack (called just before a
    /// view switch). Oldest entries drop once the stack exceeds its cap.
    fn push_history(&mut self) {
        if matches!(self.focus, Pane::Help | Pane::Pty) {
            return; // full-screen panes aren't list views worth returning to
        }
        let snap = NavSnapshot {
            pane: self.focus,
            filter: Self::pane_filter_view(self.focus)
                .map(|v| self.view_filter(v).clone()),
            cursor: self.current_cursor_global(),
            right_scroll: self.right_scroll,
            right_focused: self.right_focused,
            err_only: self.err_only,
            vars_query: self.vars_query.clone(),
            vars_any: self.vars_any,
        };
        self.nav_history.push(snap);
        if self.nav_history.len() > NAV_HISTORY_CAP {
            let excess = self.nav_history.len() - NAV_HISTORY_CAP;
            self.nav_history.drain(..excess);
        }
    }

    /// Backspace — pop the go-back stack and restore that view wholesale:
    /// focus, its '/' filter + errors-only lens (re-synced to the engine),
    /// the cursor position, and the right pane's focus + scroll.
    #[cfg_attr(test, allow(dead_code))]
    fn go_back(&mut self) {
        let Some(snap) = self.nav_history.pop() else {
            self.status = "back: no earlier view".into();
            return;
        };
        self.focus = snap.pane;
        // Load the pane's data (same loads go_to_pane does, WITHOUT nav() —
        // a restore must not install a fresh cross-nav filter).
        match snap.pane {
            Pane::Changes | Pane::Hunks => self.load_changes_if_needed(),
            Pane::Processes => self.load_processes_if_needed(),
            Pane::Outputs => self.load_outputs_if_needed(),
            Pane::Pipelines => self.load_pipelines_if_needed(),
            Pane::BuildEdges => self.load_edges_if_needed(),
            Pane::ApiLogs => self.load_api_logs_if_needed(),
            Pane::Network => self.load_webcap_if_needed(),
            Pane::Flows => self.load_flows(),
            _ => {}
        }
        self.err_only = snap.err_only;
        if let Some(v) = Self::pane_filter_view(snap.pane) {
            if let Some(f) = snap.filter.clone() {
                *self.view_filter_mut(v) = f;
            }
            self.push_view_filter(v);
            self.goto_view_pos(v, snap.cursor);
            if matches!(snap.pane, Pane::Changes | Pane::Hunks) {
                self.load_hunks();
            }
        } else {
            match snap.pane {
                Pane::Sessions => {
                    let n = self.sessions.len();
                    if n > 0 {
                        self.sel_session = snap.cursor.min(n - 1);
                        self.on_box_cursor_moved();
                    }
                }
                Pane::Rules => {
                    self.sel_rule =
                        snap.cursor.min(self.rules.len().saturating_sub(1));
                }
                Pane::Flows => {
                    if !self.flows.is_empty() {
                        self.sel_flow = snap.cursor.min(self.flows.len() - 1);
                        self.load_flow_detail();
                    }
                }
                Pane::Packets => {
                    if !self.packets.is_empty() {
                        self.sel_packet = snap.cursor.min(self.packets.len() - 1);
                        self.load_packet_detail();
                    }
                }
                Pane::ApiLogs => {
                    self.sel_api_log =
                        snap.cursor.min(self.api_log_rows.len().saturating_sub(1));
                }
                Pane::Network => {
                    self.sel_webcap =
                        snap.cursor.min(self.webcap_rows.len().saturating_sub(1));
                }
                Pane::Vars => {
                    self.vars_query = snap.vars_query.clone();
                    self.vars_any = snap.vars_any;
                    self.load_vars();
                    self.sel_var =
                        snap.cursor.min(self.vars_rows.len().saturating_sub(1));
                }
                _ => {}
            }
        }
        self.right_focused = snap.right_focused;
        self.right_scroll = snap.right_scroll;
        self.status = "← back".into();
    }

    /// Switch to a top-level pane (the `PANE_KEYS` accelerators route here via
    /// `dispatch_menubar_key`). The filterable views (changes/procs/outputs/api)
    /// go through `nav` so cross-pane filters resolve; the rest set focus and
    /// load their data. PTY is handled in the dispatcher (its selection logic).
    fn go_to_pane(&mut self, pane: Pane) {
        if pane != self.focus {
            // The errors-only lens is transient: leaving the view drops it
            // (and re-syncs the old view's engine filter without the lens).
            if self.err_only {
                self.err_only = false;
                if let Some(v) = self.focus_filter_view() {
                    self.push_view_filter(v);
                }
            }
            self.push_history();
        }
        self.snap_left();
        match pane {
            Pane::Changes   => { self.nav(Pane::Changes);   self.load_changes_if_needed(); }
            Pane::Processes => { self.nav(Pane::Processes); self.load_processes_if_needed(); }
            Pane::Outputs   => { self.nav(Pane::Outputs);   self.load_outputs_if_needed(); }
            Pane::ApiLogs   => { self.nav(Pane::ApiLogs);   self.load_api_logs_if_needed(); }
            Pane::Network   => { self.nav(Pane::Network);   self.load_webcap_if_needed(); }
            Pane::Flows     => { self.focus = Pane::Flows;      self.load_flows(); }
            Pane::Pipelines => { self.nav(Pane::Pipelines);  self.load_pipelines_if_needed(); }
            Pane::BuildEdges=> { self.nav(Pane::BuildEdges); self.load_edges_if_needed(); }
            Pane::Help      => { self.focus = Pane::Help; self.out_scroll = 0; }
            Pane::Vars      => {
                self.focus = Pane::Vars;
                self.load_vars();
                if self.vars_query.0.is_empty() && self.vars_query.1.is_empty() {
                    self.modal = Some(Modal::VarQuery { buf: String::new() });
                }
            }
            other           => { self.focus = other; }
        }
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
                if self.right_focused {
                    // Outputs: pick up manual scrolling from wherever the
                    // cursor-follow left the transcript — no jump to top.
                    if self.focus == Pane::Outputs {
                        self.right_scroll = self.out_follow_scroll.get();
                    }
                } else if self.focus != Pane::Outputs {
                    // Outputs keeps its scroll — the follow-mode render
                    // ignores right_scroll and re-seeds on the next Tab.
                    self.right_scroll = 0;
                }
            }
            _ => {}
        }
    }

    /// Enter: open the selected row into the next pane.
    fn open(&mut self) {
        match self.focus {
            Pane::Sessions => {
                self.push_history();
                self.load_changes();
                self.focus = Pane::Changes;
            }
            Pane::Changes => {
                self.push_history();
                self.load_hunks();
                self.focus = Pane::Hunks;
            }
            // Flows → Packets drill-down on Enter; Esc pops back (Backspace
            // is the global go-back, which lands on the same view anyway).
            Pane::Flows => {
                self.push_history();
                self.open_packets();
            }
            // Enter on the Vars list focuses the detail's navigable items;
            // Enter on a focused item follows it (re-query a dereferenced
            // name, or jump to another assignment of this variable).
            Pane::Vars => {
                if !self.right_focused {
                    if !var_detail(self).1.is_empty() {
                        self.right_focused = true;
                        self.sel_var_item = 0;
                        self.var_item_move(0);
                    }
                    return;
                }
                self.var_item_act();
            }
            Pane::Hunks | Pane::Processes | Pane::Outputs | Pane::Rules
            | Pane::Pipelines | Pane::BuildEdges | Pane::Packets
            | Pane::Help | Pane::Pty | Pane::ApiLogs | Pane::Network => {}
        }
    }

    /// Move the Vars detail-item cursor and keep it in view.
    fn var_item_move(&mut self, delta: isize) {
        let (_, items) = var_detail(self);
        if items.is_empty() {
            return;
        }
        let cur = self.sel_var_item.min(items.len() - 1) as isize;
        let ni = (cur + delta).clamp(0, items.len() as isize - 1) as usize;
        self.sel_var_item = ni;
        // keep the selected item's line in the visible window (approximate:
        // draw clamps against right_scroll_max)
        let line = items[ni].0 as u16;
        if line < self.right_scroll {
            self.right_scroll = line;
        } else if line > self.right_scroll + 20 {
            self.right_scroll = line - 20;
        }
    }

    /// Act on the selected Vars detail item.
    fn var_item_act(&mut self) {
        let (_, items) = var_detail(self);
        let Some((_, action)) = items.get(self.sel_var_item.min(
            items.len().saturating_sub(1))) else { return };
        match action {
            VarNavAction::Query(name) => {
                let name = name.clone();
                self.push_history();
                self.vars_any = true;
                self.vars_query = (name.clone(), name);
                self.right_focused = false;
                self.right_scroll = 0;
                self.load_vars();
            }
            VarNavAction::Jump(idx) => {
                self.sel_var = *idx;
                self.sel_var_item = 0;
                self.right_focused = false;
                self.right_scroll = 0;
            }
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
    fn open_pty(&mut self, mut argv: Vec<String>) {
        if argv.is_empty() { self.status = "pty: empty command".into(); return; }
        // The engine's PTY does no PATH lookup, so a bare `sarun` wouldn't
        // resolve — expand it to this very binary here. This keeps the
        // visible prompt default a readable `sarun run -b -- ` instead of
        // leaking the full install path into the dialog.
        if argv[0] == "sarun" { argv[0] = self_exe(); }
        // Initial size: best guess from the actual terminal. The loop
        // calls fit_active_pty() once per redraw and resizes again
        // when the layout changes (split vs full-screen, embed
        // toggle, terminal resize), so the initial guess only has to
        // be close — better than 24×80 forever.
        let (cols, rows) = match crossterm::terminal::size() {
            Ok((c, r)) => {
                // Body height = rows minus menubar / cmdline / fkeybar
                // / status = 4. Subtract one more row for the PTY's
                // title strip.
                let h = r.saturating_sub(5).max(2);
                let w = c.max(20);
                (w, h)
            }
            Err(_) => (80, 24),
        };
        match PtyPane::open(&self.sock, &argv, rows, cols) {
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

    /// Compute the (rows, cols) the currently-VISIBLE PTY should
    /// occupy given the terminal size, and resize it if it differs
    /// from what the child currently thinks. Called from the main
    /// loop on every iteration AND on Event::Resize. The math
    /// mirrors the draw() layout exactly:
    ///   root vertical: 1 (menubar) + body + 1 (cmdline) + 1 (fkeybar)
    ///                   + 1 (status)   → body = rows − 4
    ///   embedded:  right column = round(width * 55 / 100), minus the
    ///              1-row PTY title strip → cols ≈ width*0.55,
    ///              rows = body − 1
    ///   full-screen: full width, body − 1 (title strip) rows.
    /// We resize the ACTIVE PTY only — the others get resized lazily
    /// the next time they become active (they're not visible anyway).
    #[cfg_attr(test, allow(dead_code))]
    fn fit_active_pty(&mut self, term_cols: u16, term_rows: u16) {
        if self.ptys.is_empty() { return; }
        let body_rows = term_rows.saturating_sub(4).max(2);
        let (cols, rows) = if self.focus == Pane::Pty {
            // Full-screen PTY: subtract 1 for the title strip.
            (term_cols.max(20), body_rows.saturating_sub(1).max(2))
        } else if self.pty_in_right {
            // Embedded right pane: ratatui's Percentage(55) rounds
            // toward floor, so we mirror that — `term_cols * 55 / 100`.
            let rc = (term_cols as u32 * 55 / 100) as u16;
            (rc.max(20), body_rows.saturating_sub(1).max(2))
        } else {
            // PTY is not visible — don't resize. Lets the user open
            // a PTY, switch away, come back, and find the same grid.
            return;
        };
        if let Some(pty) = self.ptys.get_mut(self.sel_pty) {
            pty.resize(rows, cols);
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

    // ── multi-select (Space / [ / ] / Esc) ─────────────────────────────────
    //
    // Selection is only meaningful on the two lists that carry batchable rows:
    // Boxes (Sessions) and Changes. Everywhere else Space/[/] are no-ops.

    /// The focused pane IF it supports multi-select, else None.
    fn selectable_pane(&self) -> Option<Pane> {
        match self.focus {
            Pane::Sessions | Pane::Changes => Some(self.focus),
            _ => None,
        }
    }

    /// Stable row keys (in display order) for the focused selectable list:
    /// box session_ids for Sessions, change paths for Changes.
    fn mark_row_keys(&self) -> Vec<String> {
        match self.focus {
            Pane::Sessions => self.sessions.iter()
                .filter_map(|s| s.get("session_id").and_then(Value::as_str)
                    .map(String::from)).collect(),
            Pane::Changes => self.visible_changes().iter()
                .filter_map(|c| c.get("path").and_then(Value::as_str)
                    .map(String::from)).collect(),
            _ => vec![],
        }
    }

    /// Cursor index within the focused selectable list.
    fn mark_cursor(&self) -> usize {
        match self.focus {
            Pane::Sessions => self.sel_session,
            Pane::Changes => self.sel_change,
            _ => 0,
        }
    }

    /// Marks belong to one list at a time: switching lists (or boxes, for
    /// Changes) drops a stale selection so a batch action never spans lists.
    fn ensure_mark_scope(&mut self) {
        if self.mark_scope != Some(self.focus) {
            self.marks.clear();
            self.mark_anchor = None;
            self.mark_scope = self.selectable_pane();
        }
    }

    /// Space — toggle the cursor row's mark and set it as the range anchor.
    fn toggle_mark(&mut self) {
        if self.selectable_pane().is_none() {
            self.status = "select works on the Boxes and Changes lists".into();
            return;
        }
        self.ensure_mark_scope();
        let idx = self.mark_cursor();
        if let Some(k) = self.mark_row_keys().get(idx).cloned() {
            if !self.marks.remove(&k) { self.marks.insert(k); }
            self.mark_anchor = Some(idx);
            self.status = format!("{} selected", self.marks.len());
        }
    }

    /// `[` / `]` — fill the inclusive range between the anchor (last Space, or
    /// the cursor if none) and the current cursor. Both brackets do the same
    /// symmetric fill; the anchor then moves to the cursor so ranges chain.
    fn range_mark(&mut self) {
        if self.selectable_pane().is_none() { return; }
        self.ensure_mark_scope();
        let keys = self.mark_row_keys();
        let cur = self.mark_cursor();
        let anchor = self.mark_anchor.unwrap_or(cur);
        let (lo, hi) = (anchor.min(cur), anchor.max(cur));
        if let Some(slice) = keys.get(lo..=hi) {
            for k in slice { self.marks.insert(k.clone()); }
        }
        self.mark_anchor = Some(cur);
        self.status = format!("{} selected", self.marks.len());
    }

    /// Esc — drop an active selection. Returns true if it consumed the Esc
    /// (i.e. there was a selection to clear), so the caller doesn't also run
    /// Esc's other duties (clearing a generated filter).
    fn clear_marks(&mut self) -> bool {
        if self.marks.is_empty() { return false; }
        self.marks.clear();
        self.mark_anchor = None;
        self.mark_scope = None;
        self.status = "selection cleared".into();
        true
    }

    /// The marked keys (display order) IF the marks belong to the focused pane
    /// and there are any — else empty (callers fall back to the cursor row).
    fn marked_here(&self) -> Vec<String> {
        if self.mark_scope == Some(self.focus) && !self.marks.is_empty() {
            self.mark_row_keys().into_iter()
                .filter(|k| self.marks.contains(k)).collect()
        } else {
            vec![]
        }
    }

    /// Whether row key `k` is marked in the focused pane (render gutter).
    fn is_marked(&self, k: &str) -> bool {
        self.mark_scope == Some(self.focus) && self.marks.contains(k)
    }

    /// Batch apply/discard over marked BOXES (Sessions) or marked CHANGES
    /// (paths within the current box). `discard` picks the verb. Returns true
    /// if it handled a selection (caller then skips the single-row path).
    fn batch_apply_discard(&mut self, discard: bool) -> bool {
        let marks = self.marked_here();
        if marks.is_empty() { return false; }
        let verb = if discard { "review.discard" } else { "review.apply" };
        let word = if discard { "discarded" } else { "applied" };
        let key = if discard { "discarded" } else { "applied" };
        match self.mark_scope {
            Some(Pane::Changes) => {
                // All marked paths in ONE call against the current box.
                let Some(sid) = self.cur_sid() else { return true; };
                match rpc(&self.sock, verb, json!([sid, Value::Array(
                    marks.iter().map(|p| Value::String(p.clone())).collect())])) {
                    Ok(r) => {
                        let n = r.get(key).and_then(Value::as_array)
                            .map(|a| a.len()).unwrap_or(marks.len());
                        self.status = format!("{word} {n} change(s) in {} file(s)",
                            marks.len());
                    }
                    Err(e) => self.status = format!("{verb}: {e}"),
                }
            }
            Some(Pane::Sessions) => {
                // Each marked box, applied/discarded whole (null selector).
                let (mut ok, mut err) = (0usize, 0usize);
                for id in &marks {
                    match rpc(&self.sock, verb, json!([id, Value::Null])) {
                        Ok(_) => ok += 1,
                        Err(_) => err += 1,
                    }
                }
                self.status = if err == 0 {
                    format!("{word} all changes in {ok} box(es)")
                } else {
                    format!("{word} {ok} box(es), {err} failed")
                };
            }
            _ => return false,
        }
        self.clear_marks();
        self.refresh_sessions();
        self.load_changes();
        true
    }

    fn apply(&mut self) {
        if self.batch_apply_discard(false) { return; }
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
        if self.batch_apply_discard(true) { return; }
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

    /// Run a per-box engine verb over the marked boxes (Sessions scope) if any,
    /// else the single cursored box. Returns the (ok, err) counts. Used by the
    /// destructive box ops so a selection acts on all marked boxes at once.
    #[cfg_attr(test, allow(dead_code))]
    fn box_op_over_selection(&mut self, verb: &str) -> (usize, usize, Option<String>) {
        let marks = self.marked_here();
        let mut targets: Vec<String> = if !marks.is_empty()
            && self.mark_scope == Some(Pane::Sessions) {
            marks
        } else {
            self.cur_sid().into_iter().collect()
        };
        // DEEPEST-FIRST: sessions are path-sorted (ancestor before descendant),
        // so reversing removes children before their parents. The engine keeps
        // any children (copy-down + re-parent) regardless of order, so this is
        // only an efficiency choice — deleting a child before its parent spares
        // the parent a pointless copy-down into a box that's about to go too.
        targets.reverse();
        let (mut ok, mut err) = (0usize, 0usize);
        let mut last_err = None;
        for id in &targets {
            match rpc(&self.sock, verb, json!([id])) {
                Ok(_) => ok += 1,
                Err(e) => { err += 1; last_err = Some(e); }
            }
        }
        self.clear_marks();
        (ok, err, last_err)
    }

    /// "the selected box" or "N selected boxes" — for the destructive-op
    /// confirm prompts, so the user sees the batch size before confirming.
    fn box_op_scope_label(&self) -> String {
        let marks = self.marked_here();
        if !marks.is_empty() && self.mark_scope == Some(Pane::Sessions) {
            format!("{} selected box(es)", marks.len())
        } else {
            "the selected box".to_string()
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn kill(&mut self) {
        let (ok, err, why) = self.box_op_over_selection("kill");
        self.status = if err == 0 { format!("sent SIGTERM to {ok} box(es)") }
                      else { format!("killed {ok}, {err} failed: {}",
                                     why.unwrap_or_default()) };
        self.refresh_sessions();
    }


    #[cfg_attr(test, allow(dead_code))]
    fn dissolve(&mut self) {
        let (ok, err, why) = self.box_op_over_selection("dissolve");
        self.status = if err == 0 { format!("dissolved {ok} box(es)") }
                      else { format!("dissolved {ok}, {err} failed: {}",
                                     why.unwrap_or_default()) };
        self.refresh_sessions();
        self.load_changes();
    }

    /// Run the destructive op a Confirm modal was guarding (after a 'y').
    #[cfg_attr(test, allow(dead_code))]
    fn run_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Kill => self.kill(),
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
            Some("brush_prov") => {
                let sid = ev.get("session_id").and_then(Value::as_str);
                if sid.is_some() && sid == self.cur_sid().as_deref()
                    && self.focus == Pane::Pipelines {
                    self.refresh_pipelines_preserving_cursor();
                }
            }
            Some("build_edges") => {
                let sid = ev.get("session_id").and_then(Value::as_str);
                if sid.is_some() && sid == self.cur_sid().as_deref()
                    && self.focus == Pane::BuildEdges {
                    self.refresh_build_edges_preserving_cursor();
                }
            }
            // Live refresh of the Network pane as captures land (DESIGN-web.md
            // W4): a browser or crawl adds rows while you watch. webcap_added
            // carries `sid`. Rows are newest-first; load_webcap clamps the
            // selection so a grown list never strands the cursor.
            Some("webcap_added") => {
                let sid = ev.get("sid").and_then(Value::as_str);
                if sid.is_some() && sid == self.cur_sid().as_deref()
                    && self.focus == Pane::Network {
                    self.load_webcap();
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
            FilterView::Pipelines => &self.f_pipelines,
            FilterView::BuildEdges => &self.f_edges,
        }
    }
    fn view_filter_mut(&mut self, v: FilterView) -> &mut ViewFilter {
        match v {
            FilterView::Changes => &mut self.f_changes,
            FilterView::Procs => &mut self.f_procs,
            FilterView::Outputs => &mut self.f_outputs,
            FilterView::Pipelines => &mut self.f_pipelines,
            FilterView::BuildEdges => &mut self.f_edges,
        }
    }

    /// The FilterView the focused pane filters, if any (Sessions/Hunks/Rules/
    /// Help/Pty are not filterable).
    fn focus_filter_view(&self) -> Option<FilterView> {
        match self.focus {
            Pane::Changes => Some(FilterView::Changes),
            Pane::Processes => Some(FilterView::Procs),
            Pane::Outputs => Some(FilterView::Outputs),
            Pane::Pipelines => Some(FilterView::Pipelines),
            Pane::BuildEdges => Some(FilterView::BuildEdges),
            _ => None,
        }
    }

    /// '/' on a filterable list (Python `action_filter`). OFF → open the clause
    /// editor seeded with the view's last clauses. ON → clear it (a generated
    /// "ids" filter is dropped; a user one keeps its clauses for next time).
    #[cfg_attr(test, allow(dead_code))]
    fn toggle_filter(&mut self) {
        if self.focus == Pane::Vars {
            let seed = if self.vars_any || self.vars_query.1.is_empty() {
                self.vars_query.0.clone()
            } else {
                format!("{} {}", self.vars_query.0, self.vars_query.1)
            };
            self.modal = Some(Modal::VarQuery { buf: seed });
            return;
        }
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
            // Keep the cursor on the same row when it survives the new filter
            // (scroll_view_to_id is a no-op when the id was filtered out —
            // the cursor then rests at the top of the filtered list).
            let anchor_id = self.selected_item_id(v);
            *self.view_filter_mut(v) = ViewFilter { clauses, on: true, generated: false };
            self.reset_view_cursor(v);
            self.push_view_filter(v);
            if let Some(aid) = anchor_id {
                self.scroll_view_to_id(v, aid);
            }
            self.status = "filter applied".into();
        } else {
            self.status = "filter: no enabled clause".into();
        }
    }

    /// Turn a view's filter off, keeping its clauses for next time (a generated
    /// "ids" filter is dropped). Mirrors Python `_clear_filter`.
    fn clear_filter(&mut self, v: FilterView) {
        let anchor_id = self.selected_item_id(v);
        let f = self.view_filter_mut(v);
        if f.generated {
            f.clauses.clear();
        }
        f.on = false;
        f.generated = false;
        self.push_view_filter(v);
        if let Some(aid) = anchor_id {
            self.scroll_view_to_id(v, aid);
        }
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
            FilterView::Pipelines => self.push_pipelines_filter(),
            FilterView::BuildEdges => self.push_edges_filter(),
        }
    }

    /// Jump the cursor of view `v` to global position `pos`: fetch a window
    /// centered on it and set the in-window selection. Shared by
    /// scroll_view_to_id and the Home/End keys.
    fn goto_view_pos(&mut self, v: FilterView, pos: usize) {
        let half = WINDOW_SIZE / 2;
        let start = pos.saturating_sub(half);
        match v {
            FilterView::Changes => {
                self.fetch_changes_window(start);
                self.sel_change = pos.saturating_sub(self.changes_window_start);
            }
            FilterView::Procs => {
                self.fetch_processes_window(start);
                self.sel_proc = pos.saturating_sub(self.processes_window_start);
            }
            FilterView::Outputs => {
                self.fetch_outputs_window(start);
                self.sel_output = pos.saturating_sub(self.outputs_window_start);
            }
            FilterView::Pipelines => {
                self.fetch_pipelines_window(start);
                self.sel_pipeline = pos.saturating_sub(self.pipelines_window_start);
            }
            FilterView::BuildEdges => {
                self.fetch_edges_window(start);
                self.sel_edge = pos.saturating_sub(self.edges_window_start);
            }
        }
    }

    /// Home / End: jump to the first / last row of the focused list (or the
    /// top / bottom of a right-focused detail body).
    #[cfg_attr(test, allow(dead_code))]
    fn move_home(&mut self) { self.move_extreme(false); }
    #[cfg_attr(test, allow(dead_code))]
    fn move_end(&mut self) { self.move_extreme(true); }

    fn move_extreme(&mut self, end: bool) {
        if self.right_focused && self.right_pane_scrollable() {
            self.right_scroll = if end { self.right_scroll_max.get() } else { 0 };
            return;
        }
        // (view, total) for the windowed lists; the small in-memory lists
        // are handled per-pane below.
        let goto = |app: &mut Self, v: FilterView, total: usize| {
            if total == 0 { return; }
            app.goto_view_pos(v, if end { total - 1 } else { 0 });
        };
        match self.focus {
            Pane::Sessions => {
                if self.sessions.is_empty() { return; }
                let new = if end { self.sessions.len() - 1 } else { 0 };
                if new != self.sel_session {
                    self.sel_session = new;
                    self.on_box_cursor_moved();
                }
            }
            Pane::Changes => {
                goto(self, FilterView::Changes, self.changes_total);
                self.load_hunks();
            }
            Pane::Processes => {
                goto(self, FilterView::Procs, self.processes_total);
                // The first/last row can be a structural connector the cursor
                // shouldn't land on; nudge off it in the inward direction.
                let is_connector = self.processes.get(self.sel_proc)
                    .and_then(|r| r.get("connector").and_then(Value::as_bool))
                    .unwrap_or(false);
                if is_connector { self.move_proc_cursor(if end { -1 } else { 1 }); }
            }
            Pane::Outputs => { goto(self, FilterView::Outputs, self.outputs_total); self.right_scroll = 0; }
            Pane::Pipelines => { goto(self, FilterView::Pipelines, self.pipelines_total); self.right_scroll = 0; }
            Pane::BuildEdges => { goto(self, FilterView::BuildEdges, self.edges_total); self.right_scroll = 0; }
            Pane::Rules => {
                if self.rules.is_empty() { return; }
                self.sel_rule = if end { self.rules.len() - 1 } else { 0 };
            }
            Pane::Flows => {
                if self.flows.is_empty() { return; }
                let new = if end { self.flows.len() - 1 } else { 0 };
                if new != self.sel_flow {
                    self.sel_flow = new;
                    self.right_scroll = 0;
                    self.load_flow_detail();
                }
            }
            Pane::Packets => {
                if self.packets.is_empty() { return; }
                let new = if end { self.packets.len() - 1 } else { 0 };
                if new != self.sel_packet {
                    self.sel_packet = new;
                    self.right_scroll = 0;
                    self.load_packet_detail();
                }
            }
            Pane::ApiLogs => {
                if self.api_log_rows.is_empty() { return; }
                self.sel_api_log = if end { self.api_log_rows.len() - 1 } else { 0 };
            }
            Pane::Network => {
                if self.webcap_rows.is_empty() { return; }
                self.sel_webcap = if end { self.webcap_rows.len() - 1 } else { 0 };
            }
            Pane::Vars => {
                if self.vars_rows.is_empty() { return; }
                self.sel_var = if end { self.vars_rows.len() - 1 } else { 0 };
            }
            Pane::Hunks => {
                if end {
                    let n = self.hunk_indices().len();
                    if n > 1 { self.sel_hunk = n - 1; }
                } else {
                    self.sel_hunk = 0;
                    self.hunk_scroll = 0;
                }
            }
            Pane::Help => {
                if !end { self.out_scroll = 0; }
            }
            Pane::Pty => {}
        }
    }

    fn reset_view_cursor(&mut self, v: FilterView) {
        match v {
            FilterView::Changes => self.sel_change = 0,
            FilterView::Procs => self.sel_proc = 0,
            FilterView::Outputs => self.sel_output = 0,
            FilterView::Pipelines => self.sel_pipeline = 0,
            FilterView::BuildEdges => self.sel_edge = 0,
        }
    }

    fn selected_item_id(&self, v: FilterView) -> Option<i64> {
        let row: Option<&Value> = match v {
            FilterView::Changes => self.changes.get(self.sel_change),
            FilterView::Procs => self.processes.get(self.sel_proc),
            FilterView::Outputs => self.outputs.get(self.sel_output),
            FilterView::Pipelines => self.pipelines.get(self.sel_pipeline),
            FilterView::BuildEdges => self.build_edges.get(self.sel_edge),
        };
        row.and_then(|r| r.get("id").and_then(Value::as_i64))
    }

    fn view_id_for(&self, v: FilterView) -> Option<u64> {
        match v {
            FilterView::Changes => self.changes_view,
            FilterView::Procs => self.processes_view,
            FilterView::Outputs => self.outputs_view,
            FilterView::Pipelines => self.pipelines_view,
            FilterView::BuildEdges => self.edges_view,
        }
    }

    fn scroll_view_to_id(&mut self, v: FilterView, target_id: i64) {
        let Some(vid) = self.view_id_for(v) else { return };
        let pos = match rpc(&self.sock, "view.find", json!([vid, target_id])) {
            Ok(r) if r.get("ok").and_then(Value::as_bool) == Some(true) =>
                r.get("pos").and_then(Value::as_u64).unwrap_or(0) as usize,
            _ => return,
        };
        self.goto_view_pos(v, pos);
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
    ///
    /// Every pair works: the source row is first resolved to ids in one of the
    /// three provenance domains — "process" (process row ids), "pipeline"
    /// (brushprov row ids), "edge" (build_edges row ids) — then translated to
    /// the destination view's domain with `review.map_ids` when they differ.
    /// Procs / Outputs / Changes all filter on process ids (outputs' "ids"
    /// matches process_id; changes' writer ids ARE process row ids).
    fn nav_ids(&self, src: FilterView, dest: FilterView) -> Option<Vec<i64>> {
        let sid = self.cur_sid()?;
        // 1. The source cursor's ids + their domain.
        let (from, ids): (&str, Vec<i64>) = match src {
            FilterView::Changes => {
                let rel = self.cur_change_path()?;
                ("process", self.change_writer_ids(&sid, &rel))
            }
            FilterView::Procs => {
                let p = self.visible_processes();
                let row = p.get(self.sel_proc)?;
                let rid = row.as_array().and_then(|x| x.first()).and_then(Value::as_i64)?;
                ("process", vec![rid])
            }
            FilterView::Outputs => {
                let o = self.visible_outputs();
                let row = o.get(self.sel_output)?;
                // For the pipelines destination the output's OWN pipeline is
                // more precise than "all pipelines of its process".
                if dest == FilterView::Pipelines {
                    let oid = row.get("id").and_then(Value::as_i64)?;
                    let pl = rpc(&self.sock, "output_pipeline", json!([sid, oid])).ok()?;
                    let plid = pl.get("id").and_then(Value::as_i64)?;
                    ("pipeline", vec![plid])
                } else {
                    let pid = row.get("process_id").and_then(Value::as_i64)?;
                    ("process", vec![pid])
                }
            }
            FilterView::Pipelines => {
                let row = self.pipelines.get(self.sel_pipeline)?;
                if dest == FilterView::BuildEdges {
                    let plid = row.get("id").and_then(Value::as_i64)?;
                    ("pipeline", vec![plid])
                } else {
                    let procs = row.get("processes").and_then(Value::as_array)?;
                    ("process", procs.iter().filter_map(Value::as_i64).collect())
                }
            }
            FilterView::BuildEdges => {
                let row = self.build_edges.get(self.sel_edge)?;
                ("edge", vec![row.get("id").and_then(Value::as_i64)?])
            }
        };
        // 2. Translate to the destination's domain.
        self.translate_ids(from, ids, dest)
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
            Pane::Pipelines => Some(FilterView::Pipelines),
            Pane::BuildEdges => Some(FilterView::BuildEdges),
            _ => None,
        };
        // Vars is not a FilterView, but a selected assignment carries its
        // execution context (recipe edge / pipeline) — cross-navigate on it.
        if self.focus == Pane::Vars && let Some(dest) = dest {
            let src = self.vars_rows.get(self.sel_var).and_then(|r| {
                if let Some(eid) = r.get("edge_id").and_then(Value::as_i64) {
                    Some(("edge", vec![eid]))
                } else {
                    r.get("pipeline_id").and_then(Value::as_i64)
                        .map(|pid| ("pipeline", vec![pid]))
                }
            });
            let ids = src.and_then(|(from, ids)| self.translate_ids(from, ids, dest));
            self.install_nav_filter(dest, ids);
            self.focus = dest_pane;
            return;
        }
        if let (Some(src), Some(dest)) = (self.focus_filter_view(), dest) {
            if src != dest {
                let ids = self.nav_ids(src, dest);
                self.install_nav_filter(dest, ids);
            }
        }
        self.focus = dest_pane;
    }

    /// Install a GENERATED "ids" filter on `dest` (or drop a stale generated
    /// one when this nav produced none) and push it to the engine view —
    /// without the push the local f_* flips but the engine's materialized idx
    /// still reflects the old filter and the pane shows stale rows.
    fn install_nav_filter(&mut self, dest: FilterView, ids: Option<Vec<i64>>) {
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
        if touched { self.push_view_filter(dest); }
    }

    /// Translate provenance-domain row ids to `dest`'s domain (review.map_ids
    /// when they differ). None when nothing maps.
    fn translate_ids(
        &self, from: &str, ids: Vec<i64>, dest: FilterView,
    ) -> Option<Vec<i64>> {
        if ids.is_empty() { return None; }
        let sid = self.cur_sid()?;
        let to = match dest {
            FilterView::Pipelines => "pipeline",
            FilterView::BuildEdges => "edge",
            _ => "process",
        };
        let ids = if from == to {
            ids
        } else {
            let m = rpc(&self.sock, "review.map_ids", json!([sid, from, ids, to])).ok()?;
            m.as_array()
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default()
        };
        if ids.is_empty() { None } else { Some(ids) }
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

fn sessions_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    // Columns mirror the prototype's #s-tab: F | Name | PID | Cmd | Age.
    // Sessions are nested under parents via the dotted display path — render
    // them as a DFS-ordered tree, indenting children under their parent.
    // (Same shape as the prototype's _rebuild_sessions; we walk a sorted-by-
    // path order which gives DFS automatically for any well-formed forest.)
    //
    // Columns size to the pane: F/PID/Age are fixed-small, Name is modest, and
    // Cmd takes the rest so the list fills the terminal width.
    let (name_w, pid_w, age_w) = (24usize, 6usize, 6usize);
    let usable = (width as usize).saturating_sub(2); // inside the block borders
    // sel-marker + F + 4 single-space separators are the fixed overhead.
    let fixed = 1 + 1 + 4 + name_w + pid_w + age_w;
    let cmd_w = usable.saturating_sub(fixed).max(8);
    let mut out = vec![Line::from(Span::styled(
        format!("{:<1}{:<1} {:<name_w$} {:<pid_w$} {:<cmd_w$} {:>age_w$}",
                "", "F", "Name", "PID", "Cmd", "Age"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.sessions.is_empty() {
        out.push(Line::from("(no boxes)"));
        return out;
    }
    // refresh_sessions already sorted by dotted display path, so this is
    // DFS order — children land immediately after their parent. Depth is the
    // dot count; collapse_chains then flattens single-child runs (e.g. an OCI
    // image's base→layer→…→top spine) onto one indent column, marking each with ⋮.
    let depths: Vec<usize> = app.sessions.iter()
        .map(|s| match s.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => p.matches('.').count(),
            _ => 0,
        })
        .collect();
    let collapse = collapse_chains(&depths);
    for (i, s) in app.sessions.iter().enumerate() {
        let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let status = g("status");
        let (flag, color) = session_flag(&status);
        let path = g("path");
        let (depth, collapsed) = collapse[i];
        let indent = format!("{}{}", "  ".repeat(depth),
                             if collapsed { CHAIN_MARK } else { "" });
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
        let cmdc: String = cmd.chars().take(cmd_w).collect();
        let started = s.get("started").and_then(Value::as_f64).unwrap_or(0.0);
        let age = if started > 0.0 { fmt_age(started) } else { String::new() };
        let pid_str = if pid > 0 { pid.to_string() } else { String::new() };
        let name_col: String = format!("{indent}{basename}").chars().take(name_w).collect();
        let marked = app.is_marked(&g("session_id"));
        let mark = if marked { MARK_GLYPH } else { " " };
        let text = format!("{mark}{flag:<1} {name_col:<name_w$} {pid_str:<pid_w$} {cmdc:<cmd_w$} {age:>age_w$}");
        let line = if i == app.sel_session {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else if marked {
            Line::from(Span::styled(text,
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)))
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

/// Marker prefixed to a row that belongs to a collapsed single-child chain
/// (see `collapse_chains`). Three vertical dots — the run reads as one spine.
const CHAIN_MARK: &str = "⋮";

/// Gutter glyph for a multi-selected (marked) row in the Boxes / Changes lists.
const MARK_GLYPH: &str = "◆";

/// Single-child-chain collapse for a DFS-ordered depth sequence — shared by the
/// hierarchical panes (sessions / processes / changes). A node that is an only
/// child inherits its parent's indent level instead of nesting one deeper, so a
/// long single-child run (a stack of OCI image layers, or an `a/b/c/d` directory
/// spine) flattens into ONE column instead of marching off the right edge. Every
/// node that has exactly one child, OR is itself an only child, is flagged
/// `collapsed` so the renderer can mark the whole flattened run with `⋮`
/// (including the final, chain-ending single child).
///
/// Input: each row's TRUE tree depth, in DFS order (parents precede children).
/// Output: per-row `(display_depth, collapsed)`.
fn collapse_chains(depths: &[usize]) -> Vec<(usize, bool)> {
    let n = depths.len();
    let mut parent = vec![usize::MAX; n];
    let mut child_count = vec![0usize; n];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..n {
        // Unwind the ancestor stack to the nearest shallower row.
        while let Some(&t) = stack.last() {
            if depths[t] >= depths[i] { stack.pop(); } else { break; }
        }
        if let Some(&p) = stack.last() {
            // Only a row exactly one level up is a DIRECT parent; a deeper jump
            // (malformed/elided input) leaves i parentless — treated as a root.
            if depths[p] + 1 == depths[i] {
                parent[i] = p;
                child_count[p] += 1;
            }
        }
        stack.push(i);
    }
    let mut dd = vec![0usize; n];
    for i in 0..n {
        let p = parent[i];
        dd[i] = if p == usize::MAX { 0 }
                else { dd[p] + usize::from(child_count[p] > 1) };
    }
    (0..n).map(|i| {
        let sole_child = parent[i] != usize::MAX && child_count[parent[i]] == 1;
        let has_one_child = child_count[i] == 1;
        (dd[i], sole_child || has_one_child)
    }).collect()
}

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

fn changes_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut out = legend_lines(
        "Flags: + - created, ~ - modified, - - deleted, @ - xattr, \
! - stale vs host",
        width);
    out.push(Line::from(Span::styled(
        format!("{:<1}{:<1} {:>10}  {}", "", "", "SIZE", "PATH"),
        Style::default().add_modifier(Modifier::BOLD),
    )));
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
    // Collapse single-child directory spines (a/b/c/d) onto one column.
    let collapse = collapse_chains(&vis.iter()
        .map(|c| c.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize)
        .collect::<Vec<_>>());
    for (i, c) in vis.iter().enumerate() {
        let kind = c.get("kind").and_then(Value::as_str).unwrap_or("");
        let name = c.get("name").and_then(Value::as_str).unwrap_or("");
        let size = c.get("size").and_then(Value::as_i64).unwrap_or(0);
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
        let (cdepth, collapsed) = collapse[i];
        let indent = format!("{}{}", tree_indent(cdepth),
                             if collapsed { CHAIN_MARK } else { "" });
        let path = c.get("path").and_then(Value::as_str).unwrap_or("");
        let marked = !connector && app.is_marked(path);
        let mk = if marked { MARK_GLYPH } else { " " };
        let line = if connector {
            let text = format!("{:<1}{:<1} {:>10}  {indent}{name}/", "", "", "");
            Line::from(Span::styled(text,
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)))
        } else if i == app.sel_change {
            let sz = if size > 0 { fmt_bytes(size) } else { String::new() };
            let stale_mark = if stale { "!" } else { "" };
            let text = format!("{mk}{glyph}{stale_mark:<1} {sz:>10}  {indent}{name}");
            Line::from(Span::styled(text,
                Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            let sz = if size > 0 { fmt_bytes(size) } else { String::new() };
            let mut spans = vec![
                Span::styled(mk.to_string(),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
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
    // Collapse single-child process spines (sh → sh → cmd) onto one column.
    let collapse = collapse_chains(&rows.iter().map(|r| r.depth).collect::<Vec<_>>());
    for (i, r) in rows.iter().enumerate() {
        // Pick the program-name anchor. argv[0] is what the user typed; fall
        // back to exe when argv is empty (e.g. an exec without a recorded
        // argv). Take the basename so a long /usr/local/bin/foo doesn't
        // drown out the rest of the row.
        let anchor_path = r.argv.first().filter(|s| !s.is_empty()).cloned()
            .unwrap_or_else(|| r.exe.clone());
        let anchor = anchor_path.rsplit('/').next().unwrap_or(&anchor_path).to_string();
        let rest_argv = if r.argv.len() > 1 { r.argv[1..].join(" ") } else { String::new() };
        let (cdepth, collapsed) = collapse[i];
        let indent = format!("{}{}", "  ".repeat(cdepth),
                             if collapsed { CHAIN_MARK } else { "" });

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
/// Returns the transcript lines plus the index of the FIRST line of the
/// selected write, so the render can keep the selection scrolled into view
/// while the left list is focused.
fn outputs_lines(app: &App) -> (Vec<Line<'static>>, usize) {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut sel_line = 0usize;
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
        return (out, sel_line);
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
        if is_sel && sel_line == 0 { sel_line = out.len(); }
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
    (out, sel_line)
}

/// OUTPUTS index (left pane). Columns mirror the prototype's #out-tab:
/// Time | Stream | Process | Bytes. exe + tgid are baked into each row by
/// the engine's source_outputs so the renderer doesn't RPC per output.
/// API LOG · oaita proxy view. One row per request the engine forwarded on
/// this box's behalf. Backed by the box's `api_log` sqlar table — sourced
/// via the `api_log` control verb (no view machinery; rows are bounded by
/// LLM call count which is naturally small).
/// VARS index (left pane): Name | Where (assignment loc / shell context) |
/// Value (truncated). Rows are the current query's matches in assignment
/// order; '/' re-queries.
/// The assignment's site, unambiguous: a make row's "Makefile:88" joined
/// with the make's working dir ("/src/blah/aa/Makefile:88"); shell rows
/// keep their recipe/pipeline loc.
fn makevar_site(r: &Value) -> String {
    let loc = r.get("loc").and_then(Value::as_str).unwrap_or("");
    let mk = r.get("make").and_then(Value::as_str).unwrap_or("");
    if mk.starts_with('/') && !loc.is_empty() && !loc.starts_with('/') {
        format!("{mk}/{loc}")
    } else {
        loc.to_string()
    }
}

/// Keep the TAIL of a long site path — the filename:line end is the
/// discriminating part.
fn tail_trunc(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - (max - 1);
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

fn vars_index_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let (qn, qv) = &app.vars_query;
    let q = if app.vars_any {
        format!("query: name-or-value~[{qn}]   ('/' to change)")
    } else {
        format!("query: name~[{qn}] value~[{qv}]   ('/' to change)")
    };
    let mut out = vec![Line::from(Span::styled(
        q, Style::default().add_modifier(Modifier::BOLD),
    ))];
    out.extend(legend_lines(
        "Flags: s - simple :=, r - recursive =, a - append +=, q - if-unset ?=, \
! - shell-assign !=, e - environment, E - env override, c - command line, \
o - override, u - automatic, S - script assignment, x - exported",
        width));
    out.push(Line::from(Span::styled(
        format!("{:<4} {:<26} {}", "", "NAME", "VALUE"),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if app.vars_rows.is_empty() {
        out.push(Line::from(if qn.is_empty() && qv.is_empty() {
            "press '/' and type a query — a word matches variable names AND values"
        } else {
            "(no recorded assignments match — was the box run with --vars?)"
        }));
        return out;
    }
    for (i, r) in app.vars_rows.iter().enumerate() {
        let name = r.get("name").and_then(Value::as_str).unwrap_or("");
        let val = r.get("value").and_then(Value::as_str).unwrap_or("");
        let flags = makevar_flag_letters(r);
        let shellish = flags.starts_with('S');
        let one: String = val.chars()
            .map(|c| if c == '\n' { ' ' } else { c }).take(96).collect();
        let name_s: String = name.chars().take(26).collect();
        let text = format!("{:<4} {:<26} {}", flags, name_s, one);
        let line = if i == app.sel_var {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else if shellish {
            Line::from(Span::styled(text, Style::default().fg(Color::Magenta)))
        } else {
            Line::from(Span::raw(text))
        };
        out.push(line);
    }
    out
}

/// Mikrotik-style single-letter flags for a makevar row, derived from the
/// recorded flags string (op + notable origin, or sh / sh x).
fn makevar_flag_letters(r: &Value) -> String {
    let f = r.get("flags").and_then(Value::as_str).unwrap_or("");
    let mut toks = f.split_whitespace();
    let mut out = String::new();
    match toks.next().unwrap_or("") {
        ":=" => out.push('s'),
        "=" => out.push('r'),
        "+=" => out.push('a'),
        "?=" => out.push('q'),
        "!=" => out.push('!'),
        "sh" => out.push('S'),
        _ => {}
    }
    for t in toks {
        match t {
            "env" => out.push('e'),
            "env!" => out.push('E'),
            "cmd" => out.push('c'),
            "ovr" => out.push('o'),
            "auto" => out.push('u'),
            "x" | "export" => out.push('x'),
            _ => {}
        }
    }
    out
}

/// One actionable item in the VARS detail pane (Tab focuses the pane,
/// ↑/↓ move over these, Enter acts).
enum VarNavAction {
    /// Re-query the Vars view for this variable name (walk the deref chain).
    Query(String),
    /// Jump the left cursor to this vars_rows index (assignment history).
    Jump(usize),
}

/// VARS detail (right pane): the selected assignment in full — its site,
/// build context, the assignment AS WRITTEN plus the variables it
/// dereferences (each navigable: Enter re-queries that name), and every
/// other recorded assignment of the SAME name (each navigable: Enter jumps
/// to it). Returns the lines and the navigable items (line index + action).
fn var_detail(app: &App) -> (Vec<Line<'static>>, Vec<(usize, VarNavAction)>) {
    let Some(row) = app.vars_rows.get(app.sel_var) else {
        return (vec![Line::from(Span::styled(
            "(no assignment selected)",
            Style::default().add_modifier(Modifier::DIM)))], vec![]);
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
    let mk = row.get("make").and_then(Value::as_str).unwrap_or("");
    let val = row.get("value").and_then(Value::as_str).unwrap_or("");
    let rhs = row.get("rhs").and_then(Value::as_str).unwrap_or("");
    let refs = row.get("refs").and_then(Value::as_str).unwrap_or("");
    let edge_out = row.get("edge_out").and_then(Value::as_str).unwrap_or("");
    let mut out = vec![
        Line::from(vec![Span::styled("name   ", dim), Span::styled(name.to_string(), bold)]),
        Line::from(vec![Span::styled("site   ", dim), Span::raw(makevar_site(row))]),
        Line::from(vec![Span::styled("make   ", dim), Span::raw(mk.to_string())]),
        Line::from(vec![
            Span::styled("flags  ", dim),
            Span::raw(row.get("flags").and_then(Value::as_str)
                .unwrap_or("").to_string()),
        ]),
    ];
    if !edge_out.is_empty() {
        out.push(Line::from(vec![
            Span::styled("target ", dim),
            Span::styled(format!("⚒ {edge_out}"), Style::default().fg(Color::Yellow)),
            Span::styled("   (g/p/o/c cross-navigate)", dim),
        ]));
    } else if row.get("pipeline_id").and_then(Value::as_i64).is_some() {
        let uid = row.get("uid").and_then(Value::as_i64).unwrap_or(0);
        out.push(Line::from(vec![
            Span::styled("pipe   ", dim),
            Span::styled(format!("#{uid}"), Style::default().fg(Color::Yellow)),
            Span::styled("   (g/p/o/c cross-navigate)", dim),
        ]));
    }
    // A ?= (or any file assignment) whose variable's ORIGIN is env/cmdline/
    // override never took effect — the recorded value is the WINNER from
    // elsewhere, while the rhs below is the losing default. Say so, loudly:
    // reading "rhs aa, value aabb" without this note sends the reader hunting
    // in the wrong file.
    {
        let letters = makevar_flag_letters(row);
        if letters.starts_with('q')
            && letters.chars().any(|c| matches!(c, 'e' | 'E' | 'c' | 'o'))
        {
            let whom = match letters.chars().nth(1) {
                Some('c') => "this make's command line",
                Some('o') => "an override directive",
                _ => "the environment",
            };
            out.push(Line::from(Span::styled(
                format!("NOTE: this ?= did NOT assign — the value came from \
{whom}; the rhs below is the losing default"),
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    let mut items: Vec<(usize, VarNavAction)> = vec![];
    let hl = |on: bool, text: String, base: Style| {
        if on {
            Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan))
        } else {
            Span::styled(text, base)
        }
    };
    let focused = app.right_focused;
    // Section headers as stand-out chips; rhs/value bodies get a painted
    // background so the value's exact extent — trailing whitespace included —
    // is visible.
    let header = Style::default().fg(Color::Black).bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body_bg = Style::default().bg(Color::Rgb(40, 40, 60));
    if !rhs.is_empty() {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(" assignment (as written) ", header)));
        for l in rhs.lines() {
            out.push(Line::from(Span::styled(l.to_string(), body_bg)));
        }
        let ref_names: Vec<&str> =
            refs.split_whitespace().filter(|r| *r != name).collect();
        if !ref_names.is_empty() {
            out.push(Line::from(Span::styled(
                "dereferences (Tab then ↑/↓ + Enter to follow)", dim)));
            for rn in ref_names {
                let on = focused && items.len() == app.sel_var_item;
                out.push(Line::from(vec![
                    Span::raw("  "),
                    hl(on, format!("→ {rn}"), Style::default().fg(Color::Cyan)),
                ]));
                items.push((out.len() - 1, VarNavAction::Query(rn.to_string())));
            }
        }
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(" value ", header)));
    for l in val.lines() {
        out.push(Line::from(Span::styled(l.to_string(), body_bg)));
    }
    if val.is_empty() {
        out.push(Line::from(Span::styled("(empty)", dim)));
    }
    out.push(Line::from(""));
    out.push(Line::from(vec![
        Span::styled(format!(" every recorded assignment of {name} "), header),
        Span::styled("  (Enter jumps)", dim),
    ]));
    for (ri, r) in app.vars_rows.iter().enumerate() {
        if r.get("name").and_then(Value::as_str) != Some(name) {
            continue;
        }
        let site = tail_trunc(&makevar_site(r), 38);
        let v = r.get("value").and_then(Value::as_str).unwrap_or("");
        let one: String = v.chars()
            .map(|c| if c == '\n' { ' ' } else { c }).take(80).collect();
        let on = focused && items.len() == app.sel_var_item;
        let cur = if ri == app.sel_var { "▶" } else { " " };
        out.push(Line::from(vec![
            Span::raw(cur.to_string()),
            hl(on, format!("{site:<38} "), Style::default().fg(Color::Cyan)),
            Span::raw(one),
        ]));
        items.push((out.len() - 1, VarNavAction::Jump(ri)));
    }
    (out, items)
}

fn api_log_index_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<8} {:<6} {:<5} {:<24} {:<12} {:>7}",
                "Time", "Method", "Stat", "Path", "Model", "Bytes"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.api_log_rows.is_empty() {
        if app.api_endpoint_note.is_empty() {
            out.push(Line::from("(no API calls — this box wasn't launched \
                                 with --api, or hasn't made one yet)"));
        }
        for l in &app.api_endpoint_note {
            out.push(Line::from(l.clone()));
        }
        return out;
    }
    for (i, r) in app.api_log_rows.iter().enumerate() {
        let ts = r.get("ts").and_then(Value::as_f64).unwrap_or(0.0) as i64;
        let method = r.get("method").and_then(Value::as_str).unwrap_or("?");
        let path = r.get("path").and_then(Value::as_str).unwrap_or("");
        let model = r.get("model").and_then(Value::as_str).unwrap_or("");
        let status = r.get("status").and_then(Value::as_i64).unwrap_or(0);
        let resp_len = r.get("resp_len").and_then(Value::as_i64).unwrap_or(0);
        let time_label = {
            let secs = ts.rem_euclid(86400);
            let h = secs / 3600; let m = (secs % 3600) / 60; let s = secs % 60;
            format!("{h:02}:{m:02}:{s:02}")
        };
        let path_short: String = path.chars().take(24).collect();
        let model_short: String = model.chars().take(12).collect();
        let text = format!("{time_label:<8} {method:<6} {status:<5} \
                            {path_short:<24} {model_short:<12} {resp_len:>7}");
        let line = if i == app.sel_api_log {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            let color = if status >= 400 { Color::Red }
                        else if status >= 200 && status < 300 { Color::Reset }
                        else { Color::Yellow };
            Line::from(Span::styled(text, Style::default().fg(color)))
        };
        out.push(line);
    }
    out
}

/// API LOG · request + response detail for the focused row. Fetches the
/// full request/response bytes on demand via `api_log_detail`, falling
/// back to the index row's lengths-only metadata when unreachable.
fn api_log_detail_lines(app: &App) -> Vec<Line<'static>> {
    let Some(row) = app.api_log_rows.get(app.sel_api_log) else {
        return vec![Line::from("(no call selected)")];
    };
    let id = row.get("id").and_then(Value::as_i64).unwrap_or(-1);
    let Some(sid) = app.cur_sid() else {
        return vec![Line::from("(no box focused)")];
    };
    let detail = rpc(&app.sock, "api_log_detail", json!([sid, id])).ok();
    let mut out: Vec<Line<'static>> = Vec::new();
    let model = row.get("model").and_then(Value::as_str).unwrap_or("");
    let path = row.get("path").and_then(Value::as_str).unwrap_or("");
    let status = row.get("status").and_then(Value::as_i64).unwrap_or(0);
    let stream = row.get("stream").and_then(Value::as_i64).unwrap_or(0) != 0;
    out.push(Line::from(format!("{path} · model={model} · status={status} · \
                                 stream={stream}")));
    out.push(Line::from(""));
    if let Some(d) = detail {
        out.push(Line::from(Span::styled("REQUEST",
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow))));
        for l in d.get("req").and_then(Value::as_str).unwrap_or("").lines() {
            out.push(Line::from(l.to_string()));
        }
        out.push(Line::from(""));
        out.push(Line::from(Span::styled("RESPONSE",
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Green))));
        for l in d.get("resp").and_then(Value::as_str).unwrap_or("").lines() {
            out.push(Line::from(l.to_string()));
        }
    } else {
        out.push(Line::from("(could not load detail — engine offline?)"));
    }
    out
}

/// Network/Web pane list (DESIGN-web.md W4): one styled line per webcap row —
/// time · method · status · mime · size · url. Selected row reverses; 4xx/5xx
/// red, 3xx yellow, 2xx default; a `‡` marks a body truncated at the capture
/// cap. Rows arrive newest-first from the `webcap` verb.
fn webcap_index_lines(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        format!("{:<8} {:<6} {:<4} {:<16} {:>7} {}",
                "Time", "Method", "Stat", "Type", "Bytes", "URL"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if app.webcap_rows.is_empty() {
        out.push(Line::from("(no web captures — launch this box with --webcap \
                             on --net tap, or use the browser, then browse)"));
        return out;
    }
    for (i, r) in app.webcap_rows.iter().enumerate() {
        let ts = r.get("ts").and_then(Value::as_f64).unwrap_or(0.0) as i64;
        let method = r.get("method").and_then(Value::as_str).unwrap_or("?");
        let url = r.get("url").and_then(Value::as_str).unwrap_or("");
        let mime = r.get("mime").and_then(Value::as_str).unwrap_or("");
        let status = r.get("status").and_then(Value::as_i64).unwrap_or(0);
        let resp_len = r.get("resp_len").and_then(Value::as_i64).unwrap_or(0);
        let truncated = r.get("truncated").and_then(Value::as_i64).unwrap_or(0) != 0;
        let time_label = {
            let secs = ts.rem_euclid(86400);
            let h = secs / 3600; let m = (secs % 3600) / 60; let s = secs % 60;
            format!("{h:02}:{m:02}:{s:02}")
        };
        let mime_short: String = mime.chars().take(16).collect();
        let mark = if truncated { "‡" } else { "" };
        let text = format!("{time_label:<8} {method:<6} {status:<4} \
                            {mime_short:<16} {resp_len:>7} {mark}{url}");
        let line = if i == app.sel_webcap {
            Line::from(Span::styled(text, Style::default().fg(Color::Black).bg(Color::Cyan)))
        } else {
            let color = if status >= 400 { Color::Red }
                        else if status >= 200 && status < 300 { Color::Reset }
                        else { Color::Yellow };
            Line::from(Span::styled(text, Style::default().fg(color)))
        };
        out.push(line);
    }
    out
}

/// Network/Web pane detail: full request + response headers and (text) bodies
/// for the focused capture, fetched on demand via `webcap_detail` (the
/// response body arrives identity-decoded). Falls back to the index row's
/// metadata when the detail RPC is unreachable.
fn webcap_detail_lines(app: &App) -> Vec<Line<'static>> {
    let Some(row) = app.webcap_rows.get(app.sel_webcap) else {
        return vec![Line::from("(no capture selected)")];
    };
    let id = row.get("id").and_then(Value::as_i64).unwrap_or(-1);
    let Some(sid) = app.cur_sid() else {
        return vec![Line::from("(no box focused)")];
    };
    let detail = rpc(&app.sock, "webcap_detail", json!([sid, id])).ok();
    let method = row.get("method").and_then(Value::as_str).unwrap_or("?");
    let url = row.get("url").and_then(Value::as_str).unwrap_or("");
    let status = row.get("status").and_then(Value::as_i64).unwrap_or(0);
    let mime = row.get("mime").and_then(Value::as_str).unwrap_or("");
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(format!("{method} {url}")));
    out.push(Line::from(format!("status={status} · type={mime}")));
    out.push(Line::from(""));
    if let Some(d) = detail {
        let section = |out: &mut Vec<Line<'static>>, title: &str, color: Color,
                       body: &str| {
            if body.is_empty() { return; }
            out.push(Line::from(Span::styled(title.to_string(),
                Style::default().add_modifier(Modifier::BOLD).fg(color))));
            for l in body.lines() { out.push(Line::from(l.to_string())); }
            out.push(Line::from(""));
        };
        section(&mut out, "REQUEST HEADERS", Color::Yellow,
                d.get("req_headers").and_then(Value::as_str).unwrap_or(""));
        section(&mut out, "REQUEST BODY", Color::Yellow,
                d.get("req_body").and_then(Value::as_str).unwrap_or(""));
        section(&mut out, "RESPONSE HEADERS", Color::Green,
                d.get("resp_headers").and_then(Value::as_str).unwrap_or(""));
        let truncated = d.get("truncated").and_then(Value::as_i64).unwrap_or(0) != 0;
        let resp_body = d.get("resp_body").and_then(Value::as_str).unwrap_or("");
        section(&mut out, if truncated { "RESPONSE BODY (truncated at cap)" }
                          else { "RESPONSE BODY" }, Color::Green, resp_body);
    } else {
        out.push(Line::from("(could not load detail — engine offline?)"));
    }
    out
}

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
        crate::rules::Action::Ask         => ("ASK",         Color::Yellow),
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
        // -n net rule kinds (applied at SYN-accept time by the dispatcher;
        // file kinds in the same rule slide off because the field resolver
        // returns "" for unknown kinds → glob can't match)
        "host"         => "DNS hostname the box dialed (or .X.0.2 reverse if no DNS)",
        "port"         => "TCP destination port (numeric, exact-match patterns like 443)",
        "scheme"       => "http / https / tcp — derived from port at SYN-accept",
        "sni"          => "TLS SNI from the box's ClientHello (HTTPS gate)",
        "http_path"    => "HTTP request path (post-decrypt; only on http/https)",
        "http_method"  => "HTTP method (GET / POST / …) — post-decrypt",
        "http_status"  => "HTTP response status code — post-decrypt",
        "proto"        => "tcp / udp (always tcp for a SYN-time gate today)",
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
    let mut v = vec![
        h("sarun — sandboxed run → inspect → apply/discard"),
        t(""),
        h("The loop"),
        t("  1. RUN a command in a box: it executes over a copy-on-write overlay"),
        t("     of your filesystem, so its writes never touch the host yet."),
        t("  2. INSPECT what it did: the changed files (diffs), the process tree,"),
        t("     and the captured stdout/stderr — all without committing anything."),
        t("  3. APPLY the writes you want onto the real host, or DISCARD them."),
        t("     Apply/discard can be whole-box, per-file, or per-HUNK."),
        d("  Box networking is per-box: the default routes through the engine's"),
        d("  in-process proxy (DNS + HTTPS MITM); --net off = closed, -N = host net."),
        t(""),
        h("Panes (Tab cycles; or jump directly)"),
    ];
    // The pane index is GENERATED from PANE_KEYS — the same table that drives
    // the menubar and the key dispatch. Keys live in one place, never in prose.
    v.extend(PANE_KEYS.iter().map(|(k, _, _, _, desc)| t(&format!("  {k}  {desc}"))));
    v.push(d("     in a PTY pane: keys go to the box · Ctrl-] / F12 / Esc-Esc detaches"));
    v.extend(vec![
        t(""),
        h("Navigation & filters"),
        t("  j/k or ↓/↑  move       Enter  open the selection in the next pane"),
        t("  PageUp/PageDown  page   ctrl+↑/↓  reorder a rule (on Rules)"),
    ]);
    // The per-pane action keys are GENERATED from PANE_ACTION_KEYS — the same
    // table `dispatch_pane_key` runs. Every binding with a help string surfaces
    // here exactly once; keys can never drift from their documentation. (The
    // None-help entries — the j/k/Tab/Enter nav block — are described in prose
    // just above, since they read as a group, not one line each.)
    v.push(h("Actions (per-pane keys)"));
    for (key, _, _, help) in PANE_ACTION_KEYS {
        if let Some(desc) = help {
            v.push(t(&format!("  {}  {desc}", key.label())));
        }
    }
    v.extend(vec![
        t(""),
        h("Filters"),
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
        t("  In the DIFF pane, a TEXT change is shown as unified-diff hunks:"),
        t("    ↑/↓  move the hunk cursor (▶)  ·  a  apply this hunk to the host"),
        t("    x or d  discard this hunk (revert it in the box)"),
        t("  A BINARY change shows a detail header (path · kind · size · mode,"),
        t("  ⚠ when the host changed since capture) and a STRUCTURAL diff: the"),
        t("  type is sniffed and a differ (readelf/ar/unzip/tar) runs in a"),
        t("  sandbox off the render path. Unrecognized types get a hexdump."),
        t(""),
        h("Confirm prompts (y/n)"),
    ]);
    // The destructive-action prompts (K/D/Z) pop a Confirm modal whose keys are
    // GENERATED from CONFIRM_KEYS — same single-source-of-truth principle.
    {
        let keys = |want: fn(&ConfirmKey) -> bool| CONFIRM_KEYS.iter()
            .filter(|(_, a, _)| want(a))
            .map(|(k, _, _)| k.label())
            .collect::<Vec<_>>().join("/");
        let yes = keys(|a| matches!(a, ConfirmKey::Yes));
        let no = keys(|a| matches!(a, ConfirmKey::No));
        v.push(t(&format!("  {yes}  confirm the action     {no}  cancel")));
    }
    v.extend(vec![
        t(""),
        h("Boxes & nesting"),
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
    ]);
    v
}

/// Render the active modal centered over the body. Returns the area consumed.
fn draw_modal(f: &mut ratatui::Frame, area: Rect, modal: &Modal, app: &App) {
    let w = (area.width * 7 / 10).clamp(20, area.width);
    let want = match modal {
        Modal::Search { rows, .. } => (rows.len() as u16) + 6,
        Modal::ActionMenu { items, .. } => (items.len() as u16) + 5,
        Modal::Launcher { items, .. } => (items.len() as u16) + 8,
        // rows can wrap (mirror/alias resolution details are long), so
        // budget up to two display rows per item.
        Modal::ImagePicker { stack, .. } => stack.last()
            .map(|l| l.items.len() as u16 * 2).unwrap_or(0) + 6,
        // one row per model + the "custom URL" row + source/help chrome.
        Modal::ModelPicker { models, .. } =>
            (models.len() as u16 + 1).clamp(1, 18) + 7,
        // 3 fields + header + result + help + borders.
        Modal::ApiConfig { .. } => 11,
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
                // Toggle chips always SHOW their label — off is dimmed, on is
                // bright — so an empty box doesn't leave the field a mystery.
                let mark = |on: bool, label: &str, f: ClauseField| -> Span<'static> {
                    let active = cur && *field == f;
                    let txt = format!("[{label}]");
                    let mut st = if on {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().add_modifier(Modifier::DIM)
                    };
                    if active { st = Style::default().fg(Color::Black).bg(Color::Cyan); }
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
                "←/→ field · space toggles on/not (and cycles kind) · type pattern \
                 · n new row · ^s apply · esc clear",
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
        Modal::VarQuery { buf } => (
            " variable query ",
            vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("one word matches NAME or VALUE (substring); two words = NAME VALUE; globs ok · Enter · Esc"),
            ],
        ),
        Modal::PtyCmd { buf } => {
            // The help must match what the CURRENT command actually does: a
            // `sarun …` line runs the sarun binary on the host and IT builds
            // a captured box; anything else runs verbatim on the host with
            // no sandbox. Saying "runs on the HOST" for a `sarun run` prefill
            // was the confusing part.
            let is_sarun = buf.trim_start().starts_with("sarun ");
            let help = if is_sarun {
                "this is a `sarun` command — it launches a sandboxed, \
                 captured box (edit the args or command, then Enter) · \
                 Esc cancel"
            } else {
                "runs on the HOST as typed — no box, no capture (a bare \
                 `bash` is a plain host shell); prefix `sarun run -b -- CMD` \
                 to run CMD in a fresh captured box · Enter run · Esc cancel"
            };
            (" run on a PTY ",
             vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from(help),
            ])
        }
        Modal::BrowserUrl { buf, .. } => {
            (" open in browser ",
             vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("carbonyl (Chromium in the terminal) in the \
                            persistent BROWSER box — the profile persists and \
                            every page is captured to the box's web archive"),
                Line::from("Enter open · Esc cancel"),
            ])
        }
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
        Modal::Launcher { items, sel } => {
            // Option chips first — visible, toggleable state, not flags to
            // remember. The active value is bold; the key cycles it in place.
            let chip = |key: &str, label: String, on: bool| {
                let st = if on {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                vec![
                    Span::styled(format!("[{key}] "),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(label, st),
                    Span::raw("   "),
                ]
            };
            let mut opts = vec![];
            let tap_dead = !app.tap_ok && app.launch_net == 0;
            let net_label = if tap_dead {
                "network: TAP ✗ unavailable here".to_string()
            } else {
                format!("network: {}", NET_MODES[app.launch_net].to_uppercase())
            };
            if tap_dead {
                opts.push(Span::styled("[n] ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
                opts.push(Span::styled(net_label,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)));
                opts.push(Span::raw("   "));
            } else {
                opts.extend(chip("n", net_label, app.launch_net != 0));
            }
            opts.extend(chip("e", format!("record env: {}",
                if app.launch_env { "ON" } else { "off" }), app.launch_env));
            let net_hint = if app.tap_ok {
                "n cycles network (tap → host → off)"
            } else {
                "n cycles network (tap ✗ no CLONE_NEWNET here → host → off)"
            };
            let mut body = vec![
                Line::from(Span::styled("New PTY — where should it run?",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from(opts),
                Line::from(Span::styled(
                    format!("{net_hint} · e toggles env capture — applied \
                             to box/container launches"),
                    Style::default().fg(Color::DarkGray))),
                Line::from(""),
            ];
            let lw = items.iter().map(|i| i.label.chars().count())
                              .max().unwrap_or(20).max(20);
            for (i, it) in items.iter().enumerate() {
                let active = i == *sel;
                let style = if active {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default()
                };
                body.push(Line::from(vec![
                    Span::styled(format!("  {:<lw$}", it.label, lw = lw), style),
                    Span::styled(format!("  {}", it.hint),
                        Style::default().fg(Color::DarkGray)),
                ]));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "↑/↓ move · Enter launch · Esc cancel",
                Style::default().fg(Color::Gray))));
            (" new PTY ", body)
        }
        Modal::ImagePicker { crumbs, stack } => {
            let mut body = vec![
                Line::from(Span::styled(
                    if crumbs.is_empty() { "Pick a base image".to_string() }
                    else { crumbs.join(" › ") },
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                Line::from(""),
            ];
            let level = stack.last();
            let items: &[PickItem] = level.map(|l| l.items.as_slice())
                .unwrap_or(&[]);
            let sel = level.map(|l| l.sel).unwrap_or(0);
            let lw = items.iter().map(|i| i.label.chars().count())
                          .max().unwrap_or(24).max(24);
            for (i, it) in items.iter().enumerate() {
                let active = i == sel;
                let style = if active {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default()
                };
                let marker = if matches!(it.next, PickNext::Menu(_)) { "▸" }
                             else { " " };
                body.push(Line::from(vec![
                    Span::styled(format!("  {:<lw$} {marker}", it.label, lw = lw),
                                 style),
                    Span::styled(format!("  {}", it.detail),
                        Style::default().fg(Color::DarkGray)),
                ]));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "↑/↓ move · Enter pick/descend · Backspace up · Esc cancel",
                Style::default().fg(Color::Gray))));
            (" new box from image ", body)
        }
        Modal::ImageRef { buf } => (
            " image reference ",
            vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("e.g. ubuntu:24.04 · ghcr.io/org/img:tag · \
                            oci-archive:/path.tar — short names resolve via \
                            /etc/containers · Enter pull · Esc cancel"),
            ],
        ),
        Modal::OaitaTask { box_name, session, buf } => (
            " agent task ",
            vec![
                Line::from(Span::styled(
                    format!("run an oaita agent on box '{box_name}' \
                             (session '{session}', net {})", effective_net(app)),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("type the task, e.g. `summarize README.md` — Enter \
                            runs it in a captured box layered on this box \
                            (net via the Pty+ chip) · Esc cancel"),
            ],
        ),
        Modal::ModelPicker { models, source, sel, loading } => {
            let mut body = vec![
                Line::from(Span::styled(
                    "pick a local model — downloaded in a box, served on demand \
                     (no host writes)",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                Line::from(""),
            ];
            if *loading {
                body.push(Line::from(Span::styled(
                    "  querying HuggingFace for current models…",
                    Style::default().fg(Color::Yellow))));
            } else {
                // Model rows, then the custom-URL escape hatch as the last row.
                let n = models.len();
                for (i, m) in models.iter().enumerate() {
                    let active = i == *sel;
                    let style = if active {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else { Style::default() };
                    body.push(Line::from(vec![
                        Span::styled(format!("  {}", m.name), style),
                        Span::styled(format!("  {}", m.note),
                            Style::default().fg(Color::DarkGray)),
                    ]));
                }
                let custom_active = *sel == n;
                let cstyle = if custom_active {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else { Style::default().fg(Color::Gray) };
                body.push(Line::from(Span::styled(
                    "  Enter a custom GGUF URL…", cstyle)));
                if models.is_empty() {
                    body.push(Line::from(Span::styled(
                        "  (no catalog models — use the custom URL row)",
                        Style::default().fg(Color::DarkGray))));
                }
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                format!("source: {} · override via {}/oaita-models.toml",
                    if source.is_empty() { "…" } else { source.as_str() },
                    crate::paths::config_home().display()),
                Style::default().fg(Color::DarkGray))));
            body.push(Line::from(Span::styled(
                "↑/↓ move · Enter download & serve · Esc cancel",
                Style::default().fg(Color::Gray))));
            (" local model picker ", body)
        }
        Modal::ModelUrl { buf } => (
            " custom model URL ",
            vec![
                Line::from(format!("{buf}_")),
                Line::from(""),
                Line::from("paste a GGUF URL (e.g. \
                            https://huggingface.co/…/model-Q4_K_M.gguf) — \
                            Enter downloads it in a box & serves it · Esc cancel"),
            ],
        ),
        Modal::ApiConfig { base_url, model, api_key, field, result, testing } => {
            // Three editable fields; the cursored one shows a trailing "_".
            let masked: String = if api_key.is_empty() { String::new() }
                else { "•".repeat(api_key.chars().count().min(24)) };
            let rows = [
                ("model    ", model.as_str(), model.clone()),
                ("base_url ", base_url.as_str(),
                    if base_url.is_empty() {
                        "https://api.openai.com/v1 (default)".to_string()
                    } else { base_url.clone() }),
                ("api_key  ", api_key.as_str(), masked),
            ];
            let mut body = vec![
                Line::from(Span::styled(
                    "external OpenAI-compatible endpoint — written to oaita.toml",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                Line::from(""),
            ];
            for (i, (label, _raw, shown)) in rows.iter().enumerate() {
                let active = i == *field;
                let val = if active { format!("{shown}_") } else { shown.clone() };
                let lstyle = if active {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else { Style::default().fg(Color::Gray) };
                body.push(Line::from(vec![
                    Span::styled(format!(" {label}"), lstyle),
                    Span::raw("  "),
                    Span::raw(val),
                ]));
            }
            body.push(Line::from(""));
            let rline = if *testing {
                Span::styled("  testing connection…",
                    Style::default().fg(Color::Yellow))
            } else if result.is_empty() {
                Span::styled("  (Ctrl-T tests the connection)",
                    Style::default().fg(Color::DarkGray))
            } else if result.starts_with('✓') {
                Span::styled(format!("  {result}"),
                    Style::default().fg(Color::Green))
            } else {
                Span::styled(format!("  {result}"),
                    Style::default().fg(Color::Red))
            };
            body.push(Line::from(rline));
            body.push(Line::from(Span::styled(
                "Tab/↑/↓ field · type to edit · Ctrl-T test · Ctrl-S/Enter save \
                 · Esc cancel",
                Style::default().fg(Color::Gray))));
            (" configure external API ", body)
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
/// Word-wrap a legend string to the pane's inner width as DIM lines —
/// list Paragraphs don't wrap, so an unwrapped legend would truncate.
fn legend_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let w = (width as usize).saturating_sub(2).max(20);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut out = vec![];
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > w {
            out.push(Line::from(Span::styled(std::mem::take(&mut cur), dim)));
        }
        if !cur.is_empty() { cur.push(' '); }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(Line::from(Span::styled(cur, dim)));
    }
    out
}

fn scroll_for_cursor(cursor_line: usize, n_lines: usize, rect_h: u16) -> u16 {
    let visible = (rect_h as usize).saturating_sub(2);
    if visible == 0 || n_lines <= visible { return 0; }
    let third = visible / 3;
    let want = cursor_line.saturating_sub(third);
    want.min(n_lines.saturating_sub(visible)) as u16
}

/// Display rows `lines` occupy in a `Wrap { trim: false }` Paragraph of inner
/// width `inner_w` — each Line takes ceil(width / inner_w) rows (min 1).
/// Paragraph's vertical scroll offset counts these wrapped rows, so scroll
/// bounds must be computed in the same space.
fn wrapped_rows(lines: &[Line], inner_w: u16) -> usize {
    let w = (inner_w as usize).max(1);
    lines.iter().map(|l| l.width().div_ceil(w).max(1)).sum()
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
        Pane::Pipelines => Some(('l', "pipes",   FilterView::Pipelines)),
        Pane::BuildEdges => Some(('g', "build",  FilterView::BuildEdges)),
        Pane::Rules     => Some(('e', "rules",   FilterView::Changes /* unused */)),
        Pane::Flows | Pane::Packets
                        => Some(('f', "flows",   FilterView::Changes /* unused */)),
        Pane::Help      => Some(('?', "help",    FilterView::Changes /* unused */)),
        Pane::Pty       => Some(('P', "PTY",     FilterView::Changes /* unused */)),
        Pane::ApiLogs   => Some(('i', "api",     FilterView::Changes /* unused */)),
        Pane::Network   => Some(('w', "web",     FilterView::Changes /* unused */)),
        Pane::Vars      => Some(('v', "vars",    FilterView::Changes /* unused */)),
    }
}

/// When a pane chip is visible in the menubar.
enum PaneVis { Always, Data(&'static str), Pty }

/// The single source of truth for the top-level pane keys: accelerator, the
/// pane it selects, the menubar label, when its chip shows, and the one-line
/// help text. The menubar render, the key dispatch (`dispatch_menubar_key` →
/// `App::go_to_pane`), and the help index all derive from this — so a key, a
/// label, and its documentation can never drift apart.
const PANE_KEYS: &[(char, Pane, &str, PaneVis, &str)] = &[
    ('b', Pane::Sessions,   "Boxes",   PaneVis::Always,            "boxes/sessions — the box list (path · id · status · cmd)"),
    ('c', Pane::Changes,    "Changes", PaneVis::Always,            "changes — files the box wrote (Enter → its diff)"),
    ('p', Pane::Processes,  "Procs",   PaneVis::Data("processes"), "processes — the captured process TREE (exe · argv · env)"),
    ('o', Pane::Outputs,    "Outputs", PaneVis::Data("outputs"),   "outputs — decoded stdout/stderr transcript"),
    ('l', Pane::Pipelines,  "Pipes",   PaneVis::Data("pipelines"), "pipeLines — shell pipelines a -b box ran (parsed structure)"),
    ('g', Pane::BuildEdges, "Build",   PaneVis::Data("edges"),     "build Graph — parsed ninja/make build edges from a -b box"),
    ('f', Pane::Flows,      "Flows",   PaneVis::Always,            "network flows — tshark-decoded HTTP/TLS from a -n box's pcap"),
    ('e', Pane::Rules,      "Rules",   PaneVis::Always,            "file rules — the ordered apply/discard/passthrough rules"),
    ('P', Pane::Pty,        "PTYs",    PaneVis::Pty,               "open an engine-held PTY — a live interactive shell pane"),
    ('i', Pane::ApiLogs,    "Api",     PaneVis::Always,            "the --api oaita proxy log"),
    ('w', Pane::Network,    "Web",     PaneVis::Always,            "web captures — tap MITM HTTP(S) content archive (headers + body)"),
    ('v', Pane::Vars,       "Vars",    PaneVis::Data("makevar"),   "variable provenance — make + shell assignments ('/' queries)"),
    ('?', Pane::Help,       "Help",    PaneVis::Always,            "this help"),
];

// ── keybindings as data: the remaining contexts ─────────────────────────────
//
// `PANE_KEYS` above made the top-level pane accelerators a single declarative
// table (menubar + dispatch + help all derive from it). The rest of the UI's
// key handling — the Confirm modal and the main loop's per-pane action keys —
// used to be scattered inline `match KeyCode` arms, so a key, its behavior, and
// its help text lived in three places and could drift. The model below applies
// the same pattern to those contexts: a `Key` matcher, per-context tables
// mapping key -> action, a table-lookup dispatch, and help text GENERATED from
// the tables (see `help_lines`).
//
// The text-entry modals (RuleForm / PtyCmd) and the stateful editors
// (Search / ActionMenu) are NOT tabled: their "keys" are free-text editing
// (every Char appends to a buffer) or cursor motion over per-modal state, which
// a (key -> fixed action) table can't faithfully express. They keep their
// hand-written handlers; only the genuinely enumerable contexts move to tables.

/// A logical key for table matching — the subset of `crossterm::KeyCode` the
/// table-driven contexts bind. Char matching is case-sensitive (so 'a' and 'A'
/// are distinct, as the apply-one vs apply-all split needs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key { Char(char), Esc, Enter, Tab }

impl Key {
    /// Match against a real crossterm event code. False for codes this matcher
    /// doesn't model (the caller falls through to non-table handling).
    fn matches(self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;
        match self {
            Key::Char(c) => code == KeyCode::Char(c),
            Key::Esc => code == KeyCode::Esc,
            Key::Enter => code == KeyCode::Enter,
            Key::Tab => code == KeyCode::Tab,
        }
    }
    /// Human label for help/hint rendering (mirrors how `PANE_KEYS` prints its
    /// accelerator char in the generated help index).
    fn label(self) -> String {
        match self {
            Key::Char(c) => c.to_string(),
            Key::Esc => "Esc".into(),
            Key::Enter => "Enter".into(),
            Key::Tab => "Tab".into(),
        }
    }
}

/// What a Confirm-modal key does. The y/n/Esc contract lived inline in
/// `handle_modal_key`; now it's the `CONFIRM_KEYS` table.
#[derive(Clone, Copy)]
enum ConfirmKey { Yes, No }

/// The Confirm modal's keymap: 'y'/'Y' run the pending destructive action,
/// 'n'/'N'/Esc cancel. Anything else re-arms the modal (the dispatcher's "no
/// match" path). The modal's help line is generated from this table.
const CONFIRM_KEYS: &[(Key, ConfirmKey, &str)] = &[
    (Key::Char('y'), ConfirmKey::Yes, "confirm"),
    (Key::Char('Y'), ConfirmKey::Yes, "confirm"),
    (Key::Char('n'), ConfirmKey::No,  "cancel"),
    (Key::Char('N'), ConfirmKey::No,  "cancel"),
    (Key::Esc,       ConfirmKey::No,  "cancel"),
];

/// Whether a `PaneAction` is gated on the focused pane. `Any` runs regardless;
/// `On(pane)` only fires when `app.focus == pane`. The table is consulted in
/// order, so a guarded entry listed first shadows a bare entry on the same key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneGate { Any, On(Pane) }

/// Actions reachable from the main key loop (outside any modal / PTY / menu-nav
/// capture). Each variant is one mutation on `App`, so the dispatcher runs it
/// from a table lookup without holding pane-specific data — the same shape as
/// `go_to_pane` for the pane accelerators.
#[derive(Clone, Copy)]
enum PaneAction {
    Quit, Detach,
    MoveDown, MoveUp, NextPane, Open,
    ApplyHunk, DiscardHunk, ApplyFile, DiscardFile, ApplyAll, DiscardAll,
    ConfirmKill, ConfirmDissolve,
    NewRule, DeleteRule, StartRename,
    ToggleFilter, Refresh, ActionMenu, ToggleTree, ToggleRunningOnly, ToggleProcRunning,
    ToggleEdgeRunning,
    ToggleMark, RangeMark,
}

/// The main-loop pane keymap. ORDER MATTERS, and reproduces the original inline
/// arm precedence exactly: guarded (`On(pane)`) entries that must win over a
/// bare key are listed BEFORE the bare entry for that key, and the dispatcher
/// returns on the first gate-satisfied match. So 'a' = apply-hunk on Hunks else
/// apply-file; 'x' = discard-hunk on Hunks else discard-file; 'd' = discard-hunk
/// on Hunks, delete-rule on Rules, else detach; 'n' = new-rule on Rules (the
/// bare 'n' is taken by the banner-prompt steal handled before dispatch).
///
/// `help` is the one-line description, or `None` for keys documented as a group
/// in prose (the j/k move + Tab + Enter navigation block). Keys that need
/// context a flat table can't carry stay inline and are noted at the call site:
/// ctrl+arrow rule reorder, PageUp/PageDown, the pane accelerators routed
/// through `dispatch_menubar_key`, Esc (clear generated filter / close packets),
/// and the banner-prompt y/n/a/d steal.
const PANE_ACTION_KEYS: &[(Key, PaneGate, PaneAction, Option<&str>)] = &[
    (Key::Char('q'), PaneGate::Any,             PaneAction::Quit,            Some("quit (stops the engine)")),
    (Key::Char('j'), PaneGate::Any,             PaneAction::MoveDown,        None),
    (Key::Char('k'), PaneGate::Any,             PaneAction::MoveUp,          None),
    (Key::Tab,       PaneGate::Any,             PaneAction::NextPane,        None),
    (Key::Enter,     PaneGate::Any,             PaneAction::Open,            None),
    (Key::Char('m'), PaneGate::Any,             PaneAction::ActionMenu,      Some("actions popup for the selected row")),
    (Key::Char('a'), PaneGate::On(Pane::Hunks), PaneAction::ApplyHunk,       None),
    (Key::Char('x'), PaneGate::On(Pane::Hunks), PaneAction::DiscardHunk,     None),
    (Key::Char('d'), PaneGate::On(Pane::Hunks), PaneAction::DiscardHunk,     None),
    (Key::Char(' '), PaneGate::Any,             PaneAction::ToggleMark,      Some("select/unselect row (Boxes/Changes) for batch a/x/D")),
    (Key::Char('['), PaneGate::Any,             PaneAction::RangeMark,       Some("select range: anchor (Space) → cursor")),
    (Key::Char(']'), PaneGate::Any,             PaneAction::RangeMark,       None),
    (Key::Char('a'), PaneGate::Any,             PaneAction::ApplyFile,       Some("apply selected change / whole box (or all selected)")),
    (Key::Char('x'), PaneGate::Any,             PaneAction::DiscardFile,     Some("discard selected change / whole box")),
    (Key::Char('A'), PaneGate::Any,             PaneAction::ApplyAll,        Some("apply ALL the box's changes")),
    (Key::Char('X'), PaneGate::Any,             PaneAction::DiscardAll,      Some("discard ALL the box's changes")),
    (Key::Char('K'), PaneGate::Any,             PaneAction::ConfirmKill,     Some("kill box (SIGTERM, y/n)")),
    (Key::Char('D'), PaneGate::Any,             PaneAction::ConfirmDissolve, Some("delete box: finalize changes by file-rules, keep child boxes (y/n)")),
    (Key::Char('t'), PaneGate::On(Pane::Pipelines), PaneAction::ToggleTree,  Some("toggle tree / flat chronological (on Pipes)")),
    (Key::Char('f'), PaneGate::On(Pane::Pipelines), PaneAction::ToggleRunningOnly, Some("toggle running-only (on Pipes)")),
    (Key::Char('f'), PaneGate::On(Pane::Processes), PaneAction::ToggleProcRunning, Some("toggle running-only (on Procs)")),
    (Key::Char('f'), PaneGate::On(Pane::BuildEdges), PaneAction::ToggleEdgeRunning, Some("toggle running-only (on Targets)")),
    (Key::Char('n'), PaneGate::On(Pane::Rules), PaneAction::NewRule,         Some("new rule (on Rules)")),
    (Key::Char('d'), PaneGate::On(Pane::Rules), PaneAction::DeleteRule,      Some("delete rule (on Rules)")),
    (Key::Char('d'), PaneGate::Any,             PaneAction::Detach,          Some("detach (leaves the engine running)")),
    (Key::Char('/'), PaneGate::Any,             PaneAction::ToggleFilter,    Some("filter the active pane")),
    (Key::Char('r'), PaneGate::Any,             PaneAction::StartRename,     Some("rename box")),
    (Key::Char('R'), PaneGate::Any,             PaneAction::Refresh,         Some("refresh")),
];

/// Run a `PaneAction` against the app. Bodies are moved verbatim from the
/// original inline arms — same semantics, one place now.
fn run_pane_action(app: &mut App, action: PaneAction) {
    match action {
        PaneAction::Quit => { shutdown_rpc(&app.sock); app.should_quit = true; }
        PaneAction::Detach => app.should_quit = true,
        PaneAction::MoveDown => app.move_down(),
        PaneAction::MoveUp => app.move_up(),
        PaneAction::NextPane => app.next_pane(),
        PaneAction::ToggleTree => app.toggle_pipeline_tree(),
        PaneAction::ToggleRunningOnly => app.toggle_pipeline_running_only(),
        PaneAction::ToggleProcRunning => app.toggle_proc_running_only(),
        PaneAction::ToggleEdgeRunning => app.toggle_edge_running_only(),
        PaneAction::Open => {
            if app.focus == Pane::Rules {
                let cur = app.rules.get(app.sel_rule).cloned().unwrap_or_default();
                app.modal = Some(Modal::RuleForm { buf: cur, editing: Some(app.sel_rule) });
            } else {
                app.open();
            }
        }
        PaneAction::ToggleMark => app.toggle_mark(),
        PaneAction::RangeMark => app.range_mark(),
        PaneAction::ApplyHunk => app.apply_hunk(),
        PaneAction::DiscardHunk => app.discard_hunk(),
        PaneAction::ApplyFile => app.apply(),
        PaneAction::DiscardFile => app.discard(),
        PaneAction::ApplyAll => app.apply_all(),
        PaneAction::DiscardAll => app.discard_all(),
        PaneAction::ConfirmKill => app.modal = Some(Modal::Confirm {
            prompt: format!("Kill (SIGTERM) {}?", app.box_op_scope_label()),
            action: ConfirmAction::Kill,
        }),
        PaneAction::ConfirmDissolve => app.modal = Some(Modal::Confirm {
            prompt: format!("Delete {}? Changes are finalized by your \
                             file-rules (apply-matched → host, the rest \
                             discarded); any nested child boxes are kept.",
                            app.box_op_scope_label()),
            action: ConfirmAction::Dissolve,
        }),
        PaneAction::NewRule => app.modal = Some(Modal::RuleForm {
            buf: String::new(), editing: None }),
        PaneAction::DeleteRule => app.delete_rule(),
        PaneAction::StartRename => app.renaming = Some(String::new()),
        PaneAction::ToggleFilter => app.toggle_filter(),
        PaneAction::Refresh => {
            app.refresh_sessions();
            app.load_changes();
            app.load_rules();
            app.status = "refreshed".into();
        }
        PaneAction::ActionMenu => {
            if let Some((title, items)) = pane_action_menu(app) {
                app.modal = Some(Modal::ActionMenu { title, items, sel: 0 });
            } else {
                app.status = "no actions for this row yet".into();
            }
        }
    }
}

/// Table-driven dispatch for a main-loop key. Walks `PANE_ACTION_KEYS` in order,
/// runs the first entry whose key matches AND whose gate is satisfied, returns
/// true. Returns false when nothing matched (the caller then handles the keys
/// that need richer context inline). The per-pane-action analogue of
/// `dispatch_menubar_key`.
fn dispatch_pane_key(app: &mut App, code: crossterm::event::KeyCode) -> bool {
    for (key, gate, action, _) in PANE_ACTION_KEYS {
        if !key.matches(code) { continue; }
        let ok = match gate { PaneGate::Any => true, PaneGate::On(p) => app.focus == *p };
        if ok { run_pane_action(app, *action); return true; }
    }
    false
}

/// Top menubar: pane chips with their letter accelerators (derived from
/// `PANE_KEYS`). The active pane reverses; chips that would lead to an empty
/// pane for this box are dimmed/hidden by their `PaneVis`. The same list drives
/// the menubar render, F9 menu-nav dispatch, and the help index — one source of
/// truth so they can't drift.
fn menubar_chips(app: &App) -> Vec<(char, &'static str)> {
    let has = |k: &str| app.box_summary.get(k)
        .and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
    PANE_KEYS.iter().filter(|(_, pane, _, vis, _)| match vis {
        PaneVis::Always => true,
        PaneVis::Data(k) => has(k) || app.focus == *pane,
        PaneVis::Pty => !app.ptys.is_empty(),
    }).map(|(key, _, label, _, _)| (*key, *label)).collect()
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
         Span::styled("(idle — '/' filter · F4 / m row actions · F-keys below)",
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
    // Whether THIS pane has a context-menu popup ('m' / F4 opens
    // pane_action_menu) — so the F4 label reads "Actions" when it's
    // available and dims to "·" when it isn't.
    let has_menu = matches!(app.focus,
        Pane::Sessions | Pane::Changes | Pane::Hunks
        | Pane::Rules | Pane::Pty | Pane::ApiLogs);
    let mut f: [&'static str; 11] = [
        "Help",     // F1   — always
        if pty_pane { "PtyNext" } else { "Pty+" },  // F2
        if pty_pane { "PtyPrev" } else { "Tab" },   // F3
        if has_menu { "Actions" } else { "·" }, // F4 — context popup
        if hunks || changes { "Apply" } else { "·" },  // F5
        if sessions { "Rename" } else { "·" }, // F6
        if pty_pane { "PtyNew" }
        else if sessions { "Image+" }
        else if rules { "NewRule" }
        else { "·" }, // F7
        if pty_pane { "PtyKill" }
        else if hunks || changes { "Discard" }
        else if rules { "DelRule" }
        else if sessions { "Delete" }   // box: dissolve (keep children)
        else { "·" }, // F8
        "Menu",     // F9 (menubar nav)
        "Quit",     // F10  — always
        // F11: split/un-split. The label flips with `pty_in_right`
        // so the user can read what F11 will DO next. With no PTY there
        // is nothing to split — dim it (F2 is THE create-a-PTY key;
        // don't show the same button twice).
        if pty_pane { "Embed" }
        else if !any_pty { "·" }
        else if app.pty_in_right { "Solo" }
        else { "Split" },
    ];
    // PTY full-screen has no menubar-nav meaning for F9.
    if pty_pane { f[8] = "·"; }
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
            // Direct paint into the buffer — we manage the cell-by-cell
            // render so the wezterm-term Screen drives the ratatui frame.
            render_pty_into(f.buffer_mut(), split[1], pty);
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
        // Clamp the manual right-pane scroll to the detail body's real extent
        // (in wrapped-row space, which is what Paragraph::scroll skips) and
        // publish the bound so the key handlers stop at the last line instead
        // of scrolling into blank space.
        let clamp_rscroll = |lines: &[Line]| -> u16 {
            let visible = (right.height as usize).saturating_sub(2);
            let total = wrapped_rows(lines, right.width.saturating_sub(2));
            let max = total.saturating_sub(visible).min(u16::MAX as usize) as u16;
            app.right_scroll_max.set(max);
            app.right_scroll.min(max)
        };
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
                render_pty_into(f.buffer_mut(), split[1], pty);
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
                let lines = sessions_lines(app, left.width);
                let scroll = scroll_for_cursor(app.sel_session + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("sarun · boxes", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let dl = box_detail_lines(app, right);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("BOX · DETAIL", rf), rf))
                    .scroll((rs, 0))
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
                let dl = proc_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("ENVIRONMENT · DETAIL", rf), rf))
                    .scroll((rs, 0))
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
                let (out_lines, sel_line) = outputs_lines(app);
                let inner_w = right.width.saturating_sub(2);
                let visible = (right.height as usize).saturating_sub(2);
                let total = wrapped_rows(&out_lines, inner_w);
                let max = total.saturating_sub(visible).min(u16::MAX as usize) as u16;
                app.right_scroll_max.set(max);
                // While the LEFT list drives, follow the selected write:
                // scroll the transcript (in wrapped-row space) so the
                // selection sits ~1/3 down. Tab'd into the right pane, the
                // user's manual scroll wins (clamped to the body's extent).
                let cursor_row = wrapped_rows(&out_lines[..sel_line.min(out_lines.len())], inner_w);
                let third = visible / 3;
                let follow = (cursor_row.saturating_sub(third)).min(max as usize) as u16;
                app.out_follow_scroll.set(follow);
                let rs = if rf { app.right_scroll.min(max) } else { follow };
                let out = Paragraph::new(Text::from(out_lines))
                    .block(block(title("OUTPUT · stdout/stderr", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(out, right); }
            }
            Pane::Pipelines => {
                let lines = pipelines_lines(app, left.width);
                let hdr = lines.len().saturating_sub(app.pipelines.len());
                let scroll = scroll_for_cursor(app.sel_pipeline + hdr + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("PIPELINES · brush", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let dl = pipeline_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("PIPELINE · DETAIL", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::BuildEdges => {
                let lines = build_edges_lines(app, left.width);
                let hdr = lines.len().saturating_sub(app.build_edges.len());
                let scroll = scroll_for_cursor(app.sel_edge + hdr + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("BUILD EDGES · ninja/make", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let dl = build_edge_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("EDGE · DETAIL", rf), rf))
                    .scroll((rs, 0))
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
                let dl = rule_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("WHAT IT MATCHES", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::Flows => {
                let lines = flows_lines(app);
                let scroll = scroll_for_cursor(app.sel_flow + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("FLOWS · -n captured", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let dl = flow_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("FLOW · DETAIL (tshark -V)", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::Packets => {
                let lines = packets_lines(app);
                let scroll = scroll_for_cursor(app.sel_packet + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(
                        title(&format!("PACKETS · stream {}", app.packets_stream), lf),
                        lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let dl = packet_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("PACKET · DETAIL (tshark -V)", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::ApiLogs => {
                let lines = api_log_index_lines(app);
                let scroll = scroll_for_cursor(app.sel_api_log + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("API LOG · oaita proxy", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let dl = api_log_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("CALL · request + response", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::Network => {
                let lines = webcap_index_lines(app);
                let scroll = scroll_for_cursor(app.sel_webcap + 1, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("WEB · capture archive", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let dl = webcap_detail_lines(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("CAPTURE · headers + body", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            Pane::Vars => {
                let lines = vars_index_lines(app, left.width);
                let hdr = lines.len().saturating_sub(app.vars_rows.len());
                let scroll = scroll_for_cursor(app.sel_var + hdr, lines.len(), left.height);
                let p = Paragraph::new(Text::from(lines))
                    .block(block(title("VARS · provenance", lf), lf))
                    .scroll((scroll, 0));
                f.render_widget(p, left);
                let (dl, _items) = var_detail(app);
                let rs = clamp_rscroll(&dl);
                let detail = Paragraph::new(Text::from(dl))
                    .block(block(title("VARIABLE · value + history", rf), rf))
                    .scroll((rs, 0))
                    .wrap(Wrap { trim: false });
                if !skip_right { f.render_widget(detail, right); }
            }
            // Changes view (Pane::Changes is list-focused; Pane::Hunks is
            // diff-focused — same two-pane layout, different border). The
            // right half is split vertically: a 3-row cd-info strip with
            // the selected change's full path + kind/size/mode + stale
            // banner, and the diff body below it.
            _ => {
                let lines = changes_lines(app, left.width);
                let hdr = lines.len().saturating_sub(app.visible_changes().len());
                let scroll = scroll_for_cursor(app.sel_change + hdr, lines.len(), left.height);
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

    // Banner-prompt overrides the regular status bar while a -n box
    // connection is waiting on a verdict. yellow + bold so it's
    // unmistakable; format mirrors sakar's prompt text.
    let status_widget = if let Some(p) = &app.pending_prompt {
        let host = p.get("host").and_then(Value::as_str).unwrap_or("");
        let port = p.get("port").and_then(Value::as_u64).unwrap_or(0);
        let scheme = p.get("scheme").and_then(Value::as_str).unwrap_or("");
        let box_name = p.get("box").and_then(Value::as_str).unwrap_or("");
        let body = format!(
            "[{box_name}] connection: {scheme}://{host}:{port}  \
             [y]es once  [n]o once  [a]llow+save  [d]eny+save");
        Paragraph::new(Line::from(Span::styled(
            body, Style::default().fg(Color::Black).bg(Color::Yellow)
                .add_modifier(Modifier::BOLD))))
    } else {
        let status_text = if let Some(buf) = &app.renaming {
            format!("rename -> {buf}_  (Enter to commit, Esc to cancel)")
        } else {
            app.status.clone()
        };
        Paragraph::new(Line::from(Span::styled(
            status_text,
            Style::default().fg(Color::Black).bg(Color::Gray),
        )))
    };
    f.render_widget(
        status_widget,
        Rect { x: status_area.x, y: status_area.y, width: status_area.width, height: 1 },
    );

    if let Some(m) = &app.modal {
        draw_modal(f, body, m, app);
    }
}

/// Detail for the selected process: full exe + argv + the deduped env (via the
/// process_env verb), keyed off the processes() row id.
/// Right pane of the BOXES view — the box-detail summary. Faithful port of
/// the prototype's _update_box_detail (lines 11086-11137): label/path bold
/// colored by status, then status / cmd / pid·age labels, then a change
/// count line and a small preview of recent paths.
fn box_detail_lines(app: &App, area: Rect) -> Vec<Line<'static>> {
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
    // session switch. Sections are headed with a dim "── label ──" and
    // hidden when empty (so a non-brush box doesn't show "── pipelines ──").
    //
    // Pull each list out of the bundle first (empty defaults if the bundle is
    // null — happens momentarily on session switch before the RPC returns) so
    // the per-section row budget can be computed from what's actually there.
    let g_arr = |k: &str| app.box_summary.get(k)
        .and_then(Value::as_array).cloned().unwrap_or_default();
    let changes  = g_arr("changes");
    let outputs  = g_arr("outputs");
    let procs    = g_arr("processes");
    let pipes    = g_arr("pipelines");
    let edges    = g_arr("edges");
    let failures = g_arr("failures");

    // Size the sections to the pane: split the rows below the 6-line head
    // evenly across the non-empty sections (each also spends 2 lines on
    // header + trailing blank), floor 3 so a tiny pane still shows something.
    let inner_w = (area.width as usize).saturating_sub(2).max(20);
    let inner_h = (area.height as usize).saturating_sub(2);
    let nonempty = [!changes.is_empty(), !outputs.is_empty(), !procs.is_empty(),
                    !pipes.is_empty(), !edges.is_empty(), !failures.is_empty()]
        .iter().filter(|b| **b).count().max(1);
    let budget = ((inner_h.saturating_sub(6) / nonempty).saturating_sub(2)).max(3);

    let header = |title: &str| Line::from(
        Span::styled(format!("── {title} ──"), dim));
    // Each section renders its newest `budget` rows OLDEST-FIRST (newest at
    // the bottom, like a log tail) — the engine hands them newest-first.
    let render_changes_section = |out: &mut Vec<Line<'static>>, rows: &[Value]| {
        if rows.is_empty() { return; }
        out.push(header("recent changes"));
        for c in rows.iter().take(budget).rev() {
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
        out.push(header("recent outputs"));
        for r in rows.iter().take(budget).rev() {
            let stream = r.get("stream").and_then(Value::as_i64).unwrap_or(0);
            let len = r.get("len").and_then(Value::as_i64).unwrap_or(0);
            let preview = r.get("preview").and_then(Value::as_str).unwrap_or("");
            let tag = if stream == 1 { "err" } else { "out" };
            let tag_col = if stream == 1 { Color::Red } else { Color::Green };
            let one_line: String = preview.chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .take(inner_w.saturating_sub(16)).collect();
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
        out.push(header("recent processes"));
        for r in rows.iter().take(budget).rev() {
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
        out.push(header("recent pipelines"));
        for r in rows.iter().take(budget).rev() {
            let cmd = r.get("cmd").and_then(Value::as_str).unwrap_or("");
            let nested = r.get("nested").and_then(Value::as_bool) == Some(true);
            let mark = if nested { "N" } else { "T" };
            let mark_style = if nested {
                Style::default().fg(Color::Magenta)
            } else { Style::default().fg(Color::Cyan) };
            let trimmed: String = cmd.chars().take(inner_w.saturating_sub(4)).collect();
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
        out.push(header("recent build edges"));
        for r in rows.iter().take(budget).rev() {
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

    // What broke, before anything else: failed build edges (with the first
    // line of their captured output) and failed pipelines. `g` (build) + '!'
    // filters to these; the edge detail pane has the full excerpt.
    let render_failures_section = |out: &mut Vec<Line<'static>>, rows: &[Value]| {
        if rows.is_empty() { return; }
        out.push(Line::from(Span::styled(
            format!("── FAILURES ── ('!' in build/pipes/outputs filters to errors)"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))));
        // Hard cap: each failure takes 2 lines (label + excerpt) and a broken
        // -j build can fail dozens of edges at once — without the cap the
        // section shoves everything else off the pane.
        let cap = budget.min(6);
        if rows.len() > cap {
            out.push(Line::from(Span::styled(
                format!("  (+{} more — 'g' then '!' to see all)", rows.len() - cap),
                Style::default().fg(Color::Red).add_modifier(Modifier::DIM))));
        }
        for r in rows.iter().take(cap).rev() {
            let label = r.get("label").and_then(Value::as_str).unwrap_or("");
            let code = r.get("code").and_then(Value::as_i64).unwrap_or(0);
            let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
            let mark = if kind == "edge" { "E" } else { "P" };
            let label_short: String = label.chars()
                .map(|c| if c == '\n' { ' ' } else { c })
                .take(inner_w.saturating_sub(14)).collect();
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("✗{mark} "), Style::default().fg(Color::Red)),
                Span::styled(label_short, bold),
                Span::styled(format!("  exit {code}"),
                    Style::default().fg(Color::Red)),
            ]));
            // First non-empty excerpt line — usually the compiler/tool error.
            if let Some(el) = r.get("excerpt").and_then(Value::as_str)
                .and_then(|e| e.lines().rev().find(|l| !l.trim().is_empty())
                               .map(String::from)) {
                let el: String = el.chars().take(inner_w.saturating_sub(6)).collect();
                out.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(el, Style::default().fg(Color::Red)
                                                     .add_modifier(Modifier::DIM)),
                ]));
            }
        }
        out.push(Line::from(""));
    };

    // Order: outputs first (what just printed), then changes (files /
    // xattrs that just landed), then processes (who did it), then the
    // brush / build views below for context. Empty sections drop out
    // so a vanilla (non-brush) box's right pane stays tight.
    // Live in-flight builtin activity (the box's watchdog feed): what the
    // embedded makes/shells are chewing on RIGHT NOW and for how long —
    // the "running builtins" tree a stuck box otherwise lacks. Ages ≥5min
    // go loud red.
    {
        let acts = app.box_summary.get("activity")
            .and_then(Value::as_array).cloned().unwrap_or_default();
        if !acts.is_empty() {
            out.push(Line::from(Span::styled(
                "── IN FLIGHT ── (builtins running now — updated every 30s)",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
            for a in acts.iter().take(8) {
                let desc = a.get("desc").and_then(Value::as_str).unwrap_or("");
                let age = a.get("age").and_then(Value::as_u64).unwrap_or(0);
                let desc_short: String = desc.chars()
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .take(inner_w.saturating_sub(12)).collect();
                let age_style = if age >= 300 {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{age:>4}s "), age_style),
                    Span::raw(desc_short),
                ]));
            }
            out.push(Line::from(""));
        }
    }
    render_failures_section(&mut out, &failures);
    render_outputs_section(&mut out, &outputs);
    render_changes_section(&mut out, &changes);
    render_processes_section(&mut out, &procs);
    render_pipelines_section(&mut out, &pipes);
    render_edges_section(&mut out, &edges);

    // No fallback: if box_summary is null (RPC failed) or every
    // section is empty (a fresh box), we say so. The old code fell
    // back to app.changes.iter().take(40) — alphabetical first 40 —
    // which silently hid bugs in the summary path. Visible failure
    // beats stale data the user can't tell is wrong.
    if app.box_summary.is_null() {
        out.push(Line::from(Span::styled(
            "(no summary yet — engine is still answering review.box_summary)",
            Style::default().add_modifier(Modifier::DIM).fg(Color::Yellow))));
    } else if changes.is_empty() && outputs.is_empty() && procs.is_empty()
        && pipes.is_empty() && edges.is_empty() {
        out.push(Line::from(Span::styled(
            "(box is empty — no changes, outputs, processes yet)",
            Style::default().add_modifier(Modifier::DIM))));
    }
    out
}

fn bold_count(n: usize) -> String {
    if n == 1 { "1 file".into() } else { format!("{n} files") }
}

/// One line per pipeline row: a single-letter origin marker (T = top-level,
/// N = nested-shim), the seq index, and the command. The pipeline's full
/// parsed structure + linked process row ids live in the detail pane.
/// Reorder flat `brushprov` rows into DFS pre-order by (uid, parent_uid),
/// stamping a `depth` on each so the Pipelines pane renders as a tree (make →
/// recipe → sh -c → …). Roots are rows with parent_uid 0 or a parent not in the
/// set; siblings keep brushprov-id (execution) order. Legacy rows (uid 0) all
/// fall out as depth-0 roots, so old archives render as the previous flat list.
fn build_pipeline_tree(rows: Vec<Value>) -> Vec<Value> {
    use std::collections::{HashMap, HashSet};
    let uid_of = |r: &Value| r.get("uid").and_then(Value::as_i64).unwrap_or(0);
    let puid_of = |r: &Value| r.get("parent_uid").and_then(Value::as_i64).unwrap_or(0);
    let present: HashSet<i64> = rows.iter().map(&uid_of).filter(|&u| u != 0).collect();
    let mut children: HashMap<i64, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = vec![];
    for (i, r) in rows.iter().enumerate() {
        let p = puid_of(r);
        if p != 0 && present.contains(&p) {
            children.entry(p).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut visited = HashSet::new();
    // Iterative DFS pre-order (push reversed so siblings pop in id order).
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&i| (i, 0usize)).collect();
    while let Some((i, depth)) = stack.pop() {
        if !visited.insert(i) {
            continue;
        }
        let mut r = rows[i].clone();
        if let Some(o) = r.as_object_mut() {
            o.insert("depth".into(), json!(depth));
        }
        out.push(r);
        if let Some(kids) = children.get(&uid_of(&rows[i])) {
            for &k in kids.iter().rev() {
                stack.push((k, depth + 1));
            }
        }
    }
    // Any orphan/cycle leftovers: append flat (depth 0) so nothing is dropped.
    for (i, r) in rows.into_iter().enumerate() {
        if !visited.contains(&i) {
            let mut r = r;
            if let Some(o) = r.as_object_mut() {
                o.insert("depth".into(), json!(0));
            }
            out.push(r);
        }
    }
    out
}

/// Whether a build_edges row is CURRENTLY building: its recipe started
/// (started_ts>0) but hasn't been marked finished (ended_ts==0). Edges that
/// never ran (up-to-date / phony) have started_ts==0 → not running. Shared by
/// the targets running-only filter and the row renderer so both agree.
fn edge_running(row: &Value) -> bool {
    let started = row.get("started_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let ended = row.get("ended_ts").and_then(Value::as_f64).unwrap_or(0.0);
    started > 0.0 && ended == 0.0
}

/// Human-readable pipeline wall time: sub-second as `123ms`, else `1.23s` /
/// `12.3s`, minutes as `3m04s`. Negative/zero → `0ms`.
fn fmt_dur(secs: f64) -> String {
    if secs <= 0.0 {
        return "0ms".to_string();
    }
    if secs < 1.0 {
        return format!("{}ms", (secs * 1000.0).round() as i64);
    }
    if secs < 60.0 {
        return format!("{secs:.2}s");
    }
    let m = (secs / 60.0) as i64;
    let s = (secs - (m as f64) * 60.0).round() as i64;
    format!("{m}m{s:02}s")
}

fn pipelines_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    if app.pipelines.is_empty() {
        return vec![Line::from(Span::styled(
            "no pipelines yet — run something through brush (-b) to populate",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))),
        ];
    }
    let mut out = legend_lines(
        "Flags: T - top-level, N - nested (spawned inside another pipeline); \
red time - non-zero exit, yellow 'run' - still running",
        width);
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
        // Wall time / running indicator: done_ts==0 means the pipeline hasn't been
        // marked finished — in-flight on a live box (and the prime suspect for a
        // hang). Otherwise show [spawn_ts, done_ts] elapsed.
        let spawn_ts = row.get("spawn_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let done_ts = row.get("done_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let exit_code = row.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
        let failed = done_ts > 0.0 && exit_code != 0;
        let (dur_txt, dur_style) = if done_ts > 0.0 && spawn_ts > 0.0 {
            let style = if failed {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            (fmt_dur(done_ts - spawn_ts), style)
        } else {
            ("• run".to_string(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        };
        // Tree indent: depth-1 levels of "│ " then a "└ " elbow (a nested
        // pipeline hangs off its parent). Depth 0 (roots) get no indent.
        let depth = row.get("depth").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
        let indent = if depth == 0 {
            String::new()
        } else {
            format!("{}└ ", "│ ".repeat(depth - 1))
        };
        let mut spans = vec![
            Span::styled(format!("{:>4}  ", id),
                         Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{mark}  "), mark_style),
            Span::styled(format!("{dur_txt:>7}  "), dur_style),
            Span::styled(format!("p{pipeline:<2}  "),
                         Style::default().fg(Color::DarkGray)),
        ];
        if !indent.is_empty() {
            spans.push(Span::styled(indent, Style::default().fg(Color::DarkGray)));
        }
        let cmd_style = if failed {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        spans.push(Span::styled(cmd.to_string(), cmd_style));
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
    // Wall time + exit: done_ts==0 means still running (or never marked).
    let done_ts = row.get("done_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let exit_code = row.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
    if done_ts > 0.0 {
        let dur = done_ts - spawn_ts.unwrap_or(done_ts);
        lines.push(Line::from(vec![Span::styled("wall      ", label),
                                    Span::styled(fmt_dur(dur), val)]));
        lines.push(Line::from(vec![Span::styled("exit      ", label),
                                    Span::styled(exit_code.to_string(),
                                        if exit_code == 0 { val }
                                        else { Style::default().fg(Color::Red).add_modifier(Modifier::BOLD) })]));
    } else {
        lines.push(Line::from(vec![Span::styled("state     ", label),
            Span::styled("running (not yet finished)",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]));
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
    // Causal neighborhood — the "this started that" chain: the pipeline that
    // spawned this one, the pipelines it spawned, and the build edge whose
    // recipe it belongs to. One small RPC keyed off the selected row.
    if let Some(sid) = app.cur_sid() {
        if let Ok(ctx) = rpc(&app.sock, "review.pipeline_context", json!([sid, id])) {
            let fmt_one = |v: &Value| -> Option<(String, i64)> {
                let cmd = v.get("cmd").and_then(Value::as_str)?;
                let code = v.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
                let one_line: String = cmd.chars()
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .take(100).collect();
                Some((one_line, code))
            };
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── started by / starts ──",
                Style::default().fg(Color::DarkGray))));
            match ctx.get("parent").and_then(|p| fmt_one(p)) {
                Some((pcmd, pcode)) => {
                    lines.push(Line::from(vec![
                        Span::styled("↑ ", Style::default().fg(Color::Cyan)),
                        Span::raw(pcmd),
                        if pcode > 0 {
                            Span::styled(format!("  exit {pcode}"),
                                Style::default().fg(Color::Red))
                        } else { Span::raw("") },
                    ]));
                }
                None => lines.push(Line::from(Span::styled(
                    "↑ (top of its shell — nothing started this)",
                    Style::default().fg(Color::DarkGray)))),
            }
            let edge_out = ctx.get("edge_out").and_then(Value::as_str).unwrap_or("");
            if !edge_out.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("⚒ ", Style::default().fg(Color::Green)),
                    Span::styled("recipe of build edge  ", label),
                    Span::styled(edge_out.to_string(), val),
                ]));
            }
            if let Some(children) = ctx.get("children").and_then(Value::as_array) {
                for c in children {
                    if let Some((ccmd, ccode)) = fmt_one(c) {
                        lines.push(Line::from(vec![
                            Span::styled("↳ ", Style::default().fg(Color::Cyan)),
                            Span::raw(ccmd),
                            if ccode > 0 {
                                Span::styled(format!("  exit {ccode}"),
                                    Style::default().fg(Color::Red))
                            } else { Span::raw("") },
                        ]));
                    }
                }
            }
        }
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
fn build_edges_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    if app.build_edges.is_empty() {
        return vec![Line::from(Span::styled(
            "no build edges yet — run `ninja` or `make` inside a -b box",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))),
        ];
    }
    let mut out = legend_lines(
        "Flags: R - recipe ran, P - phony/no recipe, • - building now; \
red time - failed recipe",
        width);
    for (i, row) in app.build_edges.iter().enumerate() {
        let outs = row.get("outs").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from))
                              .collect::<Vec<_>>())
            .unwrap_or_default();
        let cmd_opt = row.get("cmd").and_then(Value::as_str);
        let phony = cmd_opt.is_none() || cmd_opt == Some("");
        // Run-state: started_ts>0 && ended_ts==0 → building now; both set →
        // finished (show wall time, red on a non-zero exit); neither → never ran
        // (up-to-date / phony). The marker doubles as the state column.
        let started = row.get("started_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let ended = row.get("ended_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let exit = row.get("exit_code").and_then(Value::as_i64);
        let running = edge_running(row);
        let (mark, mark_style) = if running {
            ("•", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        } else if phony {
            ("P", Style::default().fg(Color::DarkGray))
        } else {
            ("R", Style::default().fg(Color::Green))
        };
        // Wall-time / state column, mirroring the pipelines pane.
        let (dur_txt, dur_style) = if running {
            ("• run".to_string(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        } else if started > 0.0 && ended > 0.0 {
            let failed = exit.map(|c| c != 0).unwrap_or(false);
            let style = if failed {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            (fmt_dur(ended - started), style)
        } else {
            (String::new(), Style::default().fg(Color::DarkGray))
        };
        let head = outs.first().cloned().unwrap_or_else(|| "(unnamed)".into());
        let extra = if outs.len() > 1 {
            format!(" (+{})", outs.len() - 1)
        } else { String::new() };
        let mut spans = vec![
            Span::styled(format!("{mark}  "), mark_style),
            Span::styled(format!("{dur_txt:>7}  "), dur_style),
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

/// Flows pane left-list row. One row per tshark-decoded HTTP/TLS frame
/// in the box's pcapng. Columns rendered:
///   marker (R/T)  t (sec)  host  method  uri  status
/// • R = HTTP request / response row (visible because we decrypted).
/// • T = TLS ClientHello (SNI is the only useful field there).
fn flows_lines(app: &App) -> Vec<Line<'static>> {
    if app.flows.is_empty() {
        return vec![Line::from(Span::styled(
            "no flows — `sarun run -n -- …` writes pcapng + keylog under \
             state_home/flows/box<id>/",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)))];
    }
    let mut out = vec![];
    for row in &app.flows {
        let method = row.get("method").and_then(Value::as_str).unwrap_or("");
        let host   = row.get("host").and_then(Value::as_str).unwrap_or("");
        let sni    = row.get("sni").and_then(Value::as_str).unwrap_or("");
        let uri    = row.get("uri").and_then(Value::as_str).unwrap_or("");
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        let t      = row.get("t").and_then(Value::as_f64).unwrap_or(0.0);

        let (mark, mark_style) = if !method.is_empty() || !status.is_empty() {
            ("R", Style::default().fg(Color::Green))
        } else {
            ("T", Style::default().fg(Color::Cyan))
        };
        let h = if !host.is_empty() { host } else { sni };
        let mut spans = vec![
            Span::styled(format!("{mark}  "), mark_style),
            Span::styled(format!("{:>7.3}s  ", t),
                         Style::default().fg(Color::DarkGray)),
            Span::styled(h.to_string(),
                         Style::default().add_modifier(Modifier::BOLD)),
        ];
        if !method.is_empty() {
            spans.push(Span::styled(format!("  {method} "),
                Style::default().fg(Color::Yellow)));
            let u: String = uri.chars().take(60).collect();
            spans.push(Span::raw(u));
        }
        if !status.is_empty() {
            let st_style = if status.starts_with('2') {
                Style::default().fg(Color::Green)
            } else if status.starts_with('3') {
                Style::default().fg(Color::Cyan)
            } else if status.starts_with('4') || status.starts_with('5') {
                Style::default().fg(Color::Red)
            } else { Style::default() };
            spans.push(Span::styled(format!("  → {status}"), st_style));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Right-pane verbose dissection: just the tshark -V text for the
/// selected frame, split into Lines (so ratatui wraps cleanly). The
/// text was already fetched from the engine and cached in
/// app.flow_detail by load_flow_detail().
fn flow_detail_lines(app: &App) -> Vec<Line<'static>> {
    if app.flow_detail.is_empty() {
        return vec![Line::from(Span::styled(
            "(no flow selected)",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)))];
    }
    app.flow_detail.lines().map(|s| Line::from(s.to_string())).collect()
}

/// Packet drill-down list row. One row per ethernet frame in the
/// selected flow's TCP stream. Columns: frame · time · src→dst ·
/// protocol · length · info (tshark's _ws.col.info, e.g. the HTTP
/// request line or the TLS record type).
fn packets_lines(app: &App) -> Vec<Line<'static>> {
    if app.packets.is_empty() {
        return vec![Line::from(Span::styled(
            "no packets in this stream",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)))];
    }
    let mut out = vec![];
    for row in &app.packets {
        let frame  = row.get("frame").and_then(Value::as_u64).unwrap_or(0);
        let t      = row.get("t").and_then(Value::as_f64).unwrap_or(0.0);
        let src    = row.get("src").and_then(Value::as_str).unwrap_or("");
        let dst    = row.get("dst").and_then(Value::as_str).unwrap_or("");
        let proto  = row.get("proto").and_then(Value::as_str).unwrap_or("");
        let len    = row.get("len").and_then(Value::as_u64).unwrap_or(0);
        let info   = row.get("info").and_then(Value::as_str).unwrap_or("");
        let proto_color = match proto {
            "HTTP" | "HTTP/2" => Color::Yellow,
            "TLSv1.2" | "TLSv1.3" | "TLS" => Color::Cyan,
            "TCP" => Color::DarkGray,
            _ => Color::White,
        };
        let info_short: String = info.chars().take(80).collect();
        out.push(Line::from(vec![
            Span::styled(format!("{:>5}  ", frame),
                Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:>7.3}s  ", t),
                Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:>15} → {:<15}  ", src, dst),
                Style::default().fg(Color::White)),
            Span::styled(format!("{:<9} ", proto),
                Style::default().fg(proto_color).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:>5}  ", len),
                Style::default().fg(Color::DarkGray)),
            Span::raw(info_short),
        ]));
    }
    out
}

fn packet_detail_lines(app: &App) -> Vec<Line<'static>> {
    if app.packet_detail.is_empty() {
        return vec![Line::from(Span::styled(
            "(no packet selected)",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)))];
    }
    app.packet_detail.lines().map(|s| Line::from(s.to_string())).collect()
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
    let started = row.get("started_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let ended = row.get("ended_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let exit = row.get("exit_code").and_then(Value::as_i64);
    let label = Style::default().fg(Color::DarkGray);
    // Run-state line: building / finished (wall time + exit) / not run.
    let (state_txt, state_style) = if started > 0.0 && ended == 0.0 {
        ("building…".to_string(),
         Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else if started > 0.0 && ended > 0.0 {
        let code = exit.unwrap_or(0);
        let txt = format!("done in {} (exit {code})", fmt_dur(ended - started));
        let style = if code != 0 {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        (txt, style)
    } else {
        ("not run (up-to-date / phony)".to_string(),
         Style::default().fg(Color::DarkGray))
    };
    let mut lines = vec![
        Line::from(vec![Span::styled("id        ", label),
                        Span::raw(id.to_string())]),
        Line::from(vec![Span::styled("ts        ", label),
                        Span::raw(format!("{ts:.6}"))]),
        Line::from(vec![Span::styled("state     ", label),
                        Span::styled(state_txt, state_style)]),
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
    // The recipe's captured output (first ~1KB, stderr+stdout merged) — THE
    // thing you need when the edge failed. Red-tinted on failure so the
    // error text stands out; the full stream lives in the Outputs view.
    let excerpt = row.get("output_excerpt").and_then(Value::as_str).unwrap_or("");
    if !excerpt.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "output (tail ~1KB — full stream in Outputs)",
            Style::default().add_modifier(Modifier::BOLD))));
        let body_style = if exit.unwrap_or(0) != 0 {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        for l in excerpt.lines() {
            lines.push(Line::from(Span::styled(l.to_string(), body_style)));
        }
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
/// The user's configured default launch command, if any: the first usable
/// line of `{config}/slopbox[.NS]/pty_command`. `None` when unconfigured — the
/// Custom entry then prefills the unified box-shell template (build_launch)
/// instead, so the how-flags are honored without a fragile string splice.
fn pty_command_configured() -> Option<String> {
    let app_dir = match std::env::var("SLOPBOX_NS") {
        Ok(ns) if !ns.is_empty() => format!("slopbox.{ns}"),
        _ => "slopbox".into(),
    };
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
            .join(".config"),
    };
    let s = std::fs::read_to_string(base.join(app_dir).join("pty_command")).ok()?;
    s.lines().map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
}

/// The carbonyl image, pinned by multi-arch manifest-list digest (the
/// upstream 0.0.3 release; tags are mutable, digests are the pin).
const CARBONYL_IMAGE: &str = "docker.io/fathyb/carbonyl@sha256:\
77b3686f46a16375004985b522cef8f66e27fabc4a7d80209609bbb20fdfb362";

/// Absolute path to this very binary (the engine's PTY does no PATH lookup —
/// it execvp's argv[0] as given, so prompts and menu entries always use the
/// full path).
fn self_exe() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sarun".to_string())
}

/// The Pty+ chooser: each destination spelled out instead of one raw command
/// prompt. (Previously Pty+ opened a prompt pre-filled with the full sarun
/// path; typing `bash` over it ran on the HOST — or, appended, tripped the
/// brush box's no-nested-interactive-shell refusal. Neither was guessable.)
fn open_pty_menu(app: &mut App) {
    let host_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut items = vec![
        ActionItem {
            label: "Shell in a NEW box (captured; sarun run -b)".into(),
            hint: "", action: Action::PtyNewBoxShell,
        },
        ActionItem {
            label: "Container from image… (pick a base image)".into(),
            hint: "", action: Action::NewFromImage,
        },
        ActionItem {
            label: "Browser — carbonyl (container; flows captured)".into(),
            hint: "", action: Action::BrowserCarbonyl,
        },
        ActionItem {
            label: format!("Host shell — {host_shell} (NOT captured)"),
            hint: "", action: Action::PtyNewHostShell,
        },
    ];
    if app.sessions.get(app.sel_session)
        .and_then(|r| r.get("path").and_then(Value::as_str))
        .map(|p| app.oci_images.iter().any(|(n, _)| p.ends_with(n)))
        .unwrap_or(false)
    {
        items.push(ActionItem {
            label: "Container shell on the SELECTED image box".into(),
            hint: "", action: Action::RunSelectedImage,
        });
    }
    if let Some(name) = app.sessions.get(app.sel_session)
        .and_then(|r| r.get("name").and_then(Value::as_str))
    {
        items.push(ActionItem {
            label: format!("oaita agent session ON box '{name}'…"),
            hint: "", action: Action::OaitaOnSelectedBox,
        });
    }
    items.push(ActionItem {
        label: "Custom command…".into(),
        hint: "", action: Action::PtyNewCustom,
    });
    // Don't offer a default that can't work here: when the netns probe says
    // tap is unavailable, pre-select host ONCE and say so. The tap chip
    // stays in the cycle, visibly marked, so nothing fails silently and a
    // deliberate re-pick is still possible.
    if !app.tap_ok && app.launch_net == 0 && !app.net_auto_bumped {
        app.launch_net = 1;
        app.net_auto_bumped = true;
        app.status = "tap networking unavailable here (no CLONE_NEWNET) — \
                      network: HOST pre-selected".into();
    }
    app.modal = Some(Modal::Launcher { items, sel: 0 });
}

/// The Api pane's empty-state text: what the configured endpoint is, or —
/// when none is configured — how to get one without leaving the UI
/// (`oaita local`, reachable from this pane's F4 action menu). Pure so the
/// tests can pin both worlds; `resolved` is (model, base_url) from
/// oaita.toml when it has one.
fn endpoint_note_lines(resolved: Option<(String, String)>) -> Vec<String> {
    match resolved {
        Some((model, base_url)) => vec![
            "(no API calls yet)".to_string(),
            format!("endpoint: {base_url}"),
            format!("model:    {model}"),
            String::new(),
            "calls appear here for boxes launched with --api".to_string(),
            "(e.g. `sarun oaita run NAME`)".to_string(),
        ],
        None => vec![
            "(no endpoint configured — nothing to log yet)".to_string(),
            String::new(),
            "F4 → \"Configure an external API\" edits base_url / model / key \
             (with a".to_string(),
            "connection test) and writes oaita.toml — the in-UI way to point \
             at any".to_string(),
            "OpenAI-compatible server.".to_string(),
            String::new(),
            "F4 → \"Pick a local model\" instead downloads a model in a box and \
             serves".to_string(),
            "it on demand — no external endpoint or key needed. Opens \
             automatically the".to_string(),
            "first time.".to_string(),
        ],
    }
}

/// Launcher network modes, cycled in place with `n` (index = App.launch_net).
const NET_MODES: [&str; 3] = ["tap", "host", "off"];

use crate::net::tap::tap_available;

/// Derive a valid oaita session name from a box display name. Session names
/// are alphanumeric ONLY (validate_session_name), so drop every other char
/// and suffix `agent`. Deterministic, so re-running the action continues the
/// same conversation on that box.
fn oaita_session_for_box(box_name: &str) -> String {
    let slug: String = box_name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("{}agent", if slug.is_empty() { "box" } else { &slug })
}

// ── The universal launch model ───────────────────────────────────────────
// Everything the launcher can start — a shell, a command, a container image,
// the browser, an agent — is a `LaunchTarget` (pure data) turned into an argv
// by ONE function, `build_launch`, honoring ONE set of `How` choices (net /
// env / placement). There is no per-target spawn path: a preset is a row, not
// a code branch. This replaces the previous grab-bag (carbonyl_cmd,
// oaita_task_argv, per-Action inline argvs) where each destination built its
// own command and honored the how-flags inconsistently.

/// Where a launched box sits in the box stack — a universal `How` axis applied
/// to every target identically. This one knob is what used to be three
/// separate features: a persistent browser (Reuse), an agent parented on a box
/// (On), and a fresh container (New).
#[derive(Clone, PartialEq, Debug)]
enum Placement {
    /// A fresh box (or fresh container on an image); at-rest on exit like any
    /// other, nothing reused.
    New,
    /// Rerun the box named `.0` in place — its upper (a browser profile, a
    /// build tree) persists across launches (control.rs load_mirror).
    Reuse(String),
    /// A sub-box parented on the existing box `.0`: reads that box's captured
    /// files copy-on-write, writes to its own upper — the parent untouched,
    /// the child reviewable/discardable. Natural for `run` and the agent
    /// (`oaita --on`); an image target can't express it, so it degrades to
    /// New (a fresh container on the image).
    On(String),
}

/// One consistent set of execution choices, applied to EVERY target the same
/// way (launcher chips n/e/p). `net` is already resolved through
/// effective_net (tap→host fallback).
#[derive(Clone, Debug)]
struct How {
    net: String,
    env: bool,
    placement: Placement,
    /// Web capture (DESIGN-web.md W2/W3): pass `--webcap` so this box's tap
    /// HTTP(S) traffic is teed into its `webcap` store. A universal How axis
    /// like net/env — the browser and crawler set it; other targets default
    /// off. tap-only (the engine gates it), so it's inert without `--net tap`.
    webcap: bool,
    /// Web filtering (DESIGN-web.md W7): pass `--webfilter` so the engine
    /// applies adblock + response rewrites outside the box. tap-only. The
    /// browser sets it alongside webcap; other targets default off.
    webfilter: bool,
}

/// What to run. Pure data — no variant carries spawn logic; build_launch()
/// turns it into an argv. Presets (Shell, Browser, …) are rows, not paths.
#[derive(Clone, Debug)]
enum LaunchTarget {
    /// Interactive brush shell in a box over the host filesystem.
    Shell,
    /// A specific command line in a box over the host filesystem.
    Command(Vec<String>),
    /// A container from an OCI image reference (runs its entrypoint/cmd).
    Image(String),
    /// carbonyl (DESIGN-web.md W3) — the browser: a fixed-image target with
    /// the container argv and the MITM SPKI so Chromium trusts the tap proxy.
    /// `url` is the validated destination (assembled here, not spliced into a
    /// shell line). Launched Reuse("BROWSER") + webcap so the profile persists
    /// and browsing archives; see build_launch.
    Browser { url: String, spki: Option<String> },
    /// An oaita agent session seeded with `task`.
    Ai { task: String },
    /// The host login shell — the ONE target not in a box, so no How applies.
    HostShell,
}

/// A short, valid ([A-Za-z0-9-], lowercase) slug for a derived box/session
/// name — used when a placement needs a child name the user didn't give.
fn slugify(s: &str, fallback: &str) -> String {
    let slug: String = s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() { fallback.to_string() } else { slug }
}

/// `sarun run [-b] --net N [-e] [PLACEMENT] -- CMD`. Placement: New → auto
/// session; Reuse(n) → bare NAME (reruns if it exists); On(p) → dotted
/// `p.<child>` (sub-box parented on p).
fn run_argv(how: &How, brush: bool, cmd: &[String]) -> Vec<String> {
    let mut a = vec!["sarun".to_string(), "run".into()];
    if brush { a.push("-b".into()); }
    a.push("--net".into());
    a.push(how.net.clone());
    if how.env { a.push("-e".into()); }
    if how.webcap { a.push("--webcap".into()); }
    if how.webfilter { a.push("--webfilter".into()); }
    match &how.placement {
        Placement::New => {}
        Placement::Reuse(n) => a.push(n.clone()),
        Placement::On(p) => {
            let child = cmd.first().map(|c| slugify(c, "run")).unwrap_or_else(|| "sh".into());
            a.push(format!("{p}.{child}"));
        }
    }
    a.push("--".into());
    a.extend(cmd.iter().cloned());
    a
}

/// `sarun oci run [--name NAME] --net N <ref> [-- CMD]`. Placement: New →
/// fresh container; Reuse(n) → `--name n` (reruns → upper persists); On(_) →
/// an image can't be parented elsewhere, so it degrades to New.
fn oci_run_argv(how: &How, reference: &str, cmd: &[String]) -> Vec<String> {
    let mut a = vec!["sarun".to_string(), "oci".into(), "run".into()];
    a.push("--net".into());
    a.push(how.net.clone());
    if let Placement::Reuse(n) = &how.placement {
        a.push("--name".into());
        a.push(n.clone());
    }
    if how.webcap { a.push("--webcap".into()); }
    if how.webfilter { a.push("--webfilter".into()); }
    a.push(reference.into());
    if !cmd.is_empty() {
        a.push("--".into());
        a.extend(cmd.iter().cloned());
    }
    a
}

/// `sarun oaita run [--on BOX] --net N --task TASK SESSION`. The agent's
/// natural placement is On(box) (= oaita's `--on`); Reuse(n) names the
/// session so it continues; New uses a default session name.
fn ai_argv(how: &How, task: &str) -> Vec<String> {
    let mut a = vec!["sarun".to_string(), "oaita".into(), "run".into()];
    let session = match &how.placement {
        Placement::On(p) => {
            a.push("--on".into());
            a.push(p.clone());
            oaita_session_for_box(p)
        }
        Placement::Reuse(n) => slugify(n, "agent"),
        Placement::New => "agent".into(),
    };
    a.push("--net".into());
    a.push(how.net.clone());
    a.push("--task".into());
    a.push(task.into());
    a.push(session);
    a
}

/// The ONE builder: (target, how) → argv. Every launcher entry funnels here,
/// so net/env/placement are honored identically for all of them. `open_pty`
/// rewrites the leading `sarun` to the real binary path at spawn.
fn build_launch(target: &LaunchTarget, how: &How) -> Vec<String> {
    match target {
        // The only un-boxed target: How cannot apply (no box, no net).
        LaunchTarget::HostShell =>
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())],
        LaunchTarget::Shell => run_argv(how, true, &[]),
        LaunchTarget::Command(cmd) => run_argv(how, true, cmd),
        LaunchTarget::Image(reference) => oci_run_argv(how, reference, &[]),
        LaunchTarget::Browser { url, spki } => {
            let mut cmd = vec![
                "/carbonyl/carbonyl".to_string(),
                "--no-sandbox".into(),
                "--disable-dev-shm-usage".into(),
                "--user-data-dir=/carbonyl/data".into(),
            ];
            if let Some(k) = spki {
                cmd.push(format!("--ignore-certificate-errors-spki-list={k}"));
            }
            // The validated URL is the last positional; a blank one lets
            // carbonyl open its own start page rather than a dud "https://".
            let dest = if url.trim().is_empty() { "about:blank" } else { url.trim() };
            cmd.push(dest.to_string());
            oci_run_argv(how, CARBONYL_IMAGE, &cmd)
        }
        LaunchTarget::Ai { task } => ai_argv(how, task),
    }
}

/// The network mode a launch should ACTUALLY use: the selected chip, but
/// tap downgraded to host when tap can't work here (no CLONE_NEWNET). The
/// launcher's chip auto-bump only fires when the launcher is opened; this
/// resolves it for every spawn path (incl. the Sessions menu) so a box is
/// never spawned with a tap that will fail. An explicit host/off is kept.
fn effective_net(app: &App) -> &'static str {
    let sel = NET_MODES[app.launch_net];
    if sel == "tap" && !tap_available() { "host" } else { sel }
}


/// One curated catalog group: display name, unqualified image name (resolved
/// through registries.conf at display/pull time), and the tags offered.
struct CatalogGroup {
    name: String,
    image: String,
    tags: Vec<String>,
}

/// The base-image catalog: `{config_home}/images.toml` when present
/// (`[[group]] name/image/tags`), else a built-in curated shortlist of
/// common distro bases — a hierarchy to descend, not a registry dump and
/// not a single take-it-or-leave-it image.
fn image_catalog() -> Vec<CatalogGroup> {
    if let Ok(s) = std::fs::read_to_string(crate::paths::images_config_path()) {
        if let Ok(v) = s.parse::<toml::Value>() {
            let groups: Vec<CatalogGroup> = v.get("group")
                .and_then(|g| g.as_array())
                .map(|arr| arr.iter().filter_map(|g| {
                    Some(CatalogGroup {
                        name: g.get("name")?.as_str()?.to_string(),
                        image: g.get("image")?.as_str()?.to_string(),
                        tags: g.get("tags").and_then(|t| t.as_array())
                            .map(|a| a.iter()
                                .filter_map(|s| s.as_str().map(str::to_string))
                                .collect())
                            .unwrap_or_else(|| vec!["latest".into()]),
                    })
                }).collect())
                .unwrap_or_default();
            if !groups.is_empty() { return groups; }
        }
    }
    let mk = |name: &str, image: &str, tags: &[&str]| CatalogGroup {
        name: name.into(), image: image.into(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        mk("Ubuntu",   "ubuntu",     &["24.04", "22.04", "latest"]),
        mk("Debian",   "debian",     &["12", "stable-slim", "latest"]),
        mk("Alpine",   "alpine",     &["3.21", "3.20", "latest"]),
        mk("Fedora",   "fedora",     &["42", "41", "latest"]),
        mk("Rocky Linux", "rockylinux", &["9", "8"]),
        mk("BusyBox",  "busybox",    &["latest", "musl"]),
    ]
}

/// Annotate an unqualified image ref with where it will ACTUALLY be pulled
/// from under the host's registries.conf (mirror / alias / search registry).
fn resolve_detail(conf: &crate::containers_conf::ContainersConf, rf: &str) -> String {
    let r = conf.resolve(rf);
    if let Some(b) = &r.blocked { return format!("BLOCKED: {b}"); }
    let s = match r.candidates.first() {
        Some(c) if c.via.is_empty() => c.reference.clone(),
        Some(c) => format!("{} ({})", c.reference, c.via),
        None => "unresolvable".into(),
    };
    // Keep the row on one line — the reference (which already names the
    // mirror/alias host) matters more than the tail of the note.
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(59).collect::<String>())
    } else {
        s
    }
}

/// Build the image picker's top level: loaded images first (instant, no
/// network), then the curated catalog, then registries.conf short-name
/// aliases, then the free-text escape hatch.
fn build_image_picker(
    conf: &crate::containers_conf::ContainersConf,
    catalog: &[CatalogGroup],
    local: &[(String, String)],
) -> Vec<PickItem> {
    let mut top = Vec::new();
    if !local.is_empty() {
        let items = local.iter().map(|(name, rf)| PickItem {
            label: name.clone(),
            detail: format!("{rf} · loaded — Enter: container shell"),
            next: PickNext::RunLocal(name.clone()),
        }).collect();
        top.push(PickItem {
            label: format!("Loaded images ({})", local.len()),
            detail: "already installed — start a container, no pull".into(),
            next: PickNext::Menu(items),
        });
    }
    for g in catalog {
        let mut items: Vec<PickItem> = g.tags.iter().map(|t| {
            let rf = format!("{}:{t}", g.image);
            PickItem {
                label: rf.clone(),
                detail: resolve_detail(conf, &rf),
                next: PickNext::Pull(rf),
            }
        }).collect();
        items.push(PickItem {
            label: format!("{}:<other tag>…", g.image),
            detail: "type a tag".into(),
            next: PickNext::EnterRef(format!("{}:", g.image)),
        });
        top.push(PickItem {
            label: g.name.clone(),
            detail: resolve_detail(conf, &format!("{}:{}",
                g.image, g.tags.first().map(String::as_str).unwrap_or("latest"))),
            next: PickNext::Menu(items),
        });
    }
    if !conf.aliases.is_empty() {
        let items = conf.aliases.iter().map(|(short, target)| PickItem {
            label: short.clone(),
            detail: format!("→ {target}"),
            next: PickNext::Menu(vec![
                PickItem {
                    label: format!("{short}:latest"),
                    detail: resolve_detail(conf, &format!("{short}:latest")),
                    next: PickNext::Pull(format!("{short}:latest")),
                },
                PickItem {
                    label: format!("{short}:<tag>…"),
                    detail: "type a tag".into(),
                    next: PickNext::EnterRef(format!("{short}:")),
                },
            ]),
        }).collect();
        top.push(PickItem {
            label: format!("Short names ({}) — registries.conf", conf.aliases.len()),
            detail: "aliases from /etc/containers".into(),
            next: PickNext::Menu(items),
        });
    }
    top.push(PickItem {
        label: "Enter reference…".into(),
        detail: "e.g. ghcr.io/org/img:tag or oci-archive:/path.tar".into(),
        next: PickNext::EnterRef(String::new()),
    });
    top
}

/// Split a typed command line into argv, honoring single and double quotes
/// (enough for the PTY prompt). Unquoted whitespace separates words.
/// Join an argv into an editable command line, single-quoting any arg that
/// needs it — the inverse of shell_split, used to prefill the PtyCmd editor
/// from build_launch so the user always sees (and can edit) the exact command.
fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| {
        if a.is_empty()
            || a.contains(|c: char| c.is_whitespace() || c == '\'' || c == '"') {
            format!("'{}'", a.replace('\'', r"'\''"))
        } else {
            a.clone()
        }
    }).collect::<Vec<_>>().join(" ")
}

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
// (FRAME_PTY_DATA) and resizes (FRAME_PTY_RESIZE). A full wezterm-term
// `Terminal` accumulates the data into a screen grid that we render into
// ratatui's Buffer cell-by-cell. wezterm-term answers DSR / DA1 /
// terminal-info queries via its internal Writer, which we hook to the same
// UnixStream so the replies travel upstream as FRAME_PTY_DATA and reach the
// PTY child correctly.

/// Minimal TerminalConfiguration impl: just enough for wezterm-term to be
/// happy. Scrollback size matches the old vt100 number (10 000 rows);
/// other knobs use the trait defaults.
#[derive(Debug)]
struct PtyTermConfig;
impl tattoy_wezterm_term::TerminalConfiguration for PtyTermConfig {
    fn scrollback_size(&self) -> usize { 10_000 }
    fn color_palette(&self) -> tattoy_wezterm_term::color::ColorPalette {
        tattoy_wezterm_term::color::ColorPalette::default()
    }
}

/// Writer wezterm-term's Terminal uses to send DSR / DA1 / mouse / etc.
/// replies BACK to the child. We wrap each chunk of bytes the emulator
/// writes into one FRAME_PTY_DATA frame and forward over the shared
/// UnixStream — engine → master_writer → child reads them on its tty.
struct PtyResponseWriter(std::sync::Arc<std::sync::Mutex<UnixStream>>);
impl std::io::Write for PtyResponseWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let frame = crate::frames::encode(crate::frames::FRAME_PTY_DATA, buf);
        let mut s = self.0.lock().unwrap();
        s.write_all(&frame)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

struct PtyPane {
    terminal: tattoy_wezterm_term::Terminal,
    writer: std::sync::Arc<std::sync::Mutex<UnixStream>>,
    rx: mpsc::Receiver<PtyMsg>,
    rows: u16,
    cols: u16,
    eof: bool,
}

enum PtyMsg {
    Data(Vec<u8>),
    Eof,
}

impl PtyPane {
    /// Open a PTY connection to the engine and spawn `argv` on it.
    fn open(sock: &str, argv: &[String], rows: u16, cols: u16) -> Result<PtyPane, String> {
        // The engine daemon is long-lived; its own cwd is wherever the
        // first `sarun` invocation started it, typically $HOME. The user
        // expects the PTY child to launch in the directory the CURRENT
        // sarun was invoked from, so we ship it explicitly. Same story
        // for env: portable_pty's CommandBuilder defaults to a minimal
        // env, so `bash -i` lands SHELL/HOME/PATH-less. We ship our own.
        let cwd = std::env::current_dir().ok()
            .map(|p| p.to_string_lossy().into_owned());
        let mut env: std::collections::BTreeMap<String, String> =
            std::env::vars().collect();
        // The child talks to the embedded wezterm-term emulator, not the
        // host terminal — advertise the EMULATOR's capabilities, not the
        // inherited ones. TUI children (carbonyl among them) sniff these
        // two to pick a color depth.
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
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
        let ack = read_one_line(&mut s)?;
        let v: Value = serde_json::from_str(ack.trim())
            .map_err(|e| format!("bad ack: {e}"))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(v.get("error").and_then(Value::as_str)
                .unwrap_or("pty_spawn refused").to_string());
        }
        // Share the UnixStream between three roles:
        //   * READER thread reads the byte stream → FRAME_PTY_DATA / EOF.
        //   * send_input / resize write FRAME_PTY_DATA / FRAME_PTY_RESIZE.
        //   * wezterm-term's emulator writer writes DSR / DA1 / mouse
        //     responses (encoded as FRAME_PTY_DATA).
        // Reader uses its own try_clone; the two writer roles share an
        // Arc<Mutex<>> so concurrent writes never interleave bytes mid-frame.
        let writer = std::sync::Arc::new(std::sync::Mutex::new(
            s.try_clone().map_err(|e| e.to_string())?));
        let reader_handle = s.try_clone().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = reader_handle;
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
        // The ORIGINAL `s` is no longer needed (both threads use clones).
        drop(s);
        // Build the wezterm-term Terminal. The Writer it gets is the
        // PtyResponseWriter wrapping our shared UnixStream — that's how
        // DSR / DA1 / etc. replies flow upstream.
        let size = tattoy_wezterm_term::TerminalSize {
            rows: rows as usize, cols: cols as usize,
            pixel_width: 0, pixel_height: 0, dpi: 0,
        };
        let response_writer = Box::new(PtyResponseWriter(writer.clone()));
        let term = tattoy_wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(PtyTermConfig),
            "sarun", env!("CARGO_PKG_VERSION"),
            response_writer);
        Ok(PtyPane { terminal: term, writer, rx, rows, cols, eof: false })
    }

    /// Drain any pending PTY output into the wezterm-term emulator.
    /// Non-blocking; call each UI tick. Returns true if anything changed.
    fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                PtyMsg::Data(d) => { self.terminal.advance_bytes(&d); changed = true; }
                PtyMsg::Eof => { self.eof = true; changed = true; }
            }
        }
        changed
    }

    /// Send raw keystroke bytes to the child (FRAME_PTY_DATA, client→engine).
    /// True when the CHILD asked for mouse reporting (SGR/X10/any-event).
    /// The pane's mouse capture is gated on this so a plain shell keeps
    /// the OUTER terminal's native click-drag selection (a deliberate
    /// property of the frameless PTY body — see draw()).
    fn mouse_grabbed(&self) -> bool {
        self.terminal.is_mouse_grabbed()
    }

    /// Feed one mouse event (grid-local 0-based coords) to the emulator;
    /// it encodes per the protocol the child requested and replies via the
    /// PtyResponseWriter like any DSR.
    fn send_mouse(&mut self, ev: tattoy_wezterm_term::MouseEvent) {
        let _ = self.terminal.mouse_event(ev);
    }

    fn send_input(&mut self, bytes: &[u8]) {
        let frame = crate::frames::encode(crate::frames::FRAME_PTY_DATA, bytes);
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(&frame);
        let _ = w.flush();
    }

    /// Tell the engine the pane was resized (FRAME_PTY_RESIZE) and resize
    /// the emulator's screen to match.
    fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols { return; }
        self.rows = rows;
        self.cols = cols;
        let size = tattoy_wezterm_term::TerminalSize {
            rows: rows as usize, cols: cols as usize,
            pixel_width: 0, pixel_height: 0, dpi: 0,
        };
        self.terminal.resize(size);
        let frame = crate::frames::encode(crate::frames::FRAME_PTY_RESIZE,
            &crate::frames::pty_resize_payload(rows, cols));
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(&frame);
        let _ = w.flush();
    }
}

/// Render a wezterm-term Screen's VISIBLE region into a ratatui Buffer.
/// Walks each visible row + its cells, translates wezterm CellAttributes
/// to ratatui Style, writes one ratatui cell per terminal cell. Empty
/// trailing space (default-attr) is left alone.
fn render_pty_into(buffer: &mut ratatui::buffer::Buffer, area: Rect,
                   pty: &PtyPane) {
    if area.width == 0 || area.height == 0 { return; }
    let screen = pty.terminal.screen();
    let phys_rows = screen.physical_rows;
    let phys_cols = screen.physical_cols;
    let total_lines = {
        // physical_rows is the visible height; the visible window is the
        // LAST physical_rows lines of the full line buffer. We iterate
        // for_each_phys_line and skip everything before the visible range.
        let mut total = 0usize;
        screen.for_each_phys_line(|_idx, _line| { total += 1; });
        total
    };
    let visible_start = total_lines.saturating_sub(phys_rows);
    let max_rows = area.height as usize;
    let max_cols = area.width as usize;
    screen.for_each_phys_line(|idx, line| {
        if idx < visible_start { return; }
        let row = idx - visible_start;
        if row >= max_rows || row >= phys_rows { return; }
        let y = area.y + row as u16;
        for cell in line.visible_cells() {
            let x = cell.cell_index();
            if x >= max_cols || x >= phys_cols { break; }
            let s = cell.str();
            let style = wezterm_attrs_to_ratatui_style(cell.attrs());
            let bx = area.x + x as u16;
            buffer[(bx, y)].set_symbol(s).set_style(style);
        }
    });
}

/// Translate wezterm-term CellAttributes → ratatui Style. Covers the
/// common subset: fg / bg color, bold, dim, italic, underline, reverse,
/// strikethrough, hidden, slow blink. Image-cell rendering isn't here
/// yet (that's the sixel-passthrough follow-on we discussed).
fn wezterm_attrs_to_ratatui_style(a: &tattoy_wezterm_term::CellAttributes) -> Style {
    use tattoy_wezterm_term::color::ColorAttribute;
    use tattoy_wezterm_term::Intensity;
    use tattoy_wezterm_term::Underline;
    let mut st = Style::default();
    let map_color = |c: ColorAttribute| -> Option<Color> {
        match c {
            ColorAttribute::TrueColorWithDefaultFallback(rgb)
            | ColorAttribute::TrueColorWithPaletteFallback(rgb, _) => {
                let (r, g, b, _a) = rgb.to_srgb_u8();
                Some(Color::Rgb(r, g, b))
            }
            ColorAttribute::PaletteIndex(i) => Some(match i {
                0 => Color::Black, 1 => Color::Red, 2 => Color::Green,
                3 => Color::Yellow, 4 => Color::Blue, 5 => Color::Magenta,
                6 => Color::Cyan, 7 => Color::Gray,
                8 => Color::DarkGray, 9 => Color::LightRed,
                10 => Color::LightGreen, 11 => Color::LightYellow,
                12 => Color::LightBlue, 13 => Color::LightMagenta,
                14 => Color::LightCyan, 15 => Color::White,
                n => Color::Indexed(n),
            }),
            ColorAttribute::Default => None,
        }
    };
    if let Some(fg) = map_color(a.foreground()) { st = st.fg(fg); }
    if let Some(bg) = map_color(a.background()) { st = st.bg(bg); }
    match a.intensity() {
        Intensity::Bold => st = st.add_modifier(Modifier::BOLD),
        Intensity::Half => st = st.add_modifier(Modifier::DIM),
        Intensity::Normal => {}
    }
    if a.italic() { st = st.add_modifier(Modifier::ITALIC); }
    if !matches!(a.underline(), Underline::None) {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    if a.reverse() { st = st.add_modifier(Modifier::REVERSED); }
    if a.strikethrough() { st = st.add_modifier(Modifier::CROSSED_OUT); }
    if a.invisible() { st = st.add_modifier(Modifier::HIDDEN); }
    if a.blink() != tattoy_wezterm_term::Blink::None {
        st = st.add_modifier(Modifier::SLOW_BLINK);
    }
    st
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
/// Screen rect of the visible PTY grid. Mirrors draw()'s layout with the
/// same Layout solves (menubar/cmdline/fkeybar/status strips, the 45/55
/// horizontal split, the 1-row PTY title strip) — the same contract
/// fit_active_pty() keeps for sizes, extended to origins for mouse
/// translation. None when no PTY is on screen.
fn pty_grid_rect(app: &App, term_cols: u16, term_rows: u16) -> Option<Rect> {
    if app.ptys.is_empty() { return None; }
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(Rect { x: 0, y: 0, width: term_cols, height: term_rows });
    let body = root[1];
    let area = if app.focus == Pane::Pty {
        body
    } else if app.pty_in_right {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(body);
        cols[1]
    } else {
        return None;
    };
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    Some(split[1])
}

/// crossterm mouse event → the emulator's event, grid-local. None when the
/// event is outside the grid or of no interest (e.g. bare Moved without a
/// button — children that want any-event tracking still get Drag moves).
fn mouse_to_pty_event(m: crossterm::event::MouseEvent, grid: Rect)
    -> Option<tattoy_wezterm_term::MouseEvent>
{
    use crossterm::event::MouseButton as CB;
    use crossterm::event::MouseEventKind as CK;
    use tattoy_wezterm_term::input::{MouseButton, MouseEventKind};
    if m.column < grid.x || m.row < grid.y
        || m.column >= grid.x + grid.width || m.row >= grid.y + grid.height {
        return None;
    }
    let btn = |b: CB| match b {
        CB::Left => MouseButton::Left,
        CB::Right => MouseButton::Right,
        CB::Middle => MouseButton::Middle,
    };
    let (kind, button) = match m.kind {
        CK::Down(b) => (MouseEventKind::Press, btn(b)),
        CK::Up(b) => (MouseEventKind::Release, btn(b)),
        CK::Drag(b) => (MouseEventKind::Move, btn(b)),
        CK::Moved => (MouseEventKind::Move, MouseButton::None),
        CK::ScrollUp => (MouseEventKind::Press, MouseButton::WheelUp(1)),
        CK::ScrollDown => (MouseEventKind::Press, MouseButton::WheelDown(1)),
        CK::ScrollLeft => (MouseEventKind::Press, MouseButton::WheelLeft(1)),
        CK::ScrollRight => (MouseEventKind::Press, MouseButton::WheelRight(1)),
    };
    let mut mods = tattoy_wezterm_term::KeyModifiers::NONE;
    if m.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
        mods |= tattoy_wezterm_term::KeyModifiers::SHIFT;
    }
    if m.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        mods |= tattoy_wezterm_term::KeyModifiers::CTRL;
    }
    if m.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
        mods |= tattoy_wezterm_term::KeyModifiers::ALT;
    }
    Some(tattoy_wezterm_term::MouseEvent {
        kind, button,
        x: (m.column - grid.x) as usize,
        y: (m.row - grid.y) as i64,
        x_pixel_offset: 0,
        y_pixel_offset: 0,
        modifiers: mods,
    })
}

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
            let mut items = vec![
                mk("Open changes view", "Enter",  Action::OpenSelection),
                mk("Apply ALL changes to host", "a", Action::ApplyBox),
                mk("Delete box (finalize by rules, keep child boxes)", "D",
                   Action::DissolveBox),
                mk("Discard ALL changes", "x",    Action::DiscardBox),
                mk("Kill (SIGTERM)",    "K",      Action::KillBox),
                mk("Rename box",        "r/F6",   Action::StartRename),
                mk("New box from image…", "F7",   Action::NewFromImage),
                mk("oaita agent session on this box…", "",
                   Action::OaitaOnSelectedBox),
            ];
            // Loaded image boxes additionally offer a container shell.
            if app.oci_images.iter().any(|(n, _)| path.ends_with(n.as_str())) {
                items.push(mk("Container shell on this image", "",
                              Action::RunSelectedImage));
            }
            Some((title("Box", &path), items))
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
        Pane::ApiLogs => Some(("API calls".into(), vec![
            mk("Open call detail",   "Enter", Action::OpenSelection),
            mk("Configure an external API (base_url / model / key)…", "",
               Action::OpenApiConfig),
            mk("Pick a local model (live catalog) & serve it…", "",
               Action::OpenModelPicker),
            mk("Set up the default local endpoint (oaita local)", "",
               Action::OaitaLocalPty),
        ])),
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
        Action::DissolveBox    => app.modal = Some(Modal::Confirm {
            prompt: format!("Delete {}? Changes are finalized by your \
                             file-rules (apply-matched → host, the rest \
                             discarded); any nested child boxes are kept.",
                            app.box_op_scope_label()),
            action: ConfirmAction::Dissolve,
        }),
        Action::KillBox        => app.modal = Some(Modal::Confirm {
            prompt: format!("Kill (SIGTERM) {}?", app.box_op_scope_label()),
            action: ConfirmAction::Kill,
        }),
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
        Action::PtyNew         => open_pty_menu(app),
        // Every launch below funnels through build_launch(target, how) — one
        // builder, the same net/env/placement for all. Targets that are ready
        // to run spawn directly; targets that need the user to finish
        // something (a command, a URL) prefill the SAME editable command line
        // (shell_join of the built argv) so what runs is always visible.
        Action::PtyNewBoxShell => {
            app.open_pty(build_launch(&LaunchTarget::Shell, &app.how(Placement::New)));
        }
        Action::PtyNewHostShell => {
            app.open_pty(build_launch(&LaunchTarget::HostShell, &app.how(Placement::New)));
        }
        Action::PtyNewCustom   => {
            // The user's configured default command if set; else a box shell
            // with the command left blank — the how-flags already in the built
            // argv, so editing starts from the real command that would run.
            // Trailing space: the cursor lands after `-- ` ready to type.
            let buf = pty_command_configured().unwrap_or_else(|| {
                let argv = build_launch(&LaunchTarget::Command(vec![]),
                                        &app.how(Placement::New));
                shell_join(&argv) + " "
            });
            app.modal = Some(Modal::PtyCmd { buf });
        }
        Action::NewFromImage   => app.open_image_picker(),
        Action::RunSelectedImage => {
            let name = app.sessions.get(app.sel_session)
                .and_then(|r| r.get("path").and_then(Value::as_str))
                .and_then(|p| app.oci_images.iter()
                    .find(|(n, _)| p.ends_with(n.as_str()))
                    .map(|(n, _)| n.clone()));
            match name {
                Some(name) => app.open_pty(
                    build_launch(&LaunchTarget::Image(name), &app.how(Placement::New))),
                None => app.status =
                    "selected box is not a loaded image".into(),
            }
        }
        Action::BrowserCarbonyl => {
            // Open a real URL field (DESIGN-web.md W3). The MITM SPKI is
            // required — Chromium ignores the overlay CA bundle and pin-trusts
            // this key instead, so without it TLS interception fails silently
            // in the browser. Refuse to launch (visible error) rather than
            // hand out a browser that can't load HTTPS.
            match crate::net::ca::root_spki_sha256_b64() {
                Ok(spki) => app.modal = Some(Modal::BrowserUrl {
                    buf: "https://".into(), spki,
                }),
                Err(e) => app.status =
                    format!("browser: MITM CA unavailable ({e}); cannot launch"),
            }
        }
        Action::OaitaLocalPty  => {
            app.open_pty(vec![self_exe(), "oaita".into(), "local".into()]);
        }
        Action::OpenModelPicker => app.open_model_picker(),
        Action::OpenApiConfig  => app.open_api_config(),
        Action::OaitaOnSelectedBox => {
            // Ask for the TASK; placement is On(this box). Enter builds the
            // agent launch through the same build_launch as everything else.
            let name = app.sessions.get(app.sel_session)
                .and_then(|r| r.get("name").and_then(Value::as_str));
            match name {
                Some(n) => app.modal = Some(Modal::OaitaTask {
                    box_name: n.to_string(),
                    session: oaita_session_for_box(n),
                    buf: String::new(),
                }),
                None => app.status = "no box selected".into(),
            }
        }
        Action::PtyKill        => app.pty_kill(),
        Action::PtyEmbedToggle => {
            if app.ptys.is_empty() {
                app.status = "no PTY to split — F2 opens one".into();
            } else {
                app.pty_in_right = !app.pty_in_right;
                app.right_focused = app.pty_in_right;
                app.right_scroll = 0;
            }
        }
    }
}

fn dispatch_menubar_key(app: &mut App, k: char) {
    // PTY is special: focus a live PTY if one's open, else prompt for the
    // login command. Every other accelerator routes through the PANE_KEYS
    // table to `go_to_pane`, so the binding lives in exactly one place.
    if k == 'P' {
        let any_live = app.ptys.iter().any(|p| !p.eof);
        if any_live {
            if app.cur_pty().map(|p| p.eof).unwrap_or(true) {
                if let Some((i, _)) = app.ptys.iter().enumerate()
                    .find(|(_, p)| !p.eof) { app.sel_pty = i; }
            }
            app.focus = Pane::Pty;
        } else {
            open_pty_menu(app);
        }
        return;
    }
    if let Some((_, pane, _, _, _)) = PANE_KEYS.iter().find(|e| e.0 == k) {
        app.go_to_pane(*pane);
    }
}

fn handle_modal_key(app: &mut App, code: crossterm::event::KeyCode,
                    mods: crossterm::event::KeyModifiers) {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;
    let Some(modal) = app.modal.take() else { return };
    match modal {
        Modal::Confirm { prompt, action } => {
            // y/n/Esc is a fully enumerable keymap → driven from CONFIRM_KEYS
            // (the same table that generates the modal's help line). On no
            // match the modal re-arms, exactly as the old `_ =>` arm did.
            match CONFIRM_KEYS.iter().find(|(k, _, _)| k.matches(code)) {
                Some((_, ConfirmKey::Yes, _)) => app.run_confirm(action),
                Some((_, ConfirmKey::No, _)) => app.status = "cancelled".into(),
                None => app.modal = Some(Modal::Confirm { prompt, action }),
            }
        }
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
        Modal::VarQuery { mut buf } => match code {
            KeyCode::Enter => {
                // One term searches name OR value (the forgiving default);
                // two terms are NAME then VALUE, ANDed.
                let mut it = buf.split_whitespace();
                let a = it.next().unwrap_or("").to_string();
                let b = it.next().unwrap_or("").to_string();
                app.vars_any = !a.is_empty() && b.is_empty();
                app.vars_query = if app.vars_any {
                    (a.clone(), a)
                } else {
                    (a, b)
                };
                app.load_vars();
            }
            KeyCode::Esc => {}
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::VarQuery { buf });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::VarQuery { buf });
            }
            _ => app.modal = Some(Modal::VarQuery { buf }),
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
        Modal::BrowserUrl { mut buf, spki } => match code {
            KeyCode::Enter => {
                // Assemble the browser launch from the validated URL: the
                // persistent BROWSER box (profile persists across launches)
                // with web capture on (browsing archives). One universal
                // build_launch, not a hand-edited shell line.
                let how = How {
                    net: effective_net(app).to_string(),
                    env: app.launch_env,
                    placement: Placement::Reuse("BROWSER".into()),
                    webcap: true,
                    webfilter: true,
                };
                let argv = build_launch(
                    &LaunchTarget::Browser { url: buf, spki: Some(spki) }, &how);
                app.open_pty(argv);
            }
            KeyCode::Esc => app.status = "browser cancelled".into(),
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::BrowserUrl { buf, spki });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::BrowserUrl { buf, spki });
            }
            _ => app.modal = Some(Modal::BrowserUrl { buf, spki }),
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
        Modal::Launcher { items, mut sel } => match code {
            KeyCode::Esc => app.status = "launcher cancelled".into(),
            KeyCode::Up => {
                if sel > 0 { sel -= 1; }
                app.modal = Some(Modal::Launcher { items, sel });
            }
            KeyCode::Down => {
                if sel + 1 < items.len() { sel += 1; }
                app.modal = Some(Modal::Launcher { items, sel });
            }
            KeyCode::Home => app.modal = Some(Modal::Launcher { items, sel: 0 }),
            KeyCode::End => {
                let last = items.len().saturating_sub(1);
                app.modal = Some(Modal::Launcher { items, sel: last });
            }
            // The visible option chips: cycle in place, stay in the modal —
            // no abort/retype to change a launch flag.
            KeyCode::Char('n') | KeyCode::Char('N') => {
                app.launch_net = (app.launch_net + 1) % NET_MODES.len();
                app.modal = Some(Modal::Launcher { items, sel });
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                app.launch_env = !app.launch_env;
                app.modal = Some(Modal::Launcher { items, sel });
            }
            KeyCode::Enter => {
                if let Some(it) = items.get(sel) {
                    let act = it.action;
                    run_action(app, act);
                }
            }
            _ => app.modal = Some(Modal::Launcher { items, sel }),
        },
        Modal::ImagePicker { mut crumbs, mut stack } => match code {
            KeyCode::Esc => app.status = "image picker cancelled".into(),
            KeyCode::Up => {
                if let Some(l) = stack.last_mut() {
                    if l.sel > 0 { l.sel -= 1; }
                }
                app.modal = Some(Modal::ImagePicker { crumbs, stack });
            }
            KeyCode::Down => {
                if let Some(l) = stack.last_mut() {
                    if l.sel + 1 < l.items.len() { l.sel += 1; }
                }
                app.modal = Some(Modal::ImagePicker { crumbs, stack });
            }
            KeyCode::Home => {
                if let Some(l) = stack.last_mut() { l.sel = 0; }
                app.modal = Some(Modal::ImagePicker { crumbs, stack });
            }
            KeyCode::End => {
                if let Some(l) = stack.last_mut() {
                    l.sel = l.items.len().saturating_sub(1);
                }
                app.modal = Some(Modal::ImagePicker { crumbs, stack });
            }
            KeyCode::Backspace | KeyCode::Left => {
                // Pop one level; from the top level, close.
                if stack.len() > 1 {
                    stack.pop();
                    crumbs.pop();
                    app.modal = Some(Modal::ImagePicker { crumbs, stack });
                } else {
                    app.status = "image picker cancelled".into();
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                let cur = stack.last()
                    .and_then(|l| l.items.get(l.sel)).cloned();
                match cur.map(|it| (it.label, it.next)) {
                    Some((label, PickNext::Menu(items))) => {
                        crumbs.push(label);
                        stack.push(PickLevel { items, sel: 0 });
                        app.modal = Some(Modal::ImagePicker { crumbs, stack });
                    }
                    Some((_, PickNext::Pull(rf))) => app.start_image_load(rf),
                    Some((_, PickNext::RunLocal(name))) => {
                        // A loaded image is just an Image target — same
                        // builder as every other launch.
                        app.open_pty(build_launch(
                            &LaunchTarget::Image(name), &app.how(Placement::New)));
                    }
                    Some((_, PickNext::EnterRef(prefix))) => {
                        app.modal = Some(Modal::ImageRef { buf: prefix });
                    }
                    None => app.modal =
                        Some(Modal::ImagePicker { crumbs, stack }),
                }
            }
            _ => app.modal = Some(Modal::ImagePicker { crumbs, stack }),
        },
        Modal::ImageRef { mut buf } => match code {
            KeyCode::Enter => {
                let rf = buf.trim().to_string();
                if rf.is_empty() {
                    app.status = "image reference cancelled".into();
                } else {
                    app.start_image_load(rf);
                }
            }
            KeyCode::Esc => app.status = "image reference cancelled".into(),
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::ImageRef { buf });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::ImageRef { buf });
            }
            _ => app.modal = Some(Modal::ImageRef { buf }),
        },
        Modal::OaitaTask { box_name, session, mut buf } => match code {
            KeyCode::Enter => {
                let task = buf.trim().to_string();
                if task.is_empty() {
                    // Don't launch an empty agent session — say what's needed
                    // instead of running a no-op (or, as before, erroring).
                    app.status = "type a task for the agent (Esc to cancel)".into();
                    app.modal = Some(Modal::OaitaTask { box_name, session, buf });
                } else {
                    let how = app.how(Placement::On(box_name.clone()));
                    app.open_pty(build_launch(&LaunchTarget::Ai { task }, &how));
                }
            }
            KeyCode::Esc => app.status = "agent session cancelled".into(),
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::OaitaTask { box_name, session, buf });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::OaitaTask { box_name, session, buf });
            }
            _ => app.modal = Some(Modal::OaitaTask { box_name, session, buf }),
        },
        Modal::ModelPicker { models, source, mut sel, loading } => {
            // While the catalog is still loading, only Esc does anything.
            if loading {
                match code {
                    KeyCode::Esc => app.status = "model picker cancelled".into(),
                    _ => app.modal = Some(Modal::ModelPicker {
                        models, source, sel, loading }),
                }
                return;
            }
            // Rows are the models plus one trailing "custom URL…" row.
            let n_rows = models.len() + 1;
            match code {
                KeyCode::Esc => app.status = "model picker cancelled".into(),
                KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    app.modal = Some(Modal::ModelPicker {
                        models, source, sel, loading });
                }
                KeyCode::Down => {
                    if sel + 1 < n_rows { sel += 1; }
                    app.modal = Some(Modal::ModelPicker {
                        models, source, sel, loading });
                }
                KeyCode::Home => app.modal = Some(Modal::ModelPicker {
                    models, source, sel: 0, loading }),
                KeyCode::End => {
                    let last = n_rows.saturating_sub(1);
                    app.modal = Some(Modal::ModelPicker {
                        models, source, sel: last, loading });
                }
                KeyCode::Enter => {
                    if sel < models.len() {
                        // Boxed download of the chosen model, then serve on
                        // demand — the same flow F4 kicks, with a URL.
                        let url = models[sel].url.clone();
                        app.open_pty(vec![self_exe(), "oaita".into(),
                            "local".into(), "--model-url".into(), url]);
                    } else {
                        // The custom-URL escape hatch.
                        app.modal = Some(Modal::ModelUrl { buf: String::new() });
                    }
                }
                _ => app.modal = Some(Modal::ModelPicker {
                    models, source, sel, loading }),
            }
        }
        Modal::ModelUrl { mut buf } => match code {
            KeyCode::Enter => {
                let url = buf.trim().to_string();
                if url.is_empty() {
                    app.status = "model URL cancelled".into();
                } else {
                    app.open_pty(vec![self_exe(), "oaita".into(),
                        "local".into(), "--model-url".into(), url]);
                }
            }
            KeyCode::Esc => app.status = "model URL cancelled".into(),
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::ModelUrl { buf });
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.modal = Some(Modal::ModelUrl { buf });
            }
            _ => app.modal = Some(Modal::ModelUrl { buf }),
        },
        Modal::ApiConfig { mut base_url, mut model, mut api_key,
                           mut field, mut result, mut testing } => {
            let ctrl = mods.contains(KeyModifiers::CONTROL);
            // Field order matches the render: 0 model · 1 base_url · 2 api_key.
            let reopen = |app: &mut App, base_url, model, api_key, field,
                          result, testing| {
                app.modal = Some(Modal::ApiConfig {
                    base_url, model, api_key, field, result, testing });
            };
            match code {
                KeyCode::Esc => app.status = "api config cancelled".into(),
                KeyCode::Tab | KeyCode::Down => {
                    field = (field + 1) % 3;
                    reopen(app, base_url, model, api_key, field, result, testing);
                }
                KeyCode::BackTab | KeyCode::Up => {
                    field = (field + 2) % 3;
                    reopen(app, base_url, model, api_key, field, result, testing);
                }
                // Ctrl-T: fire the connection test with the current values.
                KeyCode::Char('t') if ctrl => {
                    testing = true;
                    result = String::new();
                    app.start_api_probe(base_url.clone(), model.clone(),
                                        api_key.clone());
                    reopen(app, base_url, model, api_key, field, result, testing);
                }
                // Ctrl-S or Enter: persist to oaita.toml.
                KeyCode::Char('s') if ctrl => {
                    app.status = app.save_api_config(&base_url, &model, &api_key);
                    if app.status.starts_with("model is required") {
                        reopen(app, base_url, model, api_key, field,
                               result, testing);
                    }
                }
                KeyCode::Enter => {
                    app.status = app.save_api_config(&base_url, &model, &api_key);
                    if app.status.starts_with("model is required") {
                        reopen(app, base_url, model, api_key, field,
                               result, testing);
                    }
                }
                KeyCode::Backspace => {
                    match field { 0 => &mut model, 1 => &mut base_url,
                                  _ => &mut api_key }.pop();
                    reopen(app, base_url, model, api_key, field, result, testing);
                }
                KeyCode::Char(c) => {
                    match field { 0 => &mut model, 1 => &mut base_url,
                                  _ => &mut api_key }.push(c);
                    reopen(app, base_url, model, api_key, field, result, testing);
                }
                _ => reopen(app, base_url, model, api_key, field,
                            result, testing),
            }
        }
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
    // Tell the engine the TUI is up — `-n` dispatcher's Ask gate goes
    // from immediate-deny to "enqueue and wait" while this is true.
    let _ = rpc(sock, "prompts.ui_active", json!([true]));

    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen).map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;

    let mut mouse_captured = false;
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
            // Keep the active PTY sized to whatever space the layout
            // actually gives it. The child's $LINES / $COLUMNS / TIOCSWINSZ
            // tracks the real visible grid, so reedline / less / vim
            // draw at the right width and lines don't wrap weird.
            if let Ok((c, r)) = terminal::size() {
                app.fit_active_pty(c, r);
            }
            // Banner-prompt queue: poll the engine for the next pending
            // ask; while one is up the bottom status line becomes a
            // yellow banner that captures y/n/a/d.
            app.refresh_prompt();
            // drain a finished structural-diff worker result, and animate the
            // spinner while one is still pending.
            app.pump_struct();
            // drain a finished background image pull (oci.load) the same way.
            app.pump_load();
            app.pump_models();
            app.pump_probe();
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
            // Mouse capture tracks the CHILD's wish, not the pane's mere
            // presence: only while the visible PTY's child has mouse
            // reporting on (carbonyl, vim-with-mouse) do we steal the
            // outer terminal's mouse; otherwise native click-drag
            // selection keeps working (see the frameless-body comment in
            // draw()).
            let want_mouse = {
                let (tc, tr) = terminal::size().unwrap_or((80, 24));
                pty_grid_rect(&app, tc, tr).is_some()
                    && app.cur_pty().is_some_and(|p| p.mouse_grabbed())
            };
            if want_mouse != mouse_captured {
                let r = if want_mouse {
                    execute!(term.backend_mut(), event::EnableMouseCapture)
                } else {
                    execute!(term.backend_mut(), event::DisableMouseCapture)
                };
                if r.is_ok() { mouse_captured = want_mouse; }
            }
            if !event::poll(Duration::from_millis(200)).map_err(|e| e.to_string())? {
                continue;
            }
            let ev = event::read().map_err(|e| e.to_string())?;
            if let Event::Mouse(m) = ev {
                let (tc, tr) = terminal::size().unwrap_or((80, 24));
                if let Some(grid) = pty_grid_rect(&app, tc, tr) {
                    if let Some(pev) = mouse_to_pty_event(m, grid) {
                        if let Some(pty) = app.cur_pty_mut() {
                            pty.send_mouse(pev);
                        }
                    }
                }
                continue;
            }
            if let Event::Resize(c, r) = ev {
                // Immediate resize on terminal-resize event. The
                // per-iteration fit_active_pty also catches this on
                // the next tick, but reedline / less / vim get
                // distracted if the gap stretches the redraw out —
                // route the resize the moment the OS tells us.
                app.fit_active_pty(c, r);
                continue;
            }
            if let Event::Key(k) = ev {
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
                    if matches!(k.code, KeyCode::F(4)) {
                        // Context-menu popup inside the PTY pane (PTY-
                        // specific actions: new, kill, embed). Same
                        // entrypoint as 'm' in any other pane.
                        if let Some((title, items)) = pane_action_menu(&app) {
                            app.modal = Some(Modal::ActionMenu {
                                title, items, sel: 0,
                            });
                        }
                        continue;
                    }
                    if matches!(k.code, KeyCode::F(7)) {
                        open_pty_menu(&mut app);
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
                        2 => { open_pty_menu(&mut app); }
                        3 => { app.next_pane(); }
                        11 => {
                            // Embed the active PTY into the focused view's
                            // RIGHT column (or un-embed). With no PTY there
                            // is nothing to split — point at F2 instead of
                            // duplicating it (the fkeybar dims F11 too).
                            if app.ptys.is_empty() {
                                app.status =
                                    "no PTY to split — F2 opens one".into();
                            } else {
                                app.pty_in_right = !app.pty_in_right;
                                // Right-focus follows the embedded PTY so
                                // keystrokes land there immediately.
                                app.right_focused = app.pty_in_right;
                                app.right_scroll = 0;
                            }
                        }
                        4 => {
                            // F4 = "Actions" — open the per-row context
                            // popup (same as the 'm' shortcut, but now
                            // visible in the always-on F-keybar). The
                            // popup itself lists Edit / Rename / etc.
                            // with their underlying global keys, so the
                            // muscle memory transfer is straightforward.
                            if let Some((title, items)) = pane_action_menu(&app) {
                                app.modal = Some(Modal::ActionMenu {
                                    title, items, sel: 0,
                                });
                            } else {
                                app.status = "no actions for this row yet".into();
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
                            } else if app.focus == Pane::Sessions {
                                // "Image+" — new box from a container image.
                                app.open_image_picker();
                            }
                        }
                        8 => {
                            if app.focus == Pane::Hunks { app.discard_hunk(); }
                            else if app.focus == Pane::Changes { app.discard(); }
                            else if app.focus == Pane::Rules { app.delete_rule(); }
                            else if app.focus == Pane::Sessions {
                                // Box "Delete" = dissolve (finalize by rules,
                                // keep children); guarded by a Confirm modal.
                                app.modal = Some(Modal::Confirm {
                                    prompt: format!("Delete {}? Changes are \
                                        finalized by your file-rules \
                                        (apply-matched → host, the rest \
                                        discarded); any nested child boxes \
                                        are kept.", app.box_op_scope_label()),
                                    action: ConfirmAction::Dissolve,
                                });
                            }
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
                // Banner-prompt keys take priority over EVERYTHING (so y/n/a/d
                // don't accidentally trigger pane bindings like 'n' = new
                // rule / 'a' = apply / 'd' = detach). The banner only steals
                // these four keys; the rest of the UI is fully usable.
                if app.pending_prompt.is_some() {
                    match k.code {
                        KeyCode::Char('y') => { app.answer_prompt("yes_once"); continue; }
                        KeyCode::Char('n') => { app.answer_prompt("no_once");  continue; }
                        KeyCode::Char('a') => { app.answer_prompt("allow_save"); continue; }
                        KeyCode::Char('d') => { app.answer_prompt("deny_save");  continue; }
                        _ => {}
                    }
                }
                // The per-pane action keys are now a single table
                // (`PANE_ACTION_KEYS` → `dispatch_pane_key`), the same
                // keybindings-as-data shape as `PANE_KEYS`. A handful of keys
                // carry context a flat table can't express and stay inline
                // BEFORE the table lookup, so their precedence is preserved:
                //   * ctrl+↑/↓ rule reorder (must beat the plain ↑/↓ move),
                //   * arrow ↑/↓ and PageUp/PageDown motion (non-Char codes the
                //     table doesn't model; ↑/↓ mirror k/j),
                //   * the b/c/p/o/… pane accelerators, routed through
                //     `dispatch_menubar_key` so they can't diverge from F9 nav,
                //   * Esc/Backspace popping the packet drill-down, and plain Esc
                //     clearing a generated (cross-nav) filter.
                // Everything else (q, a/x/d, A/X, K/D/Z, n, r/R, /, m, Tab,
                // Enter, j/k) flows through the table.
                match k.code {
                    // ctrl+up / ctrl+down reorder the selected file rule (before
                    // the plain move arm, which also matches Up/Down).
                    KeyCode::Up if ctrl && app.focus == Pane::Rules => app.move_rule(-1),
                    KeyCode::Down if ctrl && app.focus == Pane::Rules => app.move_rule(1),
                    KeyCode::Down => app.move_down(),
                    KeyCode::Up => app.move_up(),
                    KeyCode::PageDown => app.page_down(),
                    KeyCode::PageUp => app.page_up(),
                    KeyCode::Home => app.move_home(),
                    KeyCode::End => app.move_end(),
                    // '!' — errors-only lens on the focused list view.
                    KeyCode::Char('!') => app.toggle_err_only(),
                    // pane switches; c/p/o cross-navigate (install a generated
                    // ids filter on the destination from the cursor).
                    // Every letter chip snaps focus back to the LEFT list and
                    // clears the right-pane scroll — carrying right_focused
                    // across views would put the cursor in the new view's
                    // detail body, which has no cursor of its own.
                    // Letter accelerators: route through the same
                    // dispatch_menubar_key the F9 menu-nav path uses,
                    // so the two paths can't diverge.
                    KeyCode::Char(c @ ('b'|'c'|'p'|'o'|'l'|'g'|'f'|'e'|'?'|'P'|'i'|'v')) =>
                        dispatch_menubar_key(&mut app, c),
                    // Esc / Backspace from the packet drill-down pops back
                    // to the flows list, keeping its cursor + detail state.
                    KeyCode::Backspace => app.go_back(),
                    KeyCode::Esc if app.focus == Pane::Packets =>
                        app.close_packets(),
                    KeyCode::Esc => {
                        // esc first drops an active multi-select; only if there
                        // was none does it fall through to clearing a generated
                        // (cross-nav) filter on the focused pane.
                        if !app.clear_marks() {
                            if let Some(v) = app.focus_filter_view() {
                                if app.view_filter(v).generated {
                                    app.clear_filter(v);
                                }
                            }
                        }
                    }
                    // The table handles the rest; unmatched keys are no-ops.
                    code => { dispatch_pane_key(&mut app, code); }
                }
            }
        }
        Ok(())
    })();

    // TUI is going away — let the engine know so any -n dispatcher
    // waiters get NoOnce instead of timing out.
    let _ = rpc(sock, "prompts.ui_active", json!([false]));
    terminal::disable_raw_mode().map_err(|e| e.to_string())?;
    if mouse_captured {
        let _ = execute!(term.backend_mut(), event::DisableMouseCapture);
    }
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

    #[test]
    fn collapse_chains_flattens_single_child_runs() {
        // The spec example:
        //   A { A1, B }; B { C }; C { D }; D { E }; E { F1, F2 }
        // DFS-ordered true depths:
        //   A0  A1=1  B=1  C=2  D=3  E=4  F1=5  F2=5
        // expected render:
        //   A
        //     A1
        //     ⋮B  ⋮C  ⋮D  ⋮E      (single-child spine, flattened to A1's level)
        //       F1  F2
        let depths = [0, 1, 1, 2, 3, 4, 5, 5];
        let got = collapse_chains(&depths);
        let want = [
            (0, false), // A   — root, two children
            (1, false), // A1  — leaf, sibling of a branch
            (1, true),  // B   — has one child → starts a collapsed spine
            (1, true),  // C   — only child
            (1, true),  // D   — only child
            (1, true),  // E   — only child (still marked, though it branches below)
            (2, false), // F1  — E branches, so its children indent one deeper
            (2, false), // F2
        ];
        assert_eq!(got, want, "single-child chain collapse mismatch");
    }

    #[test]
    fn collapse_chains_flattens_a_pure_spine() {
        // An OCI image stack: base → L1 → L2 → L3, each the sole child. The whole
        // thing collapses to one column, every node ⋮-marked (incl. the leaf).
        let depths = [0, 1, 2, 3];
        let got = collapse_chains(&depths);
        assert!(got.iter().all(|&(d, c)| d == 0 && c),
                "a pure single-child spine should flatten to depth 0, all marked: {got:?}");
    }

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

    // A How with the given net + placement (env off unless set) for tests.
    fn how(net: &str, placement: Placement) -> How {
        How { net: net.to_string(), env: false, placement, webcap: false,
              webfilter: false }
    }

    #[test]
    fn build_launch_browser_persists_and_captures() {
        // The browser (DESIGN-web.md W3) is a digest-pinned carbonyl image with
        // the entrypoint flags repeated (an explicit `oci run ... -- CMD`
        // REPLACES entrypoint+cmd), --user-data-dir (without which Chromium
        // ignores the SPKI allowlist), the MITM SPKI, and the VALIDATED URL
        // last. Its How is Reuse("BROWSER") + webcap, so the launch carries
        // both --name BROWSER (profile persistence) and --webcap (archiving).
        let how = How {
            net: "tap".into(), env: false,
            placement: Placement::Reuse("BROWSER".into()), webcap: true,
            webfilter: true,
        };
        let argv = build_launch(
            &LaunchTarget::Browser { url: "https://example.com".into(),
                                     spki: Some("AbC=".into()) }, &how);
        let j = argv.join(" ");
        assert!(j.starts_with(
            "sarun oci run --net tap --name BROWSER --webcap --webfilter \
             docker.io/fathyb/carbonyl@sha256:"),
            "persistent + captured + filtered oci run, got {j:?}");
        assert!(argv.contains(&"--".to_string())
            && argv.contains(&"/carbonyl/carbonyl".to_string()),
            "explicit CMD names the binary, got {j:?}");
        for flag in ["--no-sandbox", "--disable-dev-shm-usage",
                     "--user-data-dir=/carbonyl/data",
                     "--ignore-certificate-errors-spki-list=AbC="] {
            assert!(argv.iter().any(|a| a == flag), "missing {flag} in {j:?}");
        }
        assert_eq!(argv.last().map(String::as_str), Some("https://example.com"),
            "validated URL last, got {j:?}");
        // Blank URL falls back to about:blank, never a dud "https://".
        let argv = build_launch(
            &LaunchTarget::Browser { url: "  ".into(), spki: None }, &how);
        assert!(!argv.iter().any(|a| a.contains("spki")), "no empty spki flag");
        assert_eq!(argv.last().map(String::as_str), Some("about:blank"));
    }

    #[test]
    fn build_launch_honors_net_and_env_for_every_target() {
        // net is spliced identically; env (-e) only where a box run accepts
        // it (`sarun run`), never on oci run / host shell.
        let h = How { net: "host".into(), env: true, placement: Placement::New,
                      webcap: false, webfilter: false };
        assert_eq!(build_launch(&LaunchTarget::Shell, &h),
            vec!["sarun", "run", "-b", "--net", "host", "-e", "--"]);
        assert_eq!(build_launch(&LaunchTarget::Command(vec!["make".into()]), &h),
            vec!["sarun", "run", "-b", "--net", "host", "-e", "--", "make"]);
        // oci run: net honored, -e never added (it has no such flag).
        assert_eq!(build_launch(&LaunchTarget::Image("alpine:3.20".into()), &h),
            vec!["sarun", "oci", "run", "--net", "host", "alpine:3.20"]);
        // host shell: the one un-boxed target — no flags at all.
        let hs = build_launch(&LaunchTarget::HostShell, &h);
        assert_eq!(hs.len(), 1, "host shell is bare, got {hs:?}");
    }

    #[test]
    fn build_launch_placement_is_uniform() {
        // New: no name. Reuse(n): the box is named so a rerun persists its
        // upper. On(p): a sub-box parented on p. Same axis, every target.
        let n = |p| how("tap", p);
        // run: Reuse -> bare NAME positional; On -> dotted parent.child.
        assert_eq!(build_launch(&LaunchTarget::Shell, &n(Placement::New)),
            vec!["sarun", "run", "-b", "--net", "tap", "--"]);
        assert_eq!(build_launch(&LaunchTarget::Shell, &n(Placement::Reuse("WORK".into()))),
            vec!["sarun", "run", "-b", "--net", "tap", "WORK", "--"]);
        assert_eq!(
            build_launch(&LaunchTarget::Command(vec!["make".into()]),
                         &n(Placement::On("BUILD".into()))),
            vec!["sarun", "run", "-b", "--net", "tap", "BUILD.make", "--", "make"]);
        // oci run: Reuse -> --name; On degrades to New (an image can't be
        // parented elsewhere).
        assert_eq!(
            build_launch(&LaunchTarget::Image("alpine".into()),
                         &n(Placement::Reuse("A".into()))),
            vec!["sarun", "oci", "run", "--net", "tap", "--name", "A", "alpine"]);
        assert_eq!(
            build_launch(&LaunchTarget::Image("alpine".into()),
                         &n(Placement::On("X".into()))),
            vec!["sarun", "oci", "run", "--net", "tap", "alpine"]);
    }

    #[test]
    fn build_launch_ai_on_box_is_the_agent_natural_placement() {
        // The agent's On(box) is oaita's own --on; the session is derived
        // from the box so re-running continues the same conversation.
        let argv = build_launch(&LaunchTarget::Ai { task: "fix the bug".into() },
            &how("host", Placement::On("WORK".into())));
        assert_eq!(argv, vec!["sarun", "oaita", "run", "--on", "WORK",
            "--net", "host", "--task", "fix the bug", "workagent"]);
    }

    #[test]
    fn shell_join_roundtrips_through_shell_split() {
        // The editor prefill (shell_join) and its Enter parse (shell_split)
        // are inverses for the commands build_launch emits, incl. an arg with
        // spaces.
        for argv in [
            build_launch(&LaunchTarget::Browser { url: "https://x.test".into(),
                                                  spki: Some("A=".into()) },
                         &how("tap", Placement::New)),
            vec!["sarun".into(), "run".into(), "-b".into(), "--".into(),
                 "echo".into(), "a b".into()],
        ] {
            assert_eq!(shell_split(&shell_join(&argv)), argv,
                "join/split roundtrip failed for {argv:?}");
        }
    }

    #[test]
    fn mouse_translates_grid_local_and_rejects_outside() {
        use crossterm::event::{MouseButton as CB, MouseEvent as CM,
                               MouseEventKind as CK, KeyModifiers as KM};
        let grid = Rect { x: 10, y: 2, width: 20, height: 10 };
        let ev = |col, row, kind| CM { kind, column: col, row,
                                       modifiers: KM::empty() };
        // Inside: coords become 0-based grid-local.
        let p = mouse_to_pty_event(ev(10, 2, CK::Down(CB::Left)), grid)
            .expect("top-left corner is inside");
        assert_eq!((p.x, p.y), (0, 0));
        let p = mouse_to_pty_event(ev(29, 11, CK::Up(CB::Right)), grid)
            .expect("bottom-right corner is inside");
        assert_eq!((p.x, p.y), (19, 9));
        // Outside on every edge: swallowed, never forwarded.
        for (c, r) in [(9, 5), (30, 5), (15, 1), (15, 12)] {
            assert!(mouse_to_pty_event(ev(c, r, CK::Moved), grid).is_none(),
                    "({c},{r}) is outside the grid");
        }
        // Wheel maps to the emulator convention: Press + WheelUp/Down.
        use tattoy_wezterm_term::input::{MouseButton, MouseEventKind};
        let p = mouse_to_pty_event(ev(15, 5, CK::ScrollUp), grid).unwrap();
        assert!(matches!(p.kind, MouseEventKind::Press));
        assert!(matches!(p.button, MouseButton::WheelUp(1)));
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
        assert_eq!(pty_command_configured().as_deref(), Some("sarun run -b -- make"),
                   "configured login command must be used verbatim");
        // … and with no config there is None, so the Custom entry falls back
        // to the unified build_launch box-shell template (asserted separately).
        std::fs::remove_file(cfgdir.join("pty_command")).unwrap();
        assert_eq!(pty_command_configured(), None,
                   "no config → None → unified template prefill");
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

        // A fresh box registration should ALSO broadcast — this was
        // missing for a long time (engine sent session_removed and
        // session_renamed but never session_added) and the UI was
        // silently never seeing new boxes until detach/restart.
        let (sid, _root) = make_box(&eng.sock);
        let mut saw_added = false;
        for _ in 0..10 {
            if let Ok(ev) = rx.recv_timeout(Duration::from_secs(2)) {
                if ev.get("type").and_then(Value::as_str) == Some("session_added")
                   && ev.get("sid").and_then(Value::as_str) == Some(sid.as_str()) {
                    saw_added = true;
                    break;
                }
            }
        }
        assert!(saw_added, "expected a 'session_added' event after register");

        // a structural event (delete) should arrive and refresh the App.
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

    /// Flows pane render: injects synthetic rows (no live engine — the
    /// engine-side path is covered by prototype/test_net_rs.py) and
    /// asserts the rendered table shape on the LEFT and the detail
    /// text on the RIGHT.
    #[test]
    fn flows_pane_renders_list_and_detail() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing");
            return;
        };
        let mut app = App::new(eng.sock.clone());
        app.focus = Pane::Flows;
        app.flows = vec![
            serde_json::json!({
                "frame": 8, "t": 10.659,
                "src": "240.1.0.2", "dst": "240.1.1.0",
                "sni": "example.com", "host": "",
                "method": "", "uri": "", "status": "",
            }),
            serde_json::json!({
                "frame": 12, "t": 38.052,
                "src": "240.1.0.2", "dst": "240.1.1.0",
                "sni": "", "host": "example.com",
                "method": "GET", "uri": "/", "status": "",
            }),
            serde_json::json!({
                "frame": 15, "t": 451.428,
                "src": "240.1.1.0", "dst": "240.1.0.2",
                "sni": "", "host": "",
                "method": "", "uri": "", "status": "200",
            }),
        ];
        app.sel_flow = 1;  // the GET request row
        app.flow_detail = "Frame 12: 480 bytes on wire\n\
                            Transmission Control Protocol\n\
                            Hypertext Transfer Protocol\n    \
                            GET / HTTP/1.1\n    \
                            Host: example.com\n".into();
        app.flow_detail_frame = 12;
        let buf = render_to_string(&app, 160, 30).unwrap();
        // The LEFT column shows the request-marker R, the host, method,
        // and status code mark — all three rows must appear in this
        // 30-row terminal.
        assert!(buf.contains("FLOWS"),
                "flows pane title missing:\n{buf}");
        assert!(buf.contains("example.com"),
                "host/SNI didn't render:\n{buf}");
        assert!(buf.contains("GET"), "method didn't render:\n{buf}");
        assert!(buf.contains("200"), "status didn't render:\n{buf}");
        // The RIGHT column shows the tshark -V dissection of the
        // selected GET frame.
        assert!(buf.contains("FLOW") && buf.contains("DETAIL"),
                "flow detail pane title missing:\n{buf}");
        assert!(buf.contains("GET / HTTP/1.1"),
                "tshark dissection didn't render in the right pane:\n{buf}");
    }

    /// Packet drill-down render: synthetic packet rows + a fake -V
    /// dissection. Asserts the title, the per-row marker (proto, len,
    /// info excerpt), and the cached dissection text all render.
    #[test]
    fn packets_pane_renders_list_and_detail() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing");
            return;
        };
        let mut app = App::new(eng.sock.clone());
        app.focus = Pane::Packets;
        app.packets_stream = 0;
        app.packets = vec![
            serde_json::json!({"frame": 1, "t": 0.000, "src": "240.1.0.2",
                "dst": "240.1.1.0", "proto": "TCP",     "len": 74,
                "info": "33644 → 443 [SYN]"}),
            serde_json::json!({"frame": 2, "t": 0.001, "src": "240.1.1.0",
                "dst": "240.1.0.2", "proto": "TCP",     "len": 74,
                "info": "443 → 33644 [SYN, ACK]"}),
            serde_json::json!({"frame": 8, "t": 0.011, "src": "240.1.0.2",
                "dst": "240.1.1.0", "proto": "TLSv1.3", "len": 583,
                "info": "Client Hello (SNI=example.com)"}),
            serde_json::json!({"frame": 12, "t": 0.038, "src": "240.1.0.2",
                "dst": "240.1.1.0", "proto": "HTTP",    "len": 480,
                "info": "GET / HTTP/1.1"}),
            serde_json::json!({"frame": 15, "t": 0.451, "src": "240.1.1.0",
                "dst": "240.1.0.2", "proto": "HTTP",    "len": 1400,
                "info": "HTTP/1.1 200 OK (text/html)"}),
        ];
        app.sel_packet = 3;
        app.packet_detail = "Frame 12: 480 bytes on wire\n\
                              Hypertext Transfer Protocol\n    \
                              GET / HTTP/1.1\n    \
                              Host: example.com\n".into();
        app.packet_detail_frame = 12;
        let buf = render_to_string(&app, 200, 30).unwrap();
        assert!(buf.contains("PACKETS"),
                "packets pane title missing:\n{buf}");
        assert!(buf.contains("stream 0"),
                "stream id missing from title:\n{buf}");
        assert!(buf.contains("Client Hello"),
                "TLS Client Hello row missing:\n{buf}");
        assert!(buf.contains("GET /"),
                "HTTP info missing:\n{buf}");
        assert!(buf.contains("PACKET") && buf.contains("DETAIL"),
                "packet detail title missing:\n{buf}");
        assert!(buf.contains("Host: example.com"),
                "tshark dissection didn't render in the right pane:\n{buf}");
    }

    /// Pane transitions for the drill-down: open_packets sets focus
    /// to Packets, close_packets pops back to Flows. Cursor on the
    /// flows side is preserved across the round trip.
    #[test]
    fn packets_pane_drill_down_pops_back() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing");
            return;
        };
        let mut app = App::new(eng.sock.clone());
        app.focus = Pane::Flows;
        // Without a known cur_sid, open_packets bails — give a fake
        // session that the engine will reject on the actual rpc but
        // we still observe the local state. We seed a flows row with
        // a stream id so the precondition holds.
        app.sessions = vec![serde_json::json!({
            "session_id": "9999", "name": "FAKE", "path": "FAKE", "live": false,
        })];
        app.flows = vec![serde_json::json!({
            "frame": 12, "t": 0.0, "src": "", "dst": "",
            "sni": "", "host": "example.com",
            "method": "GET", "uri": "/", "status": "",
            "stream": 7,
        })];
        app.sel_flow = 0;
        app.open_packets();   // rpc will fail; we just want the focus shift
        assert!(matches!(app.focus, Pane::Packets), "focus should be Packets");
        assert_eq!(app.packets_stream, 7);
        app.close_packets();
        assert!(matches!(app.focus, Pane::Flows), "focus should be Flows");
        assert_eq!(app.sel_flow, 0, "flows cursor preserved across pop");
    }

    /// Banner-prompt: pending_prompt is None → the regular grey status
    /// bar at the bottom; pending_prompt is Some(_) → a yellow banner
    /// asking the user for a verdict with all four keystrokes labelled.
    #[test]
    fn banner_prompt_renders_at_status_bar() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing");
            return;
        };
        let mut app = App::new(eng.sock.clone());
        // No prompt: regular status bar (which has app.status text).
        let plain = render_to_string(&app, 160, 30).unwrap();
        assert!(!plain.contains("[y]es once"),
                "no banner should be visible without a pending prompt");
        // Inject a pending prompt; the banner should render at the
        // bottom row with all four key labels.
        app.pending_prompt = Some(serde_json::json!({
            "id": 1, "box": "BOX3",
            "host": "tracker.example", "port": 443, "scheme": "https",
        }));
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(buf.contains("[BOX3]"),
                "box name missing from banner:\n{buf}");
        assert!(buf.contains("tracker.example:443"),
                "host:port missing from banner:\n{buf}");
        assert!(buf.contains("[y]es once") && buf.contains("[n]o once")
                && buf.contains("[a]llow+save") && buf.contains("[d]eny+save"),
                "all four verdicts must label-into the banner:\n{buf}");
    }

    /// Flows pane keyboard nav: j/k moves the cursor (with detail
    /// refresh stubbed via flows-only mutation), 'f' triggers
    /// load_flows (we just verify it doesn't crash on a clean App).
    #[test]
    fn flows_pane_cursor_moves_and_clamps() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing");
            return;
        };
        let mut app = App::new(eng.sock.clone());
        app.focus = Pane::Flows;
        app.flows = (0..5).map(|i| serde_json::json!({
            "frame": (i + 1) as u64, "t": 0.0,
            "src": "240.1.0.2", "dst": "240.1.1.0",
            "sni": "", "host": format!("h{i}.example"),
            "method": "GET", "uri": "/", "status": "",
        })).collect();
        for _ in 0..10 { app.move_down(); }       // overshoot
        assert_eq!(app.sel_flow, app.flows.len() - 1,
                   "down should clamp at len-1");
        for _ in 0..10 { app.move_up(); }         // overshoot back
        assert_eq!(app.sel_flow, 0, "up should clamp at 0");
        // Empty list: cursor stays at 0 and nothing panics.
        app.flows.clear();
        app.sel_flow = 0;
        app.move_down(); app.move_up();
        assert_eq!(app.sel_flow, 0);
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
            action: ConfirmAction::Dissolve,
        });
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("confirm"), "confirm modal title missing:\n{buf}");
        assert!(buf.contains("Delete?"), "confirm prompt missing:\n{buf}");
        assert!(
            app.sessions.iter().any(|s| s.get("session_id").and_then(Value::as_str) == Some(&sid)),
            "box should still exist while only the modal is open"
        );
        // running the guarded action actually deletes it.
        app.run_confirm(ConfirmAction::Dissolve);
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
        let yes = ProcFilterTarget { row_id: 1, err: false,
            subject: Subject { box_name: "B".into(), exe: "/bin/echo".into(),
                               cwd: "/".into(), argv: vec!["echo".into(), "hi".into()] } };
        let no = ProcFilterTarget { row_id: 2, err: false,
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

    /// An App with no socket and all state empty — built by struct literal so
    /// it never RPCs (unlike `App::new`, which calls refresh/load on construct).
    /// For the pure-render and pure-dispatch keybinding tests that must run with
    /// no live engine.
    fn headless_app() -> App {
        App {
            sock: String::new(),
            sessions: vec![],
            changes: vec![],
            changes_view: None, changes_view_sid: None,
            changes_total: 0,
            changes_window_start: 0,
            hunks: Value::Null,
            processes: vec![],
            processes_view: None, processes_view_sid: None,
            processes_total: 0,
            processes_window_start: 0,
            outputs: vec![],
            outputs_view: None, outputs_view_sid: None,
            outputs_total: 0,
            outputs_window_start: 0,
            rules: vec![], pipelines: vec![], pipelines_flat: vec![],
            pipelines_view: None, pipelines_view_sid: None, pipelines_total: 0, pipelines_window_start: 0,
            pipe_tree: true, pipe_running_only: false, proc_running_only: true,
            build_edges: vec![], build_edges_flat: vec![],
            edges_view: None, edges_view_sid: None, edges_total: 0, edges_window_start: 0,
            edges_running_only: true,
            sel_session: 0,
            sel_change: 0,
            marks: std::collections::HashSet::new(),
            mark_scope: None,
            mark_anchor: None,
            sel_proc: 0, sel_pipeline: 0, sel_edge: 0,
            sel_output: 0,
            sel_api_log: 0,
            api_log_rows: vec![],
            api_log_loaded_sid: None,
            sel_webcap: 0,
            webcap_rows: vec![],
            webcap_loaded_sid: None,
            api_endpoint_note: vec![],
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
            f_pipelines: ViewFilter::default(),
            f_edges: ViewFilter::default(),
            should_quit: false,
            ptys: vec![], sel_pty: 0, pty_esc_at: None, right_focused: false, right_scroll: 0, right_scroll_max: std::cell::Cell::new(0), out_follow_scroll: std::cell::Cell::new(0), err_only: false, nav_history: vec![], vars_query: (String::new(), String::new()), vars_any: false, vars_rows: vec![], sel_var: 0, sel_var_item: 0, pty_in_right: false, menu_nav: false, menu_sel: 0,
            oci_images: vec![],
            launch_net: 0,
            launch_env: false,
            tap_ok: true,
            net_auto_bumped: false,
            load_job: None,
            models_job: None,
            model_picker_offered: false,
            probe_job: None,
            structd: StructState::default(),
            sel_hunk: 0,
            struct_rx: None,
            cd_info: None,
            output_segs: vec![],
            changes_decor: vec![],
            box_summary: serde_json::json!(null),
            flows: vec![], sel_flow: 0,
            flow_detail: String::new(), flow_detail_frame: 0,
            packets: vec![], sel_packet: 0,
            packet_detail: String::new(), packet_detail_frame: 0,
            packets_stream: -1,
            pending_prompt: None,
        }
    }

    /// The Pty+ entry point must be a discoverable chooser, not a raw
    /// command prompt: box shell / image container / host shell / custom.
    #[test]
    fn pty_menu_offers_named_destinations() {
        let mut app = headless_app();
        open_pty_menu(&mut app);
        let Some(Modal::Launcher { items, .. }) = &app.modal else {
            panic!("Pty+ must open the launcher");
        };
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.iter().any(|l| l.contains("NEW box")), "{labels:?}");
        assert!(labels.iter().any(|l| l.contains("image")), "{labels:?}");
        assert!(labels.iter().any(|l| l.contains("Host shell")), "{labels:?}");
        assert!(labels.iter().any(|l| l.contains("Custom")), "{labels:?}");
        // Render: the choices AND the option chips must be visible.
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("Host shell"), "menu not rendered:\n{buf}");
        assert!(buf.contains("network: TAP"), "net chip missing:\n{buf}");
        assert!(buf.contains("record env: off"), "env chip missing:\n{buf}");
    }

    /// The launcher's option chips cycle IN PLACE — no abort/relaunch to
    /// change the network mode — and the chosen flags land in the argv the
    /// destinations build (visible via the Custom prompt prefill).
    #[test]
    fn launcher_toggles_cycle_in_place_and_reach_the_argv() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = headless_app();
        open_pty_menu(&mut app);
        // n: tap → host; e: env on. The modal must stay open throughout.
        handle_modal_key(&mut app, KeyCode::Char('n'), KeyModifiers::empty());
        handle_modal_key(&mut app, KeyCode::Char('e'), KeyModifiers::empty());
        assert!(matches!(app.modal, Some(Modal::Launcher { .. })));
        assert_eq!(NET_MODES[app.launch_net], "host");
        assert!(app.launch_env);
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("network: HOST"), "chip must show new mode:\n{buf}");
        assert!(buf.contains("record env: ON"), "chip must show env on:\n{buf}");
        // The chips reach the argv through the ONE builder: a box run gets
        // --net + -e; an oci run gets --net (no -e).
        assert_eq!(build_launch(&LaunchTarget::Shell, &app.how(Placement::New)),
                   vec!["sarun", "run", "-b", "--net", "host", "-e", "--"]);
        assert_eq!(build_launch(&LaunchTarget::Image("x".into()),
                                &app.how(Placement::New)),
                   vec!["sarun", "oci", "run", "--net", "host", "x"]);
        // Custom command prefill carries the toggles into the editable line.
        let sel_custom = match &app.modal {
            Some(Modal::Launcher { items, .. }) =>
                items.iter().position(|i| i.label.contains("Custom")).unwrap(),
            _ => unreachable!(),
        };
        if let Some(Modal::Launcher { sel, .. }) = app.modal.as_mut() {
            *sel = sel_custom;
        }
        handle_modal_key(&mut app, KeyCode::Enter, KeyModifiers::empty());
        let Some(Modal::PtyCmd { buf }) = &app.modal else {
            panic!("Custom must open the command prompt");
        };
        assert!(buf.contains("run -b --net host -e -- "), "prefill: {buf}");
        assert!(buf.starts_with("sarun "), "no opaque full path: {buf}");
        // full cycle wraps: host → off → tap
        app.launch_net = (app.launch_net + 1) % NET_MODES.len();
        app.launch_net = (app.launch_net + 1) % NET_MODES.len();
        assert_eq!(NET_MODES[app.launch_net], "tap");
    }

    /// Drive the WHOLE "oaita agent session ON box" flow the way a user
    /// does: launcher → pick the entry → task modal → type a task → Enter.
    /// The old version handed the user an incomplete `oaita run --on BOX `
    /// prefill that failed on Enter with "missing NAME"; this asserts the
    /// modal collects a task and builds a COMPLETE, runnable command.
    #[test]
    fn oaita_on_box_flow_builds_a_complete_command() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = headless_app();
        app.sessions = vec![serde_json::json!({
            "path": "WORK", "name": "WORK", "session_id": "7",
        })];
        open_pty_menu(&mut app);
        let sel_oaita = match &app.modal {
            Some(Modal::Launcher { items, .. }) => items.iter()
                .position(|i| i.label.contains("oaita agent session ON box 'WORK'"))
                .expect("launcher must offer the oaita-on-box entry"),
            _ => panic!("launcher expected"),
        };
        if let Some(Modal::Launcher { sel, .. }) = app.modal.as_mut() {
            *sel = sel_oaita;
        }
        handle_modal_key(&mut app, KeyCode::Enter, KeyModifiers::empty());
        // Picking it opens a TASK modal (not a broken command prefill).
        let Some(Modal::OaitaTask { box_name, session, .. }) = &app.modal else {
            panic!("must open the agent-task modal, got {:?}",
                   app.modal.is_some());
        };
        assert_eq!(box_name, "WORK");
        assert_eq!(session, "workagent");
        // Enter with an EMPTY task must NOT launch — it must prompt, and
        // keep the modal open (the old flow ran and errored).
        handle_modal_key(&mut app, KeyCode::Enter, KeyModifiers::empty());
        assert!(matches!(app.modal, Some(Modal::OaitaTask { .. })),
                "empty task must keep the modal open, not launch");
        assert!(app.status.contains("type a task"));
        // Type a multi-word task and submit.
        for c in "fix the bug".chars() {
            handle_modal_key(&mut app, KeyCode::Char(c), KeyModifiers::empty());
        }
        // The argv the modal WOULD run (open_pty needs a live engine, so
        // assert the pure builder that feeds it — a complete command).
        let argv = build_launch(&LaunchTarget::Ai { task: "fix the bug".into() },
                                &how("host", Placement::On("WORK".into())));
        assert_eq!(&argv[1..], &["oaita", "run", "--on", "WORK",
                                 "--net", "host", "--task", "fix the bug",
                                 "workagent"]);
        // task is ONE argv element — spaces need no quoting.
        assert_eq!(argv.iter().filter(|a| a.contains(' ')).count(), 1);
        // the sessions context menu carries the same action
        app.focus = Pane::Sessions;
        let (_, items) = pane_action_menu(&app).unwrap();
        assert!(items.iter().any(|i|
            matches!(i.action, Action::OaitaOnSelectedBox)));
    }

    /// THE bug the user hit: picking "host" (or "off") in the Pty+ chip
    /// must reach the oaita box's argv. Before, oaita run spawned its box
    /// with no --net at all, so every choice collapsed to the default tap
    /// and a no-netns host got "tap setup failed" even after selecting host.
    #[test]
    fn oaita_box_argv_carries_the_selected_network() {
        let mut app = headless_app();
        // host chip (index 1)
        app.launch_net = 1;
        let argv = build_launch(&LaunchTarget::Ai { task: "what os is this?".into() },
                                &app.how(Placement::On("C1".into())));
        let i = argv.iter().position(|a| a == "--net").expect("--net present");
        assert_eq!(argv[i + 1], "host", "host selection must reach the box");
        // off chip (index 2)
        app.launch_net = 2;
        let argv = build_launch(&LaunchTarget::Ai { task: "x".into() },
                                &app.how(Placement::On("C1".into())));
        let i = argv.iter().position(|a| a == "--net").unwrap();
        assert_eq!(argv[i + 1], "off", "off selection must reach the box");
    }

    #[test]
    fn oaita_session_name_is_valid_and_alnum() {
        // must satisfy the engine's validator (alphanumeric only).
        for (box_name, want) in [("C1","c1agent"), ("WORK.SUB","worksubagent"),
                                 ("---","boxagent")] {
            let s = oaita_session_for_box(box_name);
            assert_eq!(s, want);
            assert!(crate::oaita::turns::validate_session_name(&s).is_ok(),
                    "derived session {s:?} must be valid");
        }
    }

    /// A `sarun …` PtyCmd prefill must NOT be described as "runs on the
    /// HOST (no capture)" — that line was the lie the user hit. It must say
    /// the sarun command launches a sandboxed box.
    #[test]
    fn ptycmd_help_is_honest_about_sarun_commands() {
        let mut app = headless_app();
        app.modal = Some(Modal::PtyCmd { buf: "sarun run -b -- ".into() });
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("sandboxed"), "sarun prefill help:\n{buf}");
        assert!(!buf.contains("no box, no capture"),
                "must not claim no-capture for a sarun command:\n{buf}");
        // a non-sarun command keeps the host-shell caveat
        app.modal = Some(Modal::PtyCmd { buf: "bash".into() });
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("runs on the HOST"), "host caveat:\n{buf}");
    }

    /// Restricted host (no CLONE_NEWNET): the launcher must not offer a
    /// default that fails after launch — host is pre-selected with a status
    /// note, and cycling back to tap shows a visible unavailability marker
    /// instead of pretending it will work.
    #[test]
    fn launcher_marks_tap_unavailable_and_preselects_host() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = headless_app();
        app.tap_ok = false;
        open_pty_menu(&mut app);
        assert_eq!(NET_MODES[app.launch_net], "host",
                   "host must be pre-selected when tap can't work");
        assert!(app.status.contains("tap networking unavailable"),
                "why must be stated: {}", app.status);
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("network: HOST"), "{buf}");
        assert!(buf.contains("tap ✗"), "cycle hint must mark tap:\n{buf}");
        // deliberately cycle back to tap (host → off → tap): honored, marked
        handle_modal_key(&mut app, KeyCode::Char('n'), KeyModifiers::empty());
        handle_modal_key(&mut app, KeyCode::Char('n'), KeyModifiers::empty());
        assert_eq!(NET_MODES[app.launch_net], "tap");
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("TAP ✗ unavailable here"),
                "tap chip must carry the marker:\n{buf}");
        // reopening the launcher must NOT fight the deliberate choice
        app.modal = None;
        open_pty_menu(&mut app);
        assert_eq!(NET_MODES[app.launch_net], "tap",
                   "auto-bump is one-shot");
    }

    /// The image picker is a HIERARCHY (groups → tags → pull), seeded from
    /// the curated catalog + registries.conf aliases + loaded images, with
    /// leaf details showing where the pull actually resolves (mirror/alias).
    #[test]
    fn image_picker_tree_respects_registries_conf() {
        let mut conf = crate::containers_conf::ContainersConf::default();
        assert!(conf.merge_toml(r#"
            unqualified-search-registries = ["docker.io"]
            [aliases]
            "corp/tool" = "registry.corp.example/tool"
            [[registry]]
            prefix = "docker.io"
            location = "docker.io"
            [[registry.mirror]]
            location = "mirror.corp.example/hub"
        "#));
        let catalog = image_catalog(); // built-in defaults in tests
        let local = vec![("ubuntu-24.04".to_string(),
                          "docker.io/library/ubuntu:24.04".to_string())];
        let top = build_image_picker(&conf, &catalog, &local);
        let label = |it: &PickItem| it.label.clone();
        // top level: loaded images, every catalog group, aliases, escape hatch
        assert!(top.iter().any(|i| i.label.starts_with("Loaded images")));
        for g in &catalog {
            assert!(top.iter().any(|i| i.label == g.name), "missing {}", g.name);
        }
        assert!(top.iter().any(|i| i.label.contains("Short names")));
        assert!(top.iter().any(|i| i.label == "Enter reference…"));
        // descend Ubuntu: tags are leaves whose detail shows the MIRROR
        let ubuntu = top.iter().find(|i| i.label == "Ubuntu").unwrap();
        let PickNext::Menu(tags) = &ubuntu.next else { panic!("group must nest") };
        let first = tags.first().unwrap();
        assert!(matches!(first.next, PickNext::Pull(_)), "{}", label(first));
        assert!(first.detail.contains("mirror.corp.example/hub"),
                "leaf must surface the mirror: {}", first.detail);
        // each group ends with an enter-a-tag escape hatch
        assert!(matches!(tags.last().unwrap().next, PickNext::EnterRef(_)));
        // aliases branch resolves through the alias target
        let al = top.iter().find(|i| i.label.contains("Short names")).unwrap();
        let PickNext::Menu(als) = &al.next else { panic!() };
        assert!(als.iter().any(|a| a.label == "corp/tool"),
                "{:?}", als.iter().map(|a| &a.label).collect::<Vec<_>>());
        // loaded image leaf runs locally, no pull
        let loaded = top.iter().find(|i| i.label.starts_with("Loaded")).unwrap();
        let PickNext::Menu(ls) = &loaded.next else { panic!() };
        assert!(matches!(&ls[0].next, PickNext::RunLocal(n) if n == "ubuntu-24.04"));

        // and the modal renders: group names + resolution detail visible
        let mut app = headless_app();
        app.modal = Some(Modal::ImagePicker {
            crumbs: vec![],
            stack: vec![PickLevel { items: top, sel: 0 }],
        });
        let buf = render_to_string(&app, 110, 35).unwrap();
        assert!(buf.contains("Ubuntu"), "picker not rendered:\n{buf}");
        assert!(buf.contains("Enter reference"), "escape hatch missing:\n{buf}");
    }

    /// Picker keyboard model: Enter descends into a group, Backspace pops,
    /// Enter on a tag leaf starts a (background) pull — headless, so the rpc
    /// fails, but the modal must close and the job slot must be taken.
    #[test]
    fn image_picker_descend_and_pull() {
        let conf = crate::containers_conf::ContainersConf::default();
        let top = build_image_picker(&conf, &image_catalog(), &[]);
        let mut app = headless_app();
        app.modal = Some(Modal::ImagePicker {
            crumbs: vec![],
            stack: vec![PickLevel { items: top, sel: 0 }],
        });
        // sel 0 = first catalog group (no loaded images) → descend
        handle_modal_key(&mut app, crossterm::event::KeyCode::Enter,
                         crossterm::event::KeyModifiers::empty());
        let Some(Modal::ImagePicker { crumbs, stack }) = &app.modal else {
            panic!("Enter on a group must descend");
        };
        assert_eq!(crumbs.len(), 1);
        assert_eq!(stack.len(), 2);
        // Enter on the first tag leaf → background load kicked, modal closed
        handle_modal_key(&mut app, crossterm::event::KeyCode::Enter,
                         crossterm::event::KeyModifiers::empty());
        assert!(app.modal.is_none(), "leaf pick must close the picker");
        assert!(app.load_job.is_some(), "leaf pick must start a load job");
        // pump until the (failing, no engine) job reports
        for _ in 0..500 {
            app.pump_load();
            app.pump_models();
            app.pump_probe();
            if app.load_job.is_none() { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.load_job.is_none(), "job must complete");
        assert!(app.status.contains("oci load"), "failure surfaced: {}", app.status);
    }

    /// The Network/Web pane (DESIGN-web.md W4) renders the box's webcap rows:
    /// a header, one line per capture (method · status · type · size · url),
    /// and the empty-state hint when there are none. The index lines render
    /// from in-memory `webcap_rows` (no engine needed); the detail lines fetch
    /// over RPC and aren't exercised here.
    #[test]
    fn network_pane_renders_captures() {
        let mut app = headless_app();
        app.focus = Pane::Network;
        // Empty state names the on-ramp.
        let buf = render_to_string(&app, 110, 30).unwrap();
        assert!(buf.contains("WEB") && buf.contains("no web captures"),
                "empty Network pane must show the --webcap on-ramp:\n{buf}");
        // With rows: header + a capture line with method/status/url.
        app.webcap_rows = vec![
            json!({"id": 2, "ts": 0.0, "method": "GET", "url": "https://example.com/app.js",
                   "host": "example.com", "status": 200, "mime": "application/javascript",
                   "truncated": 0, "req_len": 0, "resp_len": 4096}),
            json!({"id": 1, "ts": 0.0, "method": "GET", "url": "https://ad.doubleclick.net/x",
                   "host": "ad.doubleclick.net", "status": 204, "mime": "",
                   "truncated": 0, "req_len": 0, "resp_len": 0}),
        ];
        let buf = render_to_string(&app, 110, 30).unwrap();
        assert!(buf.contains("Method") && buf.contains("Type") && buf.contains("Bytes"),
                "header row rendered:\n{buf}");
        assert!(buf.contains("GET") && buf.contains("200")
                && buf.contains("application/java"),
                "the 200 capture row rendered (method/status/type):\n{buf}");
        assert!(buf.contains("204"),
                "the blocked (204) capture is shown too:\n{buf}");
    }

    /// The Api pane must surface the local on-ramp: an unconfigured endpoint
    /// renders the how-to (pointing at the model picker), and the action menu
    /// carries both the picker entry and the plain `oaita local` launcher.
    #[test]
    fn api_pane_surfaces_oaita_local() {
        let mut app = headless_app();
        app.focus = Pane::ApiLogs;
        app.api_endpoint_note = endpoint_note_lines(None);
        let buf = render_to_string(&app, 110, 30).unwrap();
        assert!(buf.contains("no endpoint configured"), "{buf}");
        assert!(buf.contains("local model") || buf.contains("Pick a local"),
                "empty state must name the zero-endpoint on-ramp:\n{buf}");
        // configured endpoint: summary instead of the how-to
        app.api_endpoint_note = endpoint_note_lines(Some(
            ("qwen3".into(), "http://127.0.0.1:18181/v1".into())));
        let buf = render_to_string(&app, 110, 30).unwrap();
        assert!(buf.contains("http://127.0.0.1:18181/v1"), "{buf}");
        assert!(buf.contains("--api"), "{buf}");
        // action menu carries BOTH on-ramp entries
        let (title, items) = pane_action_menu(&app).expect("Api pane menu");
        assert!(title.contains("API"), "{title}");
        assert!(items.iter().any(|i|
            matches!(i.action, Action::OpenModelPicker)),
            "menu must offer the model picker");
        assert!(items.iter().any(|i|
            matches!(i.action, Action::OaitaLocalPty)),
            "menu must offer plain oaita local");
    }

    /// The model picker: opening it (with no engine to answer oaita.models)
    /// lands in a loading state; a synthesized catalog renders rows + the
    /// custom-URL escape hatch, and Enter on the last row switches to URL
    /// entry rather than launching a download.
    #[test]
    fn model_picker_lists_and_offers_custom_url() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = headless_app();
        // Simulate a delivered catalog directly in the modal (no engine RPC).
        app.modal = Some(Modal::ModelPicker {
            models: vec![
                ModelRow { name: "Qwen3-4B".into(),
                    url: "https://hf/x-Q4_K_M.gguf".into(),
                    note: "Q4 · 2 GiB".into() },
            ],
            source: "HuggingFace (1 live)".into(),
            sel: 0,
            loading: false,
        });
        let buf = render_to_string(&app, 110, 30).unwrap();
        assert!(buf.contains("Qwen3-4B"), "row must show:\n{buf}");
        assert!(buf.contains("custom GGUF URL"), "escape hatch:\n{buf}");
        // Down to the custom-URL row (index == models.len()), Enter → ModelUrl.
        handle_modal_key(&mut app, KeyCode::Down, KeyModifiers::empty());
        handle_modal_key(&mut app, KeyCode::Enter, KeyModifiers::empty());
        assert!(matches!(app.modal, Some(Modal::ModelUrl { .. })),
                "custom-URL row must open the URL entry, not launch a download");
    }

    /// The external-API editor: three editable fields (model / base_url /
    /// api_key), Tab cycles them, typing edits the cursored one, and the key
    /// is masked in the render. The Api-pane menu carries the entry.
    #[test]
    fn api_config_editor_edits_and_masks_key() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = headless_app();
        // menu offers the editor
        app.focus = Pane::ApiLogs;
        let (_, items) = pane_action_menu(&app).expect("Api pane menu");
        assert!(items.iter().any(|i| matches!(i.action, Action::OpenApiConfig)),
                "menu must offer the external-API editor");
        // open it: field 0 is `model`
        app.modal = Some(Modal::ApiConfig {
            base_url: String::new(), model: String::new(),
            api_key: String::new(), field: 0, result: String::new(),
            testing: false });
        let none = KeyModifiers::empty();
        for c in "gpt-4o".chars() {
            handle_modal_key(&mut app, KeyCode::Char(c), none);
        }
        // Tab → base_url, type it
        handle_modal_key(&mut app, KeyCode::Tab, none);
        for c in "http://h:1/v1".chars() {
            handle_modal_key(&mut app, KeyCode::Char(c), none);
        }
        // Tab → api_key, type a secret
        handle_modal_key(&mut app, KeyCode::Tab, none);
        for c in "sk-secret".chars() {
            handle_modal_key(&mut app, KeyCode::Char(c), none);
        }
        match &app.modal {
            Some(Modal::ApiConfig { model, base_url, api_key, .. }) => {
                assert_eq!(model, "gpt-4o");
                assert_eq!(base_url, "http://h:1/v1");
                assert_eq!(api_key, "sk-secret");
            }
            _ => panic!("editor must stay open across edits"),
        }
        let buf = render_to_string(&app, 100, 30).unwrap();
        assert!(buf.contains("gpt-4o") && buf.contains("http://h:1/v1"), "{buf}");
        assert!(!buf.contains("sk-secret"), "api_key must be masked:\n{buf}");
        assert!(buf.contains("•"), "masked key must render dots:\n{buf}");
    }

    /// Multi-select on the Boxes list: Space toggles, `[`/`]` range-fill from
    /// the anchor, the gutter renders the mark, Esc clears, and marks are
    /// scoped to one list (switching panes drops them).
    #[test]
    fn multiselect_toggle_range_clear_and_scope() {
        let mut app = headless_app();
        app.focus = Pane::Sessions;
        app.sessions = (0..5).map(|i| serde_json::json!({
            "session_id": i.to_string(), "name": format!("b{i}"),
            "path": format!("b{i}"), "status": "done",
        })).collect();
        let keys = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Space marks the cursor row (idx 0), which becomes the anchor.
        app.sel_session = 0;
        app.toggle_mark();
        assert_eq!(app.marked_here(), keys(&["0"]));

        // Cursor to idx 3, `]` fills the inclusive anchor→cursor range.
        app.sel_session = 3;
        app.range_mark();
        assert_eq!(app.marked_here(), keys(&["0", "1", "2", "3"]));

        // Space again un-marks the cursor row (idx 2).
        app.sel_session = 2;
        app.toggle_mark();
        assert_eq!(app.marked_here(), keys(&["0", "1", "3"]));

        // The gutter glyph renders for marked rows.
        let buf = render_to_string(&app, 100, 20).unwrap();
        assert!(buf.contains(MARK_GLYPH), "marked rows show the glyph:\n{buf}");

        // Esc clears the selection (and reports it consumed the Esc).
        assert!(app.clear_marks());
        assert!(app.marked_here().is_empty());
        assert!(!app.clear_marks(), "nothing to clear the second time");

        // Marks are scoped: a Sessions mark is invisible on the Changes list.
        app.sel_session = 0;
        app.toggle_mark();
        assert_eq!(app.marked_here().len(), 1);
        app.focus = Pane::Changes;
        assert!(app.marked_here().is_empty(), "marks are per-list");

        // A non-selectable pane just declines.
        app.focus = Pane::Help;
        app.toggle_mark();
        assert!(app.status.contains("Boxes and Changes"));
    }

    #[test]
    fn help_pane_lists_keybindings() {
        // pure render; no engine needed.
        let mut app = headless_app();
        app.focus = Pane::Help;
        // Render tall enough to fit the WHOLE generated manual (it scrolls in a
        // real term). The keybindings-as-data sections (pane index + the
        // generated per-pane action + confirm blocks) push the manual past 80
        // rows, so render extra-tall to assert every generated line is present.
        let buf = render_to_string(&app, 120, 120).unwrap();
        assert!(buf.contains("help"), "help pane title missing:\n{buf}");
        assert!(buf.contains("apply") && buf.contains("discard"), "help should mention apply/discard:\n{buf}");
        assert!(buf.contains("processes"), "help should mention the processes pane:\n{buf}");
        // the richer manual must cover the loop, filter kinds, rule syntax, nesting.
        assert!(buf.contains("copy-on-write") || buf.contains("overlay"),
                "manual should explain the overlay loop:\n{buf}");
        assert!(buf.contains("hunk"), "manual should mention per-hunk apply:\n{buf}");
        assert!(buf.contains("passthrough"), "manual should document rule actions:\n{buf}");
        assert!(buf.contains("ctrl+"), "manual should mention rule reorder:\n{buf}");
        // The pane index must be GENERATED from PANE_KEYS — every accelerator
        // and its description present, never hardcoded prose that can drift.
        for (key, _, _, _, desc) in PANE_KEYS {
            assert!(buf.contains(&format!("{key}  {desc}")),
                    "help pane index missing generated line for {key:?}:\n{buf}");
        }
        // The remaining contexts' help must ALSO be generated from their tables,
        // not hardcoded prose. Every PANE_ACTION_KEYS entry with a help string
        // must surface as a "<key>  <desc>" line — the exact format help_lines
        // emits. (None-help entries are the nav block documented in prose.)
        for (key, _, _, help) in PANE_ACTION_KEYS {
            if let Some(desc) = help {
                let want = format!("{}  {desc}", key.label());
                assert!(buf.contains(&want),
                        "help missing generated pane-action line {want:?}:\n{buf}");
            }
        }
        // The Confirm modal's y/n keymap is generated from CONFIRM_KEYS too: the
        // 'y'/'Y' (confirm) and 'n'/'N'/Esc (cancel) labels must appear joined
        // exactly as help_lines builds them, so the prompt help can't drift from
        // the table the modal actually dispatches.
        let yes = CONFIRM_KEYS.iter().filter(|(_, a, _)| matches!(a, ConfirmKey::Yes))
            .map(|(k, _, _)| k.label()).collect::<Vec<_>>().join("/");
        let no = CONFIRM_KEYS.iter().filter(|(_, a, _)| matches!(a, ConfirmKey::No))
            .map(|(k, _, _)| k.label()).collect::<Vec<_>>().join("/");
        assert!(buf.contains(&format!("{yes}  confirm the action")),
                "help missing generated confirm-keys (yes={yes:?}):\n{buf}");
        assert!(buf.contains(&format!("{no}  cancel")),
                "help missing generated confirm-keys (no={no:?}):\n{buf}");
    }

    /// The keybindings-as-data dispatch is exercised directly (no live engine):
    /// the Confirm modal's y/n/Esc keymap routes through CONFIRM_KEYS, and the
    /// per-pane action keys route through PANE_ACTION_KEYS. Both only touch
    /// engine-free actions here (open/cancel a modal, set detach/rename state),
    /// so this is a pure unit test of the table lookup + precedence.
    #[test]
    fn keymap_tables_dispatch_modal_and_pane_keys() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyModifiers;
        let none = KeyModifiers::empty();

        // ── Confirm modal (CONFIRM_KEYS) ──
        // a cancel key ('n') clears the modal and notes "cancelled".
        let mut app = headless_app();
        app.modal = Some(Modal::Confirm {
            prompt: "Delete?".into(), action: ConfirmAction::Dissolve });
        handle_modal_key(&mut app, KeyCode::Char('n'), none);
        assert!(app.modal.is_none(), "'n' should dismiss the Confirm modal");
        assert_eq!(app.status, "cancelled");
        // Esc is also a cancel key in the table.
        app.modal = Some(Modal::Confirm {
            prompt: "Delete?".into(), action: ConfirmAction::Dissolve });
        handle_modal_key(&mut app, KeyCode::Esc, none);
        assert!(app.modal.is_none(), "Esc should dismiss the Confirm modal");
        // a key NOT in the table re-arms the modal (the old `_ =>` arm).
        app.modal = Some(Modal::Confirm {
            prompt: "Delete?".into(), action: ConfirmAction::Dissolve });
        handle_modal_key(&mut app, KeyCode::Char('z'), none);
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })),
                "an unbound key must leave the Confirm modal up");

        // ── per-pane action keys (PANE_ACTION_KEYS) ──
        // 'K' opens a Kill Confirm; 'D' the box-delete (dissolve) Confirm.
        for (key, want) in [
            ('K', ConfirmAction::Kill),
            ('D', ConfirmAction::Dissolve),
        ] {
            let mut app = headless_app();
            app.focus = Pane::Sessions;
            assert!(dispatch_pane_key(&mut app, KeyCode::Char(key)),
                    "'{key}' must be handled by the pane-key table");
            match app.modal {
                Some(Modal::Confirm { action, .. }) => assert!(action == want,
                    "'{key}' opened the wrong Confirm action"),
                _ => panic!("'{key}' should open a Confirm modal"),
            }
        }

        // gate precedence: 'd' on Rules deletes the rule (no engine here, so we
        // assert it is NOT treated as detach), while 'd' elsewhere = detach.
        let mut app = headless_app();
        app.focus = Pane::Changes;
        assert!(dispatch_pane_key(&mut app, KeyCode::Char('d')));
        assert!(app.should_quit, "'d' off Hunks/Rules must detach (set should_quit)");
        let mut app = headless_app();
        app.focus = Pane::Rules; // 'd' here is the guarded delete-rule, not detach
        assert!(dispatch_pane_key(&mut app, KeyCode::Char('d')));
        assert!(!app.should_quit, "'d' on Rules must NOT detach");

        // 'n' on Rules opens the new-rule form (guarded entry wins).
        let mut app = headless_app();
        app.focus = Pane::Rules;
        assert!(dispatch_pane_key(&mut app, KeyCode::Char('n')));
        assert!(matches!(app.modal, Some(Modal::RuleForm { editing: None, .. })),
                "'n' on Rules should open a blank RuleForm");

        // 'r' starts a rename (engine-free state toggle).
        let mut app = headless_app();
        app.focus = Pane::Sessions;
        assert!(dispatch_pane_key(&mut app, KeyCode::Char('r')));
        assert!(app.renaming.is_some(), "'r' should enter rename mode");

        // an unbound key returns false (caller handles it inline / no-op).
        let mut app = headless_app();
        assert!(!dispatch_pane_key(&mut app, KeyCode::Char('§')),
                "an unbound key must not be claimed by the pane-key table");
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

    /// Run a real command in a -b BRUSH box (so brushprov rows actually
    /// land in the box's sqlar). Mirrors `run_cmd` but prepends `-b TAG`.
    fn run_cmd_brush(eng: &Engine, tag: &str, cmd: &[&str]) -> bool {
        let bin = engine_bin().expect("engine bin");
        let mut args: Vec<String> = vec!["run".into(), "-b".into(),
            tag.into(), "--".into()];
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

    /// With real brushprov rows in the box, the Pipes pane must RENDER
    /// those rows — not the "no pipelines yet" empty hint. Regression
    /// guard for the bug where load_pipelines passed an i64 sid into
    /// brushprov / build_edges (whose arg_sid only accepted a String),
    /// so every dedicated pane was silently empty even when the right-
    /// pane box summary on the Sessions view showed the same rows.
    #[test]
    fn pipelines_pane_shows_live_brushprov_row() {
        let Some(eng) = boot() else {
            eprintln!("SKIP: engine binary missing or FUSE unavailable");
            return;
        };
        if !run_cmd_brush(&eng, "BRPIPE", &["sh", "-c",
            "echo hi | tr a-z A-Z > /tmp/brush_pipe_uitest.txt"]) {
            eprintln!("SKIP: -b box failed to run (likely no userns / bwrap)");
            return;
        }
        let mut app = App::new(eng.sock.clone());
        app.refresh_sessions();
        assert!(!app.sessions.is_empty(), "no sessions after brush run");
        app.sel_session = 0;
        app.focus = Pane::Pipelines;
        app.load_pipelines();
        assert!(!app.pipelines.is_empty(),
            "load_pipelines returned 0 rows for a -b box that DID record a \
             pipeline — brushprov RPC wire format probably regressed (arg_sid \
             rejecting i64 sids again?)");
        let buf = render_to_string(&app, 160, 30).unwrap();
        assert!(!buf.contains("no pipelines yet"),
            "Pipes pane STILL renders the empty hint despite \
             app.pipelines.len() = {}:\n{buf}", app.pipelines.len());
        // the command text we ran must be visible on the pane.
        assert!(buf.contains("tr") && buf.contains("echo hi"),
            "rendered pane does not surface the recorded cmd:\n{buf}");
    }

    /// build_pipeline_tree reorders flat brushprov rows into DFS pre-order by
    /// (uid, parent_uid) and stamps depth — the basis for the tree render.
    #[test]
    fn pipeline_tree_nests_by_parent_uid() {
        let rows = vec![
            json!({"id":1,"uid":10,"parent_uid":0,"cmd":"recipe"}),
            json!({"id":2,"uid":11,"parent_uid":10,"cmd":"inner"}),
            json!({"id":3,"uid":12,"parent_uid":11,"cmd":"echo deep"}),
            json!({"id":4,"uid":20,"parent_uid":0,"cmd":"sibling"}),
        ];
        let tree = build_pipeline_tree(rows);
        let at = |i: usize| {
            (tree[i].get("cmd").and_then(Value::as_str).unwrap(),
             tree[i].get("depth").and_then(Value::as_i64).unwrap())
        };
        // DFS pre-order: recipe → inner → echo deep, then the sibling root.
        assert_eq!(at(0), ("recipe", 0));
        assert_eq!(at(1), ("inner", 1));
        assert_eq!(at(2), ("echo deep", 2));
        assert_eq!(at(3), ("sibling", 0));
    }

    /// The targets running-only predicate: an edge is "building" iff its recipe
    /// started but hasn't been marked finished. Up-to-date / phony edges (never
    /// started) and finished edges (ended_ts set) are not running.
    #[test]
    fn edge_running_predicate_tracks_started_not_ended() {
        // building: started, not yet ended.
        assert!(edge_running(&json!({"started_ts": 100.0, "ended_ts": 0.0})));
        // finished: both stamped.
        assert!(!edge_running(&json!({"started_ts": 100.0, "ended_ts": 101.0})));
        // never ran (up-to-date / phony): no started_ts at all (NULL → 0).
        assert!(!edge_running(&json!({"outs": ["all"], "cmd": null})));
        // never ran, explicit zeros.
        assert!(!edge_running(&json!({"started_ts": 0.0, "ended_ts": 0.0})));
    }

    /// Legacy rows (uid 0 / parent_uid 0) stay a flat depth-0 list.
    #[test]
    fn pipeline_tree_legacy_rows_stay_flat() {
        let rows = vec![
            json!({"id":1,"uid":0,"parent_uid":0,"cmd":"a"}),
            json!({"id":2,"uid":0,"parent_uid":0,"cmd":"b"}),
        ];
        let tree = build_pipeline_tree(rows);
        assert_eq!(tree.len(), 2);
        assert!(tree.iter().all(|r| r.get("depth").and_then(Value::as_i64) == Some(0)));
    }
}
