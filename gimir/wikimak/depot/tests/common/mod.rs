//! Shared scaffolding for depot acceptance tests.

use std::path::{Path, PathBuf};

use wikimak_depot::DepotConfig;

/// Default depot config rooted at `root`. Small file threshold so eviction
/// tests can blow through the dead ratio without writing megabytes.
#[allow(dead_code)]
pub fn cfg(root: PathBuf) -> DepotConfig {
    DepotConfig {
        root,
        max_chain_id: 1024,
        file_size_threshold: 1 << 30, // 1 GiB; default for most tests
        eviction_dead_ratio: 0.5,
    }
}

/// Build a byte payload of the given length filled with `tag`'s bytes
/// cycled. Used so each test payload is identifiable and has predictable
/// content for byte-equality assertions.
#[allow(dead_code)]
pub fn payload(tag: &str, len: usize) -> Vec<u8> {
    let bytes = tag.as_bytes();
    if bytes.is_empty() {
        return vec![0u8; len];
    }
    (0..len).map(|i| bytes[i % bytes.len()]).collect()
}

/// All regular files immediately under `dir`, sorted by name.
#[allow(dead_code)]
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
