//! parserTests conformance harness (plan §6: "Accuracy: defined, not
//! vibed"). Runs MediaWiki's own parser-test corpus through
//! [`wikimak_wikitext::render`] and reports a MEASURED pass-rate, not a
//! vibe.
//!
//! Two entry points:
//!
//!   * [`core_subset_meets_floor`] (default `cargo test`) scores a small
//!     vendored subset of REAL cases (`parsertests_data/core-subset.txt`,
//!     copied verbatim from upstream — see that file's header) and asserts
//!     the pass count is at least [`CORE_FLOOR_PASSED`], a floor set to the
//!     actually-measured rate so any regression fails. It prints
//!     `parserTests core subset: X/Y passed (Z skipped)`.
//!   * [`full_corpus_score`] (`--ignored`, network) fetches the CURRENT
//!     full `parserTests.txt` from the documented raw URL and prints the
//!     real number over the whole corpus. Gated like the livewiki tests.
//!
//! ## What "passed" means (structural, not byte-equal)
//!
//! The plan says compare at STRUCTURE level, so `normalize()` canonicalizes
//! exactly the incidental differences the plan enumerates and NOTHING that
//! would let genuinely-wrong output pass:
//!
//!   1. the renderer's own `<div class="mw-parser-output">…</div>` wrapper
//!      (our container; upstream fixtures have none);
//!   2. section-edit chrome — the `<div class="mw-heading …">` heading
//!      wrapper and the `<span class="mw-editsection">…</span>` links —
//!      which the serve layer (not the parser core) owns (plan §5/§6
//!      "section-edit spans");
//!   3. whitespace that is insignificant because it sits at a BLOCK-element
//!      boundary (MediaWiki pretty-prints newlines between/around block
//!      tags; our renderer omits them). Inline structure, inline
//!      whitespace, tag names, nesting, attributes, and text are ALL
//!      preserved and compared — so a wrong tag, wrong nesting, missing
//!      attribute, or altered text still fails. In particular this does NOT
//!      strip tags then compare text.
//!
//! Everything the renderer gets wrong under that lens counts as a failure,
//! on purpose: those failures are the parser's real gap list.

use std::collections::{BTreeMap, HashSet};

use wikimak_wikitext::{
    render, InterwikiEntry, NamespaceInfo, PageStore, RenderOptions, SiteConfig, Title,
};

// ---------------------------------------------------------------------------
// Floor. Measured first, then pinned here so regressions fail (never 0, never
// a token value — it is the real count over the vendored subset).
// ---------------------------------------------------------------------------

/// Number of vendored core-subset cases the renderer passes today. Set to
/// the measured value; a drop below this fails [`core_subset_meets_floor`].
const CORE_FLOOR_PASSED: usize = 23;

const CORE_SUBSET: &str = include_str!("parsertests_data/core-subset.txt");

/// Documented raw source for the full corpus (the `#[ignore]` test fetches
/// this). GPL-2.0-or-later; never vendored wholesale (plan §6).
const FULL_CORPUS_URL: &str =
    "https://raw.githubusercontent.com/wikimedia/mediawiki/master/tests/parser/parserTests.txt";

// ===========================================================================
// parserTests.txt format parser
// ===========================================================================

/// One `!! test … !! end` block, reduced to the sections we consult.
#[derive(Debug, Default, Clone)]
struct Case {
    name: String,
    options: String,
    wikitext: Option<String>,
    /// The legacy (non-Parsoid) expected HTML: `!! html`, or `!! html/php`,
    /// or `!! html/*` — whichever is present, in that priority.
    legacy_html: Option<String>,
    /// True when the case has ONLY a Parsoid html section (no legacy one).
    parsoid_only: bool,
}

/// Recognize a `!!`-delimiter line and return its lowercased section word
/// (`test`, `end`, `wikitext`, `html`, `html/php`, `options`, …). Restricted
/// to a known keyword set so wikitext that itself contains a leading `!!`
/// (table syntax) is NOT mistaken for a delimiter.
fn marker_word(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("!!")?;
    let word = rest.trim().split_whitespace().next().unwrap_or("");
    let w = word.to_ascii_lowercase();
    let known = w.starts_with("html")
        || matches!(
            w.as_str(),
            "test"
                | "end"
                | "wikitext"
                | "wikitext/edited"
                | "options"
                | "metadata"
                | "config"
                | "functionhooks"
                | "article"
                | "endarticle"
        );
    if known {
        Some(w)
    } else {
        None
    }
}

