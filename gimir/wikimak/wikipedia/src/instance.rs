//! `Instance` — the per-dbname Wikipedia mirror.
//!
//! Public API per SPEC §"API (sketch)".

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use strpool::{Pool, PoolConfig};
use wikimak_depot::{Depot, DepotConfig};
use wikimak_mediawiki::PageStream;

use crate::error::{Error, Result};
use crate::import::do_import;
use crate::schema::META_DDL;

pub(crate) const REVISION_DICTIONARY_BYTES: usize = 800 << 10;
pub(crate) const REVISION_SAMPLE_COUNT: usize = 32 << 10;
const REVISION_METADATA_SAMPLE_COUNT: usize = 128 << 10;
const REVISION_DICTIONARY_SAMPLE_RATIO: usize = 8;
const REVISION_MIN_SAMPLES: usize = 128;
const REVISION_MIN_SAMPLE_BYTES: usize = 1 << 20;
const OCCUPIED_ID_BATCH: usize = 4096;

/// Legacy initial index-capacity value. Fresh depots now start with
/// one eight-byte slot and grow geometrically from observed page ids;
/// the field remains in the public configuration for compatibility.
pub const DEFAULT_MAX_CHAIN_ID: u64 = 1;

/// The index capacity an EXISTING instance root currently has —
/// derived from the on-disk depot index (`capacity * 8` bytes). A
/// fresh root (no index yet) gets [`DEFAULT_MAX_CHAIN_ID`]. The depot
/// derives capacity from disk and auto-grows.
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
/// The title shard count is 0 = derive: [`Instance::open_read`] reads
/// the count persisted in meta.db at creation (legacy stores without
/// the flag: 4, the only count the CLI ever built).
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
        title_shard_count: 0, // derive from the store's persisted count
        title_seal_threshold_bytes: 64 << 10,
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
    /// Legacy initial-capacity field retained for API compatibility.
    /// Fresh indexes start at one slot and grow automatically; only
    /// ids at/above the depot's 2^40 sanity ceiling are rejected.
    pub max_chain_id: u64,
    /// Depot tuning. The implementer can pass this through to
    /// [`DepotConfig`] — `root` is forced to `<root>/depot/`. Tests
    /// supply a small `file_size_threshold` to drive eviction.
    pub depot: DepotConfig,
    /// Strpool shard count for the titles pool. Tests use 1.
    ///
    /// The EFFECTIVE count is a property of the store, not the open:
    /// exact-title lookups route by `fnv1a(title) % count`, and shard
    /// files are created lazily, so the truth cannot be recovered from
    /// disk — it is persisted in meta.db at creation (the
    /// `title_shard_count` instance flag). 0 = derive: use the
    /// persisted count (a fresh root gets 256; a legacy
    /// store without the flag counts as 4, the only value the CLI ever
    /// built — a writer open backfills the flag). A nonzero value on
    /// an existing store must MATCH the persisted count —
    /// [`crate::Error::TitleShardMismatch`] otherwise.
    pub title_shard_count: u32,
    /// Strpool seal threshold for the titles pool.
    ///
    /// This is also the dynamic re-sharding target. A shard whose file first
    /// crosses it is sealed, then compared again: doubling is based on the
    /// resulting compressed physical file size (frames plus footer), not the
    /// transient plaintext tail size.
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
    /// Same immutable revision id arrived with different complete
    /// record bytes; canonical content stayed unchanged and the
    /// incoming occurrence was archived in the correction lane.
    pub revision_conflicts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageAction {
    pub event_type: String,
    pub timestamp: String,
    pub comment: String,
    pub actor: String,
    pub historical_title: String,
    pub current_title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionVisibility {
    pub deleted_parts: String,
    pub parts_are_suppressed: bool,
    pub deleted_by_page_deletion: bool,
    pub page_deletion_timestamp: String,
}

/// A conflicting occurrence of an immutable revision retained outside
/// the canonical page chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionCorrection {
    pub revision_id: u64,
    pub occurrence: u64,
    pub incoming_record: Vec<u8>,
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

/// Outcome of the one-time revision-head dictionary finalization.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RevisionDictionaryStats {
    pub dictionary_id: Option<u32>,
    pub trained: bool,
    pub samples: u64,
    pub sample_bytes: u64,
    pub dictionary_bytes: u64,
    pub heads_repacked: u64,
}

/// Read-only accounting from the experimental split revision layout.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SplitRevisionStorageStats {
    pub pages: u64,
    pub revisions: u64,
    pub revision_count_buckets: [u64; 16],
    pub text_dictionary_id: u32,
    pub text_dictionary_bytes: u64,
    pub text_samples: u64,
    pub text_sample_bytes: u64,
    pub metadata_dictionary_id: u32,
    pub metadata_dictionary_bytes: u64,
    pub metadata_samples: u64,
    pub metadata_sample_bytes: u64,
    pub combined_dictionary_id: u32,
    pub combined_dictionary_bytes: u64,
    pub combined_samples: u64,
    pub combined_sample_bytes: u64,
    pub metadata_raw_bytes: u64,
    pub metadata_compressed_bytes: u64,
    pub metadata_frame_bytes: u64,
    pub head_text_raw_bytes: u64,
    pub head_text_compressed_bytes: u64,
    pub head_text_frame_bytes: u64,
    pub head_text_length_buckets: [u64; 16],
    pub history_text_raw_bytes: u64,
    pub history_text_compressed_bytes: u64,
    pub history_text_frame_bytes: u64,
    pub history_text_frames: u64,
    pub combined_f0_raw_bytes: u64,
    pub combined_f0_compressed_bytes: u64,
    pub combined_f0_frame_bytes: u64,
    pub combined_history_raw_bytes: u64,
    pub combined_history_compressed_bytes: u64,
    pub combined_history_frame_bytes: u64,
    pub current_live_f0_bytes: u64,
    pub current_live_f1_bytes: u64,
    pub current_live_cold_bytes: u64,
    pub packed_small_pages: u64,
    pub packed_small_revisions: u64,
    pub packed_small_raw_bytes: u64,
    pub packed_small_compressed_bytes: u64,
    pub packed_small_file_bytes: u64,
    pub packed_small_p50_compressed_shard_bytes: u64,
    pub packed_small_p95_compressed_shard_bytes: u64,
    pub packed_small_p99_compressed_shard_bytes: u64,
    pub packed_small_max_compressed_shard_bytes: u64,
    pub packed_small_split_frame_bytes: u64,
    pub packed_small_combined_frame_bytes: u64,
    pub packed_small_materialized_shards: u64,
    pub packed_small_mean_scan_bytes: u64,
    pub packed_small_p50_scan_bytes: u64,
    pub packed_small_p95_scan_bytes: u64,
    pub packed_small_p99_scan_bytes: u64,
    pub packed_small_max_scan_bytes: u64,
    pub packed_small_benchmark_pages: u64,
    pub packed_small_benchmark_raw_bytes: u64,
    pub packed_small_benchmark_compressed_bytes: u64,
    pub packed_small_benchmark_iterations: u64,
    pub packed_small_first_extract_ns: u64,
    pub packed_small_middle_extract_ns: u64,
    pub packed_small_last_extract_ns: u64,
    pub packed_small_latest_head_ts_micros: i64,
    pub packed_small_dirty_1d_pages: u64,
    pub packed_small_dirty_1d_shards: u64,
    pub packed_small_rewrite_1d_bytes: u64,
    pub packed_small_dirty_7d_pages: u64,
    pub packed_small_dirty_7d_shards: u64,
    pub packed_small_rewrite_7d_bytes: u64,
}

/// Read-only accounting from the experimental packed-current-text layout.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PackedF0StorageStats {
    pub pages: u64,
    pub small_pages: u64,
    pub big_pages: u64,
    pub small_raw_bytes: u64,
    pub all_standalone_frame_bytes: u64,
    pub replaced_standalone_frame_bytes: u64,
    pub history_raw_bytes: u64,
    pub history_compressed_bytes: u64,
    pub history_frame_bytes: u64,
    pub packed_compressed_bytes: u64,
    pub packed_file_bytes: u64,
    pub materialized_shards: u64,
    pub p50_compressed_shard_bytes: u64,
    pub p95_compressed_shard_bytes: u64,
    pub p99_compressed_shard_bytes: u64,
    pub max_compressed_shard_bytes: u64,
    pub oversized_1m_shards: u64,
    pub mean_scan_bytes: u64,
    pub p50_scan_bytes: u64,
    pub p95_scan_bytes: u64,
    pub p99_scan_bytes: u64,
    pub max_scan_bytes: u64,
    pub benchmark_pages: u64,
    pub benchmark_raw_bytes: u64,
    pub benchmark_compressed_bytes: u64,
    pub benchmark_iterations: u64,
    pub first_extract_ns: u64,
    pub middle_extract_ns: u64,
    pub last_extract_ns: u64,
    pub hysteresis_transition_pages: [u64; 5],
    pub hysteresis_transitions: [u64; 5],
    pub hysteresis_current_small_pages: [u64; 5],
}

