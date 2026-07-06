//! Streaming frame codec parity + `seal_f1`.
//!
//! The streaming encoder must be BYTE-IDENTICAL to the bulk
//! `compress_frame` for the same (input, prefix, level, declared total
//! length) — same window-log formula, LDM, refPrefix — across sizes
//! straddling the 1<<20 window-log floor and multi-MB inputs, for any
//! input chunking. The streaming decoder must decode bulk-compressed
//! frames and vice versa.

use std::io::Read as _;

use tempfile::TempDir;
use wikimak_depot::{
    compress_frame, decompress_frame, Depot, DepotConfig, Error, FrameDecoder, FrameEncoder,
};

/// Deterministic compressible-but-not-trivial bytes: repeated phrases
/// with a drifting counter, so LDM has long-distance matches to find.
fn corpus(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed | 1;
    while out.len() < len {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let phrase = format!("record {:06} payload {:016x} lorem ipsum dolor sit\n", out.len(), x);
        out.extend_from_slice(phrase.as_bytes());
    }
    out.truncate(len);
    out
}

fn stream_compress(src: &[u8], prefix: Option<&[u8]>, level: i32, chunk: usize) -> Vec<u8> {
    let mut enc = FrameEncoder::new(src.len() as u64, prefix, level).unwrap();
    for c in src.chunks(chunk.max(1)) {
        enc.write(c).unwrap();
    }
    enc.finish().unwrap()
}

fn stream_decompress(frame: &[u8], prefix: Option<&[u8]>) -> Vec<u8> {
    let mut dec = FrameDecoder::new(frame, prefix).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 61_441]; // odd size: exercise partial reads
    loop {
        let n = dec.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[test]
fn streaming_encoder_is_byte_identical_to_bulk() {
    let prefix_big = corpus(300_000, 7);
    // Sizes straddling 1<<20 (the window-log floor) and multi-MB.
    let sizes = [
        0usize,
        1,
        4_096,
        (1 << 20) - 1,
        1 << 20,
        (1 << 20) + 1,
        3 << 20,
        (5 << 20) + 12_345,
    ];
    // NB: prefixes are separate allocations. A prefix that ALIASES the
    // source buffer can legitimately compress differently between bulk
    // and multi-chunk streaming (zstd's contiguous-window detection);
    // real callers (frame anchors) never alias the streamed entries.
    let prefix_small = corpus(1_000, 99);
    for &len in &sizes {
        let src = corpus(len, len as u64 + 3);
        for prefix in [None, Some(&prefix_big[..]), Some(&prefix_small[..])] {
            for level in [1, 3] {
                let bulk = compress_frame(&src, prefix, level).unwrap();
                for chunk in [usize::MAX, 1 << 16, 4_093] {
                    let streamed = stream_compress(&src, prefix, level, chunk);
                    assert_eq!(
                        bulk, streamed,
                        "stream != bulk (len {len}, level {level}, chunk {chunk}, \
                         prefix {:?})",
                        prefix.map(|p| p.len())
                    );
                }
                // Cross-decode both ways.
                assert_eq!(stream_decompress(&bulk, prefix), src, "stream-decode of bulk");
                assert_eq!(
                    decompress_frame(&bulk, prefix).unwrap(),
                    src,
                    "bulk-decode of streamed (same bytes)"
                );
            }
        }
    }
}

#[test]
fn frame_encoder_enforces_declared_length() {
    let mut enc = FrameEncoder::new(10, None, 3).unwrap();
    enc.write(b"12345").unwrap();
    assert!(enc.finish().is_err(), "short write must fail at finish");
    let mut enc = FrameEncoder::new(3, None, 3).unwrap();
    assert!(enc.write(b"12345").is_err(), "overlong write must fail");
}

fn cfg(root: std::path::PathBuf) -> DepotConfig {
    DepotConfig {
        root,
        max_chain_id: 64,
        file_size_threshold: 1 << 20,
        eviction_dead_ratio: 0.5,
    }
}

/// `seal_f1`: the current f1 moves verbatim to cold NOW; the chain
/// reads back identically as f0 → (no f1) → cold, survives reopen,
/// and a later prepend inherits the cold head. Sealing with no f1 is
/// an ERROR (`CannotSealNoF1`; `NoFrame` on an empty chain).
#[test]
fn seal_f1_retires_current_accumulator_to_cold() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();

        // Empty chain / no-f1 chain: errors.
        assert!(matches!(depot.seal_f1(9), Err(Error::NoFrame)));
        depot.prepend(9, b"A-f0", None, false).unwrap();
        assert!(matches!(depot.seal_f1(9), Err(Error::CannotSealNoF1)));

        // Build f0 + f1 + one previously-sealed cold frame.
        depot.prepend(9, b"B-f0", Some(b"f1-B"), false).unwrap();
        depot.prepend(9, b"C-f0", Some(b"f1-C"), true).unwrap(); // seals f1-B
        // Immediate retirement of the CURRENT f1.
        depot.seal_f1(9).unwrap();
        assert_eq!(depot.read_f0(9).unwrap(), b"C-f0".to_vec());
        assert_eq!(depot.read_f1(9).unwrap(), None, "f1 slot must be empty after seal_f1");
        let cold: Vec<Vec<u8>> = depot.cold_iter(9).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(cold, vec![b"f1-C".to_vec(), b"f1-B".to_vec()], "newest-first, verbatim");
        // Double seal: nothing left to seal.
        assert!(matches!(depot.seal_f1(9), Err(Error::CannotSealNoF1)));
        depot.flush().unwrap();
    }
    // Reopen: the direct-cold pointer state must survive (index /
    // dead-byte rebuild included) and a later prepend must inherit the
    // cold head into its new f1.
    let depot = Depot::open(cfg(root)).unwrap();
    assert_eq!(depot.read_f1(9).unwrap(), None);
    depot.prepend(9, b"D-f0", Some(b"f1-D"), false).unwrap();
    assert_eq!(depot.read_f0(9).unwrap(), b"D-f0".to_vec());
    assert_eq!(depot.read_f1(9).unwrap(), Some(b"f1-D".to_vec()));
    let cold: Vec<Vec<u8>> = depot.cold_iter(9).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(cold, vec![b"f1-C".to_vec(), b"f1-B".to_vec()]);
    // And sealing the fresh f1 stacks on the same cold chain.
    depot.seal_f1(9).unwrap();
    let cold: Vec<Vec<u8>> = depot.cold_iter(9).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(
        cold,
        vec![b"f1-D".to_vec(), b"f1-C".to_vec(), b"f1-B".to_vec()],
        "seal_f1 must link ahead of the existing cold chain"
    );
}
