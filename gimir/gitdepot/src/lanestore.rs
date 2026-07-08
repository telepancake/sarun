//! The union lane store: a git repo's history as ONE reverse-delta chain
//! of union-variant states, built straight off the git object store.
//!
//! This is a NEW store path that lives ALONGSIDE the shipped TREES store
//! (`store.rs`) and changes nothing in it. It uses the same
//! `wikimak_depot` frame/codec plumbing (f0 + f1 + cold frames, sealed by
//! `seal_prepend`, walked newest-first by `walk_records`).
//!
//! ## Model
//!
//! * **Lanes.** Each commit is assigned to a branch lane
//!   ([`crate::lanes::assign_lanes`]) in append-only topological order;
//!   lane indices are compacted with reuse ([`crate::lanes::compact_lanes`])
//!   so the bitmap width is peak concurrent lanes. Each commit is ONE
//!   revision advancing exactly its `lane_of[i]` (plus any lanes a merge
//!   kills).
//! * **State** at each revision is the UNION of the live lanes' git trees
//!   in one path-keyed tree: at a file path its distinct `(mode, blob-oid)`
//!   versions are stored under `\0v`/`\0m` slots with a lane bitmap (see
//!   [`crate::variants`] and [`crate::reslot`]). Content is byte-stable
//!   across lane-membership changes.
//! * **Encoder.** [`crate::oidenc`] holds the union as slot state per path
//!   and, per revision, applies only the advancing/dying lanes' tree diffs —
//!   reading git tree objects by oid on demand (through a cache) and pruning
//!   unchanged subtrees by oid. It emits the depot REVERSE delta (rebuild
//!   the previous state from the new); f0 and seal heads are the forward
//!   full state.
//!
//! ## Reconstruction
//!
//! Walk the chain newest-first applying reverse deltas to rebuild the union
//! state at a revision, then [`crate::variants::extract`] its lane — that
//! commit's git tree. Its git tree oid equals the real object (SHA-exact).

use std::collections::HashMap;
use std::path::Path;

use depot::{codec, View};
use wikimak_depot::{
    compress_frame, decompress_frame, Depot, DepotConfig, FrameDecoder, FrameEncoder,
};

use crate::lanes::{assign_lanes, LaneId};
use crate::{Error, Result};

/// The single combined-state chain id in this store's private depot.
const CHAIN: u64 = 0;

/// Adapts the persistent `cat-file --batch` to the encoder's object store,
/// with a BOUNDED oid→tree cache: a content-addressed tree object is fetched
/// and parsed once and served from memory, so the hot re-reads (the
/// advancing lane's old tree is the new tree of its prior revision; a birth
/// re-reads shared subtrees) don't re-hit git. The cache is capped and
/// evicts FIFO so it can't grow without bound — the working set that matters
/// is recent. Blobs are not cached: they are fetched by oid only to emit a
/// `\0v` and then dropped.
type TreeEnts = std::sync::Arc<Vec<(Vec<u8>, crate::oidenc::Ent)>>;
const TREE_CACHE_CAP: usize = 1 << 18; // ~256k trees

struct Cat<'a> {
    cat: &'a mut crate::CatFile,
    trees: HashMap<String, TreeEnts>,
    order: std::collections::VecDeque<String>, // FIFO eviction
}

impl<'a> Cat<'a> {
    fn new(cat: &'a mut crate::CatFile) -> Self {
        Cat { cat, trees: HashMap::new(), order: std::collections::VecDeque::new() }
    }
}

impl crate::oidenc::Objects for Cat<'_> {
    fn tree(&mut self, oid: &str) -> Result<TreeEnts> {
        if let Some(t) = self.trees.get(oid) {
            return Ok(t.clone());
        }
        let ents = std::sync::Arc::new(crate::oidenc::parse_tree(&self.cat.get(oid)?)?);
        if self.trees.len() >= TREE_CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.trees.remove(&old);
            }
        }
        self.trees.insert(oid.to_string(), ents.clone());
        self.order.push_back(oid.to_string());
        Ok(ents)
    }
    fn blob(&mut self, oid: &str) -> Result<depot::Bytes> {
        Ok(self.cat.get(oid)?.into())
    }
}

