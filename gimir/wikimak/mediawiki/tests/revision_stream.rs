//! `RevisionStream` — the streaming core under `PageStream`.
//!
//! Pins that per-revision streaming yields EXACTLY what the
//! page-collecting iterator yields (same fixture, field-for-field),
//! that a page's revisions arrive one at a time between `next_page`
//! calls, that abandoning a page mid-revisions skips cleanly to the
//! next page, and that truncation surfaces an error and kills the
//! stream (no runaway).

mod common;

use std::io::Cursor;

use wikimak_mediawiki::{new_page_stream, new_revision_stream, Revision};

use common::fixture;

fn assert_rev_eq(a: &Revision, b: &Revision, ctx: &str) {
    assert_eq!(a.id, b.id, "{ctx}: id");
    assert_eq!(a.parent_id, b.parent_id, "{ctx}: parent_id");
    assert_eq!(a.timestamp, b.timestamp, "{ctx}: timestamp");
    assert_eq!(a.contributor, b.contributor, "{ctx}: contributor");
    assert_eq!(a.minor, b.minor, "{ctx}: minor");
    assert_eq!(a.comment, b.comment, "{ctx}: comment");
    assert_eq!(a.origin, b.origin, "{ctx}: origin");
    assert_eq!(a.model, b.model, "{ctx}: model");
    assert_eq!(a.format, b.format, "{ctx}: format");
    assert_eq!(a.text, b.text, "{ctx}: text");
    assert_eq!(a.sha1, b.sha1, "{ctx}: sha1");
    assert_eq!(a.text_hidden, b.text_hidden, "{ctx}: text_hidden");
    assert_eq!(a.comment_hidden, b.comment_hidden, "{ctx}: comment_hidden");
    assert_eq!(
        a.contributor_hidden, b.contributor_hidden,
        "{ctx}: contributor_hidden"
    );
    assert_eq!(a.suppressed, b.suppressed, "{ctx}: suppressed");
}

#[test]
fn streaming_matches_page_collection() {
    let body = fixture("export_three_pages.xml");

    // Collected reference.
    let mut pages = Vec::new();
    let mut ps = new_page_stream(Cursor::new(body.clone()));
    while let Some(p) = ps.next() {
        pages.push(p.expect("fixture parses"));
    }
    assert_eq!(pages.len(), 3);

    // Streamed.
    let mut rs = new_revision_stream(Cursor::new(body));
    for want in &pages {
        let header = rs
            .next_page()
            .expect("a page per collected page")
            .expect("header parses");
        assert_eq!(header.title, want.title);
        assert_eq!(header.namespace, want.namespace);
        assert_eq!(header.id, want.id);
        assert_eq!(header.redirect_title, want.redirect_title);
        let mut got = Vec::new();
        while let Some(r) = rs.next_revision() {
            got.push(r.expect("revision parses"));
        }
        assert_eq!(got.len(), want.revisions.len(), "page {}", want.id);
        for (a, b) in got.iter().zip(&want.revisions) {
            assert_rev_eq(a, b, &format!("page {} rev {}", want.id, b.id));
        }
    }
    assert!(rs.next_page().is_none(), "no fourth page");
    // siteinfo observed by the streaming core too.
    assert_eq!(rs.site_info().expect("siteinfo").db_name, "testwiki");
}

#[test]
fn abandoning_a_page_skips_to_the_next() {
    let body = fixture("export_three_pages.xml");
    let mut rs = new_revision_stream(Cursor::new(body));

    // Page 1: take the header only, never touch its revisions.
    let h1 = rs.next_page().unwrap().unwrap();
    assert_eq!(h1.id, 1);
    // Page 2: reached cleanly, revisions intact.
    let h2 = rs.next_page().unwrap().unwrap();
    assert_eq!(h2.id, 2);
    let r = rs.next_revision().unwrap().unwrap();
    assert_eq!(r.id, 200);
    // Abandon page 2 mid-revisions (one of two consumed).
    let h3 = rs.next_page().unwrap().unwrap();
    assert_eq!(h3.id, 3);
    assert!(rs.next_page().is_none());
}

#[test]
fn truncated_stream_errors_and_dies() {
    let body = fixture("export_truncated.xml");
    let mut rs = new_revision_stream(Cursor::new(body));

    let mut ok_revs = 0usize;
    let mut errs = 0usize;
    let mut items = 0usize;
    while let Some(h) = rs.next_page() {
        items += 1;
        if h.is_err() {
            errs += 1;
            continue;
        }
        while let Some(r) = rs.next_revision() {
            items += 1;
            match r {
                Ok(_) => ok_revs += 1,
                Err(_) => errs += 1,
            }
        }
        assert!(items < 1000, "runaway on a tiny truncated fixture");
    }
    assert!(ok_revs >= 1, "page 1 of the fixture is well-formed");
    assert!(errs >= 1, "truncation must surface an Err");
    // Dead after the error.
    assert!(rs.next_page().is_none());
    assert!(rs.next_revision().is_none());
}
