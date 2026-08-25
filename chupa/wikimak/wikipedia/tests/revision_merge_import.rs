mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::Instance;

fn document(revisions: &[(u64, &str, &str)]) -> String {
    let mut body = String::new();
    for (id, timestamp, text) in revisions {
        body.push_str(&format!(
            "<revision><id>{id}</id><timestamp>{timestamp}</timestamp>\
             <contributor><username>A</username><id>1</id></contributor>\
             <text xml:space=\"preserve\">{text}</text></revision>"
        ));
    }
    format!(
        "<mediawiki xmlns=\"http://www.mediawiki.org/xml/export-0.11/\" version=\"0.11\" xml:lang=\"en\">\
         <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>\
         <case>first-letter</case><namespaces><namespace key=\"0\" case=\"first-letter\"/></namespaces></siteinfo>\
         <page><title>P</title><ns>0</ns><id>1</id>{body}</page></mediawiki>"
    )
}

fn import(inst: &Instance, revisions: &[(u64, &str, &str)]) -> wikimak_wikipedia::ImportStats {
    let mut stream = new_page_stream(Cursor::new(document(revisions).into_bytes()));
    inst.import(&mut stream).unwrap()
}

fn ids(inst: &Instance) -> Vec<u64> {
    inst.page_history(1)
        .unwrap()
        .map(|entry| entry.unwrap().meta.rev_id)
        .collect()
}

#[test]
fn rerun_interleaved_and_conflicting_revisions_use_the_authoritative_chain() {
    let tmp = TempDir::new().unwrap();
    let inst = Instance::open(common::cfg(tmp.path().to_path_buf(), 16)).unwrap();

    let first = import(
        &inst,
        &[
            (1, "2020-01-01T00:00:00Z", "one"),
            (3, "2022-01-01T00:00:00Z", "three"),
        ],
    );
    assert_eq!(first.revisions_new, 2);
    assert_eq!(ids(&inst), [3, 1]);
    let counts = inst.depot_read_counts();
    let _ = ids(&inst);
    let after = inst.depot_read_counts();
    assert_eq!(after.f1 - counts.f1, 0, "fresh page must not have f1");
    assert_eq!(after.cold - counts.cold, 1, "fresh history is one cold frame");

    let gap = import(&inst, &[(2, "2021-01-01T00:00:00Z", "two")]);
    assert_eq!((gap.revisions_new, gap.revisions_deduped), (1, 0));
    assert_eq!(ids(&inst), [3, 2, 1]);

    let rerun = import(&inst, &[(2, "2021-01-01T00:00:00Z", "two")]);
    assert_eq!((rerun.revisions_new, rerun.revisions_deduped), (0, 1));
    assert_eq!(ids(&inst), [3, 2, 1]);

    let conflict = import(&inst, &[(2, "2030-01-01T00:00:00Z", "conflicting two")]);
    assert_eq!(conflict.revision_conflicts, 1);
    assert_eq!(inst.revision_text(1, 2).unwrap().unwrap(), b"two");
    let corrections = inst.revision_corrections(1).unwrap();
    assert_eq!(corrections.len(), 1);
    assert_eq!(corrections[0].revision_id, 2);
    let (_meta, text) =
        wikimak_wikipedia::revision::decode_revision(&corrections[0].incoming_record).unwrap();
    assert_eq!(text, b"conflicting two");

    let conflict_rerun = import(&inst, &[(2, "2030-01-01T00:00:00Z", "conflicting two")]);
    assert_eq!(conflict_rerun.revision_conflicts, 0);
    assert_eq!(inst.revision_corrections(1).unwrap().len(), 1);
}
