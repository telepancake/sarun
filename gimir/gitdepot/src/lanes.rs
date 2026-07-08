//! Branch-lane topology — the model's lane layer (see
//! `gimir/notes/branch-lane-model.md`). Pure functions, no store I/O:
//! given a commit DAG and per-lane content, assign each commit to a
//! lane and cluster live lanes into variant groups. Nothing here reads
//! or writes the depot; the caller (ingest) maps its shas to indices,
//! runs these, and encodes the result.
//!
//! Two settled properties this file is the home of:
//!
//! * **Append-only lane assignment.** A commit's lane is frozen at
//!   ingest from its first parent's (already-frozen) lane: it continues
//!   that lane if no earlier-processed sibling already took it, else it
//!   opens a fresh lane. Lane ids are minted monotonically in
//!   processing order, so running the assignment over a prefix and then
//!   over the whole DAG agrees on every shared commit — new history only
//!   ever appends lanes, never renumbers old ones. A ref tracks a lane
//!   purely by pointing at a commit whose lane is fixed forever.
//!
//! * **Variant grouping is encode-time policy, not format.** The
//!   similarity metric, the cutoff, and the base pick live behind
//!   [`cluster_variants`]; the store records only the outcome (a base
//!   lane id per group). Swapping the policy changes future encodes, not
//!   the format or any written frame.

use std::collections::{HashMap, HashSet};

/// A lane id: monotonic, frozen at birth, never reused.
pub type LaneId = u32;

/// The frozen lane assignment over a commit DAG.
#[derive(Debug, Clone)]
pub struct LaneAssignment {
    /// `lane_of[i]` = the lane id of commit `i`.
    pub lane_of: Vec<LaneId>,
    /// `span[l]` = `(birth, death)` commit indices of lane `l` — its
    /// first and last commit in processing order.
    pub span: Vec<(usize, usize)>,
}

impl LaneAssignment {
    /// Number of lanes ever minted.
    pub fn n_lanes(&self) -> LaneId {
        self.span.len() as LaneId
    }
}

/// Assign each commit to a lane, append-only.
///
/// `parents[i]` is commit `i`'s in-scope parent indices, **first parent
/// first**, every entry `< i` (the DAG is in topological order —
/// parents before children — and the caller has already dropped
/// out-of-scope parents, e.g. shallow boundaries). A commit with an
/// empty parent list is a root and opens a new lane.
///
/// The rule (the minimize-frame-delta objective realized): a commit
/// continues its first parent's lane when that lane is still that
/// parent's own tip (no sibling has extended it yet) — the parent's tree
/// is the closest slot, so continuing it is the smallest frame delta —
/// otherwise it forks a fresh lane.
pub fn assign_lanes(parents: &[Vec<usize>]) -> LaneAssignment {
    let n = parents.len();
    let mut lane_of = vec![0u32; n];
    // `extended[i]` — has a first-parent child already taken commit i's
    // lane? The first such child continues; later siblings fork.
    let mut extended = vec![false; n];
    let mut span: Vec<(usize, usize)> = Vec::new();
    for i in 0..n {
        let lane = match parents[i].first() {
            Some(&p) if !extended[p] => {
                extended[p] = true;
                lane_of[p]
            }
            _ => {
                let l = span.len() as LaneId;
                span.push((i, i));
                l
            }
        };
        lane_of[i] = lane;
        span[lane as usize].1 = i; // extend the lane's death to here
    }
    LaneAssignment { lane_of, span }
}

/// Remap the monotonic lane ids to COMPACT ids with reuse: a lane's index
/// is freed when the lane dies and the next-born lane takes the lowest free
/// index. Because a reused index is only ever handed to a lane whose life
/// starts at/after the previous holder's death, two lanes sharing an index
/// have disjoint `[birth, death)` windows and so are never both live at the
/// same revision — the bitmap stays a valid per-revision lane set. The
/// result: bitmap width = PEAK CONCURRENT live lanes, not total lanes ever
/// (which for a long-lived repo is a large difference).
///
/// `birth[l]`/`death[l]` are lane `l`'s live window in revision indices
/// (`death == n_rev` ⇒ never dies, index never freed). Returns the compact
/// id per original lane and the total width (highest index + 1 ever used).
/// Deaths at a revision are freed before that revision's births are placed,
/// so an index freed at revision `r` is available to a lane born at `r`.
pub fn compact_lanes(birth: &[usize], death: &[usize], n_rev: usize) -> (Vec<LaneId>, usize) {
    use std::collections::BinaryHeap;
    let n = birth.len();
    let mut born_at: Vec<Vec<usize>> = vec![Vec::new(); n_rev + 1];
    let mut die_at: Vec<Vec<usize>> = vec![Vec::new(); n_rev + 1];
    for l in 0..n {
        born_at[birth[l]].push(l);
        if death[l] < n_rev {
            die_at[death[l]].push(l);
        }
    }
    let mut compact = vec![0u32; n];
    let mut free: BinaryHeap<std::cmp::Reverse<u32>> = BinaryHeap::new();
    let mut next: u32 = 0;
    for rev in 0..=n_rev {
        for &l in &die_at[rev] {
            free.push(std::cmp::Reverse(compact[l]));
        }
        for &l in &born_at[rev] {
            compact[l] = match free.pop() {
                Some(std::cmp::Reverse(c)) => c,
                None => {
                    let c = next;
                    next += 1;
                    c
                }
            };
        }
    }
    (compact, next as usize)
}

