//! Page-id overflow is a LOUD import error, never a silent skip — and
//! since the depot index auto-grows, the ONLY overflow left is the
//! depot's 2^40 chain-id sanity ceiling.
//!
//! PHASES §"page_id_overflow_errors_before_writes" left the policy an
//! implementer's choice; the silent-skip choice was a data-loss bug on
//! enwiki (bound 4M, page ids to ~8e7: ~95% of pages dropped invisibly
//! while the part watermark still landed, making the loss permanent).
//! The bound itself was then retired as a knob: a page id beyond the
//! configured hint GROWS the sparse index instead of erroring. This
//! suite pins both halves:
//!
//!   * a page id past the fresh-index hint imports fine — the index
//!     grows (sparse, real st_blocks effect) and the page round-trips;
//!   * a page id at/above the 2^40 ceiling makes `import` return
//!     `Err(PageIdOverflow)` naming both numbers, leaving NO depot
//!     bytes and NO sqlite rows — for a first-page overflow the whole
//!     instance stays untouched;
//!   * a `sync` run that hits the ceiling FAILS and leaves NO part
//!     watermark, so the next run re-fetches instead of skipping a
//!     lossy part forever.

mod common;

use std::io::Cursor;
use std::os::unix::fs::MetadataExt;

use httpmock::prelude::*;
use rusqlite::Connection;
use sha1::{Digest as _, Sha1};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{sync, Error};

use common::{list_files, make_instance};

/// The depot's chain-id sanity ceiling (2^40) — the one id class that
/// still rejects.
const CEILING: u64 = 1 << 40;

fn one_page_doc(page_id: u64) -> String {
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Toobig</title><ns>0</ns><id>{page_id}</id>
    <revision>
      <id>5000</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="aa" xml:space="preserve">hi</text>
      <sha1>aa</sha1>
    </revision>
  </page>
</mediawiki>"#
    )
}

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

/// The retired knob, end to end through the importer: page id 500
/// against a fresh-index hint of 100 imports by GROWING the index —
/// sparse on disk — and the page reads back; a reopen deriving its
/// hint from the on-disk size sees the same store.
#[test]
fn page_id_beyond_the_hint_grows_the_index_sparse() {
    let tmp = TempDir::new().unwrap();
    let index = tmp.path().join("depot").join("index");
    {
        let instance = make_instance(&tmp, 100);
        assert_eq!(std::fs::metadata(&index).unwrap().len(), 100 * 8);

        let mut stream = new_page_stream(Cursor::new(one_page_doc(500).into_bytes()));
        let stats = instance
            .import(&mut stream)
            .expect("page id 500 past hint 100 must import via index growth");
        assert_eq!(stats.revisions_new, 1);
        assert_eq!(
            instance.page_head_text(500).unwrap().unwrap(),
            b"hi",
            "grown page must round-trip"
        );
        instance.flush().unwrap();
    }
    // Growth = next_power_of_two(501) slots, and SPARSE: the file's
    // allocated blocks stay tiny however far the id jumped.
    let md = std::fs::metadata(&index).unwrap();
    assert_eq!(md.len(), 512 * 8, "index grew to next_power_of_two(id+1) slots");
    assert!(md.blocks() * 512 < 1 << 20, "grown index must be sparse");

    // Reopen with the derived hint (what the CLI does): same store.
    let instance = make_instance(&tmp, wikimak_wikipedia::max_chain_id_for_root(tmp.path()));
    assert_eq!(instance.page_head(500).unwrap().unwrap().rev_id, 5000);
    assert_eq!(instance.page_head_text(500).unwrap().unwrap(), b"hi");
}

#[test]
fn ceiling_overflow_is_a_loud_error_before_any_write() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 100);

    let mut stream = new_page_stream(Cursor::new(one_page_doc(CEILING).into_bytes()));
    let err = instance
        .import(&mut stream)
        .expect_err("page id at the 2^40 ceiling must FAIL the import");
    match err {
        Error::PageIdOverflow { page_id, ceiling } => {
            assert_eq!((page_id, ceiling), (CEILING, CEILING));
        }
        other => panic!("expected PageIdOverflow, got {other:?}"),
    }
    // The message names both numbers — an operator can act on it.
    let msg = format!(
        "{}",
        Error::PageIdOverflow {
            page_id: CEILING + 7,
            ceiling: CEILING
        }
    );
    assert!(
        msg.contains(&(CEILING + 7).to_string()) && msg.contains(&CEILING.to_string()),
        "unhelpful: {msg}"
    );

    // NO depot write: every tier is empty bytes on disk, and the index
    // kept its hint size (no growth toward the corrupt id either).
    for tier in ["depot/f0", "depot/f1", "depot/cold"] {
        assert_eq!(
            dir_bytes(&tmp.path().join(tier)),
            0,
            "{tier} has bytes after a rejected import"
        );
    }
    assert_eq!(
        std::fs::metadata(tmp.path().join("depot/index")).unwrap().len(),
        100 * 8,
        "a rejected id must not grow the index"
    );

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
    ] {
        assert_eq!(
            table_count(&conn, table),
            0,
            "{table} has rows after a rejected import"
        );
    }
    // instance_flags holds exactly the creation-time shard-count flag
    // — open-time bookkeeping, not an import effect. In particular NO
    // dirty stamp: the overflow fired before the first write.
    let flags: Vec<String> = conn
        .prepare("SELECT key FROM instance_flags ORDER BY key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(
        flags,
        vec!["title_shard_count".to_string()],
        "rejected import left flags beyond the creation one"
    );
}

// ---------------------------------------------------------------------------
// The sync path: a ceiling overflow mid-part fails the RUN and must
// leave no `parts_seen` watermark — the skip signal for the next sync
// would otherwise permanently paper over the loss.
// ---------------------------------------------------------------------------

const PART: &str = "testwiki-20240601-pages-meta-history1.xml-p1p99";

#[test]
fn sync_overflow_fails_run_and_leaves_no_watermark() {
    let server = MockServer::start();
    let xml = one_page_doc(CEILING).into_bytes();
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
