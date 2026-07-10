//! IETF internet-drafts mirror (MIRRORS.md phase 2).
//!
//! Each draft series (`draft-x-00 .. draft-x-NN`) is one newest-first
//! chain of canonical depot layers in a shared [`depot_vbf::VbfDepot`];
//! the inventory — draft name → chain id, revision watermarks, fetch
//! state — lives in this crate's own sqlite (`meta.db`), never in the
//! depot (DEPOT-DESIGN.md §3, the bookkeeping fence).
//!
//! A revision's layer is a full snapshot: one child node `text` whose
//! blob is the draft body, with `rev` and `date` attrs. Wholesale blob
//! replacement is right at the layer level — successor similarity is
//! the compressor's job (the chain's refPrefix discipline), not the
//! model's.
//!
//! Discovery: the live index (`/id/all_id.txt`) lists each draft ONCE,
//! at its LATEST revision. History is enumerated, not listed: revision
//! numbers are sequential two-digit `00..NN`, so the candidate set is
//! derived from the head number and every candidate not yet
//! watermarked in `revisions_seen` is fetched (a 404 — expired from
//! the archive — is watermarked `missing` and never re-tried).
//!
//! Durability handshake, same shape as wikimak's sync but BATCHED:
//! store writes accumulate per draft; every `FLUSH_EVERY_DRAFTS` drafts
//! (and once at the run's end) one depot flush is followed by one
//! sqlite transaction watermarking everything since the last flush —
//! bytes always durable BEFORE the bookkeeping that references them,
//! without a per-draft fsync storm. The crash window between a flush
//! and its watermark tx is covered by a dirty flag (wikimak's
//! discipline): stamped durably before the run's first store write,
//! cleared only after the final watermark commit. A run that opens
//! dirty reconciles each touched draft against the CHAIN — revisions
//! already stored are watermark-aligned (rev attrs decoded, texts
//! dropped), never re-fetched, never re-prepended — so a crash at any
//! point yields no duplicate records and no lost bytes.

pub mod readout;
#[cfg(feature = "fetch")]
mod cli;
#[cfg(feature = "fetch")]
pub use cli::cli_main;

use std::collections::BTreeMap;
#[cfg(feature = "fetch")]
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use depot::{BlobOp, Layer};
#[cfg(feature = "fetch")]
use depot::Node;
use depot_vbf::VbfDepot;
#[cfg(feature = "fetch")]
use reqwest::blocking::Client;
use rusqlite::Connection;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("vbf: {0}")]
    Vbf(#[from] depot_vbf::Error),
    #[cfg(feature = "fetch")]
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("http status {status} for {url}")]
    HttpStatus { status: u16, url: String },
    #[error("parse: {0}")]
    Parse(String),
    #[error("draft inventory full: max_chain_id {0} reached")]
    InventoryFull(u64),
    #[error("corrupt layer on chain {chain_id}: {what}")]
    CorruptLayer { chain_id: u64, what: &'static str },
    /// Another process holds this mirror root (meta.db exclusive lock).
    #[error("mirror {0} is locked by another process")]
    MirrorLocked(PathBuf),
    /// `update` called on a [`Mirror::open_read`] handle.
    #[error("mirror opened read-only (shared lock): updating requires Mirror::open")]
    ReadOnly,
}

/// Configuration for the HTTP side. Production uses `Default`; tests
/// point `base_url` at a stand-in server (the same seam as
/// `wikimak_mediawiki::Config`).
#[derive(Debug, Clone)]
pub struct FetchConfig {
    pub base_url: String,
    /// Politeness pause between consecutive content GETs (the index GET
    /// is not paced). A full-history backfill is on the order of a
    /// million serial GETs — it must not hammer the server.
    pub delay: Duration,
    /// Retries after a failed GET: transport errors and 5xx only. 4xx
    /// is never retried — 404 is the missing-revision watermark signal,
    /// the rest are bugs to surface loudly.
    pub retries: u32,
    /// First retry's backoff; doubles per subsequent retry.
    pub backoff: Duration,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            base_url: "https://www.ietf.org".to_string(),
            delay: Duration::from_millis(250),
            retries: 3,
            backoff: Duration::from_millis(500),
        }
    }
}

