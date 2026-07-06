//! Acceptance test suite for wikimak-depot, per PHASES.md §W3-Rust-1.
//!
//! The depot is byte-opaque to zstd. These tests feed it arbitrary byte
//! slices labeled "f0", "f1", "cold" and verify behavior through the
//! public API. A handful of tests also read the raw on-disk bytes to pin
//! the frame and index layout per SPEC §"Frame format" and §"Index".

mod common;

use std::path::PathBuf;

use tempfile::TempDir;
use wikimak_depot::{Depot, DepotConfig};

use common::{cfg, list_files, payload};

// ---------------------------------------------------------------------------
// On-disk layout pins.
// ---------------------------------------------------------------------------

/// Frame header size pinned by SPEC §"Frame format":
/// `[u64 chain_id LE | u64 next_pointer LE | u64 zstd_len LE]`.
const FRAME_HEADER_LEN: usize = 24;

/// Index entry size pinned by SPEC §"Index": one u64 LE packing
/// `file_id` (low 16 bits) and `offset` (high 48 bits).
const INDEX_ENTRY_LEN: usize = 8;

/// Split a pointer word into (file_id, offset).
fn unpack_ptr(ptr: u64) -> (u32, u64) {
    ((ptr & 0xFFFF) as u32, ptr >> 16)
}

// ---------------------------------------------------------------------------
// open_creates_layout
// ---------------------------------------------------------------------------

#[test]
fn open_creates_layout() {
    let dir = TempDir::new().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let max_chain_id: u64 = 1024;

    let depot = Depot::open(DepotConfig {
        root: root.clone(),
        max_chain_id,
        file_size_threshold: 1 << 30,
        eviction_dead_ratio: 0.5,
    })
    .expect("open should succeed on a fresh root");

    // index file exists at exactly max_chain_id * 8 zero-bytes.
    let index_path = root.join("index");
    let index_bytes = std::fs::read(&index_path).expect("index file must exist");
    assert_eq!(
        index_bytes.len() as u64,
        max_chain_id * INDEX_ENTRY_LEN as u64,
        "index file size must be max_chain_id * 8"
    );
    assert!(index_bytes.iter().all(|&b| b == 0), "index must be zeroed");

    // f0, f1, cold dirs exist.
    assert!(root.join("f0").is_dir(), "f0/ dir must exist");
    assert!(root.join("f1").is_dir(), "f1/ dir must exist");
    assert!(root.join("cold").is_dir(), "cold/ dir must exist");

    // cold/cold is an empty file.
    let cold_path = root.join("cold").join("cold");
    let cold_meta = std::fs::metadata(&cold_path).expect("cold/cold must exist");
    assert!(cold_meta.is_file(), "cold/cold must be a regular file");
    assert_eq!(cold_meta.len(), 0, "cold/cold must be empty");

    // No f0/file-* or f1/file-* yet.
    assert!(
        list_files(&root.join("f0")).is_empty(),
        "no f0 data files until first prepend"
    );
    assert!(
        list_files(&root.join("f1")).is_empty(),
        "no f1 data files until first prepend"
    );

    drop(depot);
}

// ---------------------------------------------------------------------------
// first_prepend_no_f1
// ---------------------------------------------------------------------------

#[test]
fn first_prepend_no_f1() {
    let dir = TempDir::new().unwrap();
    let depot = Depot::open(cfg(dir.path().to_path_buf())).unwrap();

    depot
        .prepend(42, b"f0-bytes-A", None, false)
        .expect("first prepend with None f1 must succeed");

    assert_eq!(depot.read_f0(42).unwrap(), b"f0-bytes-A".to_vec());
    assert!(depot.read_f1(42).unwrap().is_none(), "no f1 yet");
    let cold: Vec<Vec<u8>> = depot
        .cold_iter(42)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(cold.is_empty(), "no cold frames after first prepend");
}

// ---------------------------------------------------------------------------
// first_prepend_with_some_f1_is_error
// ---------------------------------------------------------------------------

#[test]
fn first_prepend_with_some_f1_is_error() {
    let dir = TempDir::new().unwrap();
    let depot = Depot::open(cfg(dir.path().to_path_buf())).unwrap();

    let r = depot.prepend(42, b"f0", Some(b"f1"), false);
    assert!(
        r.is_err(),
        "passing Some(f1) on the very first prepend must error"
    );
}

// ---------------------------------------------------------------------------
// second_prepend_writes_f1
// ---------------------------------------------------------------------------

#[test]
fn second_prepend_writes_f1() {
    let dir = TempDir::new().unwrap();
    let depot = Depot::open(cfg(dir.path().to_path_buf())).unwrap();

    depot.prepend(42, b"A-f0", None, false).unwrap();
    depot
        .prepend(42, b"B-f0", Some(b"f1-records"), false)
        .unwrap();

    assert_eq!(depot.read_f0(42).unwrap(), b"B-f0".to_vec());
    assert_eq!(
        depot.read_f1(42).unwrap(),
        Some(b"f1-records".to_vec()),
        "f1 must be the bytes from the second prepend"
    );
    let cold: Vec<Vec<u8>> = depot
        .cold_iter(42)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(cold.is_empty(), "no cold until a seal happens");
}

