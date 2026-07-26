//! End-to-end `sync` acceptance: an httpmock server stands in for
//! dumps.wikimedia.org (legacy branch), serving a dumpstatus.json whose
//! one part is the `export_three_pages.xml` fixture. Asserts:
//!   - first sync fetches the part and the pages land in the depot;
//!   - the part is watermarked in `parts_seen`;
//!   - a second sync skips the part (no re-fetch: hit counter static)
//!     and imports nothing new;
//!   - a checksum mismatch fails the sync and leaves NO watermark.

mod common;

use std::io::Write;

use bzip2::write::BzEncoder;
use bzip2::Compression;
use httpmock::prelude::*;
use reqwest::blocking::Client;
use sha1::{Digest as _, Sha1};
use md5::Md5;
use tempfile::TempDir;
use wikimak_mediawiki::Config;
use wikimak_wikipedia::{maintain, reconcile_history, sync};

use common::{fixture, make_instance};

const PART: &str = "testwiki-20240601-pages-meta-history1.xml-p1p99";

fn dumpstatus(sha1_hex: &str, size: usize) -> String {
    format!(
        r#"{{"jobs":{{"metahistorybz2dump":{{"status":"done","files":{{
            "{PART}":{{"size":{size},"url":"/testwiki/20240601/{PART}","sha1":"{sha1_hex}"}}
        }}}}}}}}"#
    )
}

fn history_body(event_type: &str) -> Vec<u8> {
    let mut fields = vec![""; 78];
    fields[0] = "testwiki";
    fields[1] = "123";
    fields[2] = "page";
    fields[3] = event_type;
    fields[4] = "2024-06-01 12:34:56.0";
    fields[5] = "action comment";
    fields[6] = "9";
    fields[9] = "Editor";
    fields[28] = "1";
    fields[29] = "Old title";
    fields[30] = "Current title";
    fields[31] = "0";
    fields[33] = "0";
    fields[36] = "false";
    let mut encoder = BzEncoder::new(Vec::new(), Compression::default());
    writeln!(encoder, "{}", fields.join("\t")).unwrap();
    let mut revision = vec![""; 78];
    revision[0] = "testwiki";
    revision[2] = "revision";
    revision[3] = "create";
    revision[4] = "2024-06-01 12:00:00.0";
    revision[28] = "1";
    revision[60] = "100";
    revision[63] = "text,user";
    revision[64] = "true";
    revision[70] = "true";
    revision[71] = "2024-06-02 00:00:00.0";
    writeln!(encoder, "{}", revision.join("\t")).unwrap();
    encoder.finish().unwrap()
}

fn malformed_history_body() -> Vec<u8> {
    let mut fields = vec![""; 78];
    fields[0] = "testwiki";
    fields[2] = "page";
    fields[3] = "move";
    fields[4] = "2024-06-01 12:34:56.0";
    fields[28] = "1";
    let mut encoder = BzEncoder::new(Vec::new(), Compression::default());
    writeln!(encoder, "{}", fields.join("\t")).unwrap();
    writeln!(encoder, "truncated").unwrap();
    encoder.finish().unwrap()
}

fn mount_history(server: &MockServer) {
    server.mock(|when, then| {
        when.method(GET).path("/other/mediawiki_history/");
        then.status(200).body(r#"<a href="2024-06/">2024-06/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/");
        then.status(200).body(
            r#"<a href="2024-06.testwiki.all-time.tsv.bz2">history</a>"#,
        );
    });
    let body = history_body("move");
    server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/2024-06.testwiki.all-time.tsv.bz2");
        then.status(200).body(body.clone());
    });
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
    let part_mock = server.mock(move |when, then| {
        when.method(GET).path(format!("/testwiki/20240601/{PART}"));
        then.status(200).body(body.clone());
    });
    mount_history(server);
    part_mock
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
    let wrong = sync(&inst, &client, &cfg, "otherwiki", |_, _| ())
        .unwrap_err()
        .to_string();
    assert!(wrong.contains("belongs to testwiki"), "{wrong}");
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
    assert!(
        inst.page_head_text(1).unwrap().is_none(),
        "checksum failure must be detected before any records are imported"
    );
}

