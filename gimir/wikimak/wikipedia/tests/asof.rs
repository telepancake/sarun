//! asof-τ read API tests (browsing plan §2, the wayback contract).
//!
//! Run: `cargo test -p wikimak-wikipedia --no-default-features
//! --features fetch`. These pin REAL behavior against a fixture instance
//! built by importing synthesized XML — every assertion is a concrete
//! input→output check that a stub would fail.
//!
//! Honest scope note (see the `title_at_tau_*` tests): the importer now
//! records ONE open interval per stable title, starting at the page's
//! EARLIEST revision timestamp (import.rs `ensure_title`: start_ts =
//! earliest rev ts, end_ts = NULL), and closes/reopens it on a real
//! rename (pinned in `tests/title_rename.rs`). So title-at-τ resolution is
//! exercised three ways: (a) the importer's real single-interval behavior
//! (now gated on the first revision — a τ before it does NOT resolve);
//! (b) synthetic bounded intervals written straight into meta.db to pin
//! the τ-window SQL; (c) real rename intervals in `title_rename.rs`.
//! Back-compat: pre-interval imports left `start_ts = 0` rows, which still
//! resolve for any τ ≥ 0 under the same window SQL (pinned below).

mod common;

use std::io::Cursor;

use rusqlite::{params, Connection};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;

use common::make_instance;
use wikimak_wikipedia::instance::Instance;

/// A fixture with a 3-revision article, two redirect pages (a 1-hop and
/// a 2-hop chain), and a 2-page redirect cycle. Revision timestamps are
/// spaced a year apart so boundary instants are unambiguous.
const FIXTURE: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>Test Wiki</sitename><dbname>testwiki</dbname><base>http://x/</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces>
      <namespace key="0" case="first-letter"/>
      <namespace key="4" case="first-letter">Wikipédia</namespace>
      <namespace key="10" case="first-letter">Template</namespace>
    </namespaces>
  </siteinfo>
  <page>
    <title>Multi Rev</title><ns>0</ns><id>100</id>
    <revision>
      <id>10</id><timestamp>2020-06-15T12:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="5" sha1="aa" xml:space="preserve">alpha</text><sha1>aa</sha1>
    </revision>
    <revision>
      <id>20</id><parentid>10</parentid><timestamp>2021-06-15T12:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="4" sha1="bb" xml:space="preserve">beta</text><sha1>bb</sha1>
    </revision>
    <revision>
      <id>30</id><parentid>20</parentid><timestamp>2022-06-15T12:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="5" sha1="cc" xml:space="preserve">gamma</text><sha1>cc</sha1>
    </revision>
  </page>
  <page>
    <title>Redir One</title><ns>0</ns><id>201</id>
    <revision>
      <id>41</id><timestamp>2021-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="24" sha1="dd" xml:space="preserve">#REDIRECT [[Multi Rev]]</text><sha1>dd</sha1>
    </revision>
  </page>
  <page>
    <title>Redir Two</title><ns>0</ns><id>202</id>
    <revision>
      <id>42</id><timestamp>2021-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="24" sha1="ee" xml:space="preserve">#REDIRECT [[Redir One]]</text><sha1>ee</sha1>
    </revision>
  </page>
  <page>
    <title>Loop A</title><ns>0</ns><id>203</id>
    <revision>
      <id>43</id><timestamp>2021-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="20" sha1="ff" xml:space="preserve">#REDIRECT [[Loop B]]</text><sha1>ff</sha1>
    </revision>
  </page>
  <page>
    <title>Loop B</title><ns>0</ns><id>204</id>
    <revision>
      <id>44</id><timestamp>2021-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="20" sha1="gg" xml:space="preserve">#REDIRECT [[Loop A]]</text><sha1>gg</sha1>
    </revision>
  </page>
</mediawiki>"#;

/// Import [`FIXTURE`] into a fresh instance and flush.
fn fixture_instance(tmp: &TempDir) -> Instance {
    let instance = make_instance(tmp, 4096);
    let mut stream = new_page_stream(Cursor::new(FIXTURE.as_bytes().to_vec()));
    instance.import(&mut stream).expect("import fixture");
    instance.flush().expect("flush");
    instance
}