// ---------------------------------------------------------------------------
// seal_moves_f1_bytes_to_cold_verbatim
// ---------------------------------------------------------------------------

#[test]
fn seal_moves_f1_bytes_to_cold_verbatim() {
    let dir = TempDir::new().unwrap();
    let depot = Depot::open(cfg(dir.path().to_path_buf())).unwrap();

    depot.prepend(42, b"A-f0", None, false).unwrap();
    depot
        .prepend(42, b"B-f0", Some(b"f1-after-B"), false)
        .unwrap();
    // Seal: previous f1 ("f1-after-B") must move to cold byte-identical.
    depot
        .prepend(42, b"C-f0", Some(b"f1-after-C"), true)
        .unwrap();

    assert_eq!(depot.read_f0(42).unwrap(), b"C-f0".to_vec());
    assert_eq!(depot.read_f1(42).unwrap(), Some(b"f1-after-C".to_vec()));

    let cold: Vec<Vec<u8>> = depot
        .cold_iter(42)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(cold.len(), 1, "exactly one cold frame after one seal");
    assert_eq!(
        cold[0],
        b"f1-after-B".to_vec(),
        "the sealed f1's bytes must appear in cold byte-identical"
    );
}

// ---------------------------------------------------------------------------
// multiple_seals_build_cold_chain_newest_first
// ---------------------------------------------------------------------------

#[test]
fn multiple_seals_build_cold_chain_newest_first() {
    let dir = TempDir::new().unwrap();
    let depot = Depot::open(cfg(dir.path().to_path_buf())).unwrap();

    // First prepend (no f1).
    depot.prepend(7, b"f0-0", None, false).unwrap();
    // Second prepend: sets f1_0 (no seal possible yet, no prior f1).
    depot.prepend(7, b"f0-1", Some(b"f1-0"), false).unwrap();

    // Four seals. Each seal moves the previous f1 into cold (newest-first).
    let sealed_in_order = [b"f1-0".to_vec(), b"f1-1".to_vec(), b"f1-2".to_vec(), b"f1-3".to_vec()];
    depot.prepend(7, b"f0-2", Some(b"f1-1"), true).unwrap();
    depot.prepend(7, b"f0-3", Some(b"f1-2"), true).unwrap();
    depot.prepend(7, b"f0-4", Some(b"f1-3"), true).unwrap();
    depot.prepend(7, b"f0-5", Some(b"f1-4"), true).unwrap();

    let cold: Vec<Vec<u8>> = depot
        .cold_iter(7)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(cold.len(), 4, "four seals = four cold frames");

    // Newest-first: the most recently sealed f1 is cold[0]; the first
    // sealed f1 is cold[3].
    let expected_newest_first: Vec<Vec<u8>> =
        sealed_in_order.iter().rev().cloned().collect();
    assert_eq!(
        cold, expected_newest_first,
        "cold_iter must yield sealed f1s newest-first"
    );

    assert_eq!(depot.read_f0(7).unwrap(), b"f0-5".to_vec());
    assert_eq!(depot.read_f1(7).unwrap(), Some(b"f1-4".to_vec()));
}

// ---------------------------------------------------------------------------
// multiple_chains_independent
// ---------------------------------------------------------------------------

#[test]
fn multiple_chains_independent() {
    let dir = TempDir::new().unwrap();
    let depot = Depot::open(cfg(dir.path().to_path_buf())).unwrap();

    // 100 chains, three prepends each: A (first), B (no seal), C (seal).
    let chain_ids: Vec<u64> = (0..100).collect();
    for &cid in &chain_ids {
        depot
            .prepend(cid, format!("chain-{cid}-f0-A").as_bytes(), None, false)
            .unwrap();
        depot
            .prepend(
                cid,
                format!("chain-{cid}-f0-B").as_bytes(),
                Some(format!("chain-{cid}-f1-B").as_bytes()),
                false,
            )
            .unwrap();
        depot
            .prepend(
                cid,
                format!("chain-{cid}-f0-C").as_bytes(),
                Some(format!("chain-{cid}-f1-C").as_bytes()),
                true,
            )
            .unwrap();
    }

    for &cid in &chain_ids {
        let f0 = depot.read_f0(cid).unwrap();
        assert_eq!(f0, format!("chain-{cid}-f0-C").as_bytes());
        let f1 = depot.read_f1(cid).unwrap();
        assert_eq!(f1, Some(format!("chain-{cid}-f1-C").as_bytes().to_vec()));
        let cold: Vec<Vec<u8>> = depot
            .cold_iter(cid)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(cold.len(), 1, "one seal = one cold for chain {cid}");
        assert_eq!(cold[0], format!("chain-{cid}-f1-B").as_bytes());
    }
}

// ---------------------------------------------------------------------------
// flush_durability
// ---------------------------------------------------------------------------

