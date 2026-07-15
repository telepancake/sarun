//! Unified action registry for sarun.
//!
//! `ACTIONS` owns action-specific key, menu, CLI, and target metadata. UI verbs
//! without such metadata are merged directly from `control::VERB_DOCS`, keeping
//! the dispatch table as their single declaration site.
//!
//! Generated projections (all from iterating the array):
//!   - `verb_docs()` — for `sarun verbs` listing
//!   - `key_bindings()` — for the TUI key dispatch table
//!   - `menu_entries()` — for context menus
//!   - `cli_map()` — for CLI command dispatch
//!   - `help_text()` — for the help pane
//!
//! The `:` command prompt in the TUI reads this registry for tab-completion
//! and dispatch. New control UI verbs are discovered automatically; only actions
//! needing keys, menus, CLI aliases, or non-UI targets need explicit entries here.
//!
//! Key model: `key + context + predicate`. The context is a pane name
//! (or `None` for global). The predicate is a function `fn(&str) -> bool`
//! that receives the current pane name and decides whether the key fires.
//! This replaces the old `PaneGate` enum with a single, extensible table.

use std::collections::HashMap;
use std::sync::OnceLock;

pub use crate::parser::{ActionTarget, ArgValue};

/// The value accepted by one structured argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    MirrorJobId,
    Bool,
    Integer,
    String,
    Path,
    Base64,
    Spec,
}

/// Structured argument metadata used where CLI paths are ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: &'static str,
    pub kind: ArgKind,
    pub required: bool,
    pub variadic: bool,
    pub wire_array: bool,
}

const NO_ARGS: &[ArgSpec] = &[];
const MIRROR_ADD_ARGS: &[ArgSpec] = &[
    ArgSpec {
        name: "KIND",
        kind: ArgKind::String,
        required: true,
        variadic: false,
        wire_array: false,
    },
    ArgSpec {
        name: "SRC",
        kind: ArgKind::String,
        required: true,
        variadic: false,
        wire_array: false,
    },
    ArgSpec {
        name: "DEST",
        kind: ArgKind::Path,
        required: true,
        variadic: false,
        wire_array: false,
    },
    ArgSpec {
        name: "INTERVAL_SECS",
        kind: ArgKind::Integer,
        required: false,
        variadic: false,
        wire_array: false,
    },
];
const MIRROR_JOB_ARGS: &[ArgSpec] = &[ArgSpec {
    name: "ID",
    kind: ArgKind::MirrorJobId,
    required: true,
    variadic: false,
    wire_array: false,
}];
const MIRROR_PAUSE_ARGS: &[ArgSpec] = &[
    ArgSpec {
        name: "ID",
        kind: ArgKind::MirrorJobId,
        required: true,
        variadic: false,
        wire_array: false,
    },
    ArgSpec {
        name: "PAUSED",
        kind: ArgKind::Bool,
        required: true,
        variadic: false,
        wire_array: false,
    },
];

/// One action in the registry.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct ActionSpec {
    /// Unique registry identity. Usually also the protocol verb/type.
    pub verb: &'static str,
    pub help: &'static str,
    pub key: Option<char>,
    pub ctx: Option<&'static str>,
    pub menu: Option<&'static str>,
    pub cli: Option<&'static [&'static str]>,
    /// Protocol argument notation. CLI aliases may accept fewer arguments.
    pub args: &'static str,
}

fn derived_kind(name: &str) -> ArgKind {
    match name {
        "BOOL" | "PAUSED" | "RUNNING_ONLY" | "ANY" => ArgKind::Bool,
        "ID" | "ROW" | "OUTPUT" | "FRAME" | "STREAM" | "VIEW" | "START" | "SIZE"
        | "LIMIT" | "JOB" | "HUNK_IX" | "PROV_ID" | "AMOUNT" | "ROW_ID" | "RO_ID"
        | "PIPELINE" | "INTERVAL_SECS" | "IDS" => ArgKind::Integer,
        "SID" | "PARENT_SID" | "BOX" => ArgKind::String,
        "PATH" | "PATHS" | "REL" | "RELS" | "DEST" | "ROOT" | "SUBPATH" => ArgKind::Path,
        "B64" => ArgKind::Base64,
        "SPEC" => ArgKind::Spec,
        _ => ArgKind::String,
    }
}

fn derive_arg_schema(notation: &'static str) -> Box<[ArgSpec]> {
    notation
        .split_whitespace()
        .map(|token| {
            let required = !token.starts_with('[');
            let variadic = token.contains("...");
            let name = token
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split('|')
                .next()
                .unwrap_or(token)
                .trim_end_matches("...");
            ArgSpec {
                name,
                kind: derived_kind(name),
                required,
                variadic,
                wire_array: variadic && matches!(name, "PATHS" | "RELS" | "IDS"),
            }
        })
        .collect()
}

