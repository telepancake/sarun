//! End-to-end `sync` acceptance: an httpmock server stands in for
//! dumps.wikimedia.org (legacy branch), serving a dumpstatus.json whose
//! one part is the `export_three_pages.xml` fixture. Asserts:
//!   - first sync fetches the part and the pages land in the depot;
//!   - the part is watermarked in `parts_seen`;
//!   - a second sync skips the part (no re-fetch: hit counter static)
//!     and imports nothing new;
//!   - a checksum mismatch leaves NO watermark but preserves complete pages.

mod common;

use std::io::Write;

use bzip2::write::BzEncoder;
use bzip2::Compression;
use httpmock::prelude::*;
use reqwest::blocking::Client;
use rusqlite::Connection;
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
    history_body_with_schema(event_type, 78)
}

fn history_body_with_schema(event_type: &str, columns: usize) -> Vec<u8> {
    let (page, revision) = match columns {
        76 => (26, 58),
        78 => (28, 60),
        _ => panic!("unsupported test schema"),
    };
    let mut fields = vec![""; columns];
    fields[0] = "testwiki";
    fields[1] = "123";
    fields[2] = "page";
    fields[3] = event_type;
    fields[4] = "2024-06-01 12:34:56.0";
    fields[5] = "action comment";
    fields[6] = "9";
    fields[9] = "Editor";
    fields[page] = "1";
    fields[page + 1] = "Old title";
    fields[page + 2] = "Current title";
    fields[page + 3] = "0";
    fields[page + 5] = "0";
    fields[page + 8] = "false";
    let mut encoder = BzEncoder::new(Vec::new(), Compression::default());
    writeln!(encoder, "{}", fields.join("\t")).unwrap();
    // Old deletion log events can retain their titles but have no page id.
    let mut orphan_page = fields.clone();
    orphan_page[1] = "124";
    orphan_page[3] = "delete";
    orphan_page[page] = "0";
    orphan_page[page + 1] = "Deleted title";
    orphan_page[page + 2] = "Deleted title";
    orphan_page[page + 8] = "true";
    writeln!(encoder, "{}", orphan_page.join("\t")).unwrap();
    let mut revision_fields = vec![""; columns];
    revision_fields[0] = "testwiki";
    revision_fields[2] = "revision";
    revision_fields[3] = "create";
    revision_fields[4] = "2024-06-01 12:00:00.0";
    revision_fields[page] = "1";
    revision_fields[revision] = "100";
    revision_fields[revision + 3] = "text,user";
    revision_fields[revision + 4] = "true";
    revision_fields[revision + 10] = "true";
    revision_fields[revision + 11] = "2024-06-02 00:00:00.0";
    writeln!(encoder, "{}", revision_fields.join("\t")).unwrap();
    let mut visible_revision = vec![""; columns];
    visible_revision[0] = "testwiki";
    visible_revision[2] = "revision";
    visible_revision[3] = "create";
    visible_revision[4] = "2024-06-01 12:01:00.0";
    visible_revision[page] = "1";
    visible_revision[revision] = "101";
    visible_revision[revision + 4] = "false";
    visible_revision[revision + 10] = "false";
    writeln!(encoder, "{}", visible_revision.join("\t")).unwrap();
    // Imported/orphan revisions can have no upstream page id while still
    // carrying page-deletion visibility metadata.
    let mut orphan_revision = vec![""; columns];
    orphan_revision[0] = "testwiki";
    orphan_revision[2] = "revision";
    orphan_revision[3] = "create";
    orphan_revision[4] = "2024-06-01 12:02:00.0";
    orphan_revision[page + 1] = "Imported orphan";
    orphan_revision[page + 3] = "0";
    orphan_revision[revision] = "102";
    orphan_revision[revision + 4] = "false";
    orphan_revision[revision + 10] = "true";
    orphan_revision[revision + 11] = "2024-06-03 00:00:00.0";
    writeln!(encoder, "{}", orphan_revision.join("\t")).unwrap();
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

fn page_history_body(event_type: &str, page_id: &str) -> Vec<u8> {
    let mut fields = vec![""; 78];
    fields[0] = "testwiki";
    fields[1] = page_id;
    fields[2] = "page";
    fields[3] = event_type;
    fields[4] = "2024-07-01 12:34:56.0";
    fields[28] = page_id;
    fields[29] = "Historical title";
    fields[30] = "Current title";
    fields[31] = "0";
    fields[33] = "0";
    fields[36] = "false";
    let mut encoder = BzEncoder::new(Vec::new(), Compression::default());
    writeln!(encoder, "{}", fields.join("\t")).unwrap();
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

fn dictionary_seed_xml(pages: usize) -> Vec<u8> {
    let mut body = String::new();
    for page in 1..=pages {
        let text = format!(
            "Article {page} with representative shared encyclopedia markup. {}",
            "abcdefghij".repeat(1024)
        );
        body.push_str(&format!(
            "<page><title>Article {page}</title><ns>0</ns><id>{page}</id>\
             <revision><id>{}</id><timestamp>2024-01-01T00:00:00Z</timestamp>\
             <contributor><username>E</username><id>1</id></contributor>\
             <text xml:space=\"preserve\">{text}</text></revision></page>",
            10_000 + page
        ));
    }
    format!(
        "<mediawiki xmlns=\"http://www.mediawiki.org/xml/export-0.11/\" version=\"0.11\">\
         <siteinfo><sitename>D</sitename><dbname>d</dbname><base>x</base><generator>g</generator>\
         <case>first-letter</case><namespaces><namespace key=\"0\" case=\"first-letter\"/>\
         </namespaces></siteinfo>{body}</mediawiki>"
    )
    .into_bytes()
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
    assert!(
        !tmp.path().join(".downloads").exists(),
        "sync must stream directly without a compressed staging copy"
    );
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
fn initial_full_sync_finalizes_exactly_one_revision_dictionary() {
    let server = MockServer::start();
    let xml = dictionary_seed_xml(160);
    let sha1_hex = hex::encode(Sha1::digest(&xml));
    mount(&server, &xml, &sha1_hex);

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let client = Client::new();
    let cfg = Config { base_url: server.base_url() };
    sync(&inst, &client, &cfg, "testwiki", |_, _| ()).unwrap();

    let pointer = std::fs::read(tmp.path().join("dictionaries/revision.current")).unwrap();
    let dictionaries = std::fs::read_dir(tmp.path().join("dictionaries"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "zdict"))
        .count();
    assert_eq!(dictionaries, 1);

    // An explicit later sync may skip every part, but must not train a
    // successor dictionary or rewrite the active pointer.
    sync(&inst, &client, &cfg, "testwiki", |_, _| ()).unwrap();
    assert_eq!(
        std::fs::read(tmp.path().join("dictionaries/revision.current")).unwrap(),
        pointer
    );
    assert_eq!(
        std::fs::read_dir(tmp.path().join("dictionaries"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "zdict"))
            .count(),
        1
    );
}

#[test]
fn checksum_mismatch_salvages_complete_pages_but_leaves_no_watermark() {
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
        inst.page_head_text(1).unwrap().is_some(),
        "complete independently parsed pages should survive a whole-part checksum mismatch"
    );
    let retry_server = MockServer::start();
    let correct = hex::encode(Sha1::digest(&xml));
    mount(&retry_server, &xml, &correct);
    let retry_cfg = Config { base_url: retry_server.base_url() };
    let (_, retry) =
        sync(&inst, &client, &retry_cfg, "testwiki", |_, _| ()).unwrap();
    assert_eq!(retry.import.revisions_new, 0, "valid prefix should deduplicate");
    assert!(inst.part_seen(PART).unwrap(), "successful retry advances watermark");
}

#[test]
fn truncated_xml_salvages_complete_pages_before_the_damage() {
    let server = MockServer::start();
    let xml = fixture("export_three_pages.xml");
    let marker = b"</page>";
    let end = xml
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap()
        + marker.len();
    let truncated = xml[..end].to_vec();
    let digest = hex::encode(Sha1::digest(&xml));
    mount(&server, &truncated, &digest);

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let cfg = Config { base_url: server.base_url() };
    assert!(sync(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).is_err());
    assert!(inst.page_head_text(1).unwrap().is_some());
    assert!(inst.page_head_text(2).unwrap().is_none());
    assert!(!inst.part_seen(PART).unwrap());
}

#[test]
fn maintenance_consumes_daily_adds_changes_without_full_redownload() {
    let server = MockServer::start();
    let xml = fixture("export_three_pages.xml");
    let sha1_hex = hex::encode(Sha1::digest(&xml));
    let full_part = mount(&server, &xml, &sha1_hex);
    let tmp = TempDir::new().unwrap();
    // The CLI opens stores with a neutral local label; the persisted
    // `wiki_dbname` supplied to maintain is the upstream identity.
    let mut instance_cfg = common::cfg(tmp.path().to_path_buf(), 1024);
    instance_cfg.dbname = "wiki".into();
    let inst = wikimak_wikipedia::Instance::open(instance_cfg).unwrap();
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
    let unattached_actions: i64 = Connection::open(tmp.path().join("meta.db"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM page_actions WHERE page_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unattached_actions, 1);
    let visibility = inst.revision_visibility(100).unwrap().unwrap();
    assert_eq!(visibility.deleted_parts, "text,user");
    assert!(visibility.parts_are_suppressed);
    assert!(visibility.deleted_by_page_deletion);
    assert!(
        inst.revision_visibility(101).unwrap().is_none(),
        "fully visible revisions must not consume visibility rows"
    );
    let orphan = inst.revision_visibility(102).unwrap().unwrap();
    assert!(orphan.deleted_by_page_deletion);
    assert_eq!(orphan.page_deletion_timestamp, "2024-06-03 00:00:00.0");
}

#[test]
fn history_import_accepts_legacy_76_column_schema() {
    let server = MockServer::start();
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
    let body = history_body_with_schema("move", 76);
    server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-06/testwiki/2024-06.testwiki.all-time.tsv.bz2");
        then.status(200).body(body.clone());
    });

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let cfg = Config { base_url: server.base_url() };
    let stats =
        reconcile_history(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).unwrap();
    assert_eq!(stats.history_parts_fetched, 1);
    assert_eq!(inst.page_actions(1).unwrap().len(), 1);
    assert!(inst.revision_visibility(100).unwrap().unwrap().parts_are_suppressed);
}

#[test]
fn suppression_metadata_never_removes_archived_revision_content() {
    let server = MockServer::start();
    let xml = fixture("export_three_pages.xml");
    let sha1_hex = hex::encode(Sha1::digest(&xml));
    mount(&server, &xml, &sha1_hex);
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let cfg = Config { base_url: server.base_url() };

    sync(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).unwrap();
    let before = inst.page_head_text(1).unwrap().unwrap();
    reconcile_history(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).unwrap();

    assert!(inst.revision_visibility(100).unwrap().unwrap().parts_are_suppressed);
    assert_eq!(inst.page_head_text(1).unwrap().unwrap(), before);
    let revisions: Vec<_> = inst.page_history(1).unwrap().collect();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].as_ref().unwrap().meta.rev_id, 100);
}