/// Raw (uncompressed) f1 accumulator seal point for the combined-state
/// chain — the SAME discipline (and the SAME test override) the shipped
/// TREES store uses in `store.rs`. When absorbing a batch would push the
/// accumulator past this, the old f1 retires verbatim to cold rather than
/// being recompressed, so no prepend ever recompresses a huge frame and
/// the in-RAM record buffer stays bounded to one batch. Test-overridable
/// via `GITDEPOT_TEST_SEAL` (bytes).
const SEAL_THRESHOLD: u64 = 256 * 1024;

fn seal_threshold() -> u64 {
    std::env::var("GITDEPOT_TEST_SEAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SEAL_THRESHOLD)
}

/// In-RAM reverse-delta buffer bound: a batch is prepended (one depot
/// prepend, one new full-state f0) once its records reach this many
/// bytes. Decoupled from — and much larger than — the seal threshold on
/// purpose: each prepend rewrites the whole-combined-state f0, so FEW big
/// batches (not thousands of tiny ones) keeps both the f0-rewrite cost and
/// the transient dead-frame footprint down, while the buffer itself stays
/// far under any memory concern. Test-overridable via `GITDEPOT_TEST_BATCH`
/// so a small fixture can force many batches (multi-cold-frame coverage).
const BATCH_RAM_BOUND: u64 = 32 << 20;

fn batch_ram_bound() -> u64 {
    std::env::var("GITDEPOT_TEST_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(BATCH_RAM_BOUND)
}

fn cf(e: String) -> Error {
    Error::Chain(e)
}


/// A built lane store: a private `wikimak_depot` instance on disk (the
/// combined-state chain) plus the RAM bookkeeping a reader needs to map
/// a commit to its revision index and lane.
pub struct LaneStore {
    depot: Depot,
    /// Number of revisions (== number of in-scope commits).
    n_rev: usize,
    /// `lane_of[i]` — lane advanced at revision `i`.
    lane_of: Vec<LaneId>,
    /// `sha_of[i]` — the commit sha at revision `i`.
    sha_of: Vec<String>,
    /// Commit sha → revision index.
    sha_to_rev: HashMap<String, usize>,
    /// ref name → resolved commit sha (persisted; lets a reopened store
    /// serve any ref's tree from disk alone).
    refs: HashMap<String, String>,
}

impl LaneStore {
    // ---------------------------------------------------------- encode

    /// Like [`encode_repo`](Self::encode_repo) but stores every revision's
    /// live lanes as ONE union-variant tree (see
    /// [`crate::variants`]). No base, no delta-of-delta, no base-switching:
    /// content nodes are byte-stable across lane-membership changes, so a
    /// trunk commit that touches one lane leaves every other lane's content
    /// untouched and the frame-to-frame reverse delta is proportional to
    /// real blob churn. Reconstruction is SHA-exact.
    pub fn encode_repo_union(repo: &Path, dir: &Path, level: i32) -> Result<LaneStore> {
        use crate::oidenc::{Ent, Encoder, Trans};

        // Topology only (cheap): revision order, per-commit parents, lane
        // assignment, liveness, and the compacted (reused) lane indices.
        let dag = crate::dag_scope(repo, &[])?;
        let sha_of = dag.order.clone();
        let n_rev = sha_of.len();
        let mut index_of: HashMap<String, usize> = HashMap::with_capacity(n_rev);
        for (i, s) in sha_of.iter().enumerate() {
            index_of.insert(s.clone(), i);
        }
        let parents = parents_by_index(repo, &index_of)?;
        let assignment = assign_lanes(&parents);
        let mono = assignment.lane_of.clone();
        let birth: Vec<usize> = assignment.span.iter().map(|s| s.0).collect();
        let mut death: Vec<usize> = vec![n_rev; assignment.span.len()];
        for i in 0..n_rev {
            for &p in parents[i].iter().skip(1) {
                let l = mono[p] as usize;
                if i < death[l] {
                    death[l] = i;
                }
            }
        }
        let (compact_of, width) = crate::lanes::compact_lanes(&birth, &death, n_rev);
        let lane_of: Vec<LaneId> = mono.iter().map(|&l| compact_of[l as usize]).collect();
        let mut dying_at: Vec<Vec<LaneId>> = vec![Vec::new(); n_rev + 1];
        for l in 0..death.len() {
            if death[l] < n_rev {
                dying_at[death[l]].push(compact_of[l]);
            }
        }

        // sha → root tree oid (one log pass).
        let mut tree_of: HashMap<String, String> = HashMap::with_capacity(n_rev);
        for line in crate::git_str(repo, &["log", "--format=%H %T", "--branches", "--tags"])?.lines() {
            if let Some((s, t)) = line.split_once(' ') {
                tree_of.insert(s.to_string(), t.to_string());
            }
        }

        // The encoder reads git objects by oid on demand; nothing is
        // materialized. `lane_tree[l]` is lane l's current root tree oid.
        let depot = open_depot(dir)?;
        let batch_bound = batch_ram_bound();
        let mut cat = crate::CatFile::new(repo)?;
        let mut objs = Cat::new(&mut cat); // one persistent tree cache
        let mut enc = Encoder::new();
        let mut lane_tree: Vec<Option<String>> = vec![None; width];
        let mut batch: Vec<Vec<u8>> = Vec::new();
        let mut batch_bytes: u64 = 0;

        for i in 0..n_rev {
            let sha = &sha_of[i];
            let tree_oid =
                tree_of.get(sha).ok_or_else(|| Error::Chain(format!("no tree for {sha}")))?.clone();
            let l = lane_of[i] as usize;

            // Transitions: each dying lane leaves; the advancing lane moves.
            // Skip a dying lane whose compacted index the advancing lane is
            // reusing this revision — its advancing transition (old = the
            // dying tree, still in `lane_tree[idx]`) already moves the bit.
            let dead_old: Vec<(usize, Option<Ent>)> = dying_at[i]
                .iter()
                .filter(|&&d| d != lane_of[i])
                .map(|&d| (d as usize, lane_tree[d as usize].clone().map(Ent::dir)))
                .collect();
            let adv_old = lane_tree[l].clone().map(Ent::dir);
            let adv_new = Ent::dir(tree_oid.clone());
            let mut trans: Vec<Trans> = Vec::with_capacity(dead_old.len() + 1);
            for (d, oe) in &dead_old {
                trans.push((*d, oe.as_ref(), None));
            }
            trans.push((l, adv_old.as_ref(), Some(&adv_new)));

            let rev = enc.advance(&trans, &mut objs)?;
            for &d in &dying_at[i] {
                lane_tree[d as usize] = None;
            }
            lane_tree[l] = Some(tree_oid);

            if i == 0 {
                let f0raw = codec::encode(&enc.full(&mut objs)?);
                let f0 = compress_frame(&f0raw, None, level).map_err(cf)?;
                depot.prepend(CHAIN, &f0, None, false).map_err(|e| cf(e.to_string()))?;
            } else {
                let rec = codec::encode(&rev);
                batch_bytes += 8 + rec.len() as u64;
                batch.push(rec);
            }
            if !batch.is_empty() && batch_bytes >= batch_bound {
                let head = codec::encode(&enc.full(&mut objs)?);
                let staged: Vec<Vec<u8>> = batch.drain(..).rev().collect();
                seal_prepend(&depot, &head, &staged, level)?;
                batch_bytes = 0;
                depot.flush().map_err(|e| cf(e.to_string()))?;
            }
        }
        if !batch.is_empty() {
            let head = codec::encode(&enc.full(&mut objs)?);
            let staged: Vec<Vec<u8>> = batch.drain(..).rev().collect();
            seal_prepend(&depot, &head, &staged, level)?;
        }
        if n_rev > 0 {
            depot.flush().map_err(|e| cf(e.to_string()))?;
        }

        let sha_to_rev: HashMap<String, usize> =
            sha_of.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
        let mut refs = HashMap::new();
        for (name, sha) in collect_ref_commits(repo)? {
            if sha_to_rev.contains_key(&sha) {
                refs.insert(name, sha);
            }
        }
        persist_meta(dir, n_rev, &lane_of, &sha_of, &refs)?;
        Ok(LaneStore { depot, n_rev, lane_of, sha_of, sha_to_rev, refs })
    }


    /// Reopen a persisted lane store from `dir` alone — no repo access.
    /// The combined-state chain lives in `dir/depot` (reopened) and the
    /// sha→(rev,lane) and ref→sha bindings in `dir/meta.sqlite`; every
    /// commit's / ref's tree reconstructs SHA-exact from these.
    /// Number of sealed cold frames in the combined-state chain — test
    /// support: proves the multi-cold-frame seal path (`seal_prepend`'s
    /// seal-old branch + the cold-frame anchor recompute in the walk) was
    /// actually exercised, which only happens once the staged reverse
    /// deltas cross the seal threshold more than once.
    pub fn cold_frame_count(&self) -> Result<usize> {
        let mut n = 0;
        for cold in self.depot.cold_iter(CHAIN).map_err(|e| cf(e.to_string()))? {
            cold.map_err(|e| cf(e.to_string()))?;
            n += 1;
        }
        Ok(n)
    }

    pub fn open(dir: &Path) -> Result<LaneStore> {
        let depot = open_depot(dir)?;
        let (n_rev, lane_of, sha_of, refs) = load_meta(dir)?;
        let sha_to_rev = sha_of
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i))
            .collect();
        Ok(LaneStore { depot, n_rev, lane_of, sha_of, sha_to_rev, refs })
    }

    /// ref name → resolved commit sha (persisted set).
    pub fn refs(&self) -> &HashMap<String, String> {
        &self.refs
    }

    /// The git tree oid the ref `name` points at, reconstructed from the
    /// store (no repo access).
    pub fn tree_oid_of_ref(&self, name: &str) -> Result<String> {
        let sha = self
            .refs
            .get(name)
            .ok_or_else(|| Error::Chain(format!("ref {name} not in lane store")))?;
        self.tree_oid_of_commit(sha)
    }

    // ---------------------------------------------------------- read

    pub fn n_rev(&self) -> usize {
        self.n_rev
    }

    pub fn sha_at(&self, rev: usize) -> &str {
        &self.sha_of[rev]
    }

    pub fn rev_of(&self, sha: &str) -> Option<usize> {
        self.sha_to_rev.get(sha).copied()
    }

    /// The reconstructed combined-state View at revision `rev` — walk the
    /// chain newest-first applying reverse deltas down to it.
    pub fn combined_at(&self, rev: usize) -> Result<View> {
        if rev >= self.n_rev {
            return Err(Error::Chain(format!("no revision {rev}")));
        }
        let target_pos = self.n_rev - 1 - rev; // newest-first position
        let mut cur: Option<View> = None;
        let mut done: Option<View> = None;
        self.walk_records(&mut |pos, rec| {
            depot::apply_mut(&mut cur, &codec::decode(rec)?);
            if pos == target_pos {
                done = Some(cur.clone().unwrap_or_default());
                return Ok(true);
            }
            Ok(false)
        })?;
        done.ok_or_else(|| Error::Chain(format!("chain fell short of revision {rev}")))
    }

    /// The reconstructed git tree View of the commit at revision `rev`:
    /// extract its lane from the reconstructed union state.
    pub fn tree_at(&self, rev: usize) -> Result<View> {
        let combined = self.combined_at(rev)?;
        Ok(crate::variants::extract(&combined, self.lane_of[rev] as usize))
    }

    /// The git tree oid of the commit at revision `rev` (reconstructed).
    pub fn tree_oid_at(&self, rev: usize) -> Result<String> {
        crate::view_tree_oid(&self.tree_at(rev)?)
    }

    /// The git tree oid of the commit named by `sha` (reconstructed from
    /// the lane store) — the SHA-exact round-trip entry point.
    pub fn tree_oid_of_commit(&self, sha: &str) -> Result<String> {
        let rev = self
            .rev_of(sha)
            .ok_or_else(|| Error::Chain(format!("commit {sha} not in lane store")))?;
        self.tree_oid_at(rev)
    }

    /// Total UNCOMPRESSED bytes of every stored chain record (the head
    /// full record plus all reverse deltas). This is the frame-resident
    /// content BEFORE zstd — the honest measure of what variant-delta
    /// changes: at scale zstd's 128 MB window cannot dedup N full trees
    /// once they overflow it, so the win must live in this pre-compression
    /// content. A variant group here is ~one base subtree + small deltas,
    /// not N full trees.
    pub fn uncompressed_record_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        self.walk_records(&mut |_pos, rec| {
            total += rec.len() as u64;
            Ok(false)
        })?;
        Ok(total)
    }

    pub fn lane_of(&self, rev: usize) -> LaneId {
        self.lane_of[rev]
    }

    // ------------------------------------------------------- internals

    /// Walk the combined-state chain newest-first, handing each stored
    /// record (raw codec bytes) to `visit(pos, rec)`; `visit` returns
    /// true to stop. f0 is position 0; then the f1 records and every cold
    /// frame's records in stored order. This is the sealed TREES chain's
    /// walk (`store.rs::walk_tree_views`): the records are REVERSE DELTAS,
    /// so a cold frame's zstd refPrefix is the CANONICAL FULL-VIEW BYTES
    /// at its newest boundary — recomputed from the working view walked so
    /// far, NOT the previous record. f1 is anchored on the f0 record (the
    /// newest full-view bytes). The working view is reconstructed here
    /// only to supply those boundary anchors.
    fn walk_records(
        &self,
        visit: &mut dyn FnMut(usize, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let head = match self.depot.read_f0(CHAIN) {
            Ok(frame) => decompress_frame(&frame, None).map_err(cf)?,
            Err(wikimak_depot::Error::NoFrame) => return Ok(()),
            Err(e) => return Err(cf(e.to_string())),
        };
        let mut cur: Option<View> = None;
        depot::apply_mut(&mut cur, &codec::decode(&head)?);
        let mut pos = 0usize;
        if visit(pos, &head)? {
            return Ok(());
        }
        if let Some(f1) = self.depot.read_f1(CHAIN).map_err(|e| cf(e.to_string()))? {
            let mut stopped = false;
            stream_f1_records(&f1, &head, &mut |rec| {
                pos += 1;
                depot::apply_mut(&mut cur, &codec::decode(rec)?);
                stopped = visit(pos, rec)?;
                Ok(stopped)
            })?;
            if stopped {
                return Ok(());
            }
        }
        for cold in self.depot.cold_iter(CHAIN).map_err(|e| cf(e.to_string()))? {
            let frame = cold.map_err(|e| cf(e.to_string()))?;
            // Canonical full-view bytes at this frame's newest boundary.
            let anchor = codec::encode(&depot::diff(None, cur.as_ref()));
            let mut stopped = false;
            stream_f1_records(&frame, &anchor, &mut |rec| {
                pos += 1;
                depot::apply_mut(&mut cur, &codec::decode(rec)?);
                stopped = visit(pos, rec)?;
                Ok(stopped)
            })?;
            if stopped {
                return Ok(());
            }
        }
        Ok(())
    }
}

