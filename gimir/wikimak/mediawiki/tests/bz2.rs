//! Bz2 decoder acceptance suite. PHASES.md §W3-Rust-2 / SPEC §API.
//!
//! Pure fixture-based: no HTTP. Each test runs at `workers: 1` and
//! `workers: 4` and asserts byte-equality across worker counts.

mod common;

use std::io::Write;
use std::io::{Cursor, Read};
use std::time::Instant;

use bzip2::Compression;
use bzip2::write::BzEncoder;
use wikimak_mediawiki::{Bz2Options, new_bz2_reader};

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

// ---------------------------------------------------------------------------
// bz2_parallel_is_faster
//
// This is the performance half of the block-parallel contract. Debug builds
// distort the bit scanner enough to make wall-clock comparison meaningless,
// and a one-core runner cannot demonstrate concurrency, so those environments
// retain the correctness tests above and skip only this timing assertion.
// ---------------------------------------------------------------------------

#[test]
fn bz2_parallel_is_faster() {
    if cfg!(debug_assertions)
        || std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            < 4
    {
        return;
    }

    // Incompressible input forces many full 900 KiB bzip2 blocks and makes
    // block decode, rather than output allocation, the measured work.
    let mut state = 0x8f3d_9a27_6c51_04bdu64;
    let mut plain = vec![0u8; 32 * 1024 * 1024];
    for chunk in plain.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&plain).unwrap();
    let compressed = encoder.finish().unwrap();

    // Warm allocator and libbz2 before measuring either arm.
    assert_eq!(decode(&compressed, 4).unwrap(), plain);
    let serial_start = Instant::now();
    assert_eq!(decode(&compressed, 1).unwrap(), plain);
    let serial = serial_start.elapsed();
    let parallel_start = Instant::now();
    assert_eq!(decode(&compressed, 4).unwrap(), plain);
    let parallel = parallel_start.elapsed();
    eprintln!("bz2 decode: serial={serial:?}, four-worker={parallel:?}");
    assert!(
        parallel < serial,
        "four-worker decode must beat serial: parallel={parallel:?}, serial={serial:?}"
    );
}
