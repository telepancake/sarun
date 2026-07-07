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
use wikimak_depot::{Depot, DepotConfig, FrameDecoder, FrameEncoder};

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

/// Collect the git blob oids (hex) of every non-gitlink leaf under a full
/// subtree View — the pure oid set the variant metric clusters on. A
/// gitlink (mode 160000) is a commit pointer, not a blob, so it is
/// excluded.
fn collect_blob_oids(view: &View, out: &mut HashSet<Vec<u8>>) {
    if let Some(blob) = &view.blob {
        let gitlink = view.attrs.get(&b"mode"[..]).map(|m| m.as_slice() == b"160000").unwrap_or(false);
        if !gitlink {
            out.insert(crate::git_obj_oid("blob", blob).into_bytes());
        }
    }
    for child in view.children.values() {
        collect_blob_oids(child, out);
    }
}

/// Rewrite a FULL combined state (every lane child its own full subtree)
/// into the STORED combined state that actually lands in the frame:
/// cluster the live lanes ([`cluster_variants`]); each group's BASE lane
/// keeps its full subtree, and every VARIANT lane is replaced by a marker
/// child carrying `diff(base_subtree, variant_subtree)` — so the
/// frame-resident uncompressed content of a group is one base subtree plus
/// small per-variant deltas, not N full trees. Independent lanes are
/// singleton groups and stay full, exactly as the non-variant path stores
/// every lane. This is representation (a): the delta lives IN the child
/// slot behind a marker, self-describing, consulted by the reader.
fn build_stored(full: &View, cutoff: f64) -> Result<View> {
    let mut live: Vec<LaneId> = Vec::with_capacity(full.children.len());
    let mut blob_sets: HashMap<LaneId, HashSet<Vec<u8>>> = HashMap::new();
    for (key, child) in &full.children {
        let lane = decode_lane_key(key)?;
        let mut set = HashSet::new();
        collect_blob_oids(child, &mut set);
        blob_sets.insert(lane, set);
        live.push(lane);
    }
    let groups = cluster_variants(&live, &blob_sets, cutoff);
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
/// is `apply(base_subtree, variant_delta)` with the base resolved FIRST
/// (base is always stored full in the same combined state — the base stays
/// alive across the revisions it spans in this increment), then the
/// variant delta on top. A non-variant child is already its full subtree.
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
        let (sha_of, parents) = topo_parents(repo)?;
        let assignment = assign_lanes(&parents);
        let lane_of = assignment.lane_of;
        let n_rev = sha_of.len();

        // Per-commit tree View, built exactly as the importer builds a
        // standalone tree (full ls-tree + blobs → layer → apply). An
        // empty tree is the empty-but-present View (existence is
        // first-class in the depot).
        let mut trees: Vec<View> = Vec::with_capacity(n_rev);
        for sha in &sha_of {
            trees.push(commit_tree_view(repo, sha)?);
        }

        // Build combined states forward, emitting the newest full record
        // and the reverse deltas (record j rebuilds combined[j] from
        // combined[j+1]).
        // `full_combined` carries every live lane as its own full subtree
        // (the increment-1 shape); `cur` is what actually lands in the
        // frame — identical to `full_combined` on the non-variant path, or
        // the variant-delta rewrite when a cutoff is set.
        let mut full_combined = View::default();
        let mut prev: Option<View> = None;
        let mut reverse: Vec<Vec<u8>> = Vec::new(); // reverse[j], j = 0..n_rev-2
        let mut newest_full: Vec<u8> = Vec::new();
        for i in 0..n_rev {
            full_combined
                .children
                .insert(lane_key(lane_of[i]), Arc::new(trees[i].clone()));
            let cur = match variant_cutoff {
                Some(c) => build_stored(&full_combined, c)?,
                None => full_combined.clone(),
            };
            if let Some(p) = &prev {
                // Rebuilds combined[i-1] from combined[i].
                reverse.push(codec::encode(&depot::diff(Some(&cur), Some(p))));
            }
            if i == n_rev - 1 {
                newest_full = codec::encode(&depot::diff(None, Some(&cur)));
            }
            prev = Some(cur);
        }

        let depot = open_depot(dir)?;
        if n_rev > 0 {
            // f0 = newest full record. First prepend forbids f1.
            let f0 = wikimak_depot::compress_frame(&newest_full, None, level).map_err(cf)?;
            depot.prepend(CHAIN, &f0, None, false).map_err(|e| cf(e.to_string()))?;

            if !reverse.is_empty() {
                // f1 = older records newest-first: reverse[n-2],
                // reverse[n-3], … reverse[0]. Each u64-length-prefixed,
                // anchored on the f0 record — the TREES frame discipline.
                let f1_records: Vec<&[u8]> = reverse.iter().rev().map(|r| r.as_slice()).collect();
                let total: u64 = f1_records.iter().map(|r| 8 + r.len() as u64).sum();
                let mut enc = FrameEncoder::new(total, Some(&newest_full), level).map_err(cf)?;
                for r in &f1_records {
                    enc.write(&(r.len() as u64).to_le_bytes()).map_err(cf)?;
                    enc.write(r).map_err(cf)?;
                }
                let f1 = enc.finish().map_err(cf)?;
                let f0 = wikimak_depot::compress_frame(&newest_full, None, level).map_err(cf)?;
                depot
                    .prepend(CHAIN, &f0, Some(&f1), false)
                    .map_err(|e| cf(e.to_string()))?;
            }
            depot.flush().map_err(|e| cf(e.to_string()))?;
        }

        let sha_to_rev = sha_of
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i))
            .collect();
        Ok(LaneStore { depot, n_rev, lane_of, sha_of, sha_to_rev })
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
    /// true to stop. f0 is position 0; f1 records follow, anchored on the
    /// f0 record.
    fn walk_records(
        &self,
        visit: &mut dyn FnMut(usize, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let head = match self.depot.read_f0(CHAIN) {
            Ok(frame) => wikimak_depot::decompress_frame(&frame, None).map_err(cf)?,
            Err(wikimak_depot::Error::NoFrame) => return Ok(()),
            Err(e) => return Err(cf(e.to_string())),
        };
        let mut pos = 0usize;
        if visit(pos, &head)? {
            return Ok(());
        }
        if let Some(f1) = self.depot.read_f1(CHAIN).map_err(|e| cf(e.to_string()))? {
            let mut stopped = false;
            stream_f1_records(&f1, &head, &mut |rec| {
                pos += 1;
                stopped = visit(pos, rec)?;
                Ok(stopped)
            })?;
        }
        Ok(())
    }
}

