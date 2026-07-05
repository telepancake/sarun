//! `PageHeadReadout` — the RO-attachment readout over one page head
//! (ATTACH-CONVERGENCE.md chip 2). Shape contract: a single leaf
//! `<title>.txt` at the root, holding the HEAD text.

mod common;

use std::io::Cursor;

use depot::variant::{Blob, Readout, ReadoutKind};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::readout::PageHeadReadout;

use common::make_instance;

const ONE_PAGE_TWO_REVS: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>Sarun/Design</title><ns>0</ns><id>7</id>
    <revision>
      <id>100</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>A</username><id>1</id></contributor>
      <comment>c1</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="3" sha1="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" xml:space="preserve">old</text>
      <sha1>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</sha1>
    </revision>
    <revision>
      <id>101</id><parentid>100</parentid><timestamp>2024-01-02T00:00:00Z</timestamp>
      <contributor><username>B</username><id>2</id></contributor>
      <comment>c2</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="9" sha1="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" xml:space="preserve">head text</text>
      <sha1>bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</sha1>
    </revision>
  </page>
</mediawiki>"#;

fn imported() -> (TempDir, wikimak_wikipedia::Instance) {
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 64);
    let mut stream = new_page_stream(Cursor::new(ONE_PAGE_TWO_REVS.as_bytes()));
    inst.import(&mut stream).unwrap();
    (tmp, inst)
}

#[test]
fn serves_head_as_single_leaf() {
    let (_tmp, inst) = imported();
    // `/` in the title sanitizes to `_` — the leaf name is one component.
    let r = PageHeadReadout::new(inst, 7, Some("Sarun/Design"));
    let name = b"Sarun_Design.txt".to_vec();

    let root = r.entry(&[]).unwrap();
    assert_eq!(root.kind, ReadoutKind::Branch);
    assert_eq!(root.blob_len, None);
    assert_eq!(r.children(&[]), vec![name.clone()]);

    let leaf = r.entry(&[&name]).unwrap();
    assert_eq!(leaf.kind, ReadoutKind::Leaf);
    assert_eq!(leaf.blob_len, Some(9));
    // HEAD text, not the older revision.
    assert_eq!(r.blob(&[&name]), Some(Blob::Bytes(b"head text".to_vec())));
    assert!(r.children(&[&name]).is_empty());
}

#[test]
fn id_fallback_name_and_misses() {
    let (_tmp, inst) = imported();
    let r = PageHeadReadout::new(inst, 7, None);
    assert_eq!(r.children(&[]), vec![b"page-7.txt".to_vec()]);
    assert_eq!(r.entry(&[b"wrong.txt"]), None);
    assert_eq!(r.blob(&[b"wrong.txt"]), None);
    assert_eq!(r.entry(&[b"page-7.txt", b"deeper"]), None);
    // The root itself carries no blob.
    assert_eq!(r.blob(&[]), None);
}

#[test]
fn missing_page_is_a_miss_not_an_error() {
    let (_tmp, inst) = imported();
    let r = PageHeadReadout::new(inst, 42, Some("Nope"));
    assert_eq!(r.entry(&[]), None);
    assert!(r.children(&[]).is_empty());
    assert_eq!(r.blob(&[b"Nope.txt"]), None);
}
