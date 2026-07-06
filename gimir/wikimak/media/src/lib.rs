//! Media pipeline (plan §4): no bulk media dumps exist — media is
//! lazily materialized into a per-mirror blob store keyed by
//! (file, width), served locally forever after. URL scheme (VERIFIED):
//! `upload.wikimedia.org/<project>/<x>/<xy>/<Filename>` with x/xy from
//! md5(filename_with_underscores); thumbs at
//! `/thumb/<x>/<xy>/<F>/<NNN>px-<F>`. Standard buckets
//! 20/40/60/120/250/330/500/960 px. Robot policy (codified, not vibes):
//! ≤2 concurrent connections, ≤25 Mbps, gzip, honor 429 Retry-After,
//! 15 min pause after 5xx, descriptive User-Agent.
//!
//! Three pieces, dependency-ordered:
//!   - [`url`] — the CDN URL math (pure);
//!   - [`bucket`] — width snapping (pure);
//!   - [`store::BlobStore`] — atomic on-disk blob cache;
//! composed by [`MediaStore::materialize`], which turns (file, width)
//! into a local path — cache hit, offline miss, or (feature `fetch`) a
//! Robot-policy download through the repo chain. [`BlobMediaResolver`]
//! is the render-time [`MediaResolver`]: it always returns a *local*
//! route string; the serve layer streams that route from the store or
//! substitutes a placeholder.

pub mod bucket;
pub mod store;
pub mod url;

#[cfg(feature = "fetch")]
pub mod fetch;

use std::path::PathBuf;

use wikimak_wikitext::{MediaResolver, Title};

pub use bucket::{snap, Bucket, BUCKETS};
pub use store::BlobStore;

/// What can go wrong turning a (file, width) into a local path.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// Cache miss and fetching is not available (feature off, or the
    /// store is running offline). The renderer shows a placeholder.
    #[error("media offline: {0} not cached and fetch disabled")]
    Offline(String),
    /// The file does not exist in any repo in the chain (404),
    /// negative-cached so offline renders don't re-miss.
    #[error("media not found: {0}")]
    NotFound(String),
    /// The upload host is in Robot-policy cooldown (429/5xx back-off).
    #[error("media host backing off: {0}")]
    Backoff(String),
    /// Blob-store I/O failure.
    #[error("media store io: {0}")]
    Io(#[from] std::io::Error),
    /// Transport error talking to a repo (feature `fetch`).
    #[error("media fetch failed: {0}")]
    Fetch(String),
}

/// One media repository in the chain: a name plus its upload base, e.g.
/// `https://upload.wikimedia.org/wikipedia/commons` or
/// `.../wikipedia/en`.
#[derive(Debug, Clone)]
pub struct Repo {
    pub name: String,
    /// Base under which `<x>/<xy>/<F>` and `/thumb/...` live.
    pub upload_base: String,
}

impl Repo {
    pub fn new(name: impl Into<String>, upload_base: impl Into<String>) -> Repo {
        Repo {
            name: name.into(),
            upload_base: upload_base.into(),
        }
    }

    /// Wikimedia Commons, the shared upload repository.
    pub fn commons() -> Repo {
        Repo::new("commons", "https://upload.wikimedia.org/wikipedia/commons")
    }

    #[cfg(feature = "fetch")]
    fn url_for(&self, filename: &str, bucket: Bucket) -> String {
        match bucket {
            Bucket::Orig => url::original_url(&self.upload_base, filename),
            Bucket::Px(w) => url::thumb_url(&self.upload_base, filename, w),
        }
    }
}

/// The lazy media store: a blob cache plus a repo chain (local first,
/// then commons — MediaWiki's repo-chain model). `materialize` resolves
/// (file, width) to a local path.
pub struct MediaStore {
    store: BlobStore,
    /// Repos in priority order; the first that has the file wins.
    repos: Vec<Repo>,
    #[cfg(feature = "fetch")]
    robot: fetch::Robot,
    /// Runtime fetch switch (feature `fetch` only): a prefetch command
    /// turns it on, offline serving leaves it off even when compiled in.
    #[cfg(feature = "fetch")]
    allow_fetch: bool,
}

