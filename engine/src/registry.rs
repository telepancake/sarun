//! Unified action registry for sarun.
//!
//! A single `const ACTIONS` array is the source of truth for every
//! user-reachable action. Name derivation rules produce the CLI command,
//! menu label, and function name from the verb identity — no redundant
//! fields that can drift.
//!
//! Generated projections (all from iterating the array):
//!   - `verb_docs()` — for `sarun verbs` listing
//!   - `key_bindings()` — for the TUI key dispatch table
//!   - `menu_entries()` — for context menus
//!   - `cli_map()` — for CLI command dispatch
//!   - `help_text()` — for the help pane
//!
//! The `:` command prompt in the TUI reads this registry for tab-completion
//! and dispatch. Adding a new action means adding one `ActionSpec` entry —
//! no hunting through 6 files.
//!
//! Key model: `key + context + predicate`. The context is a pane name
//! (or `None` for global). The predicate is a function `fn(&str) -> bool`
//! that receives the current pane name and decides whether the key fires.
//! This replaces the old `PaneGate` enum with a single, extensible table.

use std::collections::HashMap;

/// One action in the registry.
#[derive(Debug, Clone, Copy)]
pub struct ActionSpec {
    /// Wire verb name (identity). Sent over the control socket as
    /// `{"verb":"<name>"}`. Also the canonical action identity.
    pub verb: &'static str,
    /// Human help text — the one thing that can't be derived.
    pub help: &'static str,
    /// TUI key binding, if any.
    pub key: Option<char>,
    /// Pane context: `None` = global, `Some("Mirrors")` = only on Mirrors.
    pub ctx: Option<&'static str>,
    /// Explicit menu label. If `None`, derived from the verb name.
    pub menu: Option<&'static str>,
    /// CLI subcommand path: `&["mirror", "run"]` means `sarun mirror run`.
    /// If `None`, the action is not reachable from the CLI.
    pub cli: Option<&'static [&'static str]>,
    /// Args notation for help: `UPPER` = required, `[X]` = optional.
    pub args: &'static str,
}

// ── Name derivation ─────────────────────────────────────────────────────

/// `mirror_run` → `["mirror", "run"]`
pub fn split_name(name: &str) -> Vec<&str> {
    name.split('_').collect()
}

/// `mirror_run` → `mirror run`
pub fn derive_cli(name: &str) -> String {
    split_name(name).join(" ")
}

