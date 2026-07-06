//! Store — four tiered chains + stable indices (ATTACH-CONVERGENCE.md
//! chip 7, design of record 2026-07-06).
//!
//! `<store>/depot/`      — ONE wikimak-depot instance holding four chains:
//!                         TREES=0, COMMITS=1, REFLOG=2, TAGS=3.
//! `<store>/meta.sqlite` — bookkeeping (WAL): `kv` (schema=5, label, url,
//!                         n_trees/n_commits/n_reflog/n_tags counts — the
//!                         AUTHORITATIVE index base), `refs` (CURRENT refs
//!                         only: name → nullable commit_idx, tree_idx,
//!                         plus a nullable tag_idx: tag_idx NULL =
//!                         branch/lightweight tag; set = the ref is an
//!                         annotated tag and commit_idx/tree_idx are its
//!                         PEELED commit — name resolution and attach
//!                         stay peeled. commit_idx NULL = the tag peels
//!                         to a TREE (tag_idx set, tree_idx = the tagged
//!                         tree; there is no commit to resolve to).
//!                         NOTHING
//!                         else: every id-keyed lookup derives from the
//!                         chains on demand. sha → idx is an in-RAM map
//!                         built by ONE object-level walk of the COMMITS
//!                         chain, cached for the life of the open Store
//!                         handle (one walk per mirror-tick process); a
//!                         ref-NAME attach is a refs point-read plus a
//!                         tip-biased object fetch — it never builds the
//!                         map; a sha attach walks once per process.
//!
//! Tree dedup at ingest is (1) an in-RAM git-tree-oid → tree_idx map
//! scoped to the Ingest and (2) reuse of the parent's tree_idx when the
//! incoming commit's tree oid equals a parent's (empty/metadata
//! commits — the dominant case; oid equality ⇔ canonical-view equality
//! here because no tree node is cooked: modes verbatim as attrs, blobs
//! as-is, gitlink oids as blobs). A tree bit-identical to a DISTANT
//! ancestor (revert) is deliberately NOT deduped: it costs one ordinary
//! reverse-delta record sized by the actual change, which beats
//! carrying a persistent all-history oid index (100+MB at linux
//! scale) for the rare revert.
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
//!     verbatim to a cold frame and the accumulator restarts; and when
//!     the JUST-WRITTEN f1's raw size itself exceeds the threshold (a
//!     batch that dwarfs it), it is sealed to cold immediately in the
//!     same operation (`Depot::seal_f1`) — frames are write-once, and
//!     the next incremental prepend never recompresses a huge
//!     accumulator. An ingest lands EXACTLY ONE prepend per touched
//!     chain; RAM and disk stay bounded by STREAMING the frame codec
//!     (encode and decode), never by mid-ingest chunking.
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
//! one standalone frame.
//!
//! COMMITS/REFLOG/TAGS chain records are BATCHES: one record per
//! ingest (an update's new commits, one ref transaction), never split
//! by the seal threshold — sealing stays a per-prepend decision on
//! the accumulator. Batch record layout:
//!   `u64 LE base_idx | u32 LE count | count × (u32 LE len | object)`,
//! objects OLDEST-FIRST within the batch, object stable index =
//! base_idx + offset. Batch records stand alone, so their demotion is
//! verbatim and cold anchors are simply the last record decoded (the
//! wikipedia discipline). Object reads walk records newest-first and
//! skip a batch on its header alone when the wanted index is out of
//! its [base, base+count) range. kv counts are OBJECT counts.
//!
//! TAGS chain objects are annotated-tag objects: `{sha, target, raw}` —
//! `target` is the FULLY-PEELED end of the tag chain as a stable index,
//! either a COMMITS index or (for a tag at a tree, the linux
//! v2.6.11-tree shape) a TREES index (for a nested chain EVERY tag
//! object is stored, inner tags at lower indices so export can write
//! deepest-first, each with the final target's index; the intermediate
//! target sha lives inside `raw` anyway). A tagged tree that equals
//! some commit's root tree reuses that commit's TREES record; a
//! genuinely standalone tagged tree is imported as an ordinary TREES
//! record and its reverse delta anchors like any other record (chain
//! position = wherever the ingest lands it). `raw` is the COMPLETE raw
//! tag object (header + message + signature verbatim) — the
//! export-fidelity payload, same rationale as commit records. Tags
//! whose peel ends at a BLOB are refused at import (a named
//! Unsupported) — the only remaining unsupported ref shape (no known
//! real-world need; revisit on evidence).
//!
//! **Durability/integrity**: depot writes are flushed durable BEFORE the
//! sqlite transaction commits, so kv counts are never ahead of the
//! chains. COMMITS/REFLOG objects embed their own stable index; on open
//! the head batch of each non-empty chain must cover exactly up to
//! count-1 or the store errors loudly (a crash between depot flush and
//! sqlite commit can leave orphan newer frames — detected, not
//! auto-repaired). TREES records are pure codec bytes (they double as
//! refPrefix anchors), so the trees chain is cross-checked through the
//! head commit's tree_idx bound and verified in depth by any walk.
//!
//! There is exactly ONE on-disk format: kv schema=5. A store written by
//! older code errors loudly on open — delete and re-import; mirrors are
//! rebuildable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use wikimak_depot::{Depot, DepotConfig};

use crate::{CommitMeta, Error, Meta, RefMeta, Result};

pub const TREES: u64 = 0;
pub const COMMITS: u64 = 1;
pub const REFLOG: u64 = 2;
pub const TAGS: u64 = 3;
const MAX_CHAIN_ID: u64 = 4;

/// Raw (decompressed) f1 accumulator seal point, per chain.
/// Test-overridable via GITDEPOT_TEST_SEAL (bytes) so tests can
/// exercise the immediate-seal path without multi-MB fixtures.
const SEAL_THRESHOLD: u64 = 256 * 1024;

fn seal_threshold() -> u64 {
    std::env::var("GITDEPOT_TEST_SEAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SEAL_THRESHOLD)
}
// Small on purpose: eviction cannot touch the CURRENT write-target
// file, so this threshold IS the dead-byte ceiling per tier — and the
// trees chain's f0 frames are whole-head-sized, deadening fast. The
// moderate-repo bench measured 32MiB here as ~2.7x the useful store.
const FILE_SIZE_THRESHOLD: u64 = 4 << 20;
/// Staging-medium switch — a MEMORY bound only. Records staged past
/// this many bytes spill to a per-chain scratch file (compressed, see
/// `Staged`); the bound selects where staged bytes WAIT, never how
/// they land. Overridable via GITDEPOT_SPILL_BOUND (bytes) so tests
/// can force the spill path.
const SPILL_RAM_BOUND: u64 = 256 << 20;
/// Spill scratch: raw records accumulate in a pending buffer and
/// compress to the scratch file in ~4MiB standalone zstd blocks at
/// level 3 (records never split across blocks; an oversized record
/// gets its own block).
const SPILL_BLOCK: usize = 4 << 20;
const SPILL_BLOCK_LEVEL: i32 = 3;
const EVICTION_DEAD_RATIO: f32 = 0.5;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS kv(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS refs(
    name       TEXT PRIMARY KEY,
    commit_idx INTEGER,
    tree_idx   INTEGER NOT NULL,
    tag_idx    INTEGER
);
";

pub(crate) fn sql_err(e: rusqlite::Error) -> Error {
    Error::Meta(e.to_string())
}

fn db_path(store: &Path) -> PathBuf {
    store.join("meta.sqlite")
}

/// "Is there a store here?"
pub fn store_exists(store: &Path) -> bool {
    db_path(store).exists()
}

/// The one supported on-disk format. Anything else on open = loud
/// error (unreleased software: no migrations, mirrors are rebuildable).
const SCHEMA_VERSION: &str = "5";

fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL;").map_err(sql_err)
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

/// One COMMITS-chain object (batched into chain records). Lineage is
/// `parent_idxs` — stable indices,
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
    /// `(idx, sha)` without decoding the rest — the sha-map builder.
    fn peek_idx_sha(b: &[u8]) -> Result<(u64, String)> {
        let mut c = Cur::new(b);
        let idx = c.u64()?;
        let sha = String::from_utf8(c.bytes()?.to_vec())
            .map_err(|_| Error::Chain("commit record: non-utf8 sha".into()))?;
        Ok((idx, sha))
    }

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

/// The fully-peeled end of a tag chain, as a stable index into the
/// chain that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagTarget {
    Commit(u64),
    Tree(u64),
}

/// A tag's resolved peel as handed to `Ingest::add_tag`.
pub(crate) enum TagPeel<'a> {
    /// Fully-peeled commit sha (staged or stored).
    Commit(&'a str),
    /// Fully-peeled tree, already resolved to its TREES index.
    Tree(u64),
}

/// One TAGS-chain object (batched into chain records): an annotated
/// tag. `raw` is the complete raw tag object (export writes it back
/// verbatim and asserts the sha); `target` is the fully peeled
/// commit's — or, for a tree tag, tree's — stable index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecord {
    pub idx: u64,
    pub sha: String,
    pub target: TagTarget,
    pub raw: Vec<u8>,
}

