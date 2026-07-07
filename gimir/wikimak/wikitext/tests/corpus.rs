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
    render, NamespaceInfo, PageStore, RenderOptions, SiteConfig, Title,
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
// Cruft-stripping structural signature (applied to BOTH sides)
// ---------------------------------------------------------------------------

/// Reduce HTML to a stream of structure+text tokens with all editor/presentation
/// chrome removed: `<style>`/`<script>` blocks and comments dropped whole,
/// `mw-editsection` spans removed with their content, every attribute dropped
/// (so `data-mw`/`typeof`/`about`/ids never count), tag names lowercased, text
/// whitespace-collapsed to lowercased words.
fn signature(html: &str) -> Vec<String> {
    let cleaned = strip_chrome(html);
    let b = cleaned.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut text = String::new();
    let flush = |text: &mut String, toks: &mut Vec<String>| {
        for w in text.split_whitespace() {
            toks.push(w.to_lowercase());
        }
        text.clear();
    };
    while i < b.len() {
        if b[i] == b'<' {
            flush(&mut text, &mut toks);
            let end = cleaned[i..].find('>').map(|e| i + e + 1).unwrap_or(b.len());
            let inner = &cleaned[i + 1..end.saturating_sub(1)];
            let close = inner.starts_with('/');
            let name: String = inner
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            if !name.is_empty() {
                // Fold reader-invisible/empty wrappers so nesting-only
                // differences don't dominate: bare spans and the parser-output
                // div carry no block structure a reader perceives.
                if name != "span" && name != "div" {
                    toks.push(if close { format!("</{name}") } else { format!("<{name}") });
                }
            }
            i = end;
        } else {
            let l = utf8_len(b[i]);
            text.push_str(&cleaned[i..i + l]);
            i += l;
        }
    }
    flush(&mut text, &mut toks);
    toks
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

/// Dice coefficient over token bigrams: `2·|A∩B| / (|A|+|B|)` on the bigram
/// multisets. Captures local order without full-sequence LCS cost.
fn dice(a: &[String], b: &[String]) -> f64 {
    let bigrams = |t: &[String]| -> HashMap<(String, String), i64> {
        let mut m = HashMap::new();
        for w in t.windows(2) {
            *m.entry((w[0].clone(), w[1].clone())).or_insert(0) += 1;
        }
        m
    };
    let (ma, mb) = (bigrams(a), bigrams(b));
    let (na, nb): (i64, i64) = (ma.values().sum(), mb.values().sum());
    if na == 0 || nb == 0 {
        return 0.0;
    }
    let inter: i64 = ma.iter().map(|(k, va)| *va.min(mb.get(k).unwrap_or(&0))).sum();
    2.0 * inter as f64 / (na + nb) as f64
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

fn render_local(b: &Bundle) -> String {
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
    render(&store, &title, &b.page_wikitext, &opts).html
}

/// Aggregate structural-similarity floor over the committed corpus. The render
/// is deterministic (τ from the fixture, no wall clock, no randomness), so this
/// is a tight regression gate: measured mean is 0.695 (per-page 0.47–0.90; the
/// heavily-templated en/zh/ja pages sit lowest, RTL ar/fa/he render well).
/// Floored a hair under the mean to catch real regressions with a little
/// refactor headroom; RAISE it as fidelity improves — that is the straightedge
/// working.
const AGGREGATE_FLOOR: f64 = 0.65;

#[test]
fn corpus_straightedge() {
    let bundles = load_bundles();
    assert!(!bundles.is_empty(), "no corpus fixtures found — run corpus/capture.py");

    let mut scores = Vec::new();
    for (name, b) in &bundles {
        let ours = render_local(b);
        let sig_ours = signature(&ours);
        let sig_ref = signature(&b.reader_html);
        let s = dice(&sig_ours, &sig_ref);
        scores.push(s);
        eprintln!(
            "  {name:14} {:>5} rtl={:<5} closure={:<4} script~{:<9} similarity {:.3}",
            b.meta.revid % 100000,
            b.meta.rtl,
            b.meta.closure_stored,
            b.meta.lang,
            s
        );
        let _ = &b.meta.resolved_title;
    }
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    eprintln!(
        "corpus straightedge: {} pages, mean structural similarity {:.3} (floor {:.2})",
        scores.len(),
        mean,
        AGGREGATE_FLOOR
    );
    assert!(
        mean >= AGGREGATE_FLOOR,
        "mean structural similarity {mean:.3} fell below floor {AGGREGATE_FLOOR:.2} — a rendering regression"
    );
}

/// The signature must actually strip chrome and preserve content — not just
/// erase everything (which would make any two pages look identical).
#[test]
fn signature_strips_chrome_not_content() {
    let a = signature(r#"<div class="mw-parser-output"><style>x{y:z}</style><h2>Hello <span class="mw-editsection">[edit]</span></h2><p>World</p></div>"#);
    assert!(a.contains(&"hello".to_string()) && a.contains(&"world".to_string()));
    assert!(!a.iter().any(|t| t == "edit"), "editsection text must be stripped: {a:?}");
    assert!(!a.iter().any(|t| t == "z" || t == "x"), "style block must be stripped: {a:?}");
    assert!(a.contains(&"<h2".to_string()), "structure must survive: {a:?}");
    // Different content must NOT compare equal (guards a degenerate normalizer).
    let b = signature("<p>totally different text here</p>");
    assert!(dice(&a, &b) < 0.5, "unrelated pages must score low");
}
