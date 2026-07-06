//! Store v2 — three tiered chains + stable indices (ATTACH-CONVERGENCE.md
//! chip 7, design of record 2026-07-06).
//!
//! `<store>/depot/`      — ONE wikimak-depot instance holding three chains:
//!                         TREES=0, COMMITS=1, REFLOG=2.
//! `<store>/meta.sqlite` — bookkeeping (WAL): `kv` (schema=2, label, url,
//!                         n_trees/n_commits/n_reflog counts — the
//!                         AUTHORITATIVE index base), `refs` (CURRENT refs
//!                         only: name → commit_idx, tree_idx), `sha_idx`
//!                         (sha → commit_idx; DERIVED import-dedup index,
//!                         rebuildable from the commits chain), `tree_hash`
//!                         (sha256 of a tree's canonical full-view bytes →
//!                         tree_idx; DERIVED dedup index, rebuildable,
//!                         never in any record).
//!
//! **Stable indices**: records are numbered from the OLDEST end. Record k
//! of a chain whose kv count is N sits at newest-first walk position
//! N-1-k. Prepends only grow N, so an index never changes — lineage
//! (commit parent edges, refs, reflog) is carried as indices in the data,
//! not by storage topology. A rewrite upstream is just new records +
//! repointed refs; old records keep their indices forever.
//!
//! **Frames** (caller-side discipline over the byte-opaque depot, per
//! wikimak/depot/SPEC.md):
//!   * f0 = the chain's newest record, standalone zstd.
//!   * f1 = older records, each u32-length-prefixed, concatenated
//!     newest-first, zstd refPrefix-anchored on the f0 RECORD.
//!   * seal: when absorbing a prepend's entries would push the raw
//!     accumulator past `SEAL_THRESHOLD`, the old f1's zstd bytes move
//!     verbatim to a cold frame and the accumulator restarts.
//!
//! TREES chain records are REVERSE DELTAS: only the head (f0) record is
//! the full layer `codec::encode(diff(None, head_view))`; record k < head
//! is `codec::encode(diff(Some(view_{k+1}), Some(view_k)))` — the delta
//! that rebuilds the older tree from the next-newer one. On prepend the
//! former head's full record is REPLACED in the accumulator by its bridge
//! delta. Cold-frame anchors for TREES are the canonical full-view bytes
//! at the frame boundary, recomputed by the decoder from the walked view
//! (bit-exact canonical encoding is load-bearing — encoder and decoder
//! share `codec::encode(diff(None, view))`). Fetching tree k therefore
//! walks head→k applying deltas (O(distance from tip)); the tip itself is
//! one standalone frame. COMMITS/REFLOG records stand alone, so their
//! demotion is verbatim and cold anchors are simply the last record
//! decoded (the wikipedia discipline).
//!
//! **Durability/integrity**: depot writes are flushed durable BEFORE the
//! sqlite transaction commits, so kv counts are never ahead of the
//! chains. COMMITS/REFLOG records embed their own stable index; on open
//! the head record of each non-empty chain must carry idx == count-1 or
//! the store errors loudly (a crash between depot flush and sqlite commit
//! can leave orphan newer frames — detected, not auto-repaired). TREES
//! records are pure codec bytes (they double as refPrefix anchors), so
//! the trees chain is cross-checked through the head commit's tree_idx
//! bound and verified in depth by any walk.
//!
//! A v1 store (flat `chain` file + schema=1 meta.sqlite or legacy
//! meta.json) is migrated to v2 in full on ANY open — read paths
//! included — after which the v1 chain file and bookkeeping are removed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use sha2::Digest as _;
use wikimak_depot::{Depot, DepotConfig};

use crate::{CommitMeta, Error, Meta, RefMeta, Result};

pub mod legacy;

pub const TREES: u64 = 0;
pub const COMMITS: u64 = 1;
pub const REFLOG: u64 = 2;
const MAX_CHAIN_ID: u64 = 3;

