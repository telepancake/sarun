//! τ-correctness of the local renderer (browsing plan §6, "Date-picker
//! correctness"), driven END-TO-END through `wikimak_wikitext::render` via
//! [`AsOfView`] — no HTTP, no server. A scripted timeline is imported once;
//! every assertion renders a page at a chosen instant τ and pins the exact
//! era-correct output.
//!
//! Run: `cargo test -p wikimak-wikipedia --test tau_render` (needs the
//! default `serve` feature, which pulls in the renderer + Lua invoker).
//!
//! The timeline (three instants t1 < t2 < t3, spaced four years apart so
//! calendar years are unambiguous):
//!
//!   * `Article` — three revisions (Era ONE / TWO / THREE) at t1/t2/t3.
//!     Each transcludes `{{Box|who=…}}` and `{{#invoke:Calc|run|…}}` and
//!     links `[[Ghost]]` (a title that never exists → red link).
//!   * `Template:Box` — two revisions (OLD at t1, NEW at t2). The template
//!     body embeds `{{CURRENTYEAR}}`, which MUST resolve to the year of τ
//!     (`PageStore::timestamp_micros`), never the wall clock.
//!   * `Module:Calc` — two revisions (`calc-one` at t1, `calc-two` at t2).
//!     Module SOURCE-at-τ is what makes the invoke output era-specific.
//!   * `Portal` — a normal page at t1 ("Portal HOME") that BECOMES a
//!     `#REDIRECT [[Article]]` at t3: before the redirect revision it
//!     renders as its own page; after, it follows.
//!   * `LateAlias` — a page whose ONLY revision (a redirect) appears at t3:
//!     the redirect is FOLLOWED only once its revision exists at τ (at head
//!     it reaches `Article`; before t3 it does not, because there is no
//!     redirect text yet).
//!
//! Honest scope notes (the importer's real limits, not idealizations):
//!
//!   * The importer records ONE open title interval `[0, ∞)` per page (see
//!     `tests/asof.rs`), so *title* existence is NOT gated on revision time.
//!     Consequences pinned here rather than papered over:
//!       - "the page is missing before t1" is checked the way the renderer
//!         sees it — `page_text_at` / `AsOfView::page_text` returning `None`
//!         (no revision ≤ τ), NOT via the title table (which says it exists);
//!       - a title whose FIRST revision is later (`LateAlias` before t3) is
//!         therefore NOT a red link — it renders BLUE (`exists_at` is a
//!         title-only point check) as an existing-but-text-less page. The
//!         only genuine red link is a title with no page row at all
//!         (`Ghost`), which this suite uses for the red-link assertions.
//!   * `resolve_at` deliberately treats a title-with-no-text as a terminal
//!     EXISTING page (`Ok(Some(id))`, documented in `asof.rs`); it is the
//!     absence of redirect *text* at τ — not title non-existence — that
//!     keeps it from following before the redirect revision exists.

#![cfg(feature = "serve")]

mod common;

use std::io::Cursor;

use chrono::{DateTime, Datelike};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_scribunto::LuaInvoker;
use wikimak_wikitext::{render, ModuleInvoker, PageStore, RenderOptions, Title};

use common::make_instance;
use wikimak_wikipedia::asof::{resolve_at, AsOfView};
use wikimak_wikipedia::Instance;

/// `#REDIRECT` follow budget, matching the serve layer.
const MAX_HOPS: u32 = 10;

