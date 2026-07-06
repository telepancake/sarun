//! Shared fixtures for the preprocessor integration tests: an in-memory
//! PageStore, a standard enwiki-shaped SiteConfig, and expansion helpers.
//! Titles are stored under the SAME normalization the preprocessor applies
//! (underscores→spaces, first-letter upper), so lookups are exact.

#![allow(dead_code)]

use std::collections::BTreeMap;
use wikimak_wikitext::preprocess::{expand, Expanded};
use wikimak_wikitext::{
    Frame, ModuleInvoker, NamespaceInfo, PageStore, RenderOptions, SiteConfig, Title,
};

pub struct MockStore {
    pub pages: BTreeMap<(i32, String), String>,
    pub site: SiteConfig,
    pub ts: i64,
}

impl MockStore {
    pub fn new() -> Self {
        MockStore {
            pages: BTreeMap::new(),
            site: standard_site(),
            ts: 1_104_537_600_000_000, // 2005-01-01T00:00:00Z in micros
        }
    }

    pub fn at(ts_micros: i64) -> Self {
        let mut s = Self::new();
        s.ts = ts_micros;
        s
    }

    /// Add a page by namespace id and already-normalized page name.
    pub fn add(&mut self, ns: i32, name: &str, body: &str) -> &mut Self {
        self.pages.insert((ns, name.to_string()), body.to_string());
        self
    }

    pub fn template(&mut self, name: &str, body: &str) -> &mut Self {
        self.add(10, name, body)
    }
}

impl PageStore for MockStore {
    fn page_text(&self, title: &Title) -> Option<String> {
        self.pages.get(&(title.ns, title.text.clone())).cloned()
    }
    fn page_exists(&self, title: &Title) -> bool {
        self.pages.contains_key(&(title.ns, title.text.clone()))
    }
    fn site(&self) -> &SiteConfig {
        &self.site
    }
    fn timestamp_micros(&self) -> i64 {
        self.ts
    }
}

/// `aliases[0]` is the localized DISPLAY name (per the NamespaceInfo
/// contract: "Localized name + aliases"); later entries are extra aliases.
fn ns(id: i32, canonical: &str, aliases: &[&str]) -> (i32, NamespaceInfo) {
    (
        id,
        NamespaceInfo {
            id,
            canonical: canonical.to_string(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            case_first_letter: true,
        },
    )
}

pub fn standard_site() -> SiteConfig {
    let mut namespaces = BTreeMap::new();
    for (id, info) in [
        ns(0, "", &[]),
        ns(1, "Talk", &[]),
        ns(2, "User", &[]),
        ns(3, "User talk", &[]),
        ns(4, "Project", &["Wikipedia"]),
        ns(5, "Project talk", &["Wikipedia talk"]),
        ns(6, "File", &["File", "Image"]),
        ns(7, "File talk", &[]),
        ns(8, "MediaWiki", &[]),
        ns(9, "MediaWiki talk", &[]),
        ns(10, "Template", &[]),
        ns(11, "Template talk", &[]),
        ns(14, "Category", &[]),
        ns(15, "Category talk", &[]),
        ns(828, "Module", &[]),
    ] {
        namespaces.insert(id, info);
    }
    SiteConfig {
        site_name: "Wikipedia".to_string(),
        db_name: "enwiki".to_string(),
        lang: "en".to_string(),
        rtl: false,
        namespaces,
        interwiki: BTreeMap::new(),
        ..Default::default()
    }
}

pub fn title(ns: i32, text: &str) -> Title {
    Title { ns, text: text.to_string() }
}

pub fn opts() -> RenderOptions<'static> {
    RenderOptions::default()
}

/// Expand mainspace page `Test` with the default (no-invoker) options.
pub fn ex(store: &MockStore, text: &str) -> Expanded {
    expand(store, &title(0, "Test"), text, &opts())
}

/// Expand on a given rendering title (for PAGENAME &c).
pub fn ex_on(store: &MockStore, t: &Title, text: &str) -> Expanded {
    expand(store, t, text, &opts())
}

/// Just the expanded text.
pub fn xt(store: &MockStore, text: &str) -> String {
    ex(store, text).text
}

/// A ModuleInvoker whose behavior is a closure over (module, function).
pub struct FnInvoker<F: Fn(&str, &str, &Frame) -> Result<String, String>>(pub F);

impl<F: Fn(&str, &str, &Frame) -> Result<String, String>> ModuleInvoker for FnInvoker<F> {
    fn invoke(
        &self,
        module: &str,
        function: &str,
        frame: &Frame,
        _store: &dyn PageStore,
    ) -> Result<String, String> {
        (self.0)(module, function, frame)
    }
}
