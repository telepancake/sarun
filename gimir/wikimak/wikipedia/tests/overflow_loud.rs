//! Page-id overflow is a LOUD import error, never a silent skip.
//!
//! PHASES §"page_id_overflow_errors_before_writes" left the policy an
//! implementer's choice; the silent-skip choice was a data-loss bug on
//! enwiki (bound 4M, page ids to ~8e7: ~95% of pages dropped invisibly
//! while the part watermark still landed, making the loss permanent).
//! This suite pins the reject-policy:
//!
//!   * `import` returns `Err(PageIdOverflow)` naming both numbers;
//!   * the offending page leaves NO depot bytes and NO sqlite rows —
//!     for a first-page overflow the whole instance stays untouched;
//!   * a `sync` run that hits an overflow FAILS and leaves NO part
//!     watermark, so the next run re-fetches instead of skipping a
//!     lossy part forever.

mod common;

use std::io::Cursor;

use httpmock::prelude::*;
use rusqlite::Connection;
use sha1::{Digest as _, Sha1};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{sync, Error};

use common::{list_files, make_instance};

/// One page (id 500) — above the test bound of 100.
const OVERFLOW_DOC: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Toobig</title><ns>0</ns><id>500</id>
    <revision>
      <id>5000</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="aa" xml:space="preserve">hi</text>
      <sha1>aa</sha1>
    </revision>
  </page>
</mediawiki>"#;

/// Total bytes across every file under `dir` (recursive one level per
/// tier layout: depot/{f0,f1,cold} hold flat files).
fn dir_bytes(dir: &std::path::Path) -> u64 {
    list_files(dir)
        .iter()
        .map(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn table_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or(0)
}

#[test]
fn overflow_is_a_loud_error_before_any_write() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 100);

    let mut stream = new_page_stream(Cursor::new(OVERFLOW_DOC.as_bytes().to_vec()));
    let err = instance
        .import(&mut stream)
        .expect_err("page id 500 past bound 100 must FAIL the import");
    match err {
        Error::PageIdOverflow {
            page_id,
            max_chain_id,
        } => {
            assert_eq!((page_id, max_chain_id), (500, 100));
        }
        other => panic!("expected PageIdOverflow, got {other:?}"),
    }
    // The message names both numbers — an operator can act on it.
    let msg = format!(
        "{}",
        Error::PageIdOverflow {
            page_id: 500,
            max_chain_id: 100
        }
    );
    assert!(msg.contains("500") && msg.contains("100"), "unhelpful: {msg}");

    // NO depot write: every tier is empty bytes on disk.
    for tier in ["depot/f0", "depot/f1", "depot/cold"] {
        assert_eq!(
            dir_bytes(&tmp.path().join(tier)),
            0,
            "{tier} has bytes after a rejected import"
        );
    }

    // NO sqlite rows: the overflow fired before the siteinfo capture,
    // the dirty stamp, and every per-page row.
    let conn = Connection::open(tmp.path().join("meta.db")).expect("meta.db");
    for table in [
        "revisions_seen",
        "page_to_title_id",
        "title_id_to_page",
        "title_intervals",
        "siteinfo_snapshots",
        "parts_seen",
        "instance_flags",
    ] {
        assert_eq!(
            table_count(&conn, table),
            0,
            "{table} has rows after a rejected import"
        );
    }
}

// ---------------------------------------------------------------------------
// The sync path: an overflow mid-part fails the RUN and must leave no
// `parts_seen` watermark — the skip signal for the next sync would
// otherwise permanently paper over the loss.
// ---------------------------------------------------------------------------

const PART: &str = "testwiki-20240601-pages-meta-history1.xml-p1p99";

#[test]
fn sync_overflow_fails_run_and_leaves_no_watermark() {
    let server = MockServer::start();
    let xml = OVERFLOW_DOC.as_bytes().to_vec();
    let sha1_hex = hex::encode(Sha1::digest(&xml));

    server.mock(|when, then| {
        when.method(GET)
            .path("/other/mediawiki_content_history/testwiki/");
        then.status(404);
    });
    server.mock(|when, then| {
        when.method(GET).path("/testwiki/");
        then.status(200).body(r#"<a href="20240601/">20240601/</a>"#);
    });
    let ds = format!(
        r#"{{"jobs":{{"metahistorybz2dump":{{"status":"done","files":{{
            "{PART}":{{"size":{},"url":"/testwiki/20240601/{PART}","sha1":"{sha1_hex}"}}
        }}}}}}}}"#,
        xml.len()
    );
    server.mock(move |when, then| {
        when.method(GET).path("/testwiki/20240601/dumpstatus.json");
        then.status(200).body(ds.clone());
    });
    server.mock(move |when, then| {
        when.method(GET).path(format!("/testwiki/20240601/{PART}"));
        then.status(200).body(xml.clone());
    });

    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 100);
    let client = reqwest::blocking::Client::new();
    let cfg = wikimak_mediawiki::Config {
        base_url: server.base_url(),
    };

    let r = sync(&inst, &client, &cfg, "testwiki", |_, _| ());
    assert!(r.is_err(), "overflowing part must fail the sync run");
    assert!(
        !inst.part_seen(PART).unwrap(),
        "watermark landed over a lossy part"
    );
}
