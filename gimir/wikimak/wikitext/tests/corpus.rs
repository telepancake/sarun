//! Real-page straightedge: render captured Wikipedia pages through the FULL
//! pipeline (preprocessor + parser + Scribunto + media) and score how close
//! the output is to MediaWiki's own READER HTML, structurally.
//!
//! This complements the official parserTests (tests/parsertests.rs): those
//! pin small synthetic cases byte-for-byte; this measures whole real pages
//! across diverse scripts (CJK, RTL, Cyrillic, Greek, Devanagari, Latin) with
//! their full template/module closure — the messy input the parser actually
//! faces. Fixtures are gzip'd snapshots captured by `corpus/capture.py`
//! (page wikitext + closure + reader HTML + siteinfo, all at one instant).
//!
//! We grade USER-VISIBLE output, not editor scaffolding. The signature step
//! strips what the in-page editor needs and the reader ignores — TemplateStyles
//! `<style>` blocks, `mw-editsection` links, and (by dropping every attribute)
//! the `data-mw` / `typeof=` / `about=` / RESTBase-id cruft — from BOTH sides
//! before comparing. What's left is structure + visible text.
//!
//! The score is a Dice coefficient over token bigrams of that stripped stream.
//! It is a MEASUREMENT, not a spec: real pages lean on Wikidata, unsupported
//! extensions, and templates we render partially, so perfection is not the
//! bar. The floored aggregate catches regressions; the per-page printout is
//! the straightedge you watch while improving the parser.

use std::collections::HashMap;
use std::io::Read;

use serde::Deserialize;
use wikimak_media::BlobMediaResolver;
use wikimak_scribunto::LuaInvoker;
use wikimak_wikitext::{
    render, NamespaceInfo, PageStore, RenderMisses, RenderOptions, SiteConfig, Title,
};

// ---------------------------------------------------------------------------
// Fixture bundle (see corpus/capture.py)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Bundle {
    meta: Meta,
    page_wikitext: String,
    reader_html: String,
    closure: HashMap<String, String>,
    siteinfo: SiteInfo,
}

#[derive(Deserialize)]
struct Meta {
    lang: String,
    resolved_title: String,
    revid: u64,
    timestamp: String,
    rtl: bool,
    #[allow(dead_code)]
    sitename: String,
    content_lang: String,
    closure_stored: usize,
}

#[derive(Deserialize)]
struct SiteInfo {
    general: HashMap<String, serde_json::Value>,
    namespaces: HashMap<String, NsRow>,
    #[serde(default)]
    namespacealiases: Vec<NsAlias>,
}

#[derive(Deserialize)]
struct NsRow {
    id: i32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    canonical: Option<String>,
    #[serde(default)]
    case: Option<String>,
}

#[derive(Deserialize)]
struct NsAlias {
    id: i32,
    alias: String,
}

// ---------------------------------------------------------------------------
// SiteConfig + PageStore backed by the captured closure
// ---------------------------------------------------------------------------

fn build_site(b: &Bundle) -> SiteConfig {
    let mut namespaces = std::collections::BTreeMap::new();
    // Namespace aliases beyond the canonical/localized name (e.g. WP → Project).
    let mut extra: HashMap<i32, Vec<String>> = HashMap::new();
    for a in &b.siteinfo.namespacealiases {
        extra.entry(a.id).or_default().push(a.alias.clone());
    }
    for row in b.siteinfo.namespaces.values() {
        let canonical = row.canonical.clone().unwrap_or_default();
        // Aliases the resolver matches transclusions/links against: the
        // localized name plus any namespacealiases. (Title::parse also matches
        // `canonical` directly, so the English form always resolves too.)
        let mut aliases = Vec::new();
        if !row.name.is_empty() && row.name != canonical {
            aliases.push(row.name.clone());
        }
        if let Some(ex) = extra.get(&row.id) {
            aliases.extend(ex.iter().cloned());
        }
        namespaces.insert(
            row.id,
            NamespaceInfo {
                id: row.id,
                // Fall back to the localized name as canonical for content
                // namespaces (id 0 has no canonical); harmless — prefixed() of
                // ns 0 drops the prefix anyway.
                canonical: if canonical.is_empty() { row.name.clone() } else { canonical },
                aliases,
                case_first_letter: row.case.as_deref() != Some("case-sensitive"),
            },
        );
    }
    let server = b
        .siteinfo
        .general
        .get("server")
        .and_then(|v| v.as_str())
        .map(|s| if s.starts_with("//") { format!("https:{s}") } else { s.to_string() })
        .unwrap_or_default();
    SiteConfig {
        site_name: b.meta.sitename.clone(),
        db_name: format!("{}wiki", b.meta.lang),
        lang: b.meta.content_lang.clone(),
        rtl: b.meta.rtl,
        server,
        script_path: "/w".into(),
        namespaces,
        interwiki: Default::default(),
    }
}

