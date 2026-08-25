//! Shared scaffolding for the wikimak-wikipedia acceptance suite.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use wikimak_depot::DepotConfig;
use wikimak_wikipedia::{Instance, InstanceConfig};

/// Default `InstanceConfig` rooted at `root`, with the depot tuned for
/// tests (1 GiB threshold, 0.5 eviction). Override fields after this.
pub fn cfg(root: PathBuf, max_chain_id: u64) -> InstanceConfig {
    let depot_root = root.join("depot");
    InstanceConfig {
        root,
        dbname: "testwiki".to_string(),
        max_chain_id,
        depot: DepotConfig {
            root: depot_root,
            max_chain_id,
            file_size_threshold: 1 << 30,
            eviction_dead_ratio: 0.5,
        },
        title_shard_count: 1,
        f1_seal_threshold_bytes: 0,
            title_seal_threshold_bytes: 1 << 20,
    }
}

/// Open a fresh `Instance` rooted under `tmp` with the given
/// `max_chain_id`. Used by most acceptance tests.
pub fn make_instance(tmp: &TempDir, max_chain_id: u64) -> Instance {
    let root = tmp.path().to_path_buf();
    Instance::open(cfg(root, max_chain_id)).expect("Instance::open on a fresh root")
}

/// Read a fixture from `tests/data/`.
pub fn fixture(name: &str) -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("data");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"))
}

/// All regular files immediately under `dir`, sorted by name.
pub fn list_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}
