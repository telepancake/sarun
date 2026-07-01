// File rules — FULL parity with the Python clause grammar (sarun: Match/Clause/
// FileRule/eval_clauses/FileRules.decide). An ordered apply/discard/passthrough
// rule list, one rule per line, first match wins. The on-disk file is the SAME
// one the Python UI edits, so parse/round-trip are byte-identical.
//
// Grammar:  ACTION [off] [not] PRED [and|or [off] [not] PRED]...
//   PRED = kind:pattern | bare-path-glob   (kind defaults to `path`)
//   KINDS: path (path glob), box (name glob), exe/cwd (writer path globs),
//          arg (writer argv glob), ids (internal comma row-id set).
//
// Matching uses the shared extended-glob vocabulary (wcmatch GLOBSTAR | EXTGLOB
// | BRACE | DOTGLOB), reimplemented faithfully in `glob` below so a Rust
// decision equals the Python decision on the identical inputs.
//
// D5 INTERACTION (see DESIGN.md D5 and test_passthrough_rule_rs):
//   `decide(rel, subject)` is the FULL-grammar decision used by review /
//   dissolve-finalize and by the live host-direct WRITE routing. The kernel
//   READ-passthrough divergence (a backing fd) is gated SEPARATELY on
//   `passthrough_path_only(rel)` — a passthrough match whose clauses are ALL
//   `path` kind — so a box-/proc-scoped passthrough never enables the
//   captured-here-but-passthrough-there read divergence.

use crate::paths;

pub mod glob;

// ── data model (mirrors Python Match / Clause / FileRule) ────────────────────

pub const FILE_KINDS: &[&str] = &["path", "box", "exe", "cwd", "arg"];

/// Network rule kinds — used by `-n` boxes' connection gate (see
/// crate::net::policy). The matcher engine + glob vocabulary are the SAME
/// as for FILE_KINDS; only the field resolver differs by context. A rule
/// file may freely mix file and net kinds in a single clause (e.g.
/// `discard box:* and host:bad.com` to deny bad.com for every box).
pub const NET_KINDS: &[&str] = &[
    "host", "port", "scheme", "sni",
    "http_path", "http_method", "http_status",
    "proto", "box", "exe", "cwd", "arg",
];


#[cfg(test)]
mod parse_net_rules {
    use super::*;
    #[test]
    fn host_kind_parses_as_net_clause() {
        // Without the NET_KINDS branch in parse_clauses, "host:bad.com" was
        // being treated as a bare path glob with literal "host:bad.com" in
        // the pattern. Regression: net rules MUST keep their kind.
        let r = FileRule::parse("discard host:bad.com").unwrap();
        assert_eq!(r.action, Action::Discard);
        assert_eq!(r.clauses.len(), 1);
        assert_eq!(r.clauses[0].m.kind, "host");
        assert_eq!(r.clauses[0].m.pattern, "bad.com");
    }
    #[test]
    fn mixed_file_and_net_kinds_in_one_rule() {
        let r = FileRule::parse(
            "discard host:tracker.example and box:BAD").unwrap();
        assert_eq!(r.clauses.len(), 2);
        assert_eq!(r.clauses[0].m.kind, "host");
        assert_eq!(r.clauses[1].m.kind, "box");
    }
    #[test]
    fn port_and_scheme_globs() {
        let r = FileRule::parse("apply scheme:https and port:443").unwrap();
        assert_eq!(r.clauses[0].m.kind, "scheme");
        assert_eq!(r.clauses[1].m.kind, "port");
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action { Apply, Discard, Passthrough,
                  /// Net-only: a matching `ask` rule prompts the user via
                  /// the banner-queue when the TUI is up, denies if not.
                  /// The user's chosen YesOnce/NoOnce verdict drives the
                  /// per-conn outcome; AllowSave/DenySave additionally
                  /// appends a new `apply host:H` / `discard host:H` line
                  /// to the filerules file so the next conn skips the
                  /// banner. File-rule paths ignore Ask (treated as Apply).
                  Ask }

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        match s {
            "apply" => Some(Action::Apply),
            "discard" => Some(Action::Discard),
            "passthrough" => Some(Action::Passthrough),
            "ask" => Some(Action::Ask),
            _ => None,
        }
    }
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self { Action::Apply => "apply", Action::Discard => "discard",
                     Action::Passthrough => "passthrough",
                     Action::Ask => "ask" }
    }
}

#[derive(Clone, Debug)]
pub struct Match { pub kind: String, pub pattern: String }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Join { And, Or }