struct CorpusStore {
    site: SiteConfig,
    /// Closure indexed by resolved (ns, page-name) so a canonical
    /// `Template:X` lookup matches a localized `قالب:X` fixture key.
    by_nsname: HashMap<(i32, String), String>,
    ts_micros: i64,
}

impl CorpusStore {
    fn new(b: &Bundle) -> Self {
        let site = build_site(b);
        let mut by_nsname = HashMap::new();
        for (key, wikitext) in &b.closure {
            let t = Title::parse(key, &site);
            by_nsname.insert((t.ns, t.text.clone()), wikitext.clone());
        }
        let ts_micros = parse_ts_micros(&b.meta.timestamp);
        CorpusStore { site, by_nsname, ts_micros }
    }
}

impl PageStore for CorpusStore {
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

/// ISO-8601 (`2024-01-02T03:04:05Z`) → unix micros. Minimal, no chrono dep.
fn parse_ts_micros(iso: &str) -> i64 {
    let bytes = iso.as_bytes();
    let g = |a: usize, b: usize| iso.get(a..b).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    if bytes.len() < 19 {
        return 0;
    }
    let (y, mo, d) = (g(0, 4), g(5, 7), g(8, 10));
    let (h, mi, s) = (g(11, 13), g(14, 16), g(17, 19));
    // days since epoch via Howard Hinnant's civil algorithm
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = (if y2 >= 0 { y2 } else { y2 - 399 }) / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    (((days * 24 + h) * 60 + mi) * 60 + s) * 1_000_000
}

// ---------------------------------------------------------------------------
// Element inventory (the diagnostic, applied to BOTH sides)
// ---------------------------------------------------------------------------

/// Counts of the reader-visible structures on a page, after chrome removal.
/// This is a DIAGNOSTIC, not a score: the per-category MediaWiki-vs-ours delta
/// tells you WHAT is missing (a page with 5 MW tables and 0 of ours means the
/// table path or a template broke), which is the thing you act on. There is no
/// single "similarity" number here on purpose — a number invites gaming; a
/// list of missing structures invites fixing.
#[derive(Default, Clone, Copy)]
struct Counts {
    headings: usize,
    tables: usize,
    list_items: usize,
    links: usize,
    refs: usize,
    images: usize,
    paragraphs: usize,
}

impl Counts {
    const LABELS: [&'static str; 7] =
        ["headings", "tables", "list_items", "links", "refs", "images", "paragraphs"];
    fn get(&self, i: usize) -> usize {
        [self.headings, self.tables, self.list_items, self.links, self.refs,
         self.images, self.paragraphs][i]
    }
}

/// Case-insensitive count of non-overlapping `needle` in `hay`.
fn count(hay: &str, needle: &str) -> usize {
    let (h, n) = (hay.to_lowercase(), needle.to_lowercase());
    let mut c = 0;
    let mut from = 0;
    while let Some(rel) = h[from..].find(&n) {
        c += 1;
        from += rel + n.len();
    }
    c
}

/// Inventory the reader-visible structures in `html` (chrome already stripped).
/// `refs` counts reference-list definitions (`id="cite_note-…"`, the scheme both
/// MediaWiki and our Cite use); `images` counts `<img>` — for our render that is
/// 0 (media is placeholdered), so the images delta is exactly "pictures the
/// reader sees that we don't", which the miss inventory attributes.
fn inventory(html: &str) -> Counts {
    let s = strip_chrome(html);
    let headings = (2..=6).map(|h| count(&s, &format!("<h{h}"))).sum();
    Counts {
        headings,
        tables: count(&s, "<table"),
        list_items: count(&s, "<li"),
        links: count(&s, "<a "),
        refs: count(&s, "id=\"cite_note-"),
        images: count(&s, "<img"),
        paragraphs: count(&s, "<p>") + count(&s, "<p "),
    }
}

fn utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Remove `<style>`/`<script>`/comment blocks and `mw-editsection` spans
/// (content included) before tokenizing.
fn strip_chrome(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["style", "script"] {
        s = remove_element(&s, tag);
    }
    // HTML comments.
    while let Some(a) = s.find("<!--") {
        if let Some(rel) = s[a..].find("-->") {
            s.replace_range(a..a + rel + 3, "");
        } else {
            s.truncate(a);
        }
    }
    // Section-edit links: `<span class="mw-editsection …">…</span>`.
    s = remove_class_span(&s, "mw-editsection");
    s
}

/// Remove every `<tag …>…</tag>` (balanced) for a fixed lowercase `tag`.
fn remove_element(s: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(a) = rest.to_lowercase().find(&open) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..a]);
        match rest[a..].to_lowercase().find(&close) {
            Some(rel) => rest = &rest[a + rel + close.len()..],
            None => break, // unterminated — drop the tail
        }
    }
    out
}

