//! Discover acceptance suite. PHASES.md §W3-Rust-2 / SPEC §API.
//!
//! Mock server (`httpmock`) stands in for `dumps.wikimedia.org`. The
//! production base URL is overridden via `Config::base_url`.

mod common;

use chrono::NaiveDate;
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

// ---------------------------------------------------------------------------
// discover_content_history_happy_path
//
// Serve the two index HTMLs and a SHA256SUMS listing 3 part files. The
// newest "done" date (2024-06-01 from the index fixture) is the one
// `_SUCCESS` is installed under; that's the date Discover must pick. The
// 3 parts come back sorted by the leading page-range integer.
// ---------------------------------------------------------------------------

#[test]
fn discover_content_history_happy_path() {
    let server = MockServer::start();

    // Index listing.
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(200)
            .header("content-type", "text/html")
            .body(fixture("content_history_index.html"));
    });

    // The newer date (2024-06-01) is fully done.
    let bz2dir = "/other/mediawiki_content_history/testwiki/2024-06-01/xml/bzip2/";
    server.mock(|when, then| {
        when.method(GET).path(bz2dir);
        then.status(200)
            .header("content-type", "text/html")
            .body(fixture("content_history_done.html"));
    });

    let p1 = ("testwiki-20240601-p1p999.xml.bz2", &b"PART-ONE-stub"[..]);
    let p2 = (
        "testwiki-20240601-p1000p1999.xml.bz2",
        &b"PART-TWO-stub"[..],
    );
    let p3 = (
        "testwiki-20240601-p2000p2999.xml.bz2",
        &b"PART-THREE-stub"[..],
    );
    for (name, body) in [p1, p2, p3] {
        let path = format!("{bz2dir}{name}");
        let b = body.to_vec();
        server.mock(move |when, then| {
            when.method(GET).path(&path);
            then.status(200).body(b);
        });
    }
    server.mock(|when, then| {
        when.method(GET).path(format!("{bz2dir}_SUCCESS"));
        then.status(200).body("");
    });
    let sums = build_sha256sums(&[p1, p2, p3]);
    server.mock(move |when, then| {
        when.method(GET).path(format!("{bz2dir}SHA256SUMS"));
        then.status(200).body(sums.clone());
    });

    // Older 2024-05-01 directory exists in the index but is incomplete:
    // no _SUCCESS, no SHA256SUMS — discover must skip it.

    let run = discover_with(&client(), &cfg(&server), "testwiki")
        .expect("happy-path discover should succeed");

    assert_eq!(run.source, RunSource::ContentHistory);
    assert_eq!(run.date, NaiveDate::from_ymd_opt(2024, 6, 1).unwrap());
    assert_eq!(run.parts.len(), 3, "expected 3 parts");

    let names: Vec<&str> = run.parts.iter().map(|p| p.filename.as_str()).collect();
    assert_eq!(
        names,
        vec![p1.0, p2.0, p3.0],
        "parts must come back sorted by leading page-range integer"
    );
    for part in &run.parts {
        assert!(
            part.url.ends_with(&part.filename),
            "url {:?} must end in filename {:?}",
            part.url,
            part.filename
        );
        assert!(part.size_bytes > 0);
        let sha = part.sha256.as_ref().expect("sha256 must be populated from SHA256SUMS");
        assert_eq!(sha.len(), 64, "sha256 must be 64 hex chars");
    }
}

// ---------------------------------------------------------------------------
// discover_filters_incomplete_dates
//
// Newer date in the index has no _SUCCESS marker; older one does.
// Discover picks the older complete one.
// ---------------------------------------------------------------------------

#[test]
fn discover_filters_incomplete_dates() {
    let server = MockServer::start();

    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(200).body(fixture("content_history_index.html"));
    });

    // 2024-06-01 directory listing exists, but _SUCCESS and SHA256SUMS
    // 404 — incomplete.
    server.mock(|when, then| {
        when.method(GET).path(
            "/other/mediawiki_content_history/testwiki/2024-06-01/xml/bzip2/",
        );
        then.status(200).body(fixture("content_history_done.html"));
    });
    // _SUCCESS and SHA256SUMS for 06-01 deliberately NOT installed (404).

    // 2024-05-01 is fully done.
    let bz2dir = "/other/mediawiki_content_history/testwiki/2024-05-01/xml/bzip2/";
    server.mock(|when, then| {
        when.method(GET).path(bz2dir);
        then.status(200).body(fixture("content_history_done.html"));
    });
    let p1 = ("testwiki-20240501-p1p999.xml.bz2", &b"MAY-PART-ONE"[..]);
    let p2 = ("testwiki-20240501-p1000p1999.xml.bz2", &b"MAY-PART-TWO"[..]);
    for (name, body) in [p1, p2] {
        let path = format!("{bz2dir}{name}");
        let b = body.to_vec();
        server.mock(move |when, then| {
            when.method(GET).path(&path);
            then.status(200).body(b);
        });
    }
    server.mock(|when, then| {
        when.method(GET).path(format!("{bz2dir}_SUCCESS"));
        then.status(200).body("");
    });
    let sums = build_sha256sums(&[p1, p2]);
    server.mock(move |when, then| {
        when.method(GET).path(format!("{bz2dir}SHA256SUMS"));
        then.status(200).body(sums.clone());
    });

    let run = discover_with(&client(), &cfg(&server), "testwiki")
        .expect("discover should fall back to the older complete date");

    assert_eq!(run.source, RunSource::ContentHistory);
    assert_eq!(
        run.date,
        NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
        "must pick the older complete date, not the newer incomplete one"
    );
}

