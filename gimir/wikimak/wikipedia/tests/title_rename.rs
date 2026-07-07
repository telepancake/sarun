//! Title-rename interval tests — the wayback title time-travel contract
//! (browsing plan §2, import plan §2.4).
//!
//! Run: `cargo test -p wikimak-wikipedia --no-default-features --features
//! fetch`. Every assertion is a concrete input→output check against a real
//! imported instance; a stub that ignores renames would fail them.
//!
//! Scenario: page_id 5 is imported first as "Old Name" (two revisions at
//! T1 < T2), then RE-imported — same page_id — as "New Name" (one revision
//! at T3 > T2), modelling a MediaWiki move. The importer must:
//!
//!   * close the "Old Name" interval at T3 and open "New Name" at T3;
//!   * resolve "Old Name" only within [T1, T3) and "New Name" from T3 on;
//!   * NOT resolve either title before the page's first revision (T1);
//!   * leave the interval rows byte-for-byte unchanged on a full re-import
//!     (idempotence — no spurious churn).

mod common;

use std::io::Cursor;

use rusqlite::Connection;
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;

use common::make_instance;
use wikimak_wikipedia::instance::Instance;

const SITEINFO: &str = r#"<siteinfo>
    <sitename>Rename Wiki</sitename><dbname>renamewiki</dbname><base>http://x/</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>"#;

/// One revision of page 5: `id`, an ISO timestamp at `year`-01-01, `text`.
fn rev(id: u64, year: u32, text: &str) -> String {
    format!(
        r#"<revision><id>{id}</id><timestamp>{year}-01-01T00:00:00Z</timestamp>
        <contributor><username>U</username><id>1</id></contributor>
        <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
        <text bytes="5" sha1="x" xml:space="preserve">{text}</text><sha1>x</sha1></revision>"#
    )
}

/// A full export document: page id 5 with `title` and the given revisions.
fn doc(title: &str, revs: &str) -> String {
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  {SITEINFO}
  <page><title>{title}</title><ns>0</ns><id>5</id>{revs}</page>
</mediawiki>"#
    )
}

fn import(inst: &Instance, xml: String) {
    let mut s = new_page_stream(Cursor::new(xml.into_bytes()));
    inst.import(&mut s).expect("import");
}

/// The "Old Name" dump: revisions 51 (2001) and 52 (2002).
fn old_dump() -> String {
    doc("Old Name", &format!("{}{}", rev(51, 2001, "one"), rev(52, 2002, "two")))
}

/// The "New Name" dump: revision 53 (2003), later than every "Old Name" rev.
fn new_dump() -> String {
    doc("New Name", &rev(53, 2003, "three"))
}

fn meta_conn(tmp: &TempDir) -> Connection {
    Connection::open(tmp.path().join("meta.db")).expect("open meta.db")
}

fn id_at(inst: &Instance, title: &str, ts: i64) -> Option<u64> {
    inst.page_id_by_title_at(title, Some(ts)).expect("page_id_by_title_at")
}

/// `(rev_id, ts_micros)` for page 5, newest-first.
fn history(inst: &Instance) -> Vec<(u64, i64)> {
    inst.page_history(5)
        .expect("history")
        .map(|e| {
            let e = e.expect("entry");
            (e.meta.rev_id, e.meta.ts.timestamp_micros())
        })
        .collect()
}

fn ts_of(inst: &Instance, rev_id: u64) -> i64 {
    history(inst).into_iter().find(|(id, _)| *id == rev_id).expect("rev present").1
}

/// The page-5 interval rows as `(title, start_ts, end_ts)`, ordered by start.
fn intervals(tmp: &TempDir) -> Vec<(String, i64, Option<i64>)> {
    let conn = meta_conn(tmp);
    let mut st = conn
        .prepare(
            "SELECT normalized_title, start_ts, end_ts FROM title_intervals
             WHERE page_id = 5 ORDER BY start_ts",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((
                String::from_utf8(r.get::<_, Vec<u8>>(0)?).unwrap(),
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

// ---------------------------------------------------------------------------
// The core rename contract: distinct titles resolve to the same page over
// disjoint, correctly-bounded τ windows, gated on the first revision.
// ---------------------------------------------------------------------------
#[test]
fn rename_closes_old_and_opens_new_interval() {
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 4096);

    import(&inst, old_dump());
    import(&inst, new_dump());
    inst.flush().expect("flush");

    let t1 = ts_of(&inst, 51); // 2001
    let t2 = ts_of(&inst, 52); // 2002
    let t3 = ts_of(&inst, 53); // 2003
    assert!(t1 < t2 && t2 < t3);

    // Interval rows are exactly: Old [t1, t3), New [t3, ∞).
    assert_eq!(
        intervals(&tmp),
        vec![
            ("Old Name".to_string(), t1, Some(t3)),
            ("New Name".to_string(), t3, None),
        ]
    );

    // "Old Name" resolves to page 5 across (T1..T3), inclusive start.
    assert_eq!(id_at(&inst, "Old Name", t1), Some(5), "at old start (inclusive)");
    assert_eq!(id_at(&inst, "Old Name", t2), Some(5), "mid old window");
    assert_eq!(id_at(&inst, "Old Name", t3 - 1), Some(5), "just before handoff");
    // The old title STOPS resolving at and after the rename instant.
    assert_eq!(id_at(&inst, "Old Name", t3), None, "at handoff (old end exclusive)");
    assert_eq!(id_at(&inst, "Old Name", t3 + 1_000_000), None, "after handoff");
    // Neither title existed before the page's first revision.
    assert_eq!(id_at(&inst, "Old Name", t1 - 1), None, "before first revision");
    assert_eq!(id_at(&inst, "New Name", t1 - 1), None);

    // "New Name" resolves to page 5 from the rename instant on, and not before.
    assert_eq!(id_at(&inst, "New Name", t2), None, "new title absent before handoff");
    assert_eq!(id_at(&inst, "New Name", t3 - 1), None, "still absent just before");
    assert_eq!(id_at(&inst, "New Name", t3), Some(5), "at handoff (new start inclusive)");
    assert_eq!(id_at(&inst, "New Name", t3 + 1_000_000), Some(5), "after handoff");
}

// ---------------------------------------------------------------------------
// Idempotence: re-importing the SAME dumps must not churn interval rows.
// ---------------------------------------------------------------------------
#[test]
fn reimport_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 4096);

    import(&inst, old_dump());
    import(&inst, new_dump());
    inst.flush().expect("flush");
    let before = intervals(&tmp);

    // Re-run the whole history (old then new) a second time.
    import(&inst, old_dump());
    import(&inst, new_dump());
    inst.flush().expect("flush");
    assert_eq!(intervals(&tmp), before, "full re-import must not change intervals");

    // Re-running just the FINAL state again is also a no-op.
    import(&inst, new_dump());
    inst.flush().expect("flush");
    assert_eq!(intervals(&tmp), before, "re-running the head dump must not change intervals");
}

// ---------------------------------------------------------------------------
// First-import single interval: before any rename, one page → one open
// interval starting at its earliest revision (not 0).
// ---------------------------------------------------------------------------
#[test]
fn first_import_opens_interval_at_earliest_revision() {
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 4096);

    import(&inst, old_dump());
    inst.flush().expect("flush");

    let t1 = ts_of(&inst, 51);
    assert_eq!(intervals(&tmp), vec![("Old Name".to_string(), t1, None)]);
    // Gated on the first revision.
    assert_eq!(id_at(&inst, "Old Name", t1 - 1), None);
    assert_eq!(id_at(&inst, "Old Name", t1), Some(5));
}