impl Iterator for HistoryIter {
    type Item = Result<HistoryEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// The per-dbname mirror. One process at a time per `root`.
pub struct Instance {
    pub(crate) root: PathBuf,
    /// `Arc` so the streaming [`HistoryIter`] (and its lazy `fetch_text`
    /// closures) can hold the handles across calls without borrowing
    /// the `Instance` — a history walk is record-at-a-time, not a
    /// snapshot of a decompressed frame or chain.
    pub(crate) inner: Arc<Mutex<InstanceInner>>,
    /// Read/import page-id clip — always the depot's 2^40 sanity
    /// ceiling (see `open`); ids below it are covered by index growth.
    pub(crate) max_chain_id: u64,
    pub(crate) f1_seal_threshold_bytes: u64,
    pub(crate) title_shard_count: AtomicU32,
    pub(crate) title_shard_target_bytes: u64,
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
    pub(crate) revision_dictionaries: crate::frames::DictionaryStore,
    /// Append-only logical lane for conflicting immutable revision
    /// records. One correction chain per page id.
    pub(crate) corrections: Depot,
    pub(crate) titles: Pool,
    pub(crate) title_slots: crate::title_slots::TitleSlots,
    pub(crate) page_titles: crate::title_slots::PageTitleSlots,
    pub(crate) pending_title_intents: usize,
    pub(crate) conn: Connection,
    /// The root's flock, held for the instance's lifetime.
    pub(crate) _lock: std::fs::File,
}

impl InstanceInner {
    /// Dense title-dictionary ids whose pool bytes equal `normalized`
    /// — the exact-title read primitive. It walks one small fnv-picked shard.
    pub(crate) fn title_ids(&self, shard_count: u32, normalized: &[u8]) -> Result<Vec<u64>> {
        crate::titles::lookup_ids(&self.titles, shard_count, normalized)
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

        // meta.db FIRST: the titles pool below must open with the
        // store's persisted shard count, so resolve it before any pool
        // file exists to get wrong.
        let mut conn = Connection::open(cfg.root.join("meta.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        for stmt in META_DDL {
            conn.execute(stmt, [])?;
        }
        // The effective shard count: persisted at creation, validated
        // against an explicit config.
        let title_shard_count =
            resolve_title_shard_count(&conn, &cfg.root, cfg.title_shard_count, true)?;

        // Depot — root forced to <root>/depot/ per SPEC.
        let mut depot_cfg = cfg.depot;
        depot_cfg.root = cfg.root.join("depot");
        std::fs::create_dir_all(&depot_cfg.root)?;
        let correction_cfg = DepotConfig {
            root: cfg.root.join("corrections"),
            max_chain_id: depot_cfg.max_chain_id,
            file_size_threshold: depot_cfg.file_size_threshold,
            eviction_dead_ratio: depot_cfg.eviction_dead_ratio,
        };
        let depot = Depot::open(depot_cfg)?;
        let corrections = Depot::open(correction_cfg)?;
        let revision_dictionaries = crate::frames::DictionaryStore::open(&cfg.root)?;

        // Title pool — <root>/titles/.
        let title_generation = persisted_title_pool_generation(&conn)?;
        let titles_dir = title_pool_dir(&cfg.root, title_generation);
        let titles = Pool::open(
            &titles_dir,
            PoolConfig {
                shard_count: title_shard_count,
                seal_threshold_bytes: cfg.title_seal_threshold_bytes,
            },
            None,
        )?;
        // Only a writer collects unselected immutable generations. The
        // selected pool is already open and is explicitly protected; this
        // also removes a complete-or-partial generation left by a crash
        // before the SQLite generation switch.
        gc_stale_title_generations(&cfg.root, title_generation)?;
        if conn
            .query_row("SELECT generation FROM title_slot_state WHERE singleton=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .is_err()
        {
            let tx = conn.transaction()?;
            let builder =
                crate::title_slots::TitleSlotGenerations::prepare_snapshot(&cfg.root, 1, &tx)?;
            builder.finish()?.commit()?;
            crate::title_slots::TitleSlotGenerations::select(&tx, 1)?;
            tx.commit()?;
        }
        recover_title_slot_intent(&cfg.root, &mut conn)?;
        let title_slots =
            crate::title_slots::TitleSlotGenerations::open_selected(&cfg.root, &conn)?;
        let page_titles =
            crate::title_slots::TitleSlotGenerations::open_selected_page_titles(&cfg.root, &conn)?;
        let selected_slots = crate::title_slots::TitleSlotGenerations::selected(&conn)?;
        crate::title_slots::TitleSlotGenerations::collect_unselected(&cfg.root, selected_slots)?;

        Ok(Self {
            root: cfg.root.clone(),
            inner: Arc::new(Mutex::new(InstanceInner {
                depot,
                revision_dictionaries,
                corrections,
                titles,
                title_slots,
                page_titles,
                pending_title_intents: 0,
                conn,
                _lock: lock,
            })),
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
            title_shard_count: AtomicU32::new(title_shard_count),
            title_shard_target_bytes: cfg.title_seal_threshold_bytes,
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
    /// Never creates or migrates anything: a non-instance or incomplete
    /// root is a loud error, and every write API refuses with
    /// [`Error::ReadOnly`].
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

        // No DDL, no pragma writes, no ALTERs: the writer created the
        // schema; this connection only ever SELECTs.
        let conn = Connection::open_with_flags(
            cfg.root.join("meta.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, // WAL recovery may write; we never do
        )?;
        // The store's shard count, NOT the config's assumption: exact
        // lookups route by fnv % count, so a reader guessing wrong
        // would silently miss titles. Derived from the flag persisted
        // at creation; never backfilled here (read-only).
        let title_shard_count =
            resolve_title_shard_count(&conn, &cfg.root, cfg.title_shard_count, false)?;

        let mut depot_cfg = cfg.depot;
        depot_cfg.root = cfg.root.join("depot");
        let correction_cfg = DepotConfig {
            root: cfg.root.join("corrections"),
            max_chain_id: depot_cfg.max_chain_id,
            file_size_threshold: depot_cfg.file_size_threshold,
            eviction_dead_ratio: depot_cfg.eviction_dead_ratio,
        };
        let depot = Depot::open(depot_cfg)?;
        let corrections = Depot::open(correction_cfg)?;
        let revision_dictionaries = crate::frames::DictionaryStore::open_existing(&cfg.root);
        let pending_title_intents: i64 =
            conn.query_row("SELECT COUNT(*) FROM title_slot_intent", [], |row| row.get(0))?;
        if pending_title_intents != 0 {
            return Err(Error::Corrupt(
                "pending title-slot intent requires writable recovery",
            ));
        }
        let title_generation = persisted_title_pool_generation(&conn)?;
        let titles = Pool::open(
            &title_pool_dir(&cfg.root, title_generation),
            PoolConfig {
                shard_count: title_shard_count,
                seal_threshold_bytes: cfg.title_seal_threshold_bytes,
            },
            None,
        )?;
        let title_slots =
            crate::title_slots::TitleSlotGenerations::open_selected(&cfg.root, &conn)?;
        let page_titles =
            crate::title_slots::TitleSlotGenerations::open_selected_page_titles(&cfg.root, &conn)?;

        Ok(Self {
            root: cfg.root.clone(),
            inner: Arc::new(Mutex::new(InstanceInner {
                depot,
                revision_dictionaries,
                corrections,
                titles,
                title_slots,
                page_titles,
                pending_title_intents: 0,
                conn,
                _lock: lock,
            })),
            read_only: true,
            max_chain_id: wikimak_depot::CHAIN_ID_CEILING,
            f1_seal_threshold_bytes: if cfg.f1_seal_threshold_bytes == 0 {
                256 * 1024
            } else {
                cfg.f1_seal_threshold_bytes
            },
            title_shard_count: AtomicU32::new(title_shard_count),
            title_shard_target_bytes: cfg.title_seal_threshold_bytes,
            dbname: cfg.dbname,
        })
    }

    /// On-disk root for download staging and short-lived read-side opens.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Import one `PageStream` into the instance. Per-page atomic.
    /// Returns counters describing the import.
    pub fn import<R: Read>(&self, stream: &mut PageStream<R>) -> Result<ImportStats> {
        if self.read_only {
            return Err(Error::ReadOnly("import"));
        }
        do_import(self, stream)
    }

    /// Read the current head revision metadata for `page_id`.
    ///
    /// The canonical f0 frame is always the highest revision id and is
    /// therefore the current-head fast path. Time-travel reads inspect the
    /// remaining immutable revision records and choose the timestamp/revision
    /// maximum at the requested instant.
    pub fn page_head(&self, page_id: u64) -> Result<Option<RevisionMeta>> {
        Ok(self.revision_query(page_id, None, false)?.map(|(m, _)| m))
    }

    /// Read the current head revision's text bytes (UTF-8) for
    /// `page_id` — the newest revision by timestamp (see [`page_head`]).
    /// `Ok(None)` if no such page.
    pub fn page_head_text(&self, page_id: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.revision_query(page_id, None, true)?.and_then(|(_, t)| t))
    }

    /// Conflicting immutable revision occurrences retained for
    /// archaeology. The canonical stored revision remains unchanged.
    pub fn revision_corrections(&self, page_id: u64) -> Result<Vec<RevisionCorrection>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        Ok(crate::revision_merge::read_corrections(&g.corrections, page_id)?
            .into_iter()
            .map(|event| RevisionCorrection {
                revision_id: event.revision_id,
                occurrence: event.occurrence,
                incoming_record: event.incoming_record,
            })
            .collect())
    }

    /// Iterate all revisions of `page_id`, newest-first (chain order).
    ///
    /// STREAMING: the iterator incrementally decodes one record at a
    /// time. It retains the compressed frame, zstd's decoder window,
    /// the prior frame's refPrefix record, and the current record, but
    /// never materializes the whole decompressed f1/cold payload.
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

    /// Finalize a seed written by an importer that could not pretrain:
    /// deterministically sample complete current f0 records, train one
    /// 800-KiB per-instance revision dictionary (scaled down only for a
    /// small corpus), publish it durably, and repack f0 only. A rerun
    /// resumes a partially completed repack using the already-active
    /// dictionary; it never trains a successor. Seekable archive imports
    /// use `prepare_seed_revision_dictionary` before their first write.
    pub fn finalize_seed_revision_dictionary(&self) -> Result<RevisionDictionaryStats> {
        if self.read_only {
            return Err(Error::ReadOnly("finalize_seed_revision_dictionary"));
        }
        self.repack_revision_dictionary_inner(false, None)
    }

    pub(crate) fn prepare_seed_revision_dictionary(
        &self,
        samples: &[Vec<u8>],
    ) -> Result<RevisionDictionaryStats> {
        if self.read_only {
            return Err(Error::ReadOnly("prepare_seed_revision_dictionary"));
        }
        let g = self.inner.lock().expect("instance mutex poisoned");
        let mut stats = RevisionDictionaryStats::default();
        let total = samples.iter().try_fold(0usize, |sum, sample| {
            sum.checked_add(sample.len())
                .ok_or(Error::Corrupt("revision dictionary sample size overflow"))
        })?;
        stats.samples = samples.len() as u64;
        stats.sample_bytes = total as u64;
        if samples.len() < REVISION_MIN_SAMPLES || total < REVISION_MIN_SAMPLE_BYTES {
            return Ok(stats);
        }
        let capacity = REVISION_DICTIONARY_BYTES.min(total / REVISION_DICTIONARY_SAMPLE_RATIO);
        let dictionary = crate::frames::train_dictionary(samples, capacity)?;
        let id = g.revision_dictionaries.persist("revision", &dictionary)?;
        g.revision_dictionaries.activate("revision", id)?;
        stats.dictionary_id = Some(id);
        stats.dictionary_bytes = dictionary.len() as u64;
        stats.trained = true;
        Ok(stats)
    }

    pub(crate) fn has_active_revision_dictionary(&self) -> Result<bool> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        Ok(g.revision_dictionaries.current("revision")?.is_some())
    }

    /// Explicitly train a successor dictionary from the current complete
    /// page heads and recompress every live f0 frame with it. Dictionary
    /// files are immutable and frames carry their native dictionary id,
    /// so interruption leaves a readable mixed generation; retrying trains
    /// the same deterministic dictionary and resumes the repack.
    pub fn retrain_revision_dictionary(&self) -> Result<RevisionDictionaryStats> {
        if self.read_only {
            return Err(Error::ReadOnly("retrain_revision_dictionary"));
        }
        self.repack_revision_dictionary_inner(true, None)
    }

    /// Read-only experiment: split every revision into a metadata prefix
    /// and bare wikitext, train independent dictionaries for metadata and
    /// head text, and account for a layout with one metadata frame, one
    /// text f0, and at most one text-history frame per page. No bytes are
    /// written to the instance.
    pub fn experiment_split_revision_storage(
        &self,
        workers: usize,
        packed_shards: usize,
    ) -> Result<SplitRevisionStorageStats> {
        if packed_shards == 0 || !packed_shards.is_power_of_two() {
            return Err(Error::Corrupt("packed-small shard count must be a power of two"));
        }
        let (ids, text_samples, metadata_samples, combined_samples, mut totals) = {
            let g = self.inner.lock().expect("instance mutex poisoned");
            let ids = occupied_chain_ids(&g.depot);
            let text_samples =
                revision_dictionary_samples_with_limit(&g, REVISION_SAMPLE_COUNT, true)?;
            let metadata_samples = revision_dictionary_samples_with_limit(
                &g,
                REVISION_METADATA_SAMPLE_COUNT,
                false,
            )?;
            let combined_samples =
                combined_revision_dictionary_samples_with_limit(&g, REVISION_SAMPLE_COUNT)?;
            let mut totals = SplitRevisionStorageStats::default();
            for (tier, _, len, dead) in g.depot.tier_stats() {
                match tier {
                    "f0" => totals.current_live_f0_bytes += len - dead,
                    "f1" => totals.current_live_f1_bytes += len - dead,
                    _ => return Err(Error::Corrupt("unknown depot tier")),
                }
            }
            let (cold_len, cold_dead) = g.depot.cold_stats();
            totals.current_live_cold_bytes = cold_len - cold_dead;
            (ids, text_samples, metadata_samples, combined_samples, totals)
        };

        let text_sample_bytes = sample_bytes(&text_samples)?;
        let metadata_sample_bytes = sample_bytes(&metadata_samples)?;
        let combined_sample_bytes = sample_bytes(&combined_samples)?;
        let text_capacity = experimental_dictionary_capacity(text_samples.len(), text_sample_bytes)?;
        let metadata_capacity =
            experimental_dictionary_capacity(metadata_samples.len(), metadata_sample_bytes)?;
        let combined_capacity =
            experimental_dictionary_capacity(combined_samples.len(), combined_sample_bytes)?;
        let text_dictionary = crate::frames::train_dictionary(&text_samples, text_capacity)?;
        let metadata_dictionary =
            crate::frames::train_dictionary(&metadata_samples, metadata_capacity)?;
        let combined_dictionary =
            crate::frames::train_dictionary(&combined_samples, combined_capacity)?;
        totals.text_dictionary_id =
            zstd::zstd_safe::get_dict_id_from_dict(&text_dictionary).map(u32::from)
                .ok_or(Error::InvalidDictionary)?;
        totals.text_dictionary_bytes = text_dictionary.len() as u64;
        totals.text_samples = text_samples.len() as u64;
        totals.text_sample_bytes = text_sample_bytes as u64;
        totals.metadata_dictionary_id =
            zstd::zstd_safe::get_dict_id_from_dict(&metadata_dictionary).map(u32::from)
                .ok_or(Error::InvalidDictionary)?;
        totals.metadata_dictionary_bytes = metadata_dictionary.len() as u64;
        totals.metadata_samples = metadata_samples.len() as u64;
        totals.metadata_sample_bytes = metadata_sample_bytes as u64;
        totals.combined_dictionary_id =
            zstd::zstd_safe::get_dict_id_from_dict(&combined_dictionary).map(u32::from)
                .ok_or(Error::InvalidDictionary)?;
        totals.combined_dictionary_bytes = combined_dictionary.len() as u64;
        totals.combined_samples = combined_samples.len() as u64;
        totals.combined_sample_bytes = combined_sample_bytes as u64;

        let ids = std::sync::Arc::new(ids);
        let text_dictionary = std::sync::Arc::new(text_dictionary);
        let metadata_dictionary = std::sync::Arc::new(metadata_dictionary);
        let combined_dictionary = std::sync::Arc::new(combined_dictionary);
        let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let packed_small = std::sync::Arc::new(
            (0..packed_shards)
                .map(|_| std::sync::Mutex::new(PackedSmallShard::default()))
                .collect::<Vec<_>>(),
        );
        let root = self.root.clone();
        let worker_count = workers.max(1).min(ids.len().max(1));
        let partials = std::thread::scope(|scope| -> Result<Vec<SplitRevisionStorageStats>> {
            let mut handles = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                let ids = std::sync::Arc::clone(&ids);
                let text_dictionary = std::sync::Arc::clone(&text_dictionary);
                let metadata_dictionary = std::sync::Arc::clone(&metadata_dictionary);
                let combined_dictionary = std::sync::Arc::clone(&combined_dictionary);
                let next = std::sync::Arc::clone(&next);
                let packed_small = std::sync::Arc::clone(&packed_small);
                let root = root.clone();
                handles.push(scope.spawn(move || {
                    split_revision_storage_worker(
                        root,
                        &ids,
                        &next,
                        &text_dictionary,
                        &metadata_dictionary,
                        &combined_dictionary,
                        Some(packed_small.as_slice()),
                    )
                }));
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| Error::Corrupt("split-storage experiment worker panicked"))?
                })
                .collect()
        })?;
        for partial in partials {
            add_split_revision_stats(&mut totals, &partial);
        }
        finish_packed_small_stats(&mut totals, packed_small.as_slice(), &combined_dictionary)?;
        Ok(totals)
    }

