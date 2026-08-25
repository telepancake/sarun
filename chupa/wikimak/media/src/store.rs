//! Per-mirror blob store (plan §4): materialized media keyed by
//! (file, width), served locally forever after.
//!
//! Layout: `cache_root/<x>/<xy>/<safe filename>/<bucket|"orig">`, where
//! `<x>/<xy>` is the same md5 shard as the upload URL (see [`crate::url`]).
//! Sharding keeps directory fan-out bounded; the filename dir groups all
//! widths of one file. A `404` sentinel file beside the blobs records a
//! negative fetch so offline renders don't re-miss (plan §4).
//!
//! Writes are atomic: bytes land in a uniquely-named `.tmp` sibling and
//! are `rename(2)`-d into place, so a reader never observes a partial or
//! zero-length blob (POSIX rename is atomic within a directory).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bucket::Bucket;
use crate::url::{normalize_filename, shard};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A blob store rooted at one directory. Cheap to clone the root path.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> BlobStore {
        BlobStore { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding every width (and the 404 sentinel) for one file.
    fn file_dir(&self, filename: &str) -> PathBuf {
        let f = normalize_filename(filename);
        let (x, xy) = shard(&f);
        self.root.join(x).join(xy).join(fs_safe(&f))
    }

    /// Absolute path a materialized blob would occupy (may not exist).
    pub fn blob_path(&self, filename: &str, bucket: Bucket) -> PathBuf {
        self.file_dir(filename).join(bucket.label())
    }

    /// Negative-cache sentinel path for a (file, bucket) 404.
    pub fn negative_path(&self, filename: &str, bucket: Bucket) -> PathBuf {
        // A `.404.<bucket>` sibling: distinct from any real blob name.
        self.file_dir(filename)
            .join(format!(".404.{}", bucket.label()))
    }

    /// Some(path) if the blob is present, else None.
    pub fn get(&self, filename: &str, bucket: Bucket) -> Option<PathBuf> {
        let p = self.blob_path(filename, bucket);
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    }

    /// Cheap presence check (stat, no read).
    pub fn exists(&self, filename: &str, bucket: Bucket) -> bool {
        self.blob_path(filename, bucket).is_file()
    }

    /// Byte length of a stored blob, or None if absent.
    pub fn stat_len(&self, filename: &str, bucket: Bucket) -> Option<u64> {
        fs::metadata(self.blob_path(filename, bucket))
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len())
    }

    pub fn has_negative(&self, filename: &str, bucket: Bucket) -> bool {
        self.negative_path(filename, bucket).is_file()
    }

    /// Record a 404 for (file, bucket) so future offline renders skip it.
    pub fn put_negative(&self, filename: &str, bucket: Bucket) -> io::Result<()> {
        let dir = self.file_dir(filename);
        fs::create_dir_all(&dir)?;
        atomic_write(&self.negative_path(filename, bucket), b"404")
    }

    /// Atomically store `bytes` as the blob for (file, bucket) and return
    /// its final path. Overwrites any prior blob (same rename target).
    pub fn put(&self, filename: &str, bucket: Bucket, bytes: &[u8]) -> io::Result<PathBuf> {
        let dir = self.file_dir(filename);
        fs::create_dir_all(&dir)?;
        let final_path = dir.join(bucket.label());
        atomic_write(&final_path, bytes)?;
        Ok(final_path)
    }
}

/// Replace a single filesystem char that would break the dir component.
/// Only `/` is illegal in a Linux path element (and NUL, which titles
/// cannot contain); `/` cannot legally appear in a File: name, but a
/// caller could hand us a raw string, so encode defensively.
fn fs_safe(name: &str) -> String {
    name.replace('/', "%2F")
}

/// Write `bytes` to `final_path` via a uniquely-named tmp sibling +
/// rename. The tmp name is unique per (pid, monotonic seq) so concurrent
/// writers never share a tmp file. Fsync the file before rename so the
/// content is durable ahead of the name.
fn atomic_write(final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = final_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "blob path has no parent dir")
    })?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".tmp.{}.{}.{}",
        std::process::id(),
        seq,
        final_path.file_name().and_then(|n| n.to_str()).unwrap_or("blob")
    );
    let tmp_path = dir.join(tmp_name);

    // Scope the handle so it is closed before the rename on all platforms.
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match fs::rename(&tmp_path, final_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Don't leak the tmp file on a failed rename.
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "wikimak-media-test-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn roundtrip_and_stat() {
        let root = tmp_root();
        let s = BlobStore::new(&root);
        assert!(!s.exists("Example.jpg", Bucket::Px(120)));
        assert_eq!(s.get("Example.jpg", Bucket::Px(120)), None);
        assert_eq!(s.stat_len("Example.jpg", Bucket::Px(120)), None);

        let path = s.put("Example.jpg", Bucket::Px(120), b"JPEGDATA").unwrap();
        assert!(path.is_file());
        assert!(s.exists("Example.jpg", Bucket::Px(120)));
        assert_eq!(s.stat_len("Example.jpg", Bucket::Px(120)), Some(8));
        assert_eq!(fs::read(&path).unwrap(), b"JPEGDATA");
        assert_eq!(s.get("Example.jpg", Bucket::Px(120)).unwrap(), path);

        // A different bucket of the same file is a distinct blob.
        assert!(!s.exists("Example.jpg", Bucket::Orig));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn path_uses_md5_shard() {
        let root = tmp_root();
        let s = BlobStore::new(&root);
        let p = s.blob_path("Example.jpg", Bucket::Orig);
        // /a/a9/Example.jpg/orig — same shard as the upload URL.
        let rel = p.strip_prefix(&root).unwrap();
        assert_eq!(
            rel,
            Path::new("a/a9/Example.jpg/orig"),
            "blob path must shard on md5 like the CDN"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn negative_cache() {
        let root = tmp_root();
        let s = BlobStore::new(&root);
        assert!(!s.has_negative("Nope.png", Bucket::Px(60)));
        s.put_negative("Nope.png", Bucket::Px(60)).unwrap();
        assert!(s.has_negative("Nope.png", Bucket::Px(60)));
        // Sentinel is per-bucket; another width is still unknown.
        assert!(!s.has_negative("Nope.png", Bucket::Px(120)));
        // A sentinel is NOT a blob.
        assert!(!s.exists("Nope.png", Bucket::Px(60)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_tmp_file_survives_a_successful_write() {
        // Atomicity guarantee: after put(), the file dir contains exactly
        // the blob — no `.tmp.*` sibling ever lingers.
        let root = tmp_root();
        let s = BlobStore::new(&root);
        s.put("Example.jpg", Bucket::Px(250), b"data").unwrap();
        let dir = s.blob_path("Example.jpg", Bucket::Px(250));
        let dir = dir.parent().unwrap();
        let entries: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["250".to_string()]);
        assert!(
            !entries.iter().any(|n| n.starts_with(".tmp.")),
            "a tmp file must never remain visible"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn overwrite_replaces_bytes() {
        let root = tmp_root();
        let s = BlobStore::new(&root);
        s.put("Example.jpg", Bucket::Orig, b"v1").unwrap();
        let p = s.put("Example.jpg", Bucket::Orig, b"version-two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"version-two");
        fs::remove_dir_all(&root).ok();
    }
}
