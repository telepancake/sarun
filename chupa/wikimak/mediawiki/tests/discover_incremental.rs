use chrono::NaiveDate;
use httpmock::prelude::*;
use reqwest::blocking::Client;
use wikimak_mediawiki::{discover_incremental_with, Config, RunSource};

#[test]
fn absent_incremental_tree_is_an_empty_feed() {
    let server = MockServer::start();
    let root = server.mock(|when, then| {
        when.method(GET).path("/other/incr/closedwiki/");
        then.status(404).body("not found");
    });

    let runs = discover_incremental_with(
        &Client::new(),
        &Config { base_url: server.base_url() },
        "closedwiki",
        Some(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()),
    )
    .unwrap();

    assert!(runs.is_empty());
    root.assert_hits(1);
}

#[test]
fn discovers_completed_daily_runs_after_watermark() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/");
        then.status(200).body(
            r#"<a href="20260720/">20260720/</a>
               <a href="20260721/">20260721/</a>"#,
        );
    });
    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/20260721/status.txt");
        then.status(200).body("Status: done\n");
    });
    let filename = "testwiki-20260721-pages-meta-hist-incr.xml.bz2";
    server.mock(move |when, then| {
        when.method(GET)
            .path("/other/incr/testwiki/20260721/testwiki-20260721-md5sums.txt");
        then.status(200).body(format!("{}  {filename}\n", "a".repeat(32)));
    });

    let runs = discover_incremental_with(
        &Client::new(),
        &Config { base_url: server.base_url() },
        "testwiki",
        Some(NaiveDate::from_ymd_opt(2026, 7, 20).unwrap()),
    )
    .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].source, RunSource::Incremental);
    assert_eq!(runs[0].date.to_string(), "2026-07-21");
    assert_eq!(runs[0].parts[0].filename, filename);
    assert_eq!(runs[0].parts[0].md5.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
}

#[test]
fn ignores_unfinished_daily_run() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/");
        then.status(200).body(r#"<a href="20260721/">20260721/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/20260721/status.txt");
        then.status(200).body("Status: running\n");
    });
    let runs = discover_incremental_with(
        &Client::new(),
        &Config { base_url: server.base_url() },
        "testwiki",
        None,
    )
    .unwrap();
    assert!(runs.is_empty());
}