/// Store sizing at open time.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    pub root: PathBuf,
    /// Chain-id capacity ≙ maximum number of draft series. The live
    /// index lists each draft once at its latest revision, ~43k drafts
    /// today; the default leaves headroom.
    pub max_chain_id: u64,
    /// depot-vbf accumulator seal threshold (decompressed bytes).
    pub seal_threshold: u64,
}

impl MirrorConfig {
    pub fn new(root: PathBuf) -> Self {
        Self { root, max_chain_id: 1 << 20, seal_threshold: 256 * 1024 }
    }
}

/// Counters from one [`Mirror::update`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateStats {
    pub drafts_seen: u64,
    pub drafts_new: u64,
    pub revisions_fetched: u64,
    pub revisions_skipped: u64,
    /// Enumerated revisions whose text GET returned 404 (expired from
    /// the archive) — recorded as seen-but-missing, never re-tried.
    pub revisions_missing: u64,
    /// Revisions found already ON the chain while reconciling after a
    /// dirty (crashed) run: watermarks aligned from the chain — no
    /// re-fetch, no duplicate prepend.
    pub revisions_reconciled: u64,
    /// The index answered 304 Not Modified: nothing to do this pass.
    pub index_not_modified: bool,
    /// Chains rebuilt onto a fresh chain id because older revisions had
    /// to be backfilled UNDER an existing newer head (legacy mirrors
    /// from the heads-only index era; crash recovery).
    pub chains_rebuilt: u64,
}

/// One entry of a draft's history (newest-first from [`Mirror::history`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionEntry {
    pub rev: String,
    pub date: Option<String>,
    pub text: Vec<u8>,
}

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS drafts (
        name TEXT PRIMARY KEY,
        chain_id INTEGER NOT NULL UNIQUE
    )",
    "CREATE TABLE IF NOT EXISTS revisions_seen (
        name TEXT NOT NULL,
        rev TEXT NOT NULL,
        missing INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(name, rev)
    ) WITHOUT ROWID",
    // Update-pass state: HTTP validators (index ETag / Last-Modified)
    // for conditional re-fetch, written only after a fully successful
    // pass; and the crash-window 'dirty' flag — '1' between a run's
    // first store write and its final watermark commit, so a run that
    // opens dirty knows chains may be AHEAD of revisions_seen and
    // reconciles instead of re-prepending.
    "CREATE TABLE IF NOT EXISTS fetch_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID",
];

pub struct Mirror {
    conn: Connection,
    store: VbfDepot,
    /// The root's flock, held for the mirror's lifetime.
    _lock: std::fs::File,
    #[cfg_attr(not(feature = "fetch"), allow(dead_code))]
    max_chain_id: u64,
    /// Opened under a shared flock: reads only, `update` refuses.
    #[cfg_attr(not(feature = "fetch"), allow(dead_code))]
    read_only: bool,
    /// The previous writing session died between a store flush and its
    /// watermark tx (the 'dirty' flag was set on open): chains may be
    /// AHEAD of `revisions_seen`, so `update` reconciles each touched
    /// draft against its chain before trusting the watermarks.
    #[cfg_attr(not(feature = "fetch"), allow(dead_code))]
    suspect: bool,
}

/// Take the per-root flock (`op` = `LOCK_EX` for the one writer,
/// `LOCK_SH` for readers), non-blocking: contention is a loud
/// [`Error::MirrorLocked`], never a silent wait behind a possibly
/// hours-long update run. Kernel-released on any exit.
fn flock_root(root: &std::path::Path, op: libc::c_int) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true).truncate(false).write(true)
        .open(root.join(".lock"))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) };
    if rc != 0 {
        return Err(Error::MirrorLocked(root.to_path_buf()));
    }
    Ok(f)
}