impl MediaStore {
    /// A store rooted at `cache_root` with an explicit repo chain.
    pub fn new(cache_root: impl Into<PathBuf>, repos: Vec<Repo>) -> MediaStore {
        MediaStore {
            store: BlobStore::new(cache_root),
            repos,
            #[cfg(feature = "fetch")]
            robot: fetch::Robot::new(),
            #[cfg(feature = "fetch")]
            allow_fetch: true,
        }
    }

    /// Convenience: a single-repo store pointed at Commons.
    pub fn commons(cache_root: impl Into<PathBuf>) -> MediaStore {
        MediaStore::new(cache_root, vec![Repo::commons()])
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.store
    }

    pub fn repos(&self) -> &[Repo] {
        &self.repos
    }

    /// Enable/disable network fetch at runtime (feature `fetch`). With it
    /// off, misses are [`MediaError::Offline`] just like a non-`fetch`
    /// build — the offline-serving path.
    #[cfg(feature = "fetch")]
    pub fn set_allow_fetch(&mut self, on: bool) {
        self.allow_fetch = on;
    }

    /// Resolve (file, width) to a local blob path.
    ///
    /// Order: cache hit → path; negative-cached 404 → `NotFound`; else a
    /// miss — `Offline` without fetch, or a Robot-policy download through
    /// the repo chain with fetch.
    pub fn materialize(&self, file: &str, width: Option<u32>) -> Result<PathBuf, MediaError> {
        let bucket = snap(width);
        if let Some(path) = self.store.get(file, bucket) {
            return Ok(path);
        }
        if self.store.has_negative(file, bucket) {
            return Err(MediaError::NotFound(file.to_string()));
        }
        self.on_miss(file, bucket)
    }

    #[cfg(not(feature = "fetch"))]
    fn on_miss(&self, file: &str, _bucket: Bucket) -> Result<PathBuf, MediaError> {
        Err(MediaError::Offline(file.to_string()))
    }

    /// Fetch a miss through the repo chain under the Robot policy, store
    /// the bytes, and return the path. A 404 in every repo is negative-
    /// cached and reported as `NotFound`; a back-off short-circuits.
    #[cfg(feature = "fetch")]
    fn on_miss(&self, file: &str, bucket: Bucket) -> Result<PathBuf, MediaError> {
        use fetch::FetchError;
        if !self.allow_fetch {
            return Err(MediaError::Offline(file.to_string()));
        }
        // Only a clean fall-through (every repo answered 404) reaches the
        // bottom; Backoff/Network return early, so no "any transient" flag
        // is needed.
        for repo in &self.repos {
            let target = repo.url_for(file, bucket);
            match self.robot.fetch(&target) {
                Ok(bytes) => {
                    let path = self.store.put(file, bucket, &bytes)?;
                    return Ok(path);
                }
                Err(FetchError::NotFound) => continue,
                Err(FetchError::Backoff(rem)) => {
                    return Err(MediaError::Backoff(format!(
                        "{file}: {}s remaining",
                        rem.as_secs()
                    )));
                }
                Err(FetchError::Network(msg)) => {
                    return Err(MediaError::Fetch(format!("{file}: {msg}")));
                }
            }
        }
        self.store.put_negative(file, bucket)?;
        Err(MediaError::NotFound(file.to_string()))
    }
}

/// Render-time [`MediaResolver`] (plan §3 dependency inversion): it does
/// NOT touch the network or the disk — it hands the parser a *local*
/// route the serve layer resolves later. Returning `Some` ALWAYS (never
/// a miss here) is deliberate: the serve layer decides per-request
/// whether to stream the blob or emit a placeholder, so the rendered
/// HTML is stable regardless of what's cached.
///
/// Route shape: `<route_prefix><percent-encoded file>?w=<bucket>`, e.g.
/// `/wiki/enwiki/media/Example.jpg?w=250`.
pub struct BlobMediaResolver {
    /// Prepended to every route, e.g. `/wiki/enwiki/media/`.
    pub route_prefix: String,
}

