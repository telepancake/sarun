//! `DraftReadout` — the RO-attachment readout over one draft series
//! (ATTACH-CONVERGENCE.md chip 2). Shape contract: every mirrored
//! revision as a leaf `<draft>-<rev>.txt` at the root.

use depot::variant::{Blob, Readout, ReadoutKind};
use httpmock::prelude::*;
use ietf_mirror::readout::DraftReadout;
use ietf_mirror::{FetchConfig, Mirror, MirrorConfig};
use reqwest::blocking::Client;
use tempfile::TempDir;

const IDX: &str = "\
draft-test-alpha-00\t2024-01-01\tActive\n\
draft-test-alpha-01\t2024-02-01\tActive\n";

/// Mirror two alpha revisions from a mocked ietf.org, hand the Mirror
/// to the adapter.
fn mirrored() -> (TempDir, Mirror) {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/id/all_id.txt");
        then.status(200).body(IDX);
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
    m.update(&Client::new(), &FetchConfig { base_url: server.base_url() }, |_, _| ()).unwrap();
    (tmp, m)
}

#[test]
fn serves_every_revision_as_leaves() {
    let (_tmp, m) = mirrored();
    let r = DraftReadout::new(m, "draft-test-alpha");

    let root = r.entry(&[]).unwrap();
    assert_eq!(root.kind, ReadoutKind::Branch);
    assert_eq!(root.blob_len, None);
    // Ordered by name — revision order coincides here.
    assert_eq!(
        r.children(&[]),
        vec![b"draft-test-alpha-00.txt".to_vec(), b"draft-test-alpha-01.txt".to_vec()]
    );

    let head = r.entry(&[b"draft-test-alpha-01.txt"]).unwrap();
    assert_eq!(head.kind, ReadoutKind::Leaf);
    assert_eq!(head.blob_len, Some(10));
    assert_eq!(
        r.blob(&[b"draft-test-alpha-01.txt"]),
        Some(Blob::Bytes(b"alpha one\n".to_vec()))
    );
    assert_eq!(
        r.blob(&[b"draft-test-alpha-00.txt"]),
        Some(Blob::Bytes(b"alpha zero\n".to_vec()))
    );
}

#[test]
fn misses() {
    let (_tmp, m) = mirrored();
    let r = DraftReadout::new(m, "draft-test-alpha");
    assert_eq!(r.entry(&[b"draft-test-alpha-99.txt"]), None);
    assert_eq!(r.blob(&[b"nope.txt"]), None);
    assert_eq!(r.entry(&[b"draft-test-alpha-00.txt", b"deeper"]), None);
    assert!(r.children(&[b"draft-test-alpha-00.txt"]).is_empty());
    assert_eq!(r.blob(&[]), None);
}

#[test]
fn unknown_draft_is_a_miss_not_an_error() {
    let (_tmp, m) = mirrored();
    let r = DraftReadout::new(m, "draft-does-not-exist");
    assert_eq!(r.entry(&[]), None);
    assert!(r.children(&[]).is_empty());
}
