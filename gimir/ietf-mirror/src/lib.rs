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
//! Durability handshake per draft, same shape as wikimak's sync:
//! import all unseen revisions → depot flush → watermark the revisions.
//! A crash before the watermark re-fetches those revisions into a
//! duplicate prepend — the watermark write is tiny, so the window is,
//! too; `update` is idempotent across completed drafts either way.

pub mod readout;
#[cfg(feature = "fetch")]
mod cli;
#[cfg(feature = "fetch")]
pub use cli::cli_main;

use std::collections::BTreeMap;
#[cfg(feature = "fetch")]
use std::io::Read;
use std::path::PathBuf;

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
/// point `base_url` at an httpmock server (the same seam as
/// `wikimak_mediawiki::Config`).
#[derive(Debug, Clone)]
pub struct FetchConfig {
    pub base_url: String,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self { base_url: "https://www.ietf.org".to_string() }
    }
}

/// Store sizing at open time.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    pub root: PathBuf,
    /// Chain-id capacity ≙ maximum number of draft series. The live
    /// corpus is ~140k series; the default leaves headroom.
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
    /// Revisions listed in the index whose text GET returned 404 —
    /// recorded as seen-but-missing so they are not re-tried forever.
    pub revisions_missing: u64,
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

    /// Discover + fetch + import every unseen revision of every draft in
    /// the index. Idempotent and resumable: completed drafts are skipped
    /// by their revision watermarks. `progress` gets `(draft-rev,
    /// fetched)` per revision considered.
    #[cfg(feature = "fetch")]
    pub fn update(
        &mut self,
        client: &Client,
        cfg: &FetchConfig,
        mut progress: impl FnMut(&str, bool),
    ) -> Result<UpdateStats> {
        let index = fetch_index(client, cfg)?;
        let mut stats = UpdateStats::default();
        for (name, revs) in &index {
            stats.drafts_seen += 1;
            let mut fresh: Vec<&(String, Option<String>)> = Vec::new();
            for r in revs {
                if self.rev_seen(name, &r.0)? {
                    stats.revisions_skipped += 1;
                    progress(&format!("{name}-{}", r.0), false);
                } else {
                    fresh.push(r);
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
            let mut batch: Vec<Layer> = Vec::new();
            for (rev, date) in fresh {
                let label = format!("{name}-{rev}");
                progress(&label, true);
                let url = format!("{}/archive/id/{label}.txt", cfg.base_url);
                match http_get(client, &url)? {
                    Some(text) => {
                        batch.push(revision_layer(rev, date.as_deref(), &text));
                        stats.revisions_fetched += 1;
                        done.push((rev, false));
                    }
                    None => {
                        // Listed but gone (ancient drafts pre-date the
                        // archive). Watermark as missing; never re-try.
                        stats.revisions_missing += 1;
                        done.push((rev, true));
                    }
                }
            }
            self.store.put_layers(chain_id, &batch)?;
            // Durability fence: bytes first, watermarks after.
            self.store.flush()?;
            let tx = self.conn.transaction()?;
            for (rev, missing) in done {
                tx.execute(
                    "INSERT OR REPLACE INTO revisions_seen(name, rev, missing) VALUES(?1,?2,?3)",
                    rusqlite::params![name, rev, missing as i64],
                )?;
            }
            tx.commit()?;
        }
        Ok(stats)
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
        let next: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(chain_id) + 1, 0) FROM drafts", [], |r| r.get(0))?;
        if next as u64 >= self.max_chain_id {
            return Err(Error::InventoryFull(self.max_chain_id));
        }
        self.conn
            .execute("INSERT INTO drafts(name, chain_id) VALUES(?1, ?2)", rusqlite::params![name, next])?;
        Ok(next as u64)
    }

    #[cfg(feature = "fetch")]
    fn rev_seen(&self, name: &str, rev: &str) -> Result<bool> {
        let n: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM revisions_seen WHERE name = ?1 AND rev = ?2",
            rusqlite::params![name, rev],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

/// Draft base name → its revisions ASCENDING as `(rev, date)`.
pub type DraftIndex = BTreeMap<String, Vec<(String, Option<String>)>>;

/// Fetch and parse `/id/all_id.txt`: the authoritative docname index.
/// Lines are `draft-...-NN<TAB>date<TAB>status...`; `#` lines and names
/// without a `-NN` suffix are skipped.
#[cfg(feature = "fetch")]
pub fn fetch_index(client: &Client, cfg: &FetchConfig) -> Result<DraftIndex> {
    let url = format!("{}/id/all_id.txt", cfg.base_url);
    let body = http_get(client, &url)?.ok_or(Error::HttpStatus { status: 404, url })?;
    let text = String::from_utf8_lossy(&body);
    let mut out: BTreeMap<String, Vec<(String, Option<String>)>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let docname = fields.next().unwrap_or("").trim();
        let date = fields.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let Some((base, rev)) = split_rev(docname) else { continue };
        out.entry(base.to_string())
            .or_default()
            .push_ascending(rev.to_string(), date);
    }
    Ok(out)
}

/// `draft-x-y-NN` → `("draft-x-y", "NN")`; `None` if there is no
/// two-digit revision suffix.
#[cfg(feature = "fetch")]
fn split_rev(docname: &str) -> Option<(&str, &str)> {
    let (base, rev) = docname.rsplit_once('-')?;
    if rev.len() == 2 && rev.bytes().all(|b| b.is_ascii_digit()) && !base.is_empty() {
        Some((base, rev))
    } else {
        None
    }
}

/// Insert keeping the revision list ascending and deduped (the index
/// occasionally repeats a docname).
#[cfg(feature = "fetch")]
trait PushAscending {
    fn push_ascending(&mut self, rev: String, date: Option<String>);
}

#[cfg(feature = "fetch")]
impl PushAscending for Vec<(String, Option<String>)> {
    fn push_ascending(&mut self, rev: String, date: Option<String>) {
        match self.binary_search_by(|(r, _)| r.as_str().cmp(rev.as_str())) {
            Ok(_) => {}
            Err(i) => self.insert(i, (rev, date)),
        }
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

/// GET returning `Ok(None)` on 404, the body on 2xx, an error otherwise.
#[cfg(feature = "fetch")]
fn http_get(client: &Client, url: &str) -> Result<Option<Vec<u8>>> {
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
}
