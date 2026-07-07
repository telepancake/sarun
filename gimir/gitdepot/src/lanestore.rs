//! The combined-state lockstep lane encoder (branch-lane model, first
//! correctness increment — see `gimir/notes/branch-lane-model.md` and
//! `branch-lane-buildspec.md`).
//!
//! This is a NEW store path that lives ALONGSIDE the shipped TREES
//! store (`store.rs`) and changes nothing in it. It proves the
//! lane-prefix + lockstep model round-trips SHA-exact. There is NO
//! variant-delta and NO base-switching here (later increments); every
//! lane is carried as its own subtree under a combined-state root, and
//! the revision axis is one deterministic lockstep chain of reverse
//! deltas — exactly the physical shape the TREES chain uses, through
//! the same `wikimak_depot` frame/codec plumbing.
//!
//! ## Model
//!
//! * **Lockstep revision axis.** Commits are processed in the append-only
//!   topological order [`crate::lanes::assign_lanes`] consumes (parents
//!   before children). Each commit is ONE revision step and advances
//!   EXACTLY ONE lane — its `lane_of[i]`.
//! * **Combined state** at revision `i` is a depot [`View`] whose
//!   top-level children are keyed by lane id (a 4-byte big-endian name,
//!   so ordering is stable). At revision `i` the child for lane
//!   `lane_of[i]` is replaced by commit `i`'s tree; every other live
//!   lane's child is carried UNCHANGED (structural sharing via the
//!   depot's `Arc` views — the lockstep empty delta is literally no
//!   work). A lane's child appears from its birth revision and is kept
//!   through the end (retirement is a later increment).
//! * **Chain record** at revision `i` is `diff(combined[i-1],
//!   combined[i])` — but stored, like TREES, as a REVERSE delta: the
//!   newest combined state is a full record (f0), every older record
//!   rebuilds `combined[k]` from `combined[k+1]`. Because only one lane's
//!   subtree changed between consecutive revisions, each such record
//!   touches exactly one lane prefix (`depot::diff`'s `Arc::ptr_eq` fast
//!   path prunes the rest) — lanes never oscillate.
//!
//! ## Reconstruction
//!
//! Walk the chain newest-first applying reverse deltas to rebuild
//! `combined[i]`, then extract child `lane_of[commit]` — that commit's
//! tree View. Its git tree oid ([`crate::view_tree_oid`]) equals the
//! real object.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use depot::{codec, View};
use wikimak_depot::{
    compress_frame, decompress_frame, Depot, DepotConfig, FrameDecoder, FrameEncoder,
};

use crate::lanes::{assign_lanes, cluster_variants, LaneId};
use crate::{Error, Result};

/// The single combined-state chain id in this store's private depot.
const CHAIN: u64 = 0;
/// Width, in bytes, of the big-endian lane-id child name.
const LANE_KEY_LEN: usize = 4;

/// The encode-time similarity cutoff — the SINGLE policy point for the
/// variant-delta path (see [`crate::lanes::cluster_variants`]). It is a
/// choice of the encoder, NOT part of the stored format: the frame
/// records only the resulting base-lane pointer, so changing this
/// constant changes future encodes, never how an existing store reads.
pub const VARIANT_CUTOFF: f64 = 0.5;

/// Attr key stamped on a lane child that is stored as a base-relative
/// variant delta rather than its own full subtree. Its value is the
/// 4-byte big-endian id of the BASE lane the delta is expressed against
/// (the base-in-effect at that revision, recorded AS DATA — the reader
/// consults it, it is never recomputed). A NUL lead byte cannot collide
/// with a git tree's own attr (`mode`) nor with any path segment. The
/// child's `blob` holds the encoded `diff(base_subtree, variant_subtree)`
/// layer. Recording the base per revision (it rides each stored combined
/// state) is exactly the hook a later base-switching increment needs: the
/// pointer can differ across revisions with no format change.
const VARIANT_MARK: &[u8] = b"\x00vbase";

fn lane_key(l: LaneId) -> Vec<u8> {
    l.to_be_bytes().to_vec()
}

