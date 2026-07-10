//! The sharded title dictionary is WIRED into reads (work order
//! "wire the designed dictionary"): exact title resolution walks the
//! one fnv-picked strpool shard, substring/pages listing scans all
//! shards in parallel, and every sqlite hop is keyed by the dense
//! `title_id` — never by scanning `title_intervals.normalized_title`.
//!
//! Every assertion here is instrumented against REAL effects:
//! `Instance::title_scan_counts` (strpool per-shard walk counters)
//! pins how many shards each read touched, and the migration test
//! rebuilds a genuinely legacy meta.db and watches the backfill fence
//! repopulate it.

mod common;

use std::io::Cursor;

use rusqlite::Connection;
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::Instance;

const SHARDS: u32 = 4;

/// Instance with a 4-shard titles pool (common::cfg uses 1 — sharding
/// is the point here).
fn sharded_instance(tmp: &TempDir) -> Instance {
    let mut cfg = common::cfg(tmp.path().to_path_buf(), 4096);
    cfg.title_shard_count = SHARDS;
    Instance::open(cfg).expect("open sharded instance")
}

/// One-revision page element. All pages share one timestamp so τ math
/// stays trivial.
fn page_xml(title: &str, id: u64, body: &str) -> String {
    format!(
        r#"  <page>
    <title>{title}</title><ns>0</ns><id>{id}</id>
    <revision>
      <id>{rev}</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{body}</text><sha1>aa</sha1>
    </revision>
  </page>
"#,
        rev = id * 10 + 1,
    )
}

fn wrap_dump(pages: &str) -> String {
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>testwiki</dbname><base>http://x/</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
{pages}</mediawiki>"#
    )
}

/// τ of the shared fixture timestamp.
fn tau() -> i64 {
    chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
        .unwrap()
        .timestamp_micros()
}

fn import_titles(inst: &Instance, titles: &[(String, u64)]) {
    let mut pages = String::new();
    for (t, id) in titles {
        pages.push_str(&page_xml(t, *id, "body"));
    }
    let doc = wrap_dump(&pages);
    let mut stream = new_page_stream(Cursor::new(doc.into_bytes()));
    inst.import(&mut stream).expect("import");
    inst.flush().expect("flush");
}

fn fixture_titles(n: u64) -> Vec<(String, u64)> {
    (1..=n).map(|i| (format!("Topic Page {i}"), i)).collect()
}

// ---------------------------------------------------------------------------
// exact_lookup_touches_exactly_one_shard
//
// An exact title lookup (τ and head form) walks the fnv-picked shard
// ONCE, no other shard ever; the repeat lookup is served from the
// bounded shard cache with ZERO pool walks. This also pins the
// read-side fnv/shard-picker parity with import: a divergence would
// look up the wrong shard and miss.
// ---------------------------------------------------------------------------
#[test]
fn exact_lookup_touches_exactly_one_shard() {
    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);
    import_titles(&inst, &fixture_titles(32));

    let base = inst.title_scan_counts();
    assert_eq!(base.len(), SHARDS as usize);

    let got = inst
        .page_id_by_title_at("Topic Page 7", Some(tau()))
        .expect("lookup");
    assert_eq!(got, Some(7), "dictionary lookup resolves the right page");

    let after = inst.title_scan_counts();
    let delta: Vec<u64> = after.iter().zip(&base).map(|(a, b)| a - b).collect();
    assert_eq!(
        delta.iter().sum::<u64>(),
        1,
        "exact lookup walked exactly ONE shard, once (delta {delta:?})"
    );

    // Same title again — cache hit, no pool I/O at all.
    let got = inst.page_id_by_title_at("Topic Page 7", Some(tau())).expect("lookup");
    assert_eq!(got, Some(7));
    // Head-form (τ = None) exact resolution of a title in the SAME
    // shard is a probe of the cached map too.
    let head = inst.page_id_by_title_at("Topic Page 7", None).expect("lookup");
    assert_eq!(head, Some(7));
    let after2 = inst.title_scan_counts();
    assert_eq!(after2, after, "repeat lookups re-walk nothing");

    // A MISS in an already-cached shard also costs zero walks: at τ,
    // an unknown title resolves to None without any pool scan.
    let miss = inst.page_id_by_title_at("Topic Page 7 (disambiguation)", Some(tau()));
    let miss_delta: u64 = inst
        .title_scan_counts()
        .iter()
        .zip(&after2)
        .map(|(a, b)| a - b)
        .sum();
    assert_eq!(miss.expect("lookup"), None);
    assert!(
        miss_delta <= 1,
        "a τ miss costs at most the one fnv-picked shard walk (got {miss_delta})"
    );
}

