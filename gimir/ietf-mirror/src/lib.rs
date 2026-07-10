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
//! Durability handshake per draft, same shape as wikimak's sync:
//! import all unseen revisions → depot flush → watermark the revisions.
//! A crash before the watermark re-fetches those revisions on the next
//! run; if the chain's head already covers them, the re-run lands on
//! the rebuild path (see `Mirror::update`) and the merge dedupes — the
//! watermark write is tiny, so the window is, too; `update` is
//! idempotent across completed drafts either way.

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
    // HTTP validators (index ETag / Last-Modified) for conditional
    // re-fetch; written only after a fully successful update pass.
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
}

impl Mirror {
    /// Open or create the mirror at `cfg.root` (`meta.db` + `depot/`).
    /// Re-open is idempotent.
    pub fn open(cfg: MirrorConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;
        let store = VbfDepot::open(cfg.root.join("depot"), cfg.max_chain_id, cfg.seal_threshold)?;
        // One-process-per-root guard: exclusive flock on <root>/.lock,
        // held for the Mirror's lifetime, kernel-released on any exit.
        // External readers of meta.db stay possible.
        let lock = {
            use std::os::fd::AsRawFd;
            let f = std::fs::OpenOptions::new()
                .create(true).truncate(false).write(true)
                .open(cfg.root.join(".lock"))?;
            let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                return Err(Error::MirrorLocked(cfg.root.clone()));
            }
            f
        };

        let conn = Connection::open(cfg.root.join("meta.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        for stmt in DDL {
            conn.execute(stmt, [])?;
        }
        Ok(Mirror { conn, store, max_chain_id: cfg.max_chain_id, _lock: lock })
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
        let mut stats = UpdateStats::default();
        let Some((index, etag, last_modified)) = self.fetch_index_conditional(client, cfg)?
        else {
            stats.index_not_modified = true;
            return Ok(stats);
        };
        let mut pacer = Pacer::new(cfg.delay);
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
            let chain_id = match self.chain_id(name)? {
                Some(id) => id,
                None => {
                    stats.drafts_new += 1;
                    self.alloc_chain(name)?
                }
            };
            // Oldest→newest; the whole draft lands as ONE batch
            // prepend (the normative multi-record form — one f0 swap,
            // one f1 re-encode), not a per-revision cycle. Fetch
            // errors abort before any store write: idempotent refetch.
            let mut done: Vec<(&str, bool)> = Vec::new();
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
                        done.push((rev.as_str(), false));
                    }
                    None => {
                        // Enumerated but gone (expired from the
                        // archive). Watermark as missing; never re-try.
                        stats.revisions_missing += 1;
                        done.push((rev.as_str(), true));
                    }
                }
            }
            // Newest-first chains grow at the HEAD with NEWER records:
            // `put_layers` makes the batch's LAST layer the new head.
            // The batch is ascending, so on a fresh chain or a head
            // bump the order is right by construction. If the chain's
            // head is already NEWER than the batch's oldest revision (a
            // legacy heads-only mirror being backfilled, or crash
            // recovery), prepending would corrupt the order — rebuild
            // the whole draft in order onto a fresh chain instead.
            let head_rev = self
                .store
                .head_layer(chain_id)?
                .map(|l| decode_revision_layer(&l, chain_id))
                .transpose()?
                .map(|e| e.rev);
            let needs_rebuild =
                matches!((&head_rev, batch.first()), (Some(h), Some((r, _))) if r < h);
            let repoint = if needs_rebuild {
                stats.chains_rebuilt += 1;
                Some(self.rebuild_chain(chain_id, batch)?)
            } else {
                let layers: Vec<Layer> = batch.into_iter().map(|(_, l)| l).collect();
                self.store.put_layers(chain_id, &layers)?;
                None
            };
            // Durability fence: bytes first, watermarks (and the chain
            // repoint, atomically with them) after.
            self.store.flush()?;
            let tx = self.conn.transaction()?;
            if let Some(new_chain) = repoint {
                tx.execute(
                    "UPDATE drafts SET chain_id = ?2 WHERE name = ?1",
                    rusqlite::params![name, new_chain as i64],
                )?;
            }
            for (rev, missing) in done {
                tx.execute(
                    "INSERT OR REPLACE INTO revisions_seen(name, rev, missing) VALUES(?1,?2,?3)",
                    rusqlite::params![name, rev, missing as i64],
                )?;
            }
            tx.commit()?;
        }
        // Persist the index validators only after a FULLY successful
        // pass: an interrupted run must re-see the full index next time
        // (and resume via watermarks), never be 304'd past unfinished
        // drafts.
        self.set_index_validators(etag.as_deref(), last_modified.as_deref())?;
        Ok(stats)
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
        let mut st = self.conn.prepare("SELECT chain_id FROM drafts WHERE name = ?1")?;
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