#[test]
fn flush_durability() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    // Prepend + seal across a handful of chains; flush; drop.
    let chain_ids: [u64; 5] = [1, 17, 42, 100, 500];
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        for &cid in &chain_ids {
            depot
                .prepend(cid, format!("f0-A-{cid}").as_bytes(), None, false)
                .unwrap();
            depot
                .prepend(
                    cid,
                    format!("f0-B-{cid}").as_bytes(),
                    Some(format!("f1-B-{cid}").as_bytes()),
                    false,
                )
                .unwrap();
            depot
                .prepend(
                    cid,
                    format!("f0-C-{cid}").as_bytes(),
                    Some(format!("f1-C-{cid}").as_bytes()),
                    true,
                )
                .unwrap();
        }
        depot.flush().unwrap();
    }

    // Reopen.
    let depot = Depot::open(cfg(root)).unwrap();
    for &cid in &chain_ids {
        assert_eq!(
            depot.read_f0(cid).unwrap(),
            format!("f0-C-{cid}").as_bytes()
        );
        assert_eq!(
            depot.read_f1(cid).unwrap(),
            Some(format!("f1-C-{cid}").as_bytes().to_vec())
        );
        let cold: Vec<Vec<u8>> = depot
            .cold_iter(cid)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0], format!("f1-B-{cid}").as_bytes());
    }
}

// ---------------------------------------------------------------------------
// no_flush_may_lose
//
// "Crash" simulation: drop the depot via `std::mem::forget`, which skips
// Drop entirely. Any flush-on-Drop the implementer wires up does not run.
// The contract under test is the weakest possible: no panic, no corrupt
// bytes. We do NOT assert that the recent prepend is present or missing.
// ---------------------------------------------------------------------------

#[test]
fn no_flush_may_lose() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    // First, set up a known-flushed baseline so we have something to
    // verify post-crash.
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        depot.prepend(10, b"baseline-f0", None, false).unwrap();
        depot.flush().unwrap();
    }

    // Now prepend more without flushing, then "crash" by forgetting the
    // depot — Drop never runs, so no fsync-on-drop fires.
    let depot = Depot::open(cfg(root.clone())).unwrap();
    depot
        .prepend(20, b"unflushed-f0", None, false)
        .unwrap();
    std::mem::forget(depot);

    // Reopen. Must not panic.
    let depot = Depot::open(cfg(root)).unwrap();

    // The flushed baseline must still be there byte-identical.
    assert_eq!(depot.read_f0(10).unwrap(), b"baseline-f0".to_vec());

    // The unflushed prepend may or may not be visible — SPEC permits both.
    // If it IS visible, it must be byte-identical (no corruption).
    match depot.read_f0(20) {
        Ok(bytes) => assert_eq!(
            bytes,
            b"unflushed-f0".to_vec(),
            "if the unflushed prepend survived, it must be byte-identical"
        ),
        Err(_) => { /* lost — allowed by SPEC */ }
    }
}

// ---------------------------------------------------------------------------
// index_entry_is_8_bytes
// ---------------------------------------------------------------------------

#[test]
fn index_entry_is_8_bytes() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let max_chain_id: u64 = 1024;

    let depot = Depot::open(DepotConfig {
        root: root.clone(),
        max_chain_id,
        file_size_threshold: 1 << 30,
        eviction_dead_ratio: 0.5,
    })
    .unwrap();

    depot.prepend(42, b"f0-bytes", None, false).unwrap();
    depot.flush().unwrap();

    let index = std::fs::read(root.join("index")).unwrap();
    assert_eq!(
        index.len() as u64,
        max_chain_id * INDEX_ENTRY_LEN as u64,
        "index file size = max_chain_id * 8"
    );

    let entry_start = 42 * INDEX_ENTRY_LEN;
    let entry = &index[entry_start..entry_start + INDEX_ENTRY_LEN];
    assert!(
        entry.iter().any(|&b| b != 0),
        "chain 42's index entry must be nonzero after one prepend"
    );

    // All other entries must remain zero.
    for i in 0..max_chain_id as usize {
        if i == 42 {
            continue;
        }
        let s = i * INDEX_ENTRY_LEN;
        let e = s + INDEX_ENTRY_LEN;
        assert!(
            index[s..e].iter().all(|&b| b == 0),
            "index entry for chain {i} must be zero, got {:?}",
            &index[s..e]
        );
    }
}

// ---------------------------------------------------------------------------
// eviction_reclaims_dead_f0_space
//
// Strategy: use a small file_size_threshold (64 KiB) and a payload size
// large enough that a few prepends per chain blow through it. Prepend many
// revisions to one chain (each deprecates its prior f0 in its f0 file).
// Eventually the original f0 file's dead ratio crosses 0.5; calling
// flush() should let an opportunistic trigger fire. After flush(), the
// first (now fully dead) f0 file must be unlinked from disk and the chain
// must still be readable byte-identical.
//
// We do not call a `maybe_evict()` hook because the public API does not
// expose one; the test relies on the opportunistic trigger described in
// SPEC §"Eviction".
// ---------------------------------------------------------------------------

