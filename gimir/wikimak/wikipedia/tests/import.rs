//! Import pipeline tests. SPEC §"Per-revision storage" and PHASES
//! §W3-Rust-3 / import.

mod common;

use std::io::Cursor;

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{
    ContributorMeta, FLAG_COMMENT_HIDDEN, FLAG_CONTRIBUTOR_HIDDEN, FLAG_SUPPRESSED,
    FLAG_TEXT_HIDDEN,
};

use common::{fixture, make_instance};

// ---------------------------------------------------------------------------
// Hand-crafted single page / single revision XML. Self-contained so we
// can pin per-rev metadata without juggling fixture files.
// ---------------------------------------------------------------------------

const ONE_PAGE_ONE_REV: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>TestWiki</sitename><dbname>testwiki</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Hello</title><ns>0</ns><id>7</id>
    <revision>
      <id>100</id>
      <timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>Alice</username><id>10</id></contributor>
      <comment>c1</comment>
      <model>wikitext</model><format>text/x-wiki</format>
      <text bytes="11" sha1="abcdefghijklmnopqrstuvwxyz01234" xml:space="preserve">hello world</text>
      <sha1>abcdefghijklmnopqrstuvwxyz01234</sha1>
    </revision>
  </page>
</mediawiki>"#;

// 1 page, 3 revisions, ids 200/201/202 newest-last.
const ONE_PAGE_THREE_REVS: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Many</title><ns>0</ns><id>8</id>
    <revision>
      <id>200</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>A</username><id>1</id></contributor>
      <comment>c1</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="1" sha1="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" xml:space="preserve">a</text>
      <sha1>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</sha1>
    </revision>
    <revision>
      <id>201</id><parentid>200</parentid><timestamp>2024-01-02T00:00:00Z</timestamp>
      <contributor><username>B</username><id>2</id></contributor>
      <comment>c2</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" xml:space="preserve">ab</text>
      <sha1>bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</sha1>
    </revision>
    <revision>
      <id>202</id><parentid>201</parentid><timestamp>2024-01-03T00:00:00Z</timestamp>
      <contributor><username>C</username><id>3</id></contributor>
      <comment>c3</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="3" sha1="ccccccccccccccccccccccccccccccc" xml:space="preserve">abc</text>
      <sha1>ccccccccccccccccccccccccccccccc</sha1>
    </revision>
  </page>
</mediawiki>"#;

// ---------------------------------------------------------------------------
// import_single_page_single_revision
// ---------------------------------------------------------------------------

#[test]
fn import_single_page_single_revision() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    let mut stream = new_page_stream(Cursor::new(ONE_PAGE_ONE_REV.as_bytes().to_vec()));
    let stats = instance.import(&mut stream).expect("import");

    assert_eq!(stats.pages, 1);
    assert_eq!(stats.revisions_new, 1);
    assert_eq!(stats.revisions_deduped, 0);
    assert_eq!(
        stats.sha1_ok + stats.sha1_fudged + stats.sha1_mismatch,
        1,
        "sha1 counters must sum to revisions imported"
    );

    let head = instance.page_head(7).expect("page_head").expect("Some");
    assert_eq!(head.rev_id, 100);
    assert_eq!(head.parent_id, 0, "no parentid → encoded as 0");
    match head.contributor {
        ContributorMeta::Named { username, user_id } => {
            assert_eq!(username, "Alice");
            assert_eq!(user_id, 10);
        }
        other => panic!("expected Named, got {other:?}"),
    }
    assert_eq!(head.comment, "c1");
    assert_eq!(head.sha1, "abcdefghijklmnopqrstuvwxyz01234");

    let mut hist = instance.page_history(7).expect("history");
    let entry = hist.next().expect("one entry").expect("ok");
    assert_eq!(entry.meta.rev_id, 100);
    let text = (entry.fetch_text)().expect("text");
    assert_eq!(text, b"hello world");
    assert!(hist.next().is_none(), "only one revision");
}

// ---------------------------------------------------------------------------
// import_page_with_multiple_revisions
//
// History walks newest-first.
// ---------------------------------------------------------------------------

#[test]
fn import_page_with_multiple_revisions() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    let mut stream = new_page_stream(Cursor::new(ONE_PAGE_THREE_REVS.as_bytes().to_vec()));
    let stats = instance.import(&mut stream).expect("import");
    assert_eq!(stats.pages, 1);
    assert_eq!(stats.revisions_new, 3);

    let head = instance.page_head(8).expect("page_head").expect("Some");
    assert_eq!(head.rev_id, 202, "head = newest rev");

    let hist: Vec<_> = instance
        .page_history(8)
        .expect("history")
        .map(|e| e.expect("ok"))
        .collect();
    assert_eq!(hist.len(), 3);
    assert_eq!(hist[0].meta.rev_id, 202, "newest-first");
    assert_eq!(hist[1].meta.rev_id, 201);
    assert_eq!(hist[2].meta.rev_id, 200);
}

// ---------------------------------------------------------------------------
// import_multiple_pages_independent
//
// Three pages in one stream; each page's history is its own.
// ---------------------------------------------------------------------------