impl Mirror {
    /// Open or create the mirror at `cfg.root` (`meta.db` + `depot/`)
    /// as THE writer (exclusive flock). Re-open is idempotent.
    pub fn open(cfg: MirrorConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;
        // Lock BEFORE touching the depot: its open walks every tier
        // file (dead-byte accounting), which must not race a writer.
        let lock = flock_root(&cfg.root, libc::LOCK_EX)?;
        let store = VbfDepot::open(cfg.root.join("depot"), cfg.max_chain_id, cfg.seal_threshold)?;
        let conn = Connection::open(cfg.root.join("meta.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        for stmt in DDL {
            conn.execute(stmt, [])?;
        }
        let suspect = read_dirty_flag(&conn)?;
        Ok(Mirror {
            conn,
            store,
            max_chain_id: cfg.max_chain_id,
            _lock: lock,
            read_only: false,
            suspect,
        })
    }

    /// Open an EXISTING mirror for reading, under a SHARED flock: any
    /// number of concurrent readers, excluded only while a writer
    /// ([`Mirror::open`]) holds the root — and vice versa. The flock is
    /// what keeps the depot's file set stable under a reader (eviction
    /// unlinks tier files and patches next-pointers in place; lock-free
    /// reads against a live writer would chase dangling pointers), so
    /// hold the handle only as long as the read takes: decode, drop.
    /// Never creates anything: a non-mirror root is a loud error.
    pub fn open_read(cfg: MirrorConfig) -> Result<Self> {
        if !cfg.root.join("meta.db").exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no mirror at {}", cfg.root.display()),
            )));
        }
        let lock = flock_root(&cfg.root, libc::LOCK_SH)?;
        let store = VbfDepot::open(cfg.root.join("depot"), cfg.max_chain_id, cfg.seal_threshold)?;
        // No DDL, no pragma writes: the writer created the schema; this
        // connection only ever SELECTs.
        let conn = Connection::open_with_flags(
            cfg.root.join("meta.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, // WAL recovery may write; we never do
        )?;
        Ok(Mirror {
            conn,
            store,
            max_chain_id: cfg.max_chain_id,
            _lock: lock,
            read_only: true,
            suspect: false,
        })
    }

    /// Discover + fetch + import every unseen revision of every draft.
    /// The index lists each draft once at its LATEST revision; the
    /// candidate set `00..NN` is enumerated from it and filtered by the
    /// `revisions_seen` watermarks — idempotent and resumable at
    /// per-revision granularity. `progress` gets `(draft-rev, fetched)`
    /// per revision considered.
    #[cfg(feature = "fetch")]
    pub fn update(
        &mut self,
        client: &Client,
        cfg: &FetchConfig,
        mut progress: impl FnMut(&str, bool),
    ) -> Result<UpdateStats> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut stats = UpdateStats::default();
        let Some((index, etag, last_modified)) = self.fetch_index_conditional(client, cfg)?
        else {
            stats.index_not_modified = true;
            return Ok(stats);
        };
        let mut pacer = Pacer::new(cfg.delay);
        // Store writes accumulate; every FLUSH_EVERY_DRAFTS drafts (and
        // once at the end) `commit_pending` runs ONE depot flush then
        // ONE watermark tx for the whole batch — the bytes-before-
        // bookkeeping fence holds per batch, without a per-draft fsync
        // storm (a flush fsyncs every tier file).
        let mut pending: Vec<PendingDraft> = Vec::new();
        let mut dirty_stamped = false;
        for (name, (latest, date)) in &index {
            stats.drafts_seen += 1;
            // Revision numbers are sequential two-digit 00..NN: the
            // head number from the index spans the whole history.
            let latest_n: u8 = latest
                .parse()
                .map_err(|_| Error::Parse(format!("bad revision {latest:?} for {name}")))?;
            let mut fresh: Vec<(String, Option<String>)> = Vec::new();
            for n in 0..=latest_n {
                let rev = format!("{n:02}");
                if self.rev_seen(name, &rev)? {
                    stats.revisions_skipped += 1;
                    progress(&format!("{name}-{rev}"), false);
                } else {
                    // Only the index's own line carries a date — it
                    // describes the latest revision, nothing older.
                    let d = if rev == *latest { date.clone() } else { None };
                    fresh.push((rev, d));
                }
            }
            if fresh.is_empty() {
                continue;
            }
            // Dirty fence, once per run, durable BEFORE the first store
            // write: between a store write and its batch's watermark tx
            // a chain is AHEAD of revisions_seen — a crash there leaves
            // the flag set and the NEXT run reconciles from the chains
            // instead of re-prepending refetched revisions forever.
            if !dirty_stamped {
                self.set_dirty(true)?;
                dirty_stamped = true;
            }
            let chain_id = match self.chain_id(name)? {
                Some(id) => id,
                None => {
                    stats.drafts_new += 1;
                    self.alloc_chain(name)?
                }
            };
            let mut done: Vec<(String, bool)> = Vec::new();
            // Suspect-mode reconcile (the previous session died dirty):
            // the chain is the data fence — any unseen candidate ALREADY
            // on the chain just gets its watermark aligned; only truly
            // absent revisions are fetched, so no duplicate prepends.
            if self.suspect {
                let on_chain = self.chain_revs(chain_id)?;
                fresh.retain(|(rev, _)| {
                    if on_chain.contains(rev) {
                        stats.revisions_reconciled += 1;
                        progress(&format!("{name}-{rev}"), false);
                        done.push((rev.clone(), false));
                        false
                    } else {
                        true
                    }
                });
            }
            // Oldest→newest; the whole draft lands as ONE batch
            // prepend (the normative multi-record form — one f0 swap,
            // one f1 re-encode; depot-vbf splits only past the seal
            // threshold), not a per-revision cycle. Fetch errors abort
            // before any store write: idempotent refetch.
            let mut batch: Vec<(String, Layer)> = Vec::new();
            for (rev, date) in &fresh {
                let label = format!("{name}-{rev}");
                progress(&label, true);
                let url = format!("{}/archive/id/{label}.txt", cfg.base_url);
                pacer.pace();
                match http_get(client, &url, cfg)? {
                    Some(text) => {
                        batch.push((rev.clone(), revision_layer(rev, date.as_deref(), &text)));
                        stats.revisions_fetched += 1;
                        done.push((rev.clone(), false));
                    }
                    None => {
                        // Enumerated but gone (expired from the
                        // archive). Watermark as missing; never re-try.
                        stats.revisions_missing += 1;
                        done.push((rev.clone(), true));
                    }
                }
            }
            // Newest-first chains grow at the HEAD with NEWER records:
            // `put_layers` makes the batch's LAST layer the new head.
            // The batch is ascending, so on a fresh chain or a head
            // bump the order is right by construction. If the chain's
            // head is already at-or-past the batch's oldest revision (a
            // legacy heads-only mirror being backfilled; a crashed run
            // predating the dirty flag), prepending would corrupt the
            // order or duplicate a record — rebuild the whole draft in
            // order onto a fresh chain instead (the merge dedupes).
            let repoint = if batch.is_empty() {
                None
            } else {
                let head_rev = self
                    .store
                    .head_layer(chain_id)?
                    .map(|l| layer_rev(&l, chain_id))
                    .transpose()?;
                let needs_rebuild =
                    matches!((&head_rev, batch.first()), (Some(h), Some((r, _))) if r <= h);
                if needs_rebuild {
                    stats.chains_rebuilt += 1;
                    Some(self.rebuild_chain(chain_id, batch)?)
                } else {
                    let layers: Vec<Layer> = batch.into_iter().map(|(_, l)| l).collect();
                    self.store.put_layers(chain_id, &layers)?;
                    None
                }
            };
            pending.push(PendingDraft { name: name.clone(), done, repoint });
            if pending.len() >= FLUSH_EVERY_DRAFTS {
                self.commit_pending(&mut pending)?;
            }
        }
        self.commit_pending(&mut pending)?;
        // Persist the index validators only after a FULLY successful
        // pass: an interrupted run must re-see the full index next time
        // (and resume via watermarks), never be 304'd past unfinished
        // drafts.
        self.set_index_validators(etag.as_deref(), last_modified.as_deref())?;
        if dirty_stamped {
            // Everything the run wrote is durable in order (bytes, then
            // bookkeeping): a crash from here on is a clean shutdown.
            self.set_dirty(false)?;
            self.suspect = false;
        }
        // Session-end compaction: dead frames from this run's prepends/
        // rebuilds otherwise sit in the under-threshold current write
        // files forever. (What collect cannot reclaim: rebuild-orphaned
        // chains — still index-live inside the depot — and cold-file
        // bytes; see VbfDepot::collect.)
        self.store.collect()?;
        Ok(stats)
    }

    /// The per-batch durability handshake: flush the depot, then one
    /// transaction writing every pending draft's watermarks and chain
    /// repoints. Bytes are durable strictly before the bookkeeping that
    /// references them; a crash in between is what the dirty flag and
    /// the reconcile path exist for.
    #[cfg(feature = "fetch")]
    fn commit_pending(&mut self, pending: &mut Vec<PendingDraft>) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        self.store.flush()?;
        // Crash-window test knob: die AFTER the store flush, BEFORE the
        // watermark tx — the exact window the dirty flag covers.
        if std::env::var_os("IETFMAK_TEST_CRASH_AFTER_FLUSH").is_some() {
            std::process::abort();
        }
        let tx = self.conn.transaction()?;
        {
            let mut mark = tx.prepare_cached(
                "INSERT OR REPLACE INTO revisions_seen(name, rev, missing) VALUES(?1,?2,?3)",
            )?;
            let mut repoint =
                tx.prepare_cached("UPDATE drafts SET chain_id = ?2 WHERE name = ?1")?;
            for d in pending.iter() {
                if let Some(new_chain) = d.repoint {
                    repoint.execute(rusqlite::params![d.name, new_chain as i64])?;
                }
                for (rev, missing) in &d.done {
                    mark.execute(rusqlite::params![d.name, rev, *missing as i64])?;
                }
            }
        }
        tx.commit()?;
        pending.clear();
        Ok(())
    }

    /// Set/clear the crash-window dirty flag, durably (WAL checkpoint —
    /// `synchronous=NORMAL` commits are not power-loss durable on their
    /// own, and this flag is only useful if it survives exactly that).
    #[cfg(feature = "fetch")]
    fn set_dirty(&self, dirty: bool) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO fetch_meta(key, value) VALUES('dirty', ?1)",
            rusqlite::params![if dirty { "1" } else { "0" }],
        )?;
        self.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
    }

    /// Every `rev` attr present on `chain_id` — the reconcile read.
    /// Bounded: the scan keeps one frame resident at a time, and only
    /// the rev strings are RETAINED (texts drop with each layer).
    #[cfg(feature = "fetch")]
    fn chain_revs(&self, chain_id: u64) -> Result<std::collections::BTreeSet<String>> {
        let mut revs = std::collections::BTreeSet::new();
        let mut bad: Option<Error> = None;
        self.store.scan_newest_first(chain_id, |layer| {
            match layer_rev(&layer, chain_id) {
                Ok(rev) => {
                    revs.insert(rev);
                    false
                }
                Err(e) => {
                    bad = Some(e);
                    true
                }
            }
        })?;
        match bad {
            Some(e) => Err(e),
            None => Ok(revs),
        }
    }

    /// Re-lay a draft whose unseen revisions sort BELOW the chain's
    /// current head: merge the existing layers with the fetched batch,
    /// ascending by revision (dedup keeps the existing layer — the
    /// crash-recovery case refetches revisions the chain already has),
    /// and write them to a fresh chain id. The caller repoints the
    /// draft's inventory row; the old chain becomes dead weight for the
    /// depot's eviction. Store-only — no sqlite writes here, so a crash
    /// leaves the draft on its intact old chain.
    #[cfg(feature = "fetch")]
    fn rebuild_chain(&mut self, old_chain: u64, batch: Vec<(String, Layer)>) -> Result<u64> {
        let mut all: Vec<(String, Layer)> = Vec::new();
        for layer in self.store.layers_newest_first(old_chain)? {
            let rev = decode_revision_layer(&layer, old_chain)?.rev;
            all.push((rev, layer));
        }
        all.extend(batch);
        all.sort_by(|a, b| a.0.cmp(&b.0)); // oldest → newest (stable)
        all.dedup_by(|a, b| a.0 == b.0); // keeps the first = existing
        let new_chain = self.free_chain_id()?;
        let layers: Vec<Layer> = all.into_iter().map(|(_, l)| l).collect();
        self.store.put_layers(new_chain, &layers)?;
        Ok(new_chain)
    }

    /// All mirrored draft names, ascending.
    pub fn drafts(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT name FROM drafts ORDER BY name")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Newest mirrored revision of `name` (one standalone f0 decode).
    pub fn head(&self, name: &str) -> Result<Option<RevisionEntry>> {
        let Some(chain_id) = self.chain_id(name)? else { return Ok(None) };
        match self.store.head_layer(chain_id)? {
            Some(layer) => Ok(Some(decode_revision_layer(&layer, chain_id)?)),
            None => Ok(None),
        }
    }

    /// ONE specific revision of `name` — the pinned-attachment read.
    /// Bounded walk: newest-first, one frame resident at a time, stops
    /// at the match; only the matching revision's text is materialized.
    pub fn revision(&self, name: &str, rev: &str) -> Result<Option<RevisionEntry>> {
        let Some(chain_id) = self.chain_id(name)? else { return Ok(None) };
        let mut found: Option<RevisionEntry> = None;
        let mut bad: Option<Error> = None;
        self.store.scan_newest_first(chain_id, |layer| {
            match layer_rev(&layer, chain_id) {
                Ok(r) if r == rev => {
                    match decode_revision_layer(&layer, chain_id) {
                        Ok(e) => found = Some(e),
                        Err(e) => bad = Some(e),
                    }
                    true
                }
                Ok(_) => false,
                Err(e) => {
                    bad = Some(e);
                    true
                }
            }
        })?;
        match bad {
            Some(e) => Err(e),
            None => Ok(found),
        }
    }

    /// Every mirrored revision of `name`, newest-first.
    pub fn history(&self, name: &str) -> Result<Vec<RevisionEntry>> {
        let Some(chain_id) = self.chain_id(name)? else { return Ok(vec![]) };
        self.store
            .layers_newest_first(chain_id)?
            .iter()
            .map(|l| decode_revision_layer(l, chain_id))
            .collect()
    }

    fn chain_id(&self, name: &str) -> Result<Option<u64>> {
        // Once per draft per pass (~43k over the live corpus): cached.
        let mut st = self.conn.prepare_cached("SELECT chain_id FROM drafts WHERE name = ?1")?;
        let mut rows = st.query([name])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get::<_, i64>(0)? as u64),
            None => None,
        })
    }

    #[cfg(feature = "fetch")]
    fn alloc_chain(&self, name: &str) -> Result<u64> {
        let next = self.free_chain_id()?;
        self.conn.execute(
            "INSERT INTO drafts(name, chain_id) VALUES(?1, ?2)",
            rusqlite::params![name, next as i64],
        )?;
        Ok(next)
    }

    /// The next chain id past the inventory maximum that is also EMPTY
    /// in the store — a crash between a store write and its inventory
    /// commit can orphan layers on an unreferenced id, and reusing such
    /// an id would splice a foreign history under a new draft.
    #[cfg(feature = "fetch")]
    fn free_chain_id(&self) -> Result<u64> {
        let next: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(chain_id) + 1, 0) FROM drafts", [], |r| r.get(0))?;
        let mut next = next as u64;
        while next < self.max_chain_id && self.store.head_layer(next)?.is_some() {
            next += 1;
        }
        if next >= self.max_chain_id {
            return Err(Error::InventoryFull(self.max_chain_id));
        }
        Ok(next)
    }

    /// Fetch `/id/all_id.txt` with `If-None-Match`/`If-Modified-Since`
    /// from the stored validators. `None` = 304, nothing changed;
    /// otherwise the parsed index plus the response's fresh validators
    /// (persisted by the caller only after the pass succeeds).
    #[cfg(feature = "fetch")]
    fn fetch_index_conditional(
        &self,
        client: &Client,
        cfg: &FetchConfig,
    ) -> Result<Option<(DraftIndex, Option<String>, Option<String>)>> {
        use reqwest::header;
        let url = format!("{}/id/all_id.txt", cfg.base_url);
        let etag = self.fetch_meta("index_etag")?;
        let last_mod = self.fetch_meta("index_last_modified")?;
        retrying(cfg, || {
            let mut req = client.get(&url);
            if let Some(v) = &etag {
                req = req.header(header::IF_NONE_MATCH, v);
            }
            if let Some(v) = &last_mod {
                req = req.header(header::IF_MODIFIED_SINCE, v);
            }
            let resp = req.send()?;
            let status = resp.status().as_u16();
            if status == 304 {
                return Ok(None);
            }
            if !resp.status().is_success() {
                return Err(Error::HttpStatus { status, url: url.clone() });
            }
            let hdr = |k: header::HeaderName| {
                resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string)
            };
            let (new_etag, new_lm) = (hdr(header::ETAG), hdr(header::LAST_MODIFIED));
            let mut body = Vec::new();
            let mut r = resp;
            r.read_to_end(&mut body)?;
            Ok(Some((parse_index(&String::from_utf8_lossy(&body)), new_etag, new_lm)))
        })
    }

    #[cfg(feature = "fetch")]
    fn fetch_meta(&self, key: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare("SELECT value FROM fetch_meta WHERE key = ?1")?;
        let mut rows = st.query([key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    #[cfg(feature = "fetch")]
    fn set_index_validators(
        &mut self,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM fetch_meta WHERE key IN ('index_etag','index_last_modified')",
            [],
        )?;
        for (k, v) in [("index_etag", etag), ("index_last_modified", last_modified)] {
            if let Some(v) = v {
                tx.execute(
                    "INSERT INTO fetch_meta(key, value) VALUES(?1, ?2)",
                    rusqlite::params![k, v],
                )?;
            }
        }
        Ok(tx.commit()?)
    }

    #[cfg(feature = "fetch")]
    fn rev_seen(&self, name: &str, rev: &str) -> Result<bool> {
        // Hot path: one probe per enumerated revision (~1.5M per full
        // pass over the live corpus) — keep the statement cached.
        let mut st = self.conn.prepare_cached(
            "SELECT COUNT(*) FROM revisions_seen WHERE name = ?1 AND rev = ?2",
        )?;
        let n: u64 = st.query_row(rusqlite::params![name, rev], |r| r.get(0))?;
        Ok(n > 0)
    }
}

/// One draft's not-yet-watermarked outcome, queued between batch
/// flushes (see `Mirror::commit_pending`).
#[cfg(feature = "fetch")]
struct PendingDraft {
    name: String,
    /// `(rev, missing)` watermarks to write.
    done: Vec<(String, bool)>,
    /// Rebuilt chain id to repoint the inventory row at, atomically
    /// with the watermarks.
    repoint: Option<u64>,
}

/// Store-flush + watermark-tx cadence, in drafts. Bounds both the fsync
/// rate (one flush per batch instead of per draft) and the redo work a
/// crash can leave (a batch's worth of chains to reconcile).
#[cfg(feature = "fetch")]
const FLUSH_EVERY_DRAFTS: usize = 64;

/// The stored crash-window flag ('dirty' in `fetch_meta`); absent (a
/// pre-flag or never-updated mirror) reads as clean.
fn read_dirty_flag(conn: &Connection) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let v: Option<String> = conn
        .query_row("SELECT value FROM fetch_meta WHERE key = 'dirty'", [], |r| r.get(0))
        .optional()?;
    Ok(v.as_deref() == Some("1"))
}

