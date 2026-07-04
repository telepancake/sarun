//! Fetch acceptance suite. PHASES.md §W3-Rust-2 / SPEC §API.
//!
//! `VerifyingReader` verifies sha256 on EOF (or sha1 if sha256 is
//! absent). Partial reads / drops skip the check.

mod common;

use std::io::Read;

use httpmock::prelude::*;
use reqwest::blocking::Client;
use wikimak_mediawiki::{fetch, Part};

use common::{fixture, sha1_hex, sha256_hex};

fn client() -> Client {
    Client::builder().build().unwrap()
}

fn part(url: String, filename: &str, body: &[u8], sha256: Option<String>, sha1: Option<String>) -> Part {
    Part {
        url,
        filename: filename.to_string(),
        size_bytes: body.len() as u64,
        sha256,
        sha1,
    }
}

// ---------------------------------------------------------------------------
// fetch_streams_with_checksum
//
// Serve the bz2 fixture with its real SHA-256 in the Part. Read to EOF;
// bytes match; no error on drop.
// ---------------------------------------------------------------------------

#[test]
fn fetch_streams_with_checksum() {
    let body = fixture("small_payload.txt.bz2");
    let server = MockServer::start();
    let body_for_mock = body.clone();
    server.mock(move |when, then| {
        when.method(GET).path("/part.xml.bz2");
        then.status(200).body(body_for_mock.clone());
    });

    let p = part(
        server.url("/part.xml.bz2"),
        "small_payload.txt.bz2",
        &body,
        Some(sha256_hex(&body)),
        None,
    );

    let mut rd = fetch(&client(), &p).expect("fetch should succeed");
    let mut got = Vec::new();
    rd.read_to_end(&mut got).expect("read to EOF should succeed");
    assert_eq!(got, body, "streamed bytes must equal served body");
}

// ---------------------------------------------------------------------------
// fetch_sha256_mismatch_errors_on_eof
//
// Corrupt the advertised sha256; reading to EOF returns an error.
// ---------------------------------------------------------------------------

#[test]
fn fetch_sha256_mismatch_errors_on_eof() {
    let body = b"actual server bytes\n".to_vec();
    let server = MockServer::start();
    let body_for_mock = body.clone();
    server.mock(move |when, then| {
        when.method(GET).path("/part.xml.bz2");
        then.status(200).body(body_for_mock.clone());
    });

    // Hash of a DIFFERENT string.
    let wrong = sha256_hex(b"not what the server serves");
    let p = part(
        server.url("/part.xml.bz2"),
        "part.xml.bz2",
        &body,
        Some(wrong),
        None,
    );

    let mut rd = fetch(&client(), &p).expect("fetch (open) must still succeed");
    let mut sink = Vec::new();
    let res = rd.read_to_end(&mut sink);
    assert!(
        res.is_err(),
        "reading to EOF on a sha256 mismatch must surface an error"
    );
}

// ---------------------------------------------------------------------------
// fetch_partial_read_skips_check
//
// Read N < total bytes, drop the reader. No panic, no error.
// ---------------------------------------------------------------------------

#[test]
fn fetch_partial_read_skips_check() {
    // Use a long-ish body so a small read is genuinely partial.
    let body = vec![b'X'; 4096];
    let server = MockServer::start();
    let body_for_mock = body.clone();
    server.mock(move |when, then| {
        when.method(GET).path("/part.xml.bz2");
        then.status(200).body(body_for_mock.clone());
    });

    // Advertise the WRONG sha256 — proves the check did not run, since
    // we never hit EOF.
    let wrong = sha256_hex(b"unrelated");
    let p = part(
        server.url("/part.xml.bz2"),
        "part.xml.bz2",
        &body,
        Some(wrong),
        None,
    );

    let mut rd = fetch(&client(), &p).expect("fetch should succeed");
    let mut buf = [0u8; 16];
    let n = rd.read(&mut buf).expect("partial read should succeed");
    assert!(n > 0, "must deliver at least some bytes");
    // Drop without reading to EOF; the check is skipped.
    drop(rd);
}

// ---------------------------------------------------------------------------
// fetch_uses_sha1_when_no_sha256
//
// Part with sha256=None, sha1=Some(...) → verification uses sha1.
// ---------------------------------------------------------------------------

#[test]
fn fetch_uses_sha1_when_no_sha256() {
    let body = b"legacy dumps use sha1\n".to_vec();
    let server = MockServer::start();
    let body_for_mock = body.clone();
    server.mock(move |when, then| {
        when.method(GET).path("/legacy.bz2");
        then.status(200).body(body_for_mock.clone());
    });

    // Good sha1 → read to EOF cleanly.
    let p_ok = part(
        server.url("/legacy.bz2"),
        "legacy.bz2",
        &body,
        None,
        Some(sha1_hex(&body)),
    );
    let mut rd = fetch(&client(), &p_ok).expect("fetch ok-sha1");
    let mut got = Vec::new();
    rd.read_to_end(&mut got).expect("good sha1: read to EOF ok");
    assert_eq!(got, body);

    // Bad sha1 → read to EOF errors.
    let p_bad = part(
        server.url("/legacy.bz2"),
        "legacy.bz2",
        &body,
        None,
        Some(sha1_hex(b"decoy")),
    );
    let mut rd = fetch(&client(), &p_bad).expect("fetch bad-sha1 (open)");
    let mut sink = Vec::new();
    let res = rd.read_to_end(&mut sink);
    assert!(res.is_err(), "bad sha1 must surface as a read error at EOF");
}