impl BlobMediaResolver {
    pub fn new(route_prefix: impl Into<String>) -> BlobMediaResolver {
        BlobMediaResolver {
            route_prefix: route_prefix.into(),
        }
    }

    /// Build the local route for a file at a requested width.
    pub fn route(&self, file: &Title, width_px: Option<u32>) -> String {
        let bucket = snap(width_px);
        format!(
            "{}{}?w={}",
            self.route_prefix,
            percent_encode(&file.text),
            bucket.label()
        )
    }
}

impl MediaResolver for BlobMediaResolver {
    fn image_url(&self, file: &Title, width_px: Option<u32>) -> Option<String> {
        Some(self.route(file, width_px))
    }
}

/// Percent-encode a URL path segment: keep RFC 3986 unreserved bytes
/// (`A-Za-z0-9-._~`), map spaces to underscores first (title form),
/// everything else → `%XX`. Enough for File: names in a URL path.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.replace(' ', "_").as_bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0xf));
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    char::from_digit(nibble as u32, 16).unwrap().to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_root() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "wikimak-media-lib-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn file_title(name: &str) -> Title {
        // ns 6 = File:; the resolver only reads `.text`.
        Title {
            ns: 6,
            text: name.to_string(),
        }
    }

    #[test]
    fn resolver_always_returns_local_route() {
        let r = BlobMediaResolver::new("/wiki/enwiki/media/");
        // Non-standard width snaps to a bucket in the route.
        let got = r.image_url(&file_title("Example.jpg"), Some(200)).unwrap();
        assert_eq!(got, "/wiki/enwiki/media/Example.jpg?w=250");
        // None → orig.
        let orig = r.image_url(&file_title("Example.jpg"), None).unwrap();
        assert_eq!(orig, "/wiki/enwiki/media/Example.jpg?w=orig");
    }

    #[test]
    fn resolver_percent_encodes_and_underscores() {
        let r = BlobMediaResolver::new("/m/");
        let got = r
            .image_url(&file_title("Foo bar & baz.png"), Some(120))
            .unwrap();
        // Space→_, '&' encoded (%26), spaces around it also underscored.
        assert_eq!(got, "/m/Foo_bar_%26_baz.png?w=120");
    }

    #[test]
    fn cache_hit_returns_stored_path() {
        let root = tmp_root();
        let ms = MediaStore::commons(&root);
        // Pre-seed the blob as if a prior fetch materialized it.
        let seeded = ms
            .blobs()
            .put("Example.jpg", Bucket::Px(250), b"PNGDATA")
            .unwrap();
        // A 200-width request snaps to the 250 bucket → cache hit.
        let got = ms.materialize("Example.jpg", Some(200)).unwrap();
        assert_eq!(got, seeded);
        assert_eq!(std::fs::read(&got).unwrap(), b"PNGDATA");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn negative_cache_reports_not_found() {
        let root = tmp_root();
        let ms = MediaStore::commons(&root);
        ms.blobs()
            .put_negative("Ghost.png", Bucket::Px(120))
            .unwrap();
        let err = ms.materialize("Ghost.png", Some(100)).unwrap_err();
        assert!(matches!(err, MediaError::NotFound(_)), "got {err:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(not(feature = "fetch"))]
    #[test]
    fn miss_without_fetch_is_offline() {
        let root = tmp_root();
        let ms = MediaStore::commons(&root);
        let err = ms.materialize("Never.jpg", Some(120)).unwrap_err();
        assert!(matches!(err, MediaError::Offline(_)), "got {err:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(feature = "fetch")]
    #[test]
    fn miss_with_fetch_disabled_is_offline() {
        let root = tmp_root();
        let mut ms = MediaStore::commons(&root);
        ms.set_allow_fetch(false);
        let err = ms.materialize("Never.jpg", Some(120)).unwrap_err();
        assert!(matches!(err, MediaError::Offline(_)), "got {err:?}");
        std::fs::remove_dir_all(&root).ok();
    }
}