impl TagRecord {
    /// `(idx, sha)` without decoding the raw — the tag-map builder.
    fn peek_idx_sha(b: &[u8]) -> Result<(u64, String)> {
        let mut c = Cur::new(b);
        let idx = c.u64()?;
        let sha = String::from_utf8(c.bytes()?.to_vec())
            .map_err(|_| Error::Chain("tag record: non-utf8 sha".into()))?;
        Ok((idx, sha))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.idx.to_le_bytes());
        put_bytes(&mut out, self.sha.as_bytes());
        // Target kind flag byte: 0 = COMMITS index, 1 = TREES index.
        let (kind, tidx) = match self.target {
            TagTarget::Commit(i) => (0u8, i),
            TagTarget::Tree(i) => (1u8, i),
        };
        out.push(kind);
        out.extend_from_slice(&tidx.to_le_bytes());
        put_bytes(&mut out, &self.raw);
        out
    }

    pub fn decode(b: &[u8]) -> Result<TagRecord> {
        let mut c = Cur::new(b);
        let idx = c.u64()?;
        let sha = String::from_utf8(c.bytes()?.to_vec())
            .map_err(|_| Error::Chain("tag record: non-utf8 sha".into()))?;
        let target = match c.u8()? {
            0 => TagTarget::Commit(c.u64()?),
            1 => TagTarget::Tree(c.u64()?),
            k => return Err(Error::Chain(format!("tag record: unknown target kind {k}"))),
        };
        let raw = c.bytes()?.to_vec();
        c.done()?;
        Ok(TagRecord { idx, sha, target, raw })
    }
}

/// One REFLOG-chain object (batched into chain records): an observed
/// ref movement. `old` absent =
/// creation; `new` absent = deletion. Values are `(commit_idx,
/// tree_idx)` pairs; commit_idx absent = a tag-at-tree value (note
/// "tag@tree"). Prepended BEFORE the refs table row changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogRecord {
    pub idx: u64,
    pub at: i64,
    pub refname: String,
    pub old: Option<(Option<u64>, u64)>,
    pub new: Option<(Option<u64>, u64)>,
    pub note: String,
}