// ---------------------------------------------------------------------------
// by_id_title_recovery_walks_no_shard
//
// The reverse lookup (`Instance::page_current_title`, the engine's
// attach-by-id name recovery) is pure indexed sqlite — open interval →
// title_id → title_id_to_page — and NEVER touches the strpool: zero
// shard walks, where the pool-wide listing it replaced walked every
// shard.
// ---------------------------------------------------------------------------
#[test]
fn by_id_title_recovery_walks_no_shard() {
    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);
    import_titles(&inst, &fixture_titles(32));

    let base = inst.title_scan_counts();
    assert_eq!(
        inst.page_current_title(7).expect("lookup").as_deref(),
        Some("Topic Page 7"),
        "by-id recovery names the current title"
    );
    assert_eq!(
        inst.page_current_title(31).expect("lookup").as_deref(),
        Some("Topic Page 31"),
    );
    assert_eq!(
        inst.page_current_title(999).expect("lookup"),
        None,
        "unknown page is a quiet None"
    );
    assert_eq!(
        inst.title_scan_counts(),
        base,
        "page-id → title walked ZERO strpool shards (indexed hops only)"
    );
}

// ---------------------------------------------------------------------------
// substring_search_touches_all_shards
//
// The pages listing / substring search is a parallel scan: one walk of
// EVERY shard per pass, results in byte order with the old
// case-insensitive lossy-lowercase filter semantics.
// ---------------------------------------------------------------------------
#[test]
fn substring_search_touches_all_shards() {
    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);
    let titles = fixture_titles(32);
    import_titles(&inst, &titles);

    let base = inst.title_scan_counts();
    // Case-insensitive: needle "topic page 1" (lowercase) must match
    // "Topic Page 1", "Topic Page 10".."19" — the exact old semantics.
    let hits = inst.pages(Some("topic page 1"), 100).expect("pages");
    let after = inst.title_scan_counts();
    let delta: Vec<u64> = after.iter().zip(&base).map(|(a, b)| a - b).collect();
    assert_eq!(
        delta,
        vec![1; SHARDS as usize],
        "substring search walks EVERY shard exactly once"
    );

    let mut expected: Vec<(u64, String)> = titles
        .iter()
        .filter(|(t, _)| t.to_lowercase().contains("topic page 1"))
        .map(|(t, id)| (*id, t.clone()))
        .collect();
    expected.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(hits, expected, "byte-ordered, case-insensitive filter parity");
}

// ---------------------------------------------------------------------------
// allpages_is_pool_ordered_without_a_text_sort
//
// The unfiltered listing comes off the SAME bounded shard scan (no
// sqlite ORDER BY over the redundant title copy) in normalized-title
// byte order, and the `limit` window is honored across passes.
// ---------------------------------------------------------------------------
#[test]
fn allpages_is_pool_ordered_without_a_text_sort() {
    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);
    let titles = fixture_titles(32);
    import_titles(&inst, &titles);

    let mut expected: Vec<(u64, String)> =
        titles.iter().map(|(t, id)| (*id, t.clone())).collect();
    expected.sort_by(|a, b| a.1.cmp(&b.1));

    let all = inst.pages(None, 1000).expect("pages");
    assert_eq!(all, expected, "full listing in byte order");

    // A limit smaller than the corpus forces the windowed multi-pass
    // top-K path; the prefix must be identical.
    let first5 = inst.pages(None, 5).expect("pages");
    assert_eq!(first5, expected[..5].to_vec(), "bounded window keeps the order");
}

