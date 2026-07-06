//! Normalized page titles: namespace-resolved, underscores → spaces,
//! first-letter case rule applied per namespace (import plan §7
//! amendment: the titles table stores NORMALIZED keys, so render-time
//! lookups must normalize identically).

use crate::SiteConfig;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Title {
    pub ns: i32,
    /// Normalized page name WITHOUT the namespace prefix.
    pub text: String,
}

impl Title {
    /// Parse "Template:Foo_bar" → Title{ns:10,"Foo bar"} using the
    /// site's namespace map + aliases; unknown prefix → ns 0 with the
    /// colon kept. Leading ':' forces mainspace. IMPLEMENTATION:
    /// preprocessor agent (owns normalization fidelity).
    pub fn parse(raw: &str, site: &SiteConfig) -> Title {
        // Placeholder skeleton: mainspace, underscores → spaces, trim.
        let _ = site;
        Title { ns: 0, text: raw.trim().replace('_', " ") }
    }

    /// Full prefixed form for display and PageStore lookups.
    pub fn prefixed(&self, site: &SiteConfig) -> String {
        match site.namespaces.get(&self.ns) {
            Some(ns) if !ns.canonical.is_empty() => format!("{}:{}", ns.canonical, self.text),
            _ => self.text.clone(),
        }
    }
}