const FIXTURE: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>Tau Test Wiki</sitename><dbname>tautestwiki</dbname>
    <base>http://tau.test/wiki/Main_Page</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces>
      <namespace key="0" case="first-letter"/>
      <namespace key="6" case="first-letter">File</namespace>
      <namespace key="10" case="first-letter">Template</namespace>
      <namespace key="14" case="first-letter">Category</namespace>
      <namespace key="828" case="first-letter">Module</namespace>
    </namespaces>
  </siteinfo>
  <page>
    <title>Article</title><ns>0</ns><id>100</id>
    <revision>
      <id>101</id><timestamp>2010-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>e1</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">Era ONE
{{Box|who=Ada}}
Calc: {{#invoke:Calc|run|Ada}}
[[Ghost]]</text><sha1>a1</sha1>
    </revision>
    <revision>
      <id>102</id><parentid>101</parentid><timestamp>2014-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>e2</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">Era TWO
{{Box|who=Bob}}
Calc: {{#invoke:Calc|run|Bob}}
[[Ghost]]</text><sha1>a2</sha1>
    </revision>
    <revision>
      <id>103</id><parentid>102</parentid><timestamp>2018-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>e3</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">Era THREE
{{Box|who=Cy}}
Calc: {{#invoke:Calc|run|Cy}}
[[Ghost]]</text><sha1>a3</sha1>
    </revision>
  </page>
  <page>
    <title>Template:Box</title><ns>10</ns><id>200</id>
    <revision>
      <id>201</id><timestamp>2010-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>t1</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">Box OLD for {{{who}}} in {{CURRENTYEAR}}</text><sha1>b1</sha1>
    </revision>
    <revision>
      <id>202</id><parentid>201</parentid><timestamp>2014-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>t2</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">Box NEW for {{{who}}} in {{CURRENTYEAR}}</text><sha1>b2</sha1>
    </revision>
  </page>
  <page>
    <title>Module:Calc</title><ns>828</ns><id>300</id>
    <revision>
      <id>301</id><timestamp>2010-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>m1</comment><model>Scribunto</model><format>text/plain</format>
      <text xml:space="preserve">local p = {}
function p.run(frame)
  return "calc-one:" .. (frame.args[1] or "?")
end
return p</text><sha1>c1</sha1>
    </revision>
    <revision>
      <id>302</id><parentid>301</parentid><timestamp>2014-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>m2</comment><model>Scribunto</model><format>text/plain</format>
      <text xml:space="preserve">local p = {}
function p.run(frame)
  return "calc-two:" .. (frame.args[1] or "?")
end
return p</text><sha1>c2</sha1>
    </revision>
  </page>
  <page>
    <title>Portal</title><ns>0</ns><id>400</id>
    <revision>
      <id>401</id><timestamp>2010-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>p1</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">Portal HOME landing content</text><sha1>d1</sha1>
    </revision>
    <revision>
      <id>402</id><parentid>401</parentid><timestamp>2018-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>p2</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">#REDIRECT [[Article]]</text><sha1>d2</sha1>
    </revision>
  </page>
  <page>
    <title>LateAlias</title><ns>0</ns><id>500</id>
    <revision>
      <id>501</id><timestamp>2018-06-15T12:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>l1</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">#REDIRECT [[Article]]</text><sha1>e1</sha1>
    </revision>
  </page>
</mediawiki>"#;

/// Import [`FIXTURE`] into a fresh instance and flush.
fn fixture_instance(tmp: &TempDir) -> Instance {
    let inst = make_instance(tmp, 4096);
    let mut stream = new_page_stream(Cursor::new(FIXTURE.as_bytes().to_vec()));
    inst.import(&mut stream).expect("import fixture");
    inst.flush().expect("flush");
    inst
}

/// The three `Article` revision timestamps (micros), oldest-first.
fn article_times(inst: &Instance) -> [i64; 3] {
    let mut times: Vec<i64> = inst
        .page_history(100)
        .expect("history")
        .map(|e| e.expect("entry").meta.ts.timestamp_micros())
        .collect();
    times.sort();
    assert_eq!(times.len(), 3, "Article has three revisions");
    [times[0], times[1], times[2]]
}

/// The calendar year the renderer will compute for τ — derived the same
/// way the renderer does (from unix micros), so `{{CURRENTYEAR}}`
/// assertions do not hardcode a value the fixture could drift from.
fn year_of(ts: i64) -> i32 {
    DateTime::from_timestamp_micros(ts).expect("valid τ").year()
}

/// Render `wikitext` under `title_str` at τ = `ts`, through the real
/// [`AsOfView`] + Lua invoker. Returns the HTML.
fn render_at(inst: &Instance, title_str: &str, wikitext: &str, ts: Option<i64>) -> String {
    let view = AsOfView::new(inst, ts).expect("build AsOfView");
    let invoker = LuaInvoker::new().expect("build Lua invoker");
    let title = Title::parse(title_str, view.site());
    let opts = RenderOptions {
        invoker: Some(&invoker as &dyn ModuleInvoker),
        media: None,
        link_prefix: "/wiki/".into(),
        asof_query: String::new(),
    };
    render(&view, &title, wikitext, &opts).html
}

/// Read `title_str`'s revision text at τ and render it. Panics if the
/// title has no readable revision ≤ τ (callers assert existence first).
fn render_page_at(inst: &Instance, title_str: &str, ts: Option<i64>) -> String {
    let pid = inst
        .page_id_by_title_at(title_str, ts)
        .expect("title lookup")
        .expect("title resolves at τ");
    let bytes = inst
        .page_text_at(pid, ts)
        .expect("text lookup")
        .expect("revision text at τ");
    let wikitext = String::from_utf8_lossy(&bytes).into_owned();
    render_at(inst, title_str, &wikitext, ts)
}

// ---------------------------------------------------------------------------
// Before t1: the page has no revision yet, so the renderer sees no text.
// ---------------------------------------------------------------------------
#[test]
fn before_first_revision_the_page_has_no_text() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);
    let [t1, _t2, _t3] = article_times(&inst);
    let before = t1 - 1; // one micro before the first Article revision.

    // The renderer's view of existence at τ: no revision text.
    assert_eq!(
        inst.page_text_at(100, Some(before)).expect("text lookup"),
        None,
        "no Article revision at τ before t1"
    );
    assert!(
        inst.revision_at(100, Some(before)).expect("revision_at").is_none(),
        "no selected revision before t1"
    );
    let view = AsOfView::new(&inst, Some(before)).expect("view");
    let title = Title::parse("Article", view.site());
    assert_eq!(view.page_text(&title), None, "PageStore::page_text → None before t1");
}

// ---------------------------------------------------------------------------
// τ ∈ (t1, t2): Era ONE page text, OLD template, calc-one module, and
// {{CURRENTYEAR}} == the year of τ (not the wall clock). Red link present.
// ---------------------------------------------------------------------------
#[test]
fn between_t1_and_t2_renders_era_one() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);
    let [t1, t2, _t3] = article_times(&inst);
    let tau = (t1 + t2) / 2;
    let year = year_of(tau);

    let html = render_page_at(&inst, "Article", Some(tau));

    // (1) era-correct PAGE text.
    assert!(html.contains("Era ONE"), "era-one page text:\n{html}");
    assert!(!html.contains("Era TWO"), "no later page text:\n{html}");
    assert!(!html.contains("Era THREE"), "no head page text:\n{html}");

    // (2) era-correct TEMPLATE output (OLD revision, arg who=Ada).
    assert!(html.contains("Box OLD for Ada"), "old template revision:\n{html}");
    assert!(!html.contains("Box NEW"), "no new template revision:\n{html}");

    // (3) era-correct MODULE output (calc-one source, arg Ada).
    assert!(html.contains("calc-one:Ada"), "old module source output:\n{html}");
    assert!(!html.contains("calc-two"), "no new module source output:\n{html}");

    // (4) {{CURRENTYEAR}} == year of τ, explicitly NOT the wall clock.
    let wall_year = chrono::Utc::now().year();
    assert!(
        html.contains(&format!("in {year}")),
        "CURRENTYEAR is the year of τ ({year}):\n{html}"
    );
    assert_ne!(year, wall_year, "fixture τ must differ from wall clock for the check to bite");
    assert!(
        !html.contains(&format!("in {wall_year}")),
        "CURRENTYEAR must NOT be the wall-clock year ({wall_year}):\n{html}"
    );

    // (5) [[Ghost]] never exists → red link.
    assert!(html.contains("class=\"new\""), "red link for nonexistent target:\n{html}");
}

// ---------------------------------------------------------------------------
// τ ∈ (t2, t3): Era TWO page text, NEW template, calc-two module, and
// {{CURRENTYEAR}} tracks this LATER τ — proving it follows τ, not a fixed
// value nor the wall clock.
// ---------------------------------------------------------------------------
#[test]
fn between_t2_and_t3_renders_era_two() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);
    let [_t1, t2, t3] = article_times(&inst);
    let tau = (t2 + t3) / 2;
    let year = year_of(tau);

    let html = render_page_at(&inst, "Article", Some(tau));

    assert!(html.contains("Era TWO"), "era-two page text:\n{html}");
    assert!(!html.contains("Era ONE"), "no earlier page text:\n{html}");
    assert!(!html.contains("Era THREE"), "no head page text:\n{html}");

    assert!(html.contains("Box NEW for Bob"), "new template revision:\n{html}");
    assert!(!html.contains("Box OLD"), "no old template revision:\n{html}");

    assert!(html.contains("calc-two:Bob"), "new module source output:\n{html}");
    assert!(!html.contains("calc-one"), "no old module source output:\n{html}");

    // CURRENTYEAR moved with τ (distinct from the (t1,t2) window's year).
    let wall_year = chrono::Utc::now().year();
    assert!(html.contains(&format!("in {year}")), "CURRENTYEAR == year of later τ ({year}):\n{html}");
    assert_ne!(year, year_of((article_times(&inst)[0] + t2) / 2), "the two windows fall in different years");
    if year != wall_year {
        assert!(!html.contains(&format!("in {wall_year}")), "not the wall-clock year:\n{html}");
    }
}

