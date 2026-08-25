//! `DraftReadout` — the RO-attachment readout over one PINNED draft
//! revision (ATTACH-CONVERGENCE.md chip 2). Shape contract: exactly
//! one leaf `<draft>-<rev>.txt` at the root, bytes frozen at the pin.
//! Locking contract: construction and idle attachments hold NOTHING;
//! the first access takes the shared lock only for the decode, so an
//! attached draft never blocks an update — and a pinned readout keeps
//! serving the pinned bytes after the head moves on.

use depot::variant::{Blob, Readout, ReadoutKind};
use httpmock::prelude::*;
use ietf_mirror::readout::DraftReadout;
use ietf_mirror::{FetchConfig, Mirror, MirrorConfig};
use reqwest::blocking::Client;
use tempfile::TempDir;

/// Mirror two alpha revisions from a mocked ietf.org into `<tmp>/m`.
fn mirrored() -> (TempDir, MockServer) {
    let server = MockServer::start();
    // REAL all_id.txt shape: each draft ONE line, at its LATEST
    // revision; the mirror enumerates 00..01 from it.
    server.mock(|when, then| {
        when.method(GET).path("/id/all_id.txt");
        then.status(200).body("draft-test-alpha-01\t2024-02-01\tActive\n");
    });
    for (doc, body) in [
        ("draft-test-alpha-00", "alpha zero\n"),
        ("draft-test-alpha-01", "alpha one\n"),
    ] {
        let path = format!("/archive/id/{doc}.txt");
        let b = body.to_string();
        server.mock(move |when, then| {
            when.method(GET).path(path.clone());
            then.status(200).body(b.clone());
        });
    }
    let tmp = TempDir::new().unwrap();
    let mut m = Mirror::open(MirrorConfig::new(tmp.path().join("m"))).unwrap();
    m.update(&Client::new(), &cfg(&server), |_| ()).unwrap();
    (tmp, server)
}

fn cfg(server: &MockServer) -> FetchConfig {
    FetchConfig {
        base_url: server.base_url(),
        delay: std::time::Duration::ZERO,
        ..FetchConfig::default()
    }
}

#[test]
fn serves_exactly_the_pinned_revision() {
    let (tmp, _srv) = mirrored();
    let r = DraftReadout::new(tmp.path().join("m"), "draft-test-alpha", "01");

    let root = r.entry(&[]).unwrap();
    assert_eq!(root.kind, ReadoutKind::Branch);
    assert_eq!(root.blob_len, None);
    assert_eq!(r.children(&[]), vec![b"draft-test-alpha-01.txt".to_vec()]);

    let leaf = r.entry(&[b"draft-test-alpha-01.txt"]).unwrap();
    assert_eq!(leaf.kind, ReadoutKind::Leaf);
    assert_eq!(leaf.blob_len, Some(10));
    assert_eq!(
        r.blob(&[b"draft-test-alpha-01.txt"]),
        Some(Blob::Bytes(b"alpha one\n".to_vec()))
    );
    // Other revisions are NOT part of a pinned attachment.
    assert_eq!(r.entry(&[b"draft-test-alpha-00.txt"]), None);
    assert_eq!(r.blob(&[b"draft-test-alpha-00.txt"]), None);

    // A pin on the OLDER revision serves the older bytes, not the head.
    let r0 = DraftReadout::new(tmp.path().join("m"), "draft-test-alpha", "00");
    assert_eq!(
        r0.blob(&[b"draft-test-alpha-00.txt"]),
        Some(Blob::Bytes(b"alpha zero\n".to_vec()))
    );
}