/// Second connection to the instance's meta.db, for synthetic-row tests
/// and reading timestamps. WAL mode: it sees the instance's committed
/// writes and its own inserts are visible to the instance's connection.
fn meta_conn(tmp: &TempDir) -> Connection {
    Connection::open(tmp.path().join("meta.db")).expect("open meta.db")
}

/// The `(rev_id, ts_micros)` of every revision of `page_id`, newest-first.
fn history_micros(inst: &Instance, page_id: u64) -> Vec<(u64, i64)> {
    inst.page_history(page_id)
        .expect("history")
        .map(|e| {
            let e = e.expect("history entry");
            (e.meta.rev_id, e.meta.ts.timestamp_micros())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// revision_at picks the newest revision with timestamp ≤ τ, at boundary
// instants (equal, one micro before, one micro after) and the ends.
// ---------------------------------------------------------------------------
#[test]
fn revision_at_boundaries() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let hist = history_micros(&inst, 100);
    // newest-first: rev30, rev20, rev10.
    assert_eq!(hist.iter().map(|h| h.0).collect::<Vec<_>>(), vec![30, 20, 10]);
    let (_r30, t30) = hist[0];
    let (_r20, t20) = hist[1];
    let (_r10, t10) = hist[2];

    // τ exactly at rev20 → rev20 (≤ is inclusive).
    assert_eq!(rev_id_at(&inst, 100, Some(t20)), Some(20));
    // one micro before rev20 → rev10.
    assert_eq!(rev_id_at(&inst, 100, Some(t20 - 1)), Some(10));
    // one micro after rev20 → still rev20 (rev30 is later).
    assert_eq!(rev_id_at(&inst, 100, Some(t20 + 1)), Some(20));
    // τ at the head instant → rev30.
    assert_eq!(rev_id_at(&inst, 100, Some(t30)), Some(30));
    // well past the head → rev30.
    assert_eq!(rev_id_at(&inst, 100, Some(t30 + 1_000_000)), Some(30));
    // one micro before the first revision → nothing existed yet.
    assert_eq!(rev_id_at(&inst, 100, Some(t10 - 1)), None);
    // None τ → head (rev30).
    assert_eq!(rev_id_at(&inst, 100, None), Some(30));
    // unknown page → None at any τ.
    assert_eq!(rev_id_at(&inst, 999, Some(t20)), None);
    assert_eq!(rev_id_at(&inst, 999, None), None);
}

fn rev_id_at(inst: &Instance, page_id: u64, ts: Option<i64>) -> Option<u64> {
    inst.revision_at(page_id, ts).expect("revision_at").map(|m| m.rev_id)
}

// ---------------------------------------------------------------------------
// page_text_at decodes the SELECTED revision's bytes at each boundary.
// ---------------------------------------------------------------------------
#[test]
fn page_text_at_boundaries() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let hist = history_micros(&inst, 100);
    let t20 = hist[1].1;
    let t10 = hist[2].1;

    let text = |ts| inst.page_text_at(100, ts).expect("page_text_at");
    assert_eq!(text(Some(t10)).as_deref(), Some(&b"alpha"[..]));
    assert_eq!(text(Some(t20)).as_deref(), Some(&b"beta"[..]));
    assert_eq!(text(Some(t20 - 1)).as_deref(), Some(&b"alpha"[..]));
    assert_eq!(text(None).as_deref(), Some(&b"gamma"[..]));
    assert_eq!(text(Some(t10 - 1)), None);
    // unknown page.
    assert_eq!(inst.page_text_at(999, None).expect("page_text_at"), None);
}

// ---------------------------------------------------------------------------
// page_id_by_title_at: None τ delegates to page_by_title; Some τ resolves
// through the single [0,∞) interval the importer wrote; unknown → None.
// This pins the importer's REAL title-at-τ behavior.
// ---------------------------------------------------------------------------
#[test]
fn title_at_tau_importer_behavior() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let hist = history_micros(&inst, 100);
    let t20 = hist[1].1;
    let t10 = hist[2].1;

    // None τ → current mapping (exact title match).
    assert_eq!(id_at(&inst, "Multi Rev", None), Some(100));
    // Some τ ≥ the first revision → the interval [t10, ∞) resolves.
    assert_eq!(id_at(&inst, "Multi Rev", Some(t20)), Some(100));
    // The first interval now starts at the EARLIEST revision (t10), so a τ
    // BEFORE the page's first revision does NOT resolve — the title did
    // not exist yet (real wayback gating, replacing the old start_ts=0).
    assert_eq!(id_at(&inst, "Multi Rev", Some(t10 - 1)), None);
    // exactly at the first revision → resolves (start inclusive).
    assert_eq!(id_at(&inst, "Multi Rev", Some(t10)), Some(100));
    // whitespace is trimmed to match import's normalization.
    assert_eq!(id_at(&inst, "  Multi Rev  ", Some(t20)), Some(100));
    // unknown title → None at any τ and at head.
    assert_eq!(id_at(&inst, "No Such Page", Some(t20)), None);
    assert_eq!(id_at(&inst, "No Such Page", None), None);
}

