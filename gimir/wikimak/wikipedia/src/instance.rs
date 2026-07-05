//! `Instance` — the per-dbname Wikipedia mirror.
//!
//! Public API per SPEC §"API (sketch)".

use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use strpool::{Pool, PoolConfig};
use wikimak_depot::{Depot, DepotConfig};
use wikimak_mediawiki::PageStream;

use crate::error::{Error, Result};
use crate::import::do_import;
use crate::revision::decode_revision;
use crate::schema::META_DDL;

/// Configuration for opening an [`Instance`].
///
/// `root` is the per-dbname directory: e.g.
/// `<gimir-cache>/wikimak/<dbname>/`. The depot, titles pool, and
/// `meta.db` all live under this root.
pub struct InstanceConfig {
    /// `<gimir-cache>/wikimak/<dbname>/`. Created if missing.
    pub root: PathBuf,
    /// Wiki database name, e.g. `"enwiki"`, `"votewiki"`.
    pub dbname: String,
    /// Maximum supported page id. Sizes the depot's index (`max_chain_id * 8`
    /// bytes). For votewiki/cswiki ≪ 1M; for enwiki ≈ 100M.
    pub max_chain_id: u64,
    /// Depot tuning. The implementer can pass this through to
    /// [`DepotConfig`] — `root` is forced to `<root>/depot/`. Tests
    /// supply a small `file_size_threshold` to drive eviction.
    pub depot: DepotConfig,
    /// Strpool shard count for the titles pool. Tests use 1.
    pub title_shard_count: u32,
    /// Strpool seal threshold for the titles pool.
    pub title_seal_threshold_bytes: u64,
    /// f1 accumulator seal threshold, in DECOMPRESSED bytes: when
    /// absorbing the spilled head would push the accumulator past this,
    /// the old f1's zstd bytes move verbatim into a cold frame and a
    /// fresh accumulator starts. 0 = use the default (256 KiB). Sizing
    /// against the real corpus is an open tuning question (tiered-VBF
    /// doc §9); the default renders the design without pretending to be
    /// measured.
    pub f1_seal_threshold_bytes: u64,
}

/// Per-revision metadata decoded from a depot frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionMeta {
    pub rev_id: u64,
    pub parent_id: u64,
    pub ts: DateTime<Utc>,
    pub contributor: ContributorMeta,
    pub comment: String,
    pub sha1: String,
    pub flags: u32,
    pub text_len: u64,
}

/// Contributor variant carried in [`RevisionMeta`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContributorMeta {
    Anonymous { ip: String },
    Named { username: String, user_id: u64 },
    Hidden,
}

/// Counters returned from [`Instance::import`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub pages: u64,
    pub revisions_new: u64,
    pub revisions_deduped: u64,
    pub sha1_ok: u64,
    pub sha1_fudged: u64,
    pub sha1_mismatch: u64,
}

/// One entry in a [`HistoryIter`]: metadata + a one-shot lazy text
/// fetcher.
pub struct HistoryEntry {
    pub meta: RevisionMeta,
    pub fetch_text: Box<dyn FnOnce() -> Result<Vec<u8>> + Send>,
}

/// Iterator over a page's revisions, newest-first. Per SPEC §API.
pub struct HistoryIter {
    pub(crate) inner: Box<dyn Iterator<Item = Result<HistoryEntry>> + Send>,
}

