//! `Depot::delete_chain` — retiring a chain the caller's inventory no
//! longer references (2026-07, task 12: rebuild orphans must be
//! reclaimable).
//!
//! Real-effect assertions:
//!   * delete then `collect` RECLAIMS the chain's f0/f1 bytes — file
//!     sizes on disk shrink to exactly the surviving chains' frames;
//!   * a deleted slot reads as an empty chain (NoFrame / no history /
//!     `has_chain` false), deleting again is a no-op, and the id is
//!     reusable from scratch;
//!   * cold bytes are NOT reclaimed (append-only tier) but are
//!     accounted dead, exactly, in `cold_stats`;
//!   * the crash window between the delete and the sidecar re-persist
//!     trips the fence once (the delete drops the sidecar — deletion
//!     changes no file length, so nothing else could trip it) and the
//!     header rebuild reproduces the delete's counters byte-for-byte.

use tempfile::TempDir;
use wikimak_depot::{Depot, DepotConfig, Error};

const HEADER_LEN: u64 = 24;

fn cfg(root: std::path::PathBuf) -> DepotConfig {
    DepotConfig {
        root,
        max_chain_id: 1024,
        file_size_threshold: 1 << 20, // one file per tier in these tests
        eviction_dead_ratio: 0.5,
    }
}

fn payload(tag: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[..8].copy_from_slice(&tag.to_le_bytes());
    v
}

/// Total bytes of regular files under `dir`.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

#[test]
fn delete_then_collect_reclaims_f0_f1_bytes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");
    let depot = Depot::open(cfg(root.clone())).unwrap();

    // Chain 1: small survivor. Chain 2: bulky victim (its frames
    // dominate both tiers, so its death alone crosses the 0.5 ratio).
    depot.prepend(1, &payload(10, 400), None, false).unwrap();
    depot.prepend(1, &payload(11, 400), Some(&payload(12, 500)), false).unwrap();
    depot.prepend(2, &payload(20, 3000), None, false).unwrap();
    depot.prepend(2, &payload(21, 3000), Some(&payload(22, 4000)), false).unwrap();
    depot.flush().unwrap();

    let before_f0 = dir_bytes(&root.join("f0"));
    let before_f1 = dir_bytes(&root.join("f1"));
    // Both chains' frames (including the deprecated first-prepend f0s)
    // are on disk.
    assert!(before_f0 >= 4 * HEADER_LEN + 400 + 400 + 3000 + 3000, "{before_f0}");
    assert!(before_f1 >= 2 * HEADER_LEN + 500 + 4000, "{before_f1}");

    depot.delete_chain(2).unwrap();
    depot.collect().unwrap();

    // Chain 2's bytes — and the prepend garbage — are GONE: what
    // remains on disk is exactly chain 1's live frames.
    let after_f0 = dir_bytes(&root.join("f0"));
    let after_f1 = dir_bytes(&root.join("f1"));
    assert_eq!(after_f0, HEADER_LEN + 400, "f0 tier = chain 1's live head only");
    assert_eq!(after_f1, HEADER_LEN + 500, "f1 tier = chain 1's live accumulator only");
    assert!(
        before_f0 - after_f0 >= 2 * HEADER_LEN + 3000 + 3000,
        "reclaimed at least chain 2's f0 frames: {before_f0} -> {after_f0}"
    );
    assert!(
        before_f1 - after_f1 >= HEADER_LEN + 4000,
        "reclaimed at least chain 2's f1 frame: {before_f1} -> {after_f1}"
    );

    // The survivor still reads back exactly, before and after reopen.
    assert_eq!(depot.read_f0(1).unwrap(), payload(11, 400));
    assert_eq!(depot.read_f1(1).unwrap().unwrap(), payload(12, 500));
    drop(depot);
    let depot = Depot::open(cfg(root)).unwrap();
    assert!(!depot.counters_rebuilt_on_open(), "collect persisted the sidecar");
    assert_eq!(depot.read_f0(1).unwrap(), payload(11, 400));
    assert!(matches!(depot.read_f0(2), Err(Error::NoFrame)), "deleted chain stays empty");
}

#[test]
fn deleted_slot_reads_as_empty_chain_and_id_is_reusable() {
    let tmp = TempDir::new().unwrap();
    let depot = Depot::open(cfg(tmp.path().join("depot"))).unwrap();

    // A full-shape chain: f0 + f1 + one sealed cold frame.
    depot.prepend(7, &payload(70, 300), None, false).unwrap();
    depot.prepend(7, &payload(71, 300), Some(&payload(72, 300)), false).unwrap();
    depot.prepend(7, &payload(73, 300), Some(&payload(74, 300)), true).unwrap();
    depot.flush().unwrap();
    assert_eq!(depot.cold_iter(7).unwrap().count(), 1, "seal produced a cold frame");

    depot.delete_chain(7).unwrap();

    assert!(matches!(depot.read_f0(7), Err(Error::NoFrame)));
    assert!(matches!(depot.read_f1(7), Err(Error::NoFrame)));
    assert!(!depot.has_chain(7).unwrap());
    assert_eq!(depot.cold_iter(7).unwrap().count(), 0, "no cold walk from an empty slot");

    // Idempotent: deleting again — or deleting a never-written id — is Ok.
    depot.delete_chain(7).unwrap();
    depot.delete_chain(999).unwrap();

    // The id is a fresh chain again: first prepend (f1 = None) works,
    // and forward bulk construction accepts it as EMPTY.
    depot.prepend(7, &payload(75, 100), None, false).unwrap();
    assert_eq!(depot.read_f0(7).unwrap(), payload(75, 100));
    assert!(depot.read_f1(7).unwrap().is_none(), "reborn chain has no f1");
    depot.delete_chain(7).unwrap();
    let mut b = depot.begin_chain(7).unwrap();
    depot.append_history_frame(&mut b, &payload(76, 100)).unwrap();
    depot.finish_chain(b, &payload(77, 100), None).unwrap();
    assert_eq!(depot.read_f0(7).unwrap(), payload(77, 100));
}