    /// Read-only experiment: pack qualifying current-revision texts into a
    /// separate hash-sharded collection while leaving large heads as direct
    /// standalone f0 frames. Historical text sizes are inspected only to
    /// measure small/large migration hysteresis.
    pub fn experiment_packed_f0(
        &self,
        workers: usize,
        packed_shards: usize,
    ) -> Result<PackedF0StorageStats> {
        if packed_shards == 0 || !packed_shards.is_power_of_two() {
            return Err(Error::Corrupt("packed-f0 shard count must be a power of two"));
        }
        let (ids, samples) = {
            let g = self.inner.lock().expect("instance mutex poisoned");
            (
                occupied_chain_ids(&g.depot),
                combined_revision_dictionary_samples_with_limit(&g, REVISION_SAMPLE_COUNT)?,
            )
        };
        let sample_bytes = sample_bytes(&samples)?;
        let capacity = experimental_dictionary_capacity(samples.len(), sample_bytes)?;
        let dictionary = std::sync::Arc::new(crate::frames::train_dictionary(&samples, capacity)?);
        let ids = std::sync::Arc::new(ids);
        let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let packed = std::sync::Arc::new(
            (0..packed_shards)
                .map(|_| std::sync::Mutex::new(PackedSmallShard::default()))
                .collect::<Vec<_>>(),
        );
        let root = self.root.clone();
        let worker_count = workers.max(1).min(ids.len().max(1));
        let partials = std::thread::scope(|scope| -> Result<Vec<PackedF0StorageStats>> {
            let mut handles = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                let ids = std::sync::Arc::clone(&ids);
                let next = std::sync::Arc::clone(&next);
                let dictionary = std::sync::Arc::clone(&dictionary);
                let packed = std::sync::Arc::clone(&packed);
                let root = root.clone();
                handles.push(scope.spawn(move || {
                    packed_f0_worker(
                        root,
                        &ids,
                        &next,
                        &dictionary,
                        packed.as_slice(),
                    )
                }));
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| Error::Corrupt("packed-f0 experiment worker panicked"))?
                })
                .collect()
        })?;
        let mut totals = PackedF0StorageStats::default();
        for partial in partials {
            add_packed_f0_stats(&mut totals, &partial);
        }
        finish_packed_f0_stats(&mut totals, packed.as_slice(), &dictionary)?;
        Ok(totals)
    }

    fn repack_revision_dictionary_inner(
        &self,
        retrain: bool,
        repack_limit: Option<usize>,
    ) -> Result<RevisionDictionaryStats> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let mut stats = RevisionDictionaryStats::default();
        let active = g.revision_dictionaries.current("revision")?;
        let dict_id = if retrain || active.is_none() {
            let samples = revision_dictionary_samples(&g)?;
            let total = samples.iter().try_fold(0usize, |sum, sample| {
                sum.checked_add(sample.len())
                    .ok_or(Error::Corrupt("revision dictionary sample size overflow"))
            })?;
            stats.samples = samples.len() as u64;
            stats.sample_bytes = total as u64;
            if samples.len() < REVISION_MIN_SAMPLES || total < REVISION_MIN_SAMPLE_BYTES {
                stats.dictionary_id = active;
                return Ok(stats);
            }
            let capacity =
                REVISION_DICTIONARY_BYTES.min(total / REVISION_DICTIONARY_SAMPLE_RATIO);
            let dictionary = crate::frames::train_dictionary(&samples, capacity)?;
            let id = g.revision_dictionaries.persist("revision", &dictionary)?;
            g.revision_dictionaries.activate("revision", id)?;
            stats.trained = true;
            stats.dictionary_bytes = dictionary.len() as u64;
            id
        } else {
            active.expect("checked above")
        };
        stats.dictionary_id = Some(dict_id);
        let dictionary = g.revision_dictionaries.load("revision", dict_id)?;
        stats.dictionary_bytes = dictionary.len() as u64;

        let mut after = None;
        'pages: loop {
            let ids = g
                .depot
                .occupied_chain_ids_after(after, OCCUPIED_ID_BATCH);
            if ids.is_empty() {
                break;
            }
            for id in ids.iter().copied() {
                after = Some(id);
                let old = g.depot.read_f0(id)?;
                if crate::frames::frame_dictionary_id(&old) == Some(dict_id) {
                    continue;
                }
                let raw = crate::frames::decompress_head(
                    &old,
                    &g.revision_dictionaries,
                    "revision",
                )?;
                let replacement =
                    crate::frames::compress_head_dictionary(&raw, &dictionary)?;
                g.depot.replace_f0(id, &replacement)?;
                stats.heads_repacked += 1;
                if repack_limit.is_some_and(|limit| stats.heads_repacked as usize >= limit) {
                    break 'pages;
                }
            }
        }
        g.depot.flush()?;
        if repack_limit.is_none() {
            g.depot.collect_f0()?;
        }
        Ok(stats)
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
    /// page through its fixed-width current slot. Memory is bounded by the
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

        let mut out: Vec<(u64, String)> = Vec::new();
        let mut window: Option<crate::titles::Candidate> = None;
        loop {
            let pass = crate::titles::scan_candidates(
                &g.titles,
                self.title_shard_count.load(Ordering::Relaxed),
                &matches,
                limit - out.len(),
                window.as_ref(),
            )?;
            for cand in &pass.candidates {
                if let Some(page_id) =
                    effective_title_binding(&g, cand.1)?.and_then(|b| b.page_id())
                {
                    out.push((page_id as u64, String::from_utf8_lossy(&cand.0).into_owned()));
                    if out.len() == limit {
                        break;
                    }
                }
            }
            match (out.len() >= limit, pass.next_window) {
                (true, _) | (false, None) => break,
                (false, Some(bound)) => window = Some(bound),
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
        let normalized = crate::titles::normalize_title(normalized);
        let g = self.inner.lock().expect("instance mutex poisoned");
        let ids = g.title_ids(self.title_shard_count.load(Ordering::Relaxed), &normalized)?;
        let mut best: Option<u64> = None;
        for id in &ids {
            if let Some(pid) =
                effective_title_binding(&g, *id)?.and_then(|binding| binding.page_id())
            {
                best = Some(best.map_or(pid as u64, |b| b.min(pid as u64)));
            }
        }
        Ok(best)
    }

    /// Strpool per-shard walk counters for the titles pool —
    /// instrumentation for the title-read acceptance tests (an exact
    /// lookup touches ONE shard; a substring search touches all).
    pub fn title_scan_counts(&self) -> Vec<u64> {
        self.inner.lock().expect("instance mutex poisoned").titles.scan_counts()
    }

    /// The EFFECTIVE titles-pool shard count this open resolved —
    /// persisted at creation, derived (or validated) at every open.
    /// Tests pin that a read-side open of an 8-shard store routes by 8.
    pub fn title_shard_count(&self) -> u32 {
        self.title_shard_count.load(Ordering::Relaxed)
    }

    /// The CURRENT title of `page_id` — the reverse of the exact-title
    /// lookup, and the engine's attach-by-id name recovery. The reverse
    /// flat file gives the dense title id in O(1), then `Pool::get`
    /// decodes only that id's bounded shard. No pool-wide listing occurs.
    /// `Ok(None)` means the page has no current title.
    pub fn page_current_title(&self, page_id: u64) -> Result<Option<String>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let page_id: u32 = page_id.try_into().map_err(|_| Error::Corrupt("page id exceeds u32"))?;
        let title = match effective_page_title_id(&g, page_id)? {
            Some(title_id) => g.titles.get(title_id)?,
            None => None,
        };
        Ok(title.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    // --- asof-τ read API (browsing plan §2, the wayback contract) ---
    //
    // Import, MWH reconstruction, and reads share title normalization:
    // underscores become spaces and whitespace is collapsed.

    /// Resolve a title to its page id AS OF `ts_micros` (unix micros).
    ///
    /// `None` τ uses the current flat slot. `Some(τ)` uses that slot when
    /// τ falls in its continuously-valid range, otherwise the sparse
    /// older-interval store. A deletion or pre-creation gap returns `None`.
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
        let key = crate::titles::normalize_title(title.as_bytes());
        let g = self.inner.lock().expect("instance mutex poisoned");
        let ids = g.title_ids(self.title_shard_count.load(Ordering::Relaxed), &key)?;
        let seconds: u32 = ts
            .div_euclid(1_000_000)
            .try_into()
            .map_err(|_| Error::Corrupt("title timestamp outside u32 seconds"))?;
        for id in ids {
            if let Some(binding) = effective_title_binding(&g, id)? {
                if seconds >= binding.valid_since {
                    if let Some(page_id) = binding.page_id() {
                        return Ok(Some(page_id as u64));
                    }
                    continue;
                }
            }
            let page_id: Option<i64> = g
                .conn
                .query_row(
                    "SELECT page_id FROM title_interval_overflow
                     WHERE title_id=?1 AND start_s<=?2 AND end_s>?2
                     ORDER BY start_s DESC LIMIT 1",
                    rusqlite::params![id as i64, seconds],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(ignore_no_rows)?;
            if let Some(page_id) = page_id.filter(|page_id| *page_id != 0) {
                return Ok(Some(page_id as u64));
            }
        }
        Ok(None)
    }

    /// Newest revision of `page_id` with timestamp ≤ `ts_micros`.
    ///
    /// `None` τ → the newest revision overall. `Some(τ)` → the newest
    /// revision whose timestamp is ≤ τ; `Ok(None)` when every revision is
    /// newer than τ (the page did not yet exist at τ).
    ///
    /// The authoritative depot chain is scanned and the answer is
    /// `argmax(timestamp, rev_id)` among qualifying records. Revision
    /// identity/order in storage is by revision id; timestamp remains
    /// record content and is not duplicated in SQLite.
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
        Ok(find_revision(
            &g.depot,
            &g.revision_dictionaries,
            page_id,
            rev_id,
            true,
        )?
        .and_then(|(_, t)| t))
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
        if ts_micros.is_none() {
            let mut walk = WalkState::new(page_id);
            let Some(record) = walk.next_record(&g.depot, &g.revision_dictionaries)? else {
                return Ok(None);
            };
            let (meta, text) = crate::revision::decode_revision_view(record)?;
            return Ok(Some((meta, want_text.then(|| text.to_vec()))));
        }
        scan_best(
            &g.depot,
            &g.revision_dictionaries,
            page_id,
            ts_micros,
            want_text,
            &mut |_, _| Ok(()),
        )
    }

    /// Existence of `title` at τ — the red-link / `#ifexist` fast path.
    ///
    /// Title metadata only, with no revision-frame decode: resolves through
    /// the same current-slot/sparse-overflow path as
    /// [`Instance::page_id_by_title_at`].
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

    pub fn has_seen_parts(&self) -> Result<bool> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        Ok(g.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM parts_seen LIMIT 1)",
            [],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    /// Digest-aware watermark lookup used by network sync. A publisher-side
    /// replacement under the same filename must not inherit the old part's
    /// skip marker.
    pub fn part_seen_with_digest(&self, filename: &str, digest: Option<&str>) -> Result<bool> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let stored: Option<Option<String>> = g
            .conn
            .query_row(
                "SELECT sha256 FROM parts_seen WHERE part_filename = ?1",
                [filename],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(ignore_no_rows)?;
        Ok(match (stored, digest) {
            (None, _) => false,
            (Some(Some(stored)), Some(digest)) => stored.eq_ignore_ascii_case(digest),
            // Preserve legacy/no-checksum watermarks only when the current
            // manifest also provides no checksum.
            (Some(None), None) => true,
            _ => false,
        })
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

    pub fn sync_state(&self, key: &str) -> Result<Option<String>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        g.conn
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |row| row.get(0))
            .map(Some)
            .or_else(ignore_no_rows)
    }

    pub fn set_sync_state(&self, key: &str, value: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly("set_sync_state"));
        }
        let g = self.inner.lock().expect("instance mutex poisoned");
        g.conn.execute(
            "INSERT OR REPLACE INTO sync_state(key, value) VALUES(?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub fn page_actions(&self, page_id: u64) -> Result<Vec<PageAction>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let mut statement = g.conn.prepare(
            "SELECT event_type,event_timestamp,event_comment,actor_name,
                    title_historical,title_current
             FROM page_actions WHERE page_id = ?1
             ORDER BY event_timestamp DESC",
        )?;
        let rows = statement.query_map([page_id], |row| {
            Ok(PageAction {
                event_type: row.get(0)?,
                timestamp: row.get(1)?,
                comment: row.get(2)?,
                actor: row.get(3)?,
                historical_title: row.get(4)?,
                current_title: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn archive_page_ids_after(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<u64>> {
        let after = after.unwrap_or(0);
        let g = self.inner.lock().expect("instance mutex poisoned");
        let mut ids = std::collections::BTreeSet::new();
        ids.extend(
            g.depot
                .occupied_chain_ids_after((after != 0).then_some(after), limit),
        );
        let mut statement = g.conn.prepare(
            "SELECT DISTINCT page_id
             FROM page_actions
             WHERE page_id IS NOT NULL AND page_id > ?1
             ORDER BY page_id
             LIMIT ?2",
        )?;
        let rows = statement.query_map(
            rusqlite::params![after as i64, limit as i64],
            |row| row.get::<_, i64>(0),
        )?;
        for row in rows {
            let id = row?;
            if id > 0 {
                ids.insert(id as u64);
            }
        }
        Ok(ids.into_iter().take(limit).collect())
    }

    pub(crate) fn archive_page_actions(
        &self,
        page_id: u64,
    ) -> Result<Vec<(crate::archive::PageActionRecord, String)>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let sql = if page_id == 0 {
            "SELECT event_log_id,source_ordinal,event_type,event_timestamp,
                    event_comment,actor_id,actor_name,title_historical,
                    namespace_historical,page_deleted
             FROM page_actions WHERE page_id IS NULL
             ORDER BY event_timestamp DESC,source_ordinal DESC"
        } else {
            "SELECT event_log_id,source_ordinal,event_type,event_timestamp,
                    event_comment,actor_id,actor_name,title_historical,
                    namespace_historical,page_deleted
             FROM page_actions WHERE page_id = ?1
             ORDER BY event_timestamp DESC,source_ordinal DESC"
        };
        let mut statement = g.conn.prepare(sql)?;
        let map = |row: &rusqlite::Row<'_>| {
            let source_ordinal = row.get::<_, i64>(1)?;
            if source_ordinal < 0 {
                return Err(rusqlite::Error::IntegralValueOutOfRange(1, source_ordinal));
            }
            let event_type: String = row.get(2)?;
            let actor_id: Option<i64> = row.get(5)?;
            let actor_name: String = row.get(6)?;
            let log_id: Option<i64> = row.get(0)?;
            Ok((
                crate::archive::PageActionRecord {
                    log_id: log_id.and_then(|id| u64::try_from(id).ok()),
                    tie_sequence: source_ordinal as u64,
                    kind: crate::archive::PageActionKind::from_name(&event_type),
                    performer: crate::archive::PerformerRecord {
                        local_user_id: actor_id.and_then(|id| u64::try_from(id).ok()),
                        central_user_id: None,
                        historical_name: (!actor_name.is_empty()).then_some(actor_name),
                        account_class: crate::archive::AccountClass::Unknown,
                    },
                    comment: row.get(4)?,
                    title_at_event: row.get(7)?,
                    namespace_at_event: row.get(8)?,
                    resulting_deleted: Some(row.get::<_, i64>(9)? != 0),
                },
                row.get(3)?,
            ))
        };
        let rows = if page_id == 0 {
            statement.query_map([], map)?
        } else {
            statement.query_map([page_id as i64], map)?
        };
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn archive_revision_visibility(
        &self,
        page_id: u64,
    ) -> Result<Vec<(u64, crate::archive::RevisionVisibilityRecord)>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        let mut statement = g.conn.prepare(
            "SELECT revision_id,source_partition,deleted_parts,
                    parts_are_suppressed,deleted_by_page_deletion,
                    page_deletion_timestamp
             FROM revision_visibility WHERE page_id = ?1",
        )?;
        let rows = statement.query_map([page_id as i64], |row| {
            let revision_id = row.get::<_, i64>(0)?;
            if revision_id < 0 {
                return Err(rusqlite::Error::IntegralValueOutOfRange(0, revision_id));
            }
            Ok((
                revision_id as u64,
                crate::archive::RevisionVisibilityRecord {
                    deleted_parts: {
                        let parts: String = row.get(2)?;
                        parts.split(',').fold(0, |bits, part| {
                            bits | match part.trim() {
                                "text" => 1,
                                "comment" => 2,
                                "user" => 4,
                                _ => 0,
                            }
                        })
                    },
                    parts_are_suppressed: row.get::<_, i64>(3)? != 0,
                    deleted_by_page_deletion: row.get::<_, i64>(4)? != 0,
                    page_deletion_timestamp_micros: {
                        let timestamp: String = row.get(5)?;
                        if timestamp.is_empty() {
                            None
                        } else {
                            chrono::NaiveDateTime::parse_from_str(
                                &timestamp,
                                "%Y-%m-%d %H:%M:%S%.f",
                            )
                            .ok()
                            .map(|timestamp| timestamp.and_utc().timestamp_micros())
                        }
                    },
                },
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn revision_visibility(&self, revision_id: u64) -> Result<Option<RevisionVisibility>> {
        let g = self.inner.lock().expect("instance mutex poisoned");
        g.conn
            .query_row(
                "SELECT deleted_parts,parts_are_suppressed,
                        deleted_by_page_deletion,page_deletion_timestamp
                 FROM revision_visibility WHERE revision_id = ?1",
                [revision_id],
                |row| {
                    Ok(RevisionVisibility {
                        deleted_parts: row.get(0)?,
                        parts_are_suppressed: row.get::<_, i64>(1)? != 0,
                        deleted_by_page_deletion: row.get::<_, i64>(2)? != 0,
                        page_deletion_timestamp: row.get(3)?,
                    })
                },
            )
            .map(Some)
            .or_else(ignore_no_rows)
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
        g.corrections.collect()?;
        Ok(())
    }

    /// Flush depot + strpool + sqlite to durable storage.
    pub fn flush(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly("flush"));
        }
        let mut g = self.inner.lock().expect("instance mutex poisoned");
        finish_title_slot_intent(&self.root, &mut g)?;
        g.depot.flush()?;
        g.corrections.flush()?;
        for sid in 0..self.title_shard_count.load(Ordering::Relaxed) {
            g.titles.maybe_seal(sid)?;
            g.titles.flush(sid)?;
        }
        while title_pool_is_oversized(
            &g.titles,
            self.title_shard_count.load(Ordering::Relaxed),
            self.title_shard_target_bytes,
        )? {
            self.reshard_titles_once(&mut g)?;
        }
        // sqlite WAL checkpoint — commit boundaries flushed by the
        // per-page transactions; the checkpoint pushes WAL pages to the
        // main db file.
        g.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .map_err(Error::Sqlite)?;
        Ok(())
    }

    /// Rebuild the immutable title-pool generation at twice the current
    /// shard count, then atomically switch every persisted dense id and the
    /// generation/count flags in one SQLite commit. The new directory is
    /// durable before the transaction; a crash before commit leaves the old
    /// generation selected, and a crash after commit selects the complete
    /// new one.
    pub(crate) fn maintain_title_shard(
        &self,
        g: &mut InstanceInner,
        shard_id: u32,
    ) -> Result<()> {
        if self.title_shard_target_bytes == 0
            || g.titles.shard_file_size(shard_id)? <= self.title_shard_target_bytes
        {
            return Ok(());
        }
        g.titles.maybe_seal(shard_id)?;
        g.titles.flush(shard_id)?;
        finish_title_slot_intent(&self.root, g)?;
        while title_pool_is_oversized(
            &g.titles,
            self.title_shard_count.load(Ordering::Relaxed),
            self.title_shard_target_bytes,
        )? {
            self.reshard_titles_once(g)?;
        }
        Ok(())
    }

    fn reshard_titles_once(&self, g: &mut InstanceInner) -> Result<()> {
        let old_count = self.title_shard_count.load(Ordering::Relaxed);
        let new_count = old_count.checked_mul(2).ok_or(Error::Corrupt("title shard count overflow"))?;
        let generation = next_title_pool_generation(&self.root)?;
        let new_dir = title_pool_dir(&self.root, generation);
        let new_pool = Pool::open(
            &new_dir,
            PoolConfig {
                shard_count: new_count,
                seal_threshold_bytes: self.title_shard_target_bytes,
            },
            None,
        )?;

        let mut remap = Vec::new();
        {
            for old_sid in 0..old_count {
                // Bound migration memory by one old shard instead of
                // materializing the wiki's entire title dictionary.
                let mut entries = Vec::<(u64, Vec<u8>)>::new();
                g.titles.for_each_in_shard(old_sid, |old_id, bytes| {
                    entries.push((old_id, bytes.to_vec()));
                    Ok(())
                })?;
                for (old_id, bytes) in entries {
                    let sid = crate::titles::shard_for(&bytes, new_count);
                    let new_id = new_pool.append(sid, &bytes)?;
                    if old_id > i64::MAX as u64 || new_id > i64::MAX as u64 {
                        return Err(Error::Corrupt("title id exceeds sqlite integer"));
                    }
                    remap.push((old_id, new_id));
                }
            }
        }
        for sid in 0..new_count {
            new_pool.maybe_seal(sid)?;
            new_pool.flush(sid)?;
        }
        sync_parent_dir(&new_dir);
        let prepared_slots = crate::title_slots::TitleSlotGenerations::prepare_remapped(
            &self.root,
            generation,
            &g.title_slots,
            &remap,
        )?;
        let new_slots = prepared_slots.commit()?;
        let new_page_titles = crate::title_slots::PageTitleSlots::open(
            self.root.join(format!("page-titles.{generation}")),
        )?;
        let tx = g.conn.transaction()?;
        crate::title_slots::SqliteOlderTitleIntervals::remap_in_transaction(&tx, &remap)?;
        crate::title_slots::TitleSlotGenerations::select(&tx, generation)?;
        tx.execute(
            "INSERT OR REPLACE INTO instance_flags(key, value)
             VALUES('title_shard_count', ?1)",
            [new_count as i64],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO instance_flags(key, value)
             VALUES('title_pool_generation', ?1)",
            [generation as i64],
        )?;
        tx.commit()?;

        g.titles = new_pool;
        g.title_slots = new_slots;
        g.page_titles = new_page_titles;
        self.title_shard_count.store(new_count, Ordering::Relaxed);
        // The live handles and SQLite flags now select the new durable
        // generation. Older generations are no longer rollback state: the
        // transaction itself is the atomic old/new boundary.
        gc_stale_title_generations(&self.root, generation)?;
        Ok(())
    }

    /// Make successfully committed pages and pending title intents durable
    /// after the source stream fails. The depot remains the dedup authority
    /// when the source is retried.
    pub(crate) fn flush_salvage(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly("flush_salvage"));
        }
        let mut g = self.inner.lock().expect("instance mutex poisoned");
        finish_title_slot_intent(&self.root, &mut g)?;
        g.depot.flush()?;
        for sid in 0..self.title_shard_count.load(Ordering::Relaxed) {
            g.titles.flush(sid)?;
        }
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

const FRESH_TITLE_SHARD_COUNT: u32 = 256;

/// The titles-pool shard count persisted at instance creation
/// (`instance_flags` key `title_shard_count`).
fn persisted_title_shard_count(conn: &Connection) -> Result<Option<u32>> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT value FROM instance_flags WHERE key = 'title_shard_count'",
            [],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(ignore_no_rows)?;
    match v {
        None => Ok(None),
        // A zero/negative/absurd count can only be a hand-mangled flag;
        // routing (or Pool::open's assert) would misbehave — refuse.
        Some(v) if v < 1 || v > u32::MAX as i64 => {
            Err(Error::Corrupt("title_shard_count instance flag"))
        }
        Some(v) => Ok(Some(v as u32)),
    }
}

/// Resolve the EFFECTIVE titles shard count for an open. The count is
/// a property of the store (shard = `fnv1a(title) % count`, and shard
/// files are lazily created, so disk cannot answer): it is persisted
/// in meta.db at creation and every open derives or validates against
/// it.
///
///   * flag present, `requested` 0 (derive) → the persisted count;
///   * flag present, `requested` equal → fine;
///   * flag present, `requested` different → loud
///     [`Error::TitleShardMismatch`] — a mis-counted open would
///     silently route exact lookups to the wrong shard;
///   * flag absent + `may_persist` (writer): a fresh root persists the
///     requested count (0 → 256);
///   * flag absent + read-only: fail loudly because routing cannot be
///     inferred from lazy shard files.
fn resolve_title_shard_count(
    conn: &Connection,
    root: &std::path::Path,
    requested: u32,
    may_persist: bool,
) -> Result<u32> {
    match persisted_title_shard_count(conn)? {
        Some(on_disk) => {
            if requested != 0 && requested != on_disk {
                return Err(Error::TitleShardMismatch {
                    root: root.to_path_buf(),
                    on_disk,
                    requested,
                });
            }
            Ok(on_disk)
        }
        None => {
            if !may_persist {
                return Err(Error::Corrupt("missing title_shard_count instance flag"));
            }
            let n = if requested != 0 { requested } else { FRESH_TITLE_SHARD_COUNT };
            conn.execute(
                "INSERT OR REPLACE INTO instance_flags(key, value)
                 VALUES('title_shard_count', ?1)",
                [n as i64],
            )?;
            Ok(n)
        }
    }
}

fn persisted_title_pool_generation(conn: &Connection) -> Result<u32> {
    let generation: Option<i64> = conn
        .query_row(
            "SELECT value FROM instance_flags WHERE key = 'title_pool_generation'",
            [],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(ignore_no_rows)?;
    match generation {
        None => Ok(0),
        Some(v) if v < 1 || v > u32::MAX as i64 => {
            Err(Error::Corrupt("title_pool_generation instance flag"))
        }
        Some(v) => Ok(v as u32),
    }
}

fn title_pool_dir(root: &std::path::Path, generation: u32) -> PathBuf {
    if generation == 0 {
        root.join("titles")
    } else {
        root.join(format!("titles-g{generation}"))
    }
}

fn title_generation_from_entry(entry: &std::fs::DirEntry) -> Option<u32> {
    let file_type = entry.file_type().ok()?;
    if !file_type.is_dir() {
        return None;
    }
    let name = entry.file_name();
    let name = name.to_str()?;
    if name == "titles" {
        return Some(0);
    }
    let suffix = name.strip_prefix("titles-g")?;
    let generation: u32 = suffix.parse().ok()?;
    (generation >= 1 && generation.to_string() == suffix).then_some(generation)
}

/// Remove only recognized, unselected immutable title-pool generations.
/// The selected generation is never a deletion candidate. Symlinks and
/// unrelated `titles-*` directories are deliberately ignored.
fn gc_stale_title_generations(root: &std::path::Path, selected: u32) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let Some(generation) = title_generation_from_entry(&entry) else {
            continue;
        };
        if generation != selected {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    sync_parent_dir(&title_pool_dir(root, selected));
    Ok(())
}

fn next_title_pool_generation(root: &std::path::Path) -> Result<u32> {
    let mut max_generation = 0u32;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let raw = name
            .strip_prefix("titles-g")
            .or_else(|| name.strip_prefix("title-slots."))
            .or_else(|| name.strip_prefix("page-titles."));
        if let Some(Ok(n)) = raw.map(str::parse::<u32>) {
            max_generation = max_generation.max(n);
        }
    }
    max_generation.checked_add(1).ok_or(Error::Corrupt("title pool generation overflow"))
}

fn title_pool_is_oversized(pool: &Pool, shard_count: u32, target: u64) -> Result<bool> {
    if target == 0 {
        return Ok(false);
    }
    for sid in 0..shard_count {
        if pool.shard_file_size(sid)? > target {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sync_parent_dir(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

pub(crate) fn effective_title_binding(
    inner: &InstanceInner,
    title_id: u64,
) -> Result<Option<crate::title_slots::TitleBinding>> {
    let pending = inner
        .conn
        .query_row(
            "SELECT page_id,valid_since FROM title_slot_intent WHERE title_id=?1",
            [title_id as i64],
            |row| {
                Ok(crate::title_slots::TitleBinding {
                    page_id: row.get(0)?,
                    valid_since: row.get(1)?,
                })
            },
        )
        .map(Some)
        .or_else(ignore_no_rows)?;
    Ok(pending.or_else(|| inner.title_slots.current(title_id)))
}

pub(crate) fn effective_page_title_id(
    inner: &InstanceInner,
    page_id: u32,
) -> Result<Option<u64>> {
    let pending: Option<i64> = inner
        .conn
        .query_row(
            "SELECT title_id FROM title_slot_intent
             WHERE page_id=?1 ORDER BY title_id LIMIT 1",
            [page_id],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(ignore_no_rows)?;
    if let Some(title_id) = pending {
        return Ok(Some(title_id as u64));
    }
    let Some(title_id) = inner.page_titles.current_title_id(page_id) else {
        return Ok(None);
    };
    let shadowed: bool = inner.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM title_slot_intent WHERE title_id=?1)",
        [title_id as i64],
        |row| row.get(0),
    )?;
    Ok((!shadowed).then_some(title_id))
}

fn load_title_slot_intents(
    conn: &Connection,
) -> Result<Vec<(u64, crate::title_slots::TitleBinding)>> {
    let mut statement = conn.prepare(
        "SELECT title_id,page_id,valid_since FROM title_slot_intent ORDER BY title_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)? as u64,
            crate::title_slots::TitleBinding {
                page_id: row.get(1)?,
                valid_since: row.get(2)?,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub(crate) fn finish_title_slot_intent(
    root: &std::path::Path,
    inner: &mut InstanceInner,
) -> Result<()> {
    let changes = load_title_slot_intents(&inner.conn)?;
    if changes.is_empty() {
        return Ok(());
    }
    let generation = crate::title_slots::TitleSlotGenerations::selected(&inner.conn)?;
    let (titles, pages) =
        crate::title_slots::TitleSlotGenerations::apply_current(root, generation, &changes)?;
    inner.title_slots = titles;
    inner.page_titles = pages;
    inner.conn.execute("DELETE FROM title_slot_intent", [])?;
    inner.pending_title_intents = 0;
    inner.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    Ok(())
}

fn recover_title_slot_intent(root: &std::path::Path, conn: &mut Connection) -> Result<()> {
    let changes = load_title_slot_intents(conn)?;
    if changes.is_empty() {
        return Ok(());
    }
    let generation = crate::title_slots::TitleSlotGenerations::selected(conn)?;
    crate::title_slots::TitleSlotGenerations::apply_current(root, generation, &changes)?;
    conn.execute("DELETE FROM title_slot_intent", [])?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    Ok(())
}

/// Deterministic min-hash sample across the full occupied id space. We
/// inspect only the index during selection, then read at most the fixed
/// sample count of heads.
fn representative_chain_ids(depot: &Depot) -> Vec<u64> {
    representative_chain_ids_with_limit(depot, REVISION_SAMPLE_COUNT)
}

fn representative_chain_ids_with_limit(depot: &Depot, limit: usize) -> Vec<u64> {
    let mut selected = std::collections::BTreeMap::<(u64, u64), ()>::new();
    let mut after = None;
    loop {
        let ids = depot.occupied_chain_ids_after(after, OCCUPIED_ID_BATCH);
        if ids.is_empty() {
            break;
        }
        for id in ids.iter().copied() {
            after = Some(id);
            selected.insert((sample_hash(id), id), ());
            if selected.len() > limit {
                selected.pop_last();
            }
        }
    }
    let mut ids: Vec<u64> = selected.keys().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    ids
}

fn occupied_chain_ids(depot: &Depot) -> Vec<u64> {
    let mut ids = Vec::new();
    let mut after = None;
    loop {
        let batch = depot.occupied_chain_ids_after(after, OCCUPIED_ID_BATCH);
        let Some(last) = batch.last().copied() else {
            break;
        };
        ids.extend(batch);
        after = Some(last);
    }
    ids
}

fn revision_dictionary_samples(g: &InstanceInner) -> Result<Vec<Vec<u8>>> {
    let ids = representative_chain_ids(&g.depot);
    let mut samples = Vec::with_capacity(ids.len());
    for id in ids {
        let frame = g.depot.read_f0(id)?;
        let record =
            crate::frames::decompress_head(&frame, &g.revision_dictionaries, "revision")?;
        if !record.is_empty() {
            samples.push(record);
        }
    }
    Ok(samples)
}

fn revision_dictionary_samples_with_limit(
    g: &InstanceInner,
    limit: usize,
    text: bool,
) -> Result<Vec<Vec<u8>>> {
    let ids = representative_chain_ids_with_limit(&g.depot, limit);
    let mut samples = Vec::with_capacity(ids.len());
    for id in ids {
        let frame = g.depot.read_f0(id)?;
        let record =
            crate::frames::decompress_head(&frame, &g.revision_dictionaries, "revision")?;
        let (_, revision_text) = crate::revision::decode_revision_view(&record)?;
        let bytes = if text {
            revision_text.to_vec()
        } else {
            let mut metadata = Vec::new();
            append_experimental_metadata(&record, &mut metadata)?;
            metadata
        };
        if !bytes.is_empty() {
            samples.push(bytes);
        }
    }
    Ok(samples)
}

fn combined_revision_dictionary_samples_with_limit(
    g: &InstanceInner,
    limit: usize,
) -> Result<Vec<Vec<u8>>> {
    let ids = representative_chain_ids_with_limit(&g.depot, limit);
    let mut samples = Vec::with_capacity(ids.len());
    for id in ids {
        let frame = g.depot.read_f0(id)?;
        let record =
            crate::frames::decompress_head(&frame, &g.revision_dictionaries, "revision")?;
        let mut combined = Vec::new();
        let text = append_experimental_metadata(&record, &mut combined)?;
        combined.extend_from_slice(text);
        if !combined.is_empty() {
            samples.push(combined);
        }
    }
    Ok(samples)
}

fn sample_bytes(samples: &[Vec<u8>]) -> Result<usize> {
    samples.iter().try_fold(0usize, |sum, sample| {
        sum.checked_add(sample.len())
            .ok_or(Error::Corrupt("revision dictionary sample size overflow"))
    })
}

fn experimental_dictionary_capacity(samples: usize, bytes: usize) -> Result<usize> {
    if samples < REVISION_MIN_SAMPLES || bytes < REVISION_MIN_SAMPLE_BYTES {
        return Err(Error::InvalidDictionary);
    }
    Ok(REVISION_DICTIONARY_BYTES.min(bytes / REVISION_DICTIONARY_SAMPLE_RATIO))
}

const PACKED_SMALL_MAX_BYTES: usize = 64 << 10;
#[derive(Default)]
struct PackedSmallShard {
    bytes: Vec<u8>,
    pages: u64,
    head_timestamps: Vec<i64>,
}

const F0_HYSTERESIS_LOWER_BYTES: [usize; 5] =
    [64 << 10, 60 << 10, 56 << 10, 48 << 10, 32 << 10];

fn packed_f0_worker(
    root: PathBuf,
    ids: &[u64],
    next: &std::sync::atomic::AtomicUsize,
    dictionary: &[u8],
    packed: &[std::sync::Mutex<PackedSmallShard>],
) -> Result<PackedF0StorageStats> {
    use std::sync::atomic::Ordering;

    const CHUNK: usize = 64;
    const DEPOT_HEADER_BYTES: u64 = 24;

    let instance = Instance::open_read(read_config(root))?;
    let g = instance.inner.lock().expect("instance mutex poisoned");
    let mut totals = PackedF0StorageStats::default();
    loop {
        let start = next.fetch_add(CHUNK, Ordering::Relaxed);
        if start >= ids.len() {
            break;
        }
        for &page_id in &ids[start..ids.len().min(start + CHUNK)] {
            let mut walk = WalkState::new(page_id);
            let mut group_sizes = Vec::new();
            let mut head_combined = None;
            let mut history_raw = 0u64;
            while let Some(record) = walk.next_record(&g.depot, &g.revision_dictionaries)? {
                let mut combined = Vec::new();
                let text = append_experimental_metadata(record, &mut combined)?;
                combined.extend_from_slice(text);
                let group_size = packed_f0_group_len(page_id, combined.len())?;
                group_sizes.push(group_size);
                if head_combined.is_none() {
                    head_combined = Some(combined);
                } else {
                    history_raw = history_raw
                        .checked_add(combined.len() as u64)
                        .ok_or(Error::Corrupt("combined history size overflow"))?;
                }
            }
            let head_combined =
                head_combined.ok_or(Error::Corrupt("occupied chain has no revisions"))?;
            let head_zstd =
                crate::frames::compress_head_dictionary(&head_combined, dictionary)?;
            let standalone_bytes = head_zstd.len() as u64 + DEPOT_HEADER_BYTES;
            totals.pages += 1;
            totals.all_standalone_frame_bytes += standalone_bytes;
            totals.history_raw_bytes += history_raw;

            if history_raw != 0 {
                let mut encoder =
                    wikimak_depot::FrameEncoder::new(history_raw, Some(&head_combined), 3)
                        .map_err(|_| Error::Codec("zstd compress"))?;
                let mut history_walk = WalkState::new(page_id);
                let _ = history_walk
                    .next_record(&g.depot, &g.revision_dictionaries)?
                    .ok_or(Error::Corrupt("occupied chain lost its head"))?;
                while let Some(record) =
                    history_walk.next_record(&g.depot, &g.revision_dictionaries)?
                {
                    let mut combined = Vec::new();
                    let text = append_experimental_metadata(record, &mut combined)?;
                    combined.extend_from_slice(text);
                    encoder
                        .write(&combined)
                        .map_err(|_| Error::Codec("zstd compress"))?;
                }
                let history_zstd =
                    encoder.finish().map_err(|_| Error::Codec("zstd compress"))?;
                totals.history_compressed_bytes += history_zstd.len() as u64;
                totals.history_frame_bytes +=
                    history_zstd.len() as u64 + DEPOT_HEADER_BYTES;
            }

            let group = encode_packed_f0_group(page_id, &head_combined);
            if group.len() <= PACKED_SMALL_MAX_BYTES {
                let shard_id = sample_hash(page_id) as usize & (packed.len() - 1);
                let mut shard = packed[shard_id]
                    .lock()
                    .expect("packed-f0 shard mutex poisoned");
                shard.bytes.extend_from_slice(&group);
                shard.pages += 1;
                totals.small_pages += 1;
                totals.small_raw_bytes += group.len() as u64;
                totals.replaced_standalone_frame_bytes += standalone_bytes;
            } else {
                totals.big_pages += 1;
            }

            group_sizes.reverse();
            for (slot, &lower) in F0_HYSTERESIS_LOWER_BYTES.iter().enumerate() {
                let mut small = group_sizes[0] <= PACKED_SMALL_MAX_BYTES;
                let mut changed = false;
                for &size in &group_sizes[1..] {
                    let next_small = if small {
                        size <= PACKED_SMALL_MAX_BYTES
                    } else {
                        size <= lower
                    };
                    if next_small != small {
                        totals.hysteresis_transitions[slot] += 1;
                        changed = true;
                        small = next_small;
                    }
                }
                if changed {
                    totals.hysteresis_transition_pages[slot] += 1;
                }
                if small {
                    totals.hysteresis_current_small_pages[slot] += 1;
                }
            }
        }
    }
    Ok(totals)
}

fn packed_f0_group_len(page_id: u64, text_len: usize) -> Result<usize> {
    varint_len(page_id)
        .checked_add(varint_len(text_len as u64))
        .and_then(|len| len.checked_add(text_len))
        .ok_or(Error::Corrupt("packed-f0 group length overflow"))
}

fn encode_packed_f0_group(page_id: u64, text: &[u8]) -> Vec<u8> {
    let mut group = Vec::with_capacity(varint_len(page_id) + varint_len(text.len() as u64) + text.len());
    crate::revision::encode_varint(page_id, &mut group);
    crate::revision::encode_varint(text.len() as u64, &mut group);
    group.extend_from_slice(text);
    group
}

fn varint_len(mut value: u64) -> usize {
    let mut bytes = 1;
    while value >= 0x80 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

fn add_packed_f0_stats(totals: &mut PackedF0StorageStats, partial: &PackedF0StorageStats) {
    totals.pages += partial.pages;
    totals.small_pages += partial.small_pages;
    totals.big_pages += partial.big_pages;
    totals.small_raw_bytes += partial.small_raw_bytes;
    totals.all_standalone_frame_bytes += partial.all_standalone_frame_bytes;
    totals.replaced_standalone_frame_bytes += partial.replaced_standalone_frame_bytes;
    totals.history_raw_bytes += partial.history_raw_bytes;
    totals.history_compressed_bytes += partial.history_compressed_bytes;
    totals.history_frame_bytes += partial.history_frame_bytes;
    for slot in 0..F0_HYSTERESIS_LOWER_BYTES.len() {
        totals.hysteresis_transition_pages[slot] +=
            partial.hysteresis_transition_pages[slot];
        totals.hysteresis_transitions[slot] += partial.hysteresis_transitions[slot];
        totals.hysteresis_current_small_pages[slot] +=
            partial.hysteresis_current_small_pages[slot];
    }
}

fn split_revision_storage_worker(
    root: PathBuf,
    ids: &[u64],
    next: &std::sync::atomic::AtomicUsize,
    text_dictionary: &[u8],
    metadata_dictionary: &[u8],
    combined_dictionary: &[u8],
    packed_small: Option<&[std::sync::Mutex<PackedSmallShard>]>,
) -> Result<SplitRevisionStorageStats> {
    use std::sync::atomic::Ordering;

    const CHUNK: usize = 64;
    const DEPOT_HEADER_BYTES: u64 = 24;

    let instance = Instance::open_read(read_config(root))?;
    let g = instance.inner.lock().expect("instance mutex poisoned");
    let mut totals = SplitRevisionStorageStats::default();
    loop {
        let start = next.fetch_add(CHUNK, Ordering::Relaxed);
        if start >= ids.len() {
            break;
        }
        for &page_id in &ids[start..ids.len().min(start + CHUNK)] {
            let mut walk = WalkState::new(page_id);
            let mut metadata = Vec::new();
            let mut head_text = Vec::new();
            let mut head_combined = Vec::new();
            let mut history_text_raw = 0u64;
            let mut history_combined_raw = 0u64;
            let mut revisions = 0u64;
            let mut head_ts_micros = 0i64;
            let mut packed_body = Vec::new();
            let mut packed_candidate = packed_small.is_some();
            while let Some(record) =
                walk.next_record(&g.depot, &g.revision_dictionaries)?
            {
                let metadata_start = metadata.len();
                let text = append_experimental_metadata(record, &mut metadata)?;
                let mut combined = Vec::with_capacity(metadata.len() - metadata_start + text.len());
                combined.extend_from_slice(&metadata[metadata_start..]);
                combined.extend_from_slice(text);
                if packed_candidate {
                    packed_body.extend_from_slice(&metadata[metadata_start..]);
                    packed_body.extend_from_slice(text);
                }
                if revisions == 0 {
                    head_ts_micros = crate::revision::decode_revision_view(record)?
                        .0
                        .ts
                        .timestamp_micros();
                    head_text.extend_from_slice(text);
                    head_combined.extend_from_slice(&combined);
                } else {
                    history_text_raw = history_text_raw
                        .checked_add(text.len() as u64)
                        .ok_or(Error::Corrupt("history text size overflow"))?;
                    history_combined_raw = history_combined_raw
                        .checked_add(combined.len() as u64)
                        .ok_or(Error::Corrupt("combined history size overflow"))?;
                }
                revisions += 1;
                if packed_body.len() > PACKED_SMALL_MAX_BYTES {
                    packed_body.clear();
                    packed_candidate = false;
                }
            }
            if revisions == 0 {
                return Err(Error::Corrupt("occupied chain has no revisions"));
            }

            let metadata_zstd =
                crate::frames::compress_head_dictionary(&metadata, metadata_dictionary)?;
            let head_zstd =
                crate::frames::compress_head_dictionary(&head_text, text_dictionary)?;
            let combined_head_zstd =
                crate::frames::compress_head_dictionary(&head_combined, combined_dictionary)?;
            totals.pages += 1;
            totals.revisions += revisions;
            totals.revision_count_buckets[revision_count_bucket(revisions)] += 1;
            totals.metadata_raw_bytes += metadata.len() as u64;
            totals.metadata_compressed_bytes += metadata_zstd.len() as u64;
            totals.metadata_frame_bytes += metadata_zstd.len() as u64 + DEPOT_HEADER_BYTES;
            totals.head_text_raw_bytes += head_text.len() as u64;
            totals.head_text_compressed_bytes += head_zstd.len() as u64;
            totals.head_text_frame_bytes += head_zstd.len() as u64 + DEPOT_HEADER_BYTES;
            totals.head_text_length_buckets[head_text_length_bucket(head_text.len())] += 1;
            totals.history_text_raw_bytes += history_text_raw;
            totals.combined_f0_raw_bytes += head_combined.len() as u64;
            totals.combined_f0_compressed_bytes += combined_head_zstd.len() as u64;
            totals.combined_f0_frame_bytes +=
                combined_head_zstd.len() as u64 + DEPOT_HEADER_BYTES;
            totals.combined_history_raw_bytes += history_combined_raw;

            let mut history_frame_bytes = 0;
            let mut combined_history_frame_bytes = 0;
            if history_text_raw != 0 {
                let mut encoder =
                    wikimak_depot::FrameEncoder::new(history_text_raw, Some(&head_text), 3)
                        .map_err(|_| Error::Codec("zstd compress"))?;
                let mut history_walk = WalkState::new(page_id);
                let _ = history_walk
                    .next_record(&g.depot, &g.revision_dictionaries)?
                    .ok_or(Error::Corrupt("occupied chain lost its head"))?;
                while let Some(record) =
                    history_walk.next_record(&g.depot, &g.revision_dictionaries)?
                {
                    let (_, text) = crate::revision::decode_revision_view(record)?;
                    encoder.write(text).map_err(|_| Error::Codec("zstd compress"))?;
                }
                let history_zstd =
                    encoder.finish().map_err(|_| Error::Codec("zstd compress"))?;
                totals.history_text_compressed_bytes += history_zstd.len() as u64;
                history_frame_bytes = history_zstd.len() as u64 + DEPOT_HEADER_BYTES;
                totals.history_text_frame_bytes += history_frame_bytes;
                totals.history_text_frames += 1;
            }
            if history_combined_raw != 0 {
                let mut encoder =
                    wikimak_depot::FrameEncoder::new(
                        history_combined_raw,
                        Some(&head_combined),
                        3,
                    )
                    .map_err(|_| Error::Codec("zstd compress"))?;
                let mut history_walk = WalkState::new(page_id);
                let _ = history_walk
                    .next_record(&g.depot, &g.revision_dictionaries)?
                    .ok_or(Error::Corrupt("occupied chain lost its head"))?;
                while let Some(record) =
                    history_walk.next_record(&g.depot, &g.revision_dictionaries)?
                {
                    let mut combined = Vec::new();
                    let text = append_experimental_metadata(record, &mut combined)?;
                    combined.extend_from_slice(text);
                    encoder
                        .write(&combined)
                        .map_err(|_| Error::Codec("zstd compress"))?;
                }
                let history_zstd =
                    encoder.finish().map_err(|_| Error::Codec("zstd compress"))?;
                totals.combined_history_compressed_bytes += history_zstd.len() as u64;
                combined_history_frame_bytes =
                    history_zstd.len() as u64 + DEPOT_HEADER_BYTES;
                totals.combined_history_frame_bytes += combined_history_frame_bytes;
            }
            if packed_candidate {
                let mut group_body = Vec::with_capacity(packed_body.len() + 2);
                crate::revision::encode_varint(revisions, &mut group_body);
                group_body.extend_from_slice(&packed_body);
                let mut group = Vec::with_capacity(group_body.len() + 12);
                crate::revision::encode_varint(page_id, &mut group);
                crate::revision::encode_varint(group_body.len() as u64, &mut group);
                group.extend_from_slice(&group_body);
                if group.len() <= PACKED_SMALL_MAX_BYTES {
                    let packed_small = packed_small.unwrap();
                    let shard_id = sample_hash(page_id) as usize & (packed_small.len() - 1);
                    let mut shard = packed_small[shard_id]
                        .lock()
                        .expect("packed-small shard mutex poisoned");
                    shard.bytes.extend_from_slice(&group);
                    shard.pages += 1;
                    shard.head_timestamps.push(head_ts_micros);
                    totals.packed_small_pages += 1;
                    totals.packed_small_revisions += revisions;
                    totals.packed_small_raw_bytes += group.len() as u64;
                    totals.packed_small_split_frame_bytes += metadata_zstd.len() as u64
                        + head_zstd.len() as u64
                        + history_frame_bytes
                        + 2 * DEPOT_HEADER_BYTES;
                    totals.packed_small_combined_frame_bytes +=
                        combined_head_zstd.len() as u64
                            + combined_history_frame_bytes
                            + DEPOT_HEADER_BYTES;
                }
            }
        }
    }
    Ok(totals)
}

fn finish_packed_small_stats(
    totals: &mut SplitRevisionStorageStats,
    shards: &[std::sync::Mutex<PackedSmallShard>],
    text_dictionary: &[u8],
) -> Result<()> {
    const SHARD_FOOTER_BYTES: u64 = 8;

    let mut scans = Vec::new();
    let mut weighted_scan_bytes = 0u128;
    for (shard_id, shard) in shards.iter().enumerate() {
        let shard = shard.lock().expect("packed-small shard mutex poisoned");
        if shard.pages == 0 {
            continue;
        }
        weighted_scan_bytes += shard.bytes.len() as u128 * shard.pages as u128;
        scans.push((shard.bytes.len() as u64, shard.pages, shard_id));
    }
    if totals.packed_small_pages == 0 {
        return Ok(());
    }
    scans.sort_unstable_by_key(|&(bytes, _, _)| bytes);
    totals.packed_small_mean_scan_bytes =
        (weighted_scan_bytes / totals.packed_small_pages as u128) as u64;
    totals.packed_small_p50_scan_bytes =
        weighted_scan_quantile(&scans, totals.packed_small_pages, 50);
    totals.packed_small_p95_scan_bytes =
        weighted_scan_quantile(&scans, totals.packed_small_pages, 95);
    totals.packed_small_p99_scan_bytes =
        weighted_scan_quantile(&scans, totals.packed_small_pages, 99);
    totals.packed_small_max_scan_bytes = scans.last().map_or(0, |&(bytes, _, _)| bytes);
    let representative = scans
        .iter()
        .min_by_key(|&&(bytes, _, _)| bytes.abs_diff(totals.packed_small_p50_scan_bytes))
        .map(|&(_, _, shard_id)| shard_id)
        .ok_or(Error::Corrupt("packed-small scan distribution is empty"))?;
    totals.packed_small_latest_head_ts_micros = shards
        .iter()
        .filter_map(|shard| {
            shard
                .lock()
                .expect("packed-small shard mutex poisoned")
                .head_timestamps
                .iter()
                .copied()
                .max()
        })
        .max()
        .unwrap_or(0);
    let cutoff_1d = totals
        .packed_small_latest_head_ts_micros
        .saturating_sub(24 * 60 * 60 * 1_000_000);
    let cutoff_7d = totals
        .packed_small_latest_head_ts_micros
        .saturating_sub(7 * 24 * 60 * 60 * 1_000_000);

    let mut compressed_sizes = Vec::with_capacity(scans.len());
    for (shard_id, shard) in shards.iter().enumerate() {
        let shard = shard.lock().expect("packed-small shard mutex poisoned");
        if shard.pages == 0 {
            continue;
        }
        let zstd = crate::frames::compress_head_dictionary(&shard.bytes, text_dictionary)?;
        totals.packed_small_compressed_bytes += zstd.len() as u64;
        totals.packed_small_file_bytes += zstd.len() as u64 + SHARD_FOOTER_BYTES;
        totals.packed_small_materialized_shards += 1;
        compressed_sizes.push(zstd.len() as u64 + SHARD_FOOTER_BYTES);
        let dirty_1d = shard
            .head_timestamps
            .iter()
            .filter(|&&ts| ts > cutoff_1d)
            .count() as u64;
        let dirty_7d = shard
            .head_timestamps
            .iter()
            .filter(|&&ts| ts > cutoff_7d)
            .count() as u64;
        totals.packed_small_dirty_1d_pages += dirty_1d;
        totals.packed_small_dirty_7d_pages += dirty_7d;
        if dirty_1d != 0 {
            totals.packed_small_dirty_1d_shards += 1;
            totals.packed_small_rewrite_1d_bytes += zstd.len() as u64 + SHARD_FOOTER_BYTES;
        }
        if dirty_7d != 0 {
            totals.packed_small_dirty_7d_shards += 1;
            totals.packed_small_rewrite_7d_bytes += zstd.len() as u64 + SHARD_FOOTER_BYTES;
        }
        if shard_id == representative {
            benchmark_packed_small_shard(totals, &shard, &zstd, text_dictionary)?;
        }
    }
    compressed_sizes.sort_unstable();
    totals.packed_small_p50_compressed_shard_bytes =
        unweighted_quantile(&compressed_sizes, 50);
    totals.packed_small_p95_compressed_shard_bytes =
        unweighted_quantile(&compressed_sizes, 95);
    totals.packed_small_p99_compressed_shard_bytes =
        unweighted_quantile(&compressed_sizes, 99);
    totals.packed_small_max_compressed_shard_bytes =
        compressed_sizes.last().copied().unwrap_or(0);
    Ok(())
}

fn finish_packed_f0_stats(
    totals: &mut PackedF0StorageStats,
    shards: &[std::sync::Mutex<PackedSmallShard>],
    dictionary: &[u8],
) -> Result<()> {
    const SHARD_FOOTER_BYTES: u64 = 8;

    let mut scans = Vec::new();
    let mut weighted_scan_bytes = 0u128;
    for (shard_id, shard) in shards.iter().enumerate() {
        let shard = shard.lock().expect("packed-f0 shard mutex poisoned");
        if shard.pages == 0 {
            continue;
        }
        weighted_scan_bytes += shard.bytes.len() as u128 * shard.pages as u128;
        scans.push((shard.bytes.len() as u64, shard.pages, shard_id));
    }
    if totals.small_pages == 0 {
        return Ok(());
    }
    scans.sort_unstable_by_key(|&(bytes, _, _)| bytes);
    totals.mean_scan_bytes = (weighted_scan_bytes / totals.small_pages as u128) as u64;
    totals.p50_scan_bytes = weighted_scan_quantile(&scans, totals.small_pages, 50);
    totals.p95_scan_bytes = weighted_scan_quantile(&scans, totals.small_pages, 95);
    totals.p99_scan_bytes = weighted_scan_quantile(&scans, totals.small_pages, 99);
    totals.max_scan_bytes = scans.last().map_or(0, |&(bytes, _, _)| bytes);
    let representative = scans
        .iter()
        .min_by_key(|&&(bytes, _, _)| bytes.abs_diff(totals.p50_scan_bytes))
        .map(|&(_, _, shard_id)| shard_id)
        .ok_or(Error::Corrupt("packed-f0 scan distribution is empty"))?;

    let mut compressed_sizes = Vec::with_capacity(scans.len());
    for (shard_id, shard) in shards.iter().enumerate() {
        let shard = shard.lock().expect("packed-f0 shard mutex poisoned");
        if shard.pages == 0 {
            continue;
        }
        let zstd = crate::frames::compress_head_dictionary(&shard.bytes, dictionary)?;
        let file_bytes = zstd.len() as u64 + SHARD_FOOTER_BYTES;
        totals.packed_compressed_bytes += zstd.len() as u64;
        totals.packed_file_bytes += file_bytes;
        totals.materialized_shards += 1;
        compressed_sizes.push(file_bytes);
        if file_bytes > 1 << 20 {
            totals.oversized_1m_shards += 1;
        }
        if shard_id == representative {
            benchmark_packed_f0_shard(totals, &shard, &zstd, dictionary)?;
        }
    }
    compressed_sizes.sort_unstable();
    totals.p50_compressed_shard_bytes = unweighted_quantile(&compressed_sizes, 50);
    totals.p95_compressed_shard_bytes = unweighted_quantile(&compressed_sizes, 95);
    totals.p99_compressed_shard_bytes = unweighted_quantile(&compressed_sizes, 99);
    totals.max_compressed_shard_bytes = compressed_sizes.last().copied().unwrap_or(0);
    Ok(())
}

fn benchmark_packed_f0_shard(
    totals: &mut PackedF0StorageStats,
    shard: &PackedSmallShard,
    zstd: &[u8],
    dictionary: &[u8],
) -> Result<()> {
    let page_ids = packed_small_page_ids(&shard.bytes)?;
    if page_ids.is_empty() {
        return Err(Error::Corrupt("packed-f0 benchmark shard has no pages"));
    }
    let targets = [
        page_ids[0],
        page_ids[page_ids.len() / 2],
        *page_ids.last().unwrap(),
    ];
    let iterations = ((256usize << 20) / shard.bytes.len().max(1)).clamp(20, 2000);
    let mut timings = [0u64; 3];
    for (slot, target) in targets.into_iter().enumerate() {
        std::hint::black_box(extract_packed_small_group(zstd, dictionary, target)?);
        let started = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(extract_packed_small_group(zstd, dictionary, target)?);
        }
        timings[slot] = (started.elapsed().as_nanos() / iterations as u128) as u64;
    }
    totals.benchmark_pages = shard.pages;
    totals.benchmark_raw_bytes = shard.bytes.len() as u64;
    totals.benchmark_compressed_bytes = zstd.len() as u64;
    totals.benchmark_iterations = iterations as u64;
    totals.first_extract_ns = timings[0];
    totals.middle_extract_ns = timings[1];
    totals.last_extract_ns = timings[2];
    Ok(())
}

fn unweighted_quantile(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values[(values.len() * percentile).div_ceil(100).saturating_sub(1)]
}

fn weighted_scan_quantile(scans: &[(u64, u64, usize)], pages: u64, percentile: u64) -> u64 {
    let wanted = pages.saturating_mul(percentile).div_ceil(100);
    let mut seen = 0u64;
    for &(bytes, count, _) in scans {
        seen += count;
        if seen >= wanted {
            return bytes;
        }
    }
    0
}

fn benchmark_packed_small_shard(
    totals: &mut SplitRevisionStorageStats,
    shard: &PackedSmallShard,
    zstd: &[u8],
    dictionary: &[u8],
) -> Result<()> {
    let page_ids = packed_small_page_ids(&shard.bytes)?;
    if page_ids.is_empty() {
        return Err(Error::Corrupt("packed-small benchmark shard has no pages"));
    }
    let targets = [
        page_ids[0],
        page_ids[page_ids.len() / 2],
        *page_ids.last().unwrap(),
    ];
    let iterations = ((256usize << 20) / shard.bytes.len().max(1)).clamp(20, 2000);
    let mut timings = [0u64; 3];
    for (slot, target) in targets.into_iter().enumerate() {
        std::hint::black_box(extract_packed_small_group(zstd, dictionary, target)?);
        let started = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(extract_packed_small_group(zstd, dictionary, target)?);
        }
        timings[slot] = (started.elapsed().as_nanos() / iterations as u128) as u64;
    }
    totals.packed_small_benchmark_pages = shard.pages;
    totals.packed_small_benchmark_raw_bytes = shard.bytes.len() as u64;
    totals.packed_small_benchmark_compressed_bytes = zstd.len() as u64;
    totals.packed_small_benchmark_iterations = iterations as u64;
    totals.packed_small_first_extract_ns = timings[0];
    totals.packed_small_middle_extract_ns = timings[1];
    totals.packed_small_last_extract_ns = timings[2];
    Ok(())
}

fn packed_small_page_ids(bytes: &[u8]) -> Result<Vec<u64>> {
    let mut offset = 0usize;
    let mut ids = Vec::new();
    while offset < bytes.len() {
        let (page_id, used) = crate::revision::decode_varint(bytes, offset)?;
        offset += used;
        let (group_len, used) = crate::revision::decode_varint(bytes, offset)?;
        offset += used;
        let group_len: usize = group_len
            .try_into()
            .map_err(|_| Error::Corrupt("packed-small group length exceeds usize"))?;
        offset = offset
            .checked_add(group_len)
            .filter(|&end| end <= bytes.len())
            .ok_or(Error::Corrupt("packed-small group exceeds shard"))?;
        ids.push(page_id);
    }
    Ok(ids)
}

fn extract_packed_small_group(
    zstd: &[u8],
    dictionary: &[u8],
    wanted_page_id: u64,
) -> Result<Vec<u8>> {
    use std::io::Read;

    let prepared = zstd::dict::DecoderDictionary::copy(dictionary);
    let mut decoder = zstd::stream::read::Decoder::with_prepared_dictionary(zstd, &prepared)
        .map_err(|_| Error::Codec("zstd decompress"))?;
    loop {
        let page_id = read_stream_varint(&mut decoder)?
            .ok_or(Error::Corrupt("packed-small page is missing from its shard"))?;
        let group_len = read_stream_varint(&mut decoder)?
            .ok_or(Error::Corrupt("packed-small group length is missing"))?;
        let group_len: usize = group_len
            .try_into()
            .map_err(|_| Error::Corrupt("packed-small group length exceeds usize"))?;
        if page_id == wanted_page_id {
            let mut group = vec![0u8; group_len];
            decoder
                .read_exact(&mut group)
                .map_err(|_| Error::Codec("truncated packed-small group"))?;
            return Ok(group);
        }
        let copied = std::io::copy(&mut decoder.by_ref().take(group_len as u64), &mut std::io::sink())
            .map_err(|_| Error::Codec("skip packed-small group"))?;
        if copied != group_len as u64 {
            return Err(Error::Codec("truncated packed-small group"));
        }
    }
}

fn read_stream_varint<R: std::io::Read>(reader: &mut R) -> Result<Option<u64>> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) if shift == 0 => return Ok(None),
            Ok(0) => return Err(Error::Codec("truncated packed-small varint")),
            Ok(_) => {}
            Err(_) => return Err(Error::Codec("read packed-small varint")),
        }
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(Some(value));
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Codec("packed-small varint overflow"));
        }
    }
}

fn head_text_length_bucket(len: usize) -> usize {
    const UPPER_BOUNDS: [usize; 16] = [
        0, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535, 131071,
        262143, usize::MAX,
    ];
    UPPER_BOUNDS.iter().position(|&upper| len <= upper).unwrap()
}

fn revision_count_bucket(revisions: u64) -> usize {
    match revisions {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        5..=7 => 4,
        8..=15 => 5,
        16..=31 => 6,
        32..=63 => 7,
        64..=127 => 8,
        128..=255 => 9,
        256..=511 => 10,
        512..=1023 => 11,
        1024..=2047 => 12,
        2048..=4095 => 13,
        4096..=8191 => 14,
        _ => 15,
    }
}

fn append_experimental_metadata<'a>(
    record: &'a [u8],
    out: &mut Vec<u8>,
) -> Result<&'a [u8]> {
    let (meta, text) = crate::revision::decode_revision_view(record)?;
    let (kind, user_id, contributor) = crate::revision::contributor_wire(&meta.contributor);
    out.extend_from_slice(&crate::revision::REVISION_SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&meta.flags.to_le_bytes());
    out.extend_from_slice(&meta.rev_id.to_le_bytes());
    out.extend_from_slice(&meta.parent_id.to_le_bytes());
    out.extend_from_slice(&(meta.ts.timestamp_micros() as u64).to_le_bytes());
    out.extend_from_slice(&user_id.to_le_bytes());
    out.push(kind);
    crate::revision::encode_varint(contributor.len() as u64, out);
    out.extend_from_slice(contributor);
    crate::revision::encode_varint(meta.comment.len() as u64, out);
    out.extend_from_slice(meta.comment.as_bytes());
    crate::revision::encode_varint(text.len() as u64, out);
    Ok(text)
}

fn add_split_revision_stats(
    totals: &mut SplitRevisionStorageStats,
    partial: &SplitRevisionStorageStats,
) {
    totals.pages += partial.pages;
    totals.revisions += partial.revisions;
    for (total, count) in totals
        .revision_count_buckets
        .iter_mut()
        .zip(partial.revision_count_buckets)
    {
        *total += count;
    }
    totals.metadata_raw_bytes += partial.metadata_raw_bytes;
    totals.metadata_compressed_bytes += partial.metadata_compressed_bytes;
    totals.metadata_frame_bytes += partial.metadata_frame_bytes;
    totals.head_text_raw_bytes += partial.head_text_raw_bytes;
    totals.head_text_compressed_bytes += partial.head_text_compressed_bytes;
    totals.head_text_frame_bytes += partial.head_text_frame_bytes;
    for (total, count) in totals
        .head_text_length_buckets
        .iter_mut()
        .zip(partial.head_text_length_buckets)
    {
        *total += count;
    }
    totals.history_text_raw_bytes += partial.history_text_raw_bytes;
    totals.history_text_compressed_bytes += partial.history_text_compressed_bytes;
    totals.history_text_frame_bytes += partial.history_text_frame_bytes;
    totals.history_text_frames += partial.history_text_frames;
    totals.combined_f0_raw_bytes += partial.combined_f0_raw_bytes;
    totals.combined_f0_compressed_bytes += partial.combined_f0_compressed_bytes;
    totals.combined_f0_frame_bytes += partial.combined_f0_frame_bytes;
    totals.combined_history_raw_bytes += partial.combined_history_raw_bytes;
    totals.combined_history_compressed_bytes +=
        partial.combined_history_compressed_bytes;
    totals.combined_history_frame_bytes += partial.combined_history_frame_bytes;
    totals.packed_small_pages += partial.packed_small_pages;
    totals.packed_small_revisions += partial.packed_small_revisions;
    totals.packed_small_raw_bytes += partial.packed_small_raw_bytes;
    totals.packed_small_split_frame_bytes += partial.packed_small_split_frame_bytes;
    totals.packed_small_combined_frame_bytes +=
        partial.packed_small_combined_frame_bytes;
}

pub(crate) fn sample_hash(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
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
// walk therefore streams without materializing a decompressed frame.
// It retains the compressed frame, zstd's window, the prior frame's
// anchor, and the current record; that record becomes the next anchor
// when it is the frame's oldest. Reads
// that used to `collect_records` the whole decompressed history now pay
// for the frames up to their early stop and nothing past it.
// ---------------------------------------------------------------------

/// Resumable newest-first record walk over one chain. Drive it with
/// [`WalkState::next_record`]; the yielded slice borrows the walk's
/// current record buffer — decode it meta-only and copy out at most the
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
    Head {
        record: Vec<u8>,
        yielded: bool,
        pending_f1: Option<Vec<u8>>,
        cold: Option<wikimak_depot::ColdCursor>,
    },
    InFrame {
        /// Incremental zstd decode state. The decompressed frame is
        /// never materialized; the decoder retains its compressed
        /// bytes and, where required, the preceding record as refPrefix.
        decoder: wikimak_depot::OwnedFrameDecoder,
        /// The current/last-yielded record. While the frame is active
        /// this is the caller-visible buffer; at frame end its final
        /// value becomes the next frame's refPrefix anchor.
        record: Vec<u8>,
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
    pub(crate) fn next_record(
        &mut self,
        depot: &Depot,
        dictionaries: &crate::frames::DictionaryStore,
    ) -> Result<Option<&[u8]>> {
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
                    let (pending_f1, cold) = if self.eager {
                        (depot.read_f1(self.chain_id)?, Some(depot.cold_cursor(self.chain_id)?))
                    } else {
                        (None, None)
                    };
                    let record = crate::frames::decompress_head(
                        &f0,
                        dictionaries,
                        "revision",
                    )?;
                    self.frame = WalkFrame::Head {
                        record,
                        yielded: false,
                        pending_f1,
                        cold,
                    };
                }
                WalkFrame::Head { yielded, .. } if !*yielded => {
                    *yielded = true;
                    break;
                }
                WalkFrame::Head { .. } => {
                    self.advance_frame(depot)?;
                }
                WalkFrame::InFrame { decoder, record, .. } => {
                    if read_revision_record(decoder, record)? {
                        break;
                    }
                    self.advance_frame(depot)?;
                }
            }
        }
        match &self.frame {
            WalkFrame::Head { record, .. } | WalkFrame::InFrame { record, .. } => {
                Ok(Some(record))
            }
            _ => unreachable!(),
        }
    }

    /// Cross a frame boundary: the current frame is exhausted; its
    /// oldest record anchors the next frame's refPrefix decode.
    fn advance_frame(&mut self, depot: &Depot) -> Result<()> {
        let (record, pending_f1, cold) =
            match std::mem::replace(&mut self.frame, WalkFrame::Done) {
                WalkFrame::Head { record, pending_f1, cold, .. } => {
                    (record, pending_f1, cold)
                }
                WalkFrame::InFrame { record, pending_f1, cold, .. } => {
                    (record, pending_f1, cold)
                }
                _ => return Ok(()),
        };
        // At EOF `record` is the oldest record yielded from this frame,
        // exactly the refPrefix anchor for the next frame.
        let anchor = record;
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
            if crate::frames::frame_dictionary_id(&f1).is_some() {
                return Err(Error::FrameEnvelope(
                    "history frame carries a dictionary id",
                ));
            }
            let decoder = wikimak_depot::OwnedFrameDecoder::new(f1, Some(anchor))
                .map_err(|_| Error::Codec("zstd decompress"))?;
            self.frame = WalkFrame::InFrame {
                decoder,
                record: Vec::new(),
                pending_f1: None,
                cold: Some(cold),
            };
            return Ok(());
        }
        match depot.cold_next(&mut cold)? {
            Some(frame) => {
                if crate::frames::frame_dictionary_id(&frame).is_some() {
                    return Err(Error::FrameEnvelope(
                        "history frame carries a dictionary id",
                    ));
                }
                let decoder = wikimak_depot::OwnedFrameDecoder::new(frame, Some(anchor))
                    .map_err(|_| Error::Codec("zstd decompress"))?;
                self.frame = WalkFrame::InFrame {
                    decoder,
                    record: Vec::new(),
                    pending_f1: None,
                    cold: Some(cold),
                };
            }
            None => self.frame = WalkFrame::Done,
        }
        Ok(())
    }
}