/// `mirror_run` → `Mirror run`
pub fn derive_menu(name: &str) -> String {
    let parts = split_name(name);
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => out.push(format!(
                    "{}{}", c.to_uppercase().next().unwrap_or(c),
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
        verb: "mirror_jobs", help: "list scheduled mirror-update jobs",
        args: "", key: None, ctx: None, menu: None,
        cli: Some(&["mirror", "ls"]),
    },
    ActionSpec {
        verb: "mirror_add", help: "add a scheduled mirror-update job",
        args: "KIND SRC DEST [INTERVAL_SECS]", key: None, ctx: None, menu: None,
        cli: Some(&["mirror", "add"]),
    },
    ActionSpec {
        verb: "mirror_run", help: "force-run one mirror job now",
        args: "ID", key: Some('r'), ctx: Some("Mirrors"),
        menu: Some("Force-run this job"),
        cli: Some(&["mirror", "run"]),
    },
    ActionSpec {
        verb: "mirror_run_pending", help: "start every due unpaused mirror job",
        args: "", key: Some('R'), ctx: Some("Mirrors"),
        menu: Some("Run all pending jobs"),
        cli: Some(&["mirror", "run"]),
    },
    ActionSpec {
        verb: "mirror_pause", help: "pause or resume a mirror job",
        args: "ID PAUSED", key: Some(' '), ctx: Some("Mirrors"),
        menu: Some("Pause/Resume this job"),
        cli: Some(&["mirror", "pause"]),
    },
    ActionSpec {
        verb: "mirror_rm", help: "remove a mirror job",
        args: "ID", key: Some('D'), ctx: Some("Mirrors"),
        menu: Some("Delete this job"),
        cli: Some(&["mirror", "rm"]),
    },
    ActionSpec {
        verb: "mirror_browse", help: "browse wiki mirror in the browser",
        args: "", key: Some('b'), ctx: Some("Mirrors"),
        menu: Some("Browse this wiki"), cli: None,
    },
    ActionSpec {
        verb: "mirror_read", help: "read a mirror in the document reader",
        args: "", key: Some('V'), ctx: Some("Mirrors"),
        menu: Some("Read in document reader"), cli: None,
    },

    // ── BOX / SESSION ───────────────────────────────────────────────────
    ActionSpec {
        verb: "apply", help: "apply a box's changes to the host",
        args: "SID [PATHS]", key: Some('a'), ctx: None,
        menu: Some("Apply ALL changes to host"),
        cli: None,  // CLI: sarun NAME apply
    },
    ActionSpec {
        verb: "discard", help: "discard a box's changes",
        args: "SID [PATHS]", key: Some('x'), ctx: None,
        menu: Some("Discard ALL changes"),
        cli: None,
    },
    ActionSpec {
        verb: "kill", help: "SIGTERM the box's runner",
        args: "SID", key: Some('K'), ctx: None,
        menu: Some("Kill (SIGTERM)"),
        cli: None,
    },
    ActionSpec {
        verb: "dissolve", help: "remove a box, promoting its changes down",
        args: "SID", key: Some('D'), ctx: None,
        menu: Some("Delete box (changes promoted down)"),
        cli: None,
    },
    ActionSpec {
        verb: "rename", help: "rename a box",
        args: "SID NEW", key: Some('r'), ctx: Some("Sessions"),
        menu: Some("Rename box"),
        cli: None,
    },
    ActionSpec {
        verb: "stuck", help: "live threads of a running box (wedge diagnosis)",
        args: "SID", key: None, ctx: None,
        menu: Some("Diagnose stuck (wchan/syscall)"),
        cli: None,
    },
    ActionSpec {
        verb: "apply_to_copy", help: "apply a box's changes onto a copy of its parent",
        args: "SID", key: None, ctx: None,
        menu: Some("Apply changes to a COPY of the parent"),
        cli: None,
    },
    ActionSpec {
        verb: "rotate", help: "promote a child box over its parent (both at rest)",
        args: "SID", key: None, ctx: Some("Sessions"),
        menu: Some("Rotate: promote child over parent"),
        cli: None,
    },

    // ── REVIEW / CHANGES ───────────────────────────────────────────────
    ActionSpec {
        verb: "review.apply_hunk", help: "apply one hunk to the host",
        args: "SID REL HUNK_IX", key: Some('a'), ctx: Some("Hunks"),
        menu: Some("Apply this hunk"), cli: None,
    },
    ActionSpec {
        verb: "review.discard_hunk", help: "discard one hunk (revert it in the box)",
        args: "SID REL HUNK_IX", key: Some('x'), ctx: Some("Hunks"),
        menu: Some("Discard this hunk"), cli: None,
    },
    ActionSpec {
        verb: "change_read", help: "open the selected change in the document reader",
        args: "", key: Some('V'), ctx: Some("Changes"),
        menu: None, cli: None,
    },
    ActionSpec {
        verb: "change_edit", help: "open the selected change in the text editor",
        args: "", key: Some('E'), ctx: Some("Changes"),
        menu: None, cli: None,
    },

    // ── RULE ───────────────────────────────────────────────────────────
    ActionSpec {
        verb: "rule_new", help: "create a new file rule",
        args: "", key: Some('n'), ctx: Some("Rules"),
        menu: Some("New rule"), cli: None,
    },
    ActionSpec {
        verb: "rule_delete", help: "delete the selected file rule",
        args: "", key: Some('d'), ctx: Some("Rules"),
        menu: Some("Delete rule"), cli: None,
    },
    ActionSpec {
        verb: "rule_edit", help: "edit the selected file rule",
        args: "", key: None, ctx: Some("Rules"),
        menu: Some("Edit rule"), cli: None,
    },

    // ── NAVIGATION ──────────────────────────────────────────────────────
    ActionSpec {
        verb: "quit", help: "quit the engine",
        args: "", key: Some('q'), ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "detach", help: "detach (leaves the engine running)",
        args: "", key: Some('d'), ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "refresh", help: "refresh sessions, changes, and rules",
        args: "", key: Some('R'), ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "filter", help: "filter the active pane",
        args: "", key: Some('/'), ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "action_menu", help: "show the actions popup for the selected row",
        args: "", key: Some('m'), ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "toggle_mark", help: "select/unselect row for batch operations",
        args: "", key: Some(' '), ctx: None, menu: None, cli: None,
    },

    // ── ATTACH ──────────────────────────────────────────────────────────
    ActionSpec {
        verb: "wiki_attach", help: "attach a wikipedia mirror page as a read-only reference",
        args: "SID ROOT PAGE [PREFIX]", key: None, ctx: None, menu: None,
        cli: Some(&["attach", "wiki"]),
    },
    ActionSpec {
        verb: "ietf_attach", help: "attach an IETF draft as a read-only reference",
        args: "SID ROOT DRAFT [PREFIX]", key: None, ctx: None, menu: None,
        cli: Some(&["attach", "ietf"]),
    },
    ActionSpec {
        verb: "git_checkout", help: "check a commit out of a mirror store into the box",
        args: "SID STORE REF [DEST] [SUBPATH]", key: None, ctx: None, menu: None,
        cli: Some(&["checkout"]),
    },

    // ── OCI ────────────────────────────────────────────────────────────
    ActionSpec {
        verb: "oci.load", help: "pull and unpack an OCI image",
        args: "REFERENCE [NAME]", key: None, ctx: None, menu: None,
        cli: Some(&["oci", "load"]),
    },
    ActionSpec {
        verb: "oci.build", help: "run an in-box-shipped Dockerfile build host-side",
        args: "SPEC", key: None, ctx: None, menu: None,
        cli: Some(&["oci", "build"]),
    },

    // ── DATA / DISCOVERY (read-only verbs, no key/CLI) ────────────────
    ActionSpec {
        verb: "session_dicts", help: "list every box with status metadata",
        args: "", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "review.session_changes", help: "changed files of a box",
        args: "SID", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "review.hunks", help: "unified-diff hunks for one changed file",
        args: "SID REL", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "review.file_bytes", help: "current bytes of one box path (base64)",
        args: "SID REL", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "review.box_summary", help: "outputs/changes/procs/pipelines/edges bundle",
        args: "SID [LIMIT]", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "processes", help: "captured process rows for a box",
        args: "SID", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "outputs", help: "decoded stdout/stderr transcript rows",
        args: "SID", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "flows.list", help: "tshark-decoded HTTP/TLS flow rows for a box",
        args: "[SID]", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "ping", help: "liveness check; broadcasts a pong event",
        args: "", key: None, ctx: None, menu: None, cli: None,
    },
    ActionSpec {
        verb: "verbs", help: "list every UI verb with its args and help",
        args: "[FILTER]", key: None, ctx: None, menu: None, cli: None,
    },
];

// ── Generated projections ───────────────────────────────────────────────

/// Verb docs: `(name, args, help)` for the `sarun verbs` listing.
pub fn verb_docs() -> Vec<(&'static str, &'static str, &'static str)> {
    ACTIONS.iter().map(|a| (a.verb, a.args, a.help)).collect()
}

/// Key bindings: `(key, ctx, verb)` for the TUI key dispatch table.
pub fn key_bindings() -> Vec<(char, Option<&'static str>, &'static str)> {
    ACTIONS.iter()
        .filter_map(|a| a.key.map(|k| (k, a.ctx, a.verb)))
        .collect()
}

/// Menu entries: `(label, key_hint, verb)` for context menus.
/// If `menu` is `None`, the label is derived from the verb name.
pub fn menu_entries() -> Vec<(&'static str, Option<char>, &'static str)> {
    ACTIONS.iter()
        .filter(|a| a.menu.is_some() || a.key.is_some())
        .filter_map(|a| {
            let label = a.menu.unwrap_or_else(|| {
                // Derive from verb, leaking the String (static lifetime needed).
                // This is fine — the set of verbs is fixed at compile time.
                Box::leak(derive_menu(a.verb).into_boxed_str())
            });
            Some((label, a.key, a.verb))
        })
        .collect()
}

/// CLI map: `subcommand_path → verb` for CLI dispatch.
pub fn cli_map() -> HashMap<Vec<&'static str>, &'static str> {
    let mut m = HashMap::new();
    for a in ACTIONS {
        if let Some(cli) = a.cli {
            m.insert(cli.to_vec(), a.verb);
        }
    }
    m
}

/// Find an action by verb name.
pub fn find(verb: &str) -> Option<&'static ActionSpec> {
    ACTIONS.iter().find(|a| a.verb == verb)
}