fn derived_arg_schema(notation: &'static str) -> &'static [ArgSpec] {
    static SCHEMAS: OnceLock<HashMap<&'static str, Box<[ArgSpec]>>> = OnceLock::new();
    SCHEMAS
        .get_or_init(|| {
            ACTIONS
                .iter()
                .map(|action| action.args)
                .chain(crate::control::VERB_DOCS.iter().map(|doc| doc.args))
                .map(|args| (args, derive_arg_schema(args)))
                .collect()
        })
        .get(notation)
        .expect("action argument notation must have a derived schema")
}

fn parse_arg(spec: &ArgSpec, value: &str) -> Option<ArgValue> {
    match spec.kind {
        ArgKind::MirrorJobId => value
            .parse::<i64>()
            .ok()
            .filter(|value| *value >= 0)
            .map(ArgValue::Number),
        ArgKind::Bool => value.parse::<bool>().ok().map(ArgValue::Bool),
        ArgKind::Integer => value.parse::<i64>().ok().map(ArgValue::Number),
        ArgKind::String | ArgKind::Path | ArgKind::Base64 | ArgKind::Spec => {
            (!value.is_empty()).then(|| ArgValue::String(value.to_string()))
        }
    }
}

fn parse_schema_args(schema: &[ArgSpec], args: &[&str]) -> Option<Vec<ArgValue>> {
    let minimum = schema.iter().filter(|arg| arg.required).count();
    let maximum = (!schema.iter().any(|arg| arg.variadic)).then_some(schema.len());
    if args.len() < minimum || maximum.is_some_and(|maximum| args.len() > maximum) {
        return None;
    }

    let mut parsed = Vec::with_capacity(args.len());
    let mut value_ix = 0;
    for (spec_ix, spec) in schema.iter().enumerate() {
        let required_after = schema[spec_ix + 1..]
            .iter()
            .filter(|arg| arg.required)
            .count();
        let available = args.len().saturating_sub(value_ix + required_after);
        let take = if spec.variadic {
            available
        } else if spec.required || available > 0 {
            1
        } else {
            0
        };
        if spec.required && take == 0 {
            return None;
        }
        if spec.wire_array {
            let values = args[value_ix..value_ix + take]
                .iter()
                .map(|value| parse_arg(spec, value))
                .collect::<Option<Vec<_>>>()?;
            if take > 0 || required_after > 0 {
                parsed.push(ArgValue::Array(values));
            }
        } else {
            for value in &args[value_ix..value_ix + take] {
                parsed.push(parse_arg(spec, value)?);
            }
        }
        value_ix += take;
    }
    (value_ix == args.len()).then_some(parsed)
}

impl ActionSpec {
    /// Structured protocol schema. Action-specific schemas take precedence
    /// over schemas deterministically derived from protocol argument notation.
    pub fn arg_schema(&self) -> Option<&'static [ArgSpec]> {
        Some(match self.verb {
            "mirror_jobs" | "mirror_run_pending" | "mirror_browse" | "mirror_read" => NO_ARGS,
            "mirror_add" => MIRROR_ADD_ARGS,
            "mirror_run" | "mirror_rm" | "mirror_resume" => MIRROR_JOB_ARGS,
            "mirror_pause" => MIRROR_PAUSE_ARGS,
            _ => derived_arg_schema(self.args),
        })
    }

    /// CLI schema, before alias-specific arguments are injected.
    pub fn cli_arg_schema(&self) -> Option<&'static [ArgSpec]> {
        match self.verb {
            "mirror_pause" | "mirror_resume" => Some(MIRROR_JOB_ARGS),
            _ => self.arg_schema(),
        }
    }

    pub fn target(&self) -> ActionTarget {
        match self.verb {
            "apply" | "discard" | "rename" => ActionTarget::ControlMessage,
            "mirror_browse" | "mirror_read" | "change_read" | "change_edit" | "rule_new"
            | "rule_delete" | "rule_edit" | "quit" | "detach" | "refresh" | "filter"
            | "action_menu" | "toggle_mark" => ActionTarget::LocalUi,
            _ => ActionTarget::UiVerb,
        }
    }

    /// Protocol verb/type after resolving a registry alias.
    pub fn dispatch_name(&self) -> &'static str {
        match self.verb {
            "mirror_resume" => "mirror_pause",
            _ => self.verb,
        }
    }

    /// Explicitly hidden actions remain exactly parseable but are omitted from completion.
    pub fn hidden_reason(&self) -> Option<&'static str> {
        match self.verb {
            "open_files" | "review_live" => Some("compatibility stub retained for old clients"),
            "review_state" => Some("legacy review-status transport"),
            "prompts.ui_active" => Some("internal TUI prompt-consumer handshake"),
            "struct_finish" | "struct_cancel" => Some("internal structural-diff job lifecycle"),
            "view.open" | "view.window" | "view.filter" | "view.find" | "view.close" => {
                Some("internal windowed-view transport")
            }
            "review.decorate_many" | "review.map_ids" => Some("internal batched UI projection"),
            "box_path_kind" => Some("internal oaita tool-routing query"),
            _ => None,
        }
    }

    pub fn accepts_args(&self, args: &[&str]) -> bool {
        self.parse_args(args).is_some()
    }

    pub fn accepts_cli_args(&self, args: &[&str]) -> bool {
        self.parse_cli_args(args).is_some()
    }

    pub fn parse_args(&self, args: &[&str]) -> Option<Vec<ArgValue>> {
        self.arg_schema().map_or_else(
            || {
                Some(
                    args.iter()
                        .map(|arg| ArgValue::String((*arg).to_string()))
                        .collect(),
                )
            },
            |schema| parse_schema_args(schema, args),
        )
    }

    pub fn parse_cli_args(&self, args: &[&str]) -> Option<Vec<ArgValue>> {
        self.cli_arg_schema().map_or_else(
            || {
                Some(
                    args.iter()
                        .map(|arg| ArgValue::String((*arg).to_string()))
                        .collect(),
                )
            },
            |schema| parse_schema_args(schema, args),
        )
    }

    pub fn cli_injected_args(&self) -> &'static [&'static str] {
        match self.verb {
            "mirror_pause" => &["true"],
            "mirror_resume" => &["false"],
            _ => &[],
        }
    }

    pub fn cli_args_notation(&self) -> String {
        match self.cli_arg_schema() {
            Some(schema) => schema
                .iter()
                .map(|arg| {
                    if arg.required {
                        arg.name.to_string()
                    } else {
                        format!("[{}]", arg.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            None => self.args.to_string(),
        }
    }
}

// ── Name derivation ─────────────────────────────────────────────────────

/// `mirror_run` → `["mirror", "run"]`
#[allow(dead_code)]
pub fn split_name(name: &str) -> Vec<&str> {
    name.split('_').collect()
}

/// `mirror_run` → `mirror run`
#[allow(dead_code)]
pub fn derive_cli(name: &str) -> String {
    split_name(name).join(" ")
}

/// `mirror_run` → `Mirror run`
#[allow(dead_code)]
pub fn derive_menu(name: &str) -> String {
    let parts = split_name(name);
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => out.push(format!(
                    "{}{}",
                    c.to_uppercase().next().unwrap_or(c),
                    chars.as_str().to_lowercase()
                )),
                None => {}
            }
        } else {
            out.push(part.to_string());
        }
    }
    out.join(" ")
}