fn id_at(inst: &Instance, title: &str, ts: Option<i64>) -> Option<u64> {
    inst.page_id_by_title_at(title, ts).expect("page_id_by_title_at")
}

// ---------------------------------------------------------------------------
// The τ-window SQL, pinned against a synthetic BOUNDED interval written
// straight into meta.db: start_ts <= τ AND (end_ts IS NULL OR end_ts > τ).
// This is the query render-time rename-aware lookups depend on, exercised
// with a real end_ts the importer never produces today.
// ---------------------------------------------------------------------------
#[test]
fn title_at_tau_bounded_interval_window() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    // Page 500 held the title "Windowed" only during [1000, 2000).
    let conn = meta_conn(&tmp);
    conn.execute(
        "INSERT INTO title_intervals(page_id, ns, normalized_title, start_ts, end_ts)
         VALUES(500, 0, ?1, 1000, 2000)",
        params![b"Windowed".to_vec()],
    )
    .unwrap();

    assert_eq!(id_at(&inst, "Windowed", Some(999)), None, "before start");
    assert_eq!(id_at(&inst, "Windowed", Some(1000)), Some(500), "at start (inclusive)");
    assert_eq!(id_at(&inst, "Windowed", Some(1500)), Some(500), "mid interval");
    assert_eq!(id_at(&inst, "Windowed", Some(1999)), Some(500), "just before end");
    assert_eq!(id_at(&inst, "Windowed", Some(2000)), None, "at end (exclusive)");
    assert_eq!(id_at(&inst, "Windowed", Some(3000)), None, "after end");
}

// ---------------------------------------------------------------------------
// Two intervals for one title over disjoint windows resolve to different
// pages by τ — the core wayback promise for renamed titles.
// ---------------------------------------------------------------------------
#[test]
fn title_at_tau_two_windows_resolve_distinct_pages() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let conn = meta_conn(&tmp);
    conn.execute(
        "INSERT INTO title_intervals(page_id, ns, normalized_title, start_ts, end_ts)
         VALUES(600, 0, ?1, 1000, 2000)",
        params![b"Moved".to_vec()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO title_intervals(page_id, ns, normalized_title, start_ts, end_ts)
         VALUES(601, 0, ?1, 2000, NULL)",
        params![b"Moved".to_vec()],
    )
    .unwrap();

    assert_eq!(id_at(&inst, "Moved", Some(1500)), Some(600));
    assert_eq!(id_at(&inst, "Moved", Some(2500)), Some(601));
    // exactly at the handoff instant: old window is end-exclusive, new is
    // start-inclusive → the newer page.
    assert_eq!(id_at(&inst, "Moved", Some(2000)), Some(601));
}