/// Find the verb for a CLI subcommand path.
pub fn verb_for_cli(path: &[&str]) -> Option<&'static str> {
    cli_map().get(path).copied()
}

/// Tab-complete a partial verb name.
pub fn complete(prefix: &str) -> Vec<&'static str> {
    ACTIONS.iter()
        .filter(|a| a.verb.starts_with(prefix))
        .map(|a| a.verb)
        .collect()
}

/// Generate help text for the help pane.
pub fn help_text() -> String {
    let mut out = String::from("Actions:\n");
    for a in ACTIONS {
        let mut parts = Vec::new();
        if let Some(k) = a.key { parts.push(format!("'{k}'")); }
        if let Some(c) = a.ctx { parts.push(format!("on:{c}")); }
        if let Some(c) = a.cli { parts.push(format!("sarun {}", c.join(" "))); }
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
pub fn cli_usage() -> String {
    let mut out = String::from("usage:\n");
    let mut seen = std::collections::HashSet::new();
    for a in ACTIONS {
        if let Some(cli) = a.cli {
            let key = cli.to_vec();
            if seen.insert(key) {
                out.push_str(&format!("  sarun {} {}\n", cli.join(" "), a.args));
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
        let mut verbs: Vec<&str> = ACTIONS.iter().map(|a| a.verb).collect();
        verbs.sort();
        let len = verbs.len();
        verbs.dedup();
        assert_eq!(verbs.len(), len, "duplicate verb names in registry");
    }

    #[test]
    fn all_help_nonempty() {
        assert!(ACTIONS.iter().all(|a| !a.help.is_empty()));
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
    fn cli_map_correct() {
        let m = cli_map();
        assert_eq!(m.get(&vec!["mirror", "ls"]), Some(&"mirror_jobs"));
        assert!(m.contains_key(&vec!["mirror", "run"]));
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
    }

    #[test]
    fn complete_empty_returns_all() {
        assert_eq!(complete("").len(), ACTIONS.len());
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
        assert!(entries.iter().any(|(l, _, _)| l.contains("Mirror run") || l.contains("Force-run")));
    }

    #[test]
    fn verb_for_cli_lookup() {
        assert_eq!(verb_for_cli(&["mirror", "ls"]), Some("mirror_jobs"));
        assert_eq!(verb_for_cli(&["nonexistent"]), None);
    }
}