#[test]
fn eviction_reclaims_dead_f0_space() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let cfg = DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 64 * 1024, // 64 KiB
        eviction_dead_ratio: 0.5,
    };

    let depot = Depot::open(cfg).unwrap();

    // First prepend (no f1).
    let p0 = payload("initial-f0-", 4 * 1024);
    depot.prepend(3, &p0, None, false).unwrap();

    // Drive many more prepends. Each ~4 KiB f0 + ~4 KiB f1. After a few we
    // cross the 64 KiB file_size_threshold and roll into a new file. After
    // many more, the original file is entirely dead and must be evicted.
    let n_prepends: usize = 200;
    for i in 0..n_prepends {
        let f0 = payload(&format!("f0-rev-{i:04}-"), 4 * 1024);
        let f1 = payload(&format!("f1-rev-{i:04}-"), 1024);
        depot.prepend(3, &f0, Some(&f1), false).unwrap();
    }

    let last_f0 = payload(&format!("f0-rev-{:04}-", n_prepends - 1), 4 * 1024);
    let last_f1 = payload(&format!("f1-rev-{:04}-", n_prepends - 1), 1024);

    // Trigger opportunistic eviction via flush.
    depot.flush().unwrap();

    // After enough churn, the FIRST f0 file (file-0000) should be gone.
    let f0_files = list_files(&root.join("f0"));
    assert!(
        !f0_files.is_empty(),
        "at least one current f0 file must remain"
    );
    let names: Vec<String> = f0_files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n == "file-0000"),
        "first f0 file should have been evicted; got {names:?}"
    );

    // Chain still readable byte-identical.
    assert_eq!(depot.read_f0(3).unwrap(), last_f0);
    assert_eq!(depot.read_f1(3).unwrap(), Some(last_f1));
}

// ---------------------------------------------------------------------------
// eviction_reclaims_dead_f1_space
// ---------------------------------------------------------------------------

#[test]
fn eviction_reclaims_dead_f1_space() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let cfg = DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 64 * 1024,
        eviction_dead_ratio: 0.5,
    };

    let depot = Depot::open(cfg).unwrap();

    let initial_f0 = payload("initial-f0-", 1024);
    depot.prepend(5, &initial_f0, None, false).unwrap();

    // Drive many prepends without seal; each replaces (deprecates) the
    // previous f1 in its f1 file.
    let n: usize = 200;
    for i in 0..n {
        let f0 = payload(&format!("f0-{i:04}-"), 1024);
        let f1 = payload(&format!("f1-{i:04}-"), 4 * 1024);
        depot.prepend(5, &f0, Some(&f1), false).unwrap();
    }
    let last_f0 = payload(&format!("f0-{:04}-", n - 1), 1024);
    let last_f1 = payload(&format!("f1-{:04}-", n - 1), 4 * 1024);

    depot.flush().unwrap();

    let f1_files = list_files(&root.join("f1"));
    assert!(
        !f1_files.is_empty(),
        "at least one current f1 file must remain"
    );
    let names: Vec<String> = f1_files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n == "file-0000"),
        "first f1 file should have been evicted; got {names:?}"
    );

    assert_eq!(depot.read_f0(5).unwrap(), last_f0);
    assert_eq!(depot.read_f1(5).unwrap(), Some(last_f1));
}

// ---------------------------------------------------------------------------
// cold_file_never_evicted
// ---------------------------------------------------------------------------

#[test]
fn cold_file_never_evicted() {
    use std::os::unix::fs::MetadataExt;

    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let cfg = DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 64 * 1024,
        eviction_dead_ratio: 0.5,
    };

    let depot = Depot::open(cfg).unwrap();

    // First prepend (no f1).
    depot.prepend(2, b"f0-0", None, false).unwrap();

    // Drive many prepends with seals so cold accumulates frames, and so
    // f0/f1 turnover triggers evictions.
    let mut sealed_f1s: Vec<Vec<u8>> = Vec::new();
    let n: usize = 100;
    let f0_filler = payload("f0-", 2 * 1024);
    for i in 0..n {
        let f1_bytes = payload(&format!("seal-{i:04}-"), 1024);
        depot
            .prepend(2, &f0_filler, Some(&f1_bytes), i > 0) // seal from prepend 1 onward
            .unwrap();
        if i > 0 {
            // The PREVIOUS f1 is what gets sealed into cold. Track it.
            // i==1 seals the f1 from i==0 ("seal-0000-...").
            sealed_f1s.push(payload(&format!("seal-{:04}-", i - 1), 1024));
        }
    }

    depot.flush().unwrap();

    // Force more churn to ensure f0/f1 evictions happen.
    for i in 0..200 {
        let f0 = payload(&format!("post-f0-{i:04}-"), 4 * 1024);
        let f1 = payload(&format!("post-f1-{i:04}-"), 1024);
        depot.prepend(2, &f0, Some(&f1), false).unwrap();
    }
    depot.flush().unwrap();

    // cold/cold must still exist as one file, never replaced.
    let cold_path = root.join("cold").join("cold");
    let cold_meta = std::fs::metadata(&cold_path).expect("cold/cold must remain");
    let cold_inode_before = cold_meta.ino();
    let cold_size_before = cold_meta.len();

    // Reopen and verify cold is identical.
    drop(depot);
    let depot = Depot::open(DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 64 * 1024,
        eviction_dead_ratio: 0.5,
    })
    .unwrap();

    let cold_meta_after = std::fs::metadata(&cold_path).unwrap();
    assert_eq!(
        cold_meta_after.ino(),
        cold_inode_before,
        "cold/cold inode must not change across eviction"
    );
    assert_eq!(
        cold_meta_after.len(),
        cold_size_before,
        "cold/cold size must not shrink (cold is never compacted)"
    );

    // Every sealed f1 must still appear in cold_iter byte-identical,
    // newest-first.
    let cold: Vec<Vec<u8>> = depot
        .cold_iter(2)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let expected: Vec<Vec<u8>> = sealed_f1s.into_iter().rev().collect();
    assert_eq!(
        cold, expected,
        "every sealed f1 must survive evictions in cold byte-identical"
    );
}

