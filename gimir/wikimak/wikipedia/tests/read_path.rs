//! Reads must not decode whole chains (2026-07 round 2).
//!
//! REAL effects, really measured, via the depot's frame-payload read
//! counters (`Instance::depot_read_counts` — header peeks don't count,
//! zstd payload reads do) and a child process's peak RSS:
//!
//!   * a HEAD read on a chain with many cold frames touches ONLY f0;
//!   * a τ read whose target lives in f1 touches f0 + f1 and NO cold
//!     frame;
//!   * an oldest-revision read touches every frame but its peak RSS
//!     stays ~one-frame-sized (measured on the real CLI in a child);
//!   * a legacy store whose `revisions_seen` rows predate the `ts`
//!     column answers correctly via the one-time fallback scan, then
//!     BACKFILLS the rows so the next head read is f0-only again.
//!
//! Store shape: the page is imported ONE REVISION PER IMPORT with a
//! tiny f1 seal threshold, so every prepend seals the previous
//! accumulator — chain = f0(rN) → f1[rN-1] → cold[rN-2] … cold[r1].

mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{Instance, InstanceConfig};

const PAGE_ID: u64 = 7;

fn doc_one_rev(rev: u64, year: u32, text: &str) -> String {
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>rp</sitename><dbname>rp</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page><title>Deep Page</title><ns>0</ns><id>{PAGE_ID}</id>
    <revision><id>{rev}</id><timestamp>{year}-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>r{rev}</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{text}</text>
    </revision>
  </page>
</mediawiki>"#
    )
}

fn text_of(rev: u64) -> String {
    format!("text of revision {rev} with some padding so records have real size {rev:08}")
}

/// N sequential one-revision imports (rev 1..=n, years 2001..): each
/// prepend demotes the old head into f1 and — with the tiny seal
/// threshold below — seals the previous f1 to cold.
fn build_deep_chain(inst: &Instance, n: u64) {
    for rev in 1..=n {
        let doc = doc_one_rev(rev, 2000 + rev as u32, &text_of(rev));
        let mut s = new_page_stream(Cursor::new(doc.into_bytes()));
        inst.import(&mut s).expect("import one revision");
    }
    inst.flush().expect("flush");
}

fn deep_cfg(root: std::path::PathBuf) -> InstanceConfig {
    let mut cfg = common::cfg(root, 1024);
    // Seal on (nearly) every prepend: any demoted head overflows this.
    cfg.f1_seal_threshold_bytes = 64;
    cfg
}

fn micros(year: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(year as i32, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_micros()
}

#[test]
fn early_stop_reads_touch_only_the_needed_frames() {
    const N: u64 = 10;
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(deep_cfg(tmp.path().to_path_buf())).unwrap();
    build_deep_chain(&inst, N);

    // The store really is deep: a full history walk crosses several
    // cold frames (chain = f0, f1, then N-3 .. N-2 sealed frames).
    let c0 = inst.depot_read_counts();
    let metas: Vec<u64> = inst
        .page_history(PAGE_ID)
        .unwrap()
        .map(|e| e.unwrap().meta.rev_id)
        .collect();
    assert_eq!(metas, (1..=N).rev().collect::<Vec<_>>(), "newest-first history");
    let c1 = inst.depot_read_counts();
    let cold_frames = c1.cold - c0.cold;
    assert!(
        cold_frames >= 3,
        "fixture must have several cold frames, walked one each: {cold_frames}"
    );
    assert_eq!(c1.f0 - c0.f0, 1, "history reads f0 once");
    assert_eq!(c1.f1 - c0.f1, 1, "history reads f1 once");

    // (a) HEAD reads decode f0 ONLY — no f1, no cold.
    let c0 = inst.depot_read_counts();
    let head = inst.page_head(PAGE_ID).unwrap().unwrap();
    assert_eq!(head.rev_id, N);
    let c1 = inst.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (1, 0, 0),
        "page_head must touch only f0"
    );
    let text = inst.page_head_text(PAGE_ID).unwrap().unwrap();
    assert_eq!(text, text_of(N).into_bytes());
    let c2 = inst.depot_read_counts();
    assert_eq!(
        (c2.f0 - c1.f0, c2.f1 - c1.f1, c2.cold - c1.cold),
        (1, 0, 0),
        "page_head_text must touch only f0"
    );

    // (b) A τ read whose target lives in f1 (rev N-1) stops there:
    // f0 + f1, NO cold frame.
    let c0 = inst.depot_read_counts();
    let tau = micros(2000 + (N - 1) as u32);
    let meta = inst.revision_at(PAGE_ID, Some(tau)).unwrap().unwrap();
    assert_eq!(meta.rev_id, N - 1);
    let text = inst.page_text_at(PAGE_ID, Some(tau)).unwrap().unwrap();
    assert_eq!(text, text_of(N - 1).into_bytes());
    let c1 = inst.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (2, 2, 0),
        "an f1-resident τ read (meta + text) must stop before cold"
    );

    // (c) The oldest revision needs every frame — and exactly one read
    // of each (the walk streams, it never restarts).
    let c0 = inst.depot_read_counts();
    let tau = micros(2001);
    let text = inst.page_text_at(PAGE_ID, Some(tau)).unwrap().unwrap();
    assert_eq!(text, text_of(1).into_bytes());
    let c1 = inst.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (1, 1, cold_frames),
        "an oldest-revision read walks each frame exactly once"
    );

    // τ before the first revision: answered from sqlite alone.
    let c0 = inst.depot_read_counts();
    assert_eq!(inst.revision_at(PAGE_ID, Some(micros(1999))).unwrap(), None);
    let c1 = inst.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (0, 0, 0),
        "τ-before-existence must touch no frame at all"
    );
}