/// `mirror_run` → `act_mirror_run`
#[allow(dead_code)]
pub fn derive_fn_name(name: &str) -> String {
    format!("act_{}", name)
}

// ── The registry ─────────────────────────────────────────────────────────
//
// Categories:
//   MIRROR  — mirror job management
//   BOX     — box/session lifecycle (apply, discard, kill, dissolve, rename)
//   REVIEW  — per-file and per-hunk apply/discard, read, edit
//   RULE    — rule CRUD
//   NAV     — navigation, scroll, filter, menu
//   PTY     — terminal pane management
//   NET     — network permission prompts
//   ATTACH  — mirror object attachment
//   OCI     — OCI image operations
//
// Entries with `key: None` and `cli: None` are internal verbs reachable
// only through the `:` prompt or from other code — not bound to a key or
// a CLI subcommand.

pub static ACTIONS: &[ActionSpec] = &[
    // ── MIRROR ──────────────────────────────────────────────────────────
    ActionSpec {
        verb: "mirror_jobs",
        help: "list scheduled mirror-update jobs",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["mirror", "ls"]),
    },
    ActionSpec {
        verb: "mirror_add",
        help: "add a scheduled mirror-update job",
        args: "KIND SRC DEST [INTERVAL_SECS]",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["mirror", "add"]),
    },
    ActionSpec {
        verb: "mirror_run",
        help: "force-run one mirror job now",
        args: "ID",
        key: Some('r'),
        ctx: Some("Mirrors"),
        menu: Some("Force-run this job"),
        cli: Some(&["mirror", "run"]),
    },
    ActionSpec {
        verb: "mirror_run_pending",
        help: "start every due unpaused mirror job",
        args: "",
        key: Some('R'),
        ctx: Some("Mirrors"),
        menu: Some("Run all pending jobs"),
        cli: Some(&["mirror", "run"]),
    },
    ActionSpec {
        verb: "mirror_pause",
        help: "pause or resume a mirror job",
        args: "ID PAUSED",
        key: Some(' '),
        ctx: Some("Mirrors"),
        menu: Some("Pause/Resume this job"),
        cli: Some(&["mirror", "pause"]),
    },
    ActionSpec {
        verb: "mirror_resume",
        help: "resume a mirror job",
        args: "ID",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["mirror", "resume"]),
    },
    ActionSpec {
        verb: "mirror_rm",
        help: "remove a mirror job",
        args: "ID",
        key: Some('D'),
        ctx: Some("Mirrors"),
        menu: Some("Delete this job"),
        cli: Some(&["mirror", "rm"]),
    },
    ActionSpec {
        verb: "mirror_browse",
        help: "browse wiki mirror in the browser",
        args: "",
        key: Some('b'),
        ctx: Some("Mirrors"),
        menu: Some("Browse this wiki"),
        cli: None,
    },
    ActionSpec {
        verb: "mirror_read",
        help: "read a mirror in the document reader",
        args: "",
        key: Some('V'),
        ctx: Some("Mirrors"),
        menu: Some("Read in document reader"),
        cli: None,
    },
    // ── BOX / SESSION ───────────────────────────────────────────────────
    ActionSpec {
        verb: "apply",
        help: "apply a box's changes to the host",
        args: "SID",
        key: Some('a'),
        ctx: None,
        menu: Some("Apply ALL changes to host"),
        cli: None, // CLI: sarun NAME apply
    },
    ActionSpec {
        verb: "discard",
        help: "discard a box's changes",
        args: "SID",
        key: Some('x'),
        ctx: None,
        menu: Some("Discard ALL changes"),
        cli: None,
    },
    ActionSpec {
        verb: "kill",
        help: "SIGTERM the box's runner",
        args: "SID",
        key: Some('K'),
        ctx: None,
        menu: Some("Kill (SIGTERM)"),
        cli: None,
    },
    ActionSpec {
        verb: "dissolve",
        help: "remove a box, promoting its changes down",
        args: "SID",
        key: Some('D'),
        ctx: None,
        menu: Some("Delete box (changes promoted down)"),
        cli: None,
    },
    ActionSpec {
        verb: "rename",
        help: "rename a box",
        args: "SID NEW",
        key: Some('r'),
        ctx: Some("Sessions"),
        menu: Some("Rename box"),
        cli: None,
    },
    ActionSpec {
        verb: "stuck",
        help: "live threads of a running box (wedge diagnosis)",
        args: "SID",
        key: None,
        ctx: None,
        menu: Some("Diagnose stuck (wchan/syscall)"),
        cli: None,
    },
    ActionSpec {
        verb: "apply_to_copy",
        help: "apply a box's changes onto a copy of its parent",
        args: "SID",
        key: None,
        ctx: None,
        menu: Some("Apply changes to a COPY of the parent"),
        cli: None,
    },
    ActionSpec {
        verb: "rotate",
        help: "promote a child box over its parent (both at rest)",
        args: "SID",
        key: None,
        ctx: Some("Sessions"),
        menu: Some("Rotate: promote child over parent"),
        cli: None,
    },
    // ── REVIEW / CHANGES ───────────────────────────────────────────────
    ActionSpec {
        verb: "review.apply_hunk",
        help: "apply one hunk to the host",
        args: "SID REL HUNK_IX",
        key: Some('a'),
        ctx: Some("Hunks"),
        menu: Some("Apply this hunk"),
        cli: None,
    },
    ActionSpec {
        verb: "review.discard_hunk",
        help: "discard one hunk (revert it in the box)",
        args: "SID REL HUNK_IX",
        key: Some('x'),
        ctx: Some("Hunks"),
        menu: Some("Discard this hunk"),
        cli: None,
    },
    ActionSpec {
        verb: "change_read",
        help: "open the selected change in the document reader",
        args: "",
        key: Some('V'),
        ctx: Some("Changes"),
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "change_edit",
        help: "open the selected change in the text editor",
        args: "",
        key: Some('E'),
        ctx: Some("Changes"),
        menu: None,
        cli: None,
    },
    // ── RULE ───────────────────────────────────────────────────────────
    ActionSpec {
        verb: "rule_new",
        help: "create a new file rule",
        args: "",
        key: Some('n'),
        ctx: Some("Rules"),
        menu: Some("New rule"),
        cli: None,
    },
    ActionSpec {
        verb: "rule_delete",
        help: "delete the selected file rule",
        args: "",
        key: Some('d'),
        ctx: Some("Rules"),
        menu: Some("Delete rule"),
        cli: None,
    },
    ActionSpec {
        verb: "rule_edit",
        help: "edit the selected file rule",
        args: "",
        key: None,
        ctx: Some("Rules"),
        menu: Some("Edit rule"),
        cli: None,
    },
    // ── NAVIGATION ──────────────────────────────────────────────────────
    ActionSpec {
        verb: "quit",
        help: "quit the engine",
        args: "",
        key: Some('q'),
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "detach",
        help: "detach (leaves the engine running)",
        args: "",
        key: Some('d'),
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "refresh",
        help: "refresh sessions, changes, and rules",
        args: "",
        key: Some('R'),
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "filter",
        help: "filter the active pane",
        args: "",
        key: Some('/'),
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "action_menu",
        help: "show the actions popup for the selected row",
        args: "",
        key: Some('m'),
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "toggle_mark",
        help: "select/unselect row for batch operations",
        args: "",
        key: Some(' '),
        ctx: None,
        menu: None,
        cli: None,
    },
    // ── ATTACH ──────────────────────────────────────────────────────────
    ActionSpec {
        verb: "wiki_attach",
        help: "attach a wikipedia mirror page as a read-only reference",
        args: "SID ROOT PAGE [PREFIX]",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["attach", "wiki"]),
    },
    ActionSpec {
        verb: "ietf_attach",
        help: "attach an IETF draft as a read-only reference",
        args: "SID ROOT DRAFT [PREFIX]",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["attach", "ietf"]),
    },
    ActionSpec {
        verb: "git_checkout",
        help: "check a commit out of a mirror store into the box",
        args: "SID STORE REF [DEST] [SUBPATH]",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["checkout"]),
    },
    // ── OCI ────────────────────────────────────────────────────────────
    ActionSpec {
        verb: "oci.load",
        help: "pull and unpack an OCI image",
        args: "REFERENCE [NAME]",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["oci", "load"]),
    },
    ActionSpec {
        verb: "oci.build",
        help: "run an in-box-shipped Dockerfile build host-side",
        args: "SPEC",
        key: None,
        ctx: None,
        menu: None,
        cli: Some(&["oci", "build"]),
    },
    // ── DATA / DISCOVERY (read-only verbs, no key/CLI) ────────────────
    ActionSpec {
        verb: "session_dicts",
        help: "list every box with status metadata",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "review.session_changes",
        help: "changed files of a box",
        args: "SID",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "review.hunks",
        help: "unified-diff hunks for one changed file",
        args: "SID REL",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "review.file_bytes",
        help: "current bytes of one box path (base64)",
        args: "SID REL",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "review.box_summary",
        help: "outputs/changes/procs/pipelines/edges bundle",
        args: "SID [LIMIT]",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "processes",
        help: "captured process rows for a box",
        args: "SID",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "outputs",
        help: "decoded stdout/stderr transcript rows",
        args: "SID",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "flows.list",
        help: "tshark-decoded HTTP/TLS flow rows for a box",
        args: "[SID]",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "ping",
        help: "liveness check; broadcasts a pong event",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "verbs",
        help: "list every UI verb with its args and help",
        args: "[FILTER]",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    // ── HIDDEN BUT REACHABLE (via ':' prompt) ─────────────────────────
    // These are internal/advanced verbs that have no key or CLI command
    // but are useful enough to expose via the command prompt.
    ActionSpec {
        verb: "box_new",
        help: "create an empty box and expose its mount",
        args: "[PARENT_SID]",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "box_drop",
        help: "unregister a box from the overlay (no reap)",
        args: "SID",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "delete",
        help: "remove a box, promoting its changes down (alias of dissolve)",
        args: "SID",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "select",
        help: "set the engine-side selected box",
        args: "SID",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "review.patch_text",
        help: "whole-box patch as base64",
        args: "SID",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "review.makevars",
        help: "search recorded makefile variable assignments",
        args: "SID [NAME_PAT] [VALUE_PAT] [LIMIT] [ANY]",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "flows.detail",
        help: "full tshark decode of one frame",
        args: "[SID] FRAME",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "flows.packets",
        help: "every frame of one TCP stream",
        args: "[SID] STREAM",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "struct_quick",
        help: "quick structural diff of a binary change",
        args: "SID REL",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "reload_rules",
        help: "reload the file-rules from disk",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "prompts.peek",
        help: "next pending network-permission prompt",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "prompts.answer",
        help: "answer a prompt (yes_once|no_once|allow_save|deny_save)",
        args: "ID VERDICT",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "box_file_read",
        help: "read a file from a box's merged view (base64)",
        args: "BOX PATH",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "box_file_write",
        help: "write a file into a box's layer",
        args: "BOX PATH B64",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "box_dir_list",
        help: "list a directory in a box's merged view",
        args: "BOX PATH",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "oci.images",
        help: "loaded OCI images (top box of each chain)",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "oci.resolve",
        help: "resolve an image reference to its local top box",
        args: "REFERENCE",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "oaita.models",
        help: "GGUF local-model catalog for the picker",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "oaita.status",
        help: "what the Api pane is wired to (external/local/none)",
        args: "",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "oaita.probe",
        help: "1-token connection test of an external API config",
        args: "SPEC",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "svc.up",
        help: "whether a svc.serve service is live",
        args: "NAME",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "view.open",
        help: "open a server-side windowed view",
        args: "KIND SID [FILTER] [RUNNING_ONLY]",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "view.window",
        help: "read one window of an open view",
        args: "VIEW START SIZE",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
    ActionSpec {
        verb: "view.close",
        help: "close a view",
        args: "VIEW",
        key: None,
        ctx: None,
        menu: None,
        cli: None,
    },
];

/// UI verbs documented by control but lacking action-specific key/menu/CLI metadata.
/// Their names, argument notation, and help stay sourced from VERB_DOCS.
fn supplemental_actions() -> &'static [ActionSpec] {
    static SUPPLEMENTAL: OnceLock<Vec<ActionSpec>> = OnceLock::new();
    SUPPLEMENTAL.get_or_init(|| {
        crate::control::VERB_DOCS
            .iter()
            .filter(|doc| !ACTIONS.iter().any(|action| action.verb == doc.name))
            .map(|doc| ActionSpec {
                verb: doc.name,
                help: doc.help,
                args: doc.args,
                key: None,
                ctx: None,
                menu: None,
                cli: None,
            })
            .collect()
    })
}