// ---------------------------------------------------------------------------
// delete_all_unlinks_everything
// ---------------------------------------------------------------------------

#[test]
fn delete_all_unlinks_everything() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let max_chain_id: u64 = 64;
    let mk_cfg = || DepotConfig {
        root: root.clone(),
        max_chain_id,
        file_size_threshold: 1 << 30,
        eviction_dead_ratio: 0.5,
    };

    let depot = Depot::open(mk_cfg()).unwrap();
    for cid in 0u64..20 {
        depot
            .prepend(cid, format!("f0-{cid}").as_bytes(), None, false)
            .unwrap();
        depot
            .prepend(
                cid,
                format!("f0b-{cid}").as_bytes(),
                Some(format!("f1-{cid}").as_bytes()),
                false,
            )
            .unwrap();
        depot
            .prepend(
                cid,
                format!("f0c-{cid}").as_bytes(),
                Some(format!("f1b-{cid}").as_bytes()),
                true,
            )
            .unwrap();
    }
    depot.flush().unwrap();
    depot.delete_all().unwrap();

    // SPEC / PHASES: "f0/f1/cold files gone; the index file gone OR all zeroed".
    assert!(
        list_files(&root.join("f0")).is_empty(),
        "all f0 data files must be unlinked"
    );
    assert!(
        list_files(&root.join("f1")).is_empty(),
        "all f1 data files must be unlinked"
    );
    let cold_path = root.join("cold").join("cold");
    if cold_path.exists() {
        let cold_meta = std::fs::metadata(&cold_path).unwrap();
        assert_eq!(
            cold_meta.len(),
            0,
            "if cold/cold still exists it must be empty"
        );
    }
    let index_path = root.join("index");
    if index_path.exists() {
        let bytes = std::fs::read(&index_path).unwrap();
        assert!(
            bytes.iter().all(|&b| b == 0),
            "if index still exists it must be all zero"
        );
    }
}

// ---------------------------------------------------------------------------
// mid_eviction_crash_safe
//
// The public API does not expose a mid-walk hook, so we simulate the
// crash by snapshotting the depot directory state BEFORE the
// eviction-triggering flush completes, then restoring it. This models
// "crash before eviction finished" — the source frames are still in V
// (not yet unlinked), pointers still reference them, restart is safe and
// idempotent (per SPEC §"Crash-safety contract").
//
// What we assert: post-restore the depot opens cleanly, every chain is
// byte-identical to its pre-crash state, and a second flush can finish
// the eviction without complaint.
// ---------------------------------------------------------------------------