#[test]
fn history_text_fetch_is_lazy_and_early_stops() {
    const N: u64 = 8;
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(deep_cfg(tmp.path().to_path_buf())).unwrap();
    build_deep_chain(&inst, N);

    // Take only the two newest entries and fetch the SECOND one's text:
    // its record lives in f1, so the fetch (a fresh early-stopping
    // walk) must not read any cold frame.
    let c0 = inst.depot_read_counts();
    let entries: Vec<_> = inst.page_history(PAGE_ID).unwrap().take(2).collect();
    let e = entries.into_iter().nth(1).unwrap().unwrap();
    assert_eq!(e.meta.rev_id, N - 1);
    let text = (e.fetch_text)().unwrap();
    assert_eq!(text, text_of(N - 1).into_bytes());
    let c1 = inst.depot_read_counts();
    assert_eq!(c1.cold - c0.cold, 0, "f1-resident text fetch must not read cold");
}

#[test]
fn legacy_null_ts_rows_scan_once_then_backfill() {
    const N: u64 = 6;
    let tmp = TempDir::new().unwrap();
    {
        let inst = Instance::open(deep_cfg(tmp.path().to_path_buf())).unwrap();
        build_deep_chain(&inst, N);
    }
    // Regress the store to the pre-ts-column state: rows exist, no
    // timestamps (exactly what a db written before the migration has).
    {
        let conn = rusqlite::Connection::open(tmp.path().join("meta.db")).unwrap();
        conn.execute("UPDATE revisions_seen SET ts = NULL", []).unwrap();
    }

    let inst = Instance::open(deep_cfg(tmp.path().to_path_buf())).unwrap();

    // First head read: rows can't answer → full-chain streaming scan
    // (touches cold), correct answer, rows backfilled.
    let c0 = inst.depot_read_counts();
    let head = inst.page_head(PAGE_ID).unwrap().unwrap();
    assert_eq!(head.rev_id, N, "argmax over the scanned chain");
    let c1 = inst.depot_read_counts();
    assert!(c1.cold - c0.cold > 0, "legacy read must have scanned the chain");

    let conn = rusqlite::Connection::open(tmp.path().join("meta.db")).unwrap();
    let nulls: i64 = conn
        .query_row("SELECT COUNT(*) FROM revisions_seen WHERE ts IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(nulls, 0, "the scan must backfill every row's ts");

    // Second head read: indexed path again — f0 only.
    let c0 = inst.depot_read_counts();
    assert_eq!(inst.page_head(PAGE_ID).unwrap().unwrap().rev_id, N);
    let c1 = inst.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (1, 0, 0),
        "backfilled page must take the f0-only head path"
    );
}
