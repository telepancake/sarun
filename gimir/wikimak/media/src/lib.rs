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
//! OWNED BY: the media agent. Skeleton: offline resolver that reports
//! misses (placeholder boxes) and the URL math contract.

use std::path::PathBuf;

use wikimak_wikitext::{MediaResolver, Title};

/// Blob-store-backed resolver. `fetch` feature off → cache-only:
/// a miss renders a placeholder (never a broken external <img>).
pub struct BlobMediaResolver {
    pub cache_root: PathBuf,
}

impl MediaResolver for BlobMediaResolver {
    fn image_url(&self, _file: &Title, _width_px: Option<u32>) -> Option<String> {
        None // offline placeholder until the media agent lands the store
    }
}