fn parse_cases(text: &str) -> Vec<Case> {
    let lines: Vec<&str> = text.lines().collect();
    let mut cases = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if marker_word(lines[i]).as_deref() != Some("test") {
            i += 1;
            continue;
        }
        i += 1;
        // Name: the lines up to the first section marker.
        let mut name = String::new();
        while i < lines.len() && marker_word(lines[i]).is_none() {
            if !name.is_empty() {
                name.push('\n');
            }
            name.push_str(lines[i]);
            i += 1;
        }
        // Sections until `!! end`.
        let mut sections: BTreeMap<String, String> = BTreeMap::new();
        let mut cur: Option<String> = None;
        let mut buf = String::new();
        while i < lines.len() {
            if let Some(w) = marker_word(lines[i]) {
                if w == "end" {
                    i += 1;
                    break;
                }
                if let Some(c) = cur.take() {
                    sections.entry(c).or_insert(buf.clone());
                }
                buf.clear();
                cur = Some(w);
                i += 1;
            } else {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(lines[i]);
                i += 1;
            }
        }
        if let Some(c) = cur.take() {
            sections.entry(c).or_insert(buf.clone());
        }

        let legacy_html = sections
            .get("html")
            .or_else(|| sections.get("html/php"))
            .or_else(|| sections.get("html/*"))
            .cloned();
        let parsoid_only = legacy_html.is_none() && sections.keys().any(|k| k.starts_with("html"));

        cases.push(Case {
            name: name.trim().to_string(),
            options: sections.get("options").cloned().unwrap_or_default(),
            wikitext: sections.get("wikitext").cloned(),
            legacy_html,
            parsoid_only,
        });
    }
    cases
}

/// Options this renderer cannot honor faithfully → the case is SKIPPED
/// (counted, not failed). We honor empty options, Parsoid round-trip flags
/// (they don't change legacy HTML), and metadata-only flags. We bail on
/// anything that changes the parse context or exercises a subsystem the
/// core parser doesn't own.
fn must_skip_options(options: &str) -> bool {
    // Tokenize on whitespace and commas; a token's key is the part before
    // '=' (e.g. `title`, `language`, `maxincludesize`).
    for tok in options.split(|c: char| c.is_whitespace() || c == ',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let key = tok.split('=').next().unwrap_or("").to_ascii_lowercase();
        let skip = matches!(
            key.as_str(),
            "title"
                | "language"
                | "wtvariantlanguage"
                | "variant"
                | "pst"
                | "msg"
                | "comment"
                | "subpage"
                | "wgrawhtml"
                | "rawhtml"
                | "maxincludesize"
                | "maxtemplatedepth"
                | "extension"
                | "showtitle"
                | "showflags"
                | "showtocdata"
                | "showindicators"
                | "showmedia"
                | "property"
                | "styletag"
                | "thumbsize"
                | "externallinktarget"
                | "preprocessor"
                | "djvu"
                | "disabled"
                | "nohtml"
        );
        if skip {
            return true;
        }
    }
    false
}

// ===========================================================================
// Minimal, template-free PageStore
// ===========================================================================

/// A store bound to a parserTest-like siteinfo. `page_text` is always None
/// (core cases are template-free; preprocessing is a near-noop), and a small
/// fixed set of titles "exists" so blue-vs-red link classification runs.
struct PtStore {
    site: SiteConfig,
    existing: HashSet<String>,
}

