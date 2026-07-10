//! The index at enwiki scale must be SPARSE — real on-disk effect, not
//! a code-reading claim. `max_chain_id = 1e8` makes the index 800MB
//! LOGICAL (8 bytes/chain); creation goes through `ftruncate`
//! (`File::set_len`), so a depot that only ever touches a handful of
//! chains must allocate almost none of it. This is what lets the
//! wikipedia layer default to an enwiki-sized bound without taxing
//! small wikis.

use std::os::unix::fs::MetadataExt;

use tempfile::TempDir;
use wikimak_depot::{Depot, DepotConfig};

const ENWIKI_SCALE: u64 = 100_000_000;

fn cfg(root: std::path::PathBuf) -> DepotConfig {
    DepotConfig {
        root,
        max_chain_id: ENWIKI_SCALE,
        file_size_threshold: 1 << 30,
        eviction_dead_ratio: 0.5,
    }
}

/// Allocated bytes on disk (`st_blocks` is 512-byte units, regardless
/// of the filesystem block size).
fn allocated(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).unwrap().blocks() * 512
}

#[test]
fn hundred_million_chain_index_is_sparse() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");
    let depot = Depot::open(cfg(root.clone())).unwrap();

    let index = root.join("index");
    let md = std::fs::metadata(&index).unwrap();
    assert_eq!(md.len(), ENWIKI_SCALE * 8, "index is 8 bytes/chain");

    // Touch the very top of the id space: the flip dirties one index
    // page, nothing else.
    depot
        .prepend(ENWIKI_SCALE - 1, b"opaque-frame-bytes", None, false)
        .unwrap();
    depot.flush().unwrap();

    let alloc = allocated(&index);
    assert!(
        alloc < 16 << 20,
        "index must stay sparse after create+flush: {alloc} bytes \
         allocated of {} logical",
        ENWIKI_SCALE * 8
    );

    // Reopen (walks DATA files for dead-byte rebuild, never the whole
    // index) and read back: still correct, still sparse.
    drop(depot);
    let depot = Depot::open(cfg(root)).unwrap();
    assert_eq!(
        depot.read_f0(ENWIKI_SCALE - 1).unwrap(),
        b"opaque-frame-bytes".to_vec()
    );
    let alloc = allocated(&index);
    assert!(alloc < 16 << 20, "reopen materialized the index: {alloc} bytes");
}

/// The retired knob: a chain id beyond the initial hint GROWS the
/// index (sparse — real st_blocks effect, not a code-reading claim)
/// instead of erroring, capacity survives a reopen with the same tiny
/// hint (derived from disk, never compared), and the store round-trips.
#[test]
fn index_grows_sparse_beyond_the_initial_hint() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");
    let mut small = cfg(root.clone());
    small.max_chain_id = 16; // tiny hint; ids way past it must still land
    let depot = Depot::open(small).unwrap();

    let index = root.join("index");
    assert_eq!(std::fs::metadata(&index).unwrap().len(), 16 * 8, "hint sizes a fresh index");

    // Far beyond the hint: 5e7 forces growth to next_pow2(5e7+1) = 2^26 slots.
    const FAR: u64 = 50_000_000;
    depot.prepend(FAR, b"grown-chain-f0", None, false).unwrap();
    depot.prepend(3, b"small-chain-f0", None, false).unwrap();
    depot.flush().unwrap();

    let md = std::fs::metadata(&index).unwrap();
    assert_eq!(md.len(), (1u64 << 26) * 8, "growth = next_power_of_two(id+1) slots");
    let alloc = allocated(&index);
    assert!(
        alloc < 16 << 20,
        "grown index must stay sparse: {alloc} bytes allocated of {} logical",
        md.len()
    );

    // Reopen with the SAME tiny hint: capacity derives from disk, both
    // chains read back, ids in the gap are just empty chains.
    drop(depot);
    let mut small = cfg(root.clone());
    small.max_chain_id = 16;
    let depot = Depot::open(small).unwrap();
    assert_eq!(depot.read_f0(FAR).unwrap(), b"grown-chain-f0".to_vec());
    assert_eq!(depot.read_f0(3).unwrap(), b"small-chain-f0".to_vec());
    assert!(matches!(depot.read_f0(FAR - 1), Err(wikimak_depot::Error::NoFrame)));
    assert!(
        matches!(depot.read_f0(1 << 27), Err(wikimak_depot::Error::NoFrame)),
        "an id beyond capacity but under the ceiling is an empty chain, not an error"
    );
    assert!(allocated(&index) < 16 << 20, "reopen materialized the grown index");
}

/// The 2^40 sanity ceiling still rejects LOUDLY — and writes nothing:
/// no index growth, no tier bytes.
#[test]
fn ceiling_rejects_loudly_with_no_writes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");
    let mut small = cfg(root.clone());
    small.max_chain_id = 16;
    let depot = Depot::open(small).unwrap();

    for id in [wikimak_depot::CHAIN_ID_CEILING, wikimak_depot::CHAIN_ID_CEILING + 12345] {
        assert!(
            matches!(
                depot.prepend(id, b"never-lands", None, false),
                Err(wikimak_depot::Error::ChainIdOutOfRange)
            ),
            "chain id {id} at/above the ceiling must be rejected"
        );
        assert!(
            matches!(depot.read_f0(id), Err(wikimak_depot::Error::ChainIdOutOfRange)),
            "reads reject the ceiling too"
        );
    }
    // No write happened: the index kept its hint size and no tier file
    // holds a byte.
    assert_eq!(std::fs::metadata(root.join("index")).unwrap().len(), 16 * 8);
    for tier in ["f0", "f1"] {
        let bytes: u64 = std::fs::read_dir(root.join(tier))
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert_eq!(bytes, 0, "{tier} has bytes after a rejected id");
    }
    assert_eq!(std::fs::metadata(root.join("cold/cold")).unwrap().len(), 0);
}

/// `delete_all` must not dirty the index either: zeroing 1e8 chains
/// byte-by-byte through the mmap would fault in and write 800MB of
/// pages. It recreates the file sparse (ftruncate) instead.
#[test]
fn delete_all_recreates_a_sparse_index() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("depot");
    let depot = Depot::open(cfg(root.clone())).unwrap();
    for cid in [0u64, 1_000_000, ENWIKI_SCALE - 1] {
        depot.prepend(cid, b"doomed", None, false).unwrap();
    }
    depot.flush().unwrap();

    depot.delete_all().unwrap();

    let index = root.join("index");
    let md = std::fs::metadata(&index).unwrap();
    assert_eq!(md.len(), ENWIKI_SCALE * 8, "index keeps its logical size");
    let alloc = allocated(&index);
    assert!(
        alloc < 16 << 20,
        "delete_all materialized the index: {alloc} bytes allocated"
    );

    // The store reopens empty and stays sparse.
    let depot = Depot::open(cfg(root)).unwrap();
    for cid in [0u64, 1_000_000, ENWIKI_SCALE - 1] {
        assert!(
            matches!(depot.read_f0(cid), Err(wikimak_depot::Error::NoFrame)),
            "chain {cid} must be gone"
        );
    }
    assert!(alloc < 16 << 20);
}
