//! Parser acceptance suite. PHASES.md §W3-Rust-2 / SPEC §API.
//!
//! Streaming export-0.11 XML → `Iterator<Item = Result<Page>>`.

mod common;

use std::io::Cursor;

use chrono::{TimeZone, Utc};
use wikimak_mediawiki::{new_page_stream, site_info, Contributor};

use common::fixture;

// ---------------------------------------------------------------------------
// parser_three_pages_round_trip
// ---------------------------------------------------------------------------

#[test]
fn parser_three_pages_round_trip() {
    let body = fixture("export_three_pages.xml");
    let mut stream = new_page_stream(Cursor::new(body));

    let mut pages = Vec::new();
    while let Some(item) = stream.next() {
        pages.push(item.expect("every page in the three-page fixture must parse cleanly"));
    }
    assert_eq!(pages.len(), 3, "fixture has exactly 3 pages");

    // Page 1: redirect, one revision.
    let p1 = &pages[0];
    assert_eq!(p1.title, "Old Title");
    assert_eq!(p1.id, 1);
    assert_eq!(p1.namespace, 0);
    assert_eq!(p1.redirect_title.as_deref(), Some("New Title"));
    assert_eq!(p1.revisions.len(), 1);
    let r = &p1.revisions[0];
    assert_eq!(r.id, 100);
    assert_eq!(r.parent_id, None);
    match &r.contributor {
        Contributor::Named { username, user_id } => {
            assert_eq!(username, "Alice");
            assert_eq!(*user_id, 10);
        }
        other => panic!("expected Named contributor, got {other:?}"),
    }
    assert_eq!(r.text, "#REDIRECT [[New Title]]");
    assert_eq!(r.sha1, "qrstuvwxyzabcdefghij1234567890a");
    assert_eq!(
        r.timestamp,
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
    );

    // Page 2: two revisions, source order is oldest-first (200, 201).
    let p2 = &pages[1];
    assert_eq!(p2.title, "New Title");
    assert_eq!(p2.id, 2);
    assert_eq!(p2.redirect_title, None);
    assert_eq!(p2.revisions.len(), 2);
    assert_eq!(p2.revisions[0].id, 200);
    assert_eq!(p2.revisions[1].id, 201);
    assert_eq!(p2.revisions[1].parent_id, Some(200));
    assert!(p2.revisions[0].minor, "page2 rev0 has <minor/>");
    assert!(!p2.revisions[1].minor);
    assert_eq!(p2.revisions[0].text, "hello world");
    assert_eq!(p2.revisions[0].comment, "first revision");
    assert_eq!(p2.revisions[0].model, "wikitext");
    assert_eq!(p2.revisions[0].format, "text/x-wiki");
    assert_eq!(p2.revisions[0].origin, Some(200));

    // Page 3: one revision with text deleted="deleted".
    let p3 = &pages[2];
    assert_eq!(p3.revisions.len(), 1);
    let r3 = &p3.revisions[0];
    assert!(r3.text_hidden, "text deleted=\"deleted\" → text_hidden");
    assert_eq!(r3.text, "");
    assert!(!r3.comment_hidden, "comment NOT deleted");
    assert!(!r3.contributor_hidden, "contributor NOT deleted");
    assert_eq!(r3.sha1, "ccccccccccccccccccccccccccccccc");

    // site_info populated.
    let si = site_info(&stream).expect("site_info must be populated after streaming");
    assert_eq!(si.site_name, "TestWiki");
    assert_eq!(si.db_name, "testwiki");
    assert_eq!(si.case, "first-letter");
    assert!(si.generator.contains("MediaWiki Content File Export"));
    assert_eq!(
        si.namespaces.get(&0).expect("ns 0").name,
        ""
    );
    assert_eq!(si.namespaces.get(&1).expect("ns 1").name, "Talk");
    assert_eq!(si.namespaces.get(&10).expect("ns 10").name, "Template");
}

// ---------------------------------------------------------------------------
// parser_contributor_variants
//
// Feed `export_anon_and_user.xml` (anon + named), plus an inline doc
// with a `<contributor deleted="deleted" />` revision. Assert all three
// Contributor variants appear with the right `contributor_hidden` flag.
// ---------------------------------------------------------------------------

