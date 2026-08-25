//! The materialization cache — DEPOT-DESIGN.md §7 ("hot", reframed):
//! not a depot variant but a persistent cache IN FRONT of all depots.
//!
//! Mount-serving consumers (mmap/exec, kernel read-passthrough's backing
//! fd, pread) need real loose files; those files are DERIVED —
//! reconstructible from whichever depot holds the layer — so this store
//! carries zero durability obligations: a crash is a cold cache, eviction
//! is space management, and the whole directory can be deleted and
//! rebuilt with nothing observable changing. Every operation here is
//! written to be safely repeatable for exactly that reason.
//!
//! Layout under `<root>/`:
//!
//! ```text
//! blob/aa/<hex>     immutable content files, named by sha256 (INTERNAL
//!                   naming only — the brief's "hashing works as a
//!                   filename scheme"; never part of any interface)
//! tree/<hex>/…      materialized layer VIEWS: real directories, files
//!                   hardlinked into blob/, symlinks recreated. Keyed by
//!                   the sha256 of the layer's canonical encoding —
//!                   derived, so also rebuildable.
//! ```
//!
//! Entries are immutable (0444) and NEVER opened writable: a write to
//! cached content is the box's D3 copy-up into its AUTHORITATIVE store,
//! not a cache mutation. Refcounting is `st_nlink`: a blob referenced by
//! no tree has nlink 1 and is reclaimable.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use depot::{codec, Attrs, BlobOp, Layer, Node, Presence};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The layer holds a node kind a materialized view cannot express
    /// (tombstones/holes are delta artifacts; a view has none).
    NotAView(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::NotAView(s) => write!(f, "not a materializable view: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub struct Cache {
    root: PathBuf,
}

fn hash_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl Cache {
    pub fn open(root: PathBuf) -> Result<Self, Error> {
        std::fs::create_dir_all(root.join("blob"))?;
        std::fs::create_dir_all(root.join("tree"))?;
        Ok(Cache { root })
    }

    fn blob_path(&self, hexname: &str) -> PathBuf {
        self.root.join("blob").join(&hexname[..2]).join(hexname)
    }

    /// Ensure `bytes` exist as an immutable pool file; return its path.
    /// Idempotent and dedup-by-content: the same bytes always land on
    /// the same file. tmp+rename, so a crash leaves either nothing or
    /// the finished (correct) file — and a half-written tmp is garbage
    /// a later call simply overwrites.
    pub fn file_for(&self, bytes: &[u8]) -> Result<PathBuf, Error> {
        let name = hash_hex(bytes);
        let path = self.blob_path(&name);
        if path.exists() {
            return Ok(path);
        }
        let dir = path.parent().expect("sharded dir");
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".tmp.{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))?;
        // rename over a concurrent winner is fine: identical content.
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// The tree key for a layer: sha256 of its canonical encoding —
    /// derived from the layer, like everything else here.
    pub fn tree_key(layer: &Layer) -> String {
        hash_hex(&codec::encode(layer))
    }

    /// Materialize a layer's VIEW as a real directory tree: directories
    /// created, file blobs HARDLINKED from the pool (mmap-able, deduped
    /// across every tree in the cache), symlinks recreated, exec bit
    /// honored from the `mode` attr. Idempotent: an existing complete
    /// tree is returned as-is; a partial one (crash) is rebuilt.
    ///
    /// The layer must be a full-content view (every file node `Set`, no
    /// tombstones — what export_layer / a packed-store read produces for
    /// a whole layer). Delta layers are compositions, not views; resolve
    /// first.
    pub fn materialize(&self, layer: &Layer) -> Result<PathBuf, Error> {
        let key = Self::tree_key(layer);
        let dst = self.root.join("tree").join(&key);
        let done = self.root.join("tree").join(format!("{key}.ok"));
        if done.exists() {
            return Ok(dst);
        }
        // Rebuild from scratch (idempotence over partial crashes).
        if dst.exists() {
            std::fs::remove_dir_all(&dst)?;
        }
        std::fs::create_dir_all(&dst)?;
        self.mat_node(&layer.root, &dst)?;
        std::fs::write(&done, b"")?;
        Ok(dst)
    }

    fn mat_node(&self, node: &Node, at: &Path) -> Result<(), Error> {
        for (name, child) in &node.children {
            if child.presence == Presence::Tombstone {
                return Err(Error::NotAView("tombstone"));
            }
            if child.anchor == depot::Anchor::Backdrop
                && matches!(child.blob, BlobOp::Keep)
                && child.attrs.is_none()
            {
                return Err(Error::NotAView("hole"));
            }
            let name_s = String::from_utf8_lossy(name).into_owned();
            let p = at.join(&name_s);
            let mode = attr_mode(&child.attrs);
            let ft = mode.map(|m| m & 0o170000);
            match (&child.blob, ft) {
                (BlobOp::Set(target), Some(0o120000)) => {
                    let t = std::ffi::OsString::from(
                        String::from_utf8_lossy(target).into_owned());
                    let _ = std::fs::remove_file(&p);
                    std::os::unix::fs::symlink(&t, &p)?;
                }
                (BlobOp::Set(bytes), _) => {
                    let pool = self.file_for(bytes)?;
                    let _ = std::fs::remove_file(&p);
                    if std::fs::hard_link(&pool, &p).is_err() {
                        // cross-device fallback (root split over mounts)
                        std::fs::copy(&pool, &p)?;
                    }
                    // exec bit rides on the DIRECTORY ENTRY via a copy?
                    // No: hardlinks share the inode, and pool files are
                    // 0444. An executable needs its own inode.
                    if let Some(m) = mode {
                        if m & 0o111 != 0
                            && std::fs::symlink_metadata(&p)?
                                .permissions().mode() & 0o111 == 0 {
                            {
                                // re-copy privately with the exec bit
                                std::fs::remove_file(&p)?;
                                std::fs::copy(&pool, &p)?;
                                std::fs::set_permissions(&p,
                                    std::fs::Permissions::from_mode(
                                        0o555))?;
                            }
                        }
                    }
                }
                (BlobOp::Keep, _) | (BlobOp::Remove, _) => {
                    // interior node: a directory
                    std::fs::create_dir_all(&p)?;
                    self.mat_node(child, &p)?;
                    continue;
                }
            }
            if !child.children.is_empty() {
                return Err(Error::NotAView("blob node with children \
                                            (fs views cannot express it)"));
            }
        }
        Ok(())
    }

    /// Drop a materialized tree (space management — the layer stays
    /// wherever its depot keeps it).
    pub fn drop_tree(&self, key: &str) -> Result<(), Error> {
        let _ = std::fs::remove_file(self.root.join("tree").join(format!("{key}.ok")));
        let dst = self.root.join("tree").join(key);
        if dst.exists() {
            std::fs::remove_dir_all(&dst)?;
        }
        Ok(())
    }

    /// Reclaim pool files no tree references (st_nlink == 1). Returns
    /// the number removed. Never data loss: everything is derived.
    pub fn evict_unreferenced(&self) -> Result<usize, Error> {
        let mut removed = 0;
        let blob_root = self.root.join("blob");
        for shard in std::fs::read_dir(&blob_root)?.flatten() {
            if !shard.path().is_dir() { continue; }
            for f in std::fs::read_dir(shard.path())?.flatten() {
                let md = match f.metadata() { Ok(m) => m, Err(_) => continue };
                use std::os::unix::fs::MetadataExt;
                if md.is_file() && md.nlink() == 1
                    && std::fs::remove_file(f.path()).is_ok()
                {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

fn attr_mode(attrs: &Option<Attrs>) -> Option<u32> {
    attrs.as_ref()
        .and_then(|a| a.get(&b"mode"[..]))
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
}
