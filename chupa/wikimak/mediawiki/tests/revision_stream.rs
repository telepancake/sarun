//! `RevisionStream` — the streaming core under `PageStream`.
//!
//! Pins that per-revision streaming yields EXACTLY what the
//! page-collecting iterator yields (same fixture, field-for-field),
//! that a page's revisions arrive one at a time between `next_page`
//! calls, that abandoning a page mid-revisions skips cleanly to the
//! next page, and that truncation surfaces an error and kills the
//! stream (no runaway).

mod common;

use std::io::{self, Cursor, Read, Write};

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

struct ChunkedReader {
    input: Vec<u8>,
    position: usize,
    chunk_size: usize,
}

impl Read for ChunkedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.position == self.input.len() {
            return Ok(0);
        }
        let amount = self
            .chunk_size
            .min(output.len())
            .min(self.input.len() - self.position);
        output[..amount].copy_from_slice(&self.input[self.position..self.position + amount]);
        self.position += amount;
        Ok(amount)
    }
}

#[test]
fn next_revision_to_handles_xml_entities_and_cdata_across_input_chunks() {
    let body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id><revision><id>2</id>
        <timestamp>2024-01-01T00:00:00Z</timestamp><contributor><username>U</username><id>3</id></contributor>
        <text xml:space="preserve">left &amp; &lt; <![CDATA[cdata ] text]]> right &#x1F600;</text>
      </revision></page>
    </mediawiki>"#
        .to_vec();
    let mut stream = new_revision_stream(ChunkedReader {
        input: body,
        position: 0,
        chunk_size: 1,
    });
    assert_eq!(stream.next_page().unwrap().unwrap().id, 1);
    let mut text = Vec::new();
    let revision = stream
        .next_revision_to(&mut text)
        .unwrap()
        .unwrap();
    assert!(revision.text.is_empty(), "streamed text is not materialized");
    assert_eq!(
        text,
        "left & < cdata ] text right 😀".as_bytes(),
        "entity and CDATA decoding must survive reader chunk boundaries"
    );
    assert!(stream.next_revision().is_none());
}

struct BoundedWrite {
    largest_write: usize,
    total: u64,
}

impl Write for BoundedWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.largest_write = self.largest_write.max(bytes.len());
        if bytes.len() > 16 << 10 {
            return Err(io::Error::other("parser attempted one revision-sized write"));
        }
        self.total += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn next_revision_to_delivers_large_text_in_bounded_writes() {
    let payload_bytes = (2 << 20) + 37;
    let mut body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id><revision><id>2</id>
        <timestamp>2024-01-01T00:00:00Z</timestamp><contributor><username>U</username><id>3</id></contributor>
        <text xml:space="preserve">"#
        .to_vec();
    body.resize(body.len() + payload_bytes, b'x');
    body.extend_from_slice(b"</text></revision></page></mediawiki>");

    let mut stream = new_revision_stream(Cursor::new(body));
    assert_eq!(stream.next_page().unwrap().unwrap().id, 1);
    let mut sink = BoundedWrite {
        largest_write: 0,
        total: 0,
    };
    let revision = stream.next_revision_to(&mut sink).unwrap().unwrap();
    assert!(revision.text.is_empty());
    assert_eq!(sink.total, payload_bytes as u64);
    assert!(sink.largest_write <= 16 << 10);
}

#[test]
fn streamed_text_rejects_invalid_utf8_across_input_chunks() {
    let mut body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id><revision>
        <id>2</id><timestamp>2024-01-01T00:00:00Z</timestamp><text>prefix "#
        .to_vec();
    // E2 starts a three-byte scalar, but 28 is not a UTF-8 continuation.
    body.extend_from_slice(&[0xe2, 0x28, 0xa1]);
    body.extend_from_slice(b"</text></revision></page></mediawiki>");

    let mut stream = new_revision_stream(ChunkedReader {
        input: body,
        position: 0,
        chunk_size: 1,
    });
    stream.next_page().unwrap().unwrap();
    let mut started = stream.next_revision_stream().unwrap().unwrap();
    let error = started.stream_text_to(&mut Vec::new()).unwrap_err();
    assert!(error.to_string().contains("invalid UTF-8"));
    drop(started);
    assert!(stream.next_revision_stream().is_none());
}

#[test]
fn streamed_text_rejects_invalid_xml_control_bytes() {
    let mut body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id><revision>
        <id>2</id><timestamp>2024-01-01T00:00:00Z</timestamp><text>prefix "#
        .to_vec();
    body.push(0x00);
    body.extend_from_slice(b"</text></revision></page></mediawiki>");

    let mut stream = new_revision_stream(ChunkedReader {
        input: body,
        position: 0,
        chunk_size: 1,
    });
    stream.next_page().unwrap().unwrap();
    let mut started = stream.next_revision_stream().unwrap().unwrap();
    let error = started.stream_text_to(&mut Vec::new()).unwrap_err();
    assert!(error.to_string().contains("invalid XML character"));
    drop(started);
    assert!(stream.next_revision_stream().is_none());
}