/// A variant group: a `base` lane and the `variants` stored as deltas
/// against it. An independent lane is a singleton group (`variants`
/// empty, it is its own base).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantGroup {
    pub base: LaneId,
    pub variants: Vec<LaneId>,
}

/// Cluster live lanes into variant groups by blob-oid overlap.
///
/// `blob_sets[l]` is lane `l`'s set of blob oids at the revision being
/// grouped. Two lanes are variants when their Jaccard overlap exceeds
/// `cutoff`. Greedy and deterministic: lanes are considered in ascending
/// id, each joins the first existing group whose base it overlaps past
/// the cutoff, else it opens a new group as its own base (lowest id in a
/// group is the base — stable across incremental runs, so a group does
/// not needlessly reframe).
///
/// This whole function is the policy seam (see module docs): metric,
/// cutoff, and base pick are here and only here, and the store keeps
/// only the resulting base ids.
pub fn cluster_variants(
    live: &[LaneId],
    blob_sets: &HashMap<LaneId, HashSet<Vec<u8>>>,
    cutoff: f64,
) -> Vec<VariantGroup> {
    let mut groups: Vec<VariantGroup> = Vec::new();
    let mut order = live.to_vec();
    order.sort_unstable();
    for lane in order {
        let empty = HashSet::new();
        let s = blob_sets.get(&lane).unwrap_or(&empty);
        let mut placed = false;
        for g in groups.iter_mut() {
            let bs = blob_sets.get(&g.base).unwrap_or(&empty);
            if jaccard(s, bs) > cutoff {
                g.variants.push(lane);
                placed = true;
                break;
            }
        }
        if !placed {
            groups.push(VariantGroup { base: lane, variants: Vec::new() });
        }
    }
    groups
}