/// Decode one self-delimiting revision record from a zstd frame.
/// `false` means clean frame EOF before any byte of another record;
/// EOF inside a record is corruption. The buffer grows only to the
/// largest single record, never to the decompressed frame size.
fn read_revision_record(
    decoder: &mut wikimak_depot::OwnedFrameDecoder,
    record: &mut Vec<u8>,
) -> Result<bool> {
    const FIXED: usize = 4 + 4 + 8 + 8 + 8 + 8 + 1;
    // Probe EOF before clearing the last record: at a frame boundary
    // that record is still needed as the next frame's refPrefix.
    let mut first = [0u8; 1];
    if decoder.read(&mut first)? == 0 {
        return Ok(false);
    }
    record.clear();
    record.push(first[0]);
    read_record_bytes(decoder, record, FIXED - 1)?;
    for _ in 0..4 {
        let mut len = 0u64;
        let mut shift = 0u32;
        loop {
            read_record_bytes(decoder, record, 1)?;
            let b = *record.last().expect("one byte appended");
            if shift == 63 && b & 0xfe != 0 {
                return Err(Error::Codec("record varint overflow"));
            }
            len |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return Err(Error::Codec("record varint overflow"));
            }
        }
        let len = usize::try_from(len).map_err(|_| Error::Codec("record field too large"))?;
        read_record_bytes(decoder, record, len)?;
    }
    Ok(true)
}

