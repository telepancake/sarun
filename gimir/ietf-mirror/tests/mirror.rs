//! End-to-end acceptance for the IETF-drafts mirror: an httpmock server
//! stands in for www.ietf.org, serving `all_id.txt` and per-revision
//! texts. Real-effect assertions:
//!   - update mirrors every listed revision; head/history read back the
//!     exact bytes, newest-first;
//!   - a second update fetches nothing (watermarks; hit counters static);
//!   - a NEW revision appearing in the index fetches ONLY that revision
//!     and becomes the head — incremental by construction;
//!   - a 404'd revision is recorded missing and never re-tried;
//!   - reopen from disk serves the same data (durability).

use httpmock::prelude::*;
use ietf_mirror::{FetchConfig, Mirror, MirrorConfig};
use reqwest::blocking::Client;
use tempfile::TempDir;

const IDX_V1: &str = "\
# header comment\n\
draft-test-alpha-00\t2024-01-01\tActive\n\
draft-test-alpha-01\t2024-02-01\tActive\n\
draft-test-beta-00\t2024-03-01\tExpired\n\
draft-old-lost-00\t1997-01-01\tExpired\n\
not-a-draft-line\n";

fn cfg(server: &MockServer) -> FetchConfig {
    FetchConfig { base_url: server.base_url() }
}

fn mirror(tmp: &TempDir) -> Mirror {
    Mirror::open(MirrorConfig::new(tmp.path().join("m"))).unwrap()
}

fn mock_text<'a>(server: &'a MockServer, docname: &str, body: &str) -> httpmock::Mock<'a> {
    let b = body.to_string();
    let path = format!("/archive/id/{docname}.txt");
    server.mock(move |when, then| {
        when.method(GET).path(path.clone());
        then.status(200).body(b.clone());
    })
}

#[test]
fn second_process_is_locked_out() {
    let tmp = TempDir::new().unwrap();
    let _first = mirror(&tmp);
    match Mirror::open(MirrorConfig::new(tmp.path().join("m"))) {
        Err(ietf_mirror::Error::MirrorLocked(_)) => {}
        Err(e) => panic!("expected MirrorLocked, got {e}"),
        Ok(_) => panic!("second open of a live root must fail"),
    }
}

#[test]
fn update_mirrors_then_increments() {
    let server = MockServer::start();
    let mut idx = server.mock(|when, then| {
        when.method(GET).path("/id/all_id.txt");
        then.status(200).body(IDX_V1);
    });
    let a0 = mock_text(&server, "draft-test-alpha-00", "alpha zero\n");
    let a1 = mock_text(&server, "draft-test-alpha-01", "alpha one\n");
    let b0 = mock_text(&server, "draft-test-beta-00", "beta zero\n");
    // draft-old-lost-00 has no mock → httpmock answers 404.

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    let client = Client::new();

    let s = m.update(&client, &cfg(&server), |_, _| ()).unwrap();
    assert_eq!(s.drafts_new, 3, "alpha, beta, old-lost all allocated");
    assert_eq!(s.revisions_fetched, 3);
    assert_eq!(s.revisions_missing, 1, "the lost draft is missing");

    // Read back: head is the newest, history newest-first, exact bytes.
    let head = m.head("draft-test-alpha").unwrap().unwrap();
    assert_eq!(head.rev, "01");
    assert_eq!(head.text, b"alpha one\n");
    assert_eq!(head.date.as_deref(), Some("2024-02-01"));
    let hist = m.history("draft-test-alpha").unwrap();
    assert_eq!(
        hist.iter().map(|e| e.rev.as_str()).collect::<Vec<_>>(),
        ["01", "00"]
    );
    assert_eq!(hist[1].text, b"alpha zero\n");
    assert_eq!(m.head("draft-test-beta").unwrap().unwrap().text, b"beta zero\n");
    assert!(m.head("draft-old-lost").unwrap().is_none(), "missing draft has no layers");
    assert_eq!(m.drafts().unwrap().len(), 3);

    // Second pass: nothing fetched, no text re-GET (including the 404).
    let (ha, hb) = (a0.hits() + a1.hits(), b0.hits());
    let s2 = m.update(&client, &cfg(&server), |_, _| ()).unwrap();
    assert_eq!(s2.revisions_fetched, 0);
    assert_eq!(s2.revisions_missing, 0, "404 watermarked, not re-tried");
    assert_eq!(s2.revisions_skipped, 4);
    assert_eq!(a0.hits() + a1.hits(), ha, "texts re-fetched");
    assert_eq!(b0.hits(), hb);

    // A new revision appears: only IT is fetched; it becomes head.
    idx.delete();
    server.mock(|when, then| {
        when.method(GET).path("/id/all_id.txt");
        then.status(200)
            .body(format!("{IDX_V1}draft-test-alpha-02\t2024-04-01\tActive\n"));
    });
    let a2 = mock_text(&server, "draft-test-alpha-02", "alpha two\n");
    let s3 = m.update(&client, &cfg(&server), |_, _| ()).unwrap();
    assert_eq!((s3.revisions_fetched, s3.drafts_new), (1, 0));
    assert_eq!(a2.hits(), 1);
    assert_eq!(a0.hits() + a1.hits(), ha, "old revisions untouched");
    let head = m.head("draft-test-alpha").unwrap().unwrap();
    assert_eq!((head.rev.as_str(), head.text.as_slice()), ("02", b"alpha two\n".as_slice()));
    assert_eq!(m.history("draft-test-alpha").unwrap().len(), 3);

    // Durability: a fresh open over the same root serves the same data.
    drop(m);
    let m2 = mirror(&tmp);
    let head = m2.head("draft-test-alpha").unwrap().unwrap();
    assert_eq!(head.rev, "02");
    assert_eq!(m2.history("draft-test-alpha").unwrap().len(), 3);
    assert_eq!(m2.head("draft-test-beta").unwrap().unwrap().text, b"beta zero\n");
}