/// `parents[i]` — the in-scope parent revision indices of the commit at
/// `dag.order` position `i`, FIRST PARENT FIRST, remapped through
/// `index_of` from ONE `rev-list --parents` pass. Out-of-scope parents
/// (none in a full non-shallow branches+tags walk) are dropped. Every
/// kept index is `< i` because `dag.order` (walk_order) lists parents
/// before children — exactly what `assign_lanes` requires.
fn parents_by_index(
    repo: &Path,
    index_of: &HashMap<String, usize>,
) -> Result<Vec<Vec<usize>>> {
    let out = crate::git_str(
        repo,
        &["rev-list", "--parents", "--branches", "--tags"],
    )?;
    let mut parents: Vec<Vec<usize>> = vec![Vec::new(); index_of.len()];
    for line in out.lines() {
        let mut it = line.split(' ');
        let Some(sha) = it.next() else { continue };
        let Some(&i) = index_of.get(sha) else { continue };
        parents[i] = it.filter_map(|p| index_of.get(p).copied()).collect();
    }
    Ok(parents)
}

/// ref name → resolved commit sha, for branches and commit-peeled tags.
/// A tag peeling to a tree (no commit) is skipped — the lane store
/// indexes commits; a tree-only tag has no revision to bind.
fn collect_ref_commits(repo: &Path) -> Result<Vec<(String, String)>> {
    let out = crate::git_str(
        repo,
        &[
            "for-each-ref",
            "--format=%(objectname) %(objecttype) %(*objectname) %(refname)",
            "refs/heads",
            "refs/tags",
        ],
    )?;
    let mut v = Vec::new();
    for line in out.lines() {
        let mut it = line.splitn(4, ' ');
        let objn = it.next().unwrap_or_default();
        let objt = it.next().unwrap_or_default();
        let peeled = it.next().unwrap_or_default();
        let name = it.next().unwrap_or_default();
        // A tag's peeled commit is %(*objectname); a branch points at a
        // commit directly.
        let sha = if objt == "tag" { peeled } else { objn };
        if sha.is_empty() || name.is_empty() {
            continue;
        }
        v.push((name.to_string(), sha.to_string()));
    }
    Ok(v)
}