fn decode_lane_key(key: &[u8]) -> Result<LaneId> {
    let a: [u8; LANE_KEY_LEN] =
        key.try_into().map_err(|_| Error::Chain("bad lane key width".into()))?;
    Ok(LaneId::from_be_bytes(a))
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

/// Rewrite a FULL combined state (every lane child its own full subtree)
/// into the STORED combined state that actually lands in the frame:
/// cluster the LIVE lanes at this revision ([`cluster_variants`]); each
/// group's BASE lane keeps its full subtree, and every VARIANT lane is
/// replaced by a marker child carrying `diff(base_subtree,
/// variant_subtree)` — so the frame-resident uncompressed content of a
/// group is one base subtree plus small per-variant deltas, not N full
/// trees. Independent lanes are singleton groups and stay full, exactly as
/// the non-variant path stores every lane. This is representation (a): the
/// delta lives IN the child slot behind a marker, self-describing,
/// consulted by the reader.
///
/// `live` is the set of lanes that are alive at THIS revision (birth ≤ i ≤
/// death from the topology). Clustering only the live set is exactly the
/// base-switching mechanism: when a group's base lane dies, it drops out of
/// `live`, so [`cluster_variants`] recomputes the group over the smaller
/// set and its own "lowest live id is the base" rule promotes a surviving
/// variant to base — stored FULL from that revision on, the other survivors
/// re-expressed against it. Lane ids are monotonic in birth order, so the
/// lowest live id only ever rises as lanes die: a base switch moves the
/// pointer strictly forward, never back, and never touches an earlier
/// frame (each revision's frame records its own base-in-effect). Only live
/// lanes are emitted; a dead lane is absent from this revision's stored
/// state (its states stay reconstructable at the earlier revisions it was
/// live — dying removes it from the live set, not from the chain).
fn build_stored(
    full: &View,
    live: &[LaneId],
    blob_sets: &HashMap<LaneId, HashSet<Vec<u8>>>,
    cutoff: f64,
) -> Result<View> {
    let groups = cluster_variants(live, blob_sets, cutoff);
    let mut stored = View::default();
    for g in &groups {
        let base_key = lane_key(g.base);
        let base_sub = full
            .children
            .get(&base_key)
            .ok_or_else(|| Error::Chain(format!("base lane {} not live", g.base)))?;
        // Base: its own full subtree (Arc-shared, no copy).
        stored.children.insert(base_key, base_sub.clone());
        for &v in &g.variants {
            let v_sub = full
                .children
                .get(&lane_key(v))
                .ok_or_else(|| Error::Chain(format!("variant lane {v} not live")))?;
            let layer = depot::diff(Some(base_sub.as_ref()), Some(v_sub.as_ref()));
            let mut node = View::default();
            node.blob = Some(codec::encode(&layer).into());
            node.attrs.insert(VARIANT_MARK.to_vec(), g.base.to_be_bytes().to_vec());
            stored.children.insert(lane_key(v), Arc::new(node));
        }
    }
    Ok(stored)
}

/// Reconstruct lane `lane`'s FULL subtree from a reconstructed stored
/// combined state. The one canonical composition order: a variant child
/// is `apply(base_subtree, variant_delta)` with the base resolved FIRST,
/// then the variant delta on top. A non-variant child is already its full
/// subtree.
///
/// The base is read PER REVISION from the child's `VARIANT_MARK` attr (the
/// base-in-effect recorded as data in this revision's frame), never
/// recomputed — so base-switching needs no reader change. The recorded
/// base is guaranteed present (live) and stored full in the SAME combined
/// state, because the encoder only ever expresses a variant against a base
/// that is live at that revision: below a switch boundary the mark points
/// at the old base (still live and reconstructable there), above it at the
/// promoted lane. Reconstruction is therefore immutable across switches —
/// an earlier revision resolves against whatever base its own frame pins,
/// independent of any later history.
fn resolve_lane_subtree(combined: &View, lane: LaneId) -> Result<View> {
    let child = combined
        .children
        .get(&lane_key(lane))
        .ok_or_else(|| Error::Chain(format!("lane child missing (lane {lane})")))?;
    match child.attrs.get(VARIANT_MARK) {
        Some(base_bytes) => {
            let base = decode_lane_key(base_bytes)?;
            let base_sub = resolve_lane_subtree(combined, base)?;
            let bytes = child
                .blob
                .as_ref()
                .ok_or_else(|| Error::Chain("variant child without delta blob".into()))?;
            let layer = codec::decode(bytes)?;
            Ok(depot::apply(Some(&base_sub), &layer).unwrap_or_default())
        }
        None => Ok((**child).clone()),
    }
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

    /// Encode a git repo's trees into a combined-state lane store rooted
    /// at `dir` (created; its `depot/` subdir holds the chain). `level`
    /// is the zstd level. Reuses the fetch-side helpers (`ls-tree` +
    /// `cat-file` + `tree_layer`) to build each commit's full tree View
    /// and the depot frame codec to store the reverse-delta chain.
    pub fn encode_repo(repo: &Path, dir: &Path, level: i32) -> Result<LaneStore> {
        Self::encode_impl(repo, dir, level, None)
    }

    /// Like [`encode_repo`](Self::encode_repo) but stores near-identical
    /// live lanes as base-relative variant deltas (the variant/cross-lane
    /// axis composed with the temporal axis — "delta-of-delta"). Grouping,
    /// metric, cutoff and base pick are the encoder's policy (behind
    /// [`cluster_variants`] and [`VARIANT_CUTOFF`]); the format records only
    /// the outcome. Reconstruction is SHA-exact and the frame-resident
    /// content of a variant group is ~one base subtree + small deltas, not
    /// N full trees.
    pub fn encode_repo_variant(repo: &Path, dir: &Path, level: i32) -> Result<LaneStore> {
        Self::encode_impl(repo, dir, level, Some(VARIANT_CUTOFF))
    }

    fn encode_impl(
        repo: &Path,
        dir: &Path,
        level: i32,
        variant_cutoff: Option<f64>,
    ) -> Result<LaneStore> {
        // Revision order + per-commit parents come from the SAME
        // O(changes) discovery the shipped importer uses: one
        // `rev-list --parents` pass yields `dag.order` (walk_order — a
        // valid topological order, parents before children, that keeps
        // first-parent segments contiguous) and the per-commit parent
        // indices. The lane axis rides that order.
        let dag = crate::dag_scope(repo, &[])?;
        let sha_of = dag.order.clone();
        let n_rev = sha_of.len();
        let mut index_of: HashMap<String, usize> = HashMap::with_capacity(n_rev);
        for (i, s) in sha_of.iter().enumerate() {
            index_of.insert(s.clone(), i);
        }
        let parents = parents_by_index(repo, &index_of)?;
        let assignment = assign_lanes(&parents);
        let lane_of = assignment.lane_of.clone();
        // Per-lane liveness window `[birth, death)` in revision indices.
        // Birth is the lane's first commit (`span.0`). Death is the model's
        // metro-lane death — a MERGE: the revision of the merge commit that
        // absorbs the lane as a NON-first parent. A lane never merged stays
        // live to the end (it is a live branch tip; retirement is a later
        // increment), so its death is `n_rev`. This — not "the lane has no
        // more commits" — is the base-switching trigger: a base lane that
        // merges away drops out of the live set while its still-unmerged
        // variants remain, and clustering promotes a survivor.
        let birth: Vec<usize> = assignment.span.iter().map(|s| s.0).collect();
        let mut death: Vec<usize> = vec![n_rev; assignment.span.len()];
        for i in 0..n_rev {
            for &p in parents[i].iter().skip(1) {
                let l = lane_of[p] as usize;
                if i < death[l] {
                    death[l] = i;
                }
            }
        }

        // Lanes that DIE at revision i (absorbed as a non-first parent of
        // the merge at i) — evicted from the RAM combined state at i so it
        // carries only the lanes LIVE at i (the model's live-lane
        // semantics). `death[l] < n_rev` ⇒ the lane merges; a never-merged
        // lane stays to the end and is never evicted.
        let mut dying_at: Vec<Vec<LaneId>> = vec![Vec::new(); n_rev + 1];
        for l in 0..death.len() {
            if death[l] < n_rev {
                dying_at[death[l]].push(l as LaneId);
            }
        }

        // Build combined states forward and SEAL them into the chain as the
        // chain grows (the TREES store's tiered prepend / FrameEncoder +
        // seal discipline — `seal_prepend`), so at most one batch's worth of
        // reverse-delta records is ever buffered in RAM. Per-commit tree
        // Views come from the shipped importer's `frontier_walk` (each View
        // is its first parent's frontier + `delta_layer`, Arc-shared,
        // O(delta), streamed in `dag.order`).
        //
        // `full_combined` carries only the LIVE lanes as their own full
        // subtrees: it inserts the advanced lane at i and EVICTS lanes that
        // die at i (variant path), so RAM is O(peak_live_lanes × tree), not
        // O(total_lanes × tree). `cur` is what lands in the frame —
        // `full_combined` itself on the non-variant path (which keeps the
        // one-lane-per-record lockstep invariant, so it does NOT evict), or
        // the variant-delta rewrite when a cutoff is set.
        let depot = open_depot(dir)?;
        let batch_bound = batch_ram_bound();
        let mut full_combined = View::default();
        let mut lane_blob_sets: HashMap<LaneId, HashSet<Vec<u8>>> = HashMap::new();
        let mut prev: Option<View> = None;
        let mut batch: Vec<Vec<u8>> = Vec::new(); // reverse records, forward order
        let mut batch_bytes: u64 = 0;
        let mut expect = 0usize;
        crate::frontier_walk(
            repo,
            &dag,
            Vec::new(),
            &Default::default(),
            variant_cutoff.is_some(),
            &mut |cm, _tree_oid, view, oids, _cat| {
                let i = expect;
                if i >= n_rev || cm.sha != sha_of[i] {
                    return Err(Error::Chain(format!(
                        "frontier walk out of revision order at {i}: got {}",
                        cm.sha
                    )));
                }
                // Evict lanes that die at i BEFORE this revision's state is
                // built (variant path only — see above). The eviction shows
                // up in `cur` as a tombstone in the reverse delta at i (the
                // lane child is dropped); walking back re-adds it, so a
                // commit on the dead lane at r < i still reconstructs exactly.
                if variant_cutoff.is_some() {
                    for &dead in &dying_at[i] {
                        full_combined.children.remove(&lane_key(dead));
                        lane_blob_sets.remove(&dead);
                    }
                }
                let child = Arc::new(view.clone());
                full_combined.children.insert(lane_key(lane_of[i]), child.clone());
                let cur = match variant_cutoff {
                    Some(c) => {
                        // The frontier maintained this commit's git-oid set
                        // as its first parent's set + this commit's changes
                        // (zero hashing). It IS the advanced lane's blob-oid
                        // set this revision; every other live lane keeps its
                        // cached set. `oids` is a multiset (count per oid) —
                        // its live KEYS are the set the metric clusters on.
                        let set: HashSet<Vec<u8>> = oids.keys().cloned().collect();
                        lane_blob_sets.insert(lane_of[i], set);
                        // Lanes live at revision i: born and not yet merged
                        // away (birth ≤ i < death). A merged-away base drops
                        // out here, so clustering reframes onto a surviving
                        // base. The lane advanced at i is always live (a merge
                        // that ends lane l happens strictly after l's own
                        // commits), so it is always present.
                        let live: Vec<LaneId> = (0..death.len() as LaneId)
                            .filter(|&l| birth[l as usize] <= i && i < death[l as usize])
                            .collect();
                        build_stored(&full_combined, &live, &lane_blob_sets, c)?
                    }
                    None => full_combined.clone(),
                };
                if i == 0 {
                    // Seed the chain's f0 with the first full state; the
                    // depot forbids f1 on a chain's first prepend.
                    let f0raw = codec::encode(&depot::diff(None, Some(&cur)));
                    let f0 = compress_frame(&f0raw, None, level).map_err(cf)?;
                    depot.prepend(CHAIN, &f0, None, false).map_err(|e| cf(e.to_string()))?;
                } else if let Some(p) = &prev {
                    // Reverse delta rebuilding combined[i-1] from combined[i].
                    let rec = codec::encode(&depot::diff(Some(&cur), Some(p)));
                    batch_bytes += 8 + rec.len() as u64;
                    batch.push(rec);
                }
                prev = Some(cur);
                // Seal a batch once it reaches a frame's worth: `prev` is
                // combined[i], the newest state in the batch, so its full
                // encoding is the new f0 and the batch's reverse deltas
                // (newest-first) become the accumulator.
                if !batch.is_empty() && batch_bytes >= batch_bound {
                    let head = codec::encode(&depot::diff(None, prev.as_ref()));
                    let staged: Vec<Vec<u8>> = batch.drain(..).rev().collect();
                    seal_prepend(&depot, &head, &staged, level)?;
                    batch_bytes = 0;
                    // Flush now so eviction reclaims the just-superseded f0/f1
                    // frames — bounds the encode's transient disk footprint
                    // instead of letting dead full-state frames pile up.
                    depot.flush().map_err(|e| cf(e.to_string()))?;
                }
                expect += 1;
                Ok(())
            },
        )?;
        if expect != n_rev {
            return Err(Error::Chain(format!(
                "frontier walk produced {expect} of {n_rev} revisions"
            )));
        }
        // Final partial batch: its newest state (`prev` == combined[n-1])
        // becomes the chain's f0.
        if !batch.is_empty() {
            let head = codec::encode(&depot::diff(None, prev.as_ref()));
            let staged: Vec<Vec<u8>> = batch.drain(..).rev().collect();
            seal_prepend(&depot, &head, &staged, level)?;
        }
        if n_rev > 0 {
            depot.flush().map_err(|e| cf(e.to_string()))?;
        }

        let sha_to_rev: HashMap<String, usize> = sha_of
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i))
            .collect();

        // Refs that resolve to an in-scope commit — persisted so a
        // reopened store serves any ref's tree from disk alone.
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

    /// The reconstructed tree View of the commit at revision `rev`: the
    /// combined state's child for that revision's lane.
    pub fn tree_at(&self, rev: usize) -> Result<View> {
        let combined = self.combined_at(rev)?;
        resolve_lane_subtree(&combined, self.lane_of[rev])
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

    /// Per newest-first record position, the sorted set of top-level
    /// (lane) prefixes its stored delta touches. Position 0 is the full
    /// head record (all live lanes); every older position is a reverse
    /// delta and MUST touch exactly one lane prefix (the lockstep
    /// O(one-lane) proof).
    pub fn record_prefixes(&self) -> Result<Vec<Vec<Vec<u8>>>> {
        let mut out = Vec::new();
        self.walk_records(&mut |_pos, rec| {
            let layer = codec::decode(rec)?;
            let mut keys: Vec<Vec<u8>> = layer.root.children.keys().cloned().collect();
            keys.sort();
            out.push(keys);
            Ok(false)
        })?;
        Ok(out)
    }

    /// Map a revision that advances a lane to the newest-first record
    /// position whose reverse delta expresses that advance. Revision
    /// `rev` (for `1 <= rev < n_rev`) is expressed by the record rebuilding
    /// `combined[rev-1]` from `combined[rev]`, at position `n_rev - rev`.
    pub fn advance_record_pos(&self, rev: usize) -> usize {
        self.n_rev - rev
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

    pub fn lane_prefix(&self, l: LaneId) -> Vec<u8> {
        lane_key(l)
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

// LANE_KEY_LEN documents the wire width; assert it matches the id type.
const _: () = assert!(LANE_KEY_LEN == std::mem::size_of::<LaneId>());