/// Append exactly `len` decompressed bytes. EOF here is always
/// corruption; clean frame EOF is probed before a record begins.
fn read_record_bytes(
    decoder: &mut wikimak_depot::OwnedFrameDecoder,
    record: &mut Vec<u8>,
    len: usize,
) -> Result<()> {
    record
        .try_reserve(len)
        .map_err(|_| Error::Codec("record field too large"))?;
    let mut scratch = [0u8; 64 << 10];
    let mut left = len;
    while left > 0 {
        let want = left.min(scratch.len());
        let n = decoder.read(&mut scratch[..want])?;
        if n == 0 {
            return Err(Error::Codec("truncated revision record"));
        }
        record.extend_from_slice(&scratch[..n]);
        left -= n;
    }
    Ok(())
}

/// Find `rev_id` on the chain: newest-first early-stopping walk,
/// records peeked by fixed offset (no per-record string decode), the
/// target decoded once and its text copied out only if `want_text`.
pub(crate) fn find_revision(
    depot: &Depot,
    dictionaries: &crate::frames::DictionaryStore,
    chain_id: u64,
    rev_id: u64,
    want_text: bool,
) -> Result<Option<(RevisionMeta, Option<Vec<u8>>)>> {
    let mut walk = WalkState::new(chain_id);
    while let Some(rec) = walk.next_record(depot, dictionaries)? {
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
/// `(rev_id, ts)` (the ts backfill hook). Besides zstd's decoder
/// window, the current compressed frame and its optional preceding-record
/// refPrefix, at most one decoded record plus the current best record's
/// text (when `want_text`) is resident.
pub(crate) fn scan_best(
    depot: &Depot,
    dictionaries: &crate::frames::DictionaryStore,
    chain_id: u64,
    tau: Option<i64>,
    want_text: bool,
    each: &mut dyn FnMut(u64, i64) -> Result<()>,
) -> Result<Option<(RevisionMeta, Option<Vec<u8>>)>> {
    let mut best: Option<(RevisionMeta, Option<Vec<u8>>)> = None;
    let mut walk = WalkState::new(chain_id);
    while let Some(rec) = walk.next_record(depot, dictionaries)? {
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
            let rec = match self.walk.next_record(&g.depot, &g.revision_dictionaries) {
                Ok(Some(rec)) => rec,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            match crate::revision::decode_revision_view(rec) {
                Ok((meta, _text)) => meta, // text stays in the record buffer
                Err(e) => return Some(Err(e)),
            }
        };
        let inner = Arc::clone(&self.inner);
        let chain_id = self.chain_id;
        let rev_id = meta.rev_id;
        let fetch_text: Box<dyn FnOnce() -> Result<Vec<u8>> + Send> = Box::new(move || {
            let g = inner.lock().expect("instance mutex poisoned");
            match find_revision(
                &g.depot,
                &g.revision_dictionaries,
                chain_id,
                rev_id,
                true,
            )? {
                Some((_meta, Some(text))) => Ok(text),
                _ => Err(Error::Corrupt("revision vanished from its chain")),
            }
        });
        Some(Ok(HistoryEntry { meta, fetch_text }))
    }
}

#[cfg(test)]
mod streaming_record_tests {
    use super::read_revision_record;

    fn record(text: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 4 + 4 + 8 + 8 + 8 + 8 + 1];
        out.extend_from_slice(&[0, 0, 0]);
        crate::revision::encode_varint(text.len() as u64, &mut out);
        out.extend_from_slice(text);
        out
    }

    #[test]
    fn large_frame_does_not_accumulate_decompressed_records() {
        let one = record(b"small immutable revision");
        let count = 200_000usize;
        let mut raw = Vec::with_capacity(one.len() * count);
        for _ in 0..count {
            raw.extend_from_slice(&one);
        }
        assert!(raw.len() > 10 << 20);
        let frame = crate::frames::compress(&raw, None).unwrap();
        let mut decoder = wikimak_depot::OwnedFrameDecoder::new(frame, None).unwrap();
        let mut rec = Vec::new();
        let mut seen = 0usize;
        while read_revision_record(&mut decoder, &mut rec).unwrap() {
            assert_eq!(rec, one);
            assert!(rec.capacity() < 1024, "record buffer accumulated decompressed records");
            seen += 1;
        }
        assert_eq!(seen, count);
    }

    #[test]
    fn truncated_record_is_not_accepted_as_frame_eof() {
        let mut raw = record(b"payload");
        raw.pop();
        let frame = crate::frames::compress(&raw, None).unwrap();
        let mut decoder = wikimak_depot::OwnedFrameDecoder::new(frame, None).unwrap();
        let err = read_revision_record(&mut decoder, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, crate::Error::Codec("truncated revision record")));
    }
}