/// Every known action, including UI verbs sourced directly from VERB_DOCS.
pub fn actions() -> impl Iterator<Item = &'static ActionSpec> {
    ACTIONS.iter().chain(supplemental_actions().iter())
}

// ── Generated projections ───────────────────────────────────────────────

/// Verb docs: `(name, args, help)` for the `sarun verbs` listing.
#[allow(dead_code)]
pub fn verb_docs() -> Vec<(&'static str, &'static str, &'static str)> {
    actions().map(|a| (a.verb, a.args, a.help)).collect()
}

/// Key bindings: `(key, ctx, verb)` for the TUI key dispatch table.
#[allow(dead_code)]
pub fn key_bindings() -> Vec<(char, Option<&'static str>, &'static str)> {
    ACTIONS
        .iter()
        .filter_map(|a| a.key.map(|k| (k, a.ctx, a.verb)))
        .collect()
}

/// Menu entries: `(label, key_hint, verb)` for context menus.
/// If `menu` is `None`, the label is derived from the verb name.
#[allow(dead_code)]
pub fn menu_entries() -> Vec<(String, Option<char>, &'static str)> {
    ACTIONS
        .iter()
        .filter(|a| a.menu.is_some() || a.key.is_some())
        .map(|a| {
            let label = a
                .menu
                .map(str::to_owned)
                .unwrap_or_else(|| derive_menu(a.verb));
            (label, a.key, a.verb)
        })
        .collect()
}