// ------------------------------------------------------- persistence
//
// A minimal sidecar `dir/meta.sqlite` (kv + two tables) recording what
// a reopen needs and nothing more. The combined-state chain itself (the
// tree data + the per-revision variant base pointers) lives in the
// depot and is NOT duplicated here; meta only maps shas/refs to the
// revision/lane axis the chain is indexed by.

const META_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS revs(rev INTEGER PRIMARY KEY, sha TEXT NOT NULL, lane INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS refs(name TEXT PRIMARY KEY, sha TEXT NOT NULL, rev INTEGER NOT NULL, lane INTEGER NOT NULL);
";
const META_SCHEMA_VERSION: &str = "1";

fn meta_path(dir: &Path) -> std::path::PathBuf {
    dir.join("meta.sqlite")
}

fn map_sql(e: rusqlite::Error) -> Error {
    Error::Meta(e.to_string())
}

/// Write the sha→(rev,lane) and ref→(sha,rev,lane) bindings so the store
/// reopens without any repo access.
fn persist_meta(
    dir: &Path,
    n_rev: usize,
    lane_of: &[LaneId],
    sha_of: &[String],
    refs: &HashMap<String, String>,
) -> Result<()> {
    let mut conn = rusqlite::Connection::open(meta_path(dir)).map_err(map_sql)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;").map_err(map_sql)?;
    conn.execute_batch(META_SCHEMA).map_err(map_sql)?;
    let tx = conn.transaction().map_err(map_sql)?;
    tx.execute("DELETE FROM revs", []).map_err(map_sql)?;
    tx.execute("DELETE FROM refs", []).map_err(map_sql)?;
    tx.execute(
        "INSERT OR REPLACE INTO kv(key,value) VALUES('schema',?1),('n_rev',?2)",
        rusqlite::params![META_SCHEMA_VERSION, n_rev.to_string()],
    )
    .map_err(map_sql)?;
    {
        let mut ins = tx
            .prepare("INSERT INTO revs(rev,sha,lane) VALUES(?1,?2,?3)")
            .map_err(map_sql)?;
        for (i, sha) in sha_of.iter().enumerate() {
            ins.execute(rusqlite::params![i as i64, sha, lane_of[i] as i64])
                .map_err(map_sql)?;
        }
    }
    {
        let idx: HashMap<&str, usize> =
            sha_of.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
        let mut ins = tx
            .prepare("INSERT INTO refs(name,sha,rev,lane) VALUES(?1,?2,?3,?4)")
            .map_err(map_sql)?;
        for (name, sha) in refs {
            let rev = idx[sha.as_str()];
            ins.execute(rusqlite::params![name, sha, rev as i64, lane_of[rev] as i64])
                .map_err(map_sql)?;
        }
    }
    tx.commit().map_err(map_sql)?;
    Ok(())
}

/// Reload `(n_rev, lane_of, sha_of, refs)` from the sidecar.
#[allow(clippy::type_complexity)]
fn load_meta(
    dir: &Path,
) -> Result<(usize, Vec<LaneId>, Vec<String>, HashMap<String, String>)> {
    let p = meta_path(dir);
    if !p.exists() {
        return Err(Error::Chain(format!("no lane store at {}", dir.display())));
    }
    let conn = rusqlite::Connection::open_with_flags(
        &p,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(map_sql)?;
    let schema: String = conn
        .query_row("SELECT value FROM kv WHERE key='schema'", [], |r| r.get(0))
        .map_err(map_sql)?;
    if schema != META_SCHEMA_VERSION {
        return Err(Error::Chain(format!(
            "lane store schema {schema:?}, this build reads {META_SCHEMA_VERSION}"
        )));
    }
    let n_rev: i64 = conn
        .query_row("SELECT value FROM kv WHERE key='n_rev'", [], |r| {
            let v: String = r.get(0)?;
            Ok(v.parse::<i64>().unwrap_or(0))
        })
        .map_err(map_sql)?;
    let n_rev = n_rev as usize;
    let mut sha_of = vec![String::new(); n_rev];
    let mut lane_of = vec![0 as LaneId; n_rev];
    {
        let mut q = conn.prepare("SELECT rev,sha,lane FROM revs").map_err(map_sql)?;
        let rows = q
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .map_err(map_sql)?;
        for row in rows {
            let (rev, sha, lane) = row.map_err(map_sql)?;
            let rev = rev as usize;
            if rev >= n_rev {
                return Err(Error::Chain(format!("meta: rev {rev} out of range")));
            }
            sha_of[rev] = sha;
            lane_of[rev] = lane as LaneId;
        }
    }
    let mut refs = HashMap::new();
    {
        let mut q = conn.prepare("SELECT name,sha FROM refs").map_err(map_sql)?;
        let rows = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(map_sql)?;
        for row in rows {
            let (name, sha) = row.map_err(map_sql)?;
            refs.insert(name, sha);
        }
    }
    Ok((n_rev, lane_of, sha_of, refs))
}

/// Stream the u64-length-prefixed records of an f1 frame in stored
/// (newest-first) order, one record at a time via `FrameDecoder`.
/// `visit` returns true to stop early.
fn stream_f1_records(
    frame: &[u8],
    anchor: &[u8],
    visit: &mut dyn FnMut(&[u8]) -> Result<bool>,
) -> Result<()> {
    use std::io::Read as _;
    let mut dec = FrameDecoder::new(frame, Some(anchor)).map_err(cf)?;
    let read_full = |dec: &mut FrameDecoder<'_>, buf: &mut [u8]| -> Result<usize> {
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
        let mut hdr = [0u8; 8];
        match read_full(&mut dec, &mut hdr)? {
            0 => return Ok(()),
            8 => {}
            _ => return Err(Error::Chain("truncated lane record header".into())),
        }
        let len = u64::from_le_bytes(hdr) as usize;
        let mut rec = vec![0u8; len];
        if read_full(&mut dec, &mut rec)? != len {
            return Err(Error::Chain("truncated lane record body".into()));
        }
        if visit(&rec)? {
            return Ok(());
        }
    }
}

/// One incremental prepend to the combined-state chain, reusing the
/// TREES store's seal discipline (`store.rs::prepend_batch`, the
/// `Demote::Dropped` case): `head_record` is the new full f0; `staged`
/// are this batch's reverse-delta records NEWEST-FIRST, whose OLDEST
/// entry is the bridge that rebuilds the former head from the new one —
/// so the old head is superseded and nothing else joins the accumulator.
/// The new f1 is stream-composed (never materialized raw) from `staged`
/// then the old f1's bytes, UNLESS absorbing the batch would push the
/// accumulator past the seal threshold — then the old f1 retires verbatim
/// to cold; and if the batch alone exceeds the threshold the just-written
/// f1 is sealed immediately, so no later prepend ever recompresses a huge
/// frame. The chain MUST already be seeded (f0 present).
fn seal_prepend(depot: &Depot, head_record: &[u8], staged: &[Vec<u8>], level: i32) -> Result<()> {
    let old_f1 = depot.read_f1(CHAIN).map_err(|e| cf(e.to_string()))?;
    let old_raw_len = match &old_f1 {
        Some(z) => zstd::zstd_safe::get_frame_content_size(z)
            .map_err(|_| cf("zstd frame content size".into()))?
            .ok_or_else(|| cf("zstd frame without content size".into()))?,
        None => 0,
    };
    let entries_len: u64 = staged.iter().map(|r| 8 + r.len() as u64).sum();
    let seal_old = old_f1.is_some() && old_raw_len + entries_len > seal_threshold();
    let total_raw = entries_len + if seal_old { 0 } else { old_raw_len };
    let mut enc = FrameEncoder::new(total_raw, Some(head_record), level).map_err(cf)?;
    for r in staged {
        enc.write(&(r.len() as u64).to_le_bytes()).map_err(cf)?;
        enc.write(r).map_err(cf)?;
    }
    if !seal_old {
        if let Some(z) = &old_f1 {
            use std::io::Read as _;
            // The old f1 was compressed against the CURRENT f0 record.
            let old_head = decompress_frame(&depot.read_f0(CHAIN).map_err(|e| cf(e.to_string()))?, None)
                .map_err(cf)?;
            let mut dec = FrameDecoder::new(z, Some(&old_head)).map_err(cf)?;
            let mut buf = vec![0u8; 128 << 10];
            loop {
                let n = dec.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                enc.write(&buf[..n]).map_err(cf)?;
            }
        }
    }
    let new_f1 = enc.finish().map_err(cf)?;
    let new_f0 = compress_frame(head_record, None, level).map_err(cf)?;
    depot
        .prepend(CHAIN, &new_f0, Some(&new_f1), seal_old)
        .map_err(|e| cf(e.to_string()))?;
    if total_raw > seal_threshold() {
        depot.seal_f1(CHAIN).map_err(|e| cf(e.to_string()))?;
    }
    Ok(())
}

fn open_depot(dir: &Path) -> Result<Depot> {
    std::fs::create_dir_all(dir.join("depot"))?;
    Depot::open(DepotConfig {
        root: dir.join("depot"),
        max_chain_id: 1,
        file_size_threshold: 4 << 20,
        eviction_dead_ratio: 0.5,
    })
    .map_err(|e| cf(e.to_string()))
}