/// Draft base name → `(latest rev, index date)`. The live index lists
/// each draft exactly once, at its latest revision.
pub type DraftIndex = BTreeMap<String, (String, Option<String>)>;

/// Fetch and parse `/id/all_id.txt` unconditionally (no validators).
/// [`Mirror::update`] uses the conditional variant instead.
#[cfg(feature = "fetch")]
pub fn fetch_index(client: &Client, cfg: &FetchConfig) -> Result<DraftIndex> {
    let url = format!("{}/id/all_id.txt", cfg.base_url);
    let body = http_get(client, &url, cfg)?.ok_or(Error::HttpStatus { status: 404, url })?;
    Ok(parse_index(&String::from_utf8_lossy(&body)))
}

/// Parse `/id/all_id.txt`: the authoritative docname index. Lines are
/// `draft-...-NN<TAB>date<TAB>status...`, ONE line per draft carrying
/// its LATEST revision (~43k drafts live); `#` lines and names without
/// a two-digit `-NN` suffix are skipped. Were a draft ever repeated,
/// the highest revision wins.
pub fn parse_index(text: &str) -> DraftIndex {
    let mut out: DraftIndex = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let docname = fields.next().unwrap_or("").trim();
        let date = fields.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let Some((base, rev)) = split_rev(docname) else { continue };
        match out.entry(base.to_string()) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert((rev.to_string(), date));
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                if rev > o.get().0.as_str() {
                    o.insert((rev.to_string(), date));
                }
            }
        }
    }
    out
}