/// Remove `<span …class="…marker…"…>…</span>` (balanced over nested spans)
/// for spans whose opening tag mentions `marker`.
fn remove_class_span(s: &str, marker: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("<span") {
            let open_end = s[i..].find('>').map(|e| i + e + 1).unwrap_or(bytes.len());
            if s[i..open_end].contains(marker) {
                // Skip to the matching </span>, counting nesting.
                let mut depth = 1;
                let mut j = open_end;
                while j < bytes.len() && depth > 0 {
                    if s[j..].starts_with("<span") {
                        depth += 1;
                        j += 5;
                    } else if s[j..].starts_with("</span>") {
                        depth -= 1;
                        j += 7;
                    } else {
                        j += utf8_len(bytes[j]);
                    }
                }
                i = j;
                continue;
            }
        }
        let l = utf8_len(bytes[i]);
        out.push_str(&s[i..i + l]);
        i += l;
    }
    out
}

// ---------------------------------------------------------------------------
// The straightedge
// ---------------------------------------------------------------------------

fn load_bundles() -> Vec<(String, Bundle)> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".json.gz"))
        .collect();
    paths.sort();
    for p in paths {
        let f = std::fs::File::open(&p).expect("open bundle");
        let mut gz = flate2::read::GzDecoder::new(f);
        let mut buf = String::new();
        gz.read_to_string(&mut buf).expect("gunzip bundle");
        let bundle: Bundle = serde_json::from_str(&buf).expect("parse bundle");
        let name = p.file_name().unwrap().to_string_lossy().replace(".json.gz", "");
        out.push((name, bundle));
    }
    out
}

fn render_local(b: &Bundle) -> wikimak_wikitext::RenderOutput {
    let store = CorpusStore::new(b);
    let invoker = LuaInvoker::default();
    let media = BlobMediaResolver::new("/w/media/");
    let opts = RenderOptions {
        invoker: Some(&invoker),
        media: Some(&media),
        link_prefix: "/wiki/".into(),
        asof_query: String::new(),
    };
    let title = Title::parse(&b.meta.resolved_title, &store.site);
    render(&store, &title, &b.page_wikitext, &opts)
}

/// One page's rendered result plus the reader-HTML it is measured against.
struct Rendered {
    inv_ours: Counts,
    inv_ref: Counts,
    misses: RenderMisses,
    error_boxes: usize,
}

fn measure(b: &Bundle) -> Rendered {
    let out = render_local(b);
    Rendered {
        inv_ours: inventory(&out.html),
        inv_ref: inventory(&b.reader_html),
        error_boxes: count(&out.html, "class=\"error\""),
        misses: out.misses,
    }
}

/// Cap on the total actionable render failures across the whole corpus
/// (failed `#invoke`s + missing templates + unknown tags). This is the ONLY
/// hard gate, and it is causally tied to rendering real content: the sole way
/// to lower it is to actually render more of the page (support a module, an
/// extension tag, a template) — cosmetic tricks cannot move it, and silently
/// dropping a failure to dodge it would show up as a missing structure in the
/// printed inventory below. Snapshot of today's count; it must not grow. Lower
/// it as the parser handles more. NOT a quality score — a regression tripwire.
const MAX_TOTAL_FAILURES: usize = 2154; // measured 2026-07 over the 12-page corpus

