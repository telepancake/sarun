//! Live-network acceptance against the real dumps.wikimedia.org.
//! Every test in this file is `#[ignore]`d so it runs only via
//! `cargo test -- --ignored`. Per PHASES.md §W3-Rust-2 "live tests".

use std::io::{Cursor, Read};
use std::time::Duration;

use reqwest::blocking::Client;
use wikimak_mediawiki::{discover, fetch, new_bz2_reader, new_page_stream, site_info, Bz2Options};

// ---------------------------------------------------------------------------
// live_votewiki_discover_fetch_bz2_pagestream
//
// Full pipeline: discover → fetch → bz2 → page_stream.
//
//   * discover("votewiki") returns Ok.
//   * Run has at least one Part.
//   * Streaming consumes ≥ 1 Page; every yielded Page is Ok.
//   * site_info reports db_name == "votewiki".
//
// No retry. 60s timeout. Flake here means dumps.wikimedia.org changed
// shape — fail loud, fix the discover code.
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn live_votewiki_discover_fetch_bz2_pagestream() {
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("build reqwest client");

    let run = discover(&client, "votewiki").expect("discover votewiki: Ok");
    assert!(!run.parts.is_empty(), "votewiki run must have ≥ 1 part");

    let part = &run.parts[0];
    let mut body = fetch(&client, part).expect("fetch first part: Ok");
    // Drain to memory: votewiki parts are small (≲ a few MB) per
    // PHASES "votewiki has very few pages, very small dumps". This
    // bridges fetch's `Box<dyn Read>` (no Send bound per SPEC) to
    // new_bz2_reader's `R: Read + Send` bound via a Cursor.
    let mut compressed = Vec::new();
    body.read_to_end(&mut compressed)
        .expect("fetch read to EOF (checksum verified)");

    let bz2_reader = new_bz2_reader(Cursor::new(compressed), Bz2Options { workers: 1 });
    let mut stream = new_page_stream(bz2_reader);

    let mut count = 0usize;
    while let Some(item) = stream.next() {
        let _page = item.expect("every yielded Page must be Ok");
        count += 1;
        // votewiki is tiny — early-exit once we've proved streaming
        // works to keep test time predictable.
        if count >= 5 {
            break;
        }
    }
    assert!(count >= 1, "expected ≥ 1 page from votewiki");

    let si = site_info(&stream).expect("site_info must be populated after streaming");
    assert_eq!(
        si.db_name, "votewiki",
        "SiteInfo.db_name must be votewiki"
    );
}