/// Jaccard overlap of two oid sets. Two empty sets are identical (1.0);
/// an empty set against a non-empty one shares nothing (0.0).
fn jaccard(a: &HashSet<Vec<u8>>, b: &HashSet<Vec<u8>>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.iter().filter(|x| b.contains(*x)).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oids(items: &[&str]) -> HashSet<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn compact_reuses_freed_indices() {
        // Three lanes, n_rev = 30. Lane 0 lives the whole way (never dies).
        // Lane 1 dies at 10; lane 2 is born at 10 → it should reuse lane 1's
        // freed index. Peak concurrent = 2, so width = 2, not 3.
        let birth = vec![0, 0, 10];
        let death = vec![30, 10, 30];
        let (compact, width) = compact_lanes(&birth, &death, 30);
        assert_eq!(width, 2, "peak concurrent live lanes is 2");
        assert_eq!(compact[0], 0);
        assert_eq!(compact[1], 1);
        assert_eq!(compact[2], compact[1], "lane 2 reuses lane 1's freed index");
    }

    #[test]
    fn compact_disjoint_lanes_share_one_index() {
        // A chain of short-lived lanes, each dying before the next is born,
        // all collapse onto a single index.
        let birth = vec![0, 5, 10, 15];
        let death = vec![5, 10, 15, 20];
        let (compact, width) = compact_lanes(&birth, &death, 20);
        assert_eq!(width, 1, "never more than one live at a time");
        assert!(compact.iter().all(|&c| c == 0));
    }

    #[test]
    fn compact_never_collides_live_lanes() {
        // Two permanently-live lanes must keep distinct indices.
        let birth = vec![0, 3];
        let death = vec![20, 20];
        let (compact, width) = compact_lanes(&birth, &death, 20);
        assert_eq!(width, 2);
        assert_ne!(compact[0], compact[1]);
    }

    #[test]
    fn linear_history_is_one_lane() {
        // 0 <- 1 <- 2 <- 3
        let p = vec![vec![], vec![0], vec![1], vec![2]];
        let a = assign_lanes(&p);
        assert_eq!(a.lane_of, vec![0, 0, 0, 0]);
        assert_eq!(a.n_lanes(), 1);
        assert_eq!(a.span, vec![(0, 3)]);
    }

    #[test]
    fn fork_opens_a_second_lane() {
        // 0 <- 1 ; 1 has two children 2 and 3 (both first-parent 1).
        // 2 is processed first -> continues lane 0; 3 forks lane 1.
        let p = vec![vec![], vec![0], vec![1], vec![1]];
        let a = assign_lanes(&p);
        assert_eq!(a.lane_of, vec![0, 0, 0, 1]);
        assert_eq!(a.n_lanes(), 2);
        assert_eq!(a.span[1], (3, 3)); // lane 1 born and (so far) dies at 3
    }

    #[test]
    fn merge_ends_the_second_parent_lane() {
        // 0 <- 1 (lane0); 0 <- 2 (fork, lane1); 3 merges: first parent 1,
        // second parent 2. 3 continues lane0; lane1's last commit is 2.
        let p = vec![vec![], vec![0], vec![0], vec![1, 2]];
        let a = assign_lanes(&p);
        assert_eq!(a.lane_of, vec![0, 0, 1, 0]);
        assert_eq!(a.span[0], (0, 3)); // mainline runs through the merge
        assert_eq!(a.span[1], (2, 2)); // topic lane ends at its tip, pre-merge
    }

    #[test]
    fn append_only_prefix_agrees_with_full() {
        // Full DAG; assigning a prefix must agree on shared commits and
        // never renumber them when the tail is added.
        let full = vec![vec![], vec![0], vec![1], vec![1], vec![2, 3]];
        let a_full = assign_lanes(&full);
        for cut in 1..=full.len() {
            let prefix: Vec<Vec<usize>> = full[..cut].to_vec();
            let a_pref = assign_lanes(&prefix);
            assert_eq!(
                a_pref.lane_of[..],
                a_full.lane_of[..cut],
                "prefix len {cut} disagrees with full assignment"
            );
        }
    }

    #[test]
    fn adding_a_child_extends_not_renumbers() {
        // A tip gets a new first-parent child in a later "fetch": the
        // child continues the tip's lane, nothing is renumbered.
        let before = assign_lanes(&[vec![], vec![0], vec![1]]);
        let after = assign_lanes(&[vec![], vec![0], vec![1], vec![2]]);
        assert_eq!(after.lane_of[..3], before.lane_of[..]);
        assert_eq!(after.lane_of[3], before.lane_of[2]); // extends lane 0
        assert_eq!(after.n_lanes(), 1);
    }

    #[test]
    fn variants_cluster_independents_stand_alone() {
        // Lanes 0 and 1 share most blobs (variants); lane 2 shares
        // nothing (independent).
        let mut sets = HashMap::new();
        sets.insert(0u32, oids(&["a", "b", "c", "d"]));
        sets.insert(1u32, oids(&["a", "b", "c", "e"])); // 3/5 with lane 0
        sets.insert(2u32, oids(&["x", "y", "z"]));
        let g = cluster_variants(&[0, 1, 2], &sets, 0.5);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0], VariantGroup { base: 0, variants: vec![1] });
        assert_eq!(g[1], VariantGroup { base: 2, variants: vec![] });
    }

    #[test]
    fn clustering_base_is_lowest_id_and_deterministic() {
        let mut sets = HashMap::new();
        sets.insert(5u32, oids(&["a", "b", "c"]));
        sets.insert(2u32, oids(&["a", "b", "c"])); // identical
        sets.insert(9u32, oids(&["a", "b", "c"]));
        let g = cluster_variants(&[9, 5, 2], &sets, 0.5);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].base, 2); // lowest id, regardless of input order
        assert_eq!(g[0].variants, vec![5, 9]);
    }

    #[test]
    fn empty_trees_are_identical_not_independent() {
        let mut sets = HashMap::new();
        sets.insert(0u32, HashSet::new());
        sets.insert(1u32, HashSet::new());
        let g = cluster_variants(&[0, 1], &sets, 0.5);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0], VariantGroup { base: 0, variants: vec![1] });
    }
}
