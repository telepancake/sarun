//! `PageReadout` — the RO-attachment readout over one PINNED page
//! revision (ATTACH-CONVERGENCE.md chip 2). Shape contract: exactly
//! one leaf `<title>.txt` at the root, bytes frozen at the pin.
//! Locking contract: construction and idle attachments hold NOTHING;
//! the first access takes the shared lock only for the decode, so an
//! attached page never blocks `wikimak import` — and a pinned readout
//! keeps serving the pinned bytes after the head moves on.

mod common;

use std::io::Cursor;

use depot::variant::{Blob, Readout, ReadoutKind};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::readout::PageReadout;
use wikimak_wikipedia::Instance;

const PAGE: u64 = 7;

fn doc(rev: u64, day: u8, text: &str) -> String {
    format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page><title>Sarun/Design</title><ns>0</ns><id>{PAGE}</id>
    <revision><id>{rev}</id><timestamp>2024-01-{day:02}T00:00:00Z</timestamp>
      <contributor><username>A</username><id>1</id></contributor>
      <comment>r{rev}</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{text}</text>
    </revision>
  </page>
</mediawiki>"#
    )
}

fn import(inst: &Instance, xml: &str) {
    let mut stream = new_page_stream(Cursor::new(xml.as_bytes().to_vec()));
    inst.import(&mut stream).unwrap();
    inst.flush().unwrap();
}

/// Rev 100 ("old") then rev 101 ("head text") imported, WRITER DROPPED
/// — readouts open the root themselves, read-side.
fn mirrored() -> TempDir {
    let tmp = TempDir::new().unwrap();
    {
        let inst = common::make_instance(&tmp, 64);
        import(&inst, &doc(100, 1, "old"));
        import(&inst, &doc(101, 2, "head text"));
    }
    tmp
}

#[test]
fn serves_exactly_the_pinned_revision() {
    let tmp = mirrored();
    // `/` in the title sanitizes to `_` — the leaf name is one component.
    let r = PageReadout::new(tmp.path().to_path_buf(), PAGE, Some("Sarun/Design"), 101);
    let name = b"Sarun_Design.txt".to_vec();

    let root = r.entry(&[]).unwrap();
    assert_eq!(root.kind, ReadoutKind::Branch);
    assert_eq!(root.blob_len, None);
    assert_eq!(r.children(&[]), vec![name.clone()]);

    let leaf = r.entry(&[&name]).unwrap();
    assert_eq!(leaf.kind, ReadoutKind::Leaf);
    assert_eq!(leaf.blob_len, Some(9));
    assert_eq!(r.blob(&[&name]), Some(Blob::Bytes(b"head text".to_vec())));
    assert!(r.children(&[&name]).is_empty());

    // A pin on the OLDER revision serves the older bytes, not the head.
    let r0 = PageReadout::new(tmp.path().to_path_buf(), PAGE, Some("Sarun/Design"), 100);
    assert_eq!(r0.blob(&[&name]), Some(Blob::Bytes(b"old".to_vec())));
    assert_eq!(r0.entry(&[&name]).unwrap().blob_len, Some(3));
}

#[test]
fn id_fallback_name_and_misses() {
    let tmp = mirrored();
    let r = PageReadout::new(tmp.path().to_path_buf(), PAGE, None, 101);
    assert_eq!(r.children(&[]), vec![b"page-7.txt".to_vec()]);
    assert_eq!(r.entry(&[b"wrong.txt"]), None);
    assert_eq!(r.blob(&[b"wrong.txt"]), None);
    assert_eq!(r.entry(&[b"page-7.txt", b"deeper"]), None);
    // The root itself carries no blob.
    assert_eq!(r.blob(&[]), None);
}

#[test]
fn missing_page_rev_or_store_is_a_miss_not_an_error() {
    let tmp = mirrored();
    // No such page.
    let r = PageReadout::new(tmp.path().to_path_buf(), 42, Some("Nope"), 100);
    assert_eq!(r.entry(&[]), None);
    assert!(r.children(&[]).is_empty());
    assert_eq!(r.blob(&[b"Nope.txt"]), None);
    // No such revision on the page's chain.
    let r = PageReadout::new(tmp.path().to_path_buf(), PAGE, Some("Sarun/Design"), 999);
    assert_eq!(r.entry(&[]), None);
    // No store at all — and the readout must not create one.
    let ghost = tmp.path().join("nonexistent");
    let r = PageReadout::new(ghost.clone(), PAGE, Some("Sarun/Design"), 101);
    assert_eq!(r.entry(&[]), None);
    assert!(!ghost.exists(), "a missing-store miss must not create the root");
}

/// The attach-honesty pair: (a) an attached (even decoded) readout
/// holds no lock, so a writer can open and import; (b) after the head
/// bumps, the pinned readout — including a FRESH one deciding what to
/// serve only now — still serves the pinned revision's bytes.
#[test]
fn import_while_attached_and_pin_survives_head_bump() {
    let tmp = mirrored();
    let name = b"Sarun_Design.txt".to_vec();

    let attached = PageReadout::new(tmp.path().to_path_buf(), PAGE, Some("Sarun/Design"), 101);
    assert_eq!(
        attached.blob(&[&name]),
        Some(Blob::Bytes(b"head text".to_vec())),
        "decode before the import"
    );

    // Head bump 101 -> 102 while the readout above stays attached: the
    // writer open + import must succeed (the decode dropped its lock).
    {
        let writer = Instance::open(common::cfg(tmp.path().to_path_buf(), 64)).expect(
            "import-while-attached: writer open must not be blocked by a readout",
        );
        import(&writer, &doc(102, 3, "newer head"));
        assert_eq!(writer.page_head(PAGE).unwrap().unwrap().rev_id, 102);
    }

    // The already-decoded attachment still serves the pin…
    assert_eq!(attached.blob(&[&name]), Some(Blob::Bytes(b"head text".to_vec())));
    // …and so does a readout whose FIRST decode happens after the bump
    // (this is the honesty fix: pre-fix it served the new head).
    let fresh = PageReadout::new(tmp.path().to_path_buf(), PAGE, Some("Sarun/Design"), 101);
    assert_eq!(fresh.children(&[]), vec![name.clone()]);
    assert_eq!(fresh.blob(&[&name]), Some(Blob::Bytes(b"head text".to_vec())));
    assert_eq!(fresh.entry(&[&name]).unwrap().blob_len, Some(9), "pinned size, not the head's");
}

/// While a writer holds the root, an access is a MISS that is NOT
/// cached: the same readout resolves once the writer is gone.
#[test]
fn writer_contention_is_a_retryable_miss() {
    let tmp = mirrored();
    let name = b"Sarun_Design.txt".to_vec();
    let r = PageReadout::new(tmp.path().to_path_buf(), PAGE, Some("Sarun/Design"), 101);

    let writer = Instance::open(common::cfg(tmp.path().to_path_buf(), 64)).unwrap();
    assert_eq!(r.entry(&[]), None, "miss while the writer holds the root");
    assert_eq!(r.blob(&[&name]), None);
    drop(writer);

    assert_eq!(
        r.blob(&[&name]),
        Some(Blob::Bytes(b"head text".to_vec())),
        "contention miss was not cached"
    );
}