#[test]
fn incomplete_partition_listing_never_advances_history_snapshot() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/other/mediawiki_history/");
        then.status(200).body(r#"<a href="2024-07/">2024-07/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-07/testwiki/");
        then.status(200).body(
            r#"<a href="2024-07.testwiki.2024-05.tsv.bz2">May</a>
               <a href="2024-07.testwiki.2024-07.tsv.bz2">July</a>"#,
        );
    });
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    inst.set_sync_state("history_frontier_snapshot", "2024-06").unwrap();
    inst.set_sync_state("history_reconciled_snapshot", "2024-06").unwrap();
    let cfg = Config { base_url: server.base_url() };

    let error =
        reconcile_history(&inst, &Client::new(), &cfg, "testwiki", |_, _| ())
            .unwrap_err()
            .to_string();
    assert!(error.contains("incomplete"), "{error}");
    assert_eq!(
        inst.sync_state("history_reconciled_snapshot").unwrap().as_deref(),
        Some("2024-06")
    );
}

#[test]
fn new_history_snapshot_replaces_every_partition() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/other/incr/testwiki/");
        then.status(200).body("");
    });
    server.mock(|when, then| {
        when.method(GET).path("/other/mediawiki_history/");
        then.status(200).body(r#"<a href="2024-07/">2024-07/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-07/testwiki/");
        then.status(200).body(
            r#"<a href="2024-07.testwiki.2024-06.tsv.bz2">June</a>
               <a href="2024-07.testwiki.2024-07.tsv.bz2">July</a>"#,
        );
    });
    let june_body = history_body("move");
    let june = server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-07/testwiki/2024-07.testwiki.2024-06.tsv.bz2");
        then.status(200).body(june_body.clone());
    });
    let july_body = page_history_body("delete", "2");
    let july = server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-07/testwiki/2024-07.testwiki.2024-07.tsv.bz2");
        then.status(200).body(july_body.clone());
    });

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let db = Connection::open(tmp.path().join("meta.db")).unwrap();
    db.execute(
        "INSERT INTO page_actions VALUES(
            'obsolete:1','obsolete',NULL,0,'delete','2001-01-01','',
            NULL,'',99,'Gone','Gone',0,0,1
        )",
        [],
    ).unwrap();
    db.execute(
        "INSERT INTO revision_visibility VALUES(999,99,'obsolete','text',1,0,'')",
        [],
    ).unwrap();
    drop(db);
    inst.set_sync_state("full_snapshot_date", "2024-06-01").unwrap();
    inst.set_sync_state("incremental_date", "2024-06-01").unwrap();
    inst.set_sync_state("history_frontier_snapshot", "2024-05").unwrap();
    inst.set_sync_state("history_frontier_partition", "2024-06").unwrap();
    let cfg = Config { base_url: server.base_url() };
    let stats = maintain(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).unwrap();

    assert_eq!(stats.history_parts_fetched, 2);
    assert_eq!(june.hits(), 1, "every partition belongs to the new snapshot");
    assert_eq!(july.hits(), 1, "every partition belongs to the new snapshot");
    assert_eq!(
        inst.sync_state("history_frontier_partition").unwrap().as_deref(),
        Some("2024-07")
    );
    assert_eq!(
        inst.sync_state("history_reconciled_snapshot").unwrap().as_deref(),
        Some("2024-07"),
        "the complete replacement is a reconciled snapshot"
    );
    assert!(inst.page_actions(99).unwrap().is_empty(), "stale action survived");
    assert!(
        inst.revision_visibility(999).unwrap().is_none(),
        "stale visibility survived"
    );
}