// ---------------------------------------------------------------------------
// Fallback path: a title with NO interval rows (an import predating
// interval bookkeeping) resolves via the current title→page mapping.
// Simulated by deleting the importer's interval row but keeping the
// title_id_to_page / page_to_title_id rows.
// ---------------------------------------------------------------------------
#[test]
fn title_at_tau_falls_back_to_current_mapping() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let conn = meta_conn(&tmp);
    conn.execute("DELETE FROM title_intervals WHERE page_id = 100", [])
        .unwrap();

    // No interval rows remain for "Multi Rev", but the current mapping
    // still points at page 100 → fall back to it.
    assert_eq!(id_at(&inst, "Multi Rev", Some(1_000_000)), Some(100));

    // A title with interval rows but none covering τ must NOT fall back.
    conn.execute(
        "INSERT INTO title_intervals(page_id, ns, normalized_title, start_ts, end_ts)
         VALUES(700, 0, ?1, 5000, 6000)",
        params![b"Gated".to_vec()],
    )
    .unwrap();
    assert_eq!(id_at(&inst, "Gated", Some(1000)), None, "has intervals, none cover τ");
}

// ---------------------------------------------------------------------------
// exists_at is a title-table point check (no frame decode); it agrees
// with page_id_by_title_at's presence.
// ---------------------------------------------------------------------------
#[test]
fn exists_at_title_only() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);
    let hist = history_micros(&inst, 100);
    let t20 = hist[1].1;
    let t10 = hist[2].1;

    assert!(inst.exists_at("Multi Rev", Some(t20)).unwrap());
    assert!(inst.exists_at("Multi Rev", None).unwrap());
    assert!(inst.exists_at("Redir One", Some(t20)).unwrap());
    assert!(!inst.exists_at("No Such Page", Some(t20)).unwrap());
    assert!(!inst.exists_at("No Such Page", None).unwrap());
    // exists_at correctness (real start_ts): FALSE before the page's first
    // revision, TRUE from it on.
    assert!(!inst.exists_at("Multi Rev", Some(t10 - 1)).unwrap(),
        "title must not exist before its first revision");
    assert!(inst.exists_at("Multi Rev", Some(t10)).unwrap(),
        "title exists from its first revision (start inclusive)");
}

// ---------------------------------------------------------------------------
// Back-compat: a pre-interval import left a `start_ts = 0` open row. Under
// the same window SQL it must still resolve for any τ ≥ 0 (old depots keep
// working). Simulated by rewriting page 100's interval start_ts to 0.
// ---------------------------------------------------------------------------
#[test]
fn back_compat_start_ts_zero_row_resolves() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let conn = meta_conn(&tmp);
    conn.execute(
        "UPDATE title_intervals SET start_ts = 0 WHERE page_id = 100 AND end_ts IS NULL",
        [],
    )
    .unwrap();

    // A tiny τ (a legacy start_ts=0 row covers all of [0, ∞)).
    assert_eq!(id_at(&inst, "Multi Rev", Some(1)), Some(100));
    assert!(inst.exists_at("Multi Rev", Some(1)).unwrap());
    // τ = 0 exactly resolves (start inclusive at 0).
    assert_eq!(id_at(&inst, "Multi Rev", Some(0)), Some(100));
}

// ---------------------------------------------------------------------------
// resolve_at_with follows #REDIRECT at τ: single hop, two hops, self page
// (non-redirect returns itself), cycle → None, and the hop budget.
// Uses a redirect parser that mirrors wikimak_wikitext::parse_redirect so
// the resolution LOOP (not the renderer) is what's pinned — the serve
// wrapper binds the real parser, compile-checked under --features serve.
// ---------------------------------------------------------------------------
#[test]
fn resolve_at_follows_redirects() {
    use wikimak_wikipedia::asof::resolve_at_with;

    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let r = |title: &str, hops: u32| {
        resolve_at_with(&inst, title, None, hops, parse_redirect_like).expect("resolve_at_with")
    };

    // non-redirect page resolves to itself.
    assert_eq!(r("Multi Rev", 4), Some(100));
    // one hop: Redir One → Multi Rev.
    assert_eq!(r("Redir One", 4), Some(100));
    // two hops: Redir Two → Redir One → Multi Rev.
    assert_eq!(r("Redir Two", 4), Some(100));
    // cycle → None (loop detected).
    assert_eq!(r("Loop A", 4), None);
    // hop budget too small: Redir One needs one hop; budget 0 forbids it.
    assert_eq!(r("Redir One", 0), None);
    // Redir Two needs two hops; budget 1 is short.
    assert_eq!(r("Redir Two", 1), None);
    // missing target resolves to None.
    assert_eq!(r("No Such Page", 4), None);
}