/// Raw (decompressed) f1 accumulator seal point, per chain.
const SEAL_THRESHOLD: u64 = 256 * 1024;
// Small on purpose: eviction cannot touch the CURRENT write-target
// file, so this threshold IS the dead-byte ceiling per tier — and the
// trees chain's f0 frames are whole-head-sized, deadening fast. The
// moderate-repo bench measured 32MiB here as ~2.7x the useful store.
const FILE_SIZE_THRESHOLD: u64 = 4 << 20;
const EVICTION_DEAD_RATIO: f32 = 0.5;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS kv(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS refs(
    name       TEXT PRIMARY KEY,
    commit_idx INTEGER NOT NULL,
    tree_idx   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sha_idx(
    sha        TEXT PRIMARY KEY,
    commit_idx INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sha_idx_rev ON sha_idx(commit_idx);
CREATE TABLE IF NOT EXISTS tree_hash(
    hash     BLOB PRIMARY KEY,
    tree_idx INTEGER NOT NULL
);
";

pub(crate) fn sql_err(e: rusqlite::Error) -> Error {
    Error::Meta(e.to_string())
}

fn db_path(store: &Path) -> PathBuf {
    store.join("meta.sqlite")
}

/// True when `store` holds a store in ANY format (v2, v1 sqlite, or
/// legacy meta.json) — "is there a store here?".
pub fn store_exists(store: &Path) -> bool {
    db_path(store).exists() || store.join("meta.json").exists()
}

fn configure(conn: &Connection) -> Result<()> {
    // sha-prefix resolution goes through LIKE; git shas are
    // case-sensitive, and case-sensitive LIKE can use the sha index.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\n\
         PRAGMA case_sensitive_like=ON;",
    )
    .map_err(sql_err)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// -------------------------------------------------------------- records

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() - self.i < n {
            return Err(Error::Chain("truncated record".into()));
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn done(&self) -> Result<()> {
        if self.i != self.b.len() {
            return Err(Error::Chain("record has trailing bytes".into()));
        }
        Ok(())
    }
}

/// One COMMITS-chain record. Lineage is `parent_idxs` — stable indices,
/// not shas; the sha is kept as the export-fidelity payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub idx: u64,
    pub sha: String,
    pub tree_idx: u64,
    pub parent_idxs: Vec<u64>,
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
    pub extra_headers: Vec<String>,
    /// Complete raw commit object, kept ONLY when extra_headers is
    /// non-empty (redundant otherwise).
    pub raw: Vec<u8>,
}

impl CommitRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.idx.to_le_bytes());
        put_bytes(&mut out, self.sha.as_bytes());
        out.extend_from_slice(&self.tree_idx.to_le_bytes());
        out.extend_from_slice(&(self.parent_idxs.len() as u32).to_le_bytes());
        for p in &self.parent_idxs {
            out.extend_from_slice(&p.to_le_bytes());
        }
        put_bytes(&mut out, &self.author);
        put_bytes(&mut out, &self.committer);
        put_bytes(&mut out, &self.message);
        out.extend_from_slice(&(self.extra_headers.len() as u32).to_le_bytes());
        for h in &self.extra_headers {
            put_bytes(&mut out, h.as_bytes());
        }
        put_bytes(&mut out, &self.raw);
        out
    }

    pub fn decode(b: &[u8]) -> Result<CommitRecord> {
        let mut c = Cur::new(b);
        let idx = c.u64()?;
        let sha = String::from_utf8(c.bytes()?.to_vec())
            .map_err(|_| Error::Chain("commit record: non-utf8 sha".into()))?;
        let tree_idx = c.u64()?;
        let n = c.u32()? as usize;
        let mut parent_idxs = Vec::with_capacity(n);
        for _ in 0..n {
            parent_idxs.push(c.u64()?);
        }
        let author = c.bytes()?.to_vec();
        let committer = c.bytes()?.to_vec();
        let message = c.bytes()?.to_vec();
        let n = c.u32()? as usize;
        let mut extra_headers = Vec::with_capacity(n);
        for _ in 0..n {
            extra_headers.push(
                String::from_utf8(c.bytes()?.to_vec())
                    .map_err(|_| Error::Chain("commit record: non-utf8 header".into()))?,
            );
        }
        let raw = c.bytes()?.to_vec();
        c.done()?;
        Ok(CommitRecord {
            idx,
            sha,
            tree_idx,
            parent_idxs,
            author,
            committer,
            message,
            extra_headers,
            raw,
        })
    }
}

/// One REFLOG-chain record: an observed ref movement. `old` absent =
/// creation; `new` absent = deletion. Values are `(commit_idx,
/// tree_idx)` pairs. Prepended BEFORE the refs table row changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogRecord {
    pub idx: u64,
    pub at: i64,
    pub refname: String,
    pub old: Option<(u64, u64)>,
    pub new: Option<(u64, u64)>,
    pub note: String,
}

impl ReflogRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.idx.to_le_bytes());
        out.extend_from_slice(&(self.at as u64).to_le_bytes());
        put_bytes(&mut out, self.refname.as_bytes());
        let flags = self.old.is_some() as u8 | ((self.new.is_some() as u8) << 1);
        out.push(flags);
        if let Some((c, t)) = self.old {
            out.extend_from_slice(&c.to_le_bytes());
            out.extend_from_slice(&t.to_le_bytes());
        }
        if let Some((c, t)) = self.new {
            out.extend_from_slice(&c.to_le_bytes());
            out.extend_from_slice(&t.to_le_bytes());
        }
        put_bytes(&mut out, self.note.as_bytes());
        out
    }

    pub fn decode(b: &[u8]) -> Result<ReflogRecord> {
        let mut c = Cur::new(b);
        let idx = c.u64()?;
        let at = c.u64()? as i64;
        let refname = String::from_utf8(c.bytes()?.to_vec())
            .map_err(|_| Error::Chain("reflog record: non-utf8 ref".into()))?;
        let flags = c.u8()?;
        let old = if flags & 1 != 0 { Some((c.u64()?, c.u64()?)) } else { None };
        let new = if flags & 2 != 0 { Some((c.u64()?, c.u64()?)) } else { None };
        let note = String::from_utf8(c.bytes()?.to_vec())
            .map_err(|_| Error::Chain("reflog record: non-utf8 note".into()))?;
        c.done()?;
        Ok(ReflogRecord { idx, at, refname, old, new, note })
    }
}

// --------------------------------------------------------------- frames

fn zstd_err(code: zstd::zstd_safe::ErrorCode) -> Error {
    Error::Chain(zstd::zstd_safe::get_error_name(code).to_string())
}

pub(crate) fn compress(src: &[u8], prefix: Option<&[u8]>, level: i32) -> Result<Vec<u8>> {
    let mut cctx = zstd::zstd_safe::CCtx::create();
    cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
        .map_err(zstd_err)?;
    if let Some(p) = prefix {
        cctx.ref_prefix(p).map_err(zstd_err)?;
    }
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(src.len()));
    cctx.compress2(&mut out, src).map_err(zstd_err)?;
    Ok(out)
}

pub(crate) fn decompress(frame: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>> {
    let raw_len = zstd::zstd_safe::get_frame_content_size(frame)
        .map_err(|_| Error::Chain("zstd frame content size".into()))?
        .ok_or_else(|| Error::Chain("zstd frame without content size".into()))?
        as usize;
    let mut dctx = zstd::zstd_safe::DCtx::create();
    if let Some(p) = prefix {
        dctx.ref_prefix(p).map_err(zstd_err)?;
    }
    let mut out = Vec::with_capacity(raw_len);
    dctx.decompress(&mut out, frame).map_err(zstd_err)?;
    Ok(out)
}

/// Split a decompressed accumulator into its u32-length-prefixed
/// records, in stored (newest-first) order.
fn split_records(buf: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut c = Cur::new(buf);
    while c.i < buf.len() {
        out.push(c.bytes()?.to_vec());
    }
    Ok(out)
}

fn frame_entry(rec: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rec.len() + 4);
    put_bytes(&mut out, rec);
    out
}