/// `draft-x-y-NN` → `("draft-x-y", "NN")`; `None` if there is no
/// two-digit revision suffix.
fn split_rev(docname: &str) -> Option<(&str, &str)> {
    let (base, rev) = docname.rsplit_once('-')?;
    if rev.len() == 2 && rev.bytes().all(|b| b.is_ascii_digit()) && !base.is_empty() {
        Some((base, rev))
    } else {
        None
    }
}

/// The canonical layer for one revision: root → child `text` with the
/// body as blob and `rev`/`date` attrs. Full snapshot — see module doc.
#[cfg(feature = "fetch")]
fn revision_layer(rev: &str, date: Option<&str>, text: &[u8]) -> Layer {
    let mut attrs = BTreeMap::new();
    attrs.insert(b"rev".to_vec(), rev.as_bytes().to_vec());
    if let Some(d) = date {
        attrs.insert(b"date".to_vec(), d.as_bytes().to_vec());
    }
    let mut root = Node::keep();
    // Opaque root: each layer is a self-contained snapshot, masking any
    // lower record when layers are stacked as deltas elsewhere.
    root.opaque = true;
    root.children.insert(
        b"text".to_vec(),
        Node {
            blob: BlobOp::Set(text.to_vec().into()),
            attrs: Some(attrs),
            ..Node::keep()
        },
    );
    Layer { root }
}

