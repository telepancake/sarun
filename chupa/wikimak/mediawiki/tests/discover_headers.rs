//! Content-history discovery must obtain exact sizes with request work that is
//! independent of the number of payload parts. Wikimedia's bounded directory
//! listing carries every byte count; `SHA256SUMS` supplies the completion fence,
//! authoritative filenames, and digests. No payload HEAD or Range probes belong
//! in discovery.

mod common;

use httpmock::prelude::*;
use reqwest::blocking::Client;
use wikimak_mediawiki::{discover_with, Config, RunSource};

use common::{build_directory_listing, fixture};

fn client() -> Client {
    Client::builder().build().unwrap()
}

fn cfg(server: &MockServer) -> Config {
    Config {
        base_url: server.base_url(),
    }
}

#[test]
fn discovery_request_count_does_not_grow_with_part_count() {
    let server = MockServer::start();

    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(200).body(fixture("content_history_index.html"));
    });
    let bz2dir = "/other/mediawiki_content_history/testwiki/2024-06-01/xml/bzip2/";
    let names = (0..256)
        .map(|index| {
            format!(
                "testwiki-20240601-p{}p{}.xml.bz2",
                index * 1000 + 1,
                index * 1000 + 1000
            )
        })
        .collect::<Vec<_>>();
    let listed = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), 100_000_000 + index as u64))
        .collect::<Vec<_>>();
    let listing = build_directory_listing(&listed);
    let listing_mock = server.mock(move |when, then| {
        when.method(GET).path(bz2dir);
        then.status(200).body(listing.clone());
    });
    let digest = "a".repeat(64);
    let sums = names
        .iter()
        .map(|name| format!("{digest}  {name}\n"))
        .collect::<String>();
    let sums_mock = server.mock(move |when, then| {
        when.method(GET).path(format!("{bz2dir}SHA256SUMS"));
        then.status(200).body(sums.clone());
    });

    let run = discover_with(&client(), &cfg(&server), "testwiki")
        .expect("one listing should size every checksum-bound part");

    assert_eq!(run.source, RunSource::ContentHistory);
    assert_eq!(run.parts.len(), names.len());
    assert_eq!(run.parts[0].size_bytes, 100_000_000);
    assert_eq!(
        run.parts.last().expect("parts").size_bytes,
        100_000_000 + names.len() as u64 - 1
    );
    assert_eq!(listing_mock.hits(), 1, "one directory listing request");
    assert_eq!(sums_mock.hits(), 1, "one checksum-manifest request");
}

#[test]
fn discovery_rejects_manifest_part_without_exact_listed_size() {
    let server = MockServer::start();

    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(200).body(fixture("content_history_index.html"));
    });
    let bz2dir = "/other/mediawiki_content_history/testwiki/2024-06-01/xml/bzip2/";
    let listed = "testwiki-20240601-p1p999.xml.bz2";
    let missing = "testwiki-20240601-p1000p1999.xml.bz2";
    let listing = build_directory_listing(&[(listed, 42_000_000)]);
    let listing_mock = server.mock(move |when, then| {
        when.method(GET).path(bz2dir);
        then.status(200).body(listing.clone());
    });
    let sums = format!("{}  {listed}\n{}  {missing}\n", "a".repeat(64), "b".repeat(64));
    let sums_mock = server.mock(move |when, then| {
        when.method(GET).path(format!("{bz2dir}SHA256SUMS"));
        then.status(200).body(sums.clone());
    });

    let error = discover_with(&client(), &cfg(&server), "testwiki")
        .expect_err("an incomplete size listing must fail closed");

    let message = error.to_string();
    assert!(message.contains("no exact size"), "{message}");
    assert!(message.contains(missing), "{message}");
    assert_eq!(listing_mock.hits(), 1);
    assert_eq!(sums_mock.hits(), 1);
}