#[test]
fn import_multiple_pages_independent() {
    let doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>P1</title><ns>0</ns><id>11</id>
    <revision><id>1100</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="aa" xml:space="preserve">p1</text><sha1>aa</sha1></revision>
    <revision><id>1101</id><parentid>1100</parentid><timestamp>2024-01-02T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="3" sha1="bb" xml:space="preserve">p1.</text><sha1>bb</sha1></revision>
  </page>
  <page>
    <title>P2</title><ns>0</ns><id>22</id>
    <revision><id>2200</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="cc" xml:space="preserve">p2</text><sha1>cc</sha1></revision>
    <revision><id>2201</id><parentid>2200</parentid><timestamp>2024-01-02T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="3" sha1="dd" xml:space="preserve">p2.</text><sha1>dd</sha1></revision>
  </page>
  <page>
    <title>P3</title><ns>0</ns><id>33</id>
    <revision><id>3300</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="ee" xml:space="preserve">p3</text><sha1>ee</sha1></revision>
    <revision><id>3301</id><parentid>3300</parentid><timestamp>2024-01-02T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="3" sha1="ff" xml:space="preserve">p3.</text><sha1>ff</sha1></revision>
  </page>
</mediawiki>"#;

    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);
    let mut stream = new_page_stream(Cursor::new(doc.as_bytes().to_vec()));
    let stats = instance.import(&mut stream).expect("import");
    assert_eq!(stats.pages, 3);
    assert_eq!(stats.revisions_new, 6);

    assert_eq!(instance.page_head(11).unwrap().unwrap().rev_id, 1101);
    assert_eq!(instance.page_head(22).unwrap().unwrap().rev_id, 2201);
    assert_eq!(instance.page_head(33).unwrap().unwrap().rev_id, 3301);

    // Cross-page contamination check: page 11's history is only revs 1100/1101.
    let hist: Vec<_> = instance
        .page_history(11)
        .unwrap()
        .map(|e| e.unwrap().meta.rev_id)
        .collect();
    assert_eq!(hist, vec![1101, 1100]);
}

// ---------------------------------------------------------------------------
// import_three_pages_fixture
//
// Feed the export_three_pages.xml fixture through the full pipeline.
// ---------------------------------------------------------------------------

#[test]
fn import_three_pages_fixture() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);
    let body = fixture("export_three_pages.xml");
    let mut stream = new_page_stream(Cursor::new(body));

    let stats = instance.import(&mut stream).expect("import");
    assert_eq!(stats.pages, 3);
    // Page 1: 1 rev; page 2: 2 revs; page 3: 1 rev = 4 revs total.
    assert_eq!(stats.revisions_new, 4);

    // Spot-check page 2's head (rev 201 with the "hello world, expanded." text).
    let head = instance.page_head(2).unwrap().unwrap();
    assert_eq!(head.rev_id, 201);
    let mut hist = instance.page_history(2).unwrap();
    let newest = hist.next().unwrap().unwrap();
    assert_eq!(newest.meta.rev_id, 201);
    let text = (newest.fetch_text)().unwrap();
    assert_eq!(text, b"hello world, expanded.");
}

// ---------------------------------------------------------------------------
// contributor_variants_round_trip
//
// export_anon_and_user.xml: rev0 = Anonymous, rev1 = Named.
// Round-trip through the depot frame, assert both come back intact.
// ---------------------------------------------------------------------------

#[test]
fn contributor_variants_round_trip() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);
    let body = fixture("export_anon_and_user.xml");
    let mut stream = new_page_stream(Cursor::new(body));
    instance.import(&mut stream).expect("import");

    let hist: Vec<_> = instance
        .page_history(4)
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    // History is newest-first, source order is oldest-first (400, 401).
    // So hist[0] = rev 401 (Named), hist[1] = rev 400 (Anonymous).
    assert_eq!(hist.len(), 2);
    match &hist[0].meta.contributor {
        ContributorMeta::Named { username, user_id } => {
            assert_eq!(username, "Dave");
            assert_eq!(*user_id, 20);
        }
        other => panic!("expected Named for rev 401, got {other:?}"),
    }
    match &hist[1].meta.contributor {
        ContributorMeta::Anonymous { ip } => assert_eq!(ip, "192.0.2.42"),
        other => panic!("expected Anonymous for rev 400, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// hidden_and_suppressed_flags_round_trip
//
// Hand-rolled doc with deleted text + deleted comment + deleted
// contributor → assert TEXT_HIDDEN | COMMENT_HIDDEN |
// CONTRIBUTOR_HIDDEN flags survive the depot round-trip. The same
// revision has no bytes= no sha1= → SUPPRESSED too.
// ---------------------------------------------------------------------------

#[test]
fn hidden_and_suppressed_flags_round_trip() {
    let doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Hidden</title><ns>0</ns><id>55</id>
    <revision>
      <id>5500</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor deleted="deleted"/>
      <comment deleted="deleted"/>
      <model>wikitext</model><format>text/x-wiki</format>
      <text deleted="deleted"/>
    </revision>
  </page>
</mediawiki>"#;
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);
    let mut stream = new_page_stream(Cursor::new(doc.as_bytes().to_vec()));
    instance.import(&mut stream).expect("import");

    let head = instance.page_head(55).unwrap().unwrap();
    let flags = head.flags;
    assert!(
        flags & FLAG_TEXT_HIDDEN != 0,
        "TEXT_HIDDEN must be set; flags = {flags:#x}"
    );
    assert!(
        flags & FLAG_COMMENT_HIDDEN != 0,
        "COMMENT_HIDDEN must be set; flags = {flags:#x}"
    );
    assert!(
        flags & FLAG_CONTRIBUTOR_HIDDEN != 0,
        "CONTRIBUTOR_HIDDEN must be set; flags = {flags:#x}"
    );
    assert!(
        flags & FLAG_SUPPRESSED != 0,
        "SUPPRESSED (no bytes= no sha1=) must be set; flags = {flags:#x}"
    );
    assert!(matches!(head.contributor, ContributorMeta::Hidden));
}
