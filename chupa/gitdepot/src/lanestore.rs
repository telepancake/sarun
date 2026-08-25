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
//! Walk the chain newest-first at the BYTE level: reverse records compose
//! delta ∘ delta on a geometric stack (the asymptotically right "sum of
//! deltas" — cost ∝ delta bytes, log depth) and ONE `overlay_full` lands
//! them on the f0 base at the stop point (holes dissolve). Then
//! [`crate::layer::extract_lane_entries`] pulls the target revision's lane
//! out of the §2 union bytes — that commit's git tree. Its git tree oid
//! equals the real object (SHA-exact). `checkout_entries` streams a
//! commit's files off that fold with bounded memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use depot::{codec, View};
use wikimak_depot::{
    compress_frame, decompress_frame, Depot, DepotConfig, FrameDecoder, FrameEncoder,
};

use crate::lanes::{assign_lanes, LaneId};
use crate::oidenc::{Encoder, Objects};
use crate::{Error, Result};


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

struct Cat {
    kind: crate::HashKind,
    cat: crate::CatFile,
    trees: HashMap<String, (TreeEnts, usize)>, // oid → (entries, byte size)
    order: std::collections::VecDeque<String>, // FIFO eviction order
    bytes: usize,
    /// git object fetches actually issued (tree cache MISSES + blob reads) —
    /// the honest measure of ingest work, used to prove an update is O(new).
    reads: usize,
}

impl Cat {
    fn new(repo: &Path) -> Result<Self> {
        Ok(Cat {
            kind: crate::HashKind::of_repo(repo),
            cat: crate::CatFile::new(repo)?,
            trees: HashMap::new(),
            order: std::collections::VecDeque::new(),
            bytes: 0,
            reads: 0,
        })
    }
}

/// The encoder's object source for a run: the git repo (a persistent
/// `cat-file --batch` per reader), or a scanned wire pack's graph — no git
/// anywhere in that path.
pub(crate) enum ObjSource {
    Git(PathBuf),
    Pack(std::sync::Arc<crate::memgraph::PackGraph>),
}

impl ObjSource {
    fn objects(&self) -> Result<Box<dyn crate::oidenc::Objects + Send>> {
        Ok(match self {
            ObjSource::Git(repo) => Box::new(Cat::new(repo)?),
            ObjSource::Pack(pg) => Box::new(crate::memgraph::GraphObjects::new(pg.clone())?),
        })
    }
}

