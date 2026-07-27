//! Fresh-import layout pinned through public reads: one standalone f0
//! and, when history exists, exactly one sealed cold frame. Mutable f1
//! is update-only.

mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::Instance;

const PAGE_ID: u64 = 7;

fn export_xml(revisions: usize) -> String {
    let mut body = String::new();
    for n in 0..revisions {
        body.push_str(&format!(
            "<revision><id>{}</id><timestamp>2024-01-01T{hour:02}:{minute:02}:00Z</timestamp>\
             <contributor><username>E</username><id>1</id></contributor>\
             <text xml:space=\"preserve\">revision {n}</text></revision>",
            1000 + n,
            hour = n / 60,
            minute = n % 60,
        ));
    }
    format!(
        "<mediawiki xmlns=\"http://www.mediawiki.org/xml/export-0.11/\" version=\"0.11\" xml:lang=\"en\">\
         <siteinfo><sitename>fb</sitename><dbname>fb</dbname><base>x</base><generator>g</generator>\
         <case>first-letter</case><namespaces><namespace key=\"0\" case=\"first-letter\"/></namespaces></siteinfo>\
         <page><title>Forward Page</title><ns>0</ns><id>{PAGE_ID}</id>{body}</page></mediawiki>"
    )
}

fn import(inst: &Instance, revisions: usize) {
    let mut stream = new_page_stream(Cursor::new(export_xml(revisions).into_bytes()));
    let stats = inst.import(&mut stream).unwrap();
    assert_eq!(stats.revisions_new as usize, revisions);
}

#[test]
fn fresh_history_is_one_cold_frame_and_no_f1() {
    const REVISIONS: usize = 80;
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(common::cfg(tmp.path().to_path_buf(), 1024)).unwrap();
    import(&inst, REVISIONS);

    let before = inst.depot_read_counts();
    let ids: Vec<u64> = inst
        .page_history(PAGE_ID)
        .unwrap()
        .map(|entry| entry.unwrap().meta.rev_id)
        .collect();
    let after = inst.depot_read_counts();
    assert_eq!(ids.len(), REVISIONS);
    assert_eq!(ids[0], 1000 + REVISIONS as u64 - 1);
    assert_eq!(
        (after.f0 - before.f0, after.f1 - before.f1, after.cold - before.cold),
        (1, 0, 1),
        "fresh multi-revision page must be f0 + one cold frame"
    );
    assert_eq!(
        std::fs::read_dir(tmp.path().join("depot/f1"))
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        0,
        "fresh import must not create an f1 file"
    );

    let before = inst.depot_read_counts();
    assert_eq!(inst.page_head(PAGE_ID).unwrap().unwrap().rev_id, ids[0]);
    let after = inst.depot_read_counts();
    assert_eq!(
        (after.f0 - before.f0, after.f1 - before.f1, after.cold - before.cold),
        (1, 0, 0),
        "head read must decode f0 only"
    );
}

#[test]
fn single_revision_fresh_page_is_f0_only() {
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(common::cfg(tmp.path().to_path_buf(), 1024)).unwrap();
    import(&inst, 1);

    let before = inst.depot_read_counts();
    assert_eq!(inst.page_history(PAGE_ID).unwrap().count(), 1);
    let after = inst.depot_read_counts();
    assert_eq!(
        (after.f0 - before.f0, after.f1 - before.f1, after.cold - before.cold),
        (1, 0, 0)
    );
}
