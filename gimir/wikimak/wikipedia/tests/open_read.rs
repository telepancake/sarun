//! `Instance::open_read` — the shared-flock read-side open behind
//! pinned attachments, and `Instance::revision_text` — the bounded
//! read-at-rev primitive. REAL effects:
//!
//!   * lock algebra: reader+reader coexist; reader excludes writer and
//!     writer excludes reader (`InstanceLocked` both ways); a second
//!     writer is still excluded;
//!   * read-only discipline: every write API refuses loudly, a legacy
//!     NULL-ts store is answered by scan WITHOUT backfilling the rows,
//!     and a missing root is a loud error that creates nothing;
//!   * the pinned read is bounded, measured by the depot's
//!     frame-payload counters: a pin at the chain head decodes
//!     (f0,f1,cold) = (1,0,0); an f1-resident pin (1,1,0) — never the
//!     whole chain.

mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{Error, Instance, InstanceConfig};

const PAGE: u64 = 7;

fn doc_one_rev(rev: u64, text: &str) -> String {
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>or</sitename><dbname>or</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page><title>Deep Page</title><ns>0</ns><id>{PAGE}</id>
    <revision><id>{rev}</id><timestamp>{}-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>r{rev}</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{text}</text>
    </revision>
  </page>
</mediawiki>"#,
        2000 + rev
    )
}

fn text_of(rev: u64) -> String {
    format!("text of revision {rev} with some padding so records have real size {rev:08}")
}

/// Deep-chain config: seal on (nearly) every prepend, so the store is
/// f0(rN) → f1[rN-1] → cold[rN-2] … cold[r1].
fn deep_cfg(root: std::path::PathBuf) -> InstanceConfig {
    let mut cfg = common::cfg(root, 1024);
    cfg.f1_seal_threshold_bytes = 64;
    cfg
}

/// N one-revision imports under a writer that is then DROPPED (lock
/// released) so read-side opens can follow.
fn build_store(tmp: &TempDir, n: u64) {
    let inst = Instance::open(deep_cfg(tmp.path().to_path_buf())).unwrap();
    for rev in 1..=n {
        let mut s = new_page_stream(Cursor::new(doc_one_rev(rev, &text_of(rev)).into_bytes()));
        inst.import(&mut s).expect("import one revision");
    }
    inst.flush().expect("flush");
}

fn open_read(tmp: &TempDir) -> wikimak_wikipedia::Result<Instance> {
    Instance::open_read(deep_cfg(tmp.path().to_path_buf()))
}

#[test]
fn lock_algebra_shared_shared_ok_writer_excluded_both_ways() {
    let tmp = TempDir::new().unwrap();
    build_store(&tmp, 3);

    // Two concurrent readers coexist (shared/shared).
    let r1 = open_read(&tmp).expect("first reader");
    let r2 = open_read(&tmp).expect("second reader alongside the first");
    assert_eq!(r1.page_head(PAGE).unwrap().unwrap().rev_id, 3);
    assert_eq!(r2.page_head(PAGE).unwrap().unwrap().rev_id, 3);

    // A writer is excluded while any reader holds the root.
    match Instance::open(deep_cfg(tmp.path().to_path_buf())) {
        Err(Error::InstanceLocked(_)) => {}
        Err(e) => panic!("writer under readers must be InstanceLocked, got {e}"),
        Ok(_) => panic!("writer under readers must be InstanceLocked, got Ok"),
    }
    drop(r1);
    match Instance::open(deep_cfg(tmp.path().to_path_buf())) {
        Err(Error::InstanceLocked(_)) => {}
        Err(e) => panic!("one reader left still excludes the writer, got {e}"),
        Ok(_) => panic!("one reader left still excludes the writer, got Ok"),
    }
    drop(r2);

    // Readers gone: the writer opens; readers are now excluded; and a
    // second writer is still excluded (exclusive stayed exclusive).
    let w = Instance::open(deep_cfg(tmp.path().to_path_buf())).expect("writer after readers");
    match open_read(&tmp) {
        Err(Error::InstanceLocked(_)) => {}
        Err(e) => panic!("reader under a writer must be InstanceLocked, got {e}"),
        Ok(_) => panic!("reader under a writer must be InstanceLocked, got Ok"),
    }
    match Instance::open(deep_cfg(tmp.path().to_path_buf())) {
        Err(Error::InstanceLocked(_)) => {}
        Err(e) => panic!("second writer must be InstanceLocked, got {e}"),
        Ok(_) => panic!("second writer must be InstanceLocked, got Ok"),
    }
    drop(w);
    open_read(&tmp).expect("reader after the writer released");
}

