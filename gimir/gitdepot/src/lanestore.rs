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
//!   versions are stored as sibling variant nodes (`name\0<slot>`) with a
//!   lane bitmap — the §2 encoding in [`crate::layer`], keyed via
//!   [`crate::reslot`]. Content is byte-stable across lane-membership changes.
//! * **Encoder.** [`crate::oidenc`] holds the union as slot state per path
//!   and, per revision, emits the REVERSE delta of only the advancing/dying
//!   lanes' tree diffs — reading git tree objects by oid on demand (through
//!   a cache) and pruning unchanged subtrees by oid (O(changed) per commit).
//!   Removals are HOLES, not tombstones: the union occludes no host, so
//!   "this key is gone" is "not occluded here", which resolves to nothing
//!   over the empty backdrop. The positive full-state head (f0 / each seal)
//!   is materialized only at seal boundaries — never per commit.
//!
//! ## Reconstruction
//!
//! Walk the chain newest-first, folding each reverse record into the working
//! view with `apply_mut` (O(delta); over the empty backdrop a removal hole
//! is resolved as a tombstone). Then [`crate::layer::extract_lane_entries`]
//! pulls the target revision's lane out of the §2 union bytes — that commit's
//! git tree. Its git tree oid equals the real object (SHA-exact).

use std::collections::HashMap;
use std::path::Path;

use depot::{codec, View};
use wikimak_depot::{
    compress_frame, decompress_frame, Depot, DepotConfig, FrameDecoder, FrameEncoder,
};

use crate::lanes::{assign_lanes, LaneId};
use crate::oidenc::{Encoder, Objects};
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
/// Cache BUDGET in bytes, not tree count: git.git has huge directory trees
/// (thousands of entries each), so a count cap can't bound RAM — 4096 big
/// trees was ~900 MB. This keeps the hot working set (root + recently
/// touched dirs) resident and is small on purpose: the cache saves cat-file
/// IPC on re-reads, but the encode's throughput floor is blob fetches, so a
/// bigger cache buys no speed.
const TREE_CACHE_BUDGET: usize = 64 << 20; // 64 MB

fn tree_bytes(ents: &[(Vec<u8>, crate::oidenc::Ent)]) -> usize {
    ents.iter().map(|(n, e)| n.len() + e.mode.len() + e.oid.len() + 48).sum::<usize>() + 32
}

struct Cat<'a> {
    cat: &'a mut crate::CatFile,
    trees: HashMap<String, (TreeEnts, usize)>, // oid → (entries, byte size)
    order: std::collections::VecDeque<String>, // FIFO eviction order
    bytes: usize,
    /// git object fetches actually issued (tree cache MISSES + blob reads) —
    /// the honest measure of ingest work, used to prove an update is O(new).
    reads: usize,
}

impl<'a> Cat<'a> {
    fn new(cat: &'a mut crate::CatFile) -> Self {
        Cat { cat, trees: HashMap::new(), order: std::collections::VecDeque::new(), bytes: 0, reads: 0 }
    }
}