// ---------------------------------------------------------------------------
// unmapped_rows_still_resolve_and_list
//
// Interval rows the dictionary does not know (written by an external
// writer, e.g. the synthetic fixtures in tests/asof.rs) stay readable
// through the O(1)-guarded compatibility branch: τ window, head
// resolution, and the pages listing all still see them.
// ---------------------------------------------------------------------------
#[test]
fn unmapped_rows_still_resolve_and_list() {
    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);
    import_titles(&inst, &fixture_titles(4));

    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    conn.execute(
        "INSERT INTO title_intervals(page_id, ns, normalized_title, start_ts, end_ts)
         VALUES(900, 0, ?1, 1000, NULL)",
        rusqlite::params![b"Zz Synthetic Only".to_vec()],
    )
    .unwrap();
    // An external writer supplies no title_id (and the dictionary has
    // no entry to derive one from) — the row is unmapped, which is
    // exactly the state under test.
    let unmapped: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM title_intervals WHERE title_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unmapped, 1, "synthetic row stays outside the dictionary");

    assert_eq!(
        inst.page_id_by_title_at("Zz Synthetic Only", Some(2000)).unwrap(),
        Some(900),
        "τ window resolves an unmapped row"
    );
    assert_eq!(
        inst.page_id_by_title_at("Zz Synthetic Only", None).unwrap(),
        Some(900),
        "head resolution reaches an unmapped row"
    );
    let all = inst.pages(None, 100).unwrap();
    assert_eq!(
        all.last().map(|(id, t)| (*id, t.as_str())),
        Some((900, "Zz Synthetic Only")),
        "listing includes and byte-orders the unmapped row"
    );
    assert_eq!(
        inst.page_current_title(900).unwrap().as_deref(),
        Some("Zz Synthetic Only"),
        "by-id recovery reaches an unmapped row via the compat branch"
    );
}

// ---------------------------------------------------------------------------
// legacy_meta_db_backfills_title_id_on_open
//
// A meta.db from before the title_id column existed (rebuilt here for
// real: table recreated without the column, its indexes gone with it)
// gets column + indexes + a full backfill at the next open — the same
// lazy-migration fence as revisions_seen.ts.
// ---------------------------------------------------------------------------
#[test]
fn legacy_meta_db_backfills_title_id_on_open() {
    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);
    import_titles(&inst, &fixture_titles(8));
    drop(inst); // release the root flock

    {
        let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE ti_legacy (
                 page_id INTEGER NOT NULL,
                 ns INTEGER NOT NULL,
                 normalized_title BLOB NOT NULL,
                 start_ts INTEGER NOT NULL,
                 end_ts INTEGER,
                 PRIMARY KEY(page_id, start_ts)
             ) WITHOUT ROWID;
             INSERT INTO ti_legacy
                 SELECT page_id, ns, normalized_title, start_ts, end_ts
                 FROM title_intervals;
             DROP TABLE title_intervals; -- drops its triggers and indexes
             ALTER TABLE ti_legacy RENAME TO title_intervals;",
        )
        .expect("rebuild legacy title_intervals");
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(title_intervals)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect();
        assert!(!cols.contains(&"title_id".to_string()), "legacy shape verified");
    }

    let inst = sharded_instance(&tmp); // reopen → migrate + backfill
    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    let unmapped: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM title_intervals WHERE title_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unmapped, 0, "open backfilled every legacy row");
    assert_eq!(
        inst.page_id_by_title_at("Topic Page 3", Some(tau())).unwrap(),
        Some(3),
        "dictionary reads work against the migrated db"
    );
}

