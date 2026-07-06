//! Shared test fixtures: a small siteinfo + mock PageStore/MediaResolver
//! and a `render`/`render_full` helper. These pin REAL parser behavior —
//! the stores return concrete facts (existence sets, media URLs) so every
//! assertion exercises the parser end to end, not a stub.

#![allow(dead_code)]

use std::collections::BTreeMap;
use wikimak_wikitext::{
    render, InterwikiEntry, MediaResolver, NamespaceInfo, PageStore, RenderOptions, RenderOutput,
    SiteConfig, Title,
};

fn ns(id: i32, canonical: &str, aliases: &[&str]) -> NamespaceInfo {
    NamespaceInfo {
        id,
        canonical: canonical.to_string(),
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        case_first_letter: true,
    }
}

pub fn site() -> SiteConfig {
    let mut namespaces = BTreeMap::new();
    for n in [
        ns(0, "", &[]),
        ns(4, "Wikipedia", &["Project"]),
        ns(6, "File", &["Image"]),
        ns(10, "Template", &[]),
        ns(12, "Help", &[]),
        ns(14, "Category", &[]),
    ] {
        namespaces.insert(n.id, n);
    }
    let mut interwiki = BTreeMap::new();
    interwiki.insert(
        "fr".to_string(),
        InterwikiEntry {
            prefix: "fr".to_string(),
            url: "https://fr.wikipedia.org/wiki/$1".to_string(),
            local_instance: None,
        },
    );
    interwiki.insert(
        "wikt".to_string(),
        InterwikiEntry {
            prefix: "wikt".to_string(),
            url: "https://en.wiktionary.org/wiki/$1".to_string(),
            local_instance: None,
        },
    );
    interwiki.insert(
        "meta".to_string(),
        InterwikiEntry {
            prefix: "meta".to_string(),
            url: "https://meta.example/wiki/$1".to_string(),
            local_instance: Some("metawiki".to_string()),
        },
    );
    SiteConfig {
        site_name: "Test Wiki".to_string(),
        db_name: "testwiki".to_string(),
        lang: "en".to_string(),
        rtl: false,
        namespaces,
        interwiki,
        ..Default::default()
    }
}

pub struct Store {
    pub site: SiteConfig,
    /// Prefixed titles that exist (blue links); everything else is red.
    pub existing: Vec<String>,
}

impl Store {
    pub fn new() -> Self {
        Store {
            site: site(),
            existing: vec![
                "Berlin".to_string(),
                "Cat".to_string(),
                "Dog".to_string(),
                "Help:Contents".to_string(),
                "Foo bar".to_string(),
                "Main Page".to_string(),
            ],
        }
    }
    pub fn rtl() -> Self {
        let mut s = Self::new();
        s.site.rtl = true;
        s.site.lang = "ar".to_string();
        s
    }
}

impl PageStore for Store {
    fn page_text(&self, _title: &Title) -> Option<String> {
        None
    }
    fn page_exists(&self, title: &Title) -> bool {
        self.existing.contains(&title.prefixed(&self.site))
    }
    fn site(&self) -> &SiteConfig {
        &self.site
    }
    fn timestamp_micros(&self) -> i64 {
        0
    }
}

/// Deterministic media: any file resolves to a stable URL except names
/// beginning with "Missing", which return None (→ placeholder + miss).
pub struct Media;

impl MediaResolver for Media {
    fn image_url(&self, file: &Title, width_px: Option<u32>) -> Option<String> {
        if file.text.starts_with("Missing") {
            return None;
        }
        let w = width_px.unwrap_or(0);
        Some(format!("https://media.example/{}?w={}", file.text, w))
    }
}

pub const WRAP_OPEN: &str = "<div class=\"mw-parser-output\">";
pub const WRAP_CLOSE: &str = "</div>";

/// Render with the default LTR site and strip the content wrapper so
/// assertions target the body HTML directly.
pub fn render_inner(input: &str) -> String {
    let full = render_full(input);
    full.strip_prefix(WRAP_OPEN)
        .and_then(|s| s.strip_suffix(WRAP_CLOSE))
        .expect("wrapper present")
        .to_string()
}

pub fn render_full(input: &str) -> String {
    let store = Store::new();
    let media = Media;
    let opts = RenderOptions {
        media: Some(&media),
        link_prefix: "/wiki/".to_string(),
        asof_query: String::new(),
        ..Default::default()
    };
    let title = Title {
        ns: 0,
        text: "Test".to_string(),
    };
    render(&store, &title, input, &opts).html
}

/// Full RenderOutput (categories, misses) with default options.
pub fn render_out(input: &str) -> RenderOutput {
    let store = Store::new();
    let media = Media;
    let opts = RenderOptions {
        media: Some(&media),
        link_prefix: "/wiki/".to_string(),
        asof_query: String::new(),
        ..Default::default()
    };
    let title = Title {
        ns: 0,
        text: "Test".to_string(),
    };
    render(&store, &title, input, &opts)
}

/// Render with a caller-provided asof query and RTL toggle.
pub fn render_inner_opts(input: &str, asof: &str, rtl: bool) -> String {
    let store = if rtl { Store::rtl() } else { Store::new() };
    let media = Media;
    let opts = RenderOptions {
        media: Some(&media),
        link_prefix: "/wiki/".to_string(),
        asof_query: asof.to_string(),
        ..Default::default()
    };
    let title = Title {
        ns: 0,
        text: "Test".to_string(),
    };
    render(&store, &title, input, &opts).html
}