/// The `rev` attr alone — no text copy (the reconcile/scan hot path).
fn layer_rev(layer: &Layer, chain_id: u64) -> Result<String> {
    let rev = layer
        .root
        .children
        .get(b"text".as_slice())
        .ok_or(Error::CorruptLayer { chain_id, what: "no text node" })?
        .attrs
        .as_ref()
        .ok_or(Error::CorruptLayer { chain_id, what: "no attrs" })?
        .get(b"rev".as_slice())
        .ok_or(Error::CorruptLayer { chain_id, what: "no rev attr" })?;
    Ok(String::from_utf8_lossy(rev).into_owned())
}

fn decode_revision_layer(layer: &Layer, chain_id: u64) -> Result<RevisionEntry> {
    let node = layer
        .root
        .children
        .get(b"text".as_slice())
        .ok_or(Error::CorruptLayer { chain_id, what: "no text node" })?;
    let BlobOp::Set(text) = &node.blob else {
        return Err(Error::CorruptLayer { chain_id, what: "text blob not Set" });
    };
    let attrs = node
        .attrs
        .as_ref()
        .ok_or(Error::CorruptLayer { chain_id, what: "no attrs" })?;
    let rev = attrs
        .get(b"rev".as_slice())
        .ok_or(Error::CorruptLayer { chain_id, what: "no rev attr" })?;
    Ok(RevisionEntry {
        rev: String::from_utf8_lossy(rev).into_owned(),
        date: attrs
            .get(b"date".as_slice())
            .map(|d| String::from_utf8_lossy(d).into_owned()),
        text: text.to_vec(),
    })
}

