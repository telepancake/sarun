//! End-to-end `sync` acceptance: an httpmock server stands in for
//! dumps.wikimedia.org (legacy branch), serving a dumpstatus.json whose
//! one part is the `export_three_pages.xml` fixture. Asserts:
//!   - first sync fetches the part and the pages land in the depot;
//!   - the part is watermarked in `parts_seen`;
//!   - a second sync skips the part (no re-fetch: hit counter static)
//!     and imports nothing new;
//!   - a checksum mismatch fails the sync and leaves NO watermark.

mod common;

use httpmock::prelude::*;
use reqwest::blocking::Client;
use sha1::{Digest as _, Sha1};
use tempfile::TempDir;
use wikimak_mediawiki::Config;
use wikimak_wikipedia::sync;

use common::{fixture, make_instance};

const PART: &str = "testwiki-20240601-pages-meta-history1.xml-p1p99";

fn dumpstatus(sha1_hex: &str, size: usize) -> String {
    format!(
        r#"{{"jobs":{{"metahistorybz2dump":{{"status":"done","files":{{
            "{PART}":{{"size":{size},"url":"/testwiki/20240601/{PART}","sha1":"{sha1_hex}"}}
        }}}}}}}}"#
    )
}

fn mount<'a>(server: &'a MockServer, xml: &[u8], sha1_hex: &str) -> httpmock::Mock<'a> {
    // Content-history branch 404s → legacy branch.
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(404);
    });
    server.mock(|when, then| {
        when.method(GET).path("/testwiki/");
        then.status(200).body(r#"<a href="20240601/">20240601/</a>"#);
    });
    let ds = dumpstatus(sha1_hex, xml.len());
    server.mock(move |when, then| {
        when.method(GET).path("/testwiki/20240601/dumpstatus.json");
        then.status(200).body(ds.clone());
    });
    let body = xml.to_vec();
    server.mock(move |when, then| {
        when.method(GET).path(format!("/testwiki/20240601/{PART}"));
        then.status(200).body(body.clone());
    })
}

#[test]
fn sync_fetches_then_skips() {
    let server = MockServer::start();
    let xml = fixture("export_three_pages.xml");
    let sha1_hex = hex::encode(Sha1::digest(&xml));
    let part_mock = mount(&server, &xml, &sha1_hex);

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let client = Client::new();
    let cfg = Config {
        base_url: server.base_url(),
    };

    let (run, s) = sync(&inst, &client, &cfg, "testwiki", |_, _| ()).unwrap();
    assert_eq!(run.date.to_string(), "2024-06-01");
    assert_eq!((s.parts_fetched, s.parts_skipped), (1, 0));
    assert!(s.import.pages >= 3, "pages imported: {}", s.import.pages);
    assert!(s.import.revisions_new > 0);
    // Real effect: page 1's head text is readable from the depot.
    assert!(inst.page_head_text(1).unwrap().is_some());
    assert!(inst.part_seen(PART).unwrap());
    let hits_after_first = part_mock.hits();
    assert!(hits_after_first >= 1);

    // Second pass: watermark short-circuits before any part GET.
    let (_, s2) = sync(&inst, &client, &cfg, "testwiki", |_, _| ()).unwrap();
    assert_eq!((s2.parts_fetched, s2.parts_skipped), (0, 1));
    assert_eq!(s2.import.revisions_new, 0);
    assert_eq!(part_mock.hits(), hits_after_first, "part re-fetched");
}

#[test]
fn checksum_mismatch_fails_and_leaves_no_watermark() {
    let server = MockServer::start();
    let xml = fixture("export_three_pages.xml");
    // Advertise a wrong digest.
    mount(&server, &xml, &"0".repeat(40));

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let client = Client::new();
    let cfg = Config {
        base_url: server.base_url(),
    };

    let err = sync(&inst, &client, &cfg, "testwiki", |_, _| ());
    assert!(err.is_err(), "mismatched sha1 must fail the sync");
    assert!(!inst.part_seen(PART).unwrap(), "no watermark on failure");
}