/// Mirrors `wikimak_wikitext::parse_redirect` (preprocess.rs) on bytes:
/// `#REDIRECT [[Target]]`, dropping a `|`-label or `#`-fragment.
fn parse_redirect_like(bytes: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    let t = s.trim_start();
    if t.get(..9)?.to_ascii_lowercase() != "#redirect" {
        return None;
    }
    let rest = &t[9..];
    let open = rest.find("[[")?;
    let close = rest[open + 2..].find("]]")?;
    let inner = &rest[open + 2..open + 2 + close];
    let target = inner.split('|').next().unwrap_or(inner).split('#').next().unwrap_or(inner);
    Some(target.trim().to_string())
}

// ---------------------------------------------------------------------------
// capture_siteinfo now records namespaces; site_config_at surfaces them.
// The importer captures one snapshot per import.
// ---------------------------------------------------------------------------
#[test]
fn site_config_at_carries_namespaces() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let cfg = inst.site_config_at(None).expect("site_config_at").expect("a snapshot");
    assert_eq!(cfg["site_name"], "Test Wiki");
    assert_eq!(cfg["db_name"], "testwiki");

    let namespaces = cfg["namespaces"].as_array().expect("namespaces array");
    // ns 0 (no name) and ns 10 (Template) from the fixture siteinfo.
    let ns10 = namespaces
        .iter()
        .find(|n| n["id"] == 10)
        .expect("Template namespace captured");
    assert_eq!(ns10["canonical"], "Template");
    assert_eq!(ns10["case"], "first-letter");
    // The dump carries no per-namespace aliases → the raw JSON `aliases`
    // stays empty. ns 10's localized name equals its canonical, so no alias
    // is derived (see the ns-4 case for a real derived alias).
    assert!(ns10["aliases"].as_array().unwrap().is_empty());
    assert_eq!(ns10["localized"], "Template");
    assert!(wikimak_wikipedia::asof::namespace_aliases(ns10).is_empty());
    assert!(namespaces.iter().any(|n| n["id"] == 0), "mainspace captured");
}

// ---------------------------------------------------------------------------
// Namespace aliases (browsing plan §7): the dump gives ONE localized name
// per namespace. When it differs from the canonical (Project ns 4 localized
// to "Wikipédia" in the fixture), the canonical fills from the built-in
// MediaWiki map and the localized name becomes a resolvable alias — never
// fabricated, only the dump's own name. Pins capture + `namespace_aliases`
// (the derivation `build_site_config` uses, compile-checked under serve).
// ---------------------------------------------------------------------------
#[test]
fn namespace_localized_becomes_alias() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    let cfg = inst.site_config_at(None).unwrap().unwrap();
    let namespaces = cfg["namespaces"].as_array().unwrap();
    let ns4 = namespaces
        .iter()
        .find(|n| n["id"] == 4)
        .expect("Project namespace captured");

    // Canonical comes from the built-in map; localized is the dump text.
    assert_eq!(ns4["canonical"], "Project", "canonical from built-in map");
    assert_eq!(ns4["localized"], "Wikipédia", "localized from the dump");

    let aliases = wikimak_wikipedia::asof::namespace_aliases(ns4);
    assert!(
        aliases.iter().any(|a| a == "Wikipédia"),
        "localized name resolves as an alias, got {aliases:?}"
    );
    // The canonical is NOT duplicated into aliases.
    assert!(!aliases.iter().any(|a| a == "Project"));

    // Direct unit pins on the pure derivation (no import needed).
    // Same-name namespace → no derived alias.
    let same = serde_json::json!({"canonical": "Template", "localized": "Template", "aliases": []});
    assert!(wikimak_wikipedia::asof::namespace_aliases(&same).is_empty());
    // Old snapshot missing `localized` → tolerated, only explicit aliases.
    let legacy = serde_json::json!({"canonical": "Help"});
    assert!(wikimak_wikipedia::asof::namespace_aliases(&legacy).is_empty());
    // Explicit aliases carried through, plus a differing localized name.
    let both = serde_json::json!({
        "canonical": "Category", "localized": "Kategorie", "aliases": ["CAT"]
    });
    let got = wikimak_wikipedia::asof::namespace_aliases(&both);
    assert!(got.iter().any(|a| a == "CAT"));
    assert!(got.iter().any(|a| a == "Kategorie"));
}