#[cfg(test)]
mod dictionary_lifecycle_tests {
    use chrono::TimeZone;
    use tempfile::TempDir;

    use super::*;

    fn cfg(root: PathBuf) -> InstanceConfig {
        InstanceConfig {
            root,
            dbname: "dictwiki".into(),
            max_chain_id: 1024,
            depot: DepotConfig {
                root: PathBuf::new(),
                max_chain_id: 1024,
                file_size_threshold: 8 << 20,
                eviction_dead_ratio: 0.45,
            },
            title_shard_count: 1,
            title_seal_threshold_bytes: 8 << 20,
            f1_seal_threshold_bytes: 1 << 20,
        }
    }

    fn record(page: usize, text_bytes: usize) -> Vec<u8> {
        let text = format!(
            "Page {page} shared encyclopedia prose and markup. {}",
            "abcdefghij".repeat(text_bytes / 10)
        );
        crate::revision::encode_revision(
            &RevisionMeta {
                rev_id: 10_000 + page as u64,
                parent_id: 0,
                ts: Utc.timestamp_opt(1_704_067_200, 0).single().unwrap(),
                contributor: ContributorMeta::Named {
                    username: "Editor".into(),
                    user_id: 1,
                },
                comment: "seed".into(),
                sha1: String::new(),
                flags: 0,
                text_len: text.len() as u64,
            },
            text.as_bytes(),
        )
    }