// ---------------------------------------------------------------------------
// retitle_in_place_rekeys_the_interval
//
// Import's full-history-re-export rename path UPDATEs the open
// interval's title in place (import.rs `ensure_title`), carrying the
// new title's dictionary id with it, so dictionary reads see the NEW
// title and drop the old one.
// ---------------------------------------------------------------------------
#[test]
fn retitle_in_place_rekeys_the_interval() {
    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);

    // v1: one revision under "Old Name".
    let v1 = wrap_dump(&page_xml("Old Name", 50, "v1"));
    let mut s = new_page_stream(Cursor::new(v1.into_bytes()));
    inst.import(&mut s).expect("import v1");

    // v2: full re-export under "New Name" — same first revision plus a
    // newer one, so earliest_ts is NOT later than the open interval →
    // the retitle-in-place path.
    let v2 = wrap_dump(
        r#"  <page>
    <title>New Name</title><ns>0</ns><id>50</id>
    <revision>
      <id>501</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">v1</text><sha1>aa</sha1>
    </revision>
    <revision>
      <id>502</id><parentid>501</parentid><timestamp>2021-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">v2</text><sha1>bb</sha1>
    </revision>
  </page>
"#,
    );
    let mut s = new_page_stream(Cursor::new(v2.into_bytes()));
    inst.import(&mut s).expect("import v2");
    inst.flush().expect("flush");

    let t2 = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .timestamp_micros();
    assert_eq!(
        inst.page_id_by_title_at("New Name", Some(t2)).unwrap(),
        Some(50),
        "retitled interval resolves under the new title"
    );
    assert_eq!(
        inst.page_id_by_title_at("Old Name", Some(t2)).unwrap(),
        None,
        "the renamed-away title stops resolving"
    );

    // The real effect: the open interval row carries the dictionary id
    // of "New Name", and nothing is left unmapped.
    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    let (row_tid, dict_tid): (i64, i64) = conn
        .query_row(
            "SELECT i.title_id,
                    (SELECT title_id FROM title_id_to_page
                      WHERE ns = 0 AND normalized_title = ?1)
             FROM title_intervals i
             WHERE i.page_id = 50 AND i.end_ts IS NULL",
            rusqlite::params![b"New Name".to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row_tid, dict_tid, "retitle UPDATE carried the new title_id");
    assert_eq!(
        inst.page_current_title(50).unwrap().as_deref(),
        Some("New Name"),
        "by-id recovery follows the retitled open interval"
    );
}

// ---------------------------------------------------------------------------
// render_walks_each_touched_shard_at_most_once
//
// A τ render of a page with many links resolves its whole link set
// through the bounded shard cache: each touched shard is decompressed
// at most ONCE for the render, and a repeat render re-walks nothing.
// ---------------------------------------------------------------------------
#[cfg(feature = "serve")]
#[test]
fn render_walks_each_touched_shard_at_most_once() {
    use wikimak_wikipedia::asof::AsOfView;
    use wikimak_wikitext::{render, PageStore, RenderOptions, Title};

    let tmp = TempDir::new().unwrap();
    let inst = sharded_instance(&tmp);
    let mut titles = fixture_titles(24);
    titles.push(("Hub".to_string(), 100));
    import_titles(&inst, &titles);

    let mut body = String::from("Hub of everything.\n");
    for i in 1..=24 {
        body.push_str(&format!("* [[Topic Page {i}]]\n"));
    }

    let render_at = |ts: Option<i64>| -> String {
        let view = AsOfView::new(&inst, ts).expect("view");
        let title = Title::parse("Hub", view.site());
        let opts = RenderOptions {
            invoker: None,
            media: None,
            link_prefix: "/wiki/".into(),
            asof_query: String::new(),
        };
        render(&view, &title, &body, &opts).html
    };

    let base = inst.title_scan_counts();
    let html = render_at(Some(tau()));
    assert!(
        html.contains("Topic Page 24"),
        "render resolved its links:\n{html}"
    );
    let after = inst.title_scan_counts();
    let delta: Vec<u64> = after.iter().zip(&base).map(|(a, b)| a - b).collect();
    assert!(
        delta.iter().all(|&d| d <= 1),
        "each touched shard decompressed at most once per render (delta {delta:?})"
    );
    assert!(delta.iter().sum::<u64>() >= 1, "the render did touch the pool");

    // Second render: the shard cache is warm, zero pool walks.
    let _ = render_at(Some(tau()));
    assert_eq!(
        inst.title_scan_counts(),
        after,
        "repeat render is served entirely from the shard cache"
    );
}