// ---------------------------------------------------------------------------
// Interwiki map (browsing plan §2). Export dumps carry no interwiki data,
// so a freshly-imported instance has an empty interwiki_map table and
// `interwiki_at` returns the built-in SEED (real prefixes, correct $1 URLs).
// When rows ARE captured (a future API/sitematrix source), they take over.
// Pins the fetch-side of the wiring; `build_site_config` (serve) maps these
// rows into SiteConfig.interwiki (compile-checked under serve).
// ---------------------------------------------------------------------------
#[test]
fn interwiki_seed_prefixes_are_real() {
    let seed = wikimak_wikipedia::asof::seed_interwiki();
    let get = |p: &str| seed.iter().find(|e| e.prefix == p).map(|e| e.url.clone());
    assert_eq!(get("w").as_deref(), Some("https://en.wikipedia.org/wiki/$1"));
    assert_eq!(get("wikt").as_deref(), Some("https://en.wiktionary.org/wiki/$1"));
    assert_eq!(get("commons").as_deref(), Some("https://commons.wikimedia.org/wiki/$1"));
    assert_eq!(get("meta").as_deref(), Some("https://meta.wikimedia.org/wiki/$1"));
    assert_eq!(get("d").as_deref(), Some("https://www.wikidata.org/wiki/$1"));
    // Every seed URL is a real https pattern with a $1 placeholder, and
    // NONE is marked local (we mirror none of these).
    for e in &seed {
        assert!(e.url.starts_with("https://") && e.url.contains("$1"), "{e:?}");
        assert!(!e.is_local, "seed prefix must never be local: {e:?}");
    }
}

#[test]
fn interwiki_at_seeds_then_prefers_captured_rows() {
    let tmp = TempDir::new().unwrap();
    let inst = fixture_instance(&tmp);

    use wikimak_wikipedia::asof::interwiki_at;

    // No interwiki rows captured (export dump has none) → the seed.
    let map = interwiki_at(&inst, None).unwrap();
    assert!(map.iter().any(|e| e.prefix == "commons"), "seed used when table empty");
    assert!(!map.iter().any(|e| e.prefix == "es"), "seed has no es prefix");

    // Attach an interwiki row to THE snapshot the importer captured, so the
    // τ selection lands on it; interwiki_at then prefers the captured rows.
    let conn = meta_conn(&tmp);
    let captured_at: i64 = conn
        .query_row(
            "SELECT captured_at FROM siteinfo_snapshots ORDER BY captured_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO interwiki_map(captured_at, prefix, url, is_local)
         VALUES(?1, 'es', 'https://es.wikipedia.org/wiki/$1', 0)",
        params![captured_at],
    )
    .unwrap();

    let map = interwiki_at(&inst, None).unwrap();
    assert!(map.iter().any(|e| e.prefix == "es"), "captured row surfaces");
    // Captured rows REPLACE the seed for that snapshot (not merged).
    assert!(!map.iter().any(|e| e.prefix == "commons"), "captured rows replace seed");
    let es = map.iter().find(|e| e.prefix == "es").unwrap();
    assert_eq!(es.url, "https://es.wikipedia.org/wiki/$1");
    assert!(!es.is_local);
}