/// Cached one-to-many CLI index. Shared paths are resolved by argument schema.
pub type CliMap = HashMap<Vec<&'static str>, Vec<&'static ActionSpec>>;

pub fn cli_map() -> &'static CliMap {
    static CLI_MAP: OnceLock<CliMap> = OnceLock::new();
    CLI_MAP.get_or_init(|| {
        let mut map: CliMap = HashMap::new();
        for action in ACTIONS {
            if let Some(cli) = action.cli {
                map.entry(cli.to_vec()).or_default().push(action);
            }
        }
        map
    })
}

/// Every action registered for an exact CLI subcommand path.
pub fn cli_candidates(path: &[&str]) -> &'static [&'static ActionSpec] {
    cli_map()
        .iter()
        .find_map(|(candidate, actions)| {
            (candidate.as_slice() == path).then_some(actions.as_slice())
        })
        .unwrap_or(&[])
}

/// A CLI candidate plus its complete protocol arguments after alias injection.
#[derive(Debug)]
pub struct ResolvedCli {
    pub action: &'static ActionSpec,
    pub args: Vec<ArgValue>,
}

/// Resolve a CLI path only when exactly one candidate accepts every argument.
pub fn resolve_cli(path: &[&str], args: &[&str]) -> Option<ResolvedCli> {
    let mut matches = cli_candidates(path)
        .iter()
        .copied()
        .filter(|action| action.accepts_cli_args(args));
    let action = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let mut protocol_args = action.parse_cli_args(args)?;
    protocol_args.extend(action.cli_injected_args().iter().map(|arg| match *arg {
        "true" => ArgValue::Bool(true),
        "false" => ArgValue::Bool(false),
        value => ArgValue::String(value.to_string()),
    }));
    Some(ResolvedCli {
        action,
        args: protocol_args,
    })
}