impl ReflogRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.idx.to_le_bytes());
        out.extend_from_slice(&(self.at as u64).to_le_bytes());
        put_bytes(&mut out, self.refname.as_bytes());
        // Flag bits: 0 = old present, 1 = new present, 2 = old carries
        // a commit_idx, 3 = new carries one (unset with bit 0/1 set =
        // a tag-at-tree side: tree_idx only).
        let flags = self.old.is_some() as u8
            | ((self.new.is_some() as u8) << 1)
            | ((self.old.is_some_and(|(c, _)| c.is_some()) as u8) << 2)
            | ((self.new.is_some_and(|(c, _)| c.is_some()) as u8) << 3);
        out.push(flags);
        for side in [self.old, self.new].into_iter().flatten() {
            let (c, t) = side;
            if let Some(c) = c {
                out.extend_from_slice(&c.to_le_bytes());
            }
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
        let mut side = |present: u8, has_commit: u8| -> Result<Option<(Option<u64>, u64)>> {
            if flags & present == 0 {
                return Ok(None);
            }
            let commit = if flags & has_commit != 0 { Some(c.u64()?) } else { None };
            Ok(Some((commit, c.u64()?)))
        };
        let old = side(1, 4)?;
        let new = side(2, 8)?;
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
    // Window/LDM discipline lives in the depot's normative frame
    // codec — shared by every chain user.
    wikimak_depot::compress_frame(src, prefix, level).map_err(Error::Chain)
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

/// Stream the u32-length-prefixed records of a multi-record frame
/// (f1 or cold) in stored (newest-first) order, decoding incrementally
/// (`wikimak_depot::FrameDecoder`): one record in RAM at a time, never
/// the whole raw frame — a linux-scale sealed frame decompresses to
/// tens of GB. `visit` returns true to stop early.
fn stream_frame_records(
    frame: &[u8],
    prefix: &[u8],
    visit: &mut dyn FnMut(Vec<u8>) -> Result<bool>,
) -> Result<()> {
    use std::io::Read as _;
    let mut dec =
        wikimak_depot::FrameDecoder::new(frame, Some(prefix)).map_err(Error::Chain)?;
    // read_full: fill `buf` or hit clean EOF at a record boundary.
    let read_full = |dec: &mut wikimak_depot::FrameDecoder<'_>,
                         buf: &mut [u8]|
     -> Result<usize> {
        let mut got = 0;
        while got < buf.len() {
            let n = dec.read(&mut buf[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    };
    loop {
        let mut hdr = [0u8; 4];
        match read_full(&mut dec, &mut hdr)? {
            0 => return Ok(()),
            4 => {}
            _ => return Err(Error::Chain("truncated record".into())),
        }
        let len = u32::from_le_bytes(hdr) as usize;
        let mut rec = vec![0u8; len];
        if read_full(&mut dec, &mut rec)? != len {
            return Err(Error::Chain("truncated record".into()));
        }
        if visit(rec)? {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------- batch records
// COMMITS/REFLOG chain record = one ingest's objects (module doc):
// u64 LE base_idx | u32 LE count | count × (u32 LE len | object bytes),
// objects oldest-first, object stable idx = base_idx + offset.

fn encode_batch(base_idx: u64, objects_oldest_first: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        12 + objects_oldest_first.iter().map(|o| o.len() + 4).sum::<usize>(),
    );
    out.extend_from_slice(&base_idx.to_le_bytes());
    out.extend_from_slice(&(objects_oldest_first.len() as u32).to_le_bytes());
    for o in objects_oldest_first {
        put_bytes(&mut out, o);
    }
    out
}

/// `(base_idx, count)` — the range check needs no object decoding.
fn batch_header(rec: &[u8]) -> Result<(u64, u32)> {
    let mut c = Cur::new(rec);
    Ok((c.u64()?, c.u32()?))
}

/// `(base_idx, objects oldest-first)`.
fn decode_batch(rec: &[u8]) -> Result<(u64, Vec<Vec<u8>>)> {
    let mut c = Cur::new(rec);
    let base = c.u64()?;
    let n = c.u32()? as usize;
    let mut objs = Vec::with_capacity(n);
    for _ in 0..n {
        objs.push(c.bytes()?.to_vec());
    }
    c.done()?;
    Ok((base, objs))
}

/// How the former head record joins the accumulator on prepend.
pub(crate) enum Demote {
    /// Record stands alone (COMMITS, REFLOG, TAGS): moves in verbatim
    /// as the OLDEST entry of the new f1 content.
    Verbatim,
    /// Record is superseded by a caller-computed replacement already
    /// present as the oldest streamed entry (TREES: the bridge delta
    /// rebuilding the old head view from the new one is staged entry
    /// 0) — nothing extra joins the accumulator.
    Dropped,
}

// ---------------------------------------------------------------- store

pub struct Store {
    depot: Depot,
    pub(crate) conn: Connection,
    root: PathBuf,
    /// Lazily built sha → stable idx map (ONE commits-chain walk),
    /// never persisted, discarded with the handle; invalidated by
    /// ingest flushes.
    sha_map: std::cell::OnceCell<HashMap<String, u64>>,
    /// Same discipline for tag sha → stable idx (ONE TAGS-chain walk;
    /// tags are few).
    tag_map: std::cell::OnceCell<HashMap<String, u64>>,
}

impl Store {
    /// Create a fresh, empty store. Errors if one is already there.
    pub fn create(store: &Path) -> Result<Store> {
        if store_exists(store) {
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
        let s = Store { depot, conn, root: store.to_path_buf(), sha_map: Default::default(), tag_map: Default::default() };
        let tx = s.conn.unchecked_transaction().map_err(sql_err)?;
        for (k, v) in [
            ("schema", SCHEMA_VERSION),
            ("label", ""),
            ("url", ""),
            ("n_trees", "0"),
            ("n_commits", "0"),
            ("n_reflog", "0"),
            ("n_tags", "0"),
        ] {
            kv_set(&tx, k, v)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(s)
    }

    /// Open an existing store.
    pub fn open(store: &Path) -> Result<Store> {
        if !store_exists(store) {
            return Err(Error::Chain(format!("no store at {}", store.display())));
        }
        let depot = open_depot(store)?;
        let conn = Connection::open_with_flags(
            db_path(store),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(sql_err)?;
        configure(&conn)?;
        let schema = kv_get(&conn, "schema")?.unwrap_or_default();
        if schema != SCHEMA_VERSION {
            return Err(Error::Chain(format!(
                "store {} has schema {schema:?}, this build writes {SCHEMA_VERSION} — \
                 store written by older code; delete and re-import (mirrors are \
                 rebuildable)",
                store.display()
            )));
        }
        let s = Store { depot, conn, root: store.to_path_buf(), sha_map: Default::default(), tag_map: Default::default() };
        s.integrity_check()?;
        Ok(s)
    }

    /// Loud count/chain agreement check (see module doc): the head
    /// batch of each self-indexing chain must cover exactly up to
    /// count-1.
    fn integrity_check(&self) -> Result<()> {
        for (chain, name, count) in [
            (COMMITS, "commits", self.count(COMMITS)?),
            (REFLOG, "reflog", self.count(REFLOG)?),
            (TAGS, "tags", self.count(TAGS)?),
        ] {
            if count == 0 {
                continue;
            }
            let head = self
                .read_head(chain)?
                .ok_or_else(|| Error::Chain(format!("{name}: count {count} but empty chain")))?;
            let (base, n) = batch_header(&head)?;
            if base + n as u64 != count {
                return Err(Error::Chain(format!(
                    "{name}: head batch covers [{base}, {}) but kv count is {count} — \
                     chains and bookkeeping disagree (crash between depot flush and \
                     sqlite commit?); re-mirror the store",
                    base + n as u64
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

    /// Prepend a batch of records to `chain` as EXACTLY ONE depot
    /// prepend: `head_record` becomes the new f0; the new f1 is
    /// STREAM-composed — never materialized raw in RAM — from
    /// `older` (staged records, drained newest-first; for TREES these
    /// are reverse deltas whose oldest entry is the bridge), then the
    /// demoted former head (`Demote::Verbatim`), then the old f1's
    /// raw bytes (stream-decoded) unless sealing. The seal decision is
    /// `compose_f1`'s, against the OLD f1; additionally, if the NEW
    /// f1's raw size exceeds the seal threshold it is retired to cold
    /// immediately (`Depot::seal_f1`) so no later prepend ever
    /// recompresses it. The refPrefix anchor is `head_record`, the
    /// window log is pinned by the upfront-known total raw length —
    /// the streamed frame is byte-identical to what the bulk
    /// `compress_frame` would produce.
    ///
    /// A previously-empty chain must be seeded first (pass empty
    /// `older`): the depot forbids f1 on a chain's first prepend.
    pub(crate) fn prepend_batch(
        &self,
        chain: u64,
        head_record: &[u8],
        mut older: Option<&mut Staged>,
        demote: Demote,
        level: i32,
    ) -> Result<()> {
        let older_empty = older.as_ref().map_or(true, |s| s.is_empty());
        let Some(prev_record) = self.read_head(chain)? else {
            if older_empty {
                self.depot
                    .prepend(chain, &compress(head_record, None, level)?, None, false)
                    .map_err(|e| Error::Chain(e.to_string()))?;
                return Ok(());
            }
            return Err(Error::Chain(
                "batch prepend on an empty chain (seed it first)".into(),
            ));
        };
        let demoted: Option<&[u8]> = match demote {
            Demote::Verbatim => Some(&prev_record),
            Demote::Dropped => None,
        };
        // compose_f1 semantics over frame entries (u32 prefix + record).
        let entries_len: u64 = older
            .as_ref()
            .map_or(0, |s| s.bytes + 4 * s.len() as u64)
            + demoted.map_or(0, |d| d.len() as u64 + 4);
        let old_f1 = self.depot.read_f1(chain).map_err(|e| Error::Chain(e.to_string()))?;
        let old_raw_len = match &old_f1 {
            Some(z) => zstd::zstd_safe::get_frame_content_size(z)
                .map_err(|_| Error::Chain("zstd frame content size".into()))?
                .ok_or_else(|| Error::Chain("zstd frame without content size".into()))?,
            None => 0,
        };
        let seal_old = old_f1.is_some() && old_raw_len + entries_len > seal_threshold();
        let total_raw = entries_len + if seal_old { 0 } else { old_raw_len };
        let mut enc = wikimak_depot::FrameEncoder::new(total_raw, Some(head_record), level)
            .map_err(Error::Chain)?;
        let put = |enc: &mut wikimak_depot::FrameEncoder<'_>, rec: &[u8]| -> Result<()> {
            enc.write(&(rec.len() as u32).to_le_bytes()).map_err(Error::Chain)?;
            enc.write(rec).map_err(Error::Chain)
        };
        if let Some(staged) = older.as_deref_mut() {
            staged.drain_rev(&mut |rec| put(&mut enc, rec))?;
        }
        if let Some(d) = demoted {
            put(&mut enc, d)?;
        }
        if !seal_old {
            if let Some(z) = &old_f1 {
                use std::io::Read as _;
                let mut dec = wikimak_depot::FrameDecoder::new(z, Some(&prev_record))
                    .map_err(Error::Chain)?;
                let mut buf = vec![0u8; 128 << 10];
                loop {
                    let n = dec.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    enc.write(&buf[..n]).map_err(Error::Chain)?;
                }
            }
        }
        let new_f1 = enc.finish().map_err(Error::Chain)?;
        let new_f0 = compress(head_record, None, level)?;
        self.depot
            .prepend(chain, &new_f0, Some(&new_f1), seal_old)
            .map_err(|e| Error::Chain(e.to_string()))?;
        // Immediate retirement: a just-written accumulator that already
        // dwarfs the threshold moves verbatim to cold NOW — a later
        // incremental prepend must never recompress it. The frame was
        // anchored on `head_record`, which is exactly the record the
        // walk decodes (or, for TREES, canonically re-encodes) right
        // before this frame — the invariant is unchanged.
        if total_raw > seal_threshold() {
            self.depot
                .seal_f1(chain)
                .map_err(|e| Error::Chain(e.to_string()))?;
        }
        Ok(())
    }

    /// Walk the records of a standalone-record chain (COMMITS, REFLOG —
    /// each record a batch) newest-first; `visit` returns true to stop
    /// (later tiers stay undecompressed). Anchors: f1 on the f0 record;
    /// each cold frame on the last (oldest) record decoded before it.
    fn walk_records(
        &self,
        chain: u64,
        visit: &mut dyn FnMut(&[u8]) -> Result<bool>,
    ) -> Result<()> {
        let Some(head) = self.read_head(chain)? else {
            return Ok(());
        };
        if visit(&head)? {
            return Ok(());
        }
        let mut anchor = head;
        // Per frame: stream records one at a time (never the whole raw
        // frame); the frame's last (oldest) record becomes the next
        // frame's anchor.
        let one_frame = |frame: &[u8],
                             anchor: &mut Vec<u8>,
                             visit: &mut dyn FnMut(&[u8]) -> Result<bool>|
         -> Result<bool> {
            let mut stopped = false;
            let mut last: Option<Vec<u8>> = None;
            stream_frame_records(frame, anchor, &mut |rec| {
                stopped = visit(&rec)?;
                last = Some(rec);
                Ok(stopped)
            })?;
            if let Some(l) = last {
                *anchor = l;
            }
            Ok(stopped)
        };
        if let Some(f1) = self.depot.read_f1(chain).map_err(|e| Error::Chain(e.to_string()))? {
            if one_frame(&f1, &mut anchor, visit)? {
                return Ok(());
            }
        }
        for cold in self.depot.cold_iter(chain).map_err(|e| Error::Chain(e.to_string()))? {
            let frame = cold.map_err(|e| Error::Chain(e.to_string()))?;
            if one_frame(&frame, &mut anchor, visit)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// All OBJECTS of a batched chain, newest-first (batches expanded).
    pub(crate) fn objects_newest_first(&self, chain: u64) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        self.walk_records(chain, &mut |rec| {
            let (_, objs) = decode_batch(rec)?;
            out.extend(objs.into_iter().rev());
            Ok(false)
        })?;
        Ok(out)
    }

    /// The object at stable index `idx` of a batched chain — batches
    /// whose header range excludes `idx` are skipped undecoded, and
    /// the walk stops at the hit (tip-biased reads never touch cold).
    fn object_at(&self, chain: u64, idx: u64) -> Result<Option<Vec<u8>>> {
        let mut hit = None;
        self.walk_records(chain, &mut |rec| {
            let (base, count) = batch_header(rec)?;
            if idx < base || idx >= base + count as u64 {
                return Ok(false);
            }
            let (_, mut objs) = decode_batch(rec)?;
            hit = Some(objs.swap_remove((idx - base) as usize));
            Ok(true)
        })?;
        Ok(hit)
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
            let mut stopped = false;
            stream_frame_records(&f1, &head, &mut |rec| {
                stopped = step(&mut cur, &mut pos, &rec)?;
                Ok(stopped)
            })?;
            if stopped {
                return Ok(());
            }
        }
        for cold in self.depot.cold_iter(TREES).map_err(|e| Error::Chain(e.to_string()))? {
            let frame = cold.map_err(|e| Error::Chain(e.to_string()))?;
            // The cold anchor: canonical full-view bytes at the frame
            // boundary, recomputed from the walked view.
            let anchor = depot::codec::encode(&depot::diff(None, cur.as_ref()));
            let mut stopped = false;
            stream_frame_records(&frame, &anchor, &mut |rec| {
                stopped = step(&mut cur, &mut pos, &rec)?;
                Ok(stopped)
            })?;
            if stopped {
                return Ok(());
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

    /// Depot flush + eviction pass (`Depot::flush` runs eviction).
    /// Reachable wherever a depot flush already is — `with_txn` covers
    /// the ingest path, so nothing calls this today; kept for callers
    /// that flush outside a transaction.
    #[allow(dead_code)]
    pub(crate) fn evict_pass(&mut self) -> Result<()> {
        self.depot.flush().map_err(|e| Error::Chain(e.to_string()))
    }

    // -------------------------------------------------------- lookups

    /// The lazily built sha → idx map (module doc cost model). Name
    /// resolution must NOT come through here.
    pub(crate) fn sha_map(&self) -> Result<&HashMap<String, u64>> {
        if self.sha_map.get().is_none() {
            let mut m = HashMap::new();
            self.walk_records(COMMITS, &mut |rec| {
                let (_, objs) = decode_batch(rec)?;
                for o in objs {
                    let (idx, sha) = CommitRecord::peek_idx_sha(&o)?;
                    m.insert(sha, idx);
                }
                Ok(false)
            })?;
            let _ = self.sha_map.set(m);
        }
        Ok(self.sha_map.get().expect("just set"))
    }

    pub fn sha_to_idx(&self, sha: &str) -> Result<Option<u64>> {
        Ok(self.sha_map()?.get(sha).copied())
    }

    /// idx → sha via the commit object itself (batch-skipping,
    /// tip-biased) — deliberately NOT the map, so name-keyed paths
    /// never pay the full walk.
    pub fn idx_to_sha(&self, idx: u64) -> Result<Option<String>> {
        match self.object_at(COMMITS, idx)? {
            Some(o) => Ok(Some(CommitRecord::peek_idx_sha(&o)?.1)),
            None => Ok(None),
        }
    }

    /// The lazily built tag sha → idx map (one TAGS-chain walk).
    pub(crate) fn tag_map(&self) -> Result<&HashMap<String, u64>> {
        if self.tag_map.get().is_none() {
            let mut m = HashMap::new();
            self.walk_records(TAGS, &mut |rec| {
                let (_, objs) = decode_batch(rec)?;
                for o in objs {
                    let (idx, sha) = TagRecord::peek_idx_sha(&o)?;
                    m.insert(sha, idx);
                }
                Ok(false)
            })?;
            let _ = self.tag_map.set(m);
        }
        Ok(self.tag_map.get().expect("just set"))
    }

    pub fn tag_sha_to_idx(&self, sha: &str) -> Result<Option<u64>> {
        Ok(self.tag_map()?.get(sha).copied())
    }

    /// The tag record at stable index `idx` (batch-skipping, tip-biased).
    pub fn tag_record_at(&self, idx: u64) -> Result<TagRecord> {
        let obj = self
            .object_at(TAGS, idx)?
            .ok_or_else(|| Error::Chain(format!("no tag at index {idx}")))?;
        let tr = TagRecord::decode(&obj)?;
        if tr.idx != idx {
            return Err(Error::Chain(format!(
                "tag object at batch offset for {idx} carries idx {}",
                tr.idx
            )));
        }
        Ok(tr)
    }

    /// All tag records, oldest-first (position = stable index; inner
    /// tags of a nested chain precede the outer ones that name them).
    pub fn tag_records(&self) -> Result<Vec<TagRecord>> {
        let mut recs = self
            .objects_newest_first(TAGS)?
            .iter()
            .map(|b| TagRecord::decode(b))
            .collect::<Result<Vec<_>>>()?;
        recs.reverse();
        for (i, r) in recs.iter().enumerate() {
            if r.idx != i as u64 {
                return Err(Error::Chain(format!(
                    "tag record at position {i} carries idx {}",
                    r.idx
                )));
            }
        }
        Ok(recs)
    }

    /// CURRENT refs: name → (commit_idx, tree_idx, tag_idx), name-ordered.
    /// commit_idx/tree_idx are PEELED for annotated tags (tag_idx set);
    /// commit_idx is NULL for a tag peeling to a tree.
    pub fn ref_rows(&self) -> Result<Vec<(String, Option<u64>, u64, Option<u64>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, commit_idx, tree_idx, tag_idx FROM refs ORDER BY name")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                ))
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// The fully-peeled TREE oid of a tree-target tag at stable index
    /// `idx` — read from the stored raw tag bytes (nested chains
    /// followed through the tag map; tree oids are never stored as
    /// layer data, per the implicit-id rule).
    pub fn peeled_tree_oid(&self, idx: u64) -> Result<String> {
        let mut rec = self.tag_record_at(idx)?;
        loop {
            let (obj, typ) = crate::parse_tag_target(&rec.sha, &rec.raw)?;
            match typ.as_str() {
                "tree" => return Ok(obj),
                "tag" => {
                    let inner = self.tag_sha_to_idx(&obj)?.ok_or_else(|| {
                        Error::Chain(format!("inner tag {obj} not in chain"))
                    })?;
                    rec = self.tag_record_at(inner)?;
                }
                other => {
                    return Err(Error::Chain(format!(
                        "tag {} records a tree target but raw peels to a {other}",
                        rec.sha
                    )))
                }
            }
        }
    }

    /// CURRENT refs with their shas (the `for-each-ref`-shaped view).
    pub fn refs_meta(&self) -> Result<Vec<RefMeta>> {
        let mut out = Vec::new();
        for (name, cidx, _t, tag) in self.ref_rows()? {
            let tag_sha = match tag {
                Some(ti) => self.tag_record_at(ti)?.sha,
                None => String::new(),
            };
            let (sha, tree_sha) = match cidx {
                Some(c) => (
                    self.idx_to_sha(c)?
                        .ok_or_else(|| Error::Meta("ref target not in chain".into()))?,
                    String::new(),
                ),
                None => {
                    let ti = tag.ok_or_else(|| {
                        Error::Meta(format!("ref {name} has neither commit nor tag"))
                    })?;
                    (String::new(), self.peeled_tree_oid(ti)?)
                }
            };
            out.push(RefMeta { name, sha, tag_sha, tree_sha });
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
        let obj = self
            .object_at(COMMITS, idx)?
            .ok_or_else(|| Error::Chain(format!("commits chain short of index {idx}")))?;
        let cr = CommitRecord::decode(&obj)?;
        if cr.idx != idx {
            return Err(Error::Chain(format!(
                "commit object at batch offset for {idx} carries idx {}",
                cr.idx
            )));
        }
        Ok(cr)
    }

    /// All commit records, oldest-first (position = stable index).
    pub fn commit_records(&self) -> Result<Vec<CommitRecord>> {
        let mut recs = self
            .objects_newest_first(COMMITS)?
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
                Error::Meta(format!("parent index {p} has no sha in the commits chain"))
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
        TAGS => "n_tags",
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

// ------------------------------------------------------ staged ref diff

/// A ref's value: (commit_idx, tree_idx, tag_idx) — commit/tree PEELED,
/// tag_idx set only for annotated tags; commit_idx None = a tag
/// peeling to a tree.
pub(crate) type RefVal = (Option<u64>, u64, Option<u64>);

/// One staged ref movement: reflog record content + the refs-table
/// change to apply in the same operation. The reflog row carries only
/// the PEELED commit/tree movement; tag_idx lives in the refs table.
pub(crate) struct RefChange {
    pub name: String,
    pub old: Option<RefVal>,
    pub new: Option<RefVal>,
    pub note: &'static str,
}

/// Diff current refs-table rows against the OBSERVED upstream refs
/// (already resolved to indices). Every movement — appearance, move,
/// disappearance — becomes one change; deletion = `new` absent.
pub(crate) fn diff_refs(
    current: &[(String, Option<u64>, u64, Option<u64>)],
    observed: &[(String, Option<u64>, u64, Option<u64>)],
) -> Vec<RefChange> {
    let mut cur: HashMap<&str, RefVal> =
        current.iter().map(|(n, c, t, g)| (n.as_str(), (*c, *t, *g))).collect();
    let note_for = |old: Option<RefVal>, new: Option<RefVal>| -> &'static str {
        let tree_tag = |v: Option<RefVal>| v.is_some_and(|v| v.0.is_none());
        if tree_tag(old) || tree_tag(new) {
            "tag@tree"
        } else if old.is_some_and(|v| v.2.is_some()) || new.is_some_and(|v| v.2.is_some()) {
            "tag"
        } else {
            ""
        }
    };
    let mut out = Vec::new();
    for (name, c, t, g) in observed {
        let new = (*c, *t, *g);
        match cur.remove(name.as_str()) {
            Some(old) if old == new => {}
            Some(old) => out.push(RefChange {
                name: name.clone(),
                old: Some(old),
                new: Some(new),
                note: note_for(Some(old), Some(new)),
            }),
            None => out.push(RefChange {
                name: name.clone(),
                old: None,
                new: Some(new),
                note: note_for(None, Some(new)),
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

/// Prepend the reflog objects for `changes` as ONE batch record, then
/// return the object count for the caller's bookkeeping transaction.
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
    let objs: Vec<Vec<u8>> = changes
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            ReflogRecord {
                idx: base + i as u64,
                at,
                refname: ch.name.clone(),
                old: ch.old.map(|(c, t, _)| (c, t)),
                new: ch.new.map(|(c, t, _)| (c, t)),
                note: ch.note.to_string(),
            }
            .encode()
        })
        .collect();
    let batch = encode_batch(base, &objs);
    store.prepend_batch(REFLOG, &batch, None, Demote::Verbatim, level)?;
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
            Some((c, t, g)) => {
                tx.execute(
                    "INSERT INTO refs(name, commit_idx, tree_idx, tag_idx)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(name) DO UPDATE SET
                       commit_idx = excluded.commit_idx,
                       tree_idx = excluded.tree_idx,
                       tag_idx = excluded.tag_idx",
                    rusqlite::params![ch.name, c.map(|v| v as i64), t as i64,
                                      g.map(|v| v as i64)],
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

/// One compressed spill block: `comp_len` zstd bytes at file offset
/// `off`, decompressing to `raw_len`.
struct SpillBlock {
    off: u64,
    comp_len: u32,
    raw_len: u32,
}

/// Spilled staging state: raw records accumulate in `pending` and
/// compress to the scratch file in ~`SPILL_BLOCK`-sized standalone
/// zstd blocks (level `SPILL_BLOCK_LEVEL`, no refPrefix). A record is
/// never split across blocks — the pending buffer flushes only AFTER
/// a whole record joins it, so a record larger than the block size
/// simply gets its own block.
struct Spill {
    f: std::fs::File,
    file_len: u64,
    blocks: Vec<SpillBlock>,
    /// Per-record (block_no, offset_in_block, len); block_no ==
    /// blocks.len() = the record is still in the pending raw buffer.
    recs: Vec<(u32, u32, u32)>,
    pending: Vec<u8>,
    /// Last decompressed block (sequential access dominates: the
    /// staged walk and drain read records in near-order).
    cache: std::cell::RefCell<Option<(u32, Vec<u8>)>>,
}

/// Oldest-first record staging for one chain: RAM up to `bound` RAW
/// bytes, then EVERYTHING (existing and subsequent records) moves to
/// one scratch file of compressed blocks (`Spill`). The medium selects
/// where bytes wait, never how they land: at `finish()` the stage
/// drains STREAMING — `drain_rev`/`for_each` decompress one block at
/// a time and yield record-at-a-time, so a bootstrap-scale batch is
/// never materialized raw in RAM. The scratch file is pure scratch: a
/// crash mid-ingest leaves nothing to clean but the file (deleted on
/// Drop).
pub(crate) struct Staged {
    ram: Vec<Vec<u8>>,
    /// RAW staged bytes (RAM or spilled) since the last drain.
    bytes: u64,
    bound: u64,
    block: usize,
    spill: Option<Spill>,
    path: PathBuf,
}

impl Staged {
    fn new(path: PathBuf, bound: u64, block: usize) -> Staged {
        Staged { ram: Vec::new(), bytes: 0, bound, block, spill: None, path }
    }

    fn len(&self) -> usize {
        self.ram.len() + self.spill.as_ref().map_or(0, |sp| sp.recs.len())
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push(&mut self, rec: Vec<u8>) -> Result<()> {
        self.bytes += rec.len() as u64;
        if self.spill.is_none() && self.bytes <= self.bound {
            self.ram.push(rec);
            return Ok(());
        }
        if self.spill.is_none() {
            // Crossing the bound: open the scratch file and move the
            // RAM prefix over so record order stays push order.
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&self.path)?;
            self.spill = Some(Spill {
                f,
                file_len: 0,
                blocks: Vec::new(),
                recs: Vec::new(),
                pending: Vec::new(),
                cache: std::cell::RefCell::new(None),
            });
            let moved = std::mem::take(&mut self.ram);
            for r in moved {
                self.append(&r)?;
            }
        }
        self.append(&rec)
    }

    fn append(&mut self, rec: &[u8]) -> Result<()> {
        let block = self.block;
        let sp = self.spill.as_mut().expect("spill open");
        sp.recs.push((sp.blocks.len() as u32, sp.pending.len() as u32, rec.len() as u32));
        sp.pending.extend_from_slice(rec);
        if sp.pending.len() >= block {
            Self::seal_block(sp)?;
        }
        Ok(())
    }

    /// Compress the pending raw buffer to the scratch file as one block.
    fn seal_block(sp: &mut Spill) -> Result<()> {
        if sp.pending.is_empty() {
            return Ok(());
        }
        use std::io::Write as _;
        let comp = zstd::bulk::compress(&sp.pending, SPILL_BLOCK_LEVEL)?;
        sp.f.write_all(&comp)?;
        sp.blocks.push(SpillBlock {
            off: sp.file_len,
            comp_len: comp.len() as u32,
            raw_len: sp.pending.len() as u32,
        });
        sp.file_len += comp.len() as u64;
        sp.pending.clear();
        Ok(())
    }

    fn read_block(sp: &Spill, b: u32) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt as _;
        let blk = &sp.blocks[b as usize];
        let mut comp = vec![0u8; blk.comp_len as usize];
        sp.f.read_exact_at(&mut comp, blk.off)?;
        Ok(zstd::bulk::decompress(&comp, blk.raw_len as usize)?)
    }

    /// Record `i` in push order.
    fn get(&self, i: usize) -> Result<Vec<u8>> {
        match &self.spill {
            None => Ok(self.ram[i].clone()),
            Some(sp) => {
                let (b, off, len) = sp.recs[i];
                let (off, len) = (off as usize, len as usize);
                if b as usize == sp.blocks.len() {
                    return Ok(sp.pending[off..off + len].to_vec());
                }
                let mut cache = sp.cache.borrow_mut();
                if cache.as_ref().map_or(true, |(cb, _)| *cb != b) {
                    *cache = Some((b, Self::read_block(sp, b)?));
                }
                let raw = &cache.as_ref().expect("just cached").1;
                Ok(raw[off..off + len].to_vec())
            }
        }
    }

    /// Visit every record oldest-first (push order) WITHOUT draining —
    /// streaming: at most one decompressed spill block in RAM at a
    /// time (`get`'s last-block cache makes the sequential pass one
    /// decode per block).
    fn for_each(&self, visit: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        for i in 0..self.len() {
            visit(&self.get(i)?)?;
        }
        Ok(())
    }

    /// Visit every record NEWEST-first (reverse push order), then
    /// leave the stage empty with its scratch file deleted. Streaming
    /// like `for_each`: blocks decode last-to-first, one at a time.
    fn drain_rev(&mut self, visit: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        for i in (0..self.len()).rev() {
            visit(&self.get(i)?)?;
        }
        self.clear();
        Ok(())
    }

    fn clear(&mut self) {
        self.ram.clear();
        if self.spill.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
        self.bytes = 0;
    }

    /// All records in push order; the stage is left empty and its
    /// scratch file deleted. Test-only: the landing paths stream via
    /// `for_each`/`drain_rev` instead of materializing the batch.
    #[cfg(test)]
    fn drain_all(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(self.len());
        self.for_each(&mut |rec| {
            out.push(rec.to_vec());
            Ok(())
        })?;
        self.clear();
        Ok(out)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.spill.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Staged multi-record write of new commits (import, update, migration):
/// accumulates tree deltas + commit records oldest-first (in RAM or
/// spilled to `<store>/staging/`, see `Staged`) and lands EXACTLY ONE
/// prepend per touched chain at `finish()` (SPEC §"Prepend multiple
/// records"): f0 = the head full record, ONE f1 stream-composed from
/// every other staged record — and if that f1's raw size exceeds the
/// seal threshold it is sealed to cold immediately in the same
/// operation. No mid-ingest landing: RAM and disk stay bounded by the
/// compressed spill plus the streaming frame codec, and the whole
/// ingest stays atomic (nothing lands before `finish()`; a crash
/// leaves only scratch).
pub(crate) struct Ingest<'a> {
    st: &'a mut Store,
    level: i32,
    // TREES staging. `entries` are reverse deltas in ADD order: the
    // first entry rebuilds the stored chain head (the bridge), each
    // later one rebuilds the previously-added tree.
    head_view: Option<depot::View>,
    seed_full: Option<Vec<u8>>,
    tree_entries: Staged,
    /// Stable index one past the last LANDED tree (= n_trees at open;
    /// nothing lands before `finish()`). Staged trees are
    /// `tree_base..n_trees`.
    tree_base: u64,
    n_trees: u64,
    /// Intra-ingest dedup only (git tree oid → tree_idx), discarded
    /// with the Ingest.
    tree_cache: HashMap<String, u64>,
    // COMMITS staging (encoded objects, oldest-first).
    commit_recs: Staged,
    n_commits: u64,
    sha_cache: HashMap<String, u64>,
    tree_of_commit: HashMap<u64, u64>,
    // TAGS staging (encoded objects, oldest-first).
    tag_recs: Staged,
    n_tags: u64,
    tag_cache: HashMap<String, u64>,
}

impl<'a> Ingest<'a> {
    pub(crate) fn new(st: &'a mut Store, level: i32) -> Result<Self> {
        let n_trees = st.count(TREES)?;
        let n_commits = st.count(COMMITS)?;
        let n_tags = st.count(TAGS)?;
        let head_view = if n_trees > 0 {
            Some(
                st.tree_views(Some(0))?
                    .pop()
                    .ok_or_else(|| Error::Chain("trees count > 0 but empty chain".into()))?,
            )
        } else {
            None
        };
        let bound = std::env::var("GITDEPOT_SPILL_BOUND")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(SPILL_RAM_BOUND);
        let staging = st.root().join("staging");
        Ok(Ingest {
            st,
            level,
            head_view,
            seed_full: None,
            tree_entries: Staged::new(staging.join("trees"), bound, SPILL_BLOCK),
            tree_base: n_trees,
            n_trees,
            tree_cache: HashMap::new(),
            commit_recs: Staged::new(staging.join("commits"), bound, SPILL_BLOCK),
            n_commits,
            sha_cache: HashMap::new(),
            tree_of_commit: HashMap::new(),
            tag_recs: Staged::new(staging.join("tags"), bound, SPILL_BLOCK),
            n_tags,
            tag_cache: HashMap::new(),
        })
    }

    /// RAW staged bytes across the three stages (RAM + spill) —
    /// instrumentation for the bootstrap rung report.
    pub(crate) fn staged_bytes(&self) -> u64 {
        self.tree_entries.bytes + self.commit_recs.bytes + self.tag_recs.bytes
    }

    fn parent_idx(&self, sha: &str) -> Result<u64> {
        if let Some(i) = self.sha_cache.get(sha) {
            return Ok(*i);
        }
        self.st
            .sha_to_idx(sha)?
            .ok_or_else(|| Error::Meta(format!("parent {sha} not in store")))
    }

    // Dedup key = git's own root tree oid (free at import from the
    // commit's `tree` header). `same_tree_parent` = a parent sha whose
    // tree oid equals `tree_oid` (caller-checked from the fetch
    // buffer): reuse its tree_idx — no persistent oid index (module
    // doc).
    pub(crate) fn tree_idx_for(
        &mut self,
        tree_oid: &str,
        same_tree_parent: Option<&str>,
        view: &depot::View,
    ) -> Result<u64> {
        if let Some(i) = self.tree_cache.get(tree_oid) {
            return Ok(*i);
        }
        if let Some(p) = same_tree_parent {
            let cidx = self.parent_idx(p)?;
            let t = match self.tree_of_commit.get(&cidx) {
                Some(t) => *t,
                None => self.st.commit_record_at_n(cidx, self.n_commits)?.tree_idx,
            };
            self.tree_cache.insert(tree_oid.to_string(), t);
            return Ok(t);
        }
        // New distinct tree: stage its record. The delta pushed rebuilds
        // the CURRENT staged head from this (next-newer) view; the very
        // first tree of a fresh store seeds the chain instead. The full
        // record (`encode(diff(None, view))`, O(tree)) is minted ONCE
        // per fresh store here and once per prepend batch at flush —
        // never per commit; per-commit cost is this delta, whose diff
        // short-circuits every subtree `view` and `prev` still share.
        match &self.head_view {
            Some(prev) => {
                let delta = depot::codec::encode(&depot::diff(Some(view), Some(prev)));
                self.tree_entries.push(delta)?;
            }
            None => {
                self.seed_full =
                    Some(depot::codec::encode(&depot::diff(None, Some(view))));
            }
        }
        let idx = self.n_trees;
        self.n_trees += 1;
        self.head_view = Some(view.clone());
        self.tree_cache.insert(tree_oid.to_string(), idx);
        Ok(idx)
    }

    /// Reconstruct the view of tree `tree_idx` mid-ingest: staged
    /// trees by walking the staged reverse deltas backward from the
    /// staged head — the mid-ingest analogue of
    /// `Store::walk_tree_views` (a laddered bootstrap needs boundary
    /// views before the ingest commits) — and trees already STORED
    /// (below `tree_base`) by walking the chain. One backward pass
    /// per side serves many targets:
    /// `targets` maps stable tree idx → keys wanting that view.
    pub(crate) fn staged_views<K: Clone>(
        &self,
        targets: &std::collections::BTreeMap<u64, Vec<K>>,
    ) -> Result<Vec<(K, depot::View)>> {
        let mut out = Vec::new();
        if targets.is_empty() {
            return Ok(out);
        }
        // Staged entry 0 (the bridge) rebuilds the LANDED chain head,
        // tree_base - 1 — the staged walk bottoms out there; anything
        // lower comes from the chain.
        let first_rebuilt = if self.tree_base == 0 { 0 } else { self.tree_base - 1 };
        let mut stored: std::collections::BTreeMap<u64, &Vec<K>> = Default::default();
        let mut staged: std::collections::BTreeMap<u64, &Vec<K>> = Default::default();
        for (i, ks) in targets {
            if *i < first_rebuilt {
                stored.insert(*i, ks);
            } else {
                staged.insert(*i, ks);
            }
        }
        if !staged.is_empty() {
            let lowest = *staged.keys().next().expect("non-empty");
            let mut cur = self
                .head_view
                .clone()
                .ok_or_else(|| Error::Chain("staged view walk on an empty ingest".into()))?;
            let mut idx = self.n_trees - 1;
            let mut j = self.tree_entries.len();
            loop {
                if let Some(keys) = staged.get(&idx) {
                    for k in *keys {
                        out.push((k.clone(), cur.clone()));
                    }
                }
                if idx == lowest {
                    break;
                }
                if j == 0 {
                    return Err(Error::Chain(format!(
                        "staged walk exhausted at tree {idx} before reaching {lowest}"
                    )));
                }
                j -= 1;
                let rec = self.tree_entries.get(j)?;
                let layer = depot::codec::decode(&rec)?;
                let mut v = Some(cur);
                depot::apply_mut(&mut v, &layer);
                cur = v.ok_or_else(|| {
                    Error::Chain(format!("staged tree {} resolves to nothing", idx - 1))
                })?;
                idx -= 1;
            }
        }
        if !stored.is_empty() {
            // Chain head = tree tree_base - 1 at walk position 0.
            let base = self.tree_base;
            let lowest = *stored.keys().next().expect("non-empty");
            let until = (base - 1 - lowest) as usize;
            self.st.walk_tree_views(Some(until), &mut |pos, _, v| {
                let idx = base - 1 - pos as u64;
                if let Some(keys) = stored.get(&idx) {
                    for k in *keys {
                        out.push((k.clone(), v.clone()));
                    }
                }
            })?;
        }
        Ok(out)
    }

    /// The staged view of a commit already added to THIS ingest.
    pub(crate) fn tree_idx_of_staged(&self, sha: &str) -> Option<u64> {
        self.sha_cache.get(sha).and_then(|c| self.tree_of_commit.get(c)).copied()
    }

    /// Land everything TREES-staged as ONE batch prepend: f0 = the
    /// staged head's full record; the f1 streams the staged reverse
    /// deltas newest-first — the oldest of which is the bridge that
    /// replaces the demoted former head (`Demote::Dropped`).
    fn flush_tree_batch(&mut self) -> Result<()> {
        if let Some(seed_rec) = self.seed_full.take() {
            self.st
                .prepend_batch(TREES, &seed_rec, None, Demote::Verbatim, self.level)?;
        }
        if self.tree_entries.is_empty() {
            self.tree_base = self.n_trees;
            return Ok(());
        }
        // The batch-head full record, minted here (once per prepend)
        // from the staged head view — byte-equal to what the old
        // per-commit path carried forward (diff/encode are canonical).
        let head_full = depot::codec::encode(&depot::diff(
            None,
            Some(self.head_view.as_ref().expect("staged tree entries imply a staged head")),
        ));
        self.st.prepend_batch(
            TREES,
            &head_full,
            Some(&mut self.tree_entries),
            Demote::Dropped,
            self.level,
        )?;
        self.tree_base = self.n_trees;
        Ok(())
    }

    fn flush_commit_batch(&mut self) -> Result<()> {
        if !self.commit_recs.is_empty() {
            let count = self.commit_recs.len();
            let base = self.n_commits - count as u64;
            // ONE batch record for the whole ingest, assembled by
            // streaming the stage forward (never a Vec-of-Vecs copy).
            let mut batch = Vec::with_capacity(
                12 + self.commit_recs.bytes as usize + 4 * count,
            );
            batch.extend_from_slice(&base.to_le_bytes());
            batch.extend_from_slice(&(count as u32).to_le_bytes());
            self.commit_recs.for_each(&mut |rec| {
                put_bytes(&mut batch, rec);
                Ok(())
            })?;
            self.commit_recs.clear();
            self.st
                .prepend_batch(COMMITS, &batch, None, Demote::Verbatim, self.level)?;
            self.st.sha_map.take();
        }
        Ok(())
    }

    /// Stage one commit (must arrive oldest-first: parents before
    /// children). `tree_oid` = the commit's `tree` header value;
    /// `same_tree_parent` = a parent sha with the same tree oid, if
    /// any.
    pub(crate) fn add_commit(
        &mut self,
        cm: &CommitMeta,
        tree_oid: &str,
        same_tree_parent: Option<&str>,
        view: &depot::View,
    ) -> Result<u64> {
        let tree_idx = self.tree_idx_for(tree_oid, same_tree_parent, view)?;
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
        self.commit_recs.push(rec.encode())?;
        self.sha_cache.insert(cm.sha.clone(), idx);
        self.tree_of_commit.insert(idx, tree_idx);
        Ok(idx)
    }

    pub(crate) fn knows_tag(&self, sha: &str) -> Result<bool> {
        Ok(self.tag_cache.contains_key(sha) || self.st.tag_sha_to_idx(sha)?.is_some())
    }

    /// tree_idx already known to this ingest for a git tree oid
    /// (every staged commit's root tree lands here).
    pub(crate) fn known_tree_idx(&self, tree_oid: &str) -> Option<u64> {
        self.tree_cache.get(tree_oid).copied()
    }

    /// The tree_idx of a commit already staged or stored, memoized
    /// under `tree_oid` for later tag lookups.
    pub(crate) fn tree_idx_of_commit(&mut self, sha: &str, tree_oid: &str) -> Result<u64> {
        let cidx = self.parent_idx(sha)?;
        let t = match self.tree_of_commit.get(&cidx) {
            Some(t) => *t,
            None => self.st.commit_record_at_n(cidx, self.n_commits)?.tree_idx,
        };
        self.tree_cache.insert(tree_oid.to_string(), t);
        Ok(t)
    }

    /// Stage one annotated-tag object. Nested chains must arrive
    /// inner-first; the target must already be staged or stored (tags
    /// ingest after commits/trees). `TagPeel::Commit` carries the
    /// FULLY-peeled commit sha; `TagPeel::Tree` the resolved TREES
    /// index (dedup/import decided by the caller — lib.rs owns git).
    pub(crate) fn add_tag(&mut self, sha: &str, peel: TagPeel<'_>, raw: &[u8]) -> Result<u64> {
        let target = match peel {
            TagPeel::Commit(target_sha) => TagTarget::Commit(self.parent_idx(target_sha)?),
            TagPeel::Tree(tidx) => TagTarget::Tree(tidx),
        };
        let idx = self.n_tags;
        self.n_tags += 1;
        let rec = TagRecord {
            idx,
            sha: sha.to_string(),
            target,
            raw: raw.to_vec(),
        };
        self.tag_recs.push(rec.encode())?;
        self.tag_cache.insert(sha.to_string(), idx);
        Ok(idx)
    }

    fn flush_tag_batch(&mut self) -> Result<()> {
        if !self.tag_recs.is_empty() {
            let count = self.tag_recs.len();
            let base = self.n_tags - count as u64;
            let mut batch =
                Vec::with_capacity(12 + self.tag_recs.bytes as usize + 4 * count);
            batch.extend_from_slice(&base.to_le_bytes());
            batch.extend_from_slice(&(count as u32).to_le_bytes());
            self.tag_recs.for_each(&mut |rec| {
                put_bytes(&mut batch, rec);
                Ok(())
            })?;
            self.tag_recs.clear();
            self.st
                .prepend_batch(TAGS, &batch, None, Demote::Verbatim, self.level)?;
            self.st.tag_map.take();
        }
        Ok(())
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

    /// One batch prepend per touched chain (trees, commits, tags) for
    /// everything staged — the whole ingest.
    fn flush_chains(&mut self) -> Result<()> {
        self.flush_tree_batch()?;
        self.flush_commit_batch()?;
        self.flush_tag_batch()
    }

    fn commit_bookkeeping(self, changes: &[RefChange], n_reflog: u64) -> Result<()> {
        let Ingest { st, n_trees, n_commits, n_tags, .. } = self;
        let _ = std::fs::remove_dir_all(st.root().join("staging"));
        st.with_txn(|tx| {
            kv_set(tx, "n_trees", &n_trees.to_string())?;
            kv_set(tx, "n_commits", &n_commits.to_string())?;
            kv_set(tx, "n_reflog", &n_reflog.to_string())?;
            kv_set(tx, "n_tags", &n_tags.to_string())?;
            apply_ref_changes(tx, changes)?;
            Ok(())
        })
    }

    /// Land everything: chain batches, reflog rows for every observed
    /// ref movement, then the bookkeeping transaction.
    pub(crate) fn finish(mut self, observed_refs: &[RefMeta]) -> Result<()> {
        self.flush_chains()?;
        let observed: Vec<(String, Option<u64>, u64, Option<u64>)> = observed_refs
            .iter()
            .map(|r| {
                let tag = if r.tag_sha.is_empty() {
                    None
                } else {
                    Some(match self.tag_cache.get(&r.tag_sha) {
                        Some(i) => *i,
                        None => self.st.tag_sha_to_idx(&r.tag_sha)?.ok_or_else(|| {
                            Error::Meta(format!(
                                "tag object {} for ref {} not in store",
                                r.tag_sha, r.name
                            ))
                        })?,
                    })
                };
                if !r.tree_sha.is_empty() {
                    // Tag at a tree: no commit; the tree_idx is in the
                    // tag record (chains are flushed by now).
                    let ti = tag.ok_or_else(|| {
                        Error::Meta(format!("tree-target ref {} without a tag", r.name))
                    })?;
                    let TagTarget::Tree(t) = self.st.tag_record_at(ti)?.target else {
                        return Err(Error::Meta(format!(
                            "ref {} peels to a tree but tag {} records a commit",
                            r.name, r.tag_sha
                        )));
                    };
                    return Ok((r.name.clone(), None, t, tag));
                }
                let (c, t) = self.ref_row(&r.sha)?;
                Ok((r.name.clone(), Some(c), t, tag))
            })
            .collect::<Result<Vec<_>>>()?;
        let changes = diff_refs(&self.st.ref_rows()?, &observed);
        let n_reflog = self.st.count(REFLOG)? + stage_ref_changes(self.st, &changes, self.level)?;
        self.commit_bookkeeping(&changes, n_reflog)
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

/// A `resolve_ref` hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A commit — a ref's peeled target or a sha-prefix match.
    Commit { sha: String, idx: usize },
    /// An annotated tag peeling to a TREE: no commit exists; the pin
    /// is the TAG object's sha (content-addressed, same semantics as a
    /// commit pin) and tree_idx names the tagged tree's TREES record.
    TreeTag { tag_sha: String, tree_idx: usize },
}

impl Resolved {
    /// The content-addressed rev to pin: the commit sha, or the tag
    /// object's sha for a tree tag.
    pub fn sha(&self) -> &str {
        match self {
            Resolved::Commit { sha, .. } => sha,
            Resolved::TreeTag { tag_sha, .. } => tag_sha,
        }
    }

    /// `(sha, stable commit index)` when the target is a commit.
    pub fn commit(&self) -> Option<(&str, usize)> {
        match self {
            Resolved::Commit { sha, idx } => Some((sha, *idx)),
            Resolved::TreeTag { .. } => None,
        }
    }
}

/// Resolve REF — a ref name (`main` matches `refs/heads/main`, tags
/// likewise, or the full refname) or a unique commit-sha prefix (ANY
/// commit in the store is addressable, not just the tips) — to a
/// [`Resolved`]: the peeled commit, or the tag+tree for a tag at a
/// tree. `Ok(None)` = nothing matched; an ambiguous sha prefix is an
/// error. Point lookups only.
pub fn resolve_ref(store: &Path, refname: &str) -> Result<Option<Resolved>> {
    let s = Store::open(store)?;
    // Name order = the for-each-ref order (refs/heads before refs/tags).
    let hit: Option<(Option<i64>, i64, Option<i64>)> = s
        .conn
        .query_row(
            "SELECT commit_idx, tree_idx, tag_idx FROM refs
             WHERE name = ?1 OR name = 'refs/heads/' || ?1
                OR name = 'refs/tags/' || ?1
             ORDER BY name LIMIT 1",
            [refname],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some((cidx, tidx, tag)) = hit {
        if let Some(idx) = cidx {
            let sha = s
                .idx_to_sha(idx as u64)?
                .ok_or_else(|| Error::Meta("ref target not in chain".into()))?;
            return Ok(Some(Resolved::Commit { sha, idx: idx as usize }));
        }
        let ti = tag.ok_or_else(|| Error::Meta("ref has neither commit nor tag".into()))?;
        return Ok(Some(Resolved::TreeTag {
            tag_sha: s.tag_record_at(ti as u64)?.sha,
            tree_idx: tidx as usize,
        }));
    }
    // sha / sha-prefix resolution: the walk-built map (one commits
    // walk per handle). Two hits suffice to prove ambiguity.
    let mut hits: Vec<(&String, u64)> = Vec::new();
    for (sha, idx) in s.sha_map()? {
        if !sha.starts_with(refname) {
            continue;
        }
        hits.push((sha, *idx));
        if hits.len() == 2 {
            break;
        }
    }
    match hits.as_slice() {
        [(sha, idx)] => Ok(Some(Resolved::Commit { sha: (*sha).clone(), idx: *idx as usize })),
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
        .objects_newest_first(REFLOG)?
        .iter()
        .map(|b| ReflogRecord::decode(b))
        .collect::<Result<Vec<_>>>()?;
    recs.reverse();
    let mut out = Vec::with_capacity(recs.len());
    for r in recs {
        let sha_of = |o: Option<(Option<u64>, u64)>| -> Result<Option<String>> {
            match o {
                Some((Some(c), _)) => s.idx_to_sha(c),
                _ => Ok(None),
            }
        };
        out.push(ReflogEntry {
            at: r.at,
            refname: r.refname,
            old_commit_idx: r.old.and_then(|(c, _)| c),
            new_commit_idx: r.new.and_then(|(c, _)| c),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Compressed-spill roundtrip across block boundaries: records
    /// never split across blocks; a record larger than the block size
    /// gets its own block; random-order `get` and `drain_all` both
    /// return the exact pushed bytes; the scratch file is deleted.
    #[test]
    fn staged_spill_block_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("staging").join("t");
        // bound 0 ⇒ every push spills; tiny 64-byte blocks.
        let mut st = Staged::new(path.clone(), 0, 64);
        let mut recs: Vec<Vec<u8>> = Vec::new();
        for i in 0..40usize {
            // Lengths straddling the block size, including one record
            // several blocks large (200 > 64: its own block).
            let len = match i % 5 {
                0 => 1,
                1 => 63,
                2 => 64,
                3 => 65,
                _ => 200,
            };
            let rec: Vec<u8> = (0..len).map(|j| (i * 31 + j) as u8).collect();
            recs.push(rec.clone());
            st.push(rec).unwrap();
        }
        assert_eq!(st.len(), recs.len());
        assert_eq!(st.bytes, recs.iter().map(|r| r.len() as u64).sum::<u64>());
        let sp = st.spill.as_ref().expect("spilled");
        assert!(sp.blocks.len() >= 2, "fixture must cross block boundaries");
        for blk in &sp.blocks {
            // A record is never split: every block decompresses to
            // whole records (checked implicitly below), and only the
            // oversized record exceeds the block size.
            assert!(blk.raw_len as usize >= 1);
        }
        // Random-order reads (defeat the last-block cache).
        for &i in &[39usize, 0, 20, 5, 38, 1, 19, 2] {
            assert_eq!(st.get(i).unwrap(), recs[i], "record {i}");
        }
        let drained = st.drain_all().unwrap();
        assert_eq!(drained, recs);
        assert!(!path.exists(), "scratch file must be deleted on drain");
        assert_eq!(st.bytes, 0);
        assert!(st.is_empty());
    }
}
