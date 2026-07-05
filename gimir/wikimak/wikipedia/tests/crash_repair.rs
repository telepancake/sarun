//! Durability holes closed 2026-07: bookkeeping must never be AHEAD of
//! the depot, and one-process-per-root is a lock, not a convention.
//!
//! Repair: a power loss between a page's sqlite commit and the depot
//! flush can leave `revisions_seen` rows whose frames were lost. The
//! session stamps a durable `dirty` flag before its first import write
//! and clears it after flush; an open that sees `dirty` re-derives each
//! touched page's rows FROM THE CHAIN before trusting them, so the
//! "lost" revision imports again instead of being skipped forever.

mod common;

use wikimak_mediawiki::new_page_stream;

use common::{fixture, make_instance};

fn import_fixture(inst: &wikimak_wikipedia::Instance) -> wikimak_wikipedia::ImportStats {
    let xml = fixture("export_three_pages.xml");
    let mut stream = new_page_stream(std::io::Cursor::new(xml));
    inst.import(&mut stream).expect("import")
}

fn meta_conn(root: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join("meta.db")).unwrap()
}

#[test]
fn dirty_flag_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    // Session 1 imports and "dies" without flushing (drop = process
    // kill for the flag's purposes; meta.db is only readable after the
    // exclusive lock releases on close).
    {
        let inst = make_instance(&tmp, 1024);
        import_fixture(&inst);
    }
    {
        let g = meta_conn(tmp.path());
        let v: i64 = g
            .query_row("SELECT value FROM instance_flags WHERE key='dirty'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1, "unflushed session must leave dirty stamped");
    }
    // Session 2 flushes: clean.
    {
        let inst = make_instance(&tmp, 1024);
        inst.flush().unwrap();
    }
    let g = meta_conn(tmp.path());
    let v: i64 = g
        .query_row("SELECT value FROM instance_flags WHERE key='dirty'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 0, "flush must clear dirty");
}

#[test]
fn suspect_open_repairs_bookkeeping_ahead_of_depot() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let inst = make_instance(&tmp, 1024);
        import_fixture(&inst);
        inst.flush().unwrap();
    }
    // Simulate the power-loss aftermath: a revisions_seen row exists for
    // a revision whose frames never reached the depot, and the session
    // died dirty. (Row for page 1, phantom rev 999.)
    {
        let g = meta_conn(tmp.path());
        g.execute("INSERT INTO revisions_seen(page_id, rev_id) VALUES(1, 999)", []).unwrap();
        g.execute(
            "INSERT OR REPLACE INTO instance_flags(key, value) VALUES('dirty', 1)",
            [],
        ).unwrap();
    }
    // Re-open (suspect) and import a dump where page 1 HAS rev 999:
    // a fresh <revision> prepended inside page 1 (whose closing tag is
    // the first </page> in the fixture).
    let inst = make_instance(&tmp, 1024);
    let xml = String::from_utf8(fixture("export_three_pages.xml")).unwrap();
    let rev999 = "<revision><id>999</id>\
        <timestamp>2024-03-01T00:00:00Z</timestamp>\
        <contributor><username>Alice</username><id>10</id></contributor>\
        <comment>recovered</comment><model>wikitext</model>\
        <format>text/x-wiki</format>\
        <text bytes=\"9\">recovered</text><sha1></sha1></revision>";
    let xml = xml.replacen("</page>", &format!("{rev999}</page>"), 1);
    let mut stream = new_page_stream(std::io::Cursor::new(xml.into_bytes()));
    let stats = inst.import(&mut stream).expect("suspect import");
    assert!(stats.revisions_new >= 1, "phantom watermark must not mask the revision");
    // The recovered revision is REALLY in the depot now.
    let head = inst.page_head(1).unwrap().expect("page 1");
    assert_eq!(head.rev_id, 999);
    assert_eq!(inst.page_head_text(1).unwrap().unwrap(), b"recovered");
    // And the repair rewrote bookkeeping to match the chain exactly: a
    // THIRD import of the same stream dedups everything (no dupes).
    inst.flush().unwrap();
    drop(inst);
    let inst = make_instance(&tmp, 1024);
    let n_before = inst.page_history(1).unwrap().count();
    let xml = fixture("export_three_pages.xml");
    let mut stream = new_page_stream(std::io::Cursor::new(xml));
    let stats = inst.import(&mut stream).unwrap();
    assert_eq!(stats.revisions_new, 0, "clean re-import must fully dedup");
    assert_eq!(inst.page_history(1).unwrap().count(), n_before, "no duplicate records");
}

#[test]
fn second_process_is_locked_out() {
    let tmp = tempfile::tempdir().unwrap();
    let _first = make_instance(&tmp, 1024);
    let root = tmp.path().to_path_buf();
    let second = wikimak_wikipedia::Instance::open(common::cfg(root, 1024));
    match second {
        Err(wikimak_wikipedia::Error::InstanceLocked(_)) => {}
        Err(e) => panic!("expected InstanceLocked, got {e}"),
        Ok(_) => panic!("second open of a live root must fail"),
    }
}