#[test]
fn open_read_refuses_writes_and_never_creates() {
    let tmp = TempDir::new().unwrap();
    build_store(&tmp, 2);
    let r = open_read(&tmp).unwrap();

    let mut s = new_page_stream(Cursor::new(doc_one_rev(9, "x").into_bytes()));
    assert!(matches!(r.import(&mut s), Err(Error::ReadOnly("import"))));
    assert!(matches!(r.flush(), Err(Error::ReadOnly("flush"))));
    assert!(matches!(r.collect(), Err(Error::ReadOnly("collect"))));
    assert!(matches!(r.mark_part_seen("p.xml", None), Err(Error::ReadOnly("mark_part_seen"))));
    // …and reads still work on the same handle.
    assert_eq!(r.page_head_text(PAGE).unwrap().unwrap(), text_of(2).into_bytes());

    // A root with no instance: loud error, nothing created.
    let ghost = tmp.path().join("nothing-here");
    match Instance::open_read(deep_cfg(ghost.clone())) {
        Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        Err(e) => panic!("open_read of a missing root must be loud NotFound, got {e}"),
        Ok(_) => panic!("open_read of a missing root must be loud, got Ok"),
    }
    assert!(!ghost.exists(), "open_read must never create a root");
}

/// A legacy store (rows predate the `ts` column values) is answered
/// correctly read-side via the chain scan, but the rows are NOT
/// backfilled — that write belongs to the exclusive-lock holder.
#[test]
fn read_only_scan_answers_but_never_backfills() {
    let tmp = TempDir::new().unwrap();
    build_store(&tmp, 4);
    {
        let conn = rusqlite::Connection::open(tmp.path().join("meta.db")).unwrap();
        conn.execute("UPDATE revisions_seen SET ts = NULL", []).unwrap();
    }
    let nulls = || -> i64 {
        let conn = rusqlite::Connection::open(tmp.path().join("meta.db")).unwrap();
        conn.query_row("SELECT COUNT(*) FROM revisions_seen WHERE ts IS NULL", [], |r| r.get(0))
            .unwrap()
    };

    let r = open_read(&tmp).unwrap();
    assert_eq!(r.page_head(PAGE).unwrap().unwrap().rev_id, 4, "scan answers");
    drop(r);
    assert!(nulls() > 0, "read-only scan must not backfill ts rows");

    // The writer's first read still backfills as before.
    let w = Instance::open(deep_cfg(tmp.path().to_path_buf())).unwrap();
    assert_eq!(w.page_head(PAGE).unwrap().unwrap().rev_id, 4);
    drop(w);
    assert_eq!(nulls(), 0, "writer-side scan backfills");
}

/// The bounded pinned read, measured: `revision_text` early-stops on
/// the newest-first walk — a pinned-head read touches ONLY f0; an
/// f1-resident pin touches f0 + f1 and NO cold frame, on a store whose
/// history spans several cold frames.
#[test]
fn pinned_read_is_bounded_by_the_pin() {
    const N: u64 = 10;
    let tmp = TempDir::new().unwrap();
    build_store(&tmp, N);
    let r = open_read(&tmp).unwrap();

    // The store really is deep (several cold frames below f0/f1).
    let c0 = r.depot_read_counts();
    assert_eq!(r.revision_text(PAGE, 1).unwrap().unwrap(), text_of(1).into_bytes());
    let c1 = r.depot_read_counts();
    assert!(c1.cold - c0.cold >= 3, "fixture depth: oldest pin crossed {} cold frames", c1.cold - c0.cold);

    // Pin at the chain head: f0 only.
    let c0 = r.depot_read_counts();
    assert_eq!(r.revision_text(PAGE, N).unwrap().unwrap(), text_of(N).into_bytes());
    let c1 = r.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (1, 0, 0),
        "pinned-head read must touch only f0"
    );

    // Pin one below the head (f1-resident): f0 + f1, no cold.
    let c0 = r.depot_read_counts();
    assert_eq!(r.revision_text(PAGE, N - 1).unwrap().unwrap(), text_of(N - 1).into_bytes());
    let c1 = r.depot_read_counts();
    assert_eq!(
        (c1.f0 - c0.f0, c1.f1 - c0.f1, c1.cold - c0.cold),
        (1, 1, 0),
        "an f1-resident pin must stop before cold"
    );

    // No such revision: a definitive miss (the whole chain was checked).
    assert_eq!(r.revision_text(PAGE, 999).unwrap(), None);
    assert_eq!(r.revision_text(4242, 1).unwrap(), None, "no such page");
}
