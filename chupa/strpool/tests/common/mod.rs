//! Shared helpers for integration tests. Avoids the `tempfile` dependency.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
pub fn scratch_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("strpool-test-{label}-{pid}-{nanos}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[allow(dead_code)]
pub fn shard_path(dir: &std::path::Path, id: u32) -> PathBuf {
    dir.join(format!("shard-{:04}", id))
}