impl PtStore {
    fn new() -> Self {
        let mut namespaces = BTreeMap::new();
        for (id, canonical, aliases) in [
            (-2, "Media", &["Media"][..]),
            (-1, "Special", &["Special"][..]),
            (0, "", &[][..]),
            (1, "Talk", &["Talk"][..]),
            (2, "User", &["User"][..]),
            (4, "Project", &["Wikipedia", "Project"][..]),
            (6, "File", &["File", "Image"][..]),
            (10, "Template", &["Template"][..]),
            (12, "Help", &["Help"][..]),
            (14, "Category", &["Category"][..]),
        ] {
            namespaces.insert(
                id,
                NamespaceInfo {
                    id,
                    canonical: canonical.to_string(),
                    aliases: aliases.iter().map(|s| s.to_string()).collect(),
                    case_first_letter: true,
                },
            );
        }
        let mut interwiki = BTreeMap::new();
        for (p, url) in [
            ("wikipedia", "https://en.wikipedia.org/wiki/$1"),
            ("meatball", "http://www.usemod.com/cgi-bin/mb.pl?$1"),
        ] {
            interwiki.insert(
                p.to_string(),
                InterwikiEntry {
                    prefix: p.to_string(),
                    url: url.to_string(),
                    local_instance: None,
                },
            );
        }
        let site = SiteConfig {
            site_name: "MediaWiki".to_string(),
            db_name: "parsertest".to_string(),
            lang: "en".to_string(),
            rtl: false,
            namespaces,
            interwiki,
            ..Default::default()
        };
        // parserTests treats these as existing articles.
        let existing = ["Main Page", "Foo", "Bar", "Template:Foo", "Category:Foo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        PtStore { site, existing }
    }
}

impl PageStore for PtStore {
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

fn render_wikitext(store: &PtStore, wikitext: &str) -> String {
    let opts = RenderOptions {
        link_prefix: "/wiki/".to_string(),
        asof_query: String::new(),
        ..Default::default()
    };
    let title = Title {
        ns: 0,
        text: "Parser test".to_string(),
    };
    render(store, &title, wikitext, &opts).html
}

// ===========================================================================
// Structural normalization
// ===========================================================================

const BLOCK_TAGS: &[&str] = &[
    "p", "div", "ul", "ol", "li", "dl", "dt", "dd", "table", "thead", "tbody", "tfoot", "tr", "td",
    "th", "caption", "h1", "h2", "h3", "h4", "h5", "h6", "hr", "pre", "blockquote", "center",
    "section", "article", "figure", "figcaption", "hgroup",
];

fn normalize(html: &str) -> String {
    let s = strip_parser_output_wrapper(html);
    let s = remove_balanced_span(&s, "<span class=\"mw-editsection\">");
    let s = unwrap_div(&s, "<div class=\"mw-heading");
    collapse_block_whitespace(&s)
}

/// Remove exactly one outer `<div class="mw-parser-output"[…]>…</div>` shell.
fn strip_parser_output_wrapper(html: &str) -> String {
    let t = html.trim();
    if let Some(rest) = t.strip_prefix("<div class=\"mw-parser-output\"") {
        if let Some(gt) = rest.find('>') {
            let inner = &rest[gt + 1..];
            if let Some(inner) = inner.strip_suffix("</div>") {
                return inner.to_string();
            }
        }
    }
    t.to_string()
}

/// Remove every `<span …>` that begins with `open` together with its
/// balanced `</span>` (nested spans counted). Used for the section-edit
/// links, whose inner brackets are themselves spans.
fn remove_balanced_span(s: &str, open: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(pos) = rest.find(open) {
        out.push_str(&rest[..pos]);
        // Scan from just past the opening tag, tracking span depth.
        let mut idx = pos + open.len();
        let mut depth = 1usize;
        while depth > 0 {
            let next_open = rest[idx..].find("<span");
            let next_close = rest[idx..].find("</span>");
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    idx += o + "<span".len();
                    depth += 1;
                }
                (_, Some(c)) => {
                    idx += c + "</span>".len();
                    depth -= 1;
                }
                _ => {
                    // Unbalanced; consume the rest.
                    idx = rest.len();
                    depth = 0;
                }
            }
        }
        rest = &rest[idx..];
    }
    out.push_str(rest);
    out
}

/// Unwrap `<div class="…"…>…</div>` blocks whose open tag begins with
/// `open_prefix`, keeping the inner content and dropping the div's own tags
/// (balanced over nested divs).
fn unwrap_div(s: &str, open_prefix: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(pos) = rest.find(open_prefix) {
        // Find the end of this opening tag.
        let after_prefix = pos + open_prefix.len();
        let gt = match rest[after_prefix..].find('>') {
            Some(g) => after_prefix + g + 1,
            None => {
                out.push_str(rest);
                return out;
            }
        };
        out.push_str(&rest[..pos]); // text before the div
        let mut idx = gt;
        let mut depth = 1usize;
        while depth > 0 {
            let next_open = rest[idx..].find("<div");
            let next_close = rest[idx..].find("</div>");
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    out.push_str(&rest[idx..idx + o + "<div".len()]);
                    idx += o + "<div".len();
                    depth += 1;
                }
                (_, Some(c)) => {
                    out.push_str(&rest[idx..idx + c]); // inner content, drop </div>
                    idx += c + "</div>".len();
                    depth -= 1;
                }
                _ => {
                    out.push_str(&rest[idx..]);
                    return out;
                }
            }
        }
        rest = &rest[idx..];
    }
    out.push_str(rest);
    out
}

#[derive(Debug)]
enum Tok {
    /// A tag; `block` is true for block-level element open/close tags.
    Tag { text: String, block: bool },
    Text(String),
}