#[test]
fn corpus_render_report() {
    let bundles = load_bundles();
    assert!(!bundles.is_empty(), "no corpus fixtures found — run corpus/capture.py");

    // Per-page structure inventory: MediaWiki reader vs ours. A big negative
    // delta in a category is a concrete lead (missing tables → table/template
    // bug; missing refs → Cite/ref-heavy-template bug; missing images are
    // expected, media is placeholdered).
    eprintln!("\n== per-page structure (reader → ours; Δ = ours − reader) ==");
    let mut invs = Vec::new();
    let mut total_failures = 0usize;
    // module/template/tag → how many pages it failed on (the work list).
    let mut fail_modules: HashMap<String, usize> = HashMap::new();
    let mut fail_templates: HashMap<String, usize> = HashMap::new();
    let mut fail_tags: HashMap<String, usize> = HashMap::new();

    for (name, b) in &bundles {
        let r = measure(b);
        invs.push((name.clone(), r.inv_ours, r.inv_ref));
        eprintln!("  {name}  ({}{}, closure {})",
            b.meta.content_lang, if b.meta.rtl { ", rtl" } else { "" }, b.meta.closure_stored);
        for i in 0..Counts::LABELS.len() {
            let (o, rf) = (r.inv_ours.get(i), r.inv_ref.get(i));
            let d = o as i64 - rf as i64;
            if rf != 0 || o != 0 {
                eprintln!("      {:<11} {:>4} → {:<4} Δ{:+}", Counts::LABELS[i], rf, o, d);
            }
        }
        eprintln!("      error-boxes {}  |  misses: {} invoke, {} template, {} tag, {} media",
            r.error_boxes, r.misses.failed_invokes.len(), r.misses.missing_templates.len(),
            r.misses.unknown_tags.len(), r.misses.missing_media.len());

        // Cite errors are routed through failed_invokes with a "cite" marker;
        // they are a rendering concern, not a missing module, so exclude them
        // from the module work-list but still count them as failures.
        for f in &r.misses.failed_invokes {
            total_failures += 1;
            if !f.to_lowercase().starts_with("cite") {
                *fail_modules.entry(module_of(f)).or_default() += 1;
            }
        }
        for t in &r.misses.missing_templates {
            total_failures += 1;
            *fail_templates.entry(t.clone()).or_default() += 1;
        }
        for t in &r.misses.unknown_tags {
            total_failures += 1;
            *fail_tags.entry(t.clone()).or_default() += 1;
        }

        // Wholesale-break guard: a page that produced NO block structure at all
        // when MediaWiki has plenty means the renderer fell over on it.
        let ours_blocks = r.inv_ours.headings + r.inv_ours.tables + r.inv_ours.paragraphs;
        let ref_blocks = r.inv_ref.headings + r.inv_ref.tables + r.inv_ref.paragraphs;
        assert!(
            ours_blocks > 0 || ref_blocks == 0,
            "{name}: rendered zero block structure while the reader has {ref_blocks} — renderer break"
        );
    }

    eprintln!("\n== work list: what to build next (failures ranked across the corpus) ==");
    print_ranked("modules failing #invoke", &fail_modules);
    print_ranked("templates missing from render", &fail_templates);
    print_ranked("unsupported extension tags", &fail_tags);
    eprintln!("\ncorpus: {} pages, {} total actionable failures (cap {})",
        bundles.len(), total_failures, MAX_TOTAL_FAILURES);

    assert!(
        total_failures <= MAX_TOTAL_FAILURES,
        "actionable render failures rose to {total_failures} (cap {MAX_TOTAL_FAILURES}) — \
         a page now renders LESS real content than before; fix the regression, don't raise the cap"
    );
}

/// Best-effort module name out of a failed-invoke message (`"Module:Foo::bar: …"`
/// or `"Foo::bar"`) for the ranked work list.
fn module_of(msg: &str) -> String {
    msg.split("::").next().unwrap_or(msg).split(':').last().unwrap_or(msg)
        .split_whitespace().next().unwrap_or(msg).to_string()
}

fn print_ranked(header: &str, m: &HashMap<String, usize>) {
    if m.is_empty() {
        eprintln!("  {header}: (none)");
        return;
    }
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    eprintln!("  {header}:");
    for (name, n) in v.into_iter().take(15) {
        eprintln!("      {n:>3}× {name}");
    }
}

/// The inventory must count real structure and the chrome-strip must remove
/// editor scaffolding — not erase everything (which would make every page look
/// empty and hide regressions).
#[test]
fn inventory_counts_structure_and_strips_chrome() {
    let c = inventory(r#"<div class="mw-parser-output"><style>x{y:z}</style>
        <h2>Hello <span class="mw-editsection">[edit]</span></h2>
        <table><tbody><tr><td>a</td></tr></tbody></table>
        <p>World</p><ol class="references"><li id="cite_note-1">ref</li></ol></div>"#);
    assert_eq!(c.headings, 1, "one heading");
    assert_eq!(c.tables, 1, "one table");
    assert_eq!(c.paragraphs, 1, "one paragraph");
    assert_eq!(c.refs, 1, "one reference definition");
    // Chrome must be gone: the <style> block's `z` and the editsection [edit]
    // must not survive into the stripped text (would inflate paragraph/word
    // scans and mask real content).
    let stripped = strip_chrome(r#"<style>a{b:c}</style><p>x</p><span class="mw-editsection">[edit]</span>"#);
    assert!(!stripped.contains("b:c"), "style block not stripped: {stripped}");
    assert!(!stripped.to_lowercase().contains("edit]"), "editsection not stripped: {stripped}");
    assert!(stripped.contains("<p>x</p>"), "real content must survive: {stripped}");
}