#[test]
fn revision_start_exposes_prefix_streams_body_and_finishes_suffix() {
    let body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id><revision>
        <id>2</id><parentid>1</parentid><timestamp>2024-01-01T00:00:00Z</timestamp>
        <contributor><username>U</username><id>3</id></contributor><comment>commit</comment><minor/>
        <origin>4</origin><model>wikitext</model><format>text/x-wiki</format>
        <text bytes="12">left &amp; <![CDATA[cdata]]></text><sha1>abc</sha1>
      </revision></page>
    </mediawiki>"#;
    let mut stream = new_revision_stream(Cursor::new(body));
    assert_eq!(stream.next_page().unwrap().unwrap().id, 1);

    let mut started = stream.next_revision_stream().unwrap().unwrap();
    let prefix = started.prefix();
    assert_eq!(prefix.id, 2);
    assert_eq!(prefix.parent_id, Some(1));
    assert_eq!(prefix.contributor, wikimak_mediawiki::Contributor::Named {
        username: "U".into(),
        user_id: 3,
    });
    assert!(prefix.minor);
    assert_eq!(prefix.comment, "commit");
    assert_eq!(prefix.origin, Some(4));
    assert_eq!(prefix.model, "wikitext");
    assert_eq!(prefix.format, "text/x-wiki");
    assert_eq!(prefix.text_bytes, Some(12));
    assert!(!prefix.text_hidden);

    let mut text = Vec::new();
    started.stream_text_to(&mut text).unwrap();
    assert_eq!(text, b"left & cdata");
    let revision = started.finish().unwrap();
    assert_eq!(revision.text, "");
    assert_eq!(revision.sha1, "abc");
    assert!(stream.next_revision_stream().is_none());
}

#[test]
fn revision_start_handles_empty_and_deleted_text_without_materializing() {
    let body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id>
        <revision><id>2</id><timestamp>2024-01-01T00:00:00Z</timestamp><text bytes="0"/></revision>
        <revision><id>3</id><timestamp>2024-01-02T00:00:00Z</timestamp><text deleted="deleted"/></revision>
      </page>
    </mediawiki>"#;
    let mut stream = new_revision_stream(Cursor::new(body));
    stream.next_page().unwrap().unwrap();

    let mut empty = stream.next_revision_stream().unwrap().unwrap();
    assert_eq!(empty.prefix().text_bytes, Some(0));
    let mut output = Vec::new();
    empty.stream_text_to(&mut output).unwrap();
    assert!(output.is_empty());
    assert_eq!(empty.finish().unwrap().id, 2);

    let mut deleted = stream.next_revision_stream().unwrap().unwrap();
    assert!(deleted.prefix().text_hidden);
    assert!(deleted.prefix().suppressed);
    deleted.stream_text_to(&mut output).unwrap();
    assert_eq!(deleted.finish().unwrap().id, 3);
    assert!(stream.next_revision_stream().is_none());
}

#[test]
fn revision_start_rejects_declared_byte_mismatch() {
    let body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id><revision>
        <id>2</id><timestamp>2024-01-01T00:00:00Z</timestamp><text bytes="4">abc</text>
      </revision></page>
    </mediawiki>"#;
    let mut stream = new_revision_stream(Cursor::new(body));
    stream.next_page().unwrap().unwrap();
    let mut started = stream.next_revision_stream().unwrap().unwrap();
    let error = started.stream_text_to(&mut Vec::new()).unwrap_err();
    assert!(error.to_string().contains("text bytes mismatch"));
    drop(started);
    assert!(stream.next_revision_stream().is_none());
}

#[test]
fn revision_start_rejects_late_prefix_fields() {
    let body = br#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
      <siteinfo><sitename>x</sitename><dbname>x</dbname><base>x</base><generator>x</generator>
        <case>first-letter</case><namespaces><namespace key="0" case="first-letter"/></namespaces></siteinfo>
      <page><title>P</title><ns>0</ns><id>1</id><revision>
        <id>2</id><timestamp>2024-01-01T00:00:00Z</timestamp><text>abc</text><comment>late</comment>
      </revision></page>
    </mediawiki>"#;

    let mut legacy = new_revision_stream(Cursor::new(body));
    legacy.next_page().unwrap().unwrap();
    let error = legacy.next_revision().unwrap().unwrap_err();
    assert!(error.to_string().contains("appears after <text>"));

    let mut sink_api = new_revision_stream(Cursor::new(body));
    sink_api.next_page().unwrap().unwrap();
    let error = sink_api
        .next_revision_to(&mut Vec::new())
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("appears after <text>"));

    let mut stream = new_revision_stream(Cursor::new(body));
    stream.next_page().unwrap().unwrap();
    let mut started = stream.next_revision_stream().unwrap().unwrap();
    started.stream_text_to(&mut Vec::new()).unwrap();
    let error = started.finish().unwrap_err();
    assert!(error.to_string().contains("appears after <text>"));
    assert!(stream.next_revision_stream().is_none());
}