/// Split HTML into tags and text runs.
fn tokenize(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut text = String::new();
    while i < b.len() {
        if b[i] == b'<' {
            // A tag runs to the next '>'.
            if let Some(rel) = s[i..].find('>') {
                let tag = &s[i..i + rel + 1];
                // Extract the element name.
                let name_src = tag.trim_start_matches('<').trim_start_matches('/');
                let name: String = name_src
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if !name.is_empty() {
                    if !text.is_empty() {
                        toks.push(Tok::Text(std::mem::take(&mut text)));
                    }
                    let block = BLOCK_TAGS.contains(&name.as_str());
                    toks.push(Tok::Tag {
                        text: tag.to_string(),
                        block,
                    });
                    i += rel + 1;
                    continue;
                }
            }
        }
        // Ordinary character (or a lone '<' that is not a tag).
        let l = utf8_len(b[i]);
        text.push_str(&s[i..i + l]);
        i += l;
    }
    if !text.is_empty() {
        toks.push(Tok::Text(text));
    }
    toks
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Drop whitespace that is insignificant because it borders a block-level
/// tag (MediaWiki's pretty-print newlines). Whitespace inside inline runs
/// and inside text is preserved untouched.
fn collapse_block_whitespace(s: &str) -> String {
    let toks = tokenize(s);
    let n = toks.len();
    let mut out = String::new();
    for (i, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Tag { text, .. } => out.push_str(text),
            Tok::Text(t) => {
                let prev_block = i
                    .checked_sub(1)
                    .and_then(|j| toks.get(j))
                    .map(|t| matches!(t, Tok::Tag { block: true, .. }))
                    .unwrap_or(true); // start-of-string acts like a block edge
                let next_block = toks
                    .get(i + 1)
                    .map(|t| matches!(t, Tok::Tag { block: true, .. }))
                    .unwrap_or(i + 1 >= n); // end-of-string acts like a block edge
                let mut piece: &str = t;
                if prev_block {
                    piece = piece.trim_start();
                }
                if next_block {
                    piece = piece.trim_end();
                }
                out.push_str(piece);
            }
        }
    }
    out
}

// ===========================================================================
// Scoring
// ===========================================================================

#[derive(Default)]
struct Score {
    passed: usize,
    failed: usize,
    skipped: usize,
    /// (name, expected-normalized, actual-normalized) for the first few
    /// failures, so a regression prints something actionable.
    fail_samples: Vec<(String, String, String)>,
}

fn score(corpus: &str, keep_samples: usize) -> Score {
    let store = PtStore::new();
    let mut sc = Score::default();
    // The corpus includes fuzz cases engineered to break parsers; a render
    // panic is a genuine failure to render (not a skip), so catch it per
    // case and count it against us. Silence the default panic printer for
    // the duration so the score output stays readable.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for case in parse_cases(corpus) {
        let (wikitext, expected) = match (&case.wikitext, &case.legacy_html) {
            (Some(w), Some(h)) => (w, h),
            // No wikitext, or only a Parsoid target (parsoid_only) — nothing
            // this renderer can be scored against: skip and count it.
            _ => {
                sc.skipped += 1;
                continue;
            }
        };
        if must_skip_options(&case.options) {
            sc.skipped += 1;
            continue;
        }
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_wikitext(&store, wikitext)
        }));
        let want = normalize(expected);
        let got = match rendered {
            Ok(actual) => normalize(&actual),
            Err(_) => "<render panicked>".to_string(),
        };
        if want == got {
            sc.passed += 1;
        } else {
            sc.failed += 1;
            if sc.fail_samples.len() < keep_samples {
                sc.fail_samples.push((case.name.clone(), want, got));
            }
        }
    }
    std::panic::set_hook(prev_hook);
    sc
}

fn total_scored(sc: &Score) -> usize {
    sc.passed + sc.failed
}