#[test]
fn misses() {
    let (tmp, _srv) = mirrored();
    let r = DraftReadout::new(tmp.path().join("m"), "draft-test-alpha", "01");
    assert_eq!(r.entry(&[b"draft-test-alpha-99.txt"]), None);
    assert_eq!(r.blob(&[b"nope.txt"]), None);
    assert_eq!(r.entry(&[b"draft-test-alpha-01.txt", b"deeper"]), None);
    assert!(r.children(&[b"draft-test-alpha-01.txt"]).is_empty());
    assert_eq!(r.blob(&[]), None);

    // Unknown draft, unknown rev, missing store: all misses, no errors.
    let r = DraftReadout::new(tmp.path().join("m"), "draft-does-not-exist", "00");
    assert_eq!(r.entry(&[]), None);
    assert!(r.children(&[]).is_empty());
    let r = DraftReadout::new(tmp.path().join("m"), "draft-test-alpha", "07");
    assert_eq!(r.entry(&[]), None);
    let r = DraftReadout::new(tmp.path().join("nonexistent"), "draft-test-alpha", "01");
    assert_eq!(r.entry(&[]), None);
}

/// The attach-honesty pair: (a) an attached (even decoded) readout
/// holds no lock, so a writer can open and update; (b) after the head
/// bumps, the pinned readout — including a FRESH one deciding what to
/// serve only now — still serves the pinned revision's bytes.
#[test]
fn update_while_attached_and_pin_survives_head_bump() {
    let (tmp, server) = mirrored();
    let root = tmp.path().join("m");

    let attached = DraftReadout::new(root.clone(), "draft-test-alpha", "01");
    assert_eq!(
        attached.blob(&[b"draft-test-alpha-01.txt"]),
        Some(Blob::Bytes(b"alpha one\n".to_vec())),
        "decode before the update"
    );

    // Head bump 01 -> 02 while the readout above stays attached: the
    // writer open + update must succeed (the decode dropped its lock).
    // Fresh stand-in host (httpmock routes are first-match-wins).
    drop(server);
    let bumped = MockServer::start();
    bumped.mock(|when, then| {
        when.method(GET).path("/id/all_id.txt");
        then.status(200).body("draft-test-alpha-02\t2024-03-01\tActive\n");
    });
    bumped.mock(|when, then| {
        when.method(GET).path("/archive/id/draft-test-alpha-02.txt");
        then.status(200).body("alpha two\n");
    });
    let mut m = Mirror::open(MirrorConfig::new(root.clone()))
        .expect("update-while-attached: writer open must not be blocked by a readout");
    let st = m.update(&Client::new(), &cfg(&bumped), |_| ()).unwrap();
    assert_eq!(st.revisions_fetched, 1);
    assert_eq!(m.head("draft-test-alpha").unwrap().unwrap().rev, "02");
    drop(m);

    // The already-decoded attachment still serves the pin…
    assert_eq!(
        attached.blob(&[b"draft-test-alpha-01.txt"]),
        Some(Blob::Bytes(b"alpha one\n".to_vec()))
    );
    // …and so does a readout whose FIRST decode happens after the bump
    // (this is the honesty fix: pre-fix it served the new head).
    let fresh = DraftReadout::new(root, "draft-test-alpha", "01");
    assert_eq!(fresh.children(&[]), vec![b"draft-test-alpha-01.txt".to_vec()]);
    assert_eq!(
        fresh.blob(&[b"draft-test-alpha-01.txt"]),
        Some(Blob::Bytes(b"alpha one\n".to_vec()))
    );
    assert_eq!(fresh.entry(&[b"draft-test-alpha-02.txt"]), None, "head not served");
}

/// While a writer holds the root, an access is a MISS that is NOT
/// cached: the same readout resolves once the writer is gone.
#[test]
fn writer_contention_is_a_retryable_miss() {
    let (tmp, _srv) = mirrored();
    let root = tmp.path().join("m");
    let r = DraftReadout::new(root.clone(), "draft-test-alpha", "01");

    let writer = Mirror::open(MirrorConfig::new(root)).unwrap();
    assert_eq!(r.entry(&[]), None, "miss while the writer holds the root");
    assert_eq!(r.blob(&[b"draft-test-alpha-01.txt"]), None);
    drop(writer);

    assert_eq!(
        r.blob(&[b"draft-test-alpha-01.txt"]),
        Some(Blob::Bytes(b"alpha one\n".to_vec())),
        "contention miss was not cached"
    );
}