    fn seed_pages(instance: &Instance, pages: usize, text_bytes: usize) {
        let g = instance.inner.lock().unwrap();
        for page in 1..=pages {
            let head = record(page, text_bytes);
            let mut builder = g.depot.begin_chain(page as u64).unwrap();
            if page == 1 {
                let older = record(5001, 1024);
                let history = crate::frames::compress_history(&older, &head).unwrap();
                g.depot.append_history_frame(&mut builder, &history).unwrap();
            }
            let f0 = crate::frames::compress_head_plain(&head).unwrap();
            g.depot.finish_chain(builder, &f0, None).unwrap();
        }
    }

    #[test]
    fn one_native_dictionary_resumes_mixed_head_repack_and_reopens() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let instance = Instance::open(cfg(root.clone())).unwrap();
        seed_pages(&instance, 160, 10 << 10);
        let cold_before = std::fs::read(root.join("depot/cold/cold")).unwrap();
        assert!(!cold_before.is_empty());

        let first = instance
            .repack_revision_dictionary_inner(false, Some(7))
            .unwrap();
        assert!(first.trained);
        assert_eq!(first.heads_repacked, 7);
        let dict_id = first.dictionary_id.unwrap();
        {
            let g = instance.inner.lock().unwrap();
            let mut encoded = 0;
            let mut plain = 0;
            for id in 1..=160 {
                match crate::frames::frame_dictionary_id(&g.depot.read_f0(id).unwrap()) {
                    Some(id) if id == dict_id => encoded += 1,
                    None => plain += 1,
                    other => panic!("unexpected dictionary id {other:?}"),
                }
            }
            assert_eq!((encoded, plain), (7, 153));
        }