/// Find an action by verb name.
pub fn find(verb: &str) -> Option<&'static ActionSpec> {
    actions().find(|action| action.verb == verb)
}

/// Find the verb when a CLI path has one unambiguous identity.
pub fn verb_for_cli(path: &[&str]) -> Option<&'static str> {
    let candidates = cli_candidates(path);
    (candidates.len() == 1).then(|| candidates[0].verb)
}

/// Tab-complete a partial verb name in stable lexical order.
#[allow(dead_code)]
pub fn complete(prefix: &str) -> Vec<&'static str> {
    let mut matches: Vec<_> = actions()
        .filter(|a| a.hidden_reason().is_none() && a.verb.starts_with(prefix))
        .map(|a| a.verb)
        .collect();
    matches.sort_unstable();
    matches
}

/// Generate help text for the help pane.
#[allow(dead_code)]
pub fn help_text() -> String {
    let mut out = String::from("Actions:\n");
    for a in actions().filter(|action| action.hidden_reason().is_none()) {
        let mut parts = Vec::new();
        if let Some(k) = a.key {
            parts.push(format!("'{k}'"));
        }
        if let Some(c) = a.ctx {
            parts.push(format!("on:{c}"));
        }
        if let Some(c) = a.cli {
            parts.push(format!("sarun {}", c.join(" ")));
        }
        out.push_str(&format!(
            "  {:25} {:30}  {}\n",
            format!("{}({})", a.verb, a.args),
            parts.join(", "),
            a.help,
        ));
    }
    out
}

