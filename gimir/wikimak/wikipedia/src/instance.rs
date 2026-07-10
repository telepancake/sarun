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

/// Default `max_chain_id` for fresh instances: sized for enwiki
/// (~80M page ids in 2026) with headroom. The cost is the depot's
/// index file at 8 bytes/chain — 800MB LOGICAL, but the index is
/// created with `ftruncate` and stays sparse: untouched chains never
/// allocate a disk block (pinned by the depot's sparse-index test).
pub const DEFAULT_MAX_CHAIN_ID: u64 = 100_000_000;

/// The `max_chain_id` an EXISTING instance root was created with —
/// derived from the on-disk depot index (`max_chain_id * 8` bytes),
/// so read-side opens never guess. A fresh root (no index yet) gets
/// [`DEFAULT_MAX_CHAIN_ID`]. A mismatched explicit config still fails
/// loudly in the depot (`IndexSizeMismatch`); this helper is how
/// callers avoid manufacturing that mismatch.
pub fn max_chain_id_for_root(root: &std::path::Path) -> u64 {
    std::fs::metadata(root.join("depot").join("index"))
        .map(|m| m.len() / 8)
        .ok()
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_CHAIN_ID)
}

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

/// [`Instance::page_by_title`]'s answer: the resolved page id (if exact
/// or unique) and the candidate `(page_id, title)` matches.
pub type TitleResolution = (Option<u64>, Vec<(u64, String)>);

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
    /// True when the previous session ended dirty (crash between an
    /// import write and a flush): `revisions_seen` may then be AHEAD of
    /// the depot (rows durable, frames lost). Imports repair each
    /// touched page's rows from the chain before trusting them.
    pub(crate) suspect: bool,
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
    /// Pages whose `revisions_seen` rows were re-derived from the chain
    /// this session (suspect-mode repair) — each repaired once.
    pub(crate) repaired: std::collections::HashSet<u64>,
    /// Whether this session has already stamped the dirty flag.
    pub(crate) dirty_stamped: bool,
    /// The root's flock, held for the instance's lifetime.
    pub(crate) _lock: std::fs::File,
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

        // One-process-per-root guard: an exclusive flock on <root>/.lock,
        // held for the Instance's lifetime and auto-released by the
        // kernel on any exit (even a crash). External READERS of
        // meta.db stay possible — only a second writing instance is
        // locked out (it would interleave depot prepends unsynchronized).
        let lock = acquire_root_lock(&cfg.root)?;

        // meta.db.
        let conn = Connection::open(cfg.root.join("meta.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        for stmt in META_DDL {
            conn.execute(stmt, [])?;
        }
        let suspect: bool = conn
            .query_row(
                "SELECT value FROM instance_flags WHERE key = 'dirty'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .unwrap_or(false);

        Ok(Self {
            inner: Mutex::new(InstanceInner {
                depot,
                titles,
                conn,
                repaired: Default::default(),
                dirty_stamped: false,
                _lock: lock,
            }),
            suspect,
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

    /// Read the current head revision metadata for `page_id` — the
    /// newest revision by timestamp.
    ///
    /// NOT the depot chain's f0 frame: f0 is the most-recently-*imported*
    /// record, which is only the newest-by-time when revisions were
    /// appended in chronological order. Out-of-order / cross-import
    /// prepends (a later import supplying a gap revision) make f0 an older
    /// revision, so the "current head" is computed as `argmax(ts)` over
    /// the whole chain (via [`Instance::revision_at`] with `None` τ).
    pub fn page_head(&self, page_id: u64) -> Result<Option<RevisionMeta>> {
        self.revision_at(page_id, None)
    }

    /// Read the current head revision's text bytes (UTF-8) for
    /// `page_id` — the newest revision by timestamp (see [`page_head`]).
    /// `Ok(None)` if no such page.
    pub fn page_head_text(&self, page_id: u64) -> Result<Option<Vec<u8>>> {
        self.page_text_at(page_id, None)
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

    /// List `(page_id, title)` pairs, title-ordered, optionally filtered
    /// by a case-insensitive substring. The answer to "which pages do I
    /// have?" — ids alone are not a UI.
    pub fn pages(&self, filter: Option<&str>, limit: usize)
        -> Result<Vec<(u64, String)>>
    {
        let g = self.inner.lock().expect("instance mutex poisoned");
        // Open intervals only: a page renamed away keeps its old title as a
        // closed interval, which must not surface as a current page.
        let mut st = g.conn.prepare(
            "SELECT page_id, normalized_title FROM title_intervals
             WHERE end_ts IS NULL
             ORDER BY normalized_title")?;
        let rows = st.query_map([], |r| Ok((
            r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?)))?;
        let needle = filter.map(str::to_lowercase);
        let mut out = Vec::new();
        for row in rows.flatten() {
            let title = String::from_utf8_lossy(&row.1).into_owned();
            if let Some(n) = &needle {
                if !title.to_lowercase().contains(n) {
                    continue;
                }
            }
            out.push((row.0, title));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Resolve a page by exact title, else by unique case-insensitive
    /// substring. `Err(TitleAmbiguous)`-free by design: ambiguity comes
    /// back as `Ok(None)` plus the candidates for the caller to show.
    pub fn page_by_title(&self, title: &str) -> Result<TitleResolution> {
        let all = self.pages(Some(title), 16)?;
        if let Some(hit) = all.iter().find(|(_, t)| t == title) {
            return Ok((Some(hit.0), all));
        }
        match all.as_slice() {
            [(id, _)] => Ok((Some(*id), all)),
            _ => Ok((None, all)),
        }
    }

    // --- asof-τ read API (browsing plan §2, the wayback contract) ---
    //
    // Title normalization here MUST match import's (`ensure_title` in
    // import.rs): the importer stores `page.title.trim()` verbatim as the
    // `normalized_title` BLOB — namespace prefix kept, underscores NOT
    // folded to spaces, no per-namespace case rule applied. So the τ
    // lookups below normalize an incoming title with `.trim()` only.
    // Fuller normalization (underscores→spaces, first-letter case from
    // siteinfo) is a documented gap: it belongs at import time (import
    // plan §7 amendment) and cannot be added at read time without
    // re-keying the stored titles.

    /// Resolve a title to its page id AS OF `ts_micros` (unix micros).
    ///
    /// `None` τ → current behavior ([`Instance::page_by_title`], exact
    /// then unique-substring). `Some(τ)` → `title_intervals` window
    /// lookup on the normalized (trimmed) title:
    /// `start_ts <= τ AND (end_ts IS NULL OR end_ts > τ)`. When NO
    /// interval rows exist for the title at all (an old import that
    /// predates interval bookkeeping), fall back to the current
    /// title→page mapping. A title that HAS interval rows but none
    /// covering τ resolves to `None` — it did not exist at τ.
    pub fn page_id_by_title_at(&self, title: &str, ts_micros: Option<i64>) -> Result<Option<u64>> {
        let ts = match ts_micros {
            None => return Ok(self.page_by_title(title)?.0),
            Some(ts) => ts,
        };
        let key = title.trim().as_bytes().to_vec();
        let g = self.inner.lock().expect("instance mutex poisoned");
        let hit: Option<i64> = g
            .conn
            .query_row(
                "SELECT page_id FROM title_intervals
                 WHERE normalized_title = ?1
                   AND start_ts <= ?2
                   AND (end_ts IS NULL OR end_ts > ?2)
                 ORDER BY start_ts DESC LIMIT 1",
                rusqlite::params![key, ts],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = hit {
            return Ok(Some(id as u64));
        }
        // Distinguish "title has interval rows, none cover τ" (→ None,
        // did not exist at τ) from "no interval rows at all" (→ fall back
        // to the current mapping, for pre-interval imports).
        let any_interval: i64 = g.conn.query_row(
            "SELECT COUNT(*) FROM title_intervals WHERE normalized_title = ?1",
            rusqlite::params![key],
            |r| r.get(0),
        )?;
        if any_interval > 0 {
            return Ok(None);
        }
        let current: Option<i64> = g
            .conn
            .query_row(
                "SELECT p.page_id FROM page_to_title_id p
                 JOIN title_id_to_page t ON t.title_id = p.title_id
                 WHERE t.normalized_title = ?1
                 LIMIT 1",
                rusqlite::params![key],
                |r| r.get(0),
            )
            .ok();
        // Fall back to the untimed mapping ONLY for a genuinely pre-interval
        // page (no title_intervals rows at all). If the resolved page IS
        // interval-tracked but none of its intervals carry this title, the
        // title was retitled away by a rename — it never covered τ, so →
        // None rather than the all-τ resolution that would report the page
        // before it existed (adversarial-review leak: a renamed-away title
        // resolving at every τ). The page stays reachable under its current
        // title's interval and, for τ = None, under `page_by_title`.
        if let Some(pid) = current {
            let tracked: i64 = g.conn.query_row(
                "SELECT COUNT(*) FROM title_intervals WHERE page_id = ?1",
                rusqlite::params![pid],
                |r| r.get(0),
            )?;
            if tracked > 0 {
                return Ok(None);
            }
        }
        Ok(current.map(|id| id as u64))
    }

    /// Newest revision of `page_id` with timestamp ≤ `ts_micros`.
    ///
    /// `None` τ → the newest revision overall. `Some(τ)` → the newest
    /// revision whose timestamp is ≤ τ; `Ok(None)` when every revision is
    /// newer than τ (the page did not yet exist at τ).
    ///
    /// The chain is walked in full and the answer chosen by `argmax` over
    /// `(timestamp, rev_id)` — NOT the first record in chain order. Chain
    /// order is import-prepend order, not timestamp order: an out-of-order
    /// or cross-import gap revision (a later import supplying an earlier
    /// revision) lands at the chain head, so "first with ts ≤ τ" would
    /// return a non-newest revision. `argmax` is correct regardless of the
    /// order revisions were imported in.
    pub fn revision_at(&self, page_id: u64, ts_micros: Option<i64>) -> Result<Option<RevisionMeta>> {
        let mut best: Option<RevisionMeta> = None;
        for entry in self.page_history(page_id)? {
            let meta = entry?.meta;
            if let Some(ts) = ts_micros {
                if meta.ts.timestamp_micros() > ts {
                    continue;
                }
            }
            if best.as_ref().map_or(true, |b| rev_key(&meta) > rev_key(b)) {
                best = Some(meta);
            }
        }
        Ok(best)
    }

    /// Text bytes of the revision selected by [`Instance::revision_at`].
    ///
    /// Same `argmax` selection; decodes only the chosen revision's text.
    /// `None` τ → newest-revision text; `Ok(None)` when no revision is ≤ τ.
    pub fn page_text_at(&self, page_id: u64, ts_micros: Option<i64>) -> Result<Option<Vec<u8>>> {
        let mut best: Option<HistoryEntry> = None;
        for entry in self.page_history(page_id)? {
            let entry = entry?;
            if let Some(ts) = ts_micros {
                if entry.meta.ts.timestamp_micros() > ts {
                    continue;
                }
            }
            if best.as_ref().map_or(true, |b| rev_key(&entry.meta) > rev_key(&b.meta)) {
                best = Some(entry);
            }
        }
        match best {
            Some(entry) => Ok(Some((entry.fetch_text)()?)),
            None => Ok(None),
        }
    }

    /// Existence of `title` at τ — the red-link / `#ifexist` fast path.
    ///
    /// Title tables only, NO frame decode: resolves through the same
    /// `title_intervals` window as [`Instance::page_id_by_title_at`], so it
    /// is `false` for τ before the title's first interval opens (import
    /// records the real earliest-revision start, not 0). Legacy pre-interval
    /// depots (start_ts = 0) still report existence from t = 0.
    pub fn exists_at(&self, title: &str, ts_micros: Option<i64>) -> Result<bool> {
        Ok(self.page_id_by_title_at(title, ts_micros)?.is_some())
    }

    /// Raw siteinfo snapshot JSON selected for τ (plan §2 siteinfo rule):
    /// the snapshot with `max(captured_at) ≤ τ`; for τ before our first
    /// snapshot, the OLDEST we hold. `None` τ → the newest snapshot.
    /// `Ok(None)` only when no snapshots exist.
    pub fn site_config_at(&self, ts_micros: Option<i64>) -> Result<Option<serde_json::Value>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let bytes: Option<Vec<u8>> = match ts_micros {
            None => g
                .conn
                .query_row(
                    "SELECT json FROM siteinfo_snapshots
                     ORDER BY captured_at DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .ok(),
            Some(ts) => {
                let at = g
                    .conn
                    .query_row(
                        "SELECT json FROM siteinfo_snapshots
                         WHERE captured_at <= ?1
                         ORDER BY captured_at DESC LIMIT 1",
                        rusqlite::params![ts],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .ok();
                match at {
                    Some(b) => Some(b),
                    None => g
                        .conn
                        .query_row(
                            "SELECT json FROM siteinfo_snapshots
                             ORDER BY captured_at ASC LIMIT 1",
                            [],
                            |r| r.get(0),
                        )
                        .ok(),
                }
            }
        };
        match bytes {
            Some(b) => Ok(Some(
                serde_json::from_slice(&b).map_err(|_| Error::Corrupt("siteinfo snapshot json"))?,
            )),
            None => Ok(None),
        }
    }

    /// Has this dump part already been fully imported? Keyed by the
    /// part's filename (`parts_seen` table).
    pub fn part_seen(&self, filename: &str) -> Result<bool> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let n: u64 = g.conn.query_row(
            "SELECT COUNT(*) FROM parts_seen WHERE part_filename = ?1",
            [filename],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record a fully-imported dump part. Call only after the part's
    /// pages are durably flushed — the watermark is the skip signal for
    /// the next sync, so writing it early would drop data on a crash.
    pub fn mark_part_seen(&self, filename: &str, sha256: Option<&str>) -> Result<()> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        g.conn.execute(
            "INSERT OR REPLACE INTO parts_seen(part_filename, sha256, completed_at)
             VALUES(?1, ?2, strftime('%s','now'))",
            rusqlite::params![filename, sha256],
        )?;
        Ok(())
    }

    /// Session-end compaction: reclaim update-churn slack parked in the
    /// depot's current write files (see `Depot::collect`). Cheap when
    /// there is nothing to reclaim; call once after a batch of imports,
    /// not per part.
    pub fn collect(&self) -> Result<()> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        g.depot.collect()?;
        Ok(())
    }

    /// Flush depot + strpool + sqlite to durable storage.
    pub fn flush(&self) -> Result<()> {
        let mut g = self.inner.lock().expect("instance mutex poisoned");
        g.dirty_stamped = false; // next import re-stamps
        let g = &*g;
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
        // Everything the session wrote is now durable IN ORDER (depot
        // first, then bookkeeping): clear the dirty flag. A crash after
        // this point is a clean shutdown for the repair logic.
        g.conn.execute(
            "INSERT OR REPLACE INTO instance_flags(key, value) VALUES('dirty', 0)",
            [],
        )?;
        g.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .map_err(Error::Sqlite)?;
        Ok(())
    }
}

/// Take the exclusive per-root flock, or fail with `InstanceLocked`.
fn acquire_root_lock(root: &std::path::Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".lock"))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(crate::error::Error::InstanceLocked(root.to_path_buf()));
    }
    Ok(f)
}

/// Collect every revision record on `chain_id`, newest-first. Walks the
/// chain the way it was encoded (depot SPEC "The shape of a chain"):
///   - f0 = newest record, standalone zstd;
///   - f1 = older records concatenated newest-first, refPrefix-anchored
///     on f0's record;
///   - each cold frame is a sealed former accumulator, anchored on the
///     OLDEST record of the next-newer frame — which is exactly the
///     last record decoded so far in this newest-first walk.
/// Total order used to pick the newest revision: latest timestamp wins,
/// ties broken by higher rev_id. See [`Instance::revision_at`] for why
/// chain position cannot be used instead.
fn rev_key(m: &RevisionMeta) -> (i64, u64) {
    (m.ts.timestamp_micros(), m.rev_id)
}

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
