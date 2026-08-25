//! Branch-lane topology — the model's lane layer (see
//! `gimir/notes/branch-lane-model.md`). Pure functions, no store I/O:
//! given a commit DAG and per-lane content, assign each commit to a
//! lane, and remap lane ids to a compact reused space. Nothing here reads
//! or writes the depot; the caller (ingest) maps its shas to indices,
//! runs these, and encodes the result.
//!
//! * **Append-only lane assignment.** [`assign_lanes`] freezes a commit's
//!   lane from its first parent's (already-frozen) lane: it continues that
//!   lane if no earlier-processed sibling already took it, else opens a
//!   fresh one. Ids are minted monotonically in processing order.
//!
//! * **Compaction with reuse.** [`compact_lanes`] then remaps those
//!   monotonic ids to a compact space, reusing a lane's index once it dies,
//!   so a bitmap over live lanes is only as wide as the peak concurrent
//!   count — not the total lanes ever minted.


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

#[cfg(test)]
mod tests {
    use super::*;

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

}