impl crate::oidenc::Objects for Cat<'_> {
    fn tree(&mut self, oid: &str) -> Result<TreeEnts> {
        if let Some((t, _)) = self.trees.get(oid) {
            return Ok(t.clone());
        }
        self.reads += 1;
        let ents = std::sync::Arc::new(crate::oidenc::parse_tree(&self.cat.get(oid)?)?);
        let sz = tree_bytes(&ents);
        while self.bytes + sz > TREE_CACHE_BUDGET {
            let Some(old) = self.order.pop_front() else { break };
            if let Some((_, osz)) = self.trees.remove(&old) {
                self.bytes -= osz;
            }
        }
        self.trees.insert(oid.to_string(), (ents.clone(), sz));
        self.order.push_back(oid.to_string());
        self.bytes += sz;
        Ok(ents)
    }
    fn blob(&mut self, oid: &str) -> Result<depot::Bytes> {
        self.reads += 1;
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

/// Set compact-lane bit `l` in a little-endian lane bitmap.
fn set_live_bit(bm: &mut Vec<u8>, l: usize) {
    let byte = l / 8;
    if bm.len() <= byte {
        bm.resize(byte + 1, 0);
    }
    bm[byte] |= 1 << (l % 8);
}
/// Clear compact-lane bit `l`.
fn clear_live_bit(bm: &mut [u8], l: usize) {
    let byte = l / 8;
    if byte < bm.len() {
        bm[byte] &= !(1 << (l % 8));
    }
}
/// Lowest set compact-lane bit, if any.
fn first_live_bit(bm: &[u8]) -> Option<usize> {
    for (byte, &b) in bm.iter().enumerate() {
        if b != 0 {
            return Some(byte * 8 + b.trailing_zeros() as usize);
        }
    }
    None
}

/// The lane assignment for a fixed topological `order`: monotonic lanes
/// (`lanes::assign_lanes`), their births/deaths (death = the revision a merge
/// second-parent's lane ends), and the compacted (reused) indices
/// (`lanes::compact_lanes`). An UPDATE reuses this over `old_shas ++ new`, and
/// because processing revisions `0..old_n` is identical to the original encode
/// (same births/deaths in range), the compact index of every already-stored
/// commit is unchanged — §8 stable lanes without renumbering.
struct LanePlan {
    n_rev: usize,
    /// Compact lane per revision.
    lane_of: Vec<LaneId>,
    width: usize,
    dying_at: Vec<Vec<LaneId>>,
}

fn plan_lanes(repo: &Path, order: &[String]) -> Result<LanePlan> {
    let n_rev = order.len();
    let mut index_of: HashMap<String, usize> = HashMap::with_capacity(n_rev);
    for (i, s) in order.iter().enumerate() {
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
    Ok(LanePlan { n_rev, lane_of, width, dying_at })
}

/// `sha -> root tree oid` for every reachable commit (one log pass).
fn tree_map(repo: &Path) -> Result<HashMap<String, String>> {
    let mut tree_of = HashMap::new();
    for line in crate::git_str(repo, &["log", "--format=%H %T", "--branches", "--tags"])?.lines() {
        if let Some((s, t)) = line.split_once(' ') {
            tree_of.insert(s.to_string(), t.to_string());
        }
    }
    Ok(tree_of)
}

/// Mutable per-run encoding state shared by the initial encode and an update.
struct RunState {
    lane_tree: Vec<Option<String>>,
    live: Vec<u8>,
    batch: Vec<Vec<u8>>,
    batch_bytes: u64,
}

/// Encode revisions `range` onto the depot: advance the encoder one revision at
/// a time (O(changed) each), accumulate reverse-delta records, and seal a batch
/// (a prepend + fresh full head) whenever it fills. `fresh` seeds f0 from the
/// very first revision (a new store); an update leaves `fresh=false` so every
/// new revision is a prepended reverse delta over the reconstructed boundary.
#[allow(clippy::too_many_arguments)]
fn run_range(
    depot: &Depot,
    enc: &mut Encoder,
    objs: &mut Cat,
    plan: &LanePlan,
    tree_of: &HashMap<String, String>,
    sha_of: &[String],
    range: std::ops::Range<usize>,
    st: &mut RunState,
    batch_bound: u64,
    level: i32,
    fresh: bool,
) -> Result<()> {
    use crate::oidenc::{Ent, Trans};
    for i in range {
        let sha = &sha_of[i];
        let tree_oid = tree_of.get(sha).ok_or_else(|| cf(format!("no tree for {sha}")))?.clone();
        let l = plan.lane_of[i] as usize;
        let dead_old: Vec<(usize, Option<Ent>)> = plan.dying_at[i]
            .iter()
            .filter(|&&d| d != plan.lane_of[i])
            .map(|&d| (d as usize, st.lane_tree[d as usize].clone().map(Ent::dir)))
            .collect();
        let adv_old = st.lane_tree[l].clone().map(Ent::dir);
        let adv_new = Ent::dir(tree_oid.clone());
        let mut trans: Vec<Trans> = Vec::with_capacity(dead_old.len() + 1);
        for (d, oe) in &dead_old {
            trans.push((*d, oe.as_ref(), None));
        }
        trans.push((l, adv_old.as_ref(), Some(&adv_new)));

        let prev_live = st.live.clone();
        for &d in &plan.dying_at[i] {
            clear_live_bit(&mut st.live, d as usize);
        }
        set_live_bit(&mut st.live, l);
        let new_live = st.live.clone();

        let rev = enc.advance(&trans, objs, &prev_live, &new_live)?;
        for &d in &plan.dying_at[i] {
            st.lane_tree[d as usize] = None;
        }
        st.lane_tree[l] = Some(tree_oid);

        if fresh && i == 0 {
            // Seed f0 with the positive full-state (the newest, i.e. only,
            // revision so far); `rev` (a delta from nothing) is discarded.
            let f0raw = codec::encode(&enc.full(objs, &st.live)?);
            let f0 = compress_frame(&f0raw, None, level).map_err(cf)?;
            depot.prepend(CHAIN, &f0, None, false).map_err(|e| cf(e.to_string()))?;
        } else {
            let rec = codec::encode(&rev);
            st.batch_bytes += 8 + rec.len() as u64;
            st.batch.push(rec);
        }
        if !st.batch.is_empty() && st.batch_bytes >= batch_bound {
            let head = codec::encode(&enc.full(objs, &st.live)?);
            let staged: Vec<Vec<u8>> = st.batch.drain(..).rev().collect();
            seal_prepend(depot, &head, &staged, level)?;
            st.batch_bytes = 0;
            depot.flush().map_err(|e| cf(e.to_string()))?;
        }
    }
    Ok(())
}

/// Seal any remaining batch and flush.
fn finish_range(depot: &Depot, enc: &Encoder, objs: &mut Cat, st: &mut RunState, level: i32) -> Result<()> {
    if !st.batch.is_empty() {
        let head = codec::encode(&enc.full(objs, &st.live)?);
        let staged: Vec<Vec<u8>> = st.batch.drain(..).rev().collect();
        seal_prepend(depot, &head, &staged, level)?;
    }
    depot.flush().map_err(|e| cf(e.to_string()))?;
    Ok(())
}

/// Descend the boundary lane's tip tree by `path` (via the object source's
/// cache) to the git `(mode, oid)` of the entry there — the variant identity
/// §6 reads from the lane trees, never by hashing stored content (correct for
/// gitlinks too, whose content is a commit id, not a blob).
fn lookup_oid(objs: &mut Cat, tip: &str, path: &[u8]) -> Result<Option<(Vec<u8>, String)>> {
    let comps: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
    let mut cur = tip.to_string();
    for (i, comp) in comps.iter().enumerate() {
        let ents = objs.tree(&cur)?;
        let Some((_, e)) = ents.iter().find(|(n, _)| n == comp) else { return Ok(None) };
        if i + 1 == comps.len() {
            return Ok(Some((e.mode.clone(), e.oid.clone())));
        }
        if !e.is_dir {
            return Ok(None);
        }
        cur = e.oid.clone();
    }
    Ok(None)
}

/// Reconstruct the encoder state at the stored boundary (the newest stored
/// revision) for an incremental update: the live lanes and their tip trees come
/// from the plan + `tree_of`; the per-path SLOTS and lane bitmaps are read back
/// from the stored f0 (so a prepended reverse delta reproduces f0 exactly); and
/// each variant's `(mode, oid)` identity is sourced from the boundary lane trees
/// (§6), not by hashing f0's content. Returns the seeded encoder plus the
/// boundary `lane_tree`/`live` state.
fn reconstruct_boundary(
    depot: &Depot,
    plan: &LanePlan,
    tree_of: &HashMap<String, String>,
    sha_of: &[String],
    old_n: usize,
    objs: &mut Cat,
) -> Result<(Encoder, Vec<Option<String>>, Vec<u8>)> {
    // The ACTUAL live lanes and their tip trees at the boundary — replay the
    // per-revision lane_tree updates the encode loop performs (a compact lane
    // dies when it appears in `dying_at`, is (re)born/advanced when it is the
    // revision's lane). Not `birth<=r<death`: a lane merged as a 2nd parent can
    // still continue via a first-parent child, and a compacted index can be
    // freed and reused, so only this replay gives the true boundary state. It
    // is O(old_n) integer work — no object reads.
    let mut alive = vec![false; plan.width];
    let mut tip_rev = vec![usize::MAX; plan.width];
    for r in 0..old_n {
        for &d in &plan.dying_at[r] {
            alive[d as usize] = false;
        }
        let c = plan.lane_of[r] as usize;
        alive[c] = true;
        tip_rev[c] = r;
    }
    let mut lane_tree: Vec<Option<String>> = vec![None; plan.width];
    let mut live: Vec<u8> = Vec::new();
    for c in 0..plan.width {
        if alive[c] {
            set_live_bit(&mut live, c);
            let tip = tree_of
                .get(&sha_of[tip_rev[c]])
                .ok_or_else(|| cf(format!("no tree for boundary tip {}", sha_of[tip_rev[c]])))?;
            lane_tree[c] = Some(tip.clone());
        }
    }

    let f0 = match depot.read_f0(CHAIN) {
        Ok(frame) => decompress_frame(&frame, None).map_err(cf)?,
        Err(e) => return Err(cf(format!("no boundary f0: {e}"))),
    };
    // (path, slot, bitmap) per variant; an omitted `lanes` child ⇒ all live.
    let mut raw: Vec<(Vec<u8>, u32, Vec<u8>)> = Vec::new();
    crate::layer::visit_entries(&f0, |e| {
        let bm = match e.bitmap {
            Some(b) => b.to_vec(),
            None => live.clone(),
        };
        raw.push((e.path.to_vec(), e.slot, bm));
    })
    .map_err(|e| cf(format!("decode boundary f0: {e:?}")))?;

    let mut variants = Vec::with_capacity(raw.len());
    for (path, slot, bitmap) in raw {
        let lane = first_live_bit(&bitmap).ok_or_else(|| {
            cf(format!(
                "boundary variant {} slot {slot} carries no live lane",
                String::from_utf8_lossy(&path)
            ))
        })?;
        let tip = lane_tree[lane]
            .as_ref()
            .ok_or_else(|| cf(format!("boundary variant in dead lane {lane}")))?
            .clone();
        let (mode, oid) = lookup_oid(objs, &tip, &path)?.ok_or_else(|| {
            cf(format!("boundary variant {} absent from lane {lane} tree", String::from_utf8_lossy(&path)))
        })?;
        variants.push(crate::oidenc::SeedVariant { path, slot, mode, oid, bitmap });
    }
    Ok((Encoder::seed(variants), lane_tree, live))
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

    /// Stores every revision's live lanes as ONE §2 union-variant tree
    /// ([`crate::layer`]). No base, no delta-of-delta, no base-switching:
    /// content nodes are byte-stable across lane-membership changes, so a
    /// trunk commit that touches one lane leaves every other lane's content
    /// untouched and the frame-to-frame reverse delta is proportional to
    /// real blob churn. Reconstruction is SHA-exact.
    pub fn encode_repo_union(repo: &Path, dir: &Path, level: i32) -> Result<LaneStore> {
        Ok(Self::encode_repo_union_stats(repo, dir, level)?.0)
    }

    /// As [`encode_repo_union`](Self::encode_repo_union), also returning the
    /// number of git object fetches issued (the ingest-work measure).
    pub fn encode_repo_union_stats(repo: &Path, dir: &Path, level: i32) -> Result<(LaneStore, usize)> {
        // Topology only (cheap): revision order + lane assignment.
        let dag = crate::dag_scope(repo, &[])?;
        let sha_of = dag.order.clone();
        let plan = plan_lanes(repo, &sha_of)?;
        let tree_of = tree_map(repo)?;

        let depot = open_depot(dir)?;
        let mut cat = crate::CatFile::new(repo)?;
        let mut objs = Cat::new(&mut cat); // one persistent tree cache
        let mut enc = Encoder::new();
        let mut st = RunState {
            lane_tree: vec![None; plan.width],
            live: Vec::new(),
            batch: Vec::new(),
            batch_bytes: 0,
        };
        run_range(&depot, &mut enc, &mut objs, &plan, &tree_of, &sha_of, 0..plan.n_rev, &mut st, batch_ram_bound(), level, true)?;
        if plan.n_rev > 0 {
            finish_range(&depot, &enc, &mut objs, &mut st, level)?;
        }
        let reads = objs.reads;

        let sha_to_rev: HashMap<String, usize> =
            sha_of.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
        let refs = live_refs(repo, &sha_to_rev)?;
        persist_meta(dir, plan.n_rev, &plan.lane_of, &sha_of, &refs)?;
        Ok((LaneStore { depot, n_rev: plan.n_rev, lane_of: plan.lane_of, sha_of, sha_to_rev, refs }, reads))
    }

    /// Incremental O(new) update (§11): fold only the NEW commits' union deltas
    /// onto the stored boundary, prepending them — no full re-encode. Stored
    /// commits keep their lanes (§8): the fixed order `old_shas ++ new` makes
    /// `plan_lanes` reproduce every stored commit's compact lane, asserted below.
    /// The boundary encoder state is reconstructed from the stored f0 + boundary
    /// lane trees (§6), then advanced through the new revisions only.
    pub fn update(repo: &Path, dir: &Path, level: i32) -> Result<LaneStore> {
        Ok(Self::update_stats(repo, dir, level)?.0)
    }

    /// As [`update`](Self::update), also returning `(new_revisions_advanced,
    /// git_object_reads)` — the O(new) proof: an update advances only the new
    /// revisions and its reads are bounded by the boundary frontier + new work,
    /// not the whole history.
    pub fn update_stats(repo: &Path, dir: &Path, level: i32) -> Result<(LaneStore, usize, usize)> {
        let existing = LaneStore::open(dir)?;
        let old_n = existing.n_rev;
        let old_sha = existing.sha_of.clone();
        let old_lane = existing.lane_of.clone();
        if old_n == 0 {
            drop(existing);
            let (s, reads) = LaneStore::encode_repo_union_stats(repo, dir, level)?;
            let n = s.n_rev;
            return Ok((s, n, reads));
        }

        // Fixed order: stored commits (unchanged order) then the new reachable
        // commits in the full walk order — a valid topo order (every parent
        // precedes its child) that freezes the stored prefix.
        let dag = crate::dag_scope(repo, &[])?;
        let old_set: std::collections::HashSet<&str> = old_sha.iter().map(|s| s.as_str()).collect();
        let mut order = old_sha.clone();
        for c in &dag.order {
            if !old_set.contains(c.as_str()) {
                order.push(c.clone());
            }
        }
        if order.len() == old_n {
            // Nothing new — just refresh refs and return.
            let sha_to_rev = existing.sha_to_rev.clone();
            let refs = live_refs(repo, &sha_to_rev)?;
            persist_meta(dir, old_n, &old_lane, &old_sha, &refs)?;
            return Ok((LaneStore { refs, ..existing }, 0, 0));
        }

        let plan = plan_lanes(repo, &order)?;
        // §8 stability: every stored commit keeps its compact lane.
        if plan.lane_of[..old_n] != old_lane[..] {
            return Err(cf("incremental update would renumber a stored lane \
                (non-fast-forward history rewrite is not supported here)".into()));
        }
        let tree_of = tree_map(repo)?;

        let depot = existing.depot;
        let mut cat = crate::CatFile::new(repo)?;
        let mut objs = Cat::new(&mut cat);
        // Reconstruct the encoder at the boundary from stored bytes + lane trees.
        let (mut enc, lane_tree, live) = reconstruct_boundary(&depot, &plan, &tree_of, &old_sha, old_n, &mut objs)?;
        let mut st = RunState { lane_tree, live, batch: Vec::new(), batch_bytes: 0 };
        // Advance ONLY the new revisions (O(new)); prepend their reverse deltas.
        run_range(&depot, &mut enc, &mut objs, &plan, &tree_of, &order, old_n..plan.n_rev, &mut st, batch_ram_bound(), level, false)?;
        finish_range(&depot, &enc, &mut objs, &mut st, level)?;
        let reads = objs.reads;
        let new_revs = plan.n_rev - old_n;

        let sha_to_rev: HashMap<String, usize> =
            order.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
        let refs = live_refs(repo, &sha_to_rev)?;
        persist_meta(dir, plan.n_rev, &plan.lane_of, &order, &refs)?;
        Ok((LaneStore { depot, n_rev: plan.n_rev, lane_of: plan.lane_of, sha_of: order, sha_to_rev, refs }, new_revs, reads))
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
            apply_reverse_record(&mut cur, rec)?;
            if pos == target_pos {
                done = Some(cur.clone().unwrap_or_default());
                return Ok(true);
            }
            Ok(false)
        })?;
        done.ok_or_else(|| Error::Chain(format!("chain fell short of revision {rev}")))
    }

    /// Lane `lane`'s flat `(path, mode, content)` entries at revision `rev`,
    /// extracted from the reconstructed §2 union state via the authoritative
    /// `layer` extractor (on the canonical union bytes).
    pub fn lane_entries_at(&self, rev: usize) -> Result<Vec<(Vec<u8>, crate::layer::Mode, Vec<u8>)>> {
        let combined = self.combined_at(rev)?;
        let bytes = codec::encode(&depot::diff(None, Some(&combined)));
        crate::layer::extract_lane_entries(&bytes, self.lane_of[rev] as u32).map_err(|e| cf(format!("{e:?}")))
    }

    /// The git tree oid of the commit at revision `rev` (reconstructed) — the
    /// SHA-exact ground truth from the stored §2 union bytes.
    pub fn tree_oid_at(&self, rev: usize) -> Result<String> {
        crate::layer::tree_oid_of_entries(&self.lane_entries_at(rev)?).map_err(|e| cf(format!("{e:?}")))
    }

    /// The commit-at-`rev`'s git tree as a nested `depot::View`: leaf files
    /// carry the blob content and a `mode` attr (git octal, e.g. `100644` /
    /// `100664`). This is the shape the mirror's readout and export consume, so
    /// the union is the mirror's TREES payload with no per-commit tree stored.
    pub fn tree_view_at(&self, rev: usize) -> Result<View> {
        use std::sync::Arc;
        let mut root = View::default();
        for (path, mode, content) in self.lane_entries_at(rev)? {
            let segs: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
            let mut cur = &mut root;
            for (i, s) in segs.iter().enumerate() {
                if i + 1 == segs.len() {
                    let mut attrs = depot::Attrs::new();
                    attrs.insert(b"mode".to_vec(), mode.octal());
                    cur.children.insert(
                        s.to_vec(),
                        Arc::new(View { blob: Some(content.clone().into()), attrs, children: Default::default() }),
                    );
                } else {
                    let e = cur
                        .children
                        .entry(s.to_vec())
                        .or_insert_with(|| Arc::new(View::default()));
                    cur = Arc::make_mut(e);
                }
            }
        }
        Ok(root)
    }

    /// The commit named `sha`'s git tree as a nested `depot::View`.
    pub fn tree_view_of_commit(&self, sha: &str) -> Result<View> {
        let rev = self.rev_of(sha).ok_or_else(|| Error::Chain(format!("commit {sha} not in store")))?;
        self.tree_view_at(rev)
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
    /// record (raw codec bytes) to `visit(pos, rec)`; `visit` returns true
    /// to stop. f0 is position 0; then the f1 records and every cold frame's
    /// records in stored order. The records are REVERSE DELTAS (removals as
    /// holes) folded into the working view with `apply_mut` (O(delta)); a
    /// cold frame's zstd refPrefix is the canonical full-view bytes at its
    /// newest boundary, recomputed from the view walked so far. f1 is
    /// anchored on the f0 record (the newest full-view bytes).
    fn walk_records(
        &self,
        visit: &mut dyn FnMut(usize, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let head = match self.depot.read_f0(CHAIN) {
            Ok(frame) => decompress_frame(&frame, None).map_err(cf)?,
            Err(wikimak_depot::Error::NoFrame) => return Ok(()),
            Err(e) => return Err(cf(e.to_string())),
        };
        // The working view is reconstructed only to supply cold-frame
        // boundary anchors: each record is a reverse delta folded with
        // `apply_mut` (O(delta), not O(union)). Over the empty backdrop a
        // removal HOLE means "absent" — exactly a tombstone's effect — so it
        // is resolved as one. A cold frame's zstd refPrefix is the canonical
        // full-view bytes at its newest boundary, recomputed from the view.
        let mut cur: Option<View> = None;
        apply_reverse_record(&mut cur, &head)?;
        let mut pos = 0usize;
        if visit(pos, &head)? {
            return Ok(());
        }
        if let Some(f1) = self.depot.read_f1(CHAIN).map_err(|e| cf(e.to_string()))? {
            let mut stopped = false;
            stream_f1_records(&f1, &head, &mut |rec| {
                pos += 1;
                apply_reverse_record(&mut cur, rec)?;
                stopped = visit(pos, rec)?;
                Ok(stopped)
            })?;
            if stopped {
                return Ok(());
            }
        }
        for cold in self.depot.cold_iter(CHAIN).map_err(|e| cf(e.to_string()))? {
            let frame = cold.map_err(|e| cf(e.to_string()))?;
            let anchor = codec::encode(&depot::diff(None, cur.as_ref()));
            let mut stopped = false;
            stream_f1_records(&frame, &anchor, &mut |rec| {
                pos += 1;
                apply_reverse_record(&mut cur, rec)?;
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

/// Fold one reverse-delta record into the working view. Removals in the
/// record are HOLES (backdrop anchors); over the empty backdrop a hole means
/// "not present", so it is converted to a tombstone — whose `apply_mut`
/// effect over any lower view is exactly that removal — keeping the fold
/// O(delta). (The union never resolves over a non-empty backdrop, where the
/// two would differ.)
fn apply_reverse_record(cur: &mut Option<View>, rec: &[u8]) -> Result<()> {
    let mut layer = codec::decode(rec)?;
    holes_to_tombstones(&mut layer.root);
    depot::apply_mut(cur, &layer);
    Ok(())
}

/// Recursively rewrite each pure removal hole (a backdrop-anchored
/// `Keep`/no-attrs/no-children node) to a tombstone. Encoder deltas only
/// hole at leaves, but the walk is general.
fn holes_to_tombstones(node: &mut depot::Node) {
    if node.anchor == depot::Anchor::Backdrop
        && node.presence == depot::Presence::Live
        && node.blob == depot::BlobOp::Keep
        && node.attrs.is_none()
        && node.children.is_empty()
    {
        *node = depot::Node::tombstone();
        return;
    }
    for child in node.children.values_mut() {
        holes_to_tombstones(child);
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

/// The current ref → sha map, keeping only refs whose commit is in the store.
fn live_refs(repo: &Path, sha_to_rev: &HashMap<String, usize>) -> Result<HashMap<String, String>> {
    let mut refs = HashMap::new();
    for (name, sha) in collect_ref_commits(repo)? {
        if sha_to_rev.contains_key(&sha) {
            refs.insert(name, sha);
        }
    }
    Ok(refs)
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

