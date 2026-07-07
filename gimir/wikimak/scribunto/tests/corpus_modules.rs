//! Real-module regression: load the FULL closure of the captured Wikipedia
//! bundles (the same fixtures the wikitext `corpus` diagnostic uses) into a
//! store and drive real modules — `Citation/CS1`, `Footnotes`, `Navbox`,
//! `Lang`, `NoteTA`, … — through the actual `LuaInvoker`. These are the
//! modules the corpus report tops its "failing #invoke" list with; the tests
//! below pin that they now RUN and produce output (or fail only for reasons
//! genuinely out of scope, e.g. a data module absent from the closure).
//!
//! This complements `invoke.rs` (small synthetic snippets pinning each mw.*
//! primitive) by exercising the mw.* surface as real modules actually stress
//! it — the localized `require` prefixes, the ustring NUL-class patterns, the
//! `frame:extensionTag`/`mw.title:fullUrl`/`mw.uri.new` calls that only show
//! up under a real citation.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use serde_json::Value as J;
use wikimak_scribunto::LuaInvoker;
use wikimak_wikitext::{
    render, NamespaceInfo, PageStore, RenderOptions, SiteConfig, Title,
};

/// Full store built from a bundle: all namespaces from siteinfo, the closure
/// indexed by resolved (ns, name) exactly as the wikitext corpus test does.
struct FullStore {
    site: SiteConfig,
    by_nsname: HashMap<(i32, String), String>,
    ts_micros: i64,
}

impl PageStore for FullStore {
    fn page_text(&self, title: &Title) -> Option<String> {
        self.by_nsname.get(&(title.ns, title.text.clone())).cloned()
    }
    fn page_exists(&self, title: &Title) -> bool {
        self.by_nsname.contains_key(&(title.ns, title.text.clone()))
    }
    fn site(&self) -> &SiteConfig {
        &self.site
    }
    fn timestamp_micros(&self) -> i64 {
        self.ts_micros
    }
}

