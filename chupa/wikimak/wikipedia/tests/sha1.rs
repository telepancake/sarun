//! SHA-1 counter tests. PHASES §sha1.
//!
//! Three crafted revisions:
//! 1. Stored sha1 matches text directly         → sha1_ok.
//! 2. Stored sha1 matches text only after a
//!    newline-fudge normalization               → sha1_fudged.
//! 3. Stored sha1 matches no normalization      → sha1_mismatch
//!                                                + SHA1_MISMATCH flag.

mod common;

use std::io::Cursor;

use sha1::{Digest, Sha1};
use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::FLAG_SHA1_MISMATCH;

use common::make_instance;

const SHA1_BASE36_LEN: usize = 31;
const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Mirror of `wikimak_mediawiki::sha1::sha1_base36` (private). Used to
/// derive the *expected* stored sha1 for hand-crafted text. Kept inline
/// so the tester depends on only the public surface.
fn sha1_base36(text: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(text);
    let digest = h.finalize();
    let mut digits: Vec<u8> = digest.to_vec();
    let mut out: Vec<u8> = Vec::new();
    loop {
        let mut rem: u32 = 0;
        let mut nonzero = false;
        for d in digits.iter_mut() {
            let cur = rem * 256 + *d as u32;
            *d = (cur / 36) as u8;
            rem = cur % 36;
            if *d != 0 {
                nonzero = true;
            }
        }
        out.insert(0, ALPHABET[rem as usize]);
        if !nonzero {
            break;
        }
    }
    while out.len() < SHA1_BASE36_LEN {
        out.insert(0, b'0');
    }
    String::from_utf8(out).unwrap()
}

// ---------------------------------------------------------------------------
// sha1_counters_populated
// ---------------------------------------------------------------------------

#[test]
fn sha1_counters_populated() {
    let tmp = TempDir::new().unwrap();
    let instance = make_instance(&tmp, 1024);

    // OK case: text = "abc". Stored sha1 = sha1_base36("abc").
    let text_ok = "abc";
    let sha1_ok = sha1_base36(text_ok.as_bytes());

    // Fudged case: stored sha1 expects text + "\n"; we serialize text without
    // the newline. The newline-fudge variant ("trailing-newline-added")
    // recovers it.
    let text_fudge_on_disk = "fudge me";
    let text_fudge_for_sha1 = format!("{text_fudge_on_disk}\n");
    let sha1_fudge = sha1_base36(text_fudge_for_sha1.as_bytes());

    // Mismatch case: stored sha1 is unrelated to text.
    let text_bad = "mismatch text";
    let sha1_bad = "0000000000000000000000000000000".to_string(); // unrelated digest

    let doc = format!(
        r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>x</sitename><dbname>x</dbname><base>x</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter"/></namespaces>
  </siteinfo>
  <page>
    <title>OK</title><ns>0</ns><id>1</id>
    <revision>
      <id>100</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="3" sha1="{sha1_ok}" xml:space="preserve">{text_ok}</text>
      <sha1>{sha1_ok}</sha1>
    </revision>
  </page>
  <page>
    <title>FUDGE</title><ns>0</ns><id>2</id>
    <revision>
      <id>200</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="8" sha1="{sha1_fudge}" xml:space="preserve">{text_fudge_on_disk}</text>
      <sha1>{sha1_fudge}</sha1>
    </revision>
  </page>
  <page>
    <title>BAD</title><ns>0</ns><id>3</id>
    <revision>
      <id>300</id><timestamp>2024-01-01T00:00:00Z</timestamp>
      <contributor><username>U</username><id>1</id></contributor>
      <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>
      <text bytes="13" sha1="{sha1_bad}" xml:space="preserve">{text_bad}</text>
      <sha1>{sha1_bad}</sha1>
    </revision>
  </page>
</mediawiki>"#
    );
    let mut stream = new_page_stream(Cursor::new(doc.into_bytes()));
    let stats = instance.import(&mut stream).expect("import");

    assert_eq!(stats.pages, 3);
    assert_eq!(stats.revisions_new, 3);
    assert_eq!(stats.sha1_ok, 1, "one direct-match revision");
    assert_eq!(stats.sha1_fudged, 1, "one newline-fudge revision");
    assert_eq!(stats.sha1_mismatch, 1, "one mismatch revision");

    // The mismatch revision must carry the SHA1_MISMATCH flag.
    let bad_head = instance.page_head(3).unwrap().unwrap();
    assert!(
        bad_head.flags & FLAG_SHA1_MISMATCH != 0,
        "mismatch revision must have SHA1_MISMATCH set; flags = {:#x}",
        bad_head.flags
    );

    // The ok / fudge revisions must NOT carry the flag.
    let ok_head = instance.page_head(1).unwrap().unwrap();
    assert_eq!(ok_head.flags & FLAG_SHA1_MISMATCH, 0);
    let fudge_head = instance.page_head(2).unwrap().unwrap();
    assert_eq!(fudge_head.flags & FLAG_SHA1_MISMATCH, 0);
}