// ---------------------------------------------------------------------------
// End-to-end interwiki capture: a dump whose <siteinfo> embeds an
// <interwikimap> is PARSED (mediawiki parser) and its prefixes PERSISTED
// (capture_siteinfo) into interwiki_map, then surfaced by interwiki_at —
// replacing the seed. Pins the parser + capture + read path together, so
// neither can be a stub. `is_local` is stored FALSE even though the dump's
// <iw> carries the same-farm `local` flag (foreign wikis are never local).
// ---------------------------------------------------------------------------
#[test]
fn interwiki_captured_from_dump_interwikimap() {
    const DOC: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>IW Wiki</sitename><dbname>iwwiki</dbname><base>http://x/</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
    <interwikimap>
      <iw prefix="es" url="https://es.wikipedia.org/wiki/$1" />
      <iw prefix="self" url="https://iw.example.org/wiki/$1" local="" />
    </interwikimap>
  </siteinfo>
  <page><title>P</title><ns>0</ns><id>1</id>
    <revision><id>1</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="1" sha1="x" xml:space="preserve">x</text><sha1>x</sha1>
    </revision>
  </page>
</mediawiki>"#;

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 16);
    let mut stream = new_page_stream(Cursor::new(DOC.as_bytes().to_vec()));
    inst.import(&mut stream).expect("import");
    inst.flush().expect("flush");

    let map = wikimak_wikipedia::asof::interwiki_at(&inst, None).unwrap();
    // The parsed prefixes surface, and the seed is replaced (no "commons").
    let es = map.iter().find(|e| e.prefix == "es").expect("es captured from dump");
    assert_eq!(es.url, "https://es.wikipedia.org/wiki/$1");
    assert!(!map.iter().any(|e| e.prefix == "commons"), "captured rows replace the seed");
    // Even a dump-`local` prefix is stored non-local (never a local link
    // for a wiki we don't mirror).
    let selfp = map.iter().find(|e| e.prefix == "self").expect("self captured");
    assert!(!selfp.is_local, "dump local flag must NOT become our is_local");
}

