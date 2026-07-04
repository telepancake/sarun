//! sha1 (base-36, newline-fudge) acceptance suite. PHASES.md §W3-Rust-2.

use sha1::{Digest, Sha1};
use wikimak_mediawiki::verify_rev_sha1;

/// Compute the canonical base-36, left-padded-to-31 digest of `text`.
/// Used to build test vectors so they agree with whatever encoder the
/// implementer ships.
fn base36_sha1(text: &str) -> String {
    let mut h = Sha1::new();
    h.update(text.as_bytes());
    let bytes = h.finalize();
    // Long-divide the 20-byte big-endian value by 36.
    let mut digits = bytes.to_vec();
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
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
        out.insert(0, alphabet[rem as usize]);
        if !nonzero {
            break;
        }
    }
    while out.len() < 31 {
        out.insert(0, b'0');
    }
    String::from_utf8(out).unwrap()
}

// ---------------------------------------------------------------------------
// verify_rev_sha1_matched_basic
// ---------------------------------------------------------------------------

#[test]
fn verify_rev_sha1_matched_basic() {
    let text = "Wikipedia is an encyclopedia.";
    let digest = base36_sha1(text);
    let (matched, normalized, tried) = verify_rev_sha1(text, &digest);
    assert!(matched, "exact match must return true");
    assert_eq!(normalized, text);
    assert!(
        tried.is_empty(),
        "tried must be empty on exact match, got {tried:?}"
    );
}

// ---------------------------------------------------------------------------
// verify_rev_sha1_no_match
// ---------------------------------------------------------------------------

#[test]
fn verify_rev_sha1_no_match() {
    let text = "lorem ipsum dolor sit amet";
    let unrelated = base36_sha1("a completely different string");
    let (matched, normalized, tried) = verify_rev_sha1(text, &unrelated);
    assert!(!matched, "unrelated text must not match");
    assert_eq!(normalized, "");
    assert!(
        !tried.is_empty(),
        "tried must list the newline-fudge variants attempted"
    );
}

// ---------------------------------------------------------------------------
// verify_rev_sha1_newline_fudge
//
// `stored` ends in "\n"; the text we have lost its trailing newline
// (the export-pipeline glitch). The trailing-\n variant matches.
// ---------------------------------------------------------------------------

#[test]
fn verify_rev_sha1_newline_fudge() {
    let stored = "[[Main Page]]\n";
    let got = stored.trim_end_matches('\n');
    let digest = base36_sha1(stored);
    let (matched, normalized, tried) = verify_rev_sha1(got, &digest);
    assert!(matched, "trailing-\\n variant must match; tried={tried:?}");
    assert_eq!(normalized, stored, "normalized must be the variant that matched");
    assert!(
        !tried.is_empty(),
        "tried must list at least the variant that matched"
    );
}

// ---------------------------------------------------------------------------
// verify_rev_sha1_leftpad_31
//
// SHA-1("") has a small base-36 value, so its 31-char encoding is
// load-bearingly left-padded with '0'. Verifier must accept the padded
// form.
// ---------------------------------------------------------------------------

#[test]
fn verify_rev_sha1_leftpad_31() {
    let empty_digest = base36_sha1("");
    assert_eq!(
        empty_digest.len(),
        31,
        "canonical encoding is always 31 chars"
    );
    let (matched, normalized, _) = verify_rev_sha1("", &empty_digest);
    assert!(matched, "verifier must accept the 31-char padded form");
    assert_eq!(normalized, "");

    // A single-byte text also exercises the encoder.
    let text = "x";
    let d = base36_sha1(text);
    assert_eq!(d.len(), 31);
    let (matched, normalized, _) = verify_rev_sha1(text, &d);
    assert!(matched);
    assert_eq!(normalized, text);
}