fn pct(passed: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * passed as f64 / total as f64
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// Default `cargo test`: score the vendored subset and hold the measured
/// floor. Prints the rate and (on regression) the first failing diffs.
#[test]
fn core_subset_meets_floor() {
    let sc = score(CORE_SUBSET, 50);
    let total = total_scored(&sc);
    println!(
        "parserTests core subset: {}/{} passed ({} skipped) — {:.1}%",
        sc.passed,
        total,
        sc.skipped,
        pct(sc.passed, total)
    );
    assert!(
        total >= 20,
        "core subset shrank unexpectedly ({total} scorable cases); the \
         vendored data file may be truncated"
    );
    if sc.passed < CORE_FLOOR_PASSED {
        for (name, want, got) in &sc.fail_samples {
            eprintln!("FAIL {name}\n  expected: {want}\n  actual:   {got}");
        }
        panic!(
            "core-subset pass count {} fell below the measured floor {} \
             (regression): {}/{} = {:.1}%",
            sc.passed,
            CORE_FLOOR_PASSED,
            sc.passed,
            total,
            pct(sc.passed, total)
        );
    }
}

/// Network-gated (`cargo test -p wikimak-wikitext --test parsertests --
/// --ignored`): fetch the CURRENT full corpus and print the real number.
/// This is the honest, whole-suite figure; it does not gate CI.
#[test]
#[ignore]
fn full_corpus_score() {
    use std::time::Duration;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("build reqwest client");
    let body = client
        .get(FULL_CORPUS_URL)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .expect("fetch full parserTests.txt");

    let sc = score(&body, 25);
    let total = total_scored(&sc);
    println!(
        "parserTests FULL corpus: {}/{} passed ({} skipped) — {:.1}%",
        sc.passed,
        total,
        sc.skipped,
        pct(sc.passed, total)
    );
    eprintln!("--- first failing constructs (sampled) ---");
    for (name, want, got) in sc.fail_samples.iter().take(25) {
        let want = truncate(want, 160);
        let got = truncate(got, 160);
        eprintln!("FAIL {name}\n  expected: {want}\n  actual:   {got}");
    }
    assert!(total > 0, "no scorable cases fetched — corpus shape changed?");
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

// ---------------------------------------------------------------------------
// Unit checks on the harness itself, so a bug in the *harness* (not the
// renderer) is caught: the normalizer must be structure-preserving, not a
// tag-stripper that lets wrong output through.
// ---------------------------------------------------------------------------

#[test]
fn normalizer_preserves_structure_but_folds_incidental_ws() {
    // Block-boundary newlines fold away; the two are structurally equal.
    assert_eq!(
        normalize("<ul><li>a</li>\n<li>b</li></ul>"),
        normalize("<ul><li>a</li><li>b</li></ul>")
    );
    // Trailing newline before a block close folds.
    assert_eq!(normalize("<p>x\n</p>"), normalize("<p>x</p>"));
    // The renderer wrapper is stripped.
    assert_eq!(
        normalize("<div class=\"mw-parser-output\"><p>x</p></div>"),
        normalize("<p>x</p>")
    );
    // Section-edit chrome around a heading folds to the bare heading.
    assert_eq!(
        normalize(
            "<div class=\"mw-heading mw-heading2\"><h2 id=\"S\">S</h2>\
             <span class=\"mw-editsection\"><span class=\"mw-editsection-bracket\">[</span>\
             <a href=\"x\">edit</a><span class=\"mw-editsection-bracket\">]</span></span></div>"
        ),
        normalize("<h2 id=\"S\">S</h2>")
    );
}

#[test]
fn normalizer_is_not_a_tag_stripper() {
    // Different tags must stay different (a lazy "strip tags, compare text"
    // normalizer would wrongly call these equal).
    assert_ne!(normalize("<b>x</b>"), normalize("<i>x</i>"));
    // Different nesting must stay different.
    assert_ne!(
        normalize("<ul><li>a<ul><li>b</li></ul></li></ul>"),
        normalize("<ul><li>a</li><li>b</li></ul>")
    );
    // A missing attribute must stay a difference.
    assert_ne!(
        normalize("<a href=\"/x\" title=\"X\">x</a>"),
        normalize("<a href=\"/x\">x</a>")
    );
    // Inline whitespace (not at a block edge) is significant.
    assert_ne!(normalize("<p>a b</p>"), normalize("<p>ab</p>"));
}

#[test]
fn format_parser_reads_sections_and_variants() {
    let sample = "\
!! test
Alpha
!! wikitext
hello
!! html
<p>hello</p>
!! end

!! test
Beta with options
!! options
title=[[X]]
!! wikitext
world
!! html/php
<p>world</p>
!! end

!! test
Gamma parsoid only
!! wikitext
z
!! html/parsoid
<p>z</p>
!! end
";
    let cases = parse_cases(sample);
    assert_eq!(cases.len(), 3);
    assert_eq!(cases[0].name, "Alpha");
    assert_eq!(cases[0].wikitext.as_deref(), Some("hello"));
    assert_eq!(cases[0].legacy_html.as_deref(), Some("<p>hello</p>"));
    // html/php is picked up as legacy html.
    assert_eq!(cases[1].legacy_html.as_deref(), Some("<p>world</p>"));
    assert!(must_skip_options(&cases[1].options));
    // Parsoid-only case has no legacy html and is flagged.
    assert!(cases[2].legacy_html.is_none());
    assert!(cases[2].parsoid_only);
}