/// The commit's full tree View (ls-tree + blobs → layer → apply). Same
/// construction the importer uses for a standalone tagged tree.
fn commit_tree_view(repo: &Path, sha: &str) -> Result<View> {
    let entries = crate::ls_tree(repo, sha)?;
    let blobs = crate::fetch_blobs(
        repo,
        entries
            .iter()
            .filter(|e| e.mode != "160000")
            .map(|e| e.oid.clone()),
    )?;
    let layer = crate::tree_layer(&entries, &blobs)?;
    Ok(depot::apply(None, &layer).unwrap_or_default())
}

/// `(shas_in_topo_order, parents_as_indices)` for every reachable commit
/// (branches + tags), parents-before-children. First parent first;
/// out-of-scope parents (none, in a full non-shallow walk) are dropped.
fn topo_parents(repo: &Path) -> Result<(Vec<String>, Vec<Vec<usize>>)> {
    let out = crate::git_str(
        repo,
        &["rev-list", "--parents", "--topo-order", "--reverse", "--branches", "--tags"],
    )?;
    let mut shas: Vec<String> = Vec::new();
    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut parents: Vec<Vec<usize>> = Vec::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let Some(sha) = it.next() else { continue };
        let i = shas.len();
        idx.insert(sha.to_string(), i);
        shas.push(sha.to_string());
        // Parents already seen (topo --reverse guarantees this for all
        // in-scope parents); keep first-parent order.
        let ps: Vec<usize> = it.filter_map(|p| idx.get(p).copied()).collect();
        parents.push(ps);
    }
    Ok((shas, parents))
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
