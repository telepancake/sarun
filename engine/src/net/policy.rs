// Bridge to the unified rules engine. A network rule lives in the SAME on-
// disk rules file as file rules, with `kind` extended to include host /
// port / scheme / sni / http_path / http_method / http_status / proto /
// box / exe / cwd / arg. The matcher engine in rules.rs is reused
// verbatim — only the field resolver differs by context.
//
// At connection time the dispatcher calls `decide` with the per-conn
// subject; on Deny we close, on Allow we proceed, on Inspect we proceed
// AND make sure MITM is on (for TLS that means we terminate; for HTTP
// it's the same path so the bit is informational).

use crate::rules::Action;

#[derive(Clone, Debug, Default)]
pub struct NetSubject {
    pub host: String,
    pub port: u16,
    pub scheme: String,         // "http" | "https" | "tcp" | "udp"
    pub sni: String,            // empty if not TLS
    pub http_path: String,      // empty for non-HTTP gates
    pub http_method: String,
    pub http_status: String,
    pub proto: String,          // "tcp" | "udp"
    pub box_name: String,
    pub exe: String,
    pub cwd: String,
    pub arg: String,
}

pub const NET_KINDS: &[&str] = &[
    "host", "port", "scheme", "sni",
    "http_path", "http_method", "http_status",
    "proto", "box", "exe", "cwd", "arg",
];

/// Default-deny: an empty rule set means "deny everything". A first-match-
/// wins walk of `rules` produces Allow / Deny; Passthrough on a net rule is
/// re-mapped to Allow (the file-rule Passthrough action is not meaningful
/// here, but we accept the keyword for one-file unification).
pub fn decide(rules: &[crate::rules::FileRule], subj: &NetSubject) -> Action {
    for r in rules {
        if matches(r, subj) {
            return match r.action {
                Action::Apply => Action::Apply,    // = Allow
                Action::Discard => Action::Discard, // = Deny
                Action::Passthrough => Action::Apply,
            };
        }
    }
    Action::Discard
}

fn matches(rule: &crate::rules::FileRule, subj: &NetSubject) -> bool {
    use crate::rules::{Join, glob_match};
    let port_str = subj.port.to_string();
    let mut acc = true;
    let mut started = false;
    for c in &rule.clauses {
        if !c.enabled { continue; }
        let v = field(&c.m.kind, subj, &port_str);
        let mut m = glob_match(&c.m.pattern, v);
        if c.negate { m = !m; }
        if !started { acc = m; started = true; }
        else { acc = match c.join { Join::And => acc && m, Join::Or => acc || m }; }
    }
    started && acc
}

fn field<'a>(kind: &str, s: &'a NetSubject, port_str: &'a str) -> &'a str {
    match kind {
        "host" => &s.host,
        "port" => port_str,
        "scheme" => &s.scheme,
        "sni" => &s.sni,
        "http_path" => &s.http_path,
        "http_method" => &s.http_method,
        "http_status" => &s.http_status,
        "proto" => &s.proto,
        "box" => &s.box_name,
        "exe" => &s.exe,
        "cwd" => &s.cwd,
        "arg" => &s.arg,
        _ => "",
    }
}
