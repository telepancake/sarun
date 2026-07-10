//! `Depot::open` must not scan the store (2026-07 round 2).
//!
//! The old open rebuilt dead-byte counters by walking EVERY frame of
//! every data file INCLUDING payloads — O(total bytes on disk) of read
//! I/O before the first query answered (at enwiki scale: minutes). Now
//! the counters persist in the `counters` sidecar at flush/collect and
//! open just loads them.
//!
//! REAL effect, really measured: /proc/self/io `rchar` (bytes moved by
//! read syscalls, page-cache hits included — the honest counter for
//! "did open read the data files") across `Depot::open` of a
//! multi-file, multi-megabyte store must stay a rounding error next to
//! the payload bytes on disk. This file holds exactly ONE test:
//! `rchar` is process-wide, and each integration-test file is its own
//! process.

use tempfile::TempDir;
use wikimak_depot::{Depot, DepotConfig};

/// Bytes read by this process so far (`/proc/self/io` rchar).
fn rchar() -> u64 {
    let io = std::fs::read_to_string("/proc/self/io").expect("read /proc/self/io");
    io.lines()
        .find_map(|l| l.strip_prefix("rchar: "))
        .expect("rchar line")
        .trim()
        .parse()
        .expect("rchar value")
}

#[test]
fn open_performs_no_data_file_payload_reads() {
    const CHAINS: u64 = 300;
    const PAYLOAD: usize = 16 << 10;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");
    let cfg = || DepotConfig {
        root: root.clone(),
        max_chain_id: 1024,
        // Small files → a properly multi-file store (~75 per tier).
        file_size_threshold: 64 << 10,
        eviction_dead_ratio: 0.9,
    };

    let payload = |cid: u64| {
        let mut v = vec![0u8; PAYLOAD];
        v[..8].copy_from_slice(&cid.to_le_bytes());
        v
    };

    {
        let depot = Depot::open(cfg()).unwrap();
        for cid in 0..CHAINS {
            depot.prepend(cid, &payload(cid), None, false).unwrap();
        }
        // Second prepends for some chains so the f1 tier is populated
        // and dead bytes exist — the counters being persisted are real.
        for cid in 0..CHAINS / 4 {
            depot.prepend(cid, &payload(cid + 1000), Some(&payload(cid + 2000)), false).unwrap();
        }
        depot.flush().unwrap();
    }

    // The store is genuinely big and multi-file.
    let mut data_bytes = 0u64;
    let mut data_files = 0u64;
    for tier in ["f0", "f1"] {
        for e in std::fs::read_dir(root.join(tier)).unwrap().flatten() {
            data_bytes += e.metadata().unwrap().len();
            data_files += 1;
        }
    }
    assert!(data_files >= 10, "fixture must span many data files: {data_files}");
    assert!(data_bytes > 4 << 20, "fixture must hold real payload volume: {data_bytes}");

    // ---- the measurement ----
    let before = rchar();
    let depot = Depot::open(cfg()).unwrap();
    let delta = rchar() - before;

    assert!(
        !depot.counters_rebuilt_on_open(),
        "clean-shutdown open must load the persisted counters"
    );
    assert!(
        delta < 512 << 10,
        "open read {delta} bytes against {data_bytes} bytes of data files — \
         it scanned the store"
    );
    eprintln!("open read {delta} bytes; store holds {data_bytes} data bytes in {data_files} files");

    // And it is a REAL open: chains read back exactly.
    assert_eq!(depot.read_f0(CHAINS - 1).unwrap(), payload(CHAINS - 1));
    assert_eq!(depot.read_f0(0).unwrap(), payload(1000));
}
