//! Bz2 decoder acceptance suite. PHASES.md §W3-Rust-2 / SPEC §API.
//!
//! Pure fixture-based: no HTTP. Each test runs at `workers: 1` and
//! `workers: 4` and asserts byte-equality across worker counts.

mod common;

use std::io::{Cursor, Read};

use wikimak_mediawiki::{new_bz2_reader, Bz2Options};

use common::fixture;

fn decode(compressed: &[u8], workers: usize) -> std::io::Result<Vec<u8>> {
    let mut r = new_bz2_reader(Cursor::new(compressed.to_vec()), Bz2Options { workers });
    let mut out = Vec::new();
    r.read_to_end(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// bz2_single_block_roundtrip
// ---------------------------------------------------------------------------

#[test]
fn bz2_single_block_roundtrip() {
    let plain = fixture("small_payload.txt");
    let compressed = fixture("small_payload.txt.bz2");
    for workers in [1usize, 4] {
        let got = decode(&compressed, workers).expect("decode must succeed");
        assert_eq!(got, plain, "workers={workers}: bytes must equal plaintext");
    }
}

// ---------------------------------------------------------------------------
// bz2_multi_block_single_stream
// ---------------------------------------------------------------------------

#[test]
fn bz2_multi_block_single_stream() {
    let plain = fixture("multiblock_payload.txt");
    let compressed = fixture("multiblock_payload.txt.bz2");
    let serial = decode(&compressed, 1).expect("serial decode must succeed");
    assert_eq!(serial, plain, "workers=1: bytes must equal plaintext");
    let parallel = decode(&compressed, 4).expect("parallel decode must succeed");
    assert_eq!(parallel, plain, "workers=4: bytes must equal plaintext");
    assert_eq!(
        serial, parallel,
        "parallel decode must be byte-identical to serial"
    );
}

// ---------------------------------------------------------------------------
// bz2_multistream
// ---------------------------------------------------------------------------

#[test]
fn bz2_multistream() {
    let plain = fixture("multistream.txt");
    let compressed = fixture("multistream.bz2");
    let serial = decode(&compressed, 1).expect("multistream serial decode");
    assert_eq!(serial, plain, "workers=1: multistream must round-trip");
    let parallel = decode(&compressed, 4).expect("multistream parallel decode");
    assert_eq!(parallel, plain, "workers=4: multistream must round-trip");
    assert_eq!(serial, parallel);
}

// ---------------------------------------------------------------------------
// bz2_truncated_errors
//
// Feed a truncated bz2 stream; the reader must surface an Err and must
// not panic. Run at workers=1 and workers=4 since the failure mode
// differs between serial and parallel paths.
// ---------------------------------------------------------------------------

#[test]
fn bz2_truncated_errors() {
    let full = fixture("multiblock_payload.txt.bz2");
    assert!(full.len() >= 50, "fixture too small to truncate");
    let truncated = full[..full.len() / 2].to_vec();

    for workers in [1usize, 4] {
        let res = decode(&truncated, workers);
        assert!(
            res.is_err(),
            "workers={workers}: truncated bz2 must surface as an io error, got Ok({:?})",
            res.as_ref().map(|v| v.len())
        );
    }
}