// ---------------------------------------------------------------------------
// τ = now (head, None): Era THREE page text and the latest template/module.
// ---------------------------------------------------------------------------
#[test]
fn head_renders_era_three() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let html = render_page_at(&inst, "Article", None);

    assert!(html.contains("Era THREE"), "head page text:\n{html}");
    assert!(!html.contains("Era ONE"), "no era-one at head:\n{html}");
    assert!(!html.contains("Era TWO"), "no era-two at head:\n{html}");

    assert!(html.contains("Box NEW for Cy"), "latest template revision, arg Cy:\n{html}");
    assert!(html.contains("calc-two:Cy"), "latest module source, arg Cy:\n{html}");
    assert!(!html.contains("calc-one"), "no old module at head:\n{html}");

    // At head τ = wall clock, so CURRENTYEAR is the current calendar year.
    let wall_year = chrono::Utc::now().year();
    assert!(html.contains(&format!("in {wall_year}")), "head CURRENTYEAR is the wall-clock year:\n{html}");
}

// ---------------------------------------------------------------------------
// Redirect-at-τ, "own page BEFORE, follows AFTER". `Portal` is its own page
// at τ ∈ (t1,t2); by head it is a #REDIRECT to Article.
// ---------------------------------------------------------------------------
#[test]
fn portal_is_own_page_before_redirect_then_follows() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);
    let [t1, t2, _t3] = article_times(&inst);
    let tau_own = (t1 + t2) / 2; // before the redirect revision at t3.

    let article_id = inst.page_id_by_title_at("Article", None).unwrap().unwrap();
    let portal_id = inst.page_id_by_title_at("Portal", None).unwrap().unwrap();
    assert_ne!(article_id, portal_id, "distinct pages");

    // BEFORE: Portal's τ-revision is its own content, not a redirect.
    let text = inst.page_text_at(portal_id, Some(tau_own)).unwrap().unwrap();
    assert!(
        wikimak_wikitext::parse_redirect(&String::from_utf8_lossy(&text)).is_none(),
        "Portal is not yet a redirect at τ"
    );
    assert_eq!(
        resolve_at(&inst, "Portal", Some(tau_own), MAX_HOPS).unwrap(),
        Some(portal_id),
        "resolves to itself, no follow"
    );
    let own_html = render_page_at(&inst, "Portal", Some(tau_own));
    assert!(own_html.contains("Portal HOME"), "renders its own page:\n{own_html}");

    // AFTER (head): Portal follows the redirect to Article.
    assert_eq!(
        resolve_at(&inst, "Portal", None, MAX_HOPS).unwrap(),
        Some(article_id),
        "follows #REDIRECT to Article at head"
    );
}