fn full_store(name: &str) -> (FullStore, String, String) {
    let path = format!(
        "{}/../wikitext/tests/corpus/{name}.json.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let f = std::fs::File::open(&path).expect("open bundle");
    let mut gz = flate2::read::GzDecoder::new(f);
    let mut buf = String::new();
    gz.read_to_string(&mut buf).expect("gunzip");
    let b: J = serde_json::from_str(&buf).expect("json");

    let mut namespaces = BTreeMap::new();
    let mut extra: HashMap<i32, Vec<String>> = HashMap::new();
    if let Some(aliases) = b["siteinfo"]["namespacealiases"].as_array() {
        for a in aliases {
            if let (Some(id), Some(al)) = (a["id"].as_i64(), a["alias"].as_str()) {
                extra.entry(id as i32).or_default().push(al.to_string());
            }
        }
    }
    if let Some(nss) = b["siteinfo"]["namespaces"].as_object() {
        for row in nss.values() {
            let id = row["id"].as_i64().unwrap_or(0) as i32;
            let canonical = row["canonical"].as_str().unwrap_or("").to_string();
            let name_ = row["name"].as_str().unwrap_or("").to_string();
            let mut aliases = Vec::new();
            if !name_.is_empty() && name_ != canonical {
                aliases.push(name_.clone());
            }
            if let Some(ex) = extra.get(&id) {
                aliases.extend(ex.iter().cloned());
            }
            namespaces.insert(
                id,
                NamespaceInfo {
                    id,
                    canonical: if canonical.is_empty() { name_.clone() } else { canonical },
                    aliases,
                    case_first_letter: row["case"].as_str() != Some("case-sensitive"),
                },
            );
        }
    }
    let site = SiteConfig {
        site_name: "corpus".into(),
        db_name: format!("{}wiki", b["meta"]["lang"].as_str().unwrap_or("en")),
        lang: b["meta"]["content_lang"].as_str().unwrap_or("en").into(),
        rtl: b["meta"]["rtl"].as_bool().unwrap_or(false),
        server: "https://en.wikipedia.org".into(),
        script_path: "/w".into(),
        namespaces,
        interwiki: Default::default(),
        ..Default::default()
    };
    let mut by_nsname = HashMap::new();
    if let Some(c) = b["closure"].as_object() {
        for (k, v) in c {
            let t = Title::parse(k, &site);
            by_nsname.insert((t.ns, t.text), v.as_str().unwrap_or("").to_string());
        }
    }
    let store = FullStore { site, by_nsname, ts_micros: 1_600_000_000_000_000 };
    let resolved = b["meta"]["resolved_title"].as_str().unwrap_or("").to_string();
    let wikitext = b["page_wikitext"].as_str().unwrap_or("").to_string();
    (store, resolved, wikitext)
}

fn render_page(name: &str) -> (wikimak_wikitext::RenderOutput, FullStore) {
    let (store, resolved, wikitext) = full_store(name);
    let invoker = LuaInvoker::default();
    let opts = RenderOptions {
        invoker: Some(&invoker),
        media: None,
        link_prefix: "/wiki/".into(),
        asof_query: String::new(),
    };
    let title = Title::parse(&resolved, &store.site);
    let out = render(&store, &title, &wikitext, &opts);
    (out, store)
}

/// Count invoke failures NOT attributable to Cite (which lives in the wikitext
/// crate, not here) — the number this crate's mw.* surface is responsible for.
fn module_failures(out: &wikimak_wikitext::RenderOutput) -> usize {
    out.misses
        .failed_invokes
        .iter()
        .filter(|f| !f.to_lowercase().starts_with("cite"))
        .count()
}

/// Every page with module source in its closure must render with its real
/// modules RUNNING. The count of module-attributable failed invokes across the
/// corpus is the regression tripwire: it started at 276 before this crate grew
/// the mw.* surface these modules need (strict/libraryUtil, localized require,
/// mw.title/uri/language/wikibase, ustring NUL patterns, …). It must not climb
/// back. NOT a quality score — a floor that lowers as more modules run.
const MAX_MODULE_FAILURES: usize = 60;

#[test]
fn real_modules_run_across_corpus() {
    let pages = [
        "ar-575311", "de-4717", "el-22057", "en-47660", "fa-395913", "he-19949",
        "hi-8417", "ja-53661", "ko-436065", "ru-82617", "uk-5663", "zh-24298",
    ];
    let mut total = 0usize;
    for page in pages {
        let (out, _store) = render_page(page);
        let n = module_failures(&out);
        total += n;
        // No page may render zero blocks when it has module-driven content.
        assert!(
            !out.html.is_empty(),
            "{page}: produced empty output"
        );
    }
    assert!(
        total <= MAX_MODULE_FAILURES,
        "module-attributable invoke failures rose to {total} (cap {MAX_MODULE_FAILURES}) — \
         a real module regressed; diagnose with the per-page breakdown, don't raise the cap"
    );
}

#[test]
fn en_citation_cs1_renders() {
    // The single most-invoked failing module in the baseline report (125×).
    // It must now produce a real <cite> citation, not a script-error box.
    let (out, _) = render_page("en-47660");
    assert!(
        out.html.contains("class=\"citation") || out.html.contains("<cite"),
        "en page produced no CS1 <cite> output"
    );
    // CS1 must not appear in the failed-invoke list.
    assert!(
        !out.misses
            .failed_invokes
            .iter()
            .any(|f| f.contains("Citation/CS1") || f.contains("citation/CS1")),
        "Citation/CS1 still failing: {:?}",
        out.misses.failed_invokes
    );
}

#[test]
fn uk_localized_require_resolves() {
    // ukwiki modules require each other by the LOCALIZED prefix (Модуль:).
    // Before namespace-aware module resolution + the string ustring aliases,
    // uk's CS1/Ref-lang closure failed ~100×; it must be clean now.
    let (out, _) = render_page("uk-5663");
    let n = module_failures(&out);
    assert!(n <= 1, "uk module failures = {n}: {:?}", out.misses.failed_invokes);
}
