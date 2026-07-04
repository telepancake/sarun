//! Durability tests. SPEC §"Crash-safety contract".

mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{Instance, InstanceConfig};

use common::{cfg, fixture, make_instance};

// ---------------------------------------------------------------------------
// flush_then_reopen_round_trip
// ---------------------------------------------------------------------------

#[test]
fn flush_then_reopen_round_trip() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    // First open: import, flush, drop.
    let pre = {
        let instance = Instance::open(cfg(root.clone(), 1024)).expect("open");
        let body = fixture("export_three_pages.xml");
        let mut stream = new_page_stream(Cursor::new(body));
        instance.import(&mut stream).expect("import");
        instance.flush().expect("flush");

        let heads: Vec<_> = [1u64, 2, 3]
            .iter()
            .map(|&id| instance.page_head(id).expect("head"))
            .collect();
        let history_2_text: Vec<Vec<u8>> = instance
            .page_history(2)
            .unwrap()
            .map(|e| (e.unwrap().fetch_text)().unwrap())
            .collect();
        drop(instance);
        (heads, history_2_text)
    };

    // Reopen at the same root.
    let instance2 = Instance::open(cfg(root, 1024)).expect("reopen");
    let heads2: Vec<_> = [1u64, 2, 3]
        .iter()
        .map(|&id| instance2.page_head(id).expect("head"))
        .collect();
    let history_2_text2: Vec<Vec<u8>> = instance2
        .page_history(2)
        .unwrap()
        .map(|e| (e.unwrap().fetch_text)().unwrap())
        .collect();

    assert_eq!(pre.0, heads2, "page_head bytes-identical across reopen");
    assert_eq!(
        pre.1, history_2_text2,
        "page_history text bytes identical across reopen"
    );
}

// ---------------------------------------------------------------------------
// unflushed_drop_may_lose_recent
//
// SPEC: per-page atomicity → committed pages stay, uncommitted vanish.
// We can't reliably distinguish a "committed but pre-flush" page from
// an "uncommitted" one without an explicit fault-injection hook, so
// this test asserts the weaker post-condition:
//   - reopen succeeds without panic
//   - every page that IS readable yields self-consistent head + history
//     (no torn page where the head exists but history is empty or vice
//     versa).
// ---------------------------------------------------------------------------

#[test]
fn unflushed_drop_may_lose_recent() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    {
        let instance = Instance::open(cfg(root.clone(), 1024)).expect("open");
        let body = fixture("export_three_pages.xml");
        let mut stream = new_page_stream(Cursor::new(body));
        instance.import(&mut stream).expect("import");
        // NO flush.
        drop(instance);
    }

    let instance2 = Instance::open(cfg(root, 1024)).expect("reopen must not panic");
    for page_id in [1u64, 2, 3] {
        let head = instance2.page_head(page_id).expect("page_head ok");
        let hist: Vec<_> = instance2
            .page_history(page_id)
            .expect("history ok")
            .collect::<Result<Vec<_>, _>>()
            .expect("history items ok");

        // No-torn-page invariant: head iff history.
        match (head, hist.is_empty()) {
            (Some(_), false) => {}
            (None, true) => {}
            (Some(h), true) => panic!("torn page {page_id}: head {h:?} but no history"),
            (None, false) => panic!("torn page {page_id}: no head but {} history items", hist.len()),
        }
    }
}

// ---------------------------------------------------------------------------
// flush_after_import_acks_durability
//
// Sanity: flush() on a fresh instance after a small import returns Ok.
// (The full round-trip lives in flush_then_reopen_round_trip; this is
// a smoke test for the flush surface alone.)
// ---------------------------------------------------------------------------

#[test]
fn flush_after_import_acks_durability() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);
    let body = fixture("export_three_pages.xml");
    let mut stream = new_page_stream(Cursor::new(body));
    instance.import(&mut stream).expect("import");
    instance.flush().expect("flush must succeed");
}

// Tell the linter we use cfg() / InstanceConfig from common via the
// type re-export.
#[allow(dead_code)]
fn _types(_: InstanceConfig) {}