#[test]
fn parser_contributor_variants() {
    // Anon and named from fixture.
    let body = fixture("export_anon_and_user.xml");
    let mut stream = new_page_stream(Cursor::new(body));
    let page = stream
        .next()
        .expect("at least one page")
        .expect("page must parse");
    assert_eq!(page.revisions.len(), 2);

    match &page.revisions[0].contributor {
        Contributor::Anonymous { ip } => assert_eq!(ip, "192.0.2.42"),
        other => panic!("rev0: expected Anonymous, got {other:?}"),
    }
    match &page.revisions[1].contributor {
        Contributor::Named { username, user_id } => {
            assert_eq!(username, "Dave");
            assert_eq!(*user_id, 20);
        }
        other => panic!("rev1: expected Named, got {other:?}"),
    }
    assert!(!page.revisions[0].contributor_hidden);
    assert!(!page.revisions[1].contributor_hidden);

    // Hidden contributor: synthesized inline doc.
    let hidden_doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>x</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Hidden Author</title><ns>0</ns><id>9</id>
    <revision>
      <id>1000</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor deleted="deleted"/>
      <comment>c</comment>
      <model>wikitext</model><format>text/x-wiki</format>
      <text bytes="2" sha1="xxx" xml:space="preserve">hi</text>
      <sha1>xxx</sha1>
    </revision>
  </page>
</mediawiki>"#;
    let mut stream = new_page_stream(Cursor::new(hidden_doc.as_bytes().to_vec()));
    let page = stream
        .next()
        .expect("hidden-author page")
        .expect("must parse");
    let r = &page.revisions[0];
    assert!(matches!(r.contributor, Contributor::Hidden));
    assert!(r.contributor_hidden, "contributor deleted → contributor_hidden");
}

// ---------------------------------------------------------------------------
// parser_hidden_text_and_comment
//
// Revision with `<text deleted="deleted"/>` and `<comment
// deleted="deleted"/>` → text_hidden, comment_hidden, text == "".
// ---------------------------------------------------------------------------

#[test]
fn parser_hidden_text_and_comment() {
    let doc = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>x</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Doubly Hidden</title><ns>0</ns><id>42</id>
    <revision>
      <id>1234</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment deleted="deleted"/>
      <model>wikitext</model><format>text/x-wiki</format>
      <text deleted="deleted"/>
      <sha1>aaa</sha1>
    </revision>
  </page>
</mediawiki>"#;
    let mut stream = new_page_stream(Cursor::new(doc.as_bytes().to_vec()));
    let page = stream.next().expect("a page").expect("must parse");
    let r = &page.revisions[0];
    assert!(r.text_hidden);
    assert!(r.comment_hidden);
    assert_eq!(r.text, "");
    assert_eq!(r.comment, "");
}

// ---------------------------------------------------------------------------
// parser_suppressed_heuristic
//
// SPEC §"Wire facts": text deleted AND no bytes= AND no sha1= →
// suppressed. With either attribute present → not suppressed.
// ---------------------------------------------------------------------------

#[test]
fn parser_suppressed_heuristic() {
    // Case A: deleted text, no bytes, no sha1 attr → suppressed.
    let suppressed = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>x</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>S</title><ns>0</ns><id>1</id>
    <revision>
      <id>1</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment>
      <model>wikitext</model><format>text/x-wiki</format>
      <text deleted="deleted"/>
      <sha1>aaa</sha1>
    </revision>
  </page>
</mediawiki>"#;
    let mut stream = new_page_stream(Cursor::new(suppressed.as_bytes().to_vec()));
    let page = stream.next().expect("page").expect("parse");
    assert!(
        page.revisions[0].suppressed,
        "deleted text with no bytes= no sha1= must set suppressed"
    );

    // Case B: deleted text but bytes= present → NOT suppressed.
    let with_bytes = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>x</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>S</title><ns>0</ns><id>1</id>
    <revision>
      <id>1</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment>
      <model>wikitext</model><format>text/x-wiki</format>
      <text deleted="deleted" bytes="7"/>
      <sha1>aaa</sha1>
    </revision>
  </page>
</mediawiki>"#;
    let mut stream = new_page_stream(Cursor::new(with_bytes.as_bytes().to_vec()));
    let page = stream.next().expect("page").expect("parse");
    assert!(
        !page.revisions[0].suppressed,
        "deleted text with bytes= must NOT set suppressed"
    );
}

// ---------------------------------------------------------------------------
// parser_truncated_returns_error
//
// Judgment call (per brief): the iterator must yield ≥ 1 Err before
// stream end, and must NOT panic. The exact interleaving (Ok…Err then
// None, vs Ok…None then Err) is left to the implementer; this test
// only asserts: (a) no panic, (b) at least one Ok page is observed
// before the failure (the fixture is well-formed through page 1), and
// (c) at least one Err is observed across the full iteration.
// ---------------------------------------------------------------------------

#[test]
fn parser_truncated_returns_error() {
    let body = fixture("export_truncated.xml");
    let mut stream = new_page_stream(Cursor::new(body));

    let mut ok_count = 0usize;
    let mut err_count = 0usize;
    loop {
        match stream.next() {
            Some(Ok(_)) => ok_count += 1,
            Some(Err(_)) => err_count += 1,
            None => break,
        }
        // Guard against an infinite Err loop from a broken implementer.
        if ok_count + err_count > 1000 {
            panic!("parser yielded > 1000 items on a tiny truncated fixture; runaway");
        }
    }
    assert!(
        ok_count >= 1,
        "fixture's page 1 is well-formed; expected ≥ 1 Ok before truncation"
    );
    assert!(
        err_count >= 1,
        "truncated XML must surface at least one Err"
    );
}
