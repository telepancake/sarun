//! The reflog (ASSEMBLY.md §5): one entry per written layer, recording ALL
//! lanes of that layer (each lane → the commit it holds) plus any ref changes
//! at that point. Lanes are the persistent columns from [`crate::lanes`]
//! (append-only, ancestry-frozen); the reflog is the per-layer record of what
//! each lane held and how the refs moved.
//!
//! Invariant enforced here: **#live lanes ≥ #live refs** — every live ref
//! points at a commit that must occupy a lane.

use std::collections::BTreeMap;

/// A git commit id (hex sha). Opaque to the reflog.
pub type CommitId = String;

/// One written layer's record: the commit each lane holds (a dead lane slot is
/// `None`), and the ref moves applied at this layer (`None` = ref deleted).
#[derive(Debug, Clone)]
pub struct LayerEntry {
    pub lanes: Vec<Option<CommitId>>,
    pub ref_changes: Vec<(String, Option<CommitId>)>,
}

impl LayerEntry {
    /// The number of live (occupied) lanes in this layer.
    pub fn live_lanes(&self) -> usize {
        self.lanes.iter().filter(|c| c.is_some()).count()
    }
}

/// The append-only reflog plus the current ref → tip map it maintains.
#[derive(Debug, Default)]
pub struct Reflog {
    pub layers: Vec<LayerEntry>,
    refs: BTreeMap<String, CommitId>,
}

impl Reflog {
    pub fn new() -> Self {
        Reflog::default()
    }

    /// Record a written layer: apply its ref changes, push the entry, and
    /// enforce `#live lanes ≥ #live refs`. Panics on violation — a live ref
    /// with no lane to hold its commit is a construction bug.
    pub fn record(&mut self, lanes: Vec<Option<CommitId>>, ref_changes: Vec<(String, Option<CommitId>)>) {
        for (name, tip) in &ref_changes {
            match tip {
                Some(c) => {
                    self.refs.insert(name.clone(), c.clone());
                }
                None => {
                    self.refs.remove(name);
                }
            }
        }
        let entry = LayerEntry { lanes, ref_changes };
        assert!(
            entry.live_lanes() >= self.refs.len(),
            "reflog invariant: {} live lanes < {} live refs",
            entry.live_lanes(),
            self.refs.len()
        );
        self.layers.push(entry);
    }

    /// The current live refs (name → tip commit).
    pub fn refs(&self) -> &BTreeMap<String, CommitId> {
        &self.refs
    }

