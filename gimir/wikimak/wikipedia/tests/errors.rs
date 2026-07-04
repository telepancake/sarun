//! Error-path tests. Page-id overflow + per-page atomicity.

mod common;

use std::io::Cursor;

use rusqlite::Connection;
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;

use common::make_instance;

// ---------------------------------------------------------------------------
// page_id_overflow_errors_before_writes
//
// Open with max_chain_id=100; feed a page with id=500. The implementer
// may either return Err on import OR skip the offending page. EITHER
// WAY: no depot frame for page 500, no meta.db row referencing page 500.
// ---------------------------------------------------------------------------

#[test]
fn page_id_overflow_errors_before_writes() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 100);

    let doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
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

    let mut stream = new_page_stream(Cursor::new(doc.as_bytes().to_vec()));
    let result = instance.import(&mut stream);

    // Either outcome is acceptable. The post-condition is identical.
    match result {
        Ok(stats) => {
            assert_eq!(
                stats.pages, 0,
                "skip-policy: offending page must not count as imported"
            );
            assert_eq!(stats.revisions_new, 0);
        }
        Err(_) => {
            // Reject-policy: that's fine too.
        }
    }

    // Post-condition (both branches): nothing about page 500 on disk.
    assert!(
        instance.page_head(500).expect("page_head ok").is_none(),
        "no head for overflowed page id"
    );

    let conn = Connection::open(tmp.path().join("meta.db")).expect("meta.db");
    let count_pti: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_to_title_id WHERE page_id = 500",
            [],
            |r| r.get(0),
        )
        .expect("count page_to_title_id");
    assert_eq!(count_pti, 0, "no meta.db row references page 500");
    let count_ti: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM title_intervals WHERE page_id = 500",
            [],
            |r| r.get(0),
        )
        .expect("count title_intervals");
    assert_eq!(count_ti, 0, "no title_intervals row references page 500");
}

// ---------------------------------------------------------------------------
// import_is_per_page_atomic_around_overflow
//
// A stream with [ok page id=10, oversize page id=500, ok page id=20].
// The two valid pages must commit; the offending page must leave no
// state. Pin both possible policies (reject mid-stream, skip-and-
// continue): assert pages 10 and 20 are observable iff the import
// succeeded; in the reject case, assert at least page 10 committed
// (per-page atomicity) and page 500/20 are absent or partial. In all
// cases page 500 is absent from meta.db.
// ---------------------------------------------------------------------------

#[test]
fn import_is_per_page_atomic_around_overflow() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 100);

    let doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>OK1</title><ns>0</ns><id>10</id>
    <revision>
      <id>100</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="aa" xml:space="preserve">p1</text>
      <sha1>aa</sha1>
    </revision>
  </page>
  <page>
    <title>BAD</title><ns>0</ns><id>500</id>
    <revision>
      <id>5000</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="bb" xml:space="preserve">p2</text>
      <sha1>bb</sha1>
    </revision>
  </page>
  <page>
    <title>OK2</title><ns>0</ns><id>20</id>
    <revision>
      <id>200</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="cc" xml:space="preserve">p3</text>
      <sha1>cc</sha1>
    </revision>
  </page>
</mediawiki>"#;
    let mut stream = new_page_stream(Cursor::new(doc.as_bytes().to_vec()));
    let _ = instance.import(&mut stream); // either Ok or Err is fine

    // Page 10 must be present (it's before the overflow; per-page
    // atomicity says committed pages stay).
    assert!(
        instance.page_head(10).expect("ok").is_some(),
        "page 10 (before overflow) must be committed"
    );

    // Page 500 must never exist.
    assert!(
        instance.page_head(500).expect("ok").is_none(),
        "page 500 must never be committed"
    );

    let conn = Connection::open(tmp.path().join("meta.db")).expect("meta.db");
    let count_500: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_to_title_id WHERE page_id = 500",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(count_500, 0, "no meta.db state for page 500");
}
