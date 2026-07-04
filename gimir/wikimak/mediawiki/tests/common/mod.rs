//! Shared helpers for the mediawiki acceptance suite.
//!
//! These do NOT call into the package-under-test directly; they only
//! load fixture files, compute hashes, and render SHA256SUMS bodies.

#![allow(dead_code)]

use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// Read a fixture from `tests/data/`.
pub fn fixture(name: &str) -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("data");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"))
}

/// Lowercase hex SHA-256 of `b`.
pub fn sha256_hex(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    hex::encode(h.finalize())
}

/// Render a SHA256SUMS file from name->bytes. Lines are
/// `"<hex>  <name>\n"`, sorted by filename for stability.
pub fn build_sha256sums(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        let body = files.iter().find(|(k, _)| *k == n).unwrap().1;
        out.push_str(&format!("{}  {}\n", sha256_hex(body), n));
    }
    out.into_bytes()
}

/// Lowercase hex SHA-1 of `b`. (Used by fetch tests for the sha1-only
/// verification path.)
pub fn sha1_hex(b: &[u8]) -> String {
    use sha1::{Digest as _, Sha1};
    let mut h = Sha1::new();
    h.update(b);
    hex::encode(h.finalize())
}
