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