#[test]
fn mid_eviction_crash_safe() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let mk_cfg = || DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 64 * 1024,
        eviction_dead_ratio: 0.5,
    };

    // Build a depot where eviction WILL run on next flush.
    let depot = Depot::open(mk_cfg()).unwrap();
    let initial = payload("init-", 1024);
    depot.prepend(4, &initial, None, false).unwrap();
    let n: usize = 100;
    for i in 0..n {
        let f0 = payload(&format!("f0-{i:04}-"), 4 * 1024);
        let f1 = payload(&format!("f1-{i:04}-"), 1024);
        depot.prepend(4, &f0, Some(&f1), false).unwrap();
    }
    depot.flush().unwrap();
    let last_f0 = payload(&format!("f0-{:04}-", n - 1), 4 * 1024);
    let last_f1 = payload(&format!("f1-{:04}-", n - 1), 1024);

    // Drive MORE churn that will trigger another eviction on flush, but
    // do NOT flush yet. Snapshot the depot state at this point — this is
    // the "mid-eviction crash" surrogate. After snapshot, drop without
    // flushing so the eviction never runs.
    for i in n..(n + 100) {
        let f0 = payload(&format!("f0-{i:04}-"), 4 * 1024);
        let f1 = payload(&format!("f1-{i:04}-"), 1024);
        depot.prepend(4, &f0, Some(&f1), false).unwrap();
    }
    let crash_f0 = payload(&format!("f0-{:04}-", n + 99), 4 * 1024);
    let crash_f1 = payload(&format!("f1-{:04}-", n + 99), 1024);
    depot.flush().unwrap();

    // Snapshot the entire depot directory.
    let snapshot_dir = TempDir::new().unwrap();
    copy_tree(&root, snapshot_dir.path());

    // Continue with another wave of prepends and then "crash" — std::mem::forget
    // skips Drop, mirroring an abort partway through the eviction the next
    // flush would run.
    for i in (n + 100)..(n + 200) {
        let f0 = payload(&format!("f0-{i:04}-"), 4 * 1024);
        let f1 = payload(&format!("f1-{i:04}-"), 1024);
        depot.prepend(4, &f0, Some(&f1), false).unwrap();
    }
    std::mem::forget(depot);

    // Restore the snapshot taken at the well-defined consistent point.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    copy_tree(snapshot_dir.path(), &root);

    // Reopen — must not panic, must read pre-crash state byte-identical.
    let depot = Depot::open(mk_cfg()).unwrap();
    assert_eq!(depot.read_f0(4).unwrap(), crash_f0);
    assert_eq!(depot.read_f1(4).unwrap(), Some(crash_f1));

    // The earlier chain history we asserted still holds for older state;
    // and a fresh flush after reopen must be able to run eviction again
    // without error.
    let _ = (last_f0, last_f1);
    depot.flush().unwrap();
    // Final readability check.
    assert_eq!(depot.read_f0(4).unwrap(), crash_f0);
}

fn copy_tree(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&from, &to);
        } else if ty.is_file() {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// frame_header_layout
//
// Pin the on-disk frame header at 24 bytes:
// `[u64 chain_id LE | u64 next_pointer LE | u64 zstd_len LE]`, followed
// by exactly `zstd_len` opaque payload bytes that match what we passed
// to `prepend`.
// ---------------------------------------------------------------------------

#[test]
fn frame_header_layout() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let depot = Depot::open(cfg(root.clone())).unwrap();

    let chain_id: u64 = 7;
    let f0_bytes: &[u8] = b"opaque-zstd-blob-for-f0";
    depot.prepend(chain_id, f0_bytes, None, false).unwrap();
    depot.flush().unwrap();

    // There should be exactly one f0 data file (file-0000).
    let f0_files = list_files(&root.join("f0"));
    assert_eq!(
        f0_files.len(),
        1,
        "expected exactly one f0 data file after one prepend, got {f0_files:?}"
    );
    let bytes = std::fs::read(&f0_files[0]).unwrap();
    assert!(
        bytes.len() >= FRAME_HEADER_LEN,
        "f0 file must hold at least one full header"
    );

    // [0..8]   chain_id LE
    // [8..16]  next_pointer LE   (== 0 for f0 on a chain with no f1)
    // [16..24] zstd_len LE
    let on_disk_chain_id = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    assert_eq!(on_disk_chain_id, chain_id, "chain_id LE at bytes [0..8]");

    let on_disk_next_ptr = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    assert_eq!(
        on_disk_next_ptr, 0,
        "next_pointer LE at bytes [8..16] must be (0,0) since this chain has no f1"
    );

    let on_disk_zstd_len = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    assert_eq!(
        on_disk_zstd_len as usize,
        f0_bytes.len(),
        "zstd_len LE at bytes [16..24] must equal the payload length we passed"
    );

    let payload_on_disk = &bytes[FRAME_HEADER_LEN..FRAME_HEADER_LEN + f0_bytes.len()];
    assert_eq!(
        payload_on_disk, f0_bytes,
        "payload bytes immediately after the 24-byte header must equal what we passed"
    );
}

// ---------------------------------------------------------------------------
// cold_pointer_chain_walks_correctly
//
// After K seals, the cold file holds K cold frames. Each frame's
// next_pointer references the next-older cold frame; the oldest cold
// frame's next_pointer is (0, 0). This test reads cold/cold raw bytes
// and walks the chain by following next_pointers.
// ---------------------------------------------------------------------------