/// How the former head record joins the accumulator on prepend.
pub(crate) enum Demote<'a> {
    /// Record stands alone (COMMITS, REFLOG): moves in verbatim.
    Verbatim,
    /// Record is superseded by a caller-computed replacement (TREES:
    /// the bridge delta rebuilding the old head view from the new one).
    Replace(&'a [u8]),
}

// ---------------------------------------------------------------- store

pub struct Store {
    depot: Depot,
    pub(crate) conn: Connection,
    root: PathBuf,
}

impl Store {
    /// Create a fresh, empty v2 store. Errors if one is already there.
    pub fn create(store: &Path) -> Result<Store> {
        if store_exists(store) || store.join("chain").exists() {
            return Err(Error::Chain(format!(
                "store {} already populated",
                store.display()
            )));
        }
        std::fs::create_dir_all(store.join("depot"))?;
        let depot = open_depot(store)?;
        let conn = Connection::open(db_path(store)).map_err(sql_err)?;
        configure(&conn)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        let s = Store { depot, conn, root: store.to_path_buf() };
        let tx = s.conn.unchecked_transaction().map_err(sql_err)?;
        for (k, v) in [
            ("schema", "2"),
            ("label", ""),
            ("url", ""),
            ("n_trees", "0"),
            ("n_commits", "0"),
            ("n_reflog", "0"),
        ] {
            kv_set(&tx, k, v)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(s)
    }

    /// Open an existing store. A v1 store is migrated to v2 first (the
    /// flat chain + v1 bookkeeping are consumed and removed).
    pub fn open(store: &Path) -> Result<Store> {
        if !store_exists(store) {
            return Err(Error::Chain(format!("no store at {}", store.display())));
        }
        if schema_version(store)? < 2 {
            legacy::migrate(store)?;
        }
        let depot = open_depot(store)?;
        let conn = Connection::open_with_flags(
            db_path(store),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(sql_err)?;
        configure(&conn)?;
        let s = Store { depot, conn, root: store.to_path_buf() };
        s.integrity_check()?;
        Ok(s)
    }

    /// Loud count/chain agreement check (see module doc): the head
    /// record of each self-indexing chain must carry idx == count-1.
    fn integrity_check(&self) -> Result<()> {
        for (chain, name, count) in [
            (COMMITS, "commits", self.count(COMMITS)?),
            (REFLOG, "reflog", self.count(REFLOG)?),
        ] {
            if count == 0 {
                continue;
            }
            let head = self
                .read_head(chain)?
                .ok_or_else(|| Error::Chain(format!("{name}: count {count} but empty chain")))?;
            let idx = Cur::new(&head).u64()?;
            if idx != count - 1 {
                return Err(Error::Chain(format!(
                    "{name}: head record idx {idx} != count-1 ({}) — chains and \
                     bookkeeping disagree (crash between depot flush and sqlite \
                     commit?); re-mirror the store",
                    count - 1
                )));
            }
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `prepend` calls on the underlying depot since open —
    /// instrumentation for the batch-prepend invariant.
    pub fn depot_prepends(&self) -> u64 {
        self.depot.prepend_count()
    }

    // ------------------------------------------------------------ kv

    pub fn count(&self, chain: u64) -> Result<u64> {
        let key = count_key(chain);
        Ok(kv_get(&self.conn, key)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    pub fn identity(&self) -> Result<(String, String)> {
        Ok((
            kv_get(&self.conn, "label")?.unwrap_or_default(),
            kv_get(&self.conn, "url")?.unwrap_or_default(),
        ))
    }

    pub fn set_identity(&self, label: &str, url: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(sql_err)?;
        kv_set(&tx, "label", label)?;
        kv_set(&tx, "url", url)?;
        tx.commit().map_err(sql_err)
    }

    // ------------------------------------------------------- chain ops

    /// The chain's decompressed head (f0) record; `None` = empty chain.
    pub(crate) fn read_head(&self, chain: u64) -> Result<Option<Vec<u8>>> {
        match self.depot.read_f0(chain) {
            Ok(frame) => Ok(Some(decompress(&frame, None)?)),
            Err(wikimak_depot::Error::NoFrame) => Ok(None),
            Err(e) => Err(Error::Chain(e.to_string())),
        }
    }

    /// Prepend a batch of records to `chain`: `head_record` becomes the
    /// new f0; `older_entries` (newest-first, already in RECORD form —
    /// for TREES these are reverse deltas) plus the demoted former head
    /// join the accumulator. ONE depot prepend for the whole batch (two
    /// on a previously-empty chain, which must be seeded — the depot
    /// forbids f1 on a chain's first prepend).
    pub(crate) fn prepend_batch(
        &self,
        chain: u64,
        head_record: &[u8],
        older_entries: &[Vec<u8>],
        demote: Demote<'_>,
        level: i32,
    ) -> Result<()> {
        let (prev_record, older) = match self.read_head(chain)? {
            Some(p) => (p, older_entries),
            None => {
                if older_entries.is_empty() {
                    self.depot
                        .prepend(chain, &compress(head_record, None, level)?, None, false)
                        .map_err(|e| Error::Chain(e.to_string()))?;
                    return Ok(());
                }
                // Seed the empty chain with the OLDEST record. Only
                // meaningful for standalone-record chains: a TREES seed
                // would need a full record, and its entries are deltas.
                if matches!(demote, Demote::Replace(_)) {
                    return Err(Error::Chain(
                        "batch prepend on an empty delta chain (seed it first)".into(),
                    ));
                }
                let (oldest, rest) = older_entries.split_last().expect("non-empty");
                self.depot
                    .prepend(chain, &compress(oldest, None, level)?, None, false)
                    .map_err(|e| Error::Chain(e.to_string()))?;
                (oldest.clone(), rest)
            }
        };
        let demoted = match demote {
            Demote::Verbatim => prev_record.clone(),
            Demote::Replace(b) => b.to_vec(),
        };
        let old_f1_raw = match self.depot.read_f1(chain).map_err(|e| Error::Chain(e.to_string()))? {
            Some(z) => Some(decompress(&z, Some(&prev_record))?),
            None => None,
        };
        let mut entries: Vec<Vec<u8>> = older.iter().map(|r| frame_entry(r)).collect();
        entries.push(frame_entry(&demoted));
        let refs: Vec<&[u8]> = entries.iter().map(|e| e.as_slice()).collect();
        let (new_f1_raw, seal) =
            wikimak_depot::compose_f1(&refs, old_f1_raw.as_deref(), SEAL_THRESHOLD);
        let new_f0 = compress(head_record, None, level)?;
        let new_f1 = compress(&new_f1_raw, Some(head_record), level)?;
        self.depot
            .prepend(chain, &new_f0, Some(&new_f1), seal)
            .map_err(|e| Error::Chain(e.to_string()))
    }

    /// All records of a standalone-record chain (COMMITS, REFLOG),
    /// newest-first. Anchors: f1 on the f0 record; each cold frame on
    /// the last (oldest) record decoded before it.
    pub(crate) fn records_newest_first(&self, chain: u64) -> Result<Vec<Vec<u8>>> {
        let Some(head) = self.read_head(chain)? else {
            return Ok(Vec::new());
        };
        let mut out = vec![head];
        if let Some(f1) = self.depot.read_f1(chain).map_err(|e| Error::Chain(e.to_string()))? {
            let raw = decompress(&f1, Some(&out[0]))?;
            out.extend(split_records(&raw)?);
        }
        for cold in self.depot.cold_iter(chain).map_err(|e| Error::Chain(e.to_string()))? {
            let frame = cold.map_err(|e| Error::Chain(e.to_string()))?;
            let anchor = out.last().expect("cold after f1").clone();
            let raw = decompress(&frame, Some(&anchor))?;
            out.extend(split_records(&raw)?);
        }
        Ok(out)
    }

    /// Walk the TREES chain newest-first, reconstructing views into ONE
    /// working view mutated in place per record (O(delta) per step —
    /// only the frame-boundary anchor re-encode is O(tree), once per
    /// cold frame). `visit(pos, record, view)` per position; stops after
    /// newest-first position `until_pos` when given. Cold anchors are
    /// the canonical full-view bytes at the frame boundary, recomputed
    /// from the walked view. Public for the read-fidelity tests.
    #[doc(hidden)]
    pub fn walk_tree_views(
        &self,
        until_pos: Option<usize>,
        visit: &mut dyn FnMut(usize, &[u8], &depot::View),
    ) -> Result<()> {
        let Some(head) = self.read_head(TREES)? else {
            return Ok(());
        };
        let mut cur: Option<depot::View> = None;
        let mut pos: usize = 0;
        let mut step = |cur: &mut Option<depot::View>, pos: &mut usize, rec: &[u8]| -> Result<bool> {
            let layer = depot::codec::decode(rec)?;
            depot::apply_mut(cur, &layer);
            let view = cur.as_ref().ok_or_else(|| {
                Error::Chain(format!("tree frame {pos} resolves to nothing"))
            })?;
            visit(*pos, rec, view);
            *pos += 1;
            Ok(until_pos.is_some_and(|p| *pos > p))
        };
        if step(&mut cur, &mut pos, &head)? {
            return Ok(());
        }
        if let Some(f1) = self.depot.read_f1(TREES).map_err(|e| Error::Chain(e.to_string()))? {
            let raw = decompress(&f1, Some(&head))?;
            for rec in split_records(&raw)? {
                if step(&mut cur, &mut pos, &rec)? {
                    return Ok(());
                }
            }
        }
        for cold in self.depot.cold_iter(TREES).map_err(|e| Error::Chain(e.to_string()))? {
            let frame = cold.map_err(|e| Error::Chain(e.to_string()))?;
            let anchor =
                depot::codec::encode(&depot::diff(None, cur.as_ref()));
            let raw = decompress(&frame, Some(&anchor))?;
            for rec in split_records(&raw)? {
                if step(&mut cur, &mut pos, &rec)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// All walked views newest-first (down to `until_pos` inclusive when
    /// given) — O(count × tree) RAM by construction; only for callers
    /// that genuinely need every view (export, migration fixtures).
    pub fn tree_views(&self, until_pos: Option<usize>) -> Result<Vec<depot::View>> {
        let mut views = Vec::new();
        self.walk_tree_views(until_pos, &mut |_, _, v| views.push(v.clone()))?;
        Ok(views)
    }

    /// The view of tree `tree_idx` (stable index) — walks head→idx, one
    /// working view, returns only the target.
    pub fn tree_view(&self, tree_idx: u64) -> Result<depot::View> {
        let n = self.count(TREES)?;
        if tree_idx >= n {
            return Err(Error::Chain(format!("no tree at index {tree_idx}")));
        }
        let target = (n - 1 - tree_idx) as usize;
        let mut out = None;
        self.walk_tree_views(Some(target), &mut |pos, _, v| {
            if pos == target {
                out = Some(v.clone());
            }
        })?;
        out.ok_or_else(|| Error::Chain(format!("tree walk fell short of index {tree_idx}")))
    }

    /// Flush the depot durable, then run `stage` inside one sqlite
    /// transaction — the write discipline (chains durable before
    /// bookkeeping commits).
    pub(crate) fn with_txn(
        &mut self,
        stage: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<()> {
        self.depot.flush().map_err(|e| Error::Chain(e.to_string()))?;
        let tx = self.conn.transaction().map_err(sql_err)?;
        stage(&tx)?;
        tx.commit().map_err(sql_err)
    }

    // -------------------------------------------------------- lookups

    pub fn sha_to_idx(&self, sha: &str) -> Result<Option<u64>> {
        self.conn
            .query_row("SELECT commit_idx FROM sha_idx WHERE sha = ?1", [sha], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })
            .optional()
            .map_err(sql_err)
    }

    pub fn idx_to_sha(&self, idx: u64) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT sha FROM sha_idx WHERE commit_idx = ?1",
                [idx as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)
    }

    pub fn tree_idx_for_hash(&self, hash: &[u8]) -> Result<Option<u64>> {
        self.conn
            .query_row("SELECT tree_idx FROM tree_hash WHERE hash = ?1", [hash], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })
            .optional()
            .map_err(sql_err)
    }

    /// CURRENT refs: name → (commit_idx, tree_idx), name-ordered.
    pub fn ref_rows(&self) -> Result<Vec<(String, u64, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, commit_idx, tree_idx FROM refs ORDER BY name")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                ))
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// CURRENT refs with their shas (the `for-each-ref`-shaped view).
    pub fn refs_meta(&self) -> Result<Vec<RefMeta>> {
        let mut out = Vec::new();
        for (name, cidx, _t) in self.ref_rows()? {
            let sha = self
                .idx_to_sha(cidx)?
                .ok_or_else(|| Error::Meta("ref target not in chain".into()))?;
            out.push(RefMeta { name, sha });
        }
        Ok(out)
    }

    /// The commit record at stable index `idx` — a newest-first walk of
    /// the commits chain down to position N-1-idx.
    pub fn commit_record_at(&self, idx: u64) -> Result<CommitRecord> {
        let n = self.count(COMMITS)?;
        self.commit_record_at_n(idx, n)
    }

    /// As `commit_record_at`, against an explicit chain length —
    /// needed mid-ingest, when records are prepended but the kv count
    /// is not yet committed.
    pub(crate) fn commit_record_at_n(&self, idx: u64, n: u64) -> Result<CommitRecord> {
        if idx >= n {
            return Err(Error::Meta(format!("no commit at index {idx}")));
        }
        let pos = (n - 1 - idx) as usize;
        let recs = self.records_newest_first(COMMITS)?;
        let rec = recs
            .get(pos)
            .ok_or_else(|| Error::Chain(format!("commits chain short of index {idx}")))?;
        let cr = CommitRecord::decode(rec)?;
        if cr.idx != idx {
            return Err(Error::Chain(format!(
                "commit record at position {pos} carries idx {} (wanted {idx})",
                cr.idx
            )));
        }
        Ok(cr)
    }

    /// All commit records, oldest-first (position = stable index).
    pub fn commit_records(&self) -> Result<Vec<CommitRecord>> {
        let mut recs = self
            .records_newest_first(COMMITS)?
            .iter()
            .map(|b| CommitRecord::decode(b))
            .collect::<Result<Vec<_>>>()?;
        recs.reverse();
        for (i, r) in recs.iter().enumerate() {
            if r.idx != i as u64 {
                return Err(Error::Chain(format!(
                    "commit record at position {i} carries idx {}",
                    r.idx
                )));
            }
        }
        Ok(recs)
    }

    /// CommitMeta view of a record (parents resolved idx→sha).
    pub fn commit_meta(&self, rec: &CommitRecord) -> Result<CommitMeta> {
        let mut parents = Vec::with_capacity(rec.parent_idxs.len());
        for p in &rec.parent_idxs {
            parents.push(self.idx_to_sha(*p)?.ok_or_else(|| {
                Error::Meta(format!("parent index {p} has no sha in sha_idx"))
            })?);
        }
        Ok(CommitMeta {
            sha: rec.sha.clone(),
            parents,
            author_hex: hex::encode(&rec.author),
            committer_hex: hex::encode(&rec.committer),
            message_hex: hex::encode(&rec.message),
            extra_headers: rec.extra_headers.clone(),
            raw_hex: if rec.raw.is_empty() { String::new() } else { hex::encode(&rec.raw) },
        })
    }
}

fn count_key(chain: u64) -> &'static str {
    match chain {
        TREES => "n_trees",
        COMMITS => "n_commits",
        REFLOG => "n_reflog",
        _ => unreachable!("unknown chain"),
    }
}

fn open_depot(store: &Path) -> Result<Depot> {
    Depot::open(DepotConfig {
        root: store.join("depot"),
        max_chain_id: MAX_CHAIN_ID,
        file_size_threshold: FILE_SIZE_THRESHOLD,
        eviction_dead_ratio: EVICTION_DEAD_RATIO,
    })
    .map_err(|e| Error::Chain(e.to_string()))
}

fn schema_version(store: &Path) -> Result<i64> {
    if !db_path(store).exists() {
        return Ok(0); // legacy meta.json
    }
    let conn = Connection::open_with_flags(
        db_path(store),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(sql_err)?;
    let v: Option<String> = conn
        .query_row("SELECT value FROM kv WHERE key = 'schema'", [], |r| r.get(0))
        .optional()
        .map_err(sql_err)?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(1))
}

pub(crate) fn kv_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .map_err(sql_err)
}

pub(crate) fn kv_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO kv(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// sha256 of a tree's canonical full-view bytes — the DERIVED dedup key
/// (never stored in any record).
pub(crate) fn tree_hash(full_record: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(full_record).to_vec()
}

// ------------------------------------------------------ staged ref diff

/// One staged ref movement: reflog record content + the refs-table
/// change to apply in the same operation.
pub(crate) struct RefChange {
    pub name: String,
    pub old: Option<(u64, u64)>,
    pub new: Option<(u64, u64)>,
    pub note: &'static str,
}

/// Diff current refs-table rows against the OBSERVED upstream refs
/// (already resolved to indices). Every movement — appearance, move,
/// disappearance — becomes one change; deletion = `new` absent.
pub(crate) fn diff_refs(
    current: &[(String, u64, u64)],
    observed: &[(String, u64, u64)],
) -> Vec<RefChange> {
    let mut cur: HashMap<&str, (u64, u64)> =
        current.iter().map(|(n, c, t)| (n.as_str(), (*c, *t))).collect();
    let mut out = Vec::new();
    for (name, c, t) in observed {
        match cur.remove(name.as_str()) {
            Some(old) if old == (*c, *t) => {}
            Some(old) => out.push(RefChange {
                name: name.clone(),
                old: Some(old),
                new: Some((*c, *t)),
                note: "",
            }),
            None => out.push(RefChange {
                name: name.clone(),
                old: None,
                new: Some((*c, *t)),
                note: "",
            }),
        }
    }
    for (name, old) in cur {
        out.push(RefChange {
            name: name.to_string(),
            old: Some(old),
            new: None,
            note: "pruned upstream",
        });
    }
    // Deterministic order (HashMap drains unordered).
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Prepend the reflog records for `changes` (one batch), then return
/// the closure-side refs-table mutations for the caller's transaction.
pub(crate) fn stage_ref_changes(
    store: &Store,
    changes: &[RefChange],
    level: i32,
) -> Result<u64> {
    if changes.is_empty() {
        return Ok(0);
    }
    let at = now_secs();
    let base = store.count(REFLOG)?;
    let recs: Vec<Vec<u8>> = changes
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            ReflogRecord {
                idx: base + i as u64,
                at,
                refname: ch.name.clone(),
                old: ch.old,
                new: ch.new,
                note: ch.note.to_string(),
            }
            .encode()
        })
        .collect();
    let (head, older) = recs.split_last().expect("non-empty");
    // recs are oldest-first; the chain wants newest-first entries.
    let mut older_rev: Vec<Vec<u8>> = older.to_vec();
    older_rev.reverse();
    store.prepend_batch(REFLOG, head, &older_rev, Demote::Verbatim, level)?;
    Ok(changes.len() as u64)
}

/// Apply `changes` to the refs table (inside the caller's transaction —
/// the reflog records were already prepended and flushed).
pub(crate) fn apply_ref_changes(
    tx: &rusqlite::Transaction<'_>,
    changes: &[RefChange],
) -> Result<()> {
    for ch in changes {
        match ch.new {
            Some((c, t)) => {
                tx.execute(
                    "INSERT INTO refs(name, commit_idx, tree_idx) VALUES (?1, ?2, ?3)
                     ON CONFLICT(name) DO UPDATE SET
                       commit_idx = excluded.commit_idx,
                       tree_idx = excluded.tree_idx",
                    rusqlite::params![ch.name, c as i64, t as i64],
                )
                .map_err(sql_err)?;
            }
            None => {
                tx.execute("DELETE FROM refs WHERE name = ?1", [&ch.name])
                    .map_err(sql_err)?;
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------------- ingest

/// Staged multi-record write of new commits (import, update, migration):
/// accumulates tree deltas + commit records oldest-first, then lands
/// each chain with ONE batch prepend (SPEC §"Prepend multiple records")
/// and one sqlite transaction. Correct for one commit or many — a
/// single-commit update is the N=1 batch.
pub(crate) struct Ingest<'a> {
    st: &'a mut Store,
    level: i32,
    // TREES staging. `entries` are reverse deltas in ADD order: the
    // first entry rebuilds the pre-batch head (the bridge), each later
    // one rebuilds the previously-added tree.
    head_view: Option<depot::View>,
    head_full: Option<Vec<u8>>,
    seed_full: Option<Vec<u8>>,
    tree_entries: Vec<Vec<u8>>,
    tree_entry_bytes: usize,
    n_trees: u64,
    new_tree_hashes: Vec<(Vec<u8>, u64)>,
    tree_cache: HashMap<Vec<u8>, u64>,
    // COMMITS staging (encoded records, oldest-first).
    commit_recs: Vec<Vec<u8>>,
    commit_rec_bytes: usize,
    n_commits: u64,
    new_shas: Vec<(String, u64)>,
    sha_cache: HashMap<String, u64>,
    tree_of_commit: HashMap<u64, u64>,
}

impl<'a> Ingest<'a> {
    pub(crate) fn new(st: &'a mut Store, level: i32) -> Result<Self> {
        let n_trees = st.count(TREES)?;
        let n_commits = st.count(COMMITS)?;
        let head_view = if n_trees > 0 {
            Some(
                st.tree_views(Some(0))?
                    .pop()
                    .ok_or_else(|| Error::Chain("trees count > 0 but empty chain".into()))?,
            )
        } else {
            None
        };
        Ok(Ingest {
            st,
            level,
            head_view,
            head_full: None,
            seed_full: None,
            tree_entries: Vec::new(),
            tree_entry_bytes: 0,
            n_trees,
            new_tree_hashes: Vec::new(),
            tree_cache: HashMap::new(),
            commit_recs: Vec::new(),
            commit_rec_bytes: 0,
            n_commits,
            new_shas: Vec::new(),
            sha_cache: HashMap::new(),
            tree_of_commit: HashMap::new(),
        })
    }

    pub(crate) fn knows_sha(&self, sha: &str) -> Result<bool> {
        Ok(self.sha_cache.contains_key(sha) || self.st.sha_to_idx(sha)?.is_some())
    }

    fn parent_idx(&self, sha: &str) -> Result<u64> {
        if let Some(i) = self.sha_cache.get(sha) {
            return Ok(*i);
        }
        self.st
            .sha_to_idx(sha)?
            .ok_or_else(|| Error::Meta(format!("parent {sha} not in store")))
    }

    fn tree_idx_for(&mut self, view: &depot::View, full_record: &[u8]) -> Result<u64> {
        let h = tree_hash(full_record);
        if let Some(i) = self.tree_cache.get(&h) {
            return Ok(*i);
        }
        if let Some(i) = self.st.tree_idx_for_hash(&h)? {
            self.tree_cache.insert(h, i);
            return Ok(i);
        }
        // New distinct tree: stage its record. The delta pushed rebuilds
        // the CURRENT staged head from this (next-newer) view; the very
        // first tree of a fresh store seeds the chain instead.
        match &self.head_view {
            Some(prev) => {
                let delta = depot::codec::encode(&depot::diff(Some(view), Some(prev)));
                self.tree_entry_bytes += delta.len();
                self.tree_entries.push(delta);
            }
            None => self.seed_full = Some(full_record.to_vec()),
        }
        let idx = self.n_trees;
        self.n_trees += 1;
        self.head_view = Some(view.clone());
        self.head_full = Some(full_record.to_vec());
        self.tree_cache.insert(h.clone(), idx);
        self.new_tree_hashes.push((h, idx));
        // Staged bytes past the seal threshold land NOW (a sub-batch of
        // the whole ingest) so accumulators and cold frames stay
        // bounded during big imports; a small update stays one prepend.
        if self.tree_entry_bytes as u64 > SEAL_THRESHOLD {
            self.flush_tree_batch()?;
        }
        Ok(idx)
    }

    fn flush_tree_batch(&mut self) -> Result<()> {
        if let Some(seed_rec) = self.seed_full.take() {
            self.st
                .prepend_batch(TREES, &seed_rec, &[], Demote::Verbatim, self.level)?;
        }
        if self.tree_entries.is_empty() {
            return Ok(());
        }
        let head_full = self
            .head_full
            .clone()
            .expect("staged tree entries imply a staged head");
        let (bridge, rest) = self
            .tree_entries
            .split_first()
            .expect("non-empty entries");
        let mut older: Vec<Vec<u8>> = rest.to_vec();
        older.reverse();
        self.st
            .prepend_batch(TREES, &head_full, &older, Demote::Replace(bridge), self.level)?;
        self.tree_entries.clear();
        self.tree_entry_bytes = 0;
        Ok(())
    }

    fn flush_commit_batch(&mut self) -> Result<()> {
        if let Some((head, older)) = self.commit_recs.split_last() {
            let mut older_rev: Vec<Vec<u8>> = older.to_vec();
            older_rev.reverse();
            self.st
                .prepend_batch(COMMITS, head, &older_rev, Demote::Verbatim, self.level)?;
        }
        self.commit_recs.clear();
        self.commit_rec_bytes = 0;
        Ok(())
    }

    /// Stage one commit (must arrive oldest-first: parents before
    /// children). `full_record` = `codec::encode(diff(None, view))`.
    pub(crate) fn add_commit(
        &mut self,
        cm: &CommitMeta,
        view: &depot::View,
        full_record: &[u8],
    ) -> Result<u64> {
        let tree_idx = self.tree_idx_for(view, full_record)?;
        let parent_idxs = cm
            .parents
            .iter()
            .map(|p| self.parent_idx(p))
            .collect::<Result<Vec<_>>>()?;
        let idx = self.n_commits;
        self.n_commits += 1;
        let dehex = |s: &str| hex::decode(s).map_err(|e| Error::Meta(e.to_string()));
        let rec = CommitRecord {
            idx,
            sha: cm.sha.clone(),
            tree_idx,
            parent_idxs,
            author: dehex(&cm.author_hex)?,
            committer: dehex(&cm.committer_hex)?,
            message: dehex(&cm.message_hex)?,
            extra_headers: cm.extra_headers.clone(),
            raw: dehex(&cm.raw_hex)?,
        };
        let enc = rec.encode();
        self.commit_rec_bytes += enc.len();
        self.commit_recs.push(enc);
        if self.commit_rec_bytes as u64 > SEAL_THRESHOLD {
            self.flush_commit_batch()?;
        }
        self.sha_cache.insert(cm.sha.clone(), idx);
        self.new_shas.push((cm.sha.clone(), idx));
        self.tree_of_commit.insert(idx, tree_idx);
        Ok(idx)
    }

    /// (commit_idx, tree_idx) for an observed ref target.
    fn ref_row(&self, sha: &str) -> Result<(u64, u64)> {
        let cidx = self.parent_idx(sha)?;
        if let Some(t) = self.tree_of_commit.get(&cidx) {
            return Ok((cidx, *t));
        }
        // Mid-ingest the chain already holds the staged records; the kv
        // count updates last.
        Ok((cidx, self.st.commit_record_at_n(cidx, self.n_commits)?.tree_idx))
    }

    /// One batch prepend per touched chain (trees, commits) for what
    /// remains staged; oversized ingests already flushed sub-batches at
    /// the seal threshold.
    fn flush_chains(&mut self) -> Result<()> {
        self.flush_tree_batch()?;
        self.flush_commit_batch()
    }

    fn commit_bookkeeping(self, changes: &[RefChange], n_reflog: u64) -> Result<()> {
        let Ingest { st, n_trees, n_commits, new_tree_hashes, new_shas, .. } = self;
        st.with_txn(|tx| {
            kv_set(tx, "n_trees", &n_trees.to_string())?;
            kv_set(tx, "n_commits", &n_commits.to_string())?;
            kv_set(tx, "n_reflog", &n_reflog.to_string())?;
            for (sha, idx) in &new_shas {
                tx.execute(
                    "INSERT INTO sha_idx(sha, commit_idx) VALUES (?1, ?2)",
                    rusqlite::params![sha, *idx as i64],
                )
                .map_err(sql_err)?;
            }
            for (h, idx) in &new_tree_hashes {
                tx.execute(
                    "INSERT INTO tree_hash(hash, tree_idx) VALUES (?1, ?2)",
                    rusqlite::params![h, *idx as i64],
                )
                .map_err(sql_err)?;
            }
            apply_ref_changes(tx, changes)?;
            Ok(())
        })
    }

    /// Land everything: chain batches, reflog rows for every observed
    /// ref movement, then the bookkeeping transaction.
    pub(crate) fn finish(mut self, observed_refs: &[RefMeta]) -> Result<()> {
        self.flush_chains()?;
        let observed: Vec<(String, u64, u64)> = observed_refs
            .iter()
            .map(|r| {
                let (c, t) = self.ref_row(&r.sha)?;
                Ok((r.name.clone(), c, t))
            })
            .collect::<Result<Vec<_>>>()?;
        let changes = diff_refs(&self.st.ref_rows()?, &observed);
        let n_reflog = self.st.count(REFLOG)? + stage_ref_changes(self.st, &changes, self.level)?;
        self.commit_bookkeeping(&changes, n_reflog)
    }

    /// Migration variant: reflog records are supplied verbatim (v1
    /// carried its own timestamps), refs rows likewise.
    pub(crate) fn finish_migration(
        mut self,
        reflog_recs: Vec<ReflogRecord>,
        ref_rows: Vec<(String, u64, u64)>,
    ) -> Result<()> {
        self.flush_chains()?;
        let n_reflog = reflog_recs.len() as u64;
        if let Some((head, older)) = reflog_recs
            .iter()
            .map(ReflogRecord::encode)
            .collect::<Vec<_>>()
            .split_last()
        {
            let mut older_rev: Vec<Vec<u8>> = older.to_vec();
            older_rev.reverse();
            self.st
                .prepend_batch(REFLOG, head, &older_rev, Demote::Verbatim, self.level)?;
        }
        let changes: Vec<RefChange> = ref_rows
            .into_iter()
            .map(|(name, c, t)| RefChange { name, old: None, new: Some((c, t)), note: "" })
            .collect();
        self.commit_bookkeeping(&changes, n_reflog)
    }

    pub(crate) fn ref_row_pub(&self, sha: &str) -> Result<(u64, u64)> {
        self.ref_row(sha)
    }
}

// --------------------------------------------------------- public reads

/// (label, url) — kv point reads.
pub fn identity(store: &Path) -> Result<(String, String)> {
    Store::open(store)?.identity()
}

pub fn set_identity(store: &Path, label: &str, url: &str) -> Result<()> {
    Store::open(store)?.set_identity(label, url)
}

/// The store's human label ("WHICH git?").
pub fn label(store: &Path) -> Result<String> {
    Ok(identity(store)?.0)
}

pub fn commit_count(store: &Path) -> Result<usize> {
    Ok(Store::open(store)?.count(COMMITS)? as usize)
}

/// LIVE refs, name-ordered — O(refs) point reads.
pub fn refs(store: &Path) -> Result<Vec<RefMeta>> {
    Store::open(store)?.refs_meta()
}

/// The commit at STABLE index `idx` (0 = oldest).
pub fn commit_at(store: &Path, idx: usize) -> Result<CommitMeta> {
    let s = Store::open(store)?;
    let rec = s.commit_record_at(idx as u64)?;
    s.commit_meta(&rec)
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Resolve REF — a ref name (`main` matches `refs/heads/main`, tags
/// likewise, or the full refname) or a unique commit-sha prefix (ANY
/// commit in the store is addressable, not just the tips) — to
/// `(sha, stable commit index)`. `Ok(None)` = nothing matched; an
/// ambiguous sha prefix is an error. Point lookups only.
pub fn resolve_ref(store: &Path, refname: &str) -> Result<Option<(String, usize)>> {
    let s = Store::open(store)?;
    // Name order = the for-each-ref order (refs/heads before refs/tags).
    let hit: Option<i64> = s
        .conn
        .query_row(
            "SELECT commit_idx FROM refs
             WHERE name = ?1 OR name = 'refs/heads/' || ?1
                OR name = 'refs/tags/' || ?1
             ORDER BY name LIMIT 1",
            [refname],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some(idx) = hit {
        let sha = s
            .idx_to_sha(idx as u64)?
            .ok_or_else(|| Error::Meta("ref target not in chain".into()))?;
        return Ok(Some((sha, idx as usize)));
    }
    let mut stmt = s
        .conn
        .prepare("SELECT sha, commit_idx FROM sha_idx WHERE sha LIKE ?1 ESCAPE '\\' LIMIT 2")
        .map_err(sql_err)?;
    let hits = stmt
        .query_map([format!("{}%", like_escape(refname))], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    match hits.as_slice() {
        [(sha, idx)] => Ok(Some((sha.clone(), *idx as usize))),
        [] => Ok(None),
        _ => Err(Error::Meta(format!("commit prefix {refname} is ambiguous"))),
    }
}

/// One reflog observation, indices resolved back to shas for display.
#[derive(Debug, Clone)]
pub struct ReflogEntry {
    pub at: i64,
    pub refname: String,
    pub old_commit_idx: Option<u64>,
    pub new_commit_idx: Option<u64>,
    pub old_sha: Option<String>,
    pub new_sha: Option<String>,
    pub note: String,
}

/// The full reflog, oldest observation first — a full chain walk (a
/// display path, not a hot path).
pub fn reflog(store: &Path) -> Result<Vec<ReflogEntry>> {
    let s = Store::open(store)?;
    let mut recs = s
        .records_newest_first(REFLOG)?
        .iter()
        .map(|b| ReflogRecord::decode(b))
        .collect::<Result<Vec<_>>>()?;
    recs.reverse();
    let mut out = Vec::with_capacity(recs.len());
    for r in recs {
        let sha_of = |o: Option<(u64, u64)>| -> Result<Option<String>> {
            match o {
                Some((c, _)) => s.idx_to_sha(c),
                None => Ok(None),
            }
        };
        out.push(ReflogEntry {
            at: r.at,
            refname: r.refname,
            old_commit_idx: r.old.map(|(c, _)| c),
            new_commit_idx: r.new.map(|(c, _)| c),
            old_sha: sha_of(r.old)?,
            new_sha: sha_of(r.new)?,
            note: r.note,
        });
    }
    Ok(out)
}

/// Full bookkeeping load — O(all commits); only for code paths that
/// genuinely need every commit (export, log). `Meta.commits` stays
/// newest-first (the v1 shape the callers expect).
pub fn read_meta(store: &Path) -> Result<Meta> {
    let s = Store::open(store)?;
    let (label, url) = s.identity()?;
    let refs = s.refs_meta()?;
    let recs = s.commit_records()?;
    let sha_of: Vec<&str> = recs.iter().map(|r| r.sha.as_str()).collect();
    let mut commits = Vec::with_capacity(recs.len());
    for r in recs.iter().rev() {
        let parents = r
            .parent_idxs
            .iter()
            .map(|p| {
                sha_of
                    .get(*p as usize)
                    .map(|s| s.to_string())
                    .ok_or_else(|| Error::Meta(format!("parent index {p} out of range")))
            })
            .collect::<Result<Vec<_>>>()?;
        commits.push(CommitMeta {
            sha: r.sha.clone(),
            parents,
            author_hex: hex::encode(&r.author),
            committer_hex: hex::encode(&r.committer),
            message_hex: hex::encode(&r.message),
            extra_headers: r.extra_headers.clone(),
            raw_hex: if r.raw.is_empty() { String::new() } else { hex::encode(&r.raw) },
        });
    }
    Ok(Meta { label, url, refs, commits })
}