// ---------------------------------------------------------------------------
// site_config_at snapshot selection: max(captured_at ≤ τ), oldest as the
// pre-first-snapshot fallback, newest for None τ. Pinned with synthetic
// snapshot rows so captured_at values are controlled.
// ---------------------------------------------------------------------------
#[test]
fn site_config_at_snapshot_selection() {
    let tmp = TempDir::new().unwrap();
    // Fresh instance, no import: only the synthetic snapshots exist.
    let inst = make_instance(&tmp, 16);
    let conn = meta_conn(&tmp);
    for (at, name) in [(100i64, "old"), (200, "mid"), (300, "new")] {
        let json = format!(r#"{{"site_name":"{name}"}}"#);
        conn.execute(
            "INSERT INTO siteinfo_snapshots(captured_at, json) VALUES(?1, ?2)",
            params![at, json.as_bytes().to_vec()],
        )
        .unwrap();
    }

    let name = |ts| {
        inst.site_config_at(ts)
            .expect("site_config_at")
            .expect("snapshot")["site_name"]
            .as_str()
            .unwrap()
            .to_string()
    };

    assert_eq!(name(None), "new", "None τ → newest");
    assert_eq!(name(Some(250)), "mid", "max captured_at ≤ τ");
    assert_eq!(name(Some(300)), "new", "τ exactly at newest");
    assert_eq!(name(Some(150)), "old", "between old and mid");
    assert_eq!(name(Some(50)), "old", "τ before first snapshot → oldest fallback");

    // No snapshots at all → None.
    let tmp2 = TempDir::new().unwrap();
    let empty = make_instance(&tmp2, 16);
    assert!(empty.site_config_at(None).unwrap().is_none());
    assert!(empty.site_config_at(Some(123)).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// REGRESSION (out-of-order / cross-import revisions): the chain is ordered
// by import-prepend order, NOT by timestamp. A later import supplying a gap
// revision lands at the chain head, so "first record with ts ≤ τ" (and f0
// as "head") returns a non-newest revision. revision_at/page_text_at/
// page_head must select argmax(ts | ts ≤ τ) instead.
//
// Import #1: rev10@2020, rev30@2022. Import #2 (later): the gap rev20@2021.
// Chain becomes [rev20, rev30, rev10] (rev20 prepended at head).
// ---------------------------------------------------------------------------
const OOO_SITEINFO: &str = r#"<siteinfo>
    <sitename>Test Wiki</sitename><dbname>testwiki</dbname><base>http://x/</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>"#;

fn ooo_rev(id: u64, parent: Option<u64>, year: u32, text: &str) -> String {
    let parentid = parent.map(|p| format!("<parentid>{p}</parentid>")).unwrap_or_default();
    format!(
        r#"<revision><id>{id}</id>{parentid}<timestamp>{year}-01-01T00:00:00Z</timestamp>
        <contributor><username>U</username><id>1</id></contributor>
        <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
        <text bytes="5" sha1="x" xml:space="preserve">{text}</text><sha1>x</sha1></revision>"#
    )
}

fn ooo_doc(revs: &str) -> String {
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  {OOO_SITEINFO}
  <page><title>Gap</title><ns>0</ns><id>300</id>{revs}</page>
</mediawiki>"#
    )
}

fn ooo_instance(tmp: &TempDir) -> Instance {
    let inst = make_instance(tmp, 4096);
    // Import #1: rev10 (2020) and rev30 (2022) — a gap at 2021.
    let d1 = ooo_doc(&format!("{}{}", ooo_rev(10, None, 2020, "y2020"), ooo_rev(30, Some(10), 2022, "y2022")));
    let mut s1 = new_page_stream(Cursor::new(d1.into_bytes()));
    inst.import(&mut s1).expect("import #1");
    // Import #2 (later): the gap-filling rev20 (2021). Prepended at the head.
    let d2 = ooo_doc(&ooo_rev(20, Some(10), 2021, "y2021"));
    let mut s2 = new_page_stream(Cursor::new(d2.into_bytes()));
    inst.import(&mut s2).expect("import #2");
    inst.flush().expect("flush");
    inst
}

#[test]
fn revision_at_out_of_order_import_selects_newest_by_time() {
    let tmp = TempDir::new().unwrap();
    let inst = ooo_instance(&tmp);

    // The chain head is the LAST-imported record (rev20), proving order is
    // import-prepend, not timestamp.
    let hist = history_micros(&inst, 300);
    assert_eq!(hist[0].0, 20, "chain head is the last-imported gap revision");
    let t2020 = hist.iter().find(|h| h.0 == 10).unwrap().1;
    let t2021 = hist.iter().find(|h| h.0 == 20).unwrap().1;
    let t2022 = hist.iter().find(|h| h.0 == 30).unwrap().1;

    // Head / None τ must be the newest BY TIME (rev30), not the chain head.
    assert_eq!(rev_id_at(&inst, 300, None), Some(30), "None τ → newest by time");
    assert_eq!(inst.page_head(300).unwrap().unwrap().rev_id, 30, "page_head → newest by time");

    // τ well past the last edit → rev30 (was rev20 with first-in-chain).
    assert_eq!(rev_id_at(&inst, 300, Some(t2022 + 1_000_000)), Some(30));
    // τ exactly at each revision instant.
    assert_eq!(rev_id_at(&inst, 300, Some(t2022)), Some(30));
    assert_eq!(rev_id_at(&inst, 300, Some(t2021)), Some(20));
    assert_eq!(rev_id_at(&inst, 300, Some(t2020)), Some(10));
    // Between 2021 and 2022 → rev20 (rev30 is newer than τ).
    assert_eq!(rev_id_at(&inst, 300, Some(t2022 - 1)), Some(20));
    // Before the first revision → nothing existed yet.
    assert_eq!(rev_id_at(&inst, 300, Some(t2020 - 1)), None);
}

#[test]
fn page_text_at_out_of_order_import_selects_newest_by_time() {
    let tmp = TempDir::new().unwrap();
    let inst = ooo_instance(&tmp);

    let hist = history_micros(&inst, 300);
    let t2021 = hist.iter().find(|h| h.0 == 20).unwrap().1;
    let t2022 = hist.iter().find(|h| h.0 == 30).unwrap().1;

    let text = |ts| inst.page_text_at(300, ts).expect("page_text_at");
    // None τ and past-head τ → the 2022 text, not the last-imported 2021 one.
    assert_eq!(text(None).as_deref(), Some(&b"y2022"[..]));
    assert_eq!(text(Some(t2022 + 1_000_000)).as_deref(), Some(&b"y2022"[..]));
    // Head text accessor agrees.
    assert_eq!(inst.page_head_text(300).unwrap().as_deref(), Some(&b"y2022"[..]));
    // A τ between the gap edit and the newest edit → the gap text.
    assert_eq!(text(Some(t2021)).as_deref(), Some(&b"y2021"[..]));
    assert_eq!(text(Some(t2022 - 1)).as_deref(), Some(&b"y2021"[..]));
}
