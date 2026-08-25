//! Dedup tests. SPEC: re-import is a no-op against revisions already
//! present.

mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;

use common::{fixture, make_instance};

// ---------------------------------------------------------------------------
// revision_dedup_on_reimport
//
// After the second import:
//   - revisions_new == 0
//   - revisions_deduped == first pass's revisions_new
//   - page_head matches first-pass state
//   - The depot's f0 file count and f1 file count are unchanged
//     (proxy for "no new frames written").
// ---------------------------------------------------------------------------

#[test]
fn revision_dedup_on_reimport() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    let body = fixture("export_three_pages.xml");

    let mut stream1 = new_page_stream(Cursor::new(body.clone()));
    let stats1 = instance.import(&mut stream1).expect("first import");
    assert!(stats1.revisions_new > 0);
    instance.flush().expect("flush");

    let f0_count_before = count_files(&tmp.path().join("depot").join("f0"));
    let f1_count_before = count_files(&tmp.path().join("depot").join("f1"));
    let head_before = instance.page_head(2).unwrap();

    let mut stream2 = new_page_stream(Cursor::new(body));
    let stats2 = instance.import(&mut stream2).expect("second import");
    assert_eq!(stats2.revisions_new, 0, "re-import must produce no new revisions");
    assert_eq!(
        stats2.revisions_deduped, stats1.revisions_new,
        "dedup counter must equal first-pass new count"
    );
    instance.flush().expect("flush");

    let head_after = instance.page_head(2).unwrap();
    assert_eq!(head_before, head_after, "page_head unchanged across re-import");

    let f0_count_after = count_files(&tmp.path().join("depot").join("f0"));
    let f1_count_after = count_files(&tmp.path().join("depot").join("f1"));
    // Same file count is the externally-observable proxy for "no new
    // depot frames". With a 1 GiB threshold no roll happens either.
    assert_eq!(f0_count_after, f0_count_before, "no new f0 files");
    assert_eq!(f1_count_after, f1_count_before, "no new f1 files");
}

fn count_files(p: &std::path::Path) -> usize {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().filter(|e| e.path().is_file()).count())
        .unwrap_or(0)
}