        let resumed = instance
            .repack_revision_dictionary_inner(true, None)
            .unwrap();
        assert!(resumed.trained);
        assert_eq!(resumed.dictionary_id, Some(dict_id));
        assert_eq!(resumed.heads_repacked, 153);
        assert_eq!(std::fs::read(root.join("depot/cold/cold")).unwrap(), cold_before);
        {
            let g = instance.inner.lock().unwrap();
            let head = record(161, 10 << 10);
            let builder = g.depot.begin_chain(161).unwrap();
            let f0 =
                crate::frames::compress_head(&head, &g.revision_dictionaries).unwrap();
            g.depot.finish_chain(builder, &f0, None).unwrap();
            assert_eq!(
                crate::frames::frame_dictionary_id(&g.depot.read_f0(161).unwrap()),
                Some(dict_id),
                "later heads use the active dictionary"
            );
        }
        let dictionaries = std::fs::read_dir(root.join("dictionaries"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "zdict"))
            .count();
        assert_eq!(dictionaries, 1);
        drop(instance);

        let reader = Instance::open_read(cfg(root)).unwrap();
        assert_eq!(reader.page_head(160).unwrap().unwrap().rev_id, 10_160);
        let g = reader.inner.lock().unwrap();
        for id in 1..=161 {
            assert_eq!(
                crate::frames::frame_dictionary_id(&g.depot.read_f0(id).unwrap()),
                Some(dict_id)
            );
        }
    }

    #[test]
    fn dictionary_samples_keep_complete_revision_records() {
        let tmp = TempDir::new().unwrap();
        let instance = Instance::open(cfg(tmp.path().to_path_buf())).unwrap();
        seed_pages(&instance, 160, 40 << 10);
        let g = instance.inner.lock().unwrap();
        let samples = revision_dictionary_samples(&g).unwrap();
        assert_eq!(samples.len(), 160);
        assert!(
            samples.iter().all(|sample| sample.len() > (40 << 10)),
            "metadata plus the complete 40-KiB text must be retained"
        );
    }

    #[test]
    fn split_storage_experiment_separates_metadata_and_text_without_writes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let instance = Instance::open(cfg(root.clone())).unwrap();
        seed_pages(&instance, 160, 10 << 10);
        instance.flush().unwrap();

        let records: Vec<Vec<u8>> = (1..=160).map(|page| record(page, 10 << 10)).collect();
        let text_samples: Vec<Vec<u8>> = records
            .iter()
            .map(|record| crate::revision::decode_revision_view(record).unwrap().1.to_vec())
            .collect();
        let metadata_samples: Vec<Vec<u8>> = records
            .iter()
            .map(|record| {
                let mut metadata = Vec::new();
                append_experimental_metadata(record, &mut metadata).unwrap();
                metadata
            })
            .collect();
        let combined_samples: Vec<Vec<u8>> = records
            .iter()
            .map(|record| {
                let mut combined = Vec::new();
                let text = append_experimental_metadata(record, &mut combined).unwrap();
                combined.extend_from_slice(text);
                combined
            })
            .collect();
        let text_dictionary = crate::frames::train_dictionary(&text_samples, 4096).unwrap();
        let metadata_dictionary =
            crate::frames::train_dictionary(&metadata_samples, 4096).unwrap();
        let combined_dictionary =
            crate::frames::train_dictionary(&combined_samples, 4096).unwrap();
        drop(instance);

        let ids: Vec<u64> = (1..=160).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let stats = split_revision_storage_worker(
            root,
            &ids,
            &next,
            &text_dictionary,
            &metadata_dictionary,
            &combined_dictionary,
            None,
        )
        .unwrap();
        assert_eq!(stats.pages, 160);
        assert_eq!(stats.revisions, 161);
        assert_eq!(stats.history_text_frames, 1);
        assert!(stats.metadata_raw_bytes > stats.metadata_compressed_bytes);
        assert!(stats.head_text_raw_bytes > stats.head_text_compressed_bytes);
        assert!(stats.history_text_raw_bytes > stats.history_text_compressed_bytes);
    }

    #[test]
    fn tiny_seed_does_not_train_a_dictionary() {
        let tmp = TempDir::new().unwrap();
        let instance = Instance::open(cfg(tmp.path().to_path_buf())).unwrap();
        seed_pages(&instance, 2, 1024);
        let stats = instance.finalize_seed_revision_dictionary().unwrap();
        assert_eq!(stats.dictionary_id, None);
        assert!(!stats.trained);
        assert_eq!(stats.samples, 2);
        assert!(stats.sample_bytes > 0);
        assert_eq!(stats.heads_repacked, 0);
        assert!(!tmp.path().join("dictionaries/revision.current").exists());
    }

    #[test]
    fn pending_title_rows_overlay_writer_reads_reject_readers_and_replay_on_open() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let instance = Instance::open(cfg(root.clone())).unwrap();
        {
            let mut g = instance.inner.lock().unwrap();
            let title = b"Crash pending title";
            let title_id = g.titles.append(0, title).unwrap();
            g.titles.flush(0).unwrap();
            g.conn
                .execute(
                    "INSERT INTO title_slot_intent(title_id,page_id,valid_since)
                     VALUES(?1,7,100)",
                    [title_id as i64],
                )
                .unwrap();
            g.pending_title_intents = 1;
            assert_eq!(effective_page_title_id(&g, 7).unwrap(), Some(title_id));
            assert_eq!(
                effective_title_binding(&g, title_id).unwrap().unwrap().page_id(),
                Some(7)
            );
        }
        assert_eq!(
            instance.page_current_title(7).unwrap().as_deref(),
            Some("Crash pending title")
        );
        drop(instance);

        let read_error = match Instance::open_read(cfg(root.clone())) {
            Ok(_) => panic!("reader accepted unreplayed title intent"),
            Err(error) => error.to_string(),
        };
        assert!(read_error.contains("writable recovery"), "{read_error}");

        let recovered = Instance::open(cfg(root)).unwrap();
        assert_eq!(
            recovered.page_current_title(7).unwrap().as_deref(),
            Some("Crash pending title")
        );
        let g = recovered.inner.lock().unwrap();
        assert_eq!(
            g.conn
                .query_row("SELECT COUNT(*) FROM title_slot_intent", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }
}