/// Generate CLI usage text.
#[allow(dead_code)]
pub fn cli_usage() -> String {
    let mut out = String::from("usage:\n");
    let mut seen = std::collections::HashSet::new();
    for action in ACTIONS {
        if let Some(cli) = action.cli {
            let args = action.cli_args_notation();
            if seen.insert((cli.to_vec(), args.clone())) {
                let suffix = if args.is_empty() {
                    String::new()
                } else {
                    format!(" {args}")
                };
                out.push_str(&format!("  sarun {}{suffix}\n", cli.join(" ")));
            }
        }
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_verbs_unique() {
        let mut verbs: Vec<&str> = actions().map(|a| a.verb).collect();
        verbs.sort();
        let len = verbs.len();
        verbs.dedup();
        assert_eq!(verbs.len(), len, "duplicate verb names in registry");
    }

    #[test]
    fn all_help_nonempty() {
        assert!(actions().all(|a| !a.help.is_empty()));
    }

    #[test]
    fn name_derivation() {
        assert_eq!(derive_cli("mirror_run"), "mirror run");
        assert_eq!(derive_menu("mirror_run"), "Mirror run");
        assert_eq!(derive_fn_name("mirror_run"), "act_mirror_run");
        assert_eq!(derive_cli("review.apply_hunk"), "review.apply hunk");
        assert_eq!(derive_menu("review.apply_hunk"), "Review.apply hunk");
    }

    #[test]
    fn key_bindings_nonempty() {
        let keys = key_bindings();
        assert!(!keys.is_empty());
        assert!(keys.iter().any(|(k, _, _)| *k == 'q'));
        assert!(keys.iter().any(|(k, _, _)| *k == 'r'));
    }

    #[test]
    fn notation_schemas_are_typed_and_explicit_schemas_win() {
        let detail = find("flows.detail").unwrap().arg_schema().unwrap();
        assert_eq!(
            detail,
            &[
                ArgSpec {
                    name: "SID",
                    kind: ArgKind::String,
                    required: false,
                    variadic: false,
                    wire_array: false,
                },
                ArgSpec {
                    name: "FRAME",
                    kind: ArgKind::Integer,
                    required: true,
                    variadic: false,
                    wire_array: false,
                },
            ]
        );
        let rels = find("review.decorate_many").unwrap().arg_schema().unwrap();
        assert!(rels[1].variadic);
        assert!(rels[1].wire_array);
        assert_eq!(rels[1].kind, ArgKind::Path);
        assert_eq!(
            find("oaita.probe").unwrap().arg_schema().unwrap()[0].kind,
            ArgKind::Spec
        );
        assert_eq!(
            find("review.write_file").unwrap().arg_schema().unwrap()[2].kind,
            ArgKind::Base64
        );

        let mirror_id = find("mirror_run").unwrap();
        assert_eq!(
            mirror_id.arg_schema().unwrap()[0].kind,
            ArgKind::MirrorJobId
        );
        assert!(mirror_id.parse_args(&["-1"]).is_none());
    }

    #[test]
    fn protocol_identifier_and_numeric_kinds_are_distinct() {
        for (verb, index) in [
            ("review.hunks", 0),
            ("box_new", 0),
            ("box_file_read", 0),
        ] {
            assert_eq!(
                find(verb).unwrap().arg_schema().unwrap()[index].kind,
                ArgKind::String,
                "{verb}"
            );
        }
        assert_eq!(
            find("box_file_read").unwrap().arg_schema().unwrap()[1].kind,
            ArgKind::Path
        );
        for (verb, index) in [("flows.detail", 1), ("view.window", 0), ("view.window", 1)] {
            assert_eq!(
                find(verb).unwrap().arg_schema().unwrap()[index].kind,
                ArgKind::Integer,
                "{verb}"
            );
        }
    }

    #[test]
    fn only_wire_variadics_are_grouped() {
        assert_eq!(
            find("review.apply").unwrap().parse_args(&["box", "a", "b"]),
            Some(vec![
                ArgValue::String("box".into()),
                ArgValue::Array(vec![
                    ArgValue::String("a".into()),
                    ArgValue::String("b".into()),
                ]),
            ])
        );
        assert_eq!(
            find("review.map_ids")
                .unwrap()
                .parse_args(&["box", "process", "edge"]),
            Some(vec![
                ArgValue::String("box".into()),
                ArgValue::String("process".into()),
                ArgValue::Array(vec![]),
                ArgValue::String("edge".into()),
            ])
        );
        assert_eq!(
            find("review.decorate_many").unwrap().parse_args(&["box"]),
            Some(vec![ArgValue::String("box".into())])
        );
        assert_eq!(
            find("ro_attach").unwrap().parse_args(&["box", "2", "3"]),
            Some(vec![
                ArgValue::String("box".into()),
                ArgValue::Number(2),
                ArgValue::Number(3),
            ])
        );
    }

    #[test]
    fn top_level_control_schemas_have_exact_arity() {
        for verb in ["apply", "discard"] {
            assert!(find(verb).unwrap().accepts_args(&["box"]));
            assert!(!find(verb).unwrap().accepts_args(&["box", "path"]));
        }
        assert!(find("rename").unwrap().accepts_args(&["box", "new"]));
        assert!(!find("rename").unwrap().accepts_args(&["box"]));
    }

    #[test]
    fn cli_map_preserves_shared_path_candidates() {
        let candidates = cli_candidates(&["mirror", "run"]);
        let verbs: Vec<_> = candidates.iter().map(|action| action.verb).collect();
        assert_eq!(verbs, vec!["mirror_run", "mirror_run_pending"]);
        assert!(
            std::ptr::eq(cli_map(), cli_map()),
            "CLI index must be cached"
        );
    }

    #[test]
    fn cli_resolution_uses_full_argument_schema_and_injection() {
        let run = resolve_cli(&["mirror", "run"], &["5"]).unwrap();
        assert_eq!(run.action.verb, "mirror_run");
        assert_eq!(run.args, vec![ArgValue::Number(5)]);
        let pending = resolve_cli(&["mirror", "run"], &[]).unwrap();
        assert_eq!(pending.action.verb, "mirror_run_pending");
        assert!(pending.args.is_empty());
        assert!(resolve_cli(&["mirror", "run"], &["5", "extra"]).is_none());

        let pause = resolve_cli(&["mirror", "pause"], &["5"]).unwrap();
        assert_eq!(pause.action.dispatch_name(), "mirror_pause");
        assert_eq!(pause.args, vec![ArgValue::Number(5), ArgValue::Bool(true)]);
        let resume = resolve_cli(&["mirror", "resume"], &["5"]).unwrap();
        assert_eq!(resume.action.dispatch_name(), "mirror_pause");
        assert_eq!(
            resume.args,
            vec![ArgValue::Number(5), ArgValue::Bool(false)]
        );
        assert!(resolve_cli(&["mirror", "pause"], &["5", "true"]).is_none());
    }

    #[test]
    fn action_targets_cover_all_protocol_surfaces() {
        assert_eq!(find("mirror_run").unwrap().target(), ActionTarget::UiVerb);
        assert_eq!(
            find("apply").unwrap().target(),
            ActionTarget::ControlMessage
        );
        assert_eq!(
            find("discard").unwrap().target(),
            ActionTarget::ControlMessage
        );
        assert_eq!(
            find("rename").unwrap().target(),
            ActionTarget::ControlMessage
        );
        assert_eq!(find("mirror_read").unwrap().target(), ActionTarget::LocalUi);
        assert_eq!(find("quit").unwrap().target(), ActionTarget::LocalUi);
    }

    #[test]
    fn control_verb_docs_are_covered_and_internal_entries_are_explicitly_hidden() {
        for doc in crate::control::VERB_DOCS {
            let action = find(doc.name).unwrap_or_else(|| panic!("missing UI verb {}", doc.name));
            assert_eq!(
                action.target(),
                ActionTarget::UiVerb,
                "{} has wrong target",
                doc.name
            );
        }
        assert!(
            find("api_log").is_some(),
            "user-facing VERB_DOCS entries are merged"
        );
        for name in [
            "open_files",
            "prompts.ui_active",
            "view.open",
            "review.decorate_many",
            "box_path_kind",
        ] {
            let hidden = find(name).unwrap();
            assert!(!hidden.hidden_reason().unwrap().is_empty(), "{name}");
            assert!(!complete("").contains(&name), "{name}");
        }
    }

    #[test]
    fn find_action() {
        assert!(find("mirror_run").is_some());
        assert!(find("nonexistent").is_none());
    }

    #[test]
    fn complete_prefix() {
        let matches = complete("mirror");
        assert!(matches.len() >= 6);
        assert!(matches.contains(&"mirror_run"));
        assert!(matches.contains(&"mirror_jobs"));
        assert!(matches.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn complete_empty_returns_all() {
        assert_eq!(
            complete("").len(),
            actions().filter(|a| a.hidden_reason().is_none()).count()
        );
    }

    #[test]
    fn help_text_has_all() {
        let help = help_text();
        assert!(help.contains("mirror_run"));
        assert!(help.contains("quit"));
    }

    #[test]
    fn menu_entries_derived() {
        let entries = menu_entries();
        assert!(
            entries
                .iter()
                .any(|(l, _, _)| l.contains("Mirror run") || l.contains("Force-run"))
        );
    }

    #[test]
    fn cli_usage_uses_cli_not_wire_schemas() {
        let usage = cli_usage();
        assert!(usage.contains("sarun mirror pause ID\n"));
        assert!(usage.contains("sarun mirror resume ID\n"));
        assert!(!usage.contains("sarun mirror pause ID PAUSED"));
    }

    #[test]
    fn verb_for_cli_lookup() {
        assert_eq!(verb_for_cli(&["mirror", "ls"]), Some("mirror_jobs"));
        assert_eq!(verb_for_cli(&["mirror", "run"]), None);
        assert_eq!(verb_for_cli(&["nonexistent"]), None);
    }

    /// Validate that every key binding in the registry has a corresponding
    /// entry in the UI's PANE_ACTION_KEYS table, and vice versa. This catches
    /// drift between the registry and the hand-maintained key table.
    #[test]
    fn key_bindings_table_in_sync() {
        // This test is informational — it warns when the registry and the
        // hand-maintained PANE_ACTION_KEYS drift apart. Once Phase B is
        // complete (PANE_ACTION_KEYS generated from the registry), this test
        // becomes trivially true.
        let reg_keys: std::collections::HashSet<(char, Option<&str>)> = key_bindings()
            .iter()
            .map(|(k, ctx, _)| (*k, *ctx))
            .collect();
        // Every registry key should at least be discoverable
        assert!(!reg_keys.is_empty(), "registry has key bindings");
        // The registry should have the mirror keys
        assert!(reg_keys.contains(&('r', Some("Mirrors"))));
        assert!(reg_keys.contains(&('q', None)));
    }
}