// ---------------------------------------------------------------------------
// discover_falls_back_to_legacy_on_404
//
// Content-history root 404s. Discover falls back to legacy
// `/testwiki/<YYYYMMDD>/dumpstatus.json`.
// ---------------------------------------------------------------------------

#[test]
fn discover_falls_back_to_legacy_on_404() {
    let server = MockServer::start();

    // Content-history index path is NOT installed → server returns 404.

    // Legacy: a listing page with one date dir + a "done" dumpstatus.json.
    server.mock(|when, then| {
        when.method(GET).path("/testwiki/");
        then.status(200)
            .body(r#"<html><body><a href="20240401/">20240401/</a></body></html>"#);
    });
    server.mock(|when, then| {
        when.method(GET).path("/testwiki/20240401/dumpstatus.json");
        then.status(200).body(fixture("dumpstatus_done.json"));
    });
    // Part bodies — size advertised in dumpstatus.json is the authoritative
    // source (97), but install some bytes so HEAD-style probes (if any)
    // succeed.
    for name in [
        "testwiki-20240401-pages-meta-history1.xml-p1p99.bz2",
        "testwiki-20240401-pages-meta-history2.xml-p100p199.bz2",
    ] {
        let path = format!("/testwiki/20240401/{name}");
        server.mock(move |when, then| {
            when.method(GET).path(&path);
            then.status(200).body(vec![0u8; 97]);
        });
    }

    let run = discover_with(&client(), &cfg(&server), "testwiki")
        .expect("legacy fallback should succeed");
    assert_eq!(run.source, RunSource::Legacy);
    assert_eq!(run.date, NaiveDate::from_ymd_opt(2024, 4, 1).unwrap());
    assert_eq!(run.parts.len(), 2);
    assert_eq!(
        run.parts[0].filename,
        "testwiki-20240401-pages-meta-history1.xml-p1p99.bz2"
    );
    assert_eq!(
        run.parts[1].filename,
        "testwiki-20240401-pages-meta-history2.xml-p100p199.bz2"
    );
    assert_eq!(
        run.parts[0].size_bytes, 97,
        "size advertised in dumpstatus.json must be used"
    );
}

// ---------------------------------------------------------------------------
// discover_legacy_status_in_progress_skipped
//
// The newest legacy date has dumpstatus.json status "in-progress"; an
// older date is "done". Discover must skip the in-progress one.
// ---------------------------------------------------------------------------

#[test]
fn discover_legacy_status_in_progress_skipped() {
    let server = MockServer::start();

    // Content-history 404 → fall back to legacy.
    server.mock(|when, then| {
        when.method(GET).path("/testwiki/");
        then.status(200).body(
            r#"<html><body>
            <a href="20240401/">20240401/</a>
            <a href="20240501/">20240501/</a>
            </body></html>"#,
        );
    });

    // Newest (20240501) is in-progress.
    server.mock(|when, then| {
        when.method(GET).path("/testwiki/20240501/dumpstatus.json");
        then.status(200)
            .body(fixture("dumpstatus_in_progress.json"));
    });
    // Older (20240401) is done.
    server.mock(|when, then| {
        when.method(GET).path("/testwiki/20240401/dumpstatus.json");
        then.status(200).body(fixture("dumpstatus_done.json"));
    });
    for name in [
        "testwiki-20240401-pages-meta-history1.xml-p1p99.bz2",
        "testwiki-20240401-pages-meta-history2.xml-p100p199.bz2",
    ] {
        let path = format!("/testwiki/20240401/{name}");
        server.mock(move |when, then| {
            when.method(GET).path(&path);
            then.status(200).body(vec![0u8; 97]);
        });
    }

    let run = discover_with(&client(), &cfg(&server), "testwiki")
        .expect("discover must pick the older done run");
    assert_eq!(run.source, RunSource::Legacy);
    assert_eq!(
        run.date,
        NaiveDate::from_ymd_opt(2024, 4, 1).unwrap(),
        "must skip the in-progress 2024-05-01 and return 2024-04-01"
    );
}

// ---------------------------------------------------------------------------
// discover_part_filenames_sorted_by_page_range
//
// Parts named `*-p1p100`, `*-p2p50`, `*-p101p200` come back sorted by
// the FIRST page-range integer — NOT lexicographically (lex would put
// p101 before p2).
// ---------------------------------------------------------------------------

#[test]
fn discover_part_filenames_sorted_by_page_range() {
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

    // Deliberately out-of-order by lex but in clean numeric order by
    // first-int: p1 < p2 < p101.
    let p1 = ("testwiki-20240601-p1p100.xml.bz2", &b"AAA"[..]);
    let p2 = ("testwiki-20240601-p2p50.xml.bz2", &b"BBB"[..]);
    let p3 = ("testwiki-20240601-p101p200.xml.bz2", &b"CCC"[..]);
    for (name, body) in [p1, p2, p3] {
        let path = format!("{bz2dir}{name}");
        let b = body.to_vec();
        server.mock(move |when, then| {
            when.method(GET).path(&path);
            then.status(200).body(b);
        });
    }
    server.mock(|when, then| {
        when.method(GET).path(format!("{bz2dir}_SUCCESS"));
        then.status(200).body("");
    });
    let sums = build_sha256sums(&[p1, p2, p3]);
    server.mock(move |when, then| {
        when.method(GET).path(format!("{bz2dir}SHA256SUMS"));
        then.status(200).body(sums.clone());
    });

    let run = discover_with(&client(), &cfg(&server), "testwiki").unwrap();
    let names: Vec<&str> = run.parts.iter().map(|p| p.filename.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "testwiki-20240601-p1p100.xml.bz2",
            "testwiki-20240601-p2p50.xml.bz2",
            "testwiki-20240601-p101p200.xml.bz2",
        ],
        "sort by first page-range int, NOT lexicographic"
    );
}
