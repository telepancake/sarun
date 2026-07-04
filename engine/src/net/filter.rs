// Proxy-side web filtering (DESIGN-web.md W7). Ad/tracker blocking and
// response rewriting on the engine MITM proxy — outside the browser, so it
// applies to every box's HTTP(S), not just carbonyl, and cannot be defeated
// by the page. Runs before the capture tee (W2), so the web archive records
// what was actually served (blocks noted, rewrites applied).
//
// The ruleset is a plain file, `{config_home}/webfilter`, one rule per line:
//   block  <glob>          block requests whose URL matches <glob> (204)
//   block-host <glob>      block by host authority (ad/tracker domains)
//   strip-header <name>    remove <name> from every response (e.g. CSP)
// `<glob>` is a case-insensitive `*`-wildcard match. Blank lines and `#`
// comments are ignored. Unknown directives are skipped (forward-compatible).
//
// Deliberately tiny and native; an EasyList (`||domain^` / `##selector`)
// importer that compiles the standard lists into these rules is a follow-on.

use hyper::HeaderMap;

/// One parsed rule. Globs are precompiled to a matcher at load.
enum Rule {
    /// Block a request whose full URL matches.
    BlockUrl(Glob),
    /// Block a request whose host authority matches.
    BlockHost(Glob),
    /// Strip a response header by (lowercased) name.
    StripHeader(String),
}

/// The loaded ruleset. Cheap to share (Arc). An empty set (no file, or all
/// lines skipped) makes every method a no-op — a box that opted into
/// filtering with no rules simply sees pure pass-through.
pub struct Filter {
    rules: Vec<Rule>,
}

/// A request-time decision.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Forward to upstream unchanged.
    Pass,
    /// Block: answer with a synthetic empty response, never dial upstream.
    Block,
}

impl Filter {
    /// Load `{config_home}/webfilter`. Missing file → empty ruleset (no-op).
    pub fn load() -> Self {
        let path = crate::paths::config_home().join("webfilter");
        match std::fs::read_to_string(&path) {
            Ok(s) => Self::parse(&s),
            Err(_) => Self { rules: Vec::new() },
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let (kind, arg) = match line.split_once(char::is_whitespace) {
                Some((k, a)) => (k, a.trim()),
                None => continue, // a bare directive with no argument is inert
            };
            if arg.is_empty() { continue; }
            match kind {
                "block" => rules.push(Rule::BlockUrl(Glob::new(arg))),
                "block-host" => rules.push(Rule::BlockHost(Glob::new(arg))),
                "strip-header" => rules.push(Rule::StripHeader(arg.to_ascii_lowercase())),
                _ => {} // unknown directive: skip, stay forward-compatible
            }
        }
        Self { rules }
    }

    pub fn is_empty(&self) -> bool { self.rules.is_empty() }

    /// Request-time verdict for a URL + host. Block wins on the first match.
    pub fn decide(&self, url: &str, host: &str) -> Decision {
        for r in &self.rules {
            match r {
                Rule::BlockUrl(g) if g.matches(url) => return Decision::Block,
                Rule::BlockHost(g) if g.matches(host) => return Decision::Block,
                _ => {}
            }
        }
        Decision::Pass
    }

    /// Apply response-header rewrites in place (strip-header rules). Returns
    /// the count removed (0 → nothing changed).
    pub fn rewrite_response_headers(&self, headers: &mut HeaderMap) -> usize {
        let mut removed = 0;
        for r in &self.rules {
            if let Rule::StripHeader(name) = r {
                // HeaderMap::remove takes a header name; remove all values.
                if let Ok(hn) = hyper::header::HeaderName::from_bytes(name.as_bytes()) {
                    while headers.remove(&hn).is_some() { removed += 1; }
                }
            }
        }
        removed
    }
}

/// A minimal case-insensitive glob: `*` matches any run (incl. empty), every
/// other char is literal. Compiled to lowercased literal segments split on
/// `*`; matching walks the segments left to right. No regex, no backtracking
/// blowup — segments are matched greedily in order, which is exact for the
/// `*`-only glob language.
struct Glob {
    /// Lowercased literal segments between `*`s.
    segs: Vec<String>,
    /// Whether the pattern anchored at the start (didn't begin with `*`).
    anchor_start: bool,
    /// Whether the pattern anchored at the end (didn't end with `*`).
    anchor_end: bool,
}

impl Glob {
    fn new(pat: &str) -> Self {
        let low = pat.to_ascii_lowercase();
        let anchor_start = !low.starts_with('*');
        let anchor_end = !low.ends_with('*');
        let segs: Vec<String> = low.split('*')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        Self { segs, anchor_start, anchor_end }
    }

    fn matches(&self, hay: &str) -> bool {
        let hay = hay.to_ascii_lowercase();
        // A pattern of pure `*` (no segments) matches anything.
        if self.segs.is_empty() { return true; }
        let mut pos = 0usize;
        for (i, seg) in self.segs.iter().enumerate() {
            let from = &hay[pos..];
            match from.find(seg.as_str()) {
                None => return false,
                Some(idx) => {
                    // First segment must sit at the very start when anchored.
                    if i == 0 && self.anchor_start && idx != 0 { return false; }
                    pos += idx + seg.len();
                }
            }
        }
        // Last segment must reach the end when anchored.
        if self.anchor_end {
            if let Some(last) = self.segs.last() {
                return hay.ends_with(last.as_str());
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_anchors_and_wildcards() {
        assert!(Glob::new("*doubleclick.net*").matches("https://ad.doubleclick.net/x"));
        assert!(Glob::new("*.ads.*").matches("http://x.ads.example.com/"));
        assert!(!Glob::new("*doubleclick.net*").matches("https://example.com/"));
        // Anchored start/end.
        assert!(Glob::new("https://track*").matches("https://tracker.io/"));
        assert!(!Glob::new("https://track*").matches("http://tracker.io/"));
        assert!(Glob::new("*/pixel.gif").matches("https://x.com/pixel.gif"));
        assert!(!Glob::new("*/pixel.gif").matches("https://x.com/pixel.gif?a=1"));
        // Pure wildcard matches all.
        assert!(Glob::new("*").matches("anything"));
    }

    #[test]
    fn decide_blocks_by_url_and_host() {
        let f = Filter::parse(
            "# comment\nblock *doubleclick.net*\nblock-host *.tracker.io\n");
        assert_eq!(f.decide("https://ad.doubleclick.net/pixel", "ad.doubleclick.net"),
                   Decision::Block);
        assert_eq!(f.decide("https://x.com/", "evil.tracker.io"), Decision::Block);
        assert_eq!(f.decide("https://example.com/", "example.com"), Decision::Pass);
    }

    #[test]
    fn strip_header_removes_all_matches() {
        let f = Filter::parse("strip-header Content-Security-Policy\n");
        let mut h = HeaderMap::new();
        h.append(hyper::header::CONTENT_SECURITY_POLICY,
                 "default-src 'self'".parse().unwrap());
        h.insert(hyper::header::CONTENT_TYPE, "text/html".parse().unwrap());
        assert_eq!(f.rewrite_response_headers(&mut h), 1);
        assert!(!h.contains_key(hyper::header::CONTENT_SECURITY_POLICY));
        assert!(h.contains_key(hyper::header::CONTENT_TYPE));
    }

    #[test]
    fn empty_ruleset_is_noop() {
        let f = Filter::parse("\n#only comments\n");
        assert!(f.is_empty());
        assert_eq!(f.decide("https://x", "x"), Decision::Pass);
    }
}