    /// The most recent layer's lane → commit vector.
    pub fn latest_lanes(&self) -> Option<&[Option<CommitId>]> {
        self.layers.last().map(|e| e.lanes.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::lanes::assign_lanes;
    use crate::layer::{self, LaneEntry, LaneTree, Mode};

    fn tree(entries: &[(&[u8], &[u8])]) -> LaneTree {
        entries
            .iter()
            .map(|(p, c)| {
                (p.to_vec(), LaneEntry { mode: Mode::File, oid: c.to_vec(), content: c.to_vec() })
            })
            .collect()
    }

    #[test]
    fn invariant_holds_and_tracks_refs() {
        let mut rl = Reflog::new();
        rl.record(vec![Some("c0".into())], vec![("refs/heads/main".into(), Some("c0".into()))]);
        assert_eq!(rl.refs().len(), 1);
        // Two refs need two live lanes.
        rl.record(
            vec![Some("c1".into()), Some("t0".into())],
            vec![("refs/heads/topic".into(), Some("t0".into())), ("refs/heads/main".into(), Some("c1".into()))],
        );
        assert_eq!(rl.refs().len(), 2);
        // Deleting a ref drops the requirement.
        rl.record(vec![Some("c2".into()), None], vec![("refs/heads/topic".into(), None)]);
        assert_eq!(rl.refs().len(), 1);
    }

    #[test]
    #[should_panic(expected = "reflog invariant")]
    fn too_few_lanes_panics() {
        let mut rl = Reflog::new();
        // Two live refs but only one live lane → violation.
        rl.record(
            vec![Some("c0".into())],
            vec![("refs/heads/a".into(), Some("c0".into())), ("refs/heads/b".into(), Some("c1".into()))],
        );
    }

    /// End-to-end capstone: a synthetic commit DAG driven through the real lane
    /// assignment and the union engine. Each commit's tree must reconstruct
    /// SHA-exact from its assigned lane, and the reflog's invariant must hold at
    /// every written layer.
    ///
    /// DAG (topo order), first-parent first:
    ///   0 ── 1 ── 2 ── 4(merge 2,3)
    ///          └─ 3 ──┘
    /// main advances along 0,1,2,4; topic is 3.
    #[test]
    fn dag_drives_union_sha_exact() {
        let parents: Vec<Vec<usize>> = vec![vec![], vec![0], vec![1], vec![1], vec![2, 3]];
        // Each commit's tree (a small evolving file set).
        let trees = vec![
            tree(&[(b"README", b"r0"), (b"src/main.rs", b"m0")]),
            tree(&[(b"README", b"r0"), (b"src/main.rs", b"m1")]),
            tree(&[(b"README", b"r0"), (b"src/main.rs", b"m2"), (b"src/lib.rs", b"l0")]),
            tree(&[(b"README", b"r0"), (b"src/main.rs", b"m1"), (b"topic.txt", b"t0")]),
            tree(&[(b"README", b"r1"), (b"src/main.rs", b"m2"), (b"src/lib.rs", b"l0"), (b"topic.txt", b"t0")]),
        ];
        let a = assign_lanes(&parents);
        let n_lanes = a.n_lanes() as usize;

        // Drive the frame: process commits in order, each updating its lane's
        // tree; the other lanes keep their current tip. Reconstruct that
        // commit's tree from its lane after the advance.
        let mut lanes: Vec<LaneTree> = vec![LaneTree::new(); n_lanes];
        let mut f: Option<Frame> = None;
        let mut rl = Reflog::new();
        // lane → commit id currently held (as an id string).
        let mut lane_tip: Vec<Option<String>> = vec![None; n_lanes];

        for c in 0..parents.len() {
            let l = a.lane_of[c] as usize;
            lanes[l] = trees[c].clone();
            lane_tip[l] = Some(format!("c{c}"));

            f = Some(match f.take() {
                None => Frame::seed(lanes.clone()),
                Some(mut fr) => {
                    fr.advance(lanes.clone());
                    fr
                }
            });
            let fr = f.as_ref().unwrap();

            // This commit reconstructs SHA-exact from its lane.
            assert_eq!(
                fr.reconstruct_tree_oid(l as u32).unwrap(),
                layer::lanetree_tree_oid(&trees[c]).unwrap(),
                "commit {c} on lane {l}"
            );

            // Record the layer in the reflog. Ref moves: main follows lane 0's
            // tip, topic follows lane 1's (when born).
            let mut ref_changes = vec![("refs/heads/main".into(), lane_tip[0].clone())];
            if n_lanes > 1 && lane_tip[1].is_some() {
                ref_changes.push(("refs/heads/topic".into(), lane_tip[1].clone()));
            }
            rl.record(lane_tip.clone(), ref_changes);
        }

        // After the whole DAG, every lane's live tip reconstructs to the tree
        // of the last commit assigned to it.
        let fr = f.as_ref().unwrap();
        for l in 0..n_lanes {
            let (birth, death) = a.span[l];
            let _ = birth;
            let tip_commit = death; // last commit index on this lane
            assert_eq!(
                fr.reconstruct_tree_oid(l as u32).unwrap(),
                layer::lanetree_tree_oid(&trees[tip_commit]).unwrap(),
                "lane {l} tip is commit {tip_commit}"
            );
        }
        // The reflog kept its invariant across every layer (record() asserts).
        assert_eq!(rl.layers.len(), parents.len());
    }
}
