//! Bz2 decoder acceptance suite. PHASES.md §W3-Rust-2 / SPEC §API.
//!
//! Pure fixture-based: no HTTP. Each test runs at `workers: 1` and
//! `workers: 4` and asserts byte-equality across worker counts.

mod common;

use std::io::{Cursor, Read};

use wikimak_mediawiki::{Bz2DecodeStats, Bz2Options, try_new_bz2_reader};

use common::fixture;

fn decode(compressed: &[u8], workers: usize) -> std::io::Result<Vec<u8>> {
    decode_with_stats(compressed, workers).map(|(output, _)| output)
}

fn decode_with_stats(
    compressed: &[u8],
    workers: usize,
) -> std::io::Result<(Vec<u8>, Bz2DecodeStats)> {
    let mut r = try_new_bz2_reader(
        Cursor::new(compressed.to_vec()),
        Bz2Options { workers },
    )?;
    let mut out = Vec::new();
    r.read_to_end(&mut out)?;
    Ok((out, r.stats()))
}

// ---------------------------------------------------------------------------
// bz2_single_block_roundtrip
// ---------------------------------------------------------------------------

#[test]
fn bz2_single_block_roundtrip() {
    let plain = fixture("small_payload.txt");
    let compressed = fixture("small_payload.txt.bz2");
    for workers in [0usize, 1, 4] {
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

    for workers in [0usize, 1, 4] {
        let res = decode(&truncated, workers);
        assert!(
            res.is_err(),
            "workers={workers}: truncated bz2 must surface as an io error, got Ok({:?})",
            res.as_ref().map(|v| v.len())
        );
    }
}

#[test]
fn bz2_combined_crc_mismatch_errors_after_complete_blocks() {
    const END_MAGIC: u64 = 0x1772_4538_5090;
    let mut compressed = fixture("multiblock_payload.txt.bz2");
    let end = (32..compressed.len() * 8 - 80)
        .find(|position| {
            (0..48).fold(0u64, |value, offset| {
                let bit = position + offset;
                (value << 1) | u64::from(compressed[bit / 8] >> (7 - bit % 8) & 1)
            }) == END_MAGIC
        })
        .expect("fixture must contain end-of-stream magic");
    let crc_bit = end + 48;
    compressed[crc_bit / 8] ^= 1 << (7 - crc_bit % 8);
    for workers in [1usize, 4] {
        assert!(
            decode(&compressed, workers).is_err(),
            "workers={workers}: bad combined CRC must fail"
        );
    }
}

#[test]
fn bz2_worker_ownership_and_peak_are_reported() {
    let compressed = fixture("multiblock_payload.txt.bz2");
    for workers in [1usize, 4] {
        let (_, stats) = decode_with_stats(&compressed, workers).expect("decode with stats");
        assert_eq!(stats.worker_limit, workers);
        assert_eq!(stats.spawned_decoder_threads, workers);
        assert!(stats.peak_active_decoders >= 1);
        assert!(stats.peak_active_decoders <= workers);
        assert_eq!(stats.active_decoders, 0);
        assert_eq!(stats.blocks_submitted, stats.blocks_finished);
    }
}