#[derive(Clone, Debug)]
pub struct Clause {
    pub m: Match,
    pub join: Join,
    pub negate: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct FileRule {
    pub action: Action,
    pub clauses: Vec<Clause>,
}

// ── shared glob/path matching (mirrors Python _glob_match / _path_match) ─────

/// True if `s` matches the extended shell glob `pat` (GLOBSTAR|EXTGLOB|BRACE|
/// DOTGLOB). Empty pattern → false. Mirrors Python `_glob_match`.
pub fn glob_match(pat: &str, s: &str) -> bool {
    let pat = pat.trim();
    if pat.is_empty() { return false; }
    glob::globmatch(pat, s)
}

/// Glob a change's ABSOLUTE path. A bare/relative pattern matches at any depth
/// (`**/` prefix); a leading `/` anchors at the root. Mirrors `_path_match`.
pub fn path_match(pat: &str, rel: &str) -> bool {
    let pat = pat.trim();
    if pat.is_empty() { return false; }
    let s = format!("/{}", rel.trim_start_matches('/')); // absolute path
    let p = if !pat.contains('/') || !pat.starts_with('/') {
        format!("**/{pat}")                              // bare/relative → any depth
    } else {
        pat.to_string()
    };
    glob::globmatch(&p, &s)
}

// ── subject + targets (mirrors Python Subject / PathTarget / ProcFilterTarget) ─

/// The box + triggering PROCESS a match is evaluated against. An empty field
/// never matches its kind.
#[derive(Clone, Default, Debug)]
pub struct Subject {
    pub box_name: String,
    pub exe: String,
    pub cwd: String,
    pub argv: Vec<String>,
}

impl Subject {
    /// match_one for the shared box/process kinds (mirrors `_subject_match`).
    /// box/arg use the raw glob; exe/cwd use path-glob semantics.
    fn subject_match(&self, m: &Match) -> bool {
        match m.kind.as_str() {
            "box" => glob_match(&m.pattern, &self.box_name),
            "exe" => path_match(&m.pattern, &self.exe),
            "cwd" => path_match(&m.pattern, &self.cwd),
            "arg" => self.argv.iter().any(|a| glob_match(&m.pattern, a)),
            _ => false,
        }
    }
}

/// Parse the internal "ids" pattern — comma-separated row ids — into a set.
fn ids_of(pattern: &str) -> std::collections::HashSet<i64> {
    pattern.split(',').filter_map(|t| t.trim().parse::<i64>().ok()).collect()
}

/// A changed path under evaluation (file-domain target, mirrors PathTarget).
pub struct PathTarget<'a> {
    pub rel: &'a str,
    pub subject: Subject,
    pub ids: Vec<i64>,
}

/// A process row under evaluation (procs-pane filter, mirrors ProcFilterTarget).
pub struct ProcFilterTarget {
    pub row_id: i64,
    pub subject: Subject,
}

pub trait Target {
    fn match_one(&self, m: &Match) -> bool;
}

impl<'a> Target for PathTarget<'a> {
    fn match_one(&self, m: &Match) -> bool {
        match m.kind.as_str() {
            "path" => path_match(&m.pattern, self.rel),
            "ids" => {
                let want = ids_of(&m.pattern);
                self.ids.iter().any(|i| want.contains(i))
            }
            _ => self.subject.subject_match(m),
        }
    }
}

impl Target for ProcFilterTarget {
    fn match_one(&self, m: &Match) -> bool {
        match m.kind.as_str() {
            "ids" => ids_of(&m.pattern).contains(&self.row_id),
            _ => self.subject.subject_match(m),
        }
    }
}

pub struct PipelineFilterTarget {
    pub row_id: i64,
    pub cmd: String,
}

impl Target for PipelineFilterTarget {
    fn match_one(&self, m: &Match) -> bool {
        match m.kind.as_str() {
            "ids" => ids_of(&m.pattern).contains(&self.row_id),
            "cmd" => glob_match(&m.pattern, &self.cmd),
            _ => false,
        }
    }
}

pub struct EdgeFilterTarget {
    pub row_id: i64,
    pub targets: Vec<String>,
    pub cmd: String,
}

impl Target for EdgeFilterTarget {
    fn match_one(&self, m: &Match) -> bool {
        match m.kind.as_str() {
            "ids" => ids_of(&m.pattern).contains(&self.row_id),
            "target" => self.targets.iter().any(|t| path_match(&m.pattern, t)),
            "cmd" => glob_match(&m.pattern, &self.cmd),
            _ => false,
        }
    }
}

/// Generic, target-agnostic left-to-right boolean fold (mirrors `eval_clauses`).
/// First enabled clause seeds (its join ignored); each negates then folds with
/// and/or; disabled clauses skip; no enabled clause → false.
pub fn eval_clauses<T: Target>(target: &T, clauses: &[Clause]) -> bool {
    let mut acc: Option<bool> = None;
    for c in clauses {
        if !c.enabled { continue; }
        let mut v = target.match_one(&c.m);
        if c.negate { v = !v; }
        acc = Some(match acc {
            None => v,
            Some(a) => match c.join { Join::Or => a || v, Join::And => a && v },
        });
    }
    acc.unwrap_or(false)
}

// ── FileRule parse / to_line (mirrors Python exactly) ────────────────────────

impl FileRule {
    pub fn matches<T: Target>(&self, target: &T) -> bool {
        eval_clauses(target, &self.clauses)
    }