#[test]
fn cold_bytes_stay_but_are_accounted_dead() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");
    let depot = Depot::open(cfg(root.clone())).unwrap();

    // Two seals → two cold frames on chain 3, of known sizes (the
    // sealed accumulator's bytes move to cold verbatim).
    depot.prepend(3, &payload(30, 300), None, false).unwrap();
    depot.prepend(3, &payload(31, 300), Some(&payload(32, 1000)), false).unwrap();
    depot.prepend(3, &payload(33, 300), Some(&payload(34, 2000)), true).unwrap(); // seals the 1000
    depot.prepend(3, &payload(35, 300), Some(&payload(36, 500)), true).unwrap(); // seals the 2000
    depot.flush().unwrap();

    let (cold_len, cold_dead) = depot.cold_stats();
    let chain_cold = 2 * HEADER_LEN + 1000 + 2000;
    assert_eq!(cold_len, 1 + chain_cold, "pad byte + the two sealed frames");
    assert_eq!(cold_dead, 0, "everything reachable while the chain lives");

    depot.delete_chain(3).unwrap();
    depot.collect().unwrap();

    // Cold: nothing reclaimed, everything accounted.
    let (cold_len_after, cold_dead_after) = depot.cold_stats();
    assert_eq!(cold_len_after, cold_len, "cold is append-only: bytes stay");
    assert_eq!(cold_dead_after, chain_cold, "the chain's cold frames are the dead ledger");
    assert_eq!(dir_bytes(&root.join("cold")), cold_len, "on-disk cold file untouched");

    // The accounting survives a clean reopen (persisted by collect).
    drop(depot);
    let depot = Depot::open(cfg(root)).unwrap();
    assert!(!depot.counters_rebuilt_on_open());
    assert_eq!(depot.cold_stats(), (cold_len, chain_cold));
}

#[test]
fn crash_between_delete_and_sidecar_persist_trips_fence_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");

    // Session 1: two chains (one with a sealed cold frame), clean flush.
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        depot.prepend(1, &payload(10, 400), None, false).unwrap();
        depot.prepend(1, &payload(11, 400), Some(&payload(12, 600)), false).unwrap();
        depot.prepend(2, &payload(20, 800), None, false).unwrap();
        depot.prepend(2, &payload(21, 800), Some(&payload(22, 900)), false).unwrap();
        depot.prepend(2, &payload(23, 800), Some(&payload(24, 700)), true).unwrap();
        depot.flush().unwrap();
    }

    // Session 2: loads the sidecar, deletes chain 2, then "crashes" —
    // no flush, so the sidecar the delete dropped is never re-written.
    // (Deletion changes no file length: WITHOUT the drop, the stale
    // sidecar would pass the length fence with undercounted dead.)
    let (stats_at_delete, cold_at_delete) = {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        assert!(!depot.counters_rebuilt_on_open(), "clean shutdown → sidecar loads");
        depot.delete_chain(2).unwrap();
        assert!(
            !root.join("counters").exists(),
            "delete must drop the sidecar — it is the only fence deletion has"
        );
        (depot.tier_stats(), depot.cold_stats())
    };

    // Session 3: the fence trips (missing sidecar), the rebuild runs
    // once, and reproduces the delete's accounting exactly — tier dead
    // bytes AND the cold dead ledger.
    let (stats_rebuilt, cold_rebuilt) = {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        assert!(depot.counters_rebuilt_on_open(), "crashed delete must force the rebuild");
        assert!(matches!(depot.read_f0(2), Err(Error::NoFrame)), "the delete itself stuck");
        assert_eq!(depot.read_f0(1).unwrap(), payload(11, 400), "survivor intact");
        (depot.tier_stats(), depot.cold_stats())
    };
    assert_eq!(stats_rebuilt, stats_at_delete, "rebuild == the delete's tier accounting");
    assert_eq!(cold_rebuilt, cold_at_delete, "rebuild == the delete's cold accounting");

    // Session 4: the rebuild was persisted — the fence trips ONCE.
    let depot = Depot::open(cfg(root)).unwrap();
    assert!(!depot.counters_rebuilt_on_open(), "rebuild must have been persisted");
    assert_eq!(depot.tier_stats(), stats_rebuilt);
    assert_eq!(depot.cold_stats(), cold_rebuilt);
}
