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

/// Default-ALLOW for net rules — an empty rule set means the proxy works
/// out of the box. To block, write a `discard host:bad.com` line in the
/// same filerules file the file-rule UI edits. First-match wins; rules
/// referencing only file kinds (path/box/exe/...) are inert here (the
/// field resolver returns "" for the unknown kind, the glob can't match,
/// the rule slides off). Passthrough on a net rule is re-mapped to Allow
/// (kept for one-file syntactic unification).
pub fn decide(rules: &[crate::rules::FileRule], subj: &NetSubject) -> Action {
    for r in rules {
        if matches(r, subj) {
            return match r.action {
                Action::Apply => Action::Apply,    // = Allow
                Action::Discard => Action::Discard, // = Deny
                Action::Passthrough => Action::Apply,
                Action::Ask => Action::Ask,         // banner-prompt the user
            };
        }
    }
    Action::Apply  // default Allow
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::FileRule;

    fn subj_https(host: &str) -> NetSubject {
        NetSubject {
            host: host.to_string(),
            port: 443,
            scheme: "https".into(),
            proto: "tcp".into(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_rules_default_allow() {
        assert_eq!(decide(&[], &subj_https("example.com")), Action::Apply);
    }

    #[test]
    fn discard_by_host_blocks_match_allows_others() {
        let rules = vec![
            FileRule::parse("discard host:tracker.example").unwrap(),
        ];
        assert_eq!(decide(&rules, &subj_https("tracker.example")),
                   Action::Discard);
        assert_eq!(decide(&rules, &subj_https("example.com")),
                   Action::Apply);
    }

    #[test]
    fn first_match_wins() {
        // explicit allow before a broad discard
        let rules = vec![
            FileRule::parse("apply host:safe.example").unwrap(),
            FileRule::parse("discard host:*").unwrap(),
        ];
        assert_eq!(decide(&rules, &subj_https("safe.example")),
                   Action::Apply);
        assert_eq!(decide(&rules, &subj_https("other.example")),
                   Action::Discard);
    }

    #[test]
    fn file_kinds_are_inert_for_net_subjects() {
        // A path-only rule shouldn't accidentally deny a net dial.
        let rules = vec![FileRule::parse("discard **/*.log").unwrap()];
        assert_eq!(decide(&rules, &subj_https("example.com")),
                   Action::Apply);
    }
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
