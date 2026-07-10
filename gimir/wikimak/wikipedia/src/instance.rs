//! `Instance` — the per-dbname Wikipedia mirror.
//!
//! Public API per SPEC §"API (sketch)".

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use strpool::{Pool, PoolConfig};
use wikimak_depot::{Depot, DepotConfig};
use wikimak_mediawiki::PageStream;

use crate::error::{Error, Result};
use crate::import::do_import;
use crate::schema::META_DDL;

/// Default `max_chain_id` INDEX SIZE HINT for fresh instances: sized
/// for enwiki (~80M page ids in 2026) with headroom. The cost is the
/// depot's index file at 8 bytes/chain — 800MB LOGICAL, but the index
/// is created with `ftruncate` and stays sparse: untouched chains
/// never allocate a disk block (pinned by the depot's sparse-index
/// test). It is ONLY a hint: page ids beyond it grow the index
/// automatically; only ids at/above the depot's 2^40 sanity ceiling
/// are rejected ([`crate::Error::PageIdOverflow`]).
pub const DEFAULT_MAX_CHAIN_ID: u64 = 100_000_000;

/// The index capacity an EXISTING instance root currently has —
/// derived from the on-disk depot index (`capacity * 8` bytes). A
/// fresh root (no index yet) gets [`DEFAULT_MAX_CHAIN_ID`]. The depot
/// derives capacity from disk and auto-grows, so any hint opens any
/// root; this helper just keeps a `--max-page-id`-less CLI open from
/// passing a hint smaller than what the store already holds.
pub fn max_chain_id_for_root(root: &std::path::Path) -> u64 {
    std::fs::metadata(root.join("depot").join("index"))
        .map(|m| m.len() / 8)
        .ok()
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_CHAIN_ID)
}

/// The read-side [`InstanceConfig`] for an EXISTING root: the page-id
/// bound derives from the on-disk depot index, everything else is the
/// wikimak driver CLI's defaults. This is what the engine's attach
/// verb and the pinned readout ([`crate::readout`]) open with — one
/// place, so a read-side open always matches what the writer created.
/// (The title shard count is not persisted on disk; this assumes the
/// CLI default, as every read-side embedder always has.)
pub fn read_config(root: PathBuf) -> InstanceConfig {
    let max_chain_id = max_chain_id_for_root(&root);
    InstanceConfig {
        root,
        dbname: "wiki".into(),
        max_chain_id,
        depot: DepotConfig {
            root: PathBuf::new(), // forced to <root>/depot/
            max_chain_id,
            file_size_threshold: 1 << 30,
            eviction_dead_ratio: 0.5,
        },
        title_shard_count: 4,
        title_seal_threshold_bytes: 8 << 20,
        f1_seal_threshold_bytes: 0,
    }
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
    /// Initial page-id capacity HINT: sizes a fresh depot index at
    /// `max_chain_id * 8` sparse bytes. NOT a limit — the index grows
    /// automatically for larger page ids; only ids at/above the
    /// depot's 2^40 sanity ceiling are rejected at import.
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
    /// `Arc` so the streaming [`HistoryIter`] (and its lazy `fetch_text`
    /// closures) can hold the handles across calls without borrowing
    /// the `Instance` — a history walk is frame-at-a-time, not a
    /// snapshot of the whole decompressed chain.
    pub(crate) inner: Arc<Mutex<InstanceInner>>,
    /// Read/import page-id clip — always the depot's 2^40 sanity
    /// ceiling (see `open`); ids below it are covered by index growth.
    pub(crate) max_chain_id: u64,
    pub(crate) f1_seal_threshold_bytes: u64,
    pub(crate) title_shard_count: u32,
    /// True when the previous session ended dirty (crash between an
    /// import write and a flush): `revisions_seen` may then be AHEAD of
    /// the depot (rows durable, frames lost). Imports repair each
    /// touched page's rows from the chain before trusting them.
    pub(crate) suspect: bool,
    /// Opened under a shared flock ([`Instance::open_read`]): every
    /// write API refuses loudly, and reads never backfill.
    pub(crate) read_only: bool,
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
    /// An import errored mid-page this session: the chain may be AHEAD
    /// of `revisions_seen` (prepends landed, rows rolled back). Reads
    /// then distrust the rows and scan the chain, exactly like a
    /// suspect open would after the crash-equivalent state.
    pub(crate) import_errored: bool,
    /// Bounded LRU of decompressed-and-indexed title shards (exact
    /// title → dense ids). See `titles::TitleCache` — this is what
    /// makes a render's link set cost at most one shard walk per
    /// touched shard.
    pub(crate) title_cache: crate::titles::TitleCache,
    /// The root's flock, held for the instance's lifetime.
    pub(crate) _lock: std::fs::File,
}

impl InstanceInner {
    /// Dense title-dictionary ids whose pool bytes equal `normalized`
    /// — the exact-title read primitive. One fnv-picked shard at most;
    /// cache hits touch no pool file at all.
    pub(crate) fn title_ids(&mut self, shard_count: u32, normalized: &[u8]) -> Result<Vec<u64>> {
        self.title_cache.lookup(&self.titles, shard_count, normalized)
    }
}