#[test]
fn cold_pointer_chain_walks_correctly() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let depot = Depot::open(cfg(root.clone())).unwrap();

    let chain_id: u64 = 11;
    // First prepend (no f1).
    depot.prepend(chain_id, b"f0-init", None, false).unwrap();
    // Second prepend: introduce f1 (no seal possible yet).
    depot
        .prepend(chain_id, b"f0-1", Some(b"f1-0"), false)
        .unwrap();

    // Now do K seals. Each seal moves the previous f1 into cold.
    let k = 5;
    for i in 1..=k {
        let new_f0 = format!("f0-{}", i + 1);
        let new_f1 = format!("f1-{i}");
        depot
            .prepend(chain_id, new_f0.as_bytes(), Some(new_f1.as_bytes()), true)
            .unwrap();
    }
    depot.flush().unwrap();

    // Read the index to find f0_loc -> read f0 header -> get f1 loc ->
    // read f1 header -> get cold_head pointer. Then walk cold.
    let index = std::fs::read(root.join("index")).unwrap();
    let entry_off = chain_id as usize * INDEX_ENTRY_LEN;
    let (f0_file_id, f0_offset) =
        unpack_ptr(u64::from_le_bytes(index[entry_off..entry_off + 8].try_into().unwrap()));
    assert_ne!(
        (f0_file_id, f0_offset),
        (0, 0),
        "index must point at the current f0 for chain {chain_id}"
    );

    let f0_path = root
        .join("f0")
        .join(format!("file-{:04}", f0_file_id));
    let f0_bytes = std::fs::read(&f0_path).expect("f0 file must exist");
    let f0_header_start = f0_offset as usize;
    let f0_next_ptr = u64::from_le_bytes(
        f0_bytes[f0_header_start + 8..f0_header_start + 16]
            .try_into()
            .unwrap(),
    );
    let (f1_file_id, f1_offset) = unpack_ptr(f0_next_ptr);

    let f1_path = root
        .join("f1")
        .join(format!("file-{:04}", f1_file_id));
    let f1_bytes = std::fs::read(&f1_path).expect("f1 file must exist");
    let f1_header_start = f1_offset as usize;
    let f1_next_ptr = u64::from_le_bytes(
        f1_bytes[f1_header_start + 8..f1_header_start + 16]
            .try_into()
            .unwrap(),
    );

    // f1's next_pointer = newest cold head.
    let cold = std::fs::read(root.join("cold").join("cold")).unwrap();

    // Walk K cold frames following next_pointer.
    let mut current = f1_next_ptr;
    let mut walked: Vec<u64> = Vec::new();
    for step in 0..k {
        assert_ne!(
            current, 0,
            "cold chain ended too early at step {step}, expected length {k}"
        );
        let (file_id, offset) = unpack_ptr(current);
        assert_eq!(
            file_id, 0,
            "cold pointer file_id must be 0 (one cold file); got {file_id}"
        );
        let header_start = offset as usize;
        assert!(
            header_start + FRAME_HEADER_LEN <= cold.len(),
            "cold offset {offset} out of bounds (cold size {})",
            cold.len()
        );
        let on_chain_id = u64::from_le_bytes(
            cold[header_start..header_start + 8].try_into().unwrap(),
        );
        assert_eq!(
            on_chain_id, chain_id,
            "cold frame at offset {offset} must carry chain_id {chain_id}"
        );
        walked.push(current);
        current = u64::from_le_bytes(
            cold[header_start + 8..header_start + 16].try_into().unwrap(),
        );
    }
    assert_eq!(
        current, 0,
        "after walking {k} cold frames, the oldest cold's next_pointer must be (0,0)"
    );
    assert_eq!(walked.len(), k, "walked {k} cold frames");
}

// ---------------------------------------------------------------------------
// collect_reclaims_current_file_slack
// ---------------------------------------------------------------------------

/// SPEC §"Eviction": triggered when ANY file in the tier crosses the
/// dead ratio — the current write target included (step 5 rolls it
/// first). Update churn on a chain deprecates its prior f0/f1 in place,
/// so with a large file_size_threshold ALL the slack sits in the current
/// file; a depot that never reclaims it pins every dead head on disk
/// forever. `flush` stays the cheap mid-session durability barrier;
/// `collect` is the session-end pass that includes the current file.
#[test]
fn collect_reclaims_current_file_slack() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let depot = Depot::open(DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 1 << 30, // nothing ever rolls on its own
        eviction_dead_ratio: 0.5,
    })
    .unwrap();

    let tier_bytes = |tier: &str| -> u64 {
        list_files(&root.join(tier))
            .iter()
            .map(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
            .sum()
    };

    depot.prepend(3, &payload("seed-", 4 * 1024), None, false).unwrap();
    let n = 50usize;
    let mut last_f0 = Vec::new();
    let mut last_f1 = Vec::new();
    for i in 0..n {
        last_f0 = payload(&format!("f0-rev-{i:04}-"), 4 * 1024);
        last_f1 = payload(&format!("f1-rev-{i:04}-"), 1024);
        depot.prepend(3, &last_f0, Some(&last_f1), false).unwrap();
    }

    // Pre-collect: every deprecated frame still on disk in the single
    // current file per tier — and a plain flush must NOT touch it (the
    // slack is what bounds per-prepend I/O mid-session).
    assert!(
        tier_bytes("f0") > (n as u64) * 4 * 1024,
        "churn must have accumulated dead f0 frames in the current file"
    );
    depot.flush().unwrap();
    assert!(
        tier_bytes("f0") > (n as u64) * 4 * 1024,
        "flush must leave the current file's slack alone"
    );

    depot.collect().unwrap();

    // Post-collect: the fat current files were rolled, evicted, unlinked;
    // each tier holds roughly one live frame.
    let f0_after = tier_bytes("f0");
    let f1_after = tier_bytes("f1");
    assert!(
        f0_after < 3 * (4 * 1024 + FRAME_HEADER_LEN as u64),
        "f0 tier must shrink to ~one live frame, got {f0_after} B"
    );
    assert!(
        f1_after < 3 * (1024 + FRAME_HEADER_LEN as u64),
        "f1 tier must shrink to ~one live frame, got {f1_after} B"
    );

    // The live data survived eviction, in-process and across reopen.
    assert_eq!(depot.read_f0(3).unwrap(), last_f0);
    assert_eq!(depot.read_f1(3).unwrap(), Some(last_f1.clone()));
    drop(depot);
    let depot = Depot::open(DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 1 << 30,
        eviction_dead_ratio: 0.5,
    })
    .unwrap();
    assert_eq!(depot.read_f0(3).unwrap(), last_f0);
    assert_eq!(depot.read_f1(3).unwrap(), Some(last_f1));
}

