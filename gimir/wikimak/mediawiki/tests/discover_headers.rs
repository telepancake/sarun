//! Discover must learn part sizes from HTTP METADATA — HEAD (or a
//! `Range: bytes=0-0` header probe) — and never pull a part body just
//! to count it: on the real host each part is gigabytes, and the old
//! GET-and-drain in `http_resolve_size` transferred every one of them
//! during discovery.
//!
//! The stand-in server routes HEAD and GET separately, so the
//! assertion is direct: after a successful discover, the part-body GET
//! mocks (the only routes that would transfer body bytes) have ZERO
//! hits, while the HEAD mocks carry the traffic.

mod common;

use httpmock::prelude::*;
use reqwest::blocking::Client;
use wikimak_mediawiki::{discover_with, Config, RunSource};

use common::{build_sha256sums, fixture};

fn client() -> Client {
    Client::builder().build().unwrap()
}

fn cfg(server: &MockServer) -> Config {
    Config {
        base_url: server.base_url(),
    }
}

#[test]
fn discover_transfers_no_part_body_bytes() {
    let server = MockServer::start();

    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(200).body(fixture("content_history_index.html"));
    });
    let bz2dir = "/other/mediawiki_content_history/testwiki/2024-06-01/xml/bzip2/";
    server.mock(|when, then| {
        when.method(GET).path(bz2dir);
        then.status(200).body(fixture("content_history_done.html"));
    });

    let p1 = ("testwiki-20240601-p1p999.xml.bz2", 1_234_567u64);
    let p2 = ("testwiki-20240601-p1000p1999.xml.bz2", 7_654_321u64);

    // HEAD answers with the entity's Content-Length and no body.
    let mut head_mocks = Vec::new();
    // Body GET routes exist (the server COULD serve the bytes) — the
    // point is that discover never asks for them.
    let mut body_mocks = Vec::new();
    for (name, size) in [p1, p2] {
        let path = format!("{bz2dir}{name}");
        let p = path.clone();
        head_mocks.push(server.mock(move |when, then| {
            when.method("HEAD").path(&p);
            then.status(200).header("Content-Length", size.to_string());
        }));
        let p = path.clone();
        body_mocks.push(server.mock(move |when, then| {
            when.method(GET).path(&p);
            then.status(200).body(vec![0u8; 4096]);
        }));
    }

    // _SUCCESS: HEAD-visible; its GET route is also body-counted.
    let success_head = server.mock(|when, then| {
        when.method("HEAD").path(format!("{bz2dir}_SUCCESS"));
        then.status(200).header("Content-Length", "0");
    });
    let success_get = server.mock(|when, then| {
        when.method(GET).path(format!("{bz2dir}_SUCCESS"));
        then.status(200).body("");
    });

    // SHA256SUMS is a listing discover legitimately reads (bounded,
    // kilobytes) — GET stays correct for it.
    let sums = build_sha256sums(&[
        (p1.0, &b"IRRELEVANT-ONE"[..]),
        (p2.0, &b"IRRELEVANT-TWO"[..]),
    ]);
    server.mock(move |when, then| {
        when.method(GET).path(format!("{bz2dir}SHA256SUMS"));
        then.status(200).body(sums.clone());
    });

    let run = discover_with(&client(), &cfg(&server), "testwiki")
        .expect("discover with HEAD-only size resolution");

    assert_eq!(run.source, RunSource::ContentHistory);
    assert_eq!(run.parts.len(), 2);
    // Sizes come from the HEAD Content-Length, NOT from any body.
    assert_eq!(run.parts[0].size_bytes, p1.1);
    assert_eq!(run.parts[1].size_bytes, p2.1);

    for m in &head_mocks {
        assert_eq!(m.hits(), 1, "each part probed with exactly one HEAD");
    }
    for m in &body_mocks {
        assert_eq!(m.hits(), 0, "ZERO part-body GETs during discover");
    }
    assert_eq!(success_head.hits(), 1, "_SUCCESS checked via HEAD");
    assert_eq!(success_get.hits(), 0, "_SUCCESS body never fetched");
}

// ---------------------------------------------------------------------------
// A server without HEAD support (405) falls back to `Range: bytes=0-0`
// and reads the total from Content-Range — still no body drain (the
// mock returns a one-byte 206).
// ---------------------------------------------------------------------------
#[test]
fn discover_range_fallback_reads_content_range_total() {
    let server = MockServer::start();

    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(200).body(fixture("content_history_index.html"));
    });
    let bz2dir = "/other/mediawiki_content_history/testwiki/2024-06-01/xml/bzip2/";
    server.mock(|when, then| {
        when.method(GET).path(bz2dir);
        then.status(200).body(fixture("content_history_done.html"));
    });

    let part = ("testwiki-20240601-p1p999.xml.bz2", 42_000_000u64);
    let path = format!("{bz2dir}{part_name}", part_name = part.0);
    let p = path.clone();
    server.mock(move |when, then| {
        when.method("HEAD").path(&p);
        then.status(405);
    });
    let p = path.clone();
    let ranged = server.mock(move |when, then| {
        when.method(GET).path(&p).header("Range", "bytes=0-0");
        then.status(206)
            .header("Content-Range", format!("bytes 0-0/{}", part.1))
            .body(vec![0u8; 1]);
    });

    server.mock(|when, then| {
        when.method("HEAD").path(format!("{bz2dir}_SUCCESS"));
        then.status(200).header("Content-Length", "0");
    });
    let sums = build_sha256sums(&[(part.0, &b"IRRELEVANT"[..])]);
    server.mock(move |when, then| {
        when.method(GET).path(format!("{bz2dir}SHA256SUMS"));
        then.status(200).body(sums.clone());
    });

    let run = discover_with(&client(), &cfg(&server), "testwiki")
        .expect("discover via Range fallback");
    assert_eq!(run.parts.len(), 1);
    assert_eq!(
        run.parts[0].size_bytes, part.1,
        "size comes from the Content-Range TOTAL, not the 1-byte body"
    );
    assert_eq!(ranged.hits(), 1, "exactly one bounded Range probe");
}