impl Instance {
    /// Open or create the instance at `cfg.root`. Creates `depot/`,
    /// `titles/`, and `meta.db` if absent. Re-open is idempotent.
    pub fn open(cfg: InstanceConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;

        // One-process-per-root guard: an exclusive flock on <root>/.lock,
        // held for the Instance's lifetime and auto-released by the
        // kernel on any exit (even a crash). Taken BEFORE the depot
        // opens: its open reads tier-file metadata (and may persist a
        // counters rebuild), which must not race the incumbent writer's
        // prepends/evictions. Shared-flock readers
        // ([`Instance::open_read`]) stay possible — only they and a
        // second writing instance are locked out while we hold this.
        let lock = flock_root(&cfg.root, libc::LOCK_EX)?;

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
        ensure_revision_ts_schema(&conn)?;
        ensure_title_dictionary_schema(&conn)?;
        let suspect: bool = conn
            .query_row(
                "SELECT value FROM instance_flags WHERE key = 'dirty'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .unwrap_or(false);

        Ok(Self {
            inner: Arc::new(Mutex::new(InstanceInner {
                depot,
                titles,
                conn,
                repaired: Default::default(),
                dirty_stamped: false,
                import_errored: false,
                title_cache: crate::titles::TitleCache::new(crate::titles::TITLE_CACHE_BUDGET),
                _lock: lock,
            })),
            suspect,
            read_only: false,
            // The page-id clip is the depot's 2^40 sanity ceiling, not
            // the config value: `cfg.max_chain_id` is only the fresh
            // index's SIZE HINT (the depot auto-grows past it), so a
            // page imported beyond the hint must stay readable.
            max_chain_id: wikimak_depot::CHAIN_ID_CEILING,
            f1_seal_threshold_bytes: if cfg.f1_seal_threshold_bytes == 0 {
                256 * 1024
            } else {
                cfg.f1_seal_threshold_bytes
            },
            title_shard_count: cfg.title_shard_count,
            dbname: cfg.dbname,
        })
    }

    /// Open an EXISTING instance for reading, under a SHARED flock: any
    /// number of concurrent readers, excluded only while a writer
    /// ([`Instance::open`]) holds the root — and vice versa. The flock
    /// is what keeps the depot's file set stable under a reader (import
    /// prepends and eviction unlink tier files and patch next-pointers
    /// in place; lock-free reads against a live writer would chase
    /// dangling pointers), so hold the handle only as long as the read
    /// takes: decode, drop.
    ///
    /// Never creates or migrates anything: a non-instance root is a
    /// loud error, a meta.db predating the read-side schema migrations
    /// is [`Error::LegacySchema`] (open it writable once to migrate),
    /// and every write API refuses with [`Error::ReadOnly`] — including
    /// the legacy-row ts backfill, which stays writer-side. A dirty
    /// flag left by a crashed writer still demotes reads to the
    /// chain-scan path (`suspect`); the repair itself is import-side
    /// and therefore refused here.
    pub fn open_read(cfg: InstanceConfig) -> Result<Self> {
        if !cfg.root.join("meta.db").exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no wikimak instance at {}", cfg.root.display()),
            )));
        }
        // Shared lock FIRST, same discipline as `open`: the depot open
        // below reads tier-file metadata, which must not race a writer.
        let lock = flock_root(&cfg.root, libc::LOCK_SH)?;

        let mut depot_cfg = cfg.depot;
        depot_cfg.root = cfg.root.join("depot");
        let depot = Depot::open(depot_cfg)?;
        let titles = Pool::open(
            &cfg.root.join("titles"),
            PoolConfig {
                shard_count: cfg.title_shard_count,
                seal_threshold_bytes: cfg.title_seal_threshold_bytes,
            },
            None,
        )?;

        // No DDL, no pragma writes, no ALTERs: the writer created the
        // schema; this connection only ever SELECTs. Reads key off the
        // migrated `ts`/`title_id` columns, so a pre-migration db is a
        // loud error naming the cure, never a wrong answer.
        let conn = Connection::open_with_flags(
            cfg.root.join("meta.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, // WAL recovery may write; we never do
        )?;
        for (table, col) in [("revisions_seen", "ts"), ("title_intervals", "title_id")] {
            if !has_column(&conn, table, col)? {
                return Err(Error::LegacySchema(cfg.root.clone()));
            }
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
            inner: Arc::new(Mutex::new(InstanceInner {
                depot,
                titles,
                conn,
                repaired: Default::default(),
                dirty_stamped: false,
                import_errored: false,
                title_cache: crate::titles::TitleCache::new(crate::titles::TITLE_CACHE_BUDGET),
                _lock: lock,
            })),
            suspect,
            read_only: true,
            max_chain_id: wikimak_depot::CHAIN_ID_CEILING,
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
        if self.read_only {
            return Err(Error::ReadOnly("import"));
        }
        do_import(self, stream)
    }

    /// Read the current head revision metadata for `page_id` — the
    /// newest revision by timestamp.
    ///
    /// NOT the depot chain's f0 frame: f0 is the most-recently-*imported*
    /// record, which is only the newest-by-time when revisions were
    /// appended in chronological order. Out-of-order / cross-import
    /// prepends (a later import supplying a gap revision) make f0 an older
    /// revision. The head's identity comes from the per-revision `ts`
    /// rows import persists in sqlite (see [`Instance::revision_at`]);
    /// in the common in-order case the named record IS f0, so a head
    /// read decodes exactly one frame.
    pub fn page_head(&self, page_id: u64) -> Result<Option<RevisionMeta>> {
        Ok(self.revision_query(page_id, None, false)?.map(|(m, _)| m))
    }

    /// Read the current head revision's text bytes (UTF-8) for
    /// `page_id` — the newest revision by timestamp (see [`page_head`]).
    /// `Ok(None)` if no such page.
    pub fn page_head_text(&self, page_id: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.revision_query(page_id, None, true)?.and_then(|(_, t)| t))
    }

    /// Iterate all revisions of `page_id`, newest-first (chain order).
    ///
    /// STREAMING: the iterator holds at most one decompressed frame at a
    /// time (plus the record anchoring the next frame's refPrefix) and
    /// decodes metadata only — no text is materialized by iteration.
    /// Each entry's `fetch_text` re-walks the chain to its record with
    /// an early stop and copies out that one text. The iterator
    /// snapshots f0/f1/cold-head on its first step, so a concurrent
    /// import doesn't tear the walk (cold frames themselves are
    /// immutable).
    pub fn page_history(&self, page_id: u64) -> Result<HistoryIter> {
        if page_id >= self.max_chain_id {
            return Ok(HistoryIter { inner: Box::new(std::iter::empty()) });
        }
        Ok(HistoryIter {
            inner: Box::new(HistoryWalk {
                inner: Arc::clone(&self.inner),
                chain_id: page_id,
                walk: WalkState::new_snapshot(page_id),
            }),
        })
    }

    /// Depot frame-payload read counters — instrumentation for the
    /// read-path acceptance tests (a head read touches only f0; a τ
    /// read stops at the frame holding its target).
    pub fn depot_read_counts(&self) -> wikimak_depot::ReadCounts {
        self.inner.lock().expect("instance mutex poisoned").depot.read_counts()
    }

    /// Cumulative depot bytes written this session (frame appends,
    /// eviction copies, pointer patches, index flips) — the numerator
    /// of the import write-amplification MEASUREMENT (forward build
    /// ≈ 1.0×, prepend path higher; see the forward_build tests).
    pub fn depot_bytes_written(&self) -> u64 {
        self.inner.lock().expect("instance mutex poisoned").depot.bytes_written()
    }

    /// List `(page_id, title)` pairs, title-ordered, optionally filtered
    /// by a case-insensitive substring. The answer to "which pages do I
    /// have?" — ids alone are not a UI.
    ///
    /// Titles come from the sharded strpool dictionary, scanned in
    /// parallel across ALL shards (`titles::scan_candidates`) with the
    /// same lossy-UTF-8 lowercase `contains` filter this method has
    /// always applied; the byte ordering equals the old
    /// `ORDER BY normalized_title`. Each matched title resolves to its
    /// page through the INTEGER-keyed `title_id` hop — reads never scan
    /// `title_intervals.normalized_title`. Memory is bounded by the
    /// scan window (≤ threads × `limit` candidates), never the corpus.
    pub fn pages(&self, filter: Option<&str>, limit: usize)
        -> Result<Vec<(u64, String)>>
    {
        let g = self.inner.lock().expect("instance mutex poisoned");
        if limit == 0 {
            return Ok(Vec::new());
        }
        let needle = filter.map(str::to_lowercase);
        let matches = |bytes: &[u8]| -> bool {
            match &needle {
                None => true,
                Some(n) => String::from_utf8_lossy(bytes).to_lowercase().contains(n.as_str()),
            }
        };

        // Open intervals only: a page renamed away keeps its old title
        // as a closed interval, which must not surface as current.
        let mut open_rows = g.conn.prepare_cached(
            "SELECT page_id FROM title_intervals
             WHERE title_id = ?1 AND end_ts IS NULL
             ORDER BY page_id",
        )?;

        // Degenerate compatibility set: open rows the dictionary does
        // not know (rows written outside import, e.g. synthetic test
        // fixtures; empty on any imported store — O(1) via the partial
        // index). Collected once, merged into the ordered walk below.
        let mut extras: Vec<(Vec<u8>, u64)> = Vec::new();
        if has_unmapped_interval_rows(&g.conn)? {
            let mut st = g.conn.prepare(
                "SELECT normalized_title, page_id FROM title_intervals
                 WHERE title_id IS NULL AND end_ts IS NULL",
            )?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)? as u64)))?;
            for row in rows {
                let (bytes, pid) = row?;
                if matches(&bytes) {
                    extras.push((bytes, pid));
                }
            }
            extras.sort();
        }
        let mut extras = extras.into_iter().peekable();

        let mut out: Vec<(u64, String)> = Vec::new();
        let mut window: Option<crate::titles::Candidate> = None;
        loop {
            let pass = crate::titles::scan_candidates(
                &g.titles,
                self.title_shard_count,
                &matches,
                limit - out.len(),
                window.as_ref(),
            )?;
            for cand in &pass.candidates {
                // Extras that sort at or before this candidate go first.
                while out.len() < limit
                    && extras.peek().is_some_and(|e| e.0.as_slice() <= cand.0.as_slice())
                {
                    let (bytes, pid) = extras.next().expect("peeked");
                    out.push((pid, String::from_utf8_lossy(&bytes).into_owned()));
                }
                if out.len() >= limit {
                    break;
                }
                let title = String::from_utf8_lossy(&cand.0).into_owned();
                let pids = open_rows
                    .query_map([cand.1 as i64], |r| r.get::<_, i64>(0))?
                    .collect::<std::result::Result<Vec<i64>, _>>()?;
                for pid in pids {
                    out.push((pid as u64, title.clone()));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            match (out.len() >= limit, pass.next_window) {
                (true, _) | (false, None) => break,
                (false, Some(bound)) => window = Some(bound),
            }
        }
        // Extras past the last dictionary candidate.
        while out.len() < limit {
            match extras.next() {
                Some((bytes, pid)) => {
                    out.push((pid, String::from_utf8_lossy(&bytes).into_owned()))
                }
                None => break,
            }
        }
        Ok(out)
    }

    /// Resolve a page by exact title, else by unique case-insensitive
    /// substring. `Err(TitleAmbiguous)`-free by design: ambiguity comes
    /// back as `Ok(None)` plus the candidates for the caller to show.
    ///
    /// The exact hit goes through the title dictionary (one shard, no
    /// pool-wide scan) — it can no longer be shadowed by 16 earlier
    /// substring matches the way the old scan-then-find could miss it.
    pub fn page_by_title(&self, title: &str) -> Result<TitleResolution> {
        let all = self.pages(Some(title), 16)?;
        if let Some(hit) = all.iter().find(|(_, t)| t == title) {
            return Ok((Some(hit.0), all));
        }
        if let Some(id) = self.exact_current_page_id(title.as_bytes())? {
            return Ok((Some(id), all));
        }
        match all.as_slice() {
            [(id, _)] => Ok((Some(*id), all)),
            _ => Ok((None, all)),
        }
    }

    /// The page currently holding EXACTLY `normalized` (open interval),
    /// resolved through the dictionary: fnv-picked shard → dense ids →
    /// integer-keyed interval rows. The smallest matching page id wins
    /// (deterministic where the old scan order was not).
    fn exact_current_page_id(&self, normalized: &[u8]) -> Result<Option<u64>> {
        let mut g = self.inner.lock().expect("instance mutex poisoned");
        let g = &mut *g;
        let ids = g.title_ids(self.title_shard_count, normalized)?;
        let mut best: Option<u64> = None;
        for id in &ids {
            let pid: Option<i64> = g
                .conn
                .prepare_cached(
                    "SELECT page_id FROM title_intervals
                     WHERE title_id = ?1 AND end_ts IS NULL
                     ORDER BY page_id LIMIT 1",
                )?
                .query_row([*id as i64], |r| r.get(0))
                .map(Some)
                .or_else(ignore_no_rows)?;
            if let Some(pid) = pid {
                best = Some(best.map_or(pid as u64, |b| b.min(pid as u64)));
            }
        }
        if best.is_none() && has_unmapped_interval_rows(&g.conn)? {
            best = g
                .conn
                .prepare_cached(
                    "SELECT page_id FROM title_intervals
                     WHERE title_id IS NULL AND normalized_title = ?1 AND end_ts IS NULL
                     ORDER BY page_id LIMIT 1",
                )?
                .query_row(rusqlite::params![normalized], |r| r.get::<_, i64>(0))
                .map(|v| Some(v as u64))
                .or_else(ignore_no_rows)?;
        }
        Ok(best)
    }

    /// Strpool per-shard walk counters for the titles pool —
    /// instrumentation for the title-read acceptance tests (an exact
    /// lookup touches ONE shard; a substring search touches all).
    pub fn title_scan_counts(&self) -> Vec<u64> {
        self.inner.lock().expect("instance mutex poisoned").titles.scan_counts()
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
    ///
    /// Resolution is dictionary-first: the trimmed title's fnv-picked
    /// strpool shard yields its dense ids (one shard walk, cached), and
    /// every sqlite hop below is an INTEGER-keyed indexed lookup —
    /// `title_intervals.normalized_title` is never scanned. Rows the
    /// dictionary does not know (written outside import; none exist on
    /// an imported store) are covered by an O(1)-guarded compatibility
    /// branch over the unmapped-row set.
    pub fn page_id_by_title_at(&self, title: &str, ts_micros: Option<i64>) -> Result<Option<u64>> {
        let ts = match ts_micros {
            None => {
                // Exact-first through the dictionary: the common link-
                // resolution case costs one shard probe, not the
                // pool-wide substring scan `page_by_title` performs for
                // its candidate list.
                if let Some(id) = self.exact_current_page_id(title.as_bytes())? {
                    return Ok(Some(id));
                }
                return Ok(self.page_by_title(title)?.0);
            }
            Some(ts) => ts,
        };
        let key = title.trim().as_bytes().to_vec();
        let mut g = self.inner.lock().expect("instance mutex poisoned");
        let g = &mut *g;
        let ids = g.title_ids(self.title_shard_count, &key)?;
        let unmapped = has_unmapped_interval_rows(&g.conn)?;
        // The τ window per id: start_ts <= τ AND (end_ts IS NULL OR
        // end_ts > τ), newest interval wins — same window SQL as ever,
        // keyed by title_id via idx_title_intervals_title_id.
        let mut hit: Option<(i64, i64)> = None; // (start_ts, page_id), max by start_ts
        for id in &ids {
            let row: Option<(i64, i64)> = g
                .conn
                .prepare_cached(
                    "SELECT start_ts, page_id FROM title_intervals
                     WHERE title_id = ?1
                       AND start_ts <= ?2
                       AND (end_ts IS NULL OR end_ts > ?2)
                     ORDER BY start_ts DESC LIMIT 1",
                )?
                .query_row(rusqlite::params![*id as i64, ts], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(Some)
                .or_else(ignore_no_rows)?;
            if let Some(row) = row {
                if hit.is_none_or(|h| row.0 > h.0) {
                    hit = Some(row);
                }
            }
        }
        if unmapped {
            let row: Option<(i64, i64)> = g
                .conn
                .prepare_cached(
                    "SELECT start_ts, page_id FROM title_intervals
                     WHERE title_id IS NULL AND normalized_title = ?1
                       AND start_ts <= ?2
                       AND (end_ts IS NULL OR end_ts > ?2)
                     ORDER BY start_ts DESC LIMIT 1",
                )?
                .query_row(rusqlite::params![key, ts], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(Some)
                .or_else(ignore_no_rows)?;
            if let Some(row) = row {
                if hit.is_none_or(|h| row.0 > h.0) {
                    hit = Some(row);
                }
            }
        }
        if let Some((_, id)) = hit {
            return Ok(Some(id as u64));
        }
        // Distinguish "title has interval rows, none cover τ" (→ None,
        // did not exist at τ) from "no interval rows at all" (→ fall back
        // to the current mapping, for pre-interval imports).
        let mut any_interval: i64 = 0;
        for id in &ids {
            any_interval += g
                .conn
                .prepare_cached("SELECT COUNT(*) FROM title_intervals WHERE title_id = ?1")?
                .query_row([*id as i64], |r| r.get::<_, i64>(0))?;
        }
        if any_interval == 0 && unmapped {
            any_interval += g.conn.query_row(
                "SELECT COUNT(*) FROM title_intervals
                 WHERE title_id IS NULL AND normalized_title = ?1",
                rusqlite::params![key],
                |r| r.get::<_, i64>(0),
            )?;
        }
        if any_interval > 0 {
            return Ok(None);
        }
        let mut current: Option<i64> = None;
        for id in &ids {
            current = g
                .conn
                .prepare_cached(
                    "SELECT page_id FROM page_to_title_id WHERE title_id = ?1 LIMIT 1",
                )?
                .query_row([*id as i64], |r| r.get(0))
                .map(Some)
                .or_else(ignore_no_rows)?;
            if current.is_some() {
                break;
            }
        }
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
    /// The answer is `argmax` over `(timestamp, rev_id)` — NOT the first
    /// record in chain order. Chain order is import-prepend order, not
    /// timestamp order: an out-of-order or cross-import gap revision (a
    /// later import supplying an earlier revision) lands at the chain
    /// head, so "first with ts ≤ τ" would return a non-newest revision.
    /// The argmax itself is one indexed lookup over the per-revision `ts`
    /// rows import persists in sqlite; the chain is then walked
    /// newest-first, meta-only, stopping at the named record — never
    /// decoding the frames past it. Only when the rows can't be trusted
    /// (legacy NULL-ts rows, a suspect open, or sqlite ahead of the
    /// chain after a crash) does the read fall back to the full
    /// streaming scan — once, backfilling the rows it derived.
    pub fn revision_at(&self, page_id: u64, ts_micros: Option<i64>) -> Result<Option<RevisionMeta>> {
        Ok(self.revision_query(page_id, ts_micros, false)?.map(|(m, _)| m))
    }

    /// Text bytes of the revision selected by [`Instance::revision_at`].
    ///
    /// Same selection; only the chosen revision's text is ever copied
    /// out of its frame. `None` τ → newest-revision text; `Ok(None)`
    /// when no revision is ≤ τ.
    pub fn page_text_at(&self, page_id: u64, ts_micros: Option<i64>) -> Result<Option<Vec<u8>>> {
        Ok(self.revision_query(page_id, ts_micros, true)?.and_then(|(_, t)| t))
    }

    /// Text bytes of EXACTLY revision `rev_id` of `page_id` — the
    /// read-at-rev primitive behind pinned attachments
    /// ([`crate::readout`]). One newest-first early-stopping chain walk
    /// (no sqlite row is consulted — the pin names its record): a pin
    /// at the chain head decodes f0 only; an older pin pays the frames
    /// down to its record and nothing past it. Residency is one frame
    /// plus the one text copied out. `Ok(None)` = no such page or the
    /// chain holds no such revision.
    pub fn revision_text(&self, page_id: u64, rev_id: u64) -> Result<Option<Vec<u8>>> {
        if page_id >= self.max_chain_id {
            return Ok(None);
        }
        let g = self.inner.lock().expect("instance mutex poisoned");
        Ok(find_revision(&g.depot, page_id, rev_id, true)?.and_then(|(_, t)| t))
    }

    /// The shared read core behind [`page_head`](Self::page_head) /
    /// [`page_head_text`](Self::page_head_text) /
    /// [`revision_at`](Self::revision_at) /
    /// [`page_text_at`](Self::page_text_at) — and, through those, the
    /// serve layer and the engine's readout. Selection contract is
    /// documented on [`Instance::revision_at`].
    fn revision_query(
        &self,
        page_id: u64,
        ts_micros: Option<i64>,
        want_text: bool,
    ) -> Result<Option<(RevisionMeta, Option<Vec<u8>>)>> {
        if page_id >= self.max_chain_id {
            return Ok(None);
        }
        let g = self.inner.lock().expect("instance mutex poisoned");
        let g = &*g;

        // COUNT(ts) counts non-NULL rows: the page's bookkeeping is
        // complete iff every row carries a timestamp.
        let (total, with_ts): (i64, i64) = g
            .conn
            .prepare_cached("SELECT COUNT(*), COUNT(ts) FROM revisions_seen WHERE page_id = ?1")?
            .query_row([page_id as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;

        // Rows are authoritative only when timestamped AND this session
        // has no reason to believe the chain diverged from them (a
        // suspect open or a mid-page import error can leave the chain
        // AHEAD of the rows — the chain is the data fence, so those
        // states scan it).
        let rows_trusted = total > 0 && with_ts == total && !self.suspect && !g.import_errored;
        if rows_trusted {
            let target: Option<i64> = match ts_micros {
                None => g
                    .conn
                    .prepare_cached(
                        "SELECT rev_id FROM revisions_seen WHERE page_id = ?1
                         ORDER BY ts DESC, rev_id DESC LIMIT 1",
                    )?
                    .query_row([page_id as i64], |r| r.get(0))
                    .map(Some)
                    .or_else(ignore_no_rows)?,
                Some(tau) => g
                    .conn
                    .prepare_cached(
                        "SELECT rev_id FROM revisions_seen WHERE page_id = ?1 AND ts <= ?2
                         ORDER BY ts DESC, rev_id DESC LIMIT 1",
                    )?
                    .query_row(rusqlite::params![page_id as i64, tau], |r| r.get(0))
                    .map(Some)
                    .or_else(ignore_no_rows)?,
            };
            match target {
                Some(rev_id) => {
                    if let Some(hit) = find_revision(&g.depot, page_id, rev_id as u64, want_text)? {
                        return Ok(Some(hit));
                    }
                    // The named revision is not on the chain: sqlite got
                    // ahead of the depot (rows durable, frames lost in a
                    // crash) and this page wasn't repaired yet. Fall
                    // through to the chain scan — the chain is truth.
                }
                // Complete, trusted rows and none qualifies: the page
                // did not exist at τ. No frame is touched at all.
                None => return Ok(None),
            }
        }

        // Fallback: stream the whole chain (one frame resident at a
        // time), argmax over (ts, rev_id) — and, when rows exist but
        // predate the ts column, backfill them inside one transaction
        // so the NEXT read takes the indexed path. Rows the chain
        // doesn't confirm are never invented here; suspect-mode import
        // repair owns row re-derivation. A read-only open still scans
        // (correct answer) but never backfills — that write belongs to
        // the exclusive-lock holder.
        let backfill = total > 0 && with_ts < total && !self.read_only;
        if backfill {
            g.conn.execute("BEGIN IMMEDIATE", [])?;
        }
        let result = (|| {
            let mut fill = if backfill {
                Some(g.conn.prepare_cached(
                    "UPDATE revisions_seen SET ts = ?3
                     WHERE page_id = ?1 AND rev_id = ?2 AND ts IS NULL",
                )?)
            } else {
                None
            };
            scan_best(&g.depot, page_id, ts_micros, want_text, &mut |rev_id, ts| {
                if let Some(st) = fill.as_mut() {
                    st.execute(rusqlite::params![page_id as i64, rev_id as i64, ts])?;
                }
                Ok(())
            })
        })();
        if backfill {
            match &result {
                Ok(_) => {
                    g.conn.execute("COMMIT", [])?;
                }
                Err(_) => {
                    let _ = g.conn.execute("ROLLBACK", []);
                }
            }
        }
        result
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
        if self.read_only {
            return Err(Error::ReadOnly("mark_part_seen"));
        }
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
        if self.read_only {
            return Err(Error::ReadOnly("collect"));
        }
        let g = self.inner.lock().expect("instance mutex poisoned");
        g.depot.collect()?;
        Ok(())
    }

    /// Flush depot + strpool + sqlite to durable storage.
    pub fn flush(&self) -> Result<()> {
        if self.read_only {
            // A read-only flush would also CLEAR the dirty flag a
            // crashed writer left — never touch the fence from a reader.
            return Err(Error::ReadOnly("flush"));
        }
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

/// Take the per-root flock (`op` = `LOCK_EX` for the one writer,
/// `LOCK_SH` for readers), non-blocking: contention is a loud
/// `InstanceLocked`, never a silent wait behind a possibly hours-long
/// import run. Kernel-released on any exit (even a crash).
fn flock_root(root: &std::path::Path, op: libc::c_int) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".lock"))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) };
    if rc != 0 {
        return Err(crate::error::Error::InstanceLocked(root.to_path_buf()));
    }
    Ok(f)
}

/// Does `table` carry a column named `col`? (PRAGMA table_info — a
/// pure read; `open_read`'s schema fence and the lazy migrations both
/// probe through this.)
fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    Ok(conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .flatten()
        .any(|name| name == col))
}

/// Total order used to pick the newest revision: latest timestamp wins,
/// ties broken by higher rev_id. See [`Instance::revision_at`] for why
/// chain position cannot be used instead.
fn rev_key(m: &RevisionMeta) -> (i64, u64) {
    (m.ts.timestamp_micros(), m.rev_id)
}

/// Map rusqlite's no-rows to `Ok(None)` for optional single-row lookups.
fn ignore_no_rows<T>(e: rusqlite::Error) -> std::result::Result<Option<T>, Error> {
    match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        e => Err(e.into()),
    }
}

/// Lazy meta.db migration for the per-revision `ts` column (2026-07,
/// "reads must not decode whole chains"): a db created before the column
/// existed gets it via ALTER (rows NULL — backfilled per page by the
/// first read that needs them, see `Instance::revision_query`); fresh
/// dbs already carry it from the DDL. The (page_id, ts, rev_id) index
/// makes the head/τ argmax one logarithmic lookup. Runs after the DDL,
/// BEFORE the index — the index references the column.
fn ensure_revision_ts_schema(conn: &Connection) -> Result<()> {
    if !has_column(conn, "revisions_seen", "ts")? {
        conn.execute("ALTER TABLE revisions_seen ADD COLUMN ts INTEGER", [])?;
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_revisions_seen_page_ts
         ON revisions_seen(page_id, ts DESC, rev_id DESC)",
        [],
    )?;
    Ok(())
}

/// Lazy meta.db migration for `title_intervals.title_id` (2026-07,
/// "wire the title dictionary"): reads resolve titles by dense strpool
/// id, so every interval row must carry the id of its title.
///
///   * Legacy dbs get the column via ALTER, then a one-shot backfill
///     joins each row to `title_id_to_page` on `(ns, normalized_title)`
///     — the same fence discipline as `ensure_revision_ts_schema`.
///   * Import writes the column directly (`ensure_title` carries
///     title_id in every `title_intervals` INSERT and in the
///     retitle-in-place UPDATE), so no write-path compatibility
///     machinery remains: the two interim triggers that derived a
///     missing title_id are DROPPED here (legacy dbs still carry them —
///     they only existed for the window when import didn't write the
///     column; the insert one was WHEN NEW.title_id IS NULL, a no-op
///     since, and the retitle one re-derived the value the UPDATE now
///     sets itself).
///   * A row whose title the dictionary genuinely lacks stays NULL and
///     is served by the reads' unmapped-row compatibility branch, whose
///     guard is O(1) via the partial index below (an INTEGER index over
///     rows that are empty on any imported store — not a text index
///     entrenching the redundant title copy).
fn ensure_title_dictionary_schema(conn: &Connection) -> Result<()> {
    if !has_column(conn, "title_intervals", "title_id")? {
        conn.execute("ALTER TABLE title_intervals ADD COLUMN title_id INTEGER", [])?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_title_intervals_title_id
             ON title_intervals(title_id, start_ts);
         CREATE INDEX IF NOT EXISTS idx_page_to_title_id_title
             ON page_to_title_id(title_id);
         CREATE INDEX IF NOT EXISTS idx_title_intervals_unmapped
             ON title_intervals(page_id) WHERE title_id IS NULL;
         DROP TRIGGER IF EXISTS title_intervals_title_id_insert;
         DROP TRIGGER IF EXISTS title_intervals_title_id_retitle;",
    )?;
    conn.execute(
        "UPDATE title_intervals SET title_id =
             (SELECT title_id FROM title_id_to_page t
               WHERE t.ns = title_intervals.ns
                 AND t.normalized_title = title_intervals.normalized_title)
         WHERE title_id IS NULL",
        [],
    )?;
    Ok(())
}

/// Do any `title_intervals` rows lack a dictionary id? O(1) via the
/// partial index; `false` on every imported store, so the reads'
/// compatibility branches cost one point query and nothing more.
fn has_unmapped_interval_rows(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM title_intervals WHERE title_id IS NULL)",
        [],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

// ---------------------------------------------------------------------
// The streaming chain walk — the ONE decoder every read goes through.
//
// A chain is decoded the way it was encoded (depot SPEC "The shape of a
// chain"): f0 = newest record, standalone zstd; f1 = older records
// concatenated newest-first, refPrefix-anchored on f0's record; each
// cold frame is a sealed former accumulator, anchored on the OLDEST
// record of the next-newer frame — exactly the last record this
// newest-first walk yielded before crossing the frame boundary. The
// walk therefore streams: ONE decompressed frame resident at a time,
// plus the (record-sized) anchor carried across the boundary. Reads
// that used to `collect_records` the whole decompressed history now pay
// for the frames up to their early stop and nothing past it.
// ---------------------------------------------------------------------

/// Resumable newest-first record walk over one chain. Drive it with
/// [`WalkState::next_record`]; the yielded slice borrows the walk's
/// current frame buffer — decode it meta-only and copy out at most the
/// ONE text the read wants.
pub(crate) struct WalkState {
    chain_id: u64,
    /// Snapshot f0/f1/cold-head in one step (under the caller's first
    /// lock hold) instead of on arrival. Used by the cross-lock
    /// [`HistoryWalk`] so a concurrent import can't tear the walk;
    /// under-lock early-stop readers stay lazy so a head read never
    /// touches f1.
    eager: bool,
    frame: WalkFrame,
}

enum WalkFrame {
    Start,
    InFrame {
        /// Decompressed records of the current frame, newest-first.
        raw: Vec<u8>,
        /// Byte offset just past the last yielded record.
        pos: usize,
        /// Byte offset of the last yielded record — at frame end this
        /// is the frame's oldest record, the next frame's anchor.
        last: usize,
        /// Compressed f1 frame captured by an eager snapshot, not yet
        /// walked (Some only while still inside f0).
        pending_f1: Option<Vec<u8>>,
        /// Cold walk continuation; `None` until needed (lazy walks).
        cold: Option<wikimak_depot::ColdCursor>,
    },
    Done,
}

impl WalkState {
    /// Lazy walk: frames are read only when the walk reaches them. Use
    /// under a single lock hold (early-stop readers).
    pub(crate) fn new(chain_id: u64) -> Self {
        WalkState { chain_id, eager: false, frame: WalkFrame::Start }
    }

    /// Snapshotting walk: the first step captures f0 + the COMPRESSED
    /// f1 + the cold head together, so later steps only read immutable
    /// cold frames. For walks that span lock holds ([`HistoryWalk`]).
    pub(crate) fn new_snapshot(chain_id: u64) -> Self {
        WalkState { chain_id, eager: true, frame: WalkFrame::Start }
    }

    /// Yield the next (newest-first) record, or `None` at chain end.
    /// The slice borrows this walk; it is invalidated by the next call.
    pub(crate) fn next_record(&mut self, depot: &Depot) -> Result<Option<&[u8]>> {
        loop {
            match &mut self.frame {
                WalkFrame::Done => return Ok(None),
                WalkFrame::Start => {
                    let f0 = match depot.read_f0(self.chain_id) {
                        Ok(frame) => frame,
                        Err(wikimak_depot::Error::NoFrame)
                        | Err(wikimak_depot::Error::ChainIdOutOfRange) => {
                            self.frame = WalkFrame::Done;
                            return Ok(None);
                        }
                        Err(e) => return Err(e.into()),
                    };
                    let raw = crate::frames::decompress(&f0, None)?;
                    let (pending_f1, cold) = if self.eager {
                        (depot.read_f1(self.chain_id)?, Some(depot.cold_cursor(self.chain_id)?))
                    } else {
                        (None, None)
                    };
                    self.frame = WalkFrame::InFrame { raw, pos: 0, last: 0, pending_f1, cold };
                }
                WalkFrame::InFrame { raw, pos, .. } if *pos < raw.len() => break,
                WalkFrame::InFrame { .. } => self.advance_frame(depot)?,
            }
        }
        // Yield phase, separated from the state mutation so the borrow
        // of `raw` doesn't pin the whole loop.
        let WalkFrame::InFrame { raw, pos, last, .. } = &mut self.frame else { unreachable!() };
        let len = crate::revision::record_len(raw, *pos)?;
        *last = *pos;
        *pos += len;
        let (last, pos) = (*last, *pos);
        Ok(Some(&raw[last..pos]))
    }

    /// Cross a frame boundary: the current frame is exhausted; its
    /// oldest record anchors the next frame's refPrefix decode.
    fn advance_frame(&mut self, depot: &Depot) -> Result<()> {
        let WalkFrame::InFrame { raw, last, pending_f1, cold, .. } =
            std::mem::replace(&mut self.frame, WalkFrame::Done)
        else {
            return Ok(());
        };
        // Keep only the oldest record as the anchor; the frame buffer
        // itself is dropped before the next frame is decompressed.
        let anchor = raw[last..].to_vec();
        drop(raw);
        // Where are we? `pending_f1 = Some` ⇔ eager walk still in f0
        // with a captured f1. `cold = None` ⇔ lazy walk still in f0
        // (f1 unread — a head read that stopped there never touched
        // it). `cold = Some` with no pending f1 ⇔ already in the tail
        // (f1 walked or absent): only cold frames remain.
        let pending_f1 = match (pending_f1, &cold) {
            (Some(f1), _) => Some(f1),
            (None, None) => depot.read_f1(self.chain_id)?,
            (None, Some(_)) => None,
        };
        let mut cold = match cold {
            Some(c) => c,
            None => depot.cold_cursor(self.chain_id)?,
        };
        if let Some(f1) = pending_f1 {
            let raw = crate::frames::decompress(&f1, Some(&anchor))?;
            self.frame =
                WalkFrame::InFrame { raw, pos: 0, last: 0, pending_f1: None, cold: Some(cold) };
            return Ok(());
        }
        match depot.cold_next(&mut cold)? {
            Some(frame) => {
                let raw = crate::frames::decompress(&frame, Some(&anchor))?;
                self.frame =
                    WalkFrame::InFrame { raw, pos: 0, last: 0, pending_f1: None, cold: Some(cold) };
            }
            None => self.frame = WalkFrame::Done,
        }
        Ok(())
    }
}

/// Find `rev_id` on the chain: newest-first early-stopping walk,
/// records peeked by fixed offset (no per-record string decode), the
/// target decoded once and its text copied out only if `want_text`.
pub(crate) fn find_revision(
    depot: &Depot,
    chain_id: u64,
    rev_id: u64,
    want_text: bool,
) -> Result<Option<(RevisionMeta, Option<Vec<u8>>)>> {
    let mut walk = WalkState::new(chain_id);
    while let Some(rec) = walk.next_record(depot)? {
        if crate::revision::peek_rev_id(rec)? == rev_id {
            let (meta, text) = crate::revision::decode_revision_view(rec)?;
            let text = if want_text { Some(text.to_vec()) } else { None };
            return Ok(Some((meta, text)));
        }
    }
    Ok(None)
}

/// Stream the WHOLE chain and pick argmax over `(ts, rev_id)` among
/// records with `ts ≤ τ` (all records for `None` τ) — the fallback for
/// pages whose sqlite rows can't answer. `each` sees every record's
/// `(rev_id, ts)` (the ts backfill hook). At most one frame plus the
/// current best record's text (when `want_text`) is resident.
pub(crate) fn scan_best(
    depot: &Depot,
    chain_id: u64,
    tau: Option<i64>,
    want_text: bool,
    each: &mut dyn FnMut(u64, i64) -> Result<()>,
) -> Result<Option<(RevisionMeta, Option<Vec<u8>>)>> {
    let mut best: Option<(RevisionMeta, Option<Vec<u8>>)> = None;
    let mut walk = WalkState::new(chain_id);
    while let Some(rec) = walk.next_record(depot)? {
        let rev_id = crate::revision::peek_rev_id(rec)?;
        let ts = crate::revision::peek_ts(rec)?;
        each(rev_id, ts)?;
        if tau.is_some_and(|t| ts > t) {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| (ts, rev_id) > rev_key(b)) {
            let (meta, text) = crate::revision::decode_revision_view(rec)?;
            best = Some((meta, if want_text { Some(text.to_vec()) } else { None }));
        }
    }
    Ok(best)
}

/// The streaming iterator behind [`Instance::page_history`]. Owns the
/// instance handles (`Arc`) so it and its entries' `fetch_text`
/// closures outlive the borrow of `Instance`; each `next()` locks only
/// for the step it takes.
struct HistoryWalk {
    inner: Arc<Mutex<InstanceInner>>,
    chain_id: u64,
    walk: WalkState,
}

impl Iterator for HistoryWalk {
    type Item = Result<HistoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let meta = {
            let g = self.inner.lock().expect("instance mutex poisoned");
            let rec = match self.walk.next_record(&g.depot) {
                Ok(Some(rec)) => rec,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            match crate::revision::decode_revision_view(rec) {
                Ok((meta, _text)) => meta, // text stays in the frame buffer
                Err(e) => return Some(Err(e)),
            }
        };
        let inner = Arc::clone(&self.inner);
        let chain_id = self.chain_id;
        let rev_id = meta.rev_id;
        let fetch_text: Box<dyn FnOnce() -> Result<Vec<u8>> + Send> = Box::new(move || {
            let g = inner.lock().expect("instance mutex poisoned");
            match find_revision(&g.depot, chain_id, rev_id, true)? {
                Some((_meta, Some(text))) => Ok(text),
                _ => Err(Error::Corrupt("revision vanished from its chain")),
            }
        });
        Some(Ok(HistoryEntry { meta, fetch_text }))
    }
}
