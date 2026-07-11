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

/// A minimal one-page dump with the given revisions (id, ts, text).
fn one_page_dump(page_id: u64, title: &str, revs: &[(u64, &str, &str)]) -> Vec<u8> {
    let mut r = String::new();
    for (id, ts, text) in revs {
        r.push_str(&format!(
            "<revision><id>{id}</id><timestamp>{ts}</timestamp>\
             <contributor><username>A</username><id>1</id></contributor>\
             <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>\
             <text bytes=\"{}\" xml:space=\"preserve\">{text}</text><sha1></sha1></revision>",
            text.len()
        ));
    }
    format!(
        "<mediawiki xmlns=\"http://www.mediawiki.org/xml/export-0.11/\" version=\"0.11\" \
         xml:lang=\"en\"><siteinfo><sitename>T</sitename><dbname>testwiki</dbname>\
         <namespaces><namespace key=\"0\" case=\"first-letter\" /></namespaces></siteinfo>\
         <page><title>{title}</title><ns>0</ns><id>{page_id}</id>{r}</page></mediawiki>"
    )
    .into_bytes()
}

/// BUG 2 (in-session duplicate risk): `Instance::suspect` is fixed at
/// open, so a SAME-PROCESS re-import after a mid-page error skipped the
/// chain-scan repair — the frames the failed attempt already prepended
/// were live on the chain but their rows had rolled back, so a naive
/// re-import re-prepended and DUPLICATED them. The in-session
/// `errored_pages` flag now routes the affected page back through the
/// same repair a suspect open uses, in-process, before trusting the rows.
#[test]
fn in_session_reimport_after_midpage_error_no_duplicates() {
    // Page id unique to this test: the FAIL knob is process-global, so it
    // must not collide with any other test's page ids.
    const PAGE: u64 = 909_090;
    let tmp = tempfile::tempdir().unwrap();
    let inst = make_instance(&tmp, 1 << 20);

    // Import 1: one revision → an EXISTING chain (so import 2 takes the
    // prepend path, where a mid-page error leaves visible frames).
    let mut s = new_page_stream(std::io::Cursor::new(one_page_dump(
        PAGE,
        "P",
        &[(700, "2024-01-01T00:00:00Z", "one")],
    )));
    inst.import(&mut s).expect("import 1");
    inst.flush().unwrap();

    // Import 2 (SAME process): the page + two more revisions, failing
    // mid-page AFTER rev 701 is prepended — the chain now LEADS the rows
    // the rollback drops (the crash-equivalent state).
    std::env::set_var("WIKIMAK_TEST_FAIL_AFTER_PREPEND", PAGE.to_string());
    let mut s = new_page_stream(std::io::Cursor::new(one_page_dump(
        PAGE,
        "P",
        &[
            (700, "2024-01-01T00:00:00Z", "one"),
            (701, "2024-01-02T00:00:00Z", "two"),
            (702, "2024-01-03T00:00:00Z", "three"),
        ],
    )));
    inst.import(&mut s).expect_err("mid-page failure must be injected");
    std::env::remove_var("WIKIMAK_TEST_FAIL_AFTER_PREPEND");

    // Import 3 (SAME process): the full page again. The errored-page
    // repair re-derives revisions_seen from the chain first, so rev 701
    // (already stored) dedups instead of being prepended a second time.
    let mut s = new_page_stream(std::io::Cursor::new(one_page_dump(
        PAGE,
        "P",
        &[
            (700, "2024-01-01T00:00:00Z", "one"),
            (701, "2024-01-02T00:00:00Z", "two"),
            (702, "2024-01-03T00:00:00Z", "three"),
        ],
    )));
    inst.import(&mut s).expect("import 3");
    inst.flush().unwrap();

    // No duplicate records: every revision present exactly once.
    let hist: Vec<u64> = inst
        .page_history(PAGE)
        .unwrap()
        .map(|e| e.unwrap().meta.rev_id)
        .collect();
    let mut ids = hist.clone();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids, vec![700, 701, 702], "every revision present exactly once");
    assert_eq!(hist.len(), 3, "no duplicate records on the chain (history was {hist:?})");
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
