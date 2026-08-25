//! SHA-1 verification with base-36 encoding and newline-fudge tolerance.
//!
//! Per SPEC §"Wire facts": MediaWiki stores per-revision SHA-1 as
//! base-36, left-padded to 31 chars.

use sha1::{Digest, Sha1};

const SHA1_BASE36_LEN: usize = 31;
const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Compute the canonical MediaWiki sha1 digest: SHA-1 of `text` encoded
/// big-endian base-36 and left-padded to 31 chars.
fn sha1_base36(text: &str) -> String {
    let mut h = Sha1::new();
    h.update(text.as_bytes());
    let digest = h.finalize();
    // Long-divide the 20-byte big-endian value by 36.
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
    String::from_utf8(out).expect("base-36 alphabet is ASCII")
}

/// Verify `text`'s SHA-1 against the stored base-36 digest. Returns
/// `(matched, normalized_text, tried_variants)` per SPEC §API.
///
/// Exact match returns `(true, text, [])`. On mismatch, tries cheap
/// newline-fudge variants in fixed order; returns the matching variant
/// in `normalized`. On full miss returns `(false, "", all_variants)`.
pub fn verify_rev_sha1(text: &str, sha1_base36_want: &str) -> (bool, String, Vec<&'static str>) {
    if sha1_base36(text) == sha1_base36_want {
        return (true, text.to_string(), Vec::new());
    }
    type Variant = (&'static str, fn(&str) -> String);
    let variants: [Variant; 4] = [
        ("trailing-newline-added", |s| format!("{s}\n")),
        ("trailing-newline-stripped", |s| {
            s.trim_end_matches(['\r', '\n']).to_string()
        }),
        ("crlf-to-lf", |s| s.replace("\r\n", "\n")),
        ("lf-to-crlf", |s| {
            // Normalize first so we don't double-convert mixed input.
            let lf = s.replace("\r\n", "\n");
            lf.replace('\n', "\r\n")
        }),
    ];
    let mut tried: Vec<&'static str> = Vec::new();
    for (name, fun) in variants {
        tried.push(name);
        let cand = fun(text);
        if cand == text {
            continue;
        }
        if sha1_base36(&cand) == sha1_base36_want {
            return (true, cand, tried);
        }
    }
    (false, String::new(), tried)
}
