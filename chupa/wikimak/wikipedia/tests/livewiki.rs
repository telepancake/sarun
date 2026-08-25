//! Live test against `https://dumps.wikimedia.org`. Gated behind
//! `#[ignore]`. Runs via `cargo test -p wikimak-wikipedia -- --ignored`.

mod common;

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::time::Duration;

use reqwest::blocking::Client;
use tempfile::TempDir;
use wikimak_mediawiki::{discover, fetch, new_bz2_reader, new_page_stream, Bz2Options};

use common::cfg;
use wikimak_wikipedia::Instance;

// ---------------------------------------------------------------------------
// live_votewiki_import_then_read_round_trip
//
// Full pipeline: discover → fetch → bz2 → page_stream → instance.import.
// Then drop, reopen, verify head text + history counts match the dump
// for ≥ 3 pages.
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn live_votewiki_import_then_read_round_trip() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("build reqwest client");

    // Pass 1: discover, fetch, import. Capture ground truth from a
    // separate fresh stream so we can verify against it after reopen.
    let dump_summary: HashMap<u64, (Vec<u8>, usize)> = {
        let instance = Instance::open(cfg(root.clone(), 1_000_000)).expect("open");

        let run = discover(&client, "votewiki").expect("discover");
        let part = run.parts.first().expect("≥ 1 part");

        let mut resp = fetch(&client, part).expect("fetch");
        let mut compressed = Vec::new();
        resp.read_to_end(&mut compressed).expect("drain to EOF");
        let bz2 = new_bz2_reader(Cursor::new(compressed.clone()), Bz2Options { workers: 1 });
        let mut stream = new_page_stream(bz2);

        instance.import(&mut stream).expect("import");
        instance.flush().expect("flush");

        // Re-walk the dump (from a fresh decoder over the buffer we
        // already have) to capture ground truth.
        let bz2_2 = new_bz2_reader(Cursor::new(compressed), Bz2Options { workers: 1 });
        let mut stream2 = new_page_stream(bz2_2);

        let mut by_id: HashMap<u64, (Vec<u8>, usize)> = HashMap::new();
        while let Some(page) = stream2.next() {
            let page = page.expect("page parses");
            if let Some(last) = page.revisions.last() {
                by_id.insert(
                    page.id as u64,
                    (last.text.as_bytes().to_vec(), page.revisions.len()),
                );
            }
        }
        drop(instance);
        by_id
    };

    assert!(
        dump_summary.len() >= 3,
        "votewiki dump must yield ≥ 3 pages; got {}",
        dump_summary.len()
    );

    // Pass 2: reopen at the same root, verify.
    let instance2 = Instance::open(cfg(root, 1_000_000)).expect("reopen");
    let mut checked = 0usize;
    for (page_id, (want_text, want_count)) in dump_summary.iter().take(3) {
        let head = instance2
            .page_head(*page_id)
            .expect("page_head ok")
            .expect("present");
        let hist: Vec<_> = instance2
            .page_history(*page_id)
            .expect("history")
            .collect::<Result<Vec<_>, _>>()
            .expect("history items");
        assert_eq!(
            hist.len(),
            *want_count,
            "page {page_id}: history count must match dump"
        );
        let newest = hist.into_iter().next().expect("≥ 1 revision");
        let text = (newest.fetch_text)().expect("text fetch");
        assert_eq!(
            &text, want_text,
            "page {page_id}: newest text must match dump's last revision"
        );
        assert_eq!(newest.meta.rev_id, head.rev_id);
        checked += 1;
    }
    assert_eq!(checked, 3, "must have checked 3 pages");
}
