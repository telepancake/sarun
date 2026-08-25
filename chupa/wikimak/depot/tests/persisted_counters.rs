//! The persisted dead-byte counters survive clean shutdowns, fence off
//! dirty ones, and keep eviction firing at the right thresholds across
//! reopens (2026-07 round 2).
//!
//! The fence is the recorded file set + per-file lengths + cold length:
//! every mutation the depot can make changes at least one of them, so
//! equality proves the sidecar describes THIS on-disk state. A crash
//! between prepends (durable writes, no flush) trips it → the one-time
//! header-only rebuild, then re-persist.

use tempfile::TempDir;
use wikimak_depot::{Depot, DepotConfig};

const THRESHOLD: u64 = 8192;
/// 1000-byte payload + 24-byte header = 1024 bytes per frame,
/// 8 frames exactly per data file.
const PAYLOAD: usize = 1000;

fn cfg(root: std::path::PathBuf) -> DepotConfig {
    DepotConfig {
        root,
        max_chain_id: 1024,
        file_size_threshold: THRESHOLD,
        eviction_dead_ratio: 0.5,
    }
}

fn payload(tag: u64) -> Vec<u8> {
    let mut v = vec![0u8; PAYLOAD];
    v[..8].copy_from_slice(&tag.to_le_bytes());
    v
}

#[test]
fn dirty_shutdown_trips_the_fence_rebuilds_once_and_persists() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");

    // Session 1: real store with f1 frames and dead bytes; clean flush.
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        for cid in 1..=8u64 {
            depot.prepend(cid, &payload(cid), None, false).unwrap();
        }
        for cid in 1..=3u64 {
            depot.prepend(cid, &payload(cid + 100), Some(&payload(cid + 200)), false).unwrap();
        }
        depot.flush().unwrap();
    }

    // Session 2: loads the sidecar (clean), then "crashes" — prepends
    // land on disk but no flush ever runs, so the sidecar goes stale.
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        assert!(!depot.counters_rebuilt_on_open(), "clean shutdown → sidecar loads");
        for cid in 4..=5u64 {
            depot.prepend(cid, &payload(cid + 100), Some(&payload(cid + 200)), false).unwrap();
        }
        // Drop without flush = the crash, as far as the fence is concerned.
    }

    // Session 3: the fence must trip (file lengths moved), the rebuild
    // must run once, and its result must be persisted for session 4.
    let stats_rebuilt = {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        assert!(
            depot.counters_rebuilt_on_open(),
            "dirty shutdown must force the counter rebuild"
        );
        depot.tier_stats()
    };

    // Session 4: nothing mutated since session 3 persisted its rebuild —
    // the sidecar must load and agree byte-for-byte with the rebuild.
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        assert!(!depot.counters_rebuilt_on_open(), "rebuild must have been persisted");
        assert_eq!(depot.tier_stats(), stats_rebuilt, "loaded counters == rebuilt counters");
    }

    // Deleting the sidecar outright also degrades to the same rebuild.
    std::fs::remove_file(root.join("counters")).unwrap();
    let depot = Depot::open(cfg(root.clone())).unwrap();
    assert!(depot.counters_rebuilt_on_open(), "missing sidecar → rebuild");
    assert_eq!(depot.tier_stats(), stats_rebuilt, "rebuild is deterministic");
}

#[test]
fn evictions_fire_at_the_same_threshold_after_reopen() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");

    // Session 1: fill f0/file-0001 with chains 1..=8 (8 × 1024 bytes =
    // exactly one file), roll it with chain 9, then deprecate 3 of its
    // frames — 37.5% dead, just UNDER the 0.5 eviction ratio.
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        for cid in 1..=8u64 {
            depot.prepend(cid, &payload(cid), None, false).unwrap();
        }
        depot.prepend(9, &payload(9), None, false).unwrap(); // rolls file-0001
        for cid in 1..=3u64 {
            depot.prepend(cid, &payload(cid + 100), Some(&payload(cid + 200)), false).unwrap();
        }
        depot.flush().unwrap();
        assert!(
            root.join("f0").join("file-0001").exists(),
            "under-ratio file must survive session 1"
        );
    }

    // Session 2: the loaded counters must carry the 37.5% — two more
    // deprecations (62.5%) push file-0001 over the ratio and the flush
    // must evict it. A reopen that lost the counters would see only
    // 2/8 dead and never fire.
    let depot = Depot::open(cfg(root.clone())).unwrap();
    assert!(!depot.counters_rebuilt_on_open());
    for cid in 4..=5u64 {
        depot.prepend(cid, &payload(cid + 100), Some(&payload(cid + 200)), false).unwrap();
    }
    depot.flush().unwrap();
    assert!(
        !root.join("f0").join("file-0001").exists(),
        "eviction must fire from loaded counters + new deprecations"
    );
    // Live frames migrated, nothing lost: every chain reads back its
    // current head.
    for cid in 1..=5u64 {
        assert_eq!(depot.read_f0(cid).unwrap(), payload(cid + 100), "chain {cid}");
    }
    for cid in 6..=9u64 {
        assert_eq!(depot.read_f0(cid).unwrap(), payload(cid), "chain {cid}");
    }
}