    #[allow(dead_code)]
    pub fn to_line(&self) -> String {
        let mut out = vec![self.action.as_str().to_string()];
        for (n, c) in self.clauses.iter().enumerate() {
            let mut seg = vec![];
            if n > 0 { seg.push(match c.join { Join::Or => "or", Join::And => "and" }.to_string()); }
            if !c.enabled { seg.push("off".to_string()); }
            if c.negate { seg.push("not".to_string()); }
            if c.m.kind == "path" {
                seg.push(c.m.pattern.clone());          // path renders bare
            } else {
                seg.push(format!("{}:{}", c.m.kind, c.m.pattern));
            }
            out.push(seg.join(" "));
        }
        out.join(" ")
    }

    fn parse_clauses(s: &str) -> Vec<Clause> {
        let toks: Vec<&str> = s.split_whitespace().collect();
        let mut clauses: Vec<Clause> = vec![];
        let mut i = 0;
        let mut join = Join::And;
        while i < toks.len() {
            if !clauses.is_empty() {
                let lc = toks[i].to_ascii_lowercase();
                if lc == "and" || lc == "or" {
                    join = if lc == "or" { Join::Or } else { Join::And };
                    i += 1;
                }
            }
            let (mut off, mut neg) = (false, false);
            while i < toks.len() {
                let lc = toks[i].to_ascii_lowercase();
                if lc == "off" { off = true; i += 1; }
                else if lc == "not" { neg = true; i += 1; }
                else { break; }
            }
            if i >= toks.len() { break; }
            let pred = toks[i];
            i += 1;
            let (kind, pat) = match pred.split_once(':') {
                Some((k, p)) if FILE_KINDS.contains(&k.to_ascii_lowercase().as_str())
                             || NET_KINDS.contains(&k.to_ascii_lowercase().as_str()) =>
                    (k.to_ascii_lowercase(), p.to_string()),
                _ => ("path".to_string(), pred.to_string()), // bare → path kind
            };
            if pat.is_empty() { continue; }
            clauses.push(Clause {
                m: Match { kind, pattern: pat },
                join: if clauses.is_empty() { Join::And } else { join },
                negate: neg,
                enabled: !off,
            });
            join = Join::And;
        }
        clauses
    }

    /// Parse one rule line. None for blanks, comments, or a missing/invalid
    /// action (an explicit action is always required), or no clauses.
    pub fn parse(line: &str) -> Option<FileRule> {
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') { return None; }
        let (verb, rest) = match s.split_once(' ') {
            Some((v, r)) => (v, r),
            None => (s, ""),
        };
        let action = Action::parse(&verb.to_ascii_lowercase())?;
        let clauses = Self::parse_clauses(rest.trim());
        if clauses.is_empty() { return None; }
        Some(FileRule { action, clauses })
    }

    /// True iff every clause is the `path` kind — the D5-safe form a passthrough
    /// rule must have to enable kernel read-passthrough.
    pub fn path_only(&self) -> bool {
        self.clauses.iter().all(|c| c.m.kind == "path")
    }
}

// ── FileRules (mirrors Python FileRules) ─────────────────────────────────────

pub struct Rules { pub rules: Vec<FileRule> }

impl Rules {
    pub fn load() -> Rules {
        let text = std::fs::read_to_string(paths::config_home().join("filerules"))
            .unwrap_or_default();
        Rules::parse(&text)
    }

    pub fn parse(text: &str) -> Rules {
        let rules = text.lines().filter_map(FileRule::parse).collect();
        Rules { rules }
    }

    /// First-match FULL-grammar decision for a path, given the box display name
    /// and the writer's provenance (exe/cwd/argv). Mirrors `FileRules.decide`.
    pub fn decide(&self, rel: &str, subject: &Subject) -> Option<Action> {
        let target = PathTarget { rel, subject: subject.clone(), ids: vec![] };
        self.rules.iter().find(|r| r.matches(&target)).map(|r| r.action)
    }

    /// PATH-ONLY passthrough decision for the D5 kernel-read-passthrough gate.
    /// Only passthrough rules whose clauses are ALL `path` count; a box-/proc-
    /// scoped passthrough therefore never enables read-passthrough divergence.
    /// First matching path-only rule wins (so a higher path-only apply/discard
    /// still shadows a lower passthrough).
    pub fn passthrough_path_only(&self, rel: &str) -> bool {
        let target = PathTarget { rel, subject: Subject::default(), ids: vec![] };
        for r in &self.rules {
            if !r.path_only() { continue; }
            if r.matches(&target) {
                return r.action == Action::Passthrough;
            }
        }
        false
    }

    /// True if any rule tests a process facet (lets a hot caller skip resolving
    /// the writer's provenance when no rule would use it). Mirrors needs_proc.
    pub fn needs_proc(&self) -> bool {
        self.rules.iter().flat_map(|r| &r.clauses)
            .any(|c| matches!(c.m.kind.as_str(), "exe" | "cwd" | "arg"))
    }

    /// True if any rule tests the box facet.
    pub fn needs_box(&self) -> bool {
        self.rules.iter().flat_map(|r| &r.clauses)
            .any(|c| c.m.kind == "box")
    }
}