impl crate::oidenc::Objects for Cat {
    fn tree(&mut self, oid: &str) -> Result<TreeEnts> {
        if let Some((t, _)) = self.trees.get(oid) {
            return Ok(t.clone());
        }
        self.reads += 1;
        let ents = std::sync::Arc::new(crate::oidenc::parse_tree_oids(
            &self.cat.get(oid)?,
            self.kind.oid_len(),
        )?);
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
    fn reads(&self) -> usize {
        self.reads
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

/// The in-scope parent revision indices for `order`, from ONE
/// `rev-list --parents` pass over the repo. In a SHALLOW repo the
/// commits at the graft boundary lose their parent edges — callers that
/// need a stable prefix (the update path) must instead take the prefix
/// parents from the persisted meta and only compute the new suffix here.
fn compute_parents(repo: &Path, order: &[String]) -> Result<Vec<Vec<usize>>> {
    let mut index_of: HashMap<String, usize> = HashMap::with_capacity(order.len());
    for (i, s) in order.iter().enumerate() {
        index_of.insert(s.clone(), i);
    }
    parents_by_index(repo, &index_of)
}

fn plan_lanes(repo: &Path, order: &[String]) -> Result<(LanePlan, Vec<Vec<usize>>)> {
    let parents = compute_parents(repo, order)?;
    Ok((plan_from_parents(&parents), parents))
}

/// The lane plan for a fixed order given its per-revision parent indices.
/// Pure — no repo access — so an update can feed it a prefix taken from
/// stored meta plus a repo-computed suffix.
fn plan_from_parents(parents: &[Vec<usize>]) -> LanePlan {
    let n_rev = parents.len();
    let assignment = assign_lanes(parents);
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
    LanePlan { n_rev, lane_of, width, dying_at }
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
}

/// One revision's routed work for a §9 shard thread.
struct RevWork {
    pt: crate::oidenc::PathTrans,
    prev_live: std::sync::Arc<Vec<u8>>,
    new_live: std::sync::Arc<Vec<u8>>,
    /// Fresh store, first revision: seed this shard's f0 from the advanced
    /// state instead of batching the (delta-from-nothing) record.
    seed_f0: bool,
}

/// The per-shard encode worker (§9: one thread per shard, completely
/// separate tree/delta state, sharing the git repo as the object source
/// via its own reader). Consumes routed revisions in order — an empty
/// slice still advances the lockstep (an empty record) — batches reverse
/// records, seals at `bound`, and seals the remainder on channel close.
/// Returns the shard's encoder (for the caller's later use) and its git
/// object read count.
#[allow(clippy::too_many_arguments)]
fn shard_worker(
    shard: u64,
    mut enc: Encoder,
    src: &ObjSource,
    depot: &Depot,
    rx: std::sync::mpsc::Receiver<RevWork>,
    bound: u64,
    level: i32,
) -> Result<(Encoder, usize)> {
    let mut objs_box = src.objects()?;
    let objs = objs_box.as_mut();
    let mut batch: Vec<Vec<u8>> = Vec::new();
    let mut batch_bytes = 0u64;
    let mut live: Vec<u8> = Vec::new();
    for w in rx {
        let rev = enc.advance_paths(w.pt, objs, &w.prev_live, &w.new_live)?;
        live.clone_from(&w.new_live);
        if w.seed_f0 {
            // seal_record IS the §5 frame write: fold the stack, stream the
            // record — no View, no node tree.
            let f0raw = enc.seal_record(objs, &live)?;
            let f0 = compress_frame(&f0raw, None, level).map_err(cf)?;
            depot.prepend(shard, &f0, None, false).map_err(|e| cf(e.to_string()))?;
        } else {
            let rec = codec::encode(&rev);
            batch_bytes += 8 + rec.len() as u64;
            batch.push(rec);
        }
        if !batch.is_empty() && batch_bytes >= bound {
            let head = enc.seal_record(objs, &live)?;
            let staged: Vec<Vec<u8>> = batch.drain(..).rev().collect();
            seal_prepend(depot, shard, &head, &staged, level)?;
            batch_bytes = 0;
            depot.flush().map_err(|e| cf(e.to_string()))?;
        }
    }
    if !batch.is_empty() {
        let head = enc.seal_record(objs, &live)?;
        let staged: Vec<Vec<u8>> = batch.drain(..).rev().collect();
        seal_prepend(depot, shard, &head, &staged, level)?;
    }
    Ok((enc, objs.reads()))
}

/// Encode revisions `range` onto the depot and seal every shard's remaining
/// batch: the §9 run. The git side runs once per revision on this thread
/// (lane transitions distributed and routed by path hash, O(changed)); each
/// shard advances in its own thread in lockstep. `fresh` seeds each shard's
/// f0 from the very first revision (a new store) and runs session-end
/// compaction at the end (a fresh encode is already O(history); an update
/// stays amortized — `flush`'s ratio-threshold eviction — keeping its I/O
/// O(new), not O(history)).
#[allow(clippy::too_many_arguments)]
fn run_shards(
    depot: &Depot,
    encs: Vec<Encoder>,
    bits: u32,
    src: &ObjSource,
    objs: &mut dyn crate::oidenc::Objects,
    plan: &LanePlan,
    tree_of: &HashMap<String, String>,
    sha_of: &[String],
    range: std::ops::Range<usize>,
    st: &mut RunState,
    batch_bound: u64,
    level: i32,
    fresh: bool,
) -> Result<(Vec<Encoder>, usize)> {
    use crate::oidenc::{route_trans, Ent, Trans};
    use std::sync::Arc;
    let n = encs.len();
    let per_shard_bound = (batch_bound / n as u64).max(1);

    std::thread::scope(|scope| -> Result<(Vec<Encoder>, usize)> {
        let mut txs = Vec::with_capacity(n);
        let mut handles = Vec::with_capacity(n);
        for (s, enc) in encs.into_iter().enumerate() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<RevWork>(64);
            handles.push(scope.spawn(move || {
                shard_worker(s as u64, enc, src, depot, rx, per_shard_bound, level)
            }));
            txs.push(tx);
        }
        // A send fails only when a worker died — fall through and surface
        // its join error instead of the disconnect.
        let mut send_broke = false;
        'revs: for i in range {
            let sha = &sha_of[i];
            let tree_oid =
                tree_of.get(sha).ok_or_else(|| cf(format!("no tree for {sha}")))?.clone();
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

            let prev_live = Arc::new(st.live.clone());
            for &d in &plan.dying_at[i] {
                clear_live_bit(&mut st.live, d as usize);
            }
            set_live_bit(&mut st.live, l);
            let new_live = Arc::new(st.live.clone());

            let routed = route_trans(&trans, objs, bits)?;
            for (s, pt) in routed.into_iter().enumerate() {
                let w = RevWork {
                    pt,
                    prev_live: prev_live.clone(),
                    new_live: new_live.clone(),
                    seed_f0: fresh && i == 0,
                };
                if txs[s].send(w).is_err() {
                    send_broke = true;
                    break 'revs;
                }
            }
            for &d in &plan.dying_at[i] {
                st.lane_tree[d as usize] = None;
            }
            st.lane_tree[l] = Some(tree_oid);
        }
        drop(txs);
        let mut out = Vec::with_capacity(n);
        let mut reads = 0usize;
        let mut first_err: Option<Error> = None;
        for h in handles {
            match h.join().map_err(|_| cf("shard worker panicked".into()))? {
                Ok((enc, r)) => {
                    out.push(enc);
                    reads += r;
                }
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        if send_broke {
            return Err(cf("shard worker exited early".into()));
        }
        if fresh {
            depot.collect().map_err(|e| cf(e.to_string()))?;
        } else {
            depot.flush().map_err(|e| cf(e.to_string()))?;
        }
        Ok((out, reads))
    })
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
/// The revision that is the tip of each live lane at the boundary
/// (`old_n`), by replaying the per-revision lane life/death — the same
/// logic `reconstruct_boundary` uses. Pure integer work, O(old_n).
fn boundary_tip_revs(plan: &LanePlan, old_n: usize) -> Vec<usize> {
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
    (0..plan.width).filter(|&c| alive[c]).map(|c| tip_rev[c]).collect()
}

fn reconstruct_boundary(
    depot: &Depot,
    shard_bits: u32,
    plan: &LanePlan,
    tree_of: &HashMap<String, String>,
    sha_of: &[String],
    old_n: usize,
    objs: &mut Cat,
) -> Result<(Vec<Encoder>, Vec<Option<String>>, Vec<u8>)> {
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

    let mut encs = Vec::with_capacity(1usize << shard_bits);
    for shard in 0..(1u64 << shard_bits) {
        let f0 = match depot.read_f0(shard) {
            Ok(frame) => decompress_frame(&frame, None).map_err(cf)?,
            Err(e) => return Err(cf(format!("no boundary f0 for shard {shard}: {e}"))),
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
        .map_err(|e| cf(format!("decode boundary f0 (shard {shard}): {e:?}")))?;

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
        encs.push(Encoder::seed(variants));
    }
    Ok((encs, lane_tree, live))
}


/// A built lane store: a private `wikimak_depot` instance on disk (the
/// combined-state chain) plus the RAM bookkeeping a reader needs to map
/// a commit to its revision index and lane.
pub struct LaneStore {
    depot: Depot,
    /// §9 `shard-bits`: the union is split across `2^shard_bits` chains
    /// (chain id = shard) by a stable hash of each full path. Fixed at
    /// store creation; persisted in meta.
    shard_bits: u32,
    /// Number of revisions — a revision is a ref-tree EVENT: almost always
    /// a commit, occasionally a tag-at-tree's tagged tree (a lane that is
    /// born once and never advances — the union just holds one more tree).
    n_rev: usize,
    /// `lane_of[i]` — lane advanced at revision `i`.
    lane_of: Vec<LaneId>,
    /// `sha_of[i]` — the commit sha at revision `i`; for a tag-tree
    /// revision, the TREE oid itself (uniform for nested tag chains: every
    /// tag peeling to that tree shares the one revision).
    sha_of: Vec<String>,
    /// `tag_rev[i]` — revision `i` is a tag-tree event, not a commit.
    tag_rev: Vec<bool>,
    /// Commit sha (or tag-tree oid) → revision index.
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
        // Topology only (cheap): revision order + lane assignment. Tag-at-
        // tree refs append one revision per distinct tagged TREE after the
        // commits — a parentless root lane that never advances (§8: every
        // live ref occupies a lane; the union just holds one more tree).
        let dag = crate::dag_scope(repo, &[])?;
        let mut sha_of = dag.order.clone();
        let tag_trees = collect_tree_tags(repo)?;
        let mut seen: std::collections::HashSet<&str> =
            sha_of.iter().map(|s| s.as_str()).collect();
        let mut tag_revs: Vec<String> = Vec::new();
        for (_name, _tag_sha, tree) in &tag_trees {
            if seen.insert(tree.as_str()) {
                tag_revs.push(tree.clone());
            }
        }
        sha_of.extend(tag_revs.iter().cloned());
        let n_commits = dag.order.len();
        let tag_rev: Vec<bool> = (0..sha_of.len()).map(|i| i >= n_commits).collect();
        let (plan, parents) = plan_lanes(repo, &sha_of)?;
        let mut tree_of = tree_map(repo)?;
        for t in &tag_revs {
            tree_of.insert(t.clone(), t.clone()); // a tag-tree rev IS its tree
        }

        let shard_bits = shard_bits_param();
        let n_shards = 1u64 << shard_bits;
        let depot = open_depot(dir, n_shards)?;
        let src = ObjSource::Git(repo.to_path_buf());
        let mut objs = Cat::new(repo)?; // one persistent tree cache
        let encs: Vec<Encoder> = (0..n_shards).map(|_| Encoder::new()).collect();
        let mut st = RunState { lane_tree: vec![None; plan.width], live: Vec::new() };
        let mut reads = 0usize;
        if plan.n_rev > 0 {
            let (_encs, worker_reads) = run_shards(
                &depot, encs, shard_bits, &src, &mut objs, &plan, &tree_of, &sha_of,
                0..plan.n_rev, &mut st, batch_ram_bound(), level, true,
            )?;
            reads = objs.reads + worker_reads;
        }

        let sha_to_rev: HashMap<String, usize> =
            sha_of.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
        let refs = live_refs(repo, &sha_to_rev)?;
        persist_meta(dir, plan.n_rev, &plan.lane_of, &sha_of, &tag_rev, &parents, &refs, shard_bits)?;
        Ok((LaneStore { depot, shard_bits, n_rev: plan.n_rev, lane_of: plan.lane_of, sha_of, tag_rev, sha_to_rev, refs }, reads))
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
        let old_tag = existing.tag_rev.clone();
        let old_lane = existing.lane_of.clone();
        // Stored prefix parent edges — the repo may be SHALLOW (bootstrap
        // re-pin), so the old commits' topology must come from meta, not a
        // repo rev-list that would see them as parentless roots.
        let (_, _, _, _, old_parents, _, _) = load_meta(dir)?;
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
        let mut old_set: std::collections::HashSet<String> =
            old_sha.iter().cloned().collect();
        let mut order = old_sha.clone();
        let mut tag_rev = old_tag.clone();
        for c in &dag.order {
            if old_set.insert(c.clone()) {
                order.push(c.clone());
                tag_rev.push(false);
            }
        }
        // NEW tag-at-tree refs append after the new commits, same rule as
        // import; stored tag revisions ride in the frozen prefix.
        let mut new_tag_trees: Vec<String> = Vec::new();
        for (_name, _tag_sha, tree) in collect_tree_tags(repo)? {
            if old_set.insert(tree.clone()) {
                order.push(tree.clone());
                tag_rev.push(true);
                new_tag_trees.push(tree);
            }
        }
        if order.len() == old_n {
            // Nothing new — just refresh refs and return.
            let sha_to_rev = existing.sha_to_rev.clone();
            let refs = live_refs(repo, &sha_to_rev)?;
            persist_meta(dir, old_n, &old_lane, &old_sha, &old_tag, &old_parents, &refs, existing.shard_bits)?;
            return Ok((LaneStore { refs, ..existing }, 0, 0));
        }

        // Parent edges for the full order: the new suffix from the repo
        // (present, freshly fetched), the stored prefix from meta. This
        // freezes the prefix regardless of a shallow repo.
        let mut parents = compute_parents(repo, &order)?;
        parents[..old_n].clone_from_slice(&old_parents[..old_n]);
        let plan = plan_from_parents(&parents);
        // §8 stability: every stored commit keeps its compact lane. With
        // the frozen prefix parents this holds by construction; the check
        // guards against a non-fast-forward that reorders the prefix.
        if plan.lane_of[..old_n] != old_lane[..] {
            return Err(cf("incremental update would renumber a stored lane \
                (non-fast-forward history rewrite is not supported here)".into()));
        }
        let mut tree_of = tree_map(repo)?;
        for (i, sha) in order.iter().enumerate() {
            if tag_rev[i] {
                tree_of.insert(sha.clone(), sha.clone()); // tag-tree rev IS its tree
            }
        }
        // A live boundary tip may be an OLD commit that a history rewrite
        // (amend) made unreachable in the buffer, so `git log` over the
        // repo omits it. Its tree oid is still reconstructable from the
        // stored union — backfill those before consuming the depot.
        for r in boundary_tip_revs(&plan, old_n) {
            let sha = &old_sha[r];
            if !tree_of.contains_key(sha) {
                tree_of.insert(sha.clone(), existing.tree_oid_at(r)?);
            }
        }

        let depot = existing.depot;
        let shard_bits = existing.shard_bits;
        let src = ObjSource::Git(repo.to_path_buf());
        let mut objs = Cat::new(repo)?;
        // Reconstruct each shard's encoder at the boundary from its stored
        // f0 + the boundary lane trees.
        let (encs, lane_tree, live) =
            reconstruct_boundary(&depot, shard_bits, &plan, &tree_of, &old_sha, old_n, &mut objs)?;
        let mut st = RunState { lane_tree, live };
        // Advance ONLY the new revisions (O(new)); prepend their reverse deltas.
        let (_encs, worker_reads) = run_shards(
            &depot, encs, shard_bits, &src, &mut objs, &plan, &tree_of, &order,
            old_n..plan.n_rev, &mut st, batch_ram_bound(), level, false,
        )?;
        let reads = objs.reads + worker_reads;
        let new_revs = plan.n_rev - old_n;

        let sha_to_rev: HashMap<String, usize> =
            order.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
        let refs = live_refs(repo, &sha_to_rev)?;
        persist_meta(dir, plan.n_rev, &plan.lane_of, &order, &tag_rev, &parents, &refs, shard_bits)?;
        Ok((LaneStore { depot, shard_bits, n_rev: plan.n_rev, lane_of: plan.lane_of, sha_of: order, tag_rev, sha_to_rev, refs }, new_revs, reads))
    }

    /// Encode ONE self-contained received pack straight into a union store
    /// — no git repo anywhere in the path (the direct-from-wire pipeline):
    /// ids were hashed by the pack scan, trees and topology come from the
    /// graph (listings + parent pointers), blob bytes from the transient
    /// pack file, and `refs` is the fetch dialogue's `(name, sha)`
    /// advertisement. Returns the store and the object-fetch count.
    pub fn encode_pack_union_stats(
        pack: &Path,
        refs: &[(String, String)],
        dir: &Path,
        level: i32,
    ) -> Result<(LaneStore, usize)> {
        // The refs' oid width names the repo's hash format (§: the fetch
        // dialogue's object-format capability, carried by the ref shas).
        let kind = refs
            .first()
            .and_then(|(_, sha)| crate::HashKind::of_hex_len(sha.len()))
            .ok_or_else(|| cf("pack-union: no refs (or malformed shas) to infer hash format".into()))?;
        let pg = std::sync::Arc::new(crate::memgraph::build_pack(pack, kind)?);
        let g = &pg.graph;

        // Resolve each ref through tag chains: a commit binds the ref, a
        // tree is a §8 tag-at-tree revision, anything else is refused
        // loudly (same rule as the git-side import).
        let mut ref_commits: Vec<(String, u32)> = Vec::new();
        let mut tree_tags: Vec<(String, String)> = Vec::new(); // (name, tree oid)
        for (name, sha) in refs {
            let mut slot = g
                .slot_of_hex(sha)
                .ok_or_else(|| cf(format!("ref {name}: {sha} not in pack")))?;
            let mut hops = 0;
            while g.is_tag(slot) {
                slot = g.tag_target(slot);
                hops += 1;
                if hops > 32 {
                    return Err(cf(format!("ref {name}: tag chain too deep")));
                }
            }
            if g.is_commit(slot) {
                ref_commits.push((name.clone(), slot));
            } else if g.is_tree(slot) {
                tree_tags.push((name.clone(), g.sha_hex(slot)));
            } else {
                return Err(cf(format!("ref {name} peels to neither commit nor tree")));
            }
        }

        // Reachable commits, parents-first, deterministic: Kahn's ordering
        // with the ready set keyed by sha. Out-of-pack parents (shallow
        // cuts) drop, same as the rev-list path.
        let mut indeg: HashMap<u32, usize> = HashMap::new();
        let mut stack: Vec<u32> = ref_commits.iter().map(|(_, s)| *s).collect();
        while let Some(s) = stack.pop() {
            if indeg.contains_key(&s) {
                continue;
            }
            let (_, ps) = g.commit_parts(s);
            let in_pack: Vec<u32> = ps.iter().copied().filter(|&p| g.is_commit(p)).collect();
            indeg.insert(s, in_pack.len());
            stack.extend(in_pack);
        }
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for &s in indeg.keys() {
            let (_, ps) = g.commit_parts(s);
            for &p in ps.iter().filter(|&&p| g.is_commit(p)) {
                children.entry(p).or_default().push(s);
            }
        }
        let total = indeg.len();
        let mut ready: std::collections::BTreeSet<(String, u32)> = indeg
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&s, _)| (g.sha_hex(s), s))
            .collect();
        let mut order_slots: Vec<u32> = Vec::with_capacity(total);
        while let Some((_, s)) = ready.pop_first() {
            order_slots.push(s);
            if let Some(kids) = children.get(&s) {
                for &k in kids {
                    let d = indeg.get_mut(&k).expect("child in scope");
                    *d -= 1;
                    if *d == 0 {
                        ready.insert((g.sha_hex(k), k));
                    }
                }
            }
        }
        if order_slots.len() != total {
            return Err(cf("pack: commit graph has a cycle (corrupt)".into()));
        }

        let mut sha_of: Vec<String> = order_slots.iter().map(|&s| g.sha_hex(s)).collect();
        let idx_of: HashMap<String, usize> =
            sha_of.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
        let mut parents: Vec<Vec<usize>> = order_slots
            .iter()
            .map(|&s| {
                let (_, ps) = g.commit_parts(s);
                ps.iter()
                    .filter(|&&p| g.is_commit(p))
                    .map(|&p| idx_of[&g.sha_hex(p)])
                    .collect()
            })
            .collect();
        let mut tree_of: HashMap<String, String> = order_slots
            .iter()
            .map(|&s| (g.sha_hex(s), g.sha_hex(g.commit_parts(s).0)))
            .collect();

        // Tag-at-tree revisions append after the commits, one per distinct
        // tagged tree — parentless root lanes (§8), exactly the repo path.
        let n_commits = sha_of.len();
        let mut seen_sha: std::collections::HashSet<String> = sha_of.iter().cloned().collect();
        for (_name, tree) in &tree_tags {
            if seen_sha.insert(tree.clone()) {
                sha_of.push(tree.clone());
                parents.push(Vec::new());
                tree_of.insert(tree.clone(), tree.clone());
            }
        }
        let tag_rev: Vec<bool> = (0..sha_of.len()).map(|i| i >= n_commits).collect();

        let plan = plan_from_parents(&parents);
        let shard_bits = shard_bits_param();
        let n_shards = 1u64 << shard_bits;
        let depot = open_depot(dir, n_shards)?;
        let src = ObjSource::Pack(pg.clone());
        let mut objs = crate::memgraph::GraphObjects::new(pg.clone())?;
        let encs: Vec<Encoder> = (0..n_shards).map(|_| Encoder::new()).collect();
        let mut st = RunState { lane_tree: vec![None; plan.width], live: Vec::new() };
        let mut reads = 0usize;
        if plan.n_rev > 0 {
            let (_encs, worker_reads) = run_shards(
                &depot, encs, shard_bits, &src, &mut objs, &plan, &tree_of, &sha_of,
                0..plan.n_rev, &mut st, batch_ram_bound(), level, true,
            )?;
            reads = crate::oidenc::Objects::reads(&objs) + worker_reads;
        }

        let sha_to_rev: HashMap<String, usize> =
            sha_of.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
        let mut live: HashMap<String, String> = ref_commits
            .iter()
            .map(|(name, slot)| (name.clone(), g.sha_hex(*slot)))
            .collect();
        for (name, tree) in &tree_tags {
            live.insert(name.clone(), tree.clone());
        }
        live.retain(|_, sha| sha_to_rev.contains_key(sha));
        persist_meta(dir, plan.n_rev, &plan.lane_of, &sha_of, &tag_rev, &parents, &live, shard_bits)?;
        Ok((
            LaneStore {
                depot,
                shard_bits,
                n_rev: plan.n_rev,
                lane_of: plan.lane_of,
                sha_of,
                tag_rev,
                sha_to_rev,
                refs: live,
            },
            reads,
        ))
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
        for shard in 0..self.n_shards() as u64 {
            for cold in self.depot.cold_iter(shard).map_err(|e| cf(e.to_string()))? {
                cold.map_err(|e| cf(e.to_string()))?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// The commit shas that are live lane tips at the stored boundary —
    /// the trees an incremental [`update`](Self::update) reconstructs from
    /// (§6). A caller driving update against a possibly-shallow buffer
    /// must ensure these trees are materialized there first: a lane can be
    /// live at the boundary yet its tip commit be unreachable from the
    /// current refs (a deleted branch), so `git` alone can't supply it.
    pub fn boundary_tip_shas(dir: &Path) -> Result<Vec<String>> {
        let (n_rev, _lane_of, sha_of, tag_rev, parents, _refs, _bits) = load_meta(dir)?;
        if n_rev == 0 {
            return Ok(Vec::new());
        }
        let plan = plan_from_parents(&parents);
        // Tag-tree revisions are excluded: their tree rides the tag
        // object's closure through the stub re-pin, not a commit want.
        Ok(boundary_tip_revs(&plan, n_rev)
            .into_iter()
            .filter(|&r| !tag_rev[r])
            .map(|r| sha_of[r].clone())
            .collect())
    }

    pub fn open(dir: &Path) -> Result<LaneStore> {
        let (n_rev, lane_of, sha_of, tag_rev, _parents, refs, shard_bits) = load_meta(dir)?;
        let depot = open_depot(dir, 1u64 << shard_bits)?;
        let sha_to_rev = sha_of
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i))
            .collect();
        Ok(LaneStore { depot, shard_bits, n_rev, lane_of, sha_of, tag_rev, sha_to_rev, refs })
    }

    /// Number of §9 shards (union chains) in this store.
    pub fn n_shards(&self) -> usize {
        1usize << self.shard_bits
    }

    /// The store's object-id format, inferred from its stored oid widths
    /// (never persisted — the width IS the format).
    pub fn hash_kind(&self) -> crate::HashKind {
        self.sha_of
            .first()
            .and_then(|s| crate::HashKind::of_hex_len(s.len()))
            .unwrap_or(crate::HashKind::Sha1)
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

    /// Revision `rev` is a tag-tree event (its `sha_at` is a TREE oid), not
    /// a commit.
    pub fn is_tag_rev(&self, rev: usize) -> bool {
        self.tag_rev.get(rev).copied().unwrap_or(false)
    }

    /// One §9 shard's canonical §2 union bytes at revision `rev` — that
    /// shard's chain walked newest-first down to it, folded at the byte
    /// level: records compose delta ∘ delta on a geometric stack and ONE
    /// overlay lands them on the f0 base ([`walk_records_state`]). No View
    /// is built; memory is the base bytes plus the geometric stack.
    fn shard_bytes_at(&self, shard: u64, rev: usize) -> Result<Vec<u8>> {
        if rev >= self.n_rev {
            return Err(Error::Chain(format!("no revision {rev}")));
        }
        let target_pos = self.n_rev - 1 - rev; // newest-first position
        self.walk_records_state(shard, &mut |pos, _| Ok(pos == target_pos))?
            .ok_or_else(|| Error::Chain(format!("shard {shard} fell short of revision {rev}")))
    }

    /// Every shard's union bytes at revision `rev` (§9: the union IS the
    /// gather across shards).
    fn all_shard_bytes_at(&self, rev: usize) -> Result<Vec<Vec<u8>>> {
        (0..self.n_shards() as u64).map(|s| self.shard_bytes_at(s, rev)).collect()
    }

    /// Lane `lane`'s flat `(path, mode, content)` entries at revision `rev`,
    /// gathered across every shard (§9 — the split is invisible in the
    /// resulting tree oid) via the authoritative `layer` extractor.
    pub fn lane_entries_at(&self, rev: usize) -> Result<Vec<(Vec<u8>, crate::layer::Mode, Vec<u8>)>> {
        let mut out = Vec::new();
        for bytes in self.all_shard_bytes_at(rev)? {
            out.extend(
                crate::layer::extract_lane_entries(&bytes, self.lane_of[rev] as u32)
                    .map_err(|e| cf(format!("{e:?}")))?,
            );
        }
        Ok(out)
    }

    /// Stream ONE commit's tree (or the subtree under `subpath`) as flat
    /// `(path, mode, content)` entries in canonical container order — the
    /// checkout primitive. Bounded memory: the union at the commit's revision
    /// is folded once ([`union_bytes_at`]) and walked as bytes; each entry's
    /// content is a slice into that buffer, never retained. `subpath` empty =
    /// the whole tree; otherwise entries under it, paths relative to it.
    pub fn checkout_entries(
        &self,
        sha: &str,
        subpath: &[u8],
        visit: &mut dyn FnMut(&[u8], crate::layer::Mode, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let rev =
            self.rev_of(sha).ok_or_else(|| Error::Chain(format!("commit {sha} not in store")))?;
        self.checkout_entries_at(rev, subpath, visit)
    }

    /// [`checkout_entries`] addressed by stable revision index instead of a
    /// commit sha — the entry point for a tag-at-tree ref, whose pin is the
    /// tag object's sha but whose tree is a revision's.
    pub fn checkout_entries_at(
        &self,
        rev: usize,
        subpath: &[u8],
        visit: &mut dyn FnMut(&[u8], crate::layer::Mode, &[u8]) -> Result<()>,
    ) -> Result<()> {
        use crate::layer::{container_path_cmp, EntryCursor};
        let lane = self.lane_of[rev] as u32;
        let shard_bytes = self.all_shard_bytes_at(rev)?;
        // K-way lockstep over the shards' canonical streams — the merged
        // visit order is exactly the unsharded container order. Content
        // stays borrowed from each shard's folded buffer.
        let mut curs: Vec<EntryCursor<'_>> = shard_bytes
            .iter()
            .map(|b| EntryCursor::new(b))
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| cf(format!("{e:?}")))?;
        let mut heads = Vec::with_capacity(curs.len());
        for c in &mut curs {
            heads.push(c.next().map_err(|e| cf(format!("{e:?}")))?);
        }
        loop {
            let mut min: Option<usize> = None;
            for (k, h) in heads.iter().enumerate() {
                let Some(h) = h else { continue };
                min = match min {
                    None => Some(k),
                    Some(m) => {
                        let mh = heads[m].as_ref().expect("min head present");
                        if container_path_cmp(&h.path, h.slot, &mh.path, mh.slot).is_lt() {
                            Some(k)
                        } else {
                            Some(m)
                        }
                    }
                };
            }
            let Some(k) = min else { break };
            let e = heads[k].take().expect("min head present");
            heads[k] = curs[k].next().map_err(|ie| cf(format!("{ie:?}")))?;
            let inl = match e.bitmap {
                Some(b) => (b.get((lane / 8) as usize).copied().unwrap_or(0) & (1 << (lane % 8))) != 0,
                None => true,
            };
            if !inl {
                continue;
            }
            let rel: &[u8] = if subpath.is_empty() {
                &e.path
            } else if e.path == subpath {
                // The subpath names a file: serve it under its own name.
                match e.path.rsplit(|&b| b == b'/').next() {
                    Some(base) => base,
                    None => &e.path,
                }
            } else if e.path.len() > subpath.len()
                && e.path.starts_with(subpath)
                && e.path[subpath.len()] == b'/'
            {
                &e.path[subpath.len() + 1..]
            } else {
                continue; // outside the requested subtree
            };
            visit(rel, e.mode, e.content)?;
        }
        Ok(())
    }

    /// The git tree oid of the commit at revision `rev` (reconstructed) — the
    /// SHA-exact ground truth from the stored §2 union bytes.
    pub fn tree_oid_at(&self, rev: usize) -> Result<String> {
        crate::layer::tree_oid_of_entries(&self.lane_entries_at(rev)?, self.hash_kind())
            .map_err(|e| cf(format!("{e:?}")))
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
        for shard in 0..self.n_shards() as u64 {
            self.walk_records(shard, &mut |_pos, rec| {
                total += rec.len() as u64;
                Ok(false)
            })?;
        }
        Ok(total)
    }

    pub fn lane_of(&self, rev: usize) -> LaneId {
        self.lane_of[rev]
    }

    // ------------------------------------------------------- internals

    /// Walk the combined-state chain newest-first, handing each stored
    /// record (raw codec bytes) to `visit(pos, rec)`; `visit` returns true
    /// to stop. f0 is position 0; then the f1 records and every cold frame's
    /// records in stored order.
    fn walk_records(
        &self,
        shard: u64,
        visit: &mut dyn FnMut(usize, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        self.walk_records_state(shard, visit).map(|_| ())
    }

    /// [`walk_records`], maintaining the working full-state at the BYTE
    /// level as a base (`refPrefix` bytes, seeded by the f0 record) plus a
    /// geometric delta stack — the read-side twin of the encoder's §5 state
    /// and the asymptotically right "sum of deltas": each reverse record
    /// composes delta ∘ delta on the stack (cost ∝ delta bytes, log depth,
    /// holes survive), and the full state is touched only where it is
    /// actually needed — a cold frame's boundary anchor (where the fold also
    /// reseeds the base, exactly a §5 seal) and the caller's stop point —
    /// never once per record. Returns the folded full-state bytes at the
    /// stop point (`None` if the walk ran out without `visit` stopping).
    /// A cold frame's zstd refPrefix IS that boundary fold, so any
    /// non-canonical byte in it fails the decompression loudly.
    fn walk_records_state(
        &self,
        shard: u64,
        visit: &mut dyn FnMut(usize, &[u8]) -> Result<bool>,
    ) -> Result<Option<Vec<u8>>> {
        let head = match self.depot.read_f0(shard) {
            Ok(frame) => decompress_frame(&frame, None).map_err(cf)?,
            Err(wikimak_depot::Error::NoFrame) => return Ok(None),
            Err(e) => return Err(cf(e.to_string())),
        };
        let mut base = head.clone();
        let mut stack: crate::geostack::GeoStack<Vec<u8>> = crate::geostack::GeoStack::new();
        let mut pos = 0usize;
        if visit(pos, &head)? {
            return Ok(Some(base));
        }
        if let Some(f1) = self.depot.read_f1(shard).map_err(|e| cf(e.to_string()))? {
            let mut stopped = false;
            stream_f1_records(&f1, &head, &mut |rec| {
                pos += 1;
                stack.push(rec.to_vec(), |l| l.len() as u64, compose_bytes);
                stopped = visit(pos, rec)?;
                Ok(stopped)
            })?;
            if stopped {
                return Ok(Some(fold_state(&base, std::mem::take(&mut stack))));
            }
        }
        for cold in self.depot.cold_iter(shard).map_err(|e| cf(e.to_string()))? {
            let frame = cold.map_err(|e| cf(e.to_string()))?;
            // The boundary anchor is the full state here; folding it also
            // reseeds the base (a §5 seal on the read side).
            base = fold_state(&base, std::mem::take(&mut stack));
            let mut stopped = false;
            stream_f1_records(&frame, &base, &mut |rec| {
                pos += 1;
                stack.push(rec.to_vec(), |l| l.len() as u64, compose_bytes);
                stopped = visit(pos, rec)?;
                Ok(stopped)
            })?;
            if stopped {
                return Ok(Some(fold_state(&base, std::mem::take(&mut stack))));
            }
        }
        Ok(None)
    }
}

/// delta ∘ delta at the byte level (§4 compose — holes survive).
fn compose_bytes(lower: Vec<u8>, upper: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    depot::stream::compose_stream(&lower, &upper, &mut out)
        .expect("compose_stream on canonical records");
    out
}

/// The full-state bytes of base + stack: collapse the geometric stack to one
/// combined delta and overlay it onto the base ONCE (holes dissolve to
/// removals). An empty stack is the base itself.
fn fold_state(base: &[u8], stack: crate::geostack::GeoStack<Vec<u8>>) -> Vec<u8> {
    match stack.collapse(compose_bytes) {
        None => base.to_vec(),
        Some(combined) => {
            let mut out = Vec::new();
            depot::stream::overlay_full(base, &combined, &mut out)
                .expect("overlay_full on canonical records");
            if out.is_empty() {
                // The union root always exists (§2: an empty node is a real
                // object). A delta that empties the whole state resolves to
                // the canonical empty full-state — exactly the writer's
                // full() bytes for an empty revision, so anchors stay
                // bit-exact.
                out = codec::encode(&depot::diff(None, Some(&View::default())));
            }
            out
        }
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
    // A tag-at-tree ref binds to its tagged TREE's revision (the tag-tree
    // event keyed by the tree oid).
    for (name, _tag_sha, tree) in collect_tree_tags(repo)? {
        if sha_to_rev.contains_key(&tree) {
            refs.insert(name, tree);
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

/// `(refname, outer tag sha, fully-peeled TREE oid)` for every tag ref
/// peeling to a TREE — the tag-at-tree shape (linux v2.6.11-tree). `%(*…)`
/// peels one level; a nested tag chain is finished with `rev-parse ^{}`.
fn collect_tree_tags(repo: &Path) -> Result<Vec<(String, String, String)>> {
    let out = crate::git_str(
        repo,
        &[
            "for-each-ref",
            "--format=%(objectname) %(objecttype) %(*objectname) %(*objecttype) %(refname)",
            "refs/tags",
        ],
    )?;
    let mut v = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(5, ' ').collect();
        if f.len() < 5 || f[1] != "tag" {
            continue;
        }
        let (mut peeled, mut ptyp) = (f[2].to_string(), f[3].to_string());
        if ptyp == "tag" {
            peeled = crate::git_str(repo, &["rev-parse", &format!("{}^{{}}", f[4])])?
                .trim()
                .to_string();
            ptyp = crate::git_str(repo, &["cat-file", "-t", &peeled])?.trim().to_string();
        }
        if ptyp == "tree" {
            v.push((f[4].to_string(), f[0].to_string(), peeled));
        }
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
CREATE TABLE IF NOT EXISTS revs(rev INTEGER PRIMARY KEY, sha TEXT NOT NULL, lane INTEGER NOT NULL, parents TEXT NOT NULL DEFAULT '', kind INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS refs(name TEXT PRIMARY KEY, sha TEXT NOT NULL, rev INTEGER NOT NULL, lane INTEGER NOT NULL);
";
const META_SCHEMA_VERSION: &str = "4";

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
    tag_rev: &[bool],
    parents: &[Vec<usize>],
    refs: &HashMap<String, String>,
    shard_bits: u32,
) -> Result<()> {
    let mut conn = rusqlite::Connection::open(meta_path(dir)).map_err(map_sql)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;").map_err(map_sql)?;
    conn.execute_batch(META_SCHEMA).map_err(map_sql)?;
    let tx = conn.transaction().map_err(map_sql)?;
    tx.execute("DELETE FROM revs", []).map_err(map_sql)?;
    tx.execute("DELETE FROM refs", []).map_err(map_sql)?;
    tx.execute(
        "INSERT OR REPLACE INTO kv(key,value) VALUES('schema',?1),('n_rev',?2),('shard_bits',?3)",
        rusqlite::params![META_SCHEMA_VERSION, n_rev.to_string(), shard_bits.to_string()],
    )
    .map_err(map_sql)?;
    {
        let mut ins = tx
            .prepare("INSERT INTO revs(rev,sha,lane,parents,kind) VALUES(?1,?2,?3,?4,?5)")
            .map_err(map_sql)?;
        for (i, sha) in sha_of.iter().enumerate() {
            let ps = parents[i].iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
            let kind = if tag_rev.get(i).copied().unwrap_or(false) { 1i64 } else { 0 };
            ins.execute(rusqlite::params![i as i64, sha, lane_of[i] as i64, ps, kind])
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
#[allow(clippy::type_complexity)]
fn load_meta(
    dir: &Path,
) -> Result<(usize, Vec<LaneId>, Vec<String>, Vec<bool>, Vec<Vec<usize>>, HashMap<String, String>, u32)>
{
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
    let mut tag_rev = vec![false; n_rev];
    let mut parents = vec![Vec::new(); n_rev];
    {
        let mut q = conn.prepare("SELECT rev,sha,lane,parents,kind FROM revs").map_err(map_sql)?;
        let rows = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(map_sql)?;
        for row in rows {
            let (rev, sha, lane, ps, kind) = row.map_err(map_sql)?;
            let rev = rev as usize;
            if rev >= n_rev {
                return Err(Error::Chain(format!("meta: rev {rev} out of range")));
            }
            sha_of[rev] = sha;
            lane_of[rev] = lane as LaneId;
            tag_rev[rev] = kind != 0;
            parents[rev] = if ps.is_empty() {
                Vec::new()
            } else {
                ps.split(',').map(|p| p.parse::<usize>().unwrap_or(0)).collect()
            };
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
    let shard_bits: u32 = conn
        .query_row("SELECT value FROM kv WHERE key='shard_bits'", [], |r| {
            let v: String = r.get(0)?;
            Ok(v.parse().unwrap_or(0))
        })
        .unwrap_or(0);
    Ok((n_rev, lane_of, sha_of, tag_rev, parents, refs, shard_bits))
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
fn seal_prepend(depot: &Depot, chain: u64, head_record: &[u8], staged: &[Vec<u8>], level: i32) -> Result<()> {
    let old_f1 = depot.read_f1(chain).map_err(|e| cf(e.to_string()))?;
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
            let old_head = decompress_frame(&depot.read_f0(chain).map_err(|e| cf(e.to_string()))?, None)
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
        .prepend(chain, &new_f0, Some(&new_f1), seal_old)
        .map_err(|e| cf(e.to_string()))?;
    if total_raw > seal_threshold() {
        depot.seal_f1(chain).map_err(|e| cf(e.to_string()))?;
    }
    Ok(())
}

/// Open the combined-state depot with one chain per §9 shard (chain id =
/// shard index). The index is sized at open, so the store's shard count is
/// fixed at creation (an offline re-shard rebuilds the store).
fn open_depot(dir: &Path, n_shards: u64) -> Result<Depot> {
    std::fs::create_dir_all(dir.join("depot"))?;
    Depot::open(DepotConfig {
        root: dir.join("depot"),
        max_chain_id: n_shards.max(1),
        file_size_threshold: 4 << 20,
        eviction_dead_ratio: 0.5,
    })
    .map_err(|e| cf(e.to_string()))
}

/// The §9 `shard-bits` for a NEW store: the CLI/env parameter (clamped to 8
/// — 256 shards is already far past any thread-count win here). Existing
/// stores read their bits from meta, never from this.
fn shard_bits_param() -> u32 {
    std::env::var("GITDEPOT_SHARD_BITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(8)
}