#[test]
fn malformed_history_partition_rolls_back_all_rows() {
    let initial = MockServer::start();
    mount_history(&initial);
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 1024);
    let initial_cfg = Config { base_url: initial.base_url() };
    reconcile_history(&inst, &Client::new(), &initial_cfg, "testwiki", |_, _| ()).unwrap();
    let before = inst.page_actions(1).unwrap();
    assert_eq!(before.len(), 1);

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/other/mediawiki_history/");
        then.status(200).body(r#"<a href="2024-07/">2024-07/</a>"#);
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-07/testwiki/");
        then.status(200).body(
            r#"<a href="2024-07.testwiki.2024-06.tsv.bz2">June</a>
               <a href="2024-07.testwiki.2024-07.tsv.bz2">July</a>"#,
        );
    });
    let good_body = page_history_body("delete", "2");
    server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-07/testwiki/2024-07.testwiki.2024-06.tsv.bz2");
        then.status(200).body(good_body.clone());
    });
    let body = malformed_history_body();
    server.mock(move |when, then| {
        when.method(GET)
            .path("/other/mediawiki_history/2024-07/testwiki/2024-07.testwiki.2024-07.tsv.bz2");
        then.status(200).body(body.clone());
    });
    let cfg = Config { base_url: server.base_url() };

    assert!(
        reconcile_history(&inst, &Client::new(), &cfg, "testwiki", |_, _| ()).is_err()
    );
    assert_eq!(inst.page_actions(1).unwrap(), before);
    assert_eq!(
        inst.sync_state("history_reconciled_snapshot").unwrap().as_deref(),
        Some("2024-06")
    );
    assert!(
        inst.page_actions(2).unwrap().is_empty(),
        "rows from the first part of a failed snapshot leaked"
    );
}