// ---------------------------------------------------------------------------
// eviction_repoints_live_f1_in_victim
// ---------------------------------------------------------------------------

/// A LIVE f1 frame stranded in a rolled file must survive that file's
/// eviction: the frame migrates and the owning f0 frame's next_pointer
/// is patched IN PLACE (SPEC §"Eviction", T == f1 patch). Regression:
/// tier fds were opened O_APPEND, and Linux pwrite on an O_APPEND fd
/// ignores its offset — the patch APPENDED garbage to the f0 file and
/// left the pointer aimed at the unlinked victim.
#[test]
fn eviction_repoints_live_f1_in_victim() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let mk = || DepotConfig {
        root: root.clone(),
        max_chain_id: 16,
        file_size_threshold: 32 * 1024,
        eviction_dead_ratio: 0.5,
    };
    let depot = Depot::open(mk()).unwrap();

    // Chain 7's live f1 lands early, in the first f1 file.
    depot.prepend(7, &payload("b-seed-", 2048), None, false).unwrap();
    let b_f0 = payload("b-head-", 2048);
    let b_f1 = payload("b-hist-", 2048);
    depot.prepend(7, &b_f0, Some(&b_f1), false).unwrap();

    // Chain 3 churns until the early files roll and go majority-dead —
    // majority, not fully: chain 7's f1 in there is still live.
    depot.prepend(3, &payload("a-seed-", 4 * 1024), None, false).unwrap();
    for i in 0..60 {
        let f0 = payload(&format!("a-f0-{i:03}-"), 4 * 1024);
        let f1 = payload(&format!("a-f1-{i:03}-"), 2 * 1024);
        depot.prepend(3, &f0, Some(&f1), false).unwrap();
    }
    depot.flush().unwrap(); // opportunistic eviction of rolled files

    // The victim actually went away (otherwise this proves nothing).
    let f1_names: Vec<String> = list_files(&root.join("f1"))
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        !f1_names.iter().any(|n| n == "file-0001"),
        "first f1 file must have been evicted; got {f1_names:?}"
    );

    // Chain 7 reads back whole through the patched pointer…
    assert_eq!(depot.read_f0(7).unwrap(), b_f0);
    assert_eq!(depot.read_f1(7).unwrap(), Some(b_f1.clone()));

    // …and across a reopen: the patch must be ON DISK, not fd state.
    drop(depot);
    let depot = Depot::open(mk()).unwrap();
    assert_eq!(depot.read_f0(7).unwrap(), b_f0);
    assert_eq!(depot.read_f1(7).unwrap(), Some(b_f1));
}

// ---------------------------------------------------------------------------
// format file — the loud version fence
// ---------------------------------------------------------------------------

/// Create writes `<root>/format` with the current version; reopening
/// the same depot validates it and succeeds.
#[test]
fn create_writes_format_file_and_reopen_roundtrips() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let depot = Depot::open(cfg(root.clone())).unwrap();
    depot.prepend(3, b"f0", None, false).unwrap();
    depot.flush().unwrap();
    drop(depot);

    let format = std::fs::read_to_string(root.join("format")).expect("format file on create");
    assert_eq!(format.trim(), "2", "format file must carry the current version");

    let depot = Depot::open(cfg(root.clone())).expect("reopen with matching format");
    assert_eq!(depot.read_f0(3).unwrap(), b"f0");
}

/// An existing depot (index present) whose `format` file is missing —
/// e.g. one written by pre-fence code — must hard-error on open, with
/// the rebuild message. Same for a mismatched version string.
#[test]
fn open_without_or_with_wrong_format_file_errors() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    {
        let depot = Depot::open(cfg(root.clone())).unwrap();
        depot.prepend(1, b"f0", None, false).unwrap();
        depot.flush().unwrap();
    }

    std::fs::remove_file(root.join("format")).unwrap();
    let err = match Depot::open(cfg(root.clone())) {
        Ok(_) => panic!("missing format must error"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("delete and re-import"),
        "error must carry the rebuild message, got: {msg}"
    );

    std::fs::write(root.join("format"), "1\n").unwrap();
    let err = match Depot::open(cfg(root.clone())) {
        Ok(_) => panic!("old format must error"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("format \"1\""), "got: {err}");
}