// ---------------------------------------------------------------------------
// Redirect-at-τ, "does NOT follow before the redirect revision, follows
// after". `LateAlias`'s only revision (a redirect) appears at t3.
//
// This pins the REAL importer behavior, not an idealized red-link one: the
// open [0,∞) title interval means the title already "exists" before t3
// (exists_at → true, blue link), but with no revision text at τ. So
// resolve_at stops AT LateAlias (terminal text-less page, id 500) instead of
// following through to Article; only at head does the redirect text exist
// and the resolution reach Article. Genuine red-link-at-τ is covered by the
// `Ghost` assertions in the era tests (a title with no page row at all).
// ---------------------------------------------------------------------------
#[test]
fn late_alias_follows_only_after_its_redirect_revision_exists() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);
    let [_t1, t2, t3] = article_times(&inst);
    let tau_before = (t2 + t3) / 2; // before LateAlias's only revision at t3.

    let article_id = inst.page_id_by_title_at("Article", None).unwrap().unwrap();

    // BEFORE t3: no redirect text at τ, so resolution does NOT reach Article.
    assert_eq!(
        inst.page_text_at(500, Some(tau_before)).unwrap(),
        None,
        "no LateAlias revision before t3"
    );
    let before = resolve_at(&inst, "LateAlias", Some(tau_before), MAX_HOPS).unwrap();
    assert_eq!(before, Some(500), "stops at the text-less title, does not follow");
    assert_ne!(before, Some(article_id), "crucially, has NOT reached Article yet");
    // The open title interval means the link is BLUE (exists_at is title-only),
    // NOT a red link — the documented importer gap, pinned as-is.
    let html_before = render_at(&inst, "Article", "[[LateAlias]] [[Ghost]]", Some(tau_before));
    assert!(
        html_before.contains(r#"<a href="/wiki/LateAlias">LateAlias</a>"#),
        "LateAlias renders blue (title exists via open interval):\n{html_before}"
    );
    assert!(
        html_before.contains(r#"class="new">Ghost"#),
        "a title with no page row (Ghost) is the genuine red link:\n{html_before}"
    );

    // AFTER (head): the redirect revision exists ⇒ follows to Article.
    assert_eq!(
        resolve_at(&inst, "LateAlias", None, MAX_HOPS).unwrap(),
        Some(article_id),
        "follows #REDIRECT to Article at head"
    );
}
