//! Depot-authoritative restart/salvage behavior and root locking.

mod common;

use std::io::Cursor;

use wikimak_mediawiki::new_page_stream;

use common::{fixture, make_instance};

fn import_fixture(inst: &wikimak_wikipedia::Instance) -> wikimak_wikipedia::ImportStats {
    let mut stream = new_page_stream(Cursor::new(fixture("export_three_pages.xml")));
    inst.import(&mut stream).expect("import")
}

#[test]
fn reopen_reads_and_deduplicates_without_relational_revision_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let before = {
        let inst = make_instance(&tmp, 1024);
        let stats = import_fixture(&inst);
        assert!(stats.revisions_new > 0);
        inst.page_history(1).unwrap().count()
    };

    let inst = make_instance(&tmp, 1024);
    assert_eq!(inst.page_history(1).unwrap().count(), before);
    let stats = import_fixture(&inst);
    assert_eq!(stats.revisions_new, 0);
    assert_eq!(inst.page_history(1).unwrap().count(), before);
}

#[test]
fn truncated_later_page_keeps_complete_prefix_and_retry_deduplicates_it() {
    let tmp = tempfile::tempdir().unwrap();
    let inst = make_instance(&tmp, 1024);
    let full = String::from_utf8(fixture("export_three_pages.xml")).unwrap();
    let second_page = full.match_indices("<page>").nth(1).unwrap().0;
    let truncated = full[..second_page + "<page><title>broken".len().min(full.len() - second_page)]
        .as_bytes()
        .to_vec();
    let mut stream = new_page_stream(Cursor::new(truncated));
    assert!(inst.import(&mut stream).is_err());
    let prefix_count = inst.page_history(1).unwrap().count();
    assert!(prefix_count > 0, "complete first page survives source failure");

    let stats = import_fixture(&inst);
    assert!(stats.revisions_deduped as usize >= prefix_count);
    let ids: Vec<u64> = inst
        .page_history(1)
        .unwrap()
        .map(|entry| entry.unwrap().meta.rev_id)
        .collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(ids.len(), unique.len(), "retry must not duplicate salvaged revisions");
}

#[test]
fn second_process_is_locked_out() {
    let tmp = tempfile::tempdir().unwrap();
    let _first = make_instance(&tmp, 1024);
    let root = tmp.path().to_path_buf();
    match wikimak_wikipedia::Instance::open(common::cfg(root, 1024)) {
        Err(wikimak_wikipedia::Error::InstanceLocked(_)) => {}
        Err(error) => panic!("expected InstanceLocked, got {error}"),
        Ok(_) => panic!("second open of a live root must fail"),
    }
}