#[test]
fn maintenance_consumes_daily_adds_changes_without_full_redownload() {
    let server = MockServer::start();
    let xml = fixture("export_three_pages.xml");
    let sha1_hex = hex::encode(Sha1::digest(&xml));
    let full_part = mount(&server, &xml, &sha1_hex);
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let client = Client::new();
    let cfg = Config { base_url: server.base_url() };
    sync(&inst, &client, &cfg, "testwiki", |_, _| ()).unwrap();
    let full_hits = full_part.hits();

    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/");
        then.status(200).body(r#"<a href="20240602/">20240602/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/20240602/status.txt");
        then.status(200).body("Status: done\n");
    });
    let daily_name = "testwiki-20240602-pages-meta-hist-incr.xml.bz2";
    let mut encoder = BzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&xml).unwrap();
    let daily_bytes = encoder.finish().unwrap();
    let daily_md5 = hex::encode(Md5::digest(&daily_bytes));
    server.mock(move |when, then| {
        when.method(GET)
            .path("/other/incr/testwiki/20240602/testwiki-20240602-md5sums.txt");
        then.status(200).body(format!("{daily_md5}  {daily_name}\n"));
    });
    let daily_body = daily_bytes;
    let daily = server.mock(move |when, then| {
        when.method(GET).path(format!("/other/incr/testwiki/20240602/{daily_name}"));
        then.status(200).body(daily_body.clone());
    });

    let stats = maintain(&inst, &client, &cfg, "testwiki", |_, _| ()).unwrap();
    assert_eq!(stats.parts_fetched, 1);
    assert_eq!(daily.hits(), 1);
    assert_eq!(
        full_part.hits(),
        full_hits,
        "routine maintenance must not redownload the full snapshot"
    );
    assert_eq!(
        inst.sync_state("incremental_date").unwrap().as_deref(),
        Some("2024-06-02")
    );
    let actions = inst.page_actions(1).unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].event_type, "move");
    let visibility = inst.revision_visibility(100).unwrap().unwrap();
    assert_eq!(visibility.deleted_parts, "text,user");
    assert!(visibility.parts_are_suppressed);
    assert!(visibility.deleted_by_page_deletion);
}

#[test]
fn history_fast_forward_replaces_the_previous_frontier_and_new_tail() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/");
        then.status(200).body("");
    });
    server.mock(|when, then| {
        when.method(GET).path("/other/mediawiki_history/");
        then.status(200).body(r#"<a href="2024-06/">2024-06/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/");
        then.status(200).body(
            r#"<a href="2024-06.testwiki.2024-06.tsv.bz2">June</a>
               <a href="2024-06.testwiki.2024-07.tsv.bz2">July</a>"#,
        );
    });
    let june_body = history_body("move");
    let june = server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/2024-06.testwiki.2024-06.tsv.bz2");
        then.status(200).body(june_body.clone());
    });
    let july_body = history_body("delete");
    let july = server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/2024-06.testwiki.2024-07.tsv.bz2");
        then.status(200).body(july_body.clone());
    });

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    inst.set_sync_state("full_snapshot_date", "2024-06-01").unwrap();
    inst.set_sync_state("incremental_date", "2024-06-01").unwrap();
    inst.set_sync_state("history_frontier_snapshot", "2024-05").unwrap();
    inst.set_sync_state("history_frontier_partition", "2024-06").unwrap();
    let cfg = Config { base_url: server.base_url() };
    let stats = maintain(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).unwrap();

    assert_eq!(stats.history_parts_fetched, 2);
    assert_eq!(june.hits(), 1, "expanded prior frontier must be replaced");
    assert_eq!(july.hits(), 1, "new partial frontier must be imported");
    assert_eq!(
        inst.sync_state("history_frontier_partition").unwrap().as_deref(),
        Some("2024-07")
    );
    assert_eq!(
        inst.sync_state("history_reconciled_snapshot").unwrap(),
        None,
        "a frontier update must not claim full reconciliation"
    );
}

#[test]
fn malformed_history_partition_rolls_back_all_rows() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/other/mediawiki_history/");
        then.status(200).body(r#"<a href="2024-06/">2024-06/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/");
        then.status(200).body(
            r#"<a href="2024-06.testwiki.all-time.tsv.bz2">all</a>"#,
        );
    });
    let body = malformed_history_body();
    server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/2024-06.testwiki.all-time.tsv.bz2");
        then.status(200).body(body.clone());
    });
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let cfg = Config { base_url: server.base_url() };

    assert!(
        reconcile_history(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).is_err()
    );
    assert!(inst.page_actions(1).unwrap().is_empty());
    assert_eq!(inst.sync_state("history_reconciled_snapshot").unwrap(), None);
}