/// Inter-request pacing for content GETs: `pace()` before each GET
/// sleeps out the remainder of `delay` since the previous one.
#[cfg(feature = "fetch")]
struct Pacer {
    delay: Duration,
    last: Option<std::time::Instant>,
}

#[cfg(feature = "fetch")]
impl Pacer {
    fn new(delay: Duration) -> Self {
        Pacer { delay, last: None }
    }

    fn pace(&mut self) {
        if let Some(t) = self.last {
            let elapsed = t.elapsed();
            if elapsed < self.delay {
                std::thread::sleep(self.delay - elapsed);
            }
        }
        self.last = Some(std::time::Instant::now());
    }
}

/// Run `f` with up to `cfg.retries` retries on retryable failures
/// (transport errors, 5xx), exponential backoff between attempts.
#[cfg(feature = "fetch")]
fn retrying<T>(cfg: &FetchConfig, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 0u32;
    loop {
        match f() {
            Err(e) if attempt < cfg.retries && retryable(&e) => {
                std::thread::sleep(cfg.backoff * 2u32.saturating_pow(attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Transient failures worth re-trying: transport errors (timeouts,
/// resets) and 5xx. 4xx is deterministic — 404 is the watermark signal
/// (handled before this is reached), the rest fail the run loudly.
#[cfg(feature = "fetch")]
fn retryable(e: &Error) -> bool {
    match e {
        Error::Http(_) => true,
        Error::HttpStatus { status, .. } => *status >= 500,
        _ => false,
    }
}

/// GET with retries: `Ok(None)` on 404, the body on 2xx, an error
/// otherwise (5xx only after the retry budget is exhausted).
#[cfg(feature = "fetch")]
fn http_get(client: &Client, url: &str, cfg: &FetchConfig) -> Result<Option<Vec<u8>>> {
    retrying(cfg, || {
        let resp = client.get(url).send()?;
        let status = resp.status();
        if status.as_u16() == 404 {
            let _ = resp.bytes();
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Error::HttpStatus { status: status.as_u16(), url: url.to_string() });
        }
        let mut body = Vec::new();
        let mut r = resp;
        r.read_to_end(&mut body)?;
        Ok(Some(body))
    })
}
