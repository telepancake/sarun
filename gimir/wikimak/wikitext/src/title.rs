//! Normalized page titles: namespace-resolved, underscores → spaces,
//! first-letter case rule applied per namespace (import plan §7
//! amendment: the titles table stores NORMALIZED keys, so render-time
//! lookups must normalize identically).
//!
//! The `Title` struct fields (`ns`, `text`) are a shared contract — a
//! fragment is NOT a field (two links to the same page differing only by
//! `#section` must compare equal for PageStore lookups). Fragments are
//! returned alongside via [`Title::parse_parts`].

use crate::{NamespaceInfo, SiteConfig};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Title {
    pub ns: i32,
    /// Normalized page name WITHOUT the namespace prefix.
    pub text: String,
}

/// Namespace ids the parser dispatches on (MediaWiki canonical numbers).
pub const NS_MAIN: i32 = 0;
pub const NS_FILE: i32 = 6;
pub const NS_CATEGORY: i32 = 14;

impl Title {
    /// Parse "Template:Foo_bar" → `Title{ns:10,"Foo bar"}` using the
    /// site's namespace map + aliases; unknown prefix → ns 0 with the
    /// colon kept as part of the mainspace title. A leading ':' is
    /// consumed (it is a link-suppression marker, not part of the name).
    /// Any `#fragment` is dropped here — use [`Title::parse_parts`] to
    /// keep it.
    pub fn parse(raw: &str, site: &SiteConfig) -> Title {
        Self::parse_parts(raw, site).0
    }

    /// Like [`Title::parse`] but also returns the `#fragment` (spaces
    /// normalized, `None` if absent) and whether the raw target carried a
    /// leading colon (the caller uses that to force Category/File/interwiki
    /// targets to render as ordinary links).
    pub fn parse_parts(raw: &str, site: &SiteConfig) -> (Title, Option<String>) {
        let (t, frag, _colon) = Self::parse_full(raw, site);
        (t, frag)
    }

    /// Full parse exposing the leading-colon flag as well.
    pub fn parse_full(raw: &str, site: &SiteConfig) -> (Title, Option<String>, bool) {
        let mut s = raw.trim();
        let leading_colon = s.starts_with(':');
        if leading_colon {
            s = s[1..].trim_start();
        }
        // Split off the fragment on the first '#'.
        let (base, frag) = match s.find('#') {
            Some(i) => {
                let f = collapse_ws(&s[i + 1..].replace('_', " "));
                let f = if f.is_empty() { None } else { Some(f) };
                (&s[..i], f)
            }
            None => (s, None),
        };
        let base = base.trim();
        if let Some(idx) = base.find(':') {
            let prefix = &base[..idx];
            let rest = &base[idx + 1..];
            if let Some(ns) = resolve_ns(prefix, site) {
                let text = normalize_title(rest, ns.case_first_letter);
                return (Title { ns: ns.id, text }, frag, leading_colon);
            }
        }
        let cf = site
            .namespaces
            .get(&NS_MAIN)
            .map(|n| n.case_first_letter)
            .unwrap_or(true);
        (
            Title {
                ns: NS_MAIN,
                text: normalize_title(base, cf),
            },
            frag,
            leading_colon,
        )
    }

    /// Full prefixed form for display and PageStore lookups.
    pub fn prefixed(&self, site: &SiteConfig) -> String {
        match site.namespaces.get(&self.ns) {
            Some(ns) if !ns.canonical.is_empty() => format!("{}:{}", ns.canonical, self.text),
            _ => self.text.clone(),
        }
    }
}

/// Resolve a namespace prefix (case-insensitive, underscores/whitespace
/// normalized) against the canonical name and every alias. `None` = not a
/// namespace (mainspace or interwiki).
pub fn resolve_ns<'a>(prefix: &str, site: &'a SiteConfig) -> Option<&'a NamespaceInfo> {
    let want = collapse_ws(&prefix.replace('_', " ")).to_lowercase();
    if want.is_empty() {
        return None;
    }
    for ns in site.namespaces.values() {
        if !ns.canonical.is_empty() && ns.canonical.to_lowercase() == want {
            return Some(ns);
        }
        for alias in &ns.aliases {
            if alias.to_lowercase() == want {
                return Some(ns);
            }
        }
    }
    None
}

/// Underscores → spaces, whitespace collapsed and trimmed, then the
/// first-letter case rule applied when the namespace demands it.
fn normalize_title(raw: &str, case_first_letter: bool) -> String {
    let spaced = raw.replace('_', " ");
    let collapsed = collapse_ws(&spaced);
    if case_first_letter {
        uppercase_first(&collapsed)
    } else {
        collapsed
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn uppercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
