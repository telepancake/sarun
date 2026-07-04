//! Title pool + title_intervals tests.

mod common;

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use strpool::{Pool, PoolConfig};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;

use common::make_instance;

// ---------------------------------------------------------------------------
// title_id_pool_stores_normalized_title
//
// Import one page; meta.db.page_to_title_id(page_id) returns a title_id;
// the strpool entry at that id decodes to the normalized title bytes.
//
// Walk the strpool shard via `Pool::for_each_in_shard` and look for an
// entry whose global id matches.
// ---------------------------------------------------------------------------

#[test]
fn title_id_pool_stores_normalized_title() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    let doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>My Page</title><ns>0</ns><id>42</id>
    <revision>
      <id>1</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="1" sha1="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" xml:space="preserve">x</text>
      <sha1>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</sha1>
    </revision>
  </page>
</mediawiki>"#;
    let mut stream = new_page_stream(Cursor::new(doc.as_bytes().to_vec()));
    instance.import(&mut stream).expect("import");
    instance.flush().expect("flush");

    // Drop the instance so the strpool file is closed and we can reopen it.
    drop(instance);

    // page_to_title_id row.
    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    let title_id: i64 = conn
        .query_row(
            "SELECT title_id FROM page_to_title_id WHERE page_id = 42",
            [],
            |r| r.get(0),
        )
        .expect("row must exist");
    let title_id = title_id as u64;

    // Open the pool ourselves and walk the shard.
    let pool = Pool::open(
        &tmp.path().join("titles"),
        PoolConfig {
            shard_count: 1,
            seal_threshold_bytes: 1 << 20,
        },
        None,
    )
    .expect("open titles pool");

    let found: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let f = found.clone();
    pool.for_each_in_shard(0, |id, bytes| {
        if id == title_id {
            *f.lock().unwrap() = Some(bytes.to_vec());
        }
        Ok(())
    })
    .expect("walk shard");

    let bytes = found.lock().unwrap().clone().expect("title bytes for id");
    assert_eq!(bytes, b"My Page", "stored title bytes match input title");
}

// ---------------------------------------------------------------------------
// title_intervals_single_title_one_row
//
// Per PHASES guidance: pin the simpler single-title invariant. A page
// with one title (any number of revisions) yields exactly one
// `title_intervals` row with `end_ts IS NULL` for that page_id.
//
// Rename history (multi-row title_intervals) is W6 territory.
// ---------------------------------------------------------------------------

#[test]
fn title_intervals_single_title_one_row() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    let doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Stable Title</title><ns>0</ns><id>77</id>
    <revision>
      <id>1</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="1" sha1="aa" xml:space="preserve">a</text><sha1>aa</sha1>
    </revision>
    <revision>
      <id>2</id><parentid>1</parentid><timestamp>2024-01-02T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="bb" xml:space="preserve">ab</text><sha1>bb</sha1>
    </revision>
  </page>
</mediawiki>"#;
    let mut stream = new_page_stream(Cursor::new(doc.as_bytes().to_vec()));
    instance.import(&mut stream).expect("import");
    instance.flush().expect("flush");

    let conn = Connection::open(tmp.path().join("meta.db")).unwrap();
    let rows: Vec<(i64, Option<i64>)> = conn
        .prepare("SELECT start_ts, end_ts FROM title_intervals WHERE page_id = 77")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(rows.len(), 1, "exactly one title_intervals row for stable title");
    assert!(rows[0].1.is_none(), "end_ts IS NULL for open interval");
}
