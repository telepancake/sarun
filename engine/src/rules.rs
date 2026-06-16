// File rules (path-glob subset): an ordered apply/discard/passthrough rule
// list, one per line, first match wins — the dominant real-world form
// (`discard **/*.log`, `apply src/**`). The full Python grammar also has
// and/or/not clauses and box:/proc: kinds matched against writer provenance;
// those advanced clause lines are skipped here (path-only rules cover the
// common automation). The rules file is the SAME on-disk file the UI edits.

use glob::Pattern;

use crate::paths;

#[derive(Clone, Copy, PartialEq)]
pub enum Action { Apply, Discard, Passthrough }

pub struct Rule { pub action: Action, pub pat: Pattern }

pub struct Rules { pub rules: Vec<Rule> }

impl Rules {
    pub fn load() -> Rules {
        let text = std::fs::read_to_string(paths::config_home().join("filerules"))
            .unwrap_or_default();
        let mut rules = vec![];
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let mut it = line.split_whitespace();
            let Some(act) = it.next() else { continue };
            let action = match act {
                "apply" => Action::Apply,
                "discard" => Action::Discard,
                // `passthrough <glob>`: the path is HOST-DIRECT — reads served by
                // the kernel (read-passthrough, D5), writes straight to the host,
                // uncaptured. PATH-ONLY by construction (clause lines with ':'
                // are skipped below): a passthrough path must be host-direct in
                // EVERY box, never captured-here-but-passthrough-there, or a
                // child reading through a parent's still-captured copy would hit
                // the kernel's passthrough-vs-write EIO (see DESIGN.md D5).
                "passthrough" => Action::Passthrough,
                _ => continue,
            };
            let rest: Vec<&str> = it.collect();
            // Skip advanced clause lines (and/or/not/off/kind:) — path-only here.
            if rest.len() != 1 || rest[0].contains(':') { continue; }
            if let Ok(pat) = Pattern::new(rest[0]) {
                rules.push(Rule { action, pat });
            }
        }
        Rules { rules }
    }

    /// First-match decision for a path (leading slash stripped), or None.
    pub fn decide(&self, rel: &str) -> Option<Action> {
        let rel = rel.trim_start_matches('/');
        self.rules.iter().find(|r| r.pat.matches(rel)).map(|r| r.action)
    }
}