impl Iterator for HistoryIter {
    type Item = Result<HistoryEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// The per-dbname mirror. One process at a time per `root`.
pub struct Instance {
    pub(crate) inner: Mutex<InstanceInner>,
    pub(crate) max_chain_id: u64,
    pub(crate) f1_seal_threshold_bytes: u64,
    pub(crate) title_shard_count: u32,
    #[allow(dead_code)]
    // dbname retained for future logging / sharding decisions; unread today.
    pub(crate) dbname: String,
}

/// All the I/O handles owned by an `Instance`. Held behind a single
/// `Mutex` so that import / read paths serialize at this boundary —
/// keeps the per-page atomicity story simple.
pub(crate) struct InstanceInner {
    pub(crate) depot: Depot,
    pub(crate) titles: Pool,
    pub(crate) conn: Connection,
}

impl Instance {
    /// Open or create the instance at `cfg.root`. Creates `depot/`,
    /// `titles/`, and `meta.db` if absent. Re-open is idempotent.
    pub fn open(cfg: InstanceConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;

        // Depot — root forced to <root>/depot/ per SPEC.
        let mut depot_cfg = cfg.depot;
        depot_cfg.root = cfg.root.join("depot");
        std::fs::create_dir_all(&depot_cfg.root)?;
        let depot = Depot::open(depot_cfg)?;

        // Title pool — <root>/titles/.
        let titles_dir = cfg.root.join("titles");
        let titles = Pool::open(
            &titles_dir,
            PoolConfig {
                shard_count: cfg.title_shard_count,
                seal_threshold_bytes: cfg.title_seal_threshold_bytes,
            },
            None,
        )?;

        // meta.db.
        let conn = Connection::open(cfg.root.join("meta.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        for stmt in META_DDL {
            conn.execute(stmt, [])?;
        }

        Ok(Self {
            inner: Mutex::new(InstanceInner {
                depot,
                titles,
                conn,
            }),
            max_chain_id: cfg.max_chain_id,
            f1_seal_threshold_bytes: if cfg.f1_seal_threshold_bytes == 0 {
                256 * 1024
            } else {
                cfg.f1_seal_threshold_bytes
            },
            title_shard_count: cfg.title_shard_count,
            dbname: cfg.dbname,
        })
    }

    /// Import one `PageStream` into the instance. Per-page atomic.
    /// Returns counters describing the import.
    pub fn import<R: Read>(&self, stream: &mut PageStream<R>) -> Result<ImportStats> {
        do_import(self, stream)
    }

    /// Read the current head revision metadata for `page_id`.
    pub fn page_head(&self, page_id: u64) -> Result<Option<RevisionMeta>> {
        if page_id >= self.max_chain_id {
            return Ok(None);
        }
        let g = self.inner.lock().expect("instance mutex poisoned");
        match g.depot.read_f0(page_id) {
            Ok(frame) => {
                let bytes = crate::frames::decompress(&frame, None)?;
                let (meta, _text) = decode_revision(&bytes)?;
                Ok(Some(meta))
            }
            Err(wikimak_depot::Error::NoFrame) => Ok(None),
            Err(wikimak_depot::Error::ChainIdOutOfRange) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Read the current head revision's text bytes (UTF-8) for
    /// `page_id`. `Ok(None)` if no such page.
    pub fn page_head_text(&self, page_id: u64) -> Result<Option<Vec<u8>>> {
        if page_id >= self.max_chain_id {
            return Ok(None);
        }
        let g = self.inner.lock().expect("instance mutex poisoned");
        match g.depot.read_f0(page_id) {
            Ok(frame) => {
                let bytes = crate::frames::decompress(&frame, None)?;
                let (_meta, text) = decode_revision(&bytes)?;
                Ok(Some(text))
            }
            Err(wikimak_depot::Error::NoFrame) => Ok(None),
            Err(wikimak_depot::Error::ChainIdOutOfRange) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Iterate all revisions of `page_id`, newest-first.
    pub fn page_history(&self, page_id: u64) -> Result<HistoryIter> {
        // Walk the chain eagerly here (under the lock) to collect each
        // record's encoded bytes; the lazy contract is satisfied by
        // deferring the *decode* of text bytes into the fetch_text
        // closure (the records themselves are small; text dominates).
        let records: Vec<Vec<u8>> = if page_id >= self.max_chain_id {
            Vec::new()
        } else {
            let g = self.inner.lock().expect("instance mutex poisoned");
            collect_records(&g.depot, page_id)?
        };

        let iter = records.into_iter().map(|rec| {
            let (meta, _text) = decode_revision(&rec)?;
            // Clone of rec moves into the closure for lazy text decode.
            let rec_for_text = rec;
            let fetch_text: Box<dyn FnOnce() -> Result<Vec<u8>> + Send> = Box::new(move || {
                let (_m, t) = decode_revision(&rec_for_text)?;
                Ok(t)
            });
            Ok(HistoryEntry { meta, fetch_text })
        });

        Ok(HistoryIter {
            inner: Box::new(iter),
        })
    }

    /// Flush depot + strpool + sqlite to durable storage.
    pub fn flush(&self) -> Result<()> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        g.depot.flush()?;
        for sid in 0..self.title_shard_count {
            g.titles.flush(sid)?;
        }
        // sqlite WAL checkpoint — commit boundaries flushed by the
        // per-page transactions; the checkpoint pushes WAL pages to the
        // main db file.
        g.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .map_err(Error::Sqlite)?;
        Ok(())
    }
}

/// Collect every revision record on `chain_id`, newest-first. Walks the
/// chain the way it was encoded (depot SPEC "The shape of a chain"):
///   - f0 = newest record, standalone zstd;
///   - f1 = older records concatenated newest-first, refPrefix-anchored
///     on f0's record;
///   - each cold frame is a sealed former accumulator, anchored on the
///     OLDEST record of the next-newer frame — which is exactly the
///     last record decoded so far in this newest-first walk.
pub(crate) fn collect_records(depot: &Depot, chain_id: u64) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    match depot.read_f0(chain_id) {
        Ok(frame) => out.push(crate::frames::decompress(&frame, None)?),
        Err(wikimak_depot::Error::NoFrame) => return Ok(out),
        Err(wikimak_depot::Error::ChainIdOutOfRange) => return Ok(out),
        Err(e) => return Err(e.into()),
    }
    if let Some(f1) = depot.read_f1(chain_id)? {
        let anchor = out[0].clone();
        let raw = crate::frames::decompress(&f1, Some(&anchor))?;
        split_concatenated_records(&raw, &mut out)?;
    }
    for cold in depot.cold_iter(chain_id)? {
        let frame = cold?;
        let anchor = out.last().expect("cold after f1").clone();
        let raw = crate::frames::decompress(&frame, Some(&anchor))?;
        split_concatenated_records(&raw, &mut out)?;
    }
    Ok(out)
}

/// Walk `buf` as zero or more revision records back-to-back; push each
/// into `out`. Uses the codec's prefix sizes (fixed u32+u32+u64+u64+u64
/// +u64+u8 = 41 bytes, then 4 varint-prefixed blobs) to compute the
/// length of each record without copying.
fn split_concatenated_records(buf: &[u8], out: &mut Vec<Vec<u8>>) -> Result<()> {
    let mut i = 0;
    while i < buf.len() {
        let start = i;
        // Skip fixed prefix: 4 + 4 + 8 + 8 + 8 + 8 + 1 = 41.
        const FIXED: usize = 4 + 4 + 8 + 8 + 8 + 8 + 1;
        if i + FIXED > buf.len() {
            return Err(Error::Codec("truncated record fixed prefix"));
        }
        i += FIXED;
        // Four length-prefixed byte fields (contributor, comment, sha1, text).
        for _ in 0..4 {
            let (len, n) = crate::revision::decode_varint(buf, i)?;
            i += n;
            let len = len as usize;
            if i + len > buf.len() {
                return Err(Error::Codec("truncated record payload"));
            }
            i += len;
        }
        out.push(buf[start..i].to_vec());
    }
    Ok(())
}
