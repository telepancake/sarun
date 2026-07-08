//! The commit/tree/lane encoder: turns a sequence of parallel-lane states
//! into the union frame chain, driving the per-path [`crate::reslot`] unit
//! for stable slot assignment. It holds a persistent **skeleton** — the
//! union structure as slot state per path — and mutates it one revision at
//! a time, emitting the depot delta from the slot changes.
//!
//! ## Incremental by construction
//!
//! A revision is described by the LANE TRANSITIONS it makes: the one
//! advancing lane (old tree → new tree) plus any lanes a merge kills (tree
//! → absent). [`Encoder::advance`] walks ONLY the paths those transitions
//! actually change, and non-transitioning lanes are never revisited: their
//! variants already sit in the skeleton. So a revision costs O(the advancing
//! commit's own diff), not O(all live lanes × tree).
//!
//! An unchanged subtree is skipped by [`view_eq`], which first tries `Arc`
//! pointer equality. That fires because the frontier builds each commit's
//! view as `first_parent_view + apply_mut(delta)` with structural sharing,
//! and for a lane-CONTINUING commit the first parent is the previous view on
//! that lane — the very `Arc` this encoder holds — so an untouched subtree is
//! the same `Arc`, an O(1) skip. Where the `Arc`s do NOT coincide (a lane
//! birth, whose `old` is absent; a fork/merge boundary), `view_eq` falls back
//! to a value comparison — correct, but O(subtree). The principled,
//! frontend-native prune would compare the git subtree OID (which the walker
//! sees) instead of leaning on in-memory identity; `depot::View` does not
//! carry the OID today, so this is a deliberate shortcut, not a guarantee.
//!
//! A variant's identity is the pair `(attrs, content)` (Arc-backed, so its
//! bytes are shared, not copied), which is exactly what a `\0v`/`\0m` frame
//! node needs — no separate content lookup, no id hashing.
//!
//! Two outputs, matching the reverse-delta chain the depot stores:
//!   * [`Encoder::advance`] applies the transitions and returns the REVERSE
//!     delta (rebuild the previous state from the new) — each older record.
//!   * [`Encoder::full`] is the FORWARD delta building the current state
//!     from empty — the `f0` head and every seal boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use depot::{Attrs, Bytes, Layer, Name, Node, View};

use crate::reslot::{Bitmap, Occupant, Slots};
use crate::variants::{content_key, content_view, leaf_delta, meta_key, meta_view};

/// A variant's identity: file attrs and content. The `\0v` blob is `.1`, the
/// `\0m` attrs are `.0`.
type VarKey = (Attrs, Bytes);

/// One lane's change this revision: `(lane, old_view_here, new_view_here)`.
/// At the root the views are the lane's whole old/new tree; a dying lane has
/// `new = None`; the initial birth of a lane has `old = None`.
pub type Trans<'a> = (usize, Option<&'a View>, Option<&'a View>);

/// The persistent union skeleton at one path: file variants as slots, plus
/// subdirectory children. Both coexist (a path that is a file in some lanes
/// and a directory in others).
#[derive(Default)]
struct Skel {
    slots: Slots<VarKey>,
    children: BTreeMap<Name, Skel>,
}

impl Skel {
    fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.children.is_empty()
    }
}

fn set_bit(bm: &mut Bitmap, i: usize) {
    let byte = i / 8;
    if bm.len() <= byte {
        bm.resize(byte + 1, 0);
    }
    bm[byte] |= 1 << (i % 8);
}
fn clear_bit(bm: &mut [u8], i: usize) {
    let byte = i / 8;
    if byte < bm.len() {
        bm[byte] &= !(1 << (i % 8));
    }
}

fn view_eq(a: Option<&View>, b: Option<&View>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => std::ptr::eq(a, b) || a == b,
        _ => false,
    }
}
/// The lane's child View at `name`, if the lane is a directory here.
fn child_at<'a>(v: Option<&'a View>, name: &[u8]) -> Option<&'a View> {
    v.filter(|v| v.blob.is_none()).and_then(|v| v.children.get(name)).map(Arc::as_ref)
}
fn blob_present(v: Option<&View>) -> bool {
    v.is_some_and(|v| v.blob.is_some())
}

fn occ_content(o: &Occupant<VarKey>) -> View {
    content_view(&o.id.1)
}
fn occ_meta(o: &Occupant<VarKey>) -> View {
    meta_view(&o.id.0, &o.bitmap)
}

/// The path's new variant set: the skeleton's CURRENT variants (all lanes),
/// with each transitioning lane's bit moved to whatever content it now
/// carries here (removed if it no longer has a blob here). Non-transitioning
/// lanes' bits are already in the slots and stay put — that is what makes
/// the step cost independent of the lane count.
fn new_variant_set(slots: &Slots<VarKey>, trans: &[Trans]) -> BTreeMap<VarKey, Bitmap> {
    let mut set: BTreeMap<VarKey, Bitmap> = BTreeMap::new();
    for (_, occ) in slots.iter() {
        set.insert(occ.id.clone(), occ.bitmap.clone());
    }
    for (lane, _old, new) in trans {
        for bm in set.values_mut() {
            clear_bit(bm, *lane);
        }
        if let Some(v) = new {
            if let Some(content) = &v.blob {
                set_bit(set.entry((v.attrs.clone(), content.clone())).or_default(), *lane);
            }
        }
    }
    set.retain(|_, bm| bm.iter().any(|&b| b != 0));
    set
}

/// Apply the transitions at one path, returning the reverse delta that
/// rebuilds the OLD state from the NEW one. Recurses only into children some
/// transition actually changes.
fn advance_node(node: &mut Skel, trans: &[Trans]) -> Node {
    let mut out = Node::keep();

    // File variants: only if this path has any (existing slots, or a
    // transition presenting a blob here).
    if !node.slots.is_empty() || trans.iter().any(|(_, o, n)| blob_present(*o) || blob_present(*n)) {
        let new_set = new_variant_set(&node.slots, trans);
        for ch in node.slots.reslot(&new_set) {
            // reverse: rebuild `before` (old) from `after` (new).
            let c = leaf_delta(
                ch.after.as_ref().map(occ_content).as_ref(),
                ch.before.as_ref().map(occ_content).as_ref(),
            );
            if !c.is_identity() {
                out.children.insert(content_key(ch.slot), c);
            }
            let m = leaf_delta(
                ch.after.as_ref().map(occ_meta).as_ref(),
                ch.before.as_ref().map(occ_meta).as_ref(),
            );
            if !m.is_identity() {
                out.children.insert(meta_key(ch.slot), m);
            }
        }
    }

    // Directory children — only the names some transition changes. A child a
    // transition leaves `Arc`-identical is never enumerated, so an untouched
    // subtree costs nothing.
    let mut names: BTreeSet<Name> = BTreeSet::new();
    for (_, o, n) in trans {
        for v in [o, n].into_iter().flatten() {
            if v.blob.is_none() {
                names.extend(v.children.keys().cloned());
            }
        }
    }
    for name in names {
        let sub: Vec<Trans> = trans
            .iter()
            .filter_map(|(l, o, n)| {
                let oc = child_at(*o, &name);
                let nc = child_at(*n, &name);
                if view_eq(oc, nc) {
                    None // this lane unchanged at this child
                } else {
                    Some((*l, oc, nc))
                }
            })
            .collect();
        if sub.is_empty() {
            continue;
        }
        let (cnode, empty) = {
            let child = node.children.entry(name.clone()).or_default();
            let cn = advance_node(child, &sub);
            (cn, child.is_empty())
        };
        if empty {
            node.children.remove(&name);
        }
        if !cnode.is_identity() {
            out.children.insert(name, cnode);
        }
    }
    out
}

/// Forward full node: build the current skeleton state from the empty base.
fn full_node(node: &Skel) -> Node {
    let mut out = Node::keep();
    for (slot, occ) in node.slots.iter() {
        let c = leaf_delta(None, Some(&occ_content(occ)));
        if !c.is_identity() {
            out.children.insert(content_key(slot), c);
        }
        let m = leaf_delta(None, Some(&occ_meta(occ)));
        if !m.is_identity() {
            out.children.insert(meta_key(slot), m);
        }
    }
    for (name, child) in &node.children {
        let cn = full_node(child);
        if !cn.is_identity() {
            out.children.insert(name.clone(), cn);
        }
    }
    out
}

/// The stateful union encoder.
#[derive(Default)]
pub struct Encoder {
    root: Skel,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder::default()
    }

    /// Apply this revision's lane transitions and return the reverse delta
    /// rebuilding the PREVIOUS state from the new one — each older chain
    /// record. At the first revision the return is discarded in favour of
    /// [`full`](Self::full).
    pub fn advance(&mut self, trans: &[Trans]) -> Layer {
        Layer { root: advance_node(&mut self.root, trans) }
    }

    /// The forward full delta of the current state (build from empty) — the
    /// chain's `f0` head and every seal boundary.
    pub fn full(&self) -> Layer {
        Layer { root: full_node(&self.root) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variants::extract;

    fn leaf(content: &str, mode: &str) -> View {
        let mut attrs = Attrs::new();
        attrs.insert(b"mode".to_vec(), mode.as_bytes().to_vec());
        View { blob: Some(content.as_bytes().into()), attrs, children: BTreeMap::new() }
    }
    fn dir(entries: &[(&str, View)]) -> View {
        View {
            blob: None,
            attrs: Attrs::new(),
            children: entries.iter().map(|(n, v)| (n.as_bytes().to_vec(), Arc::new(v.clone()))).collect(),
        }
    }

    /// Transitions from one full state to the next: every lane whose view
    /// changed (the prune skips the rest cheaply).
    fn transitions<'a>(prev: &'a [Option<View>], next: &'a [Option<View>]) -> Vec<Trans<'a>> {
        (0..prev.len().max(next.len()))
            .map(|l| (l, prev.get(l).and_then(Option::as_ref), next.get(l).and_then(Option::as_ref)))
            .collect()
    }

    /// Drive a sequence of states through the encoder as the store does (f0 =
    /// full of the newest; each older record = the transitions from it to the
    /// older state), then reconstruct every state newest-first and extract
    /// each lane, checking it equals the input.
    fn assert_chain(states: &[Vec<Option<View>>]) {
        let n_lanes = states.iter().map(Vec::len).max().unwrap_or(0);
        let empty: Vec<Option<View>> = vec![None; n_lanes];
        let mut enc = Encoder::new();
        let mut records: Vec<Layer> = Vec::new(); // record[k] rebuilds state k from k+1
        let mut prev = &empty;
        for st in states {
            let rev = enc.advance(&transitions(prev, st));
            records.push(rev); // record for state index; [0] rebuilds empty (unused)
            prev = st;
        }
        let f0 = enc.full();
        let mut cur = depot::apply(None, &f0);
        let newest = states.len() - 1;
        for (i, want) in states[newest].iter().enumerate() {
            let got = cur.as_ref().map(|u| extract(u, i)).unwrap_or_default();
            assert_eq!(&got, want.as_ref().unwrap_or(&View::default()), "newest lane {i}");
        }
        for k in (0..newest).rev() {
            // record that rebuilds state k from state k+1 was produced when
            // advancing INTO state k+1 (records[k+1]).
            depot::apply_mut(&mut cur, &records[k + 1]);
            for (i, want) in states[k].iter().enumerate() {
                let got = cur.as_ref().map(|u| extract(u, i)).unwrap_or_default();
                assert_eq!(&got, want.as_ref().unwrap_or(&View::default()), "state {k} lane {i}");
            }
        }
    }

    #[test]
    fn single_edit_round_trips() {
        assert_chain(&[vec![
            Some(dir(&[("a", leaf("a", "100644")), ("b", leaf("b", "100644"))])),
            Some(dir(&[("a", leaf("a", "100644")), ("b", leaf("B", "100644"))])),
        ]]);
    }

    #[test]
    fn in_place_edit_chain() {
        let s0 = vec![
            Some(dir(&[("shared", leaf("x", "100644")), ("f", leaf("old", "100644"))])),
            Some(dir(&[("shared", leaf("x", "100644")), ("f", leaf("l1-old", "100644"))])),
        ];
        let s1 = vec![
            Some(dir(&[("shared", leaf("x", "100644")), ("f", leaf("old", "100644"))])),
            Some(dir(&[("shared", leaf("x", "100644")), ("f", leaf("l1-new", "100644"))])),
        ];
        assert_chain(&[s0, s1]);
    }

    #[test]
    fn add_remove_paths_and_lane_death() {
        let s0 = vec![
            Some(dir(&[("keep", leaf("k", "100644")), ("gone", dir(&[("f", leaf("f", "100644"))]))])),
            Some(dir(&[("keep", leaf("k", "100644")), ("gone", dir(&[("f", leaf("f", "100644"))]))])),
        ];
        let s1 = vec![
            Some(dir(&[("keep", leaf("k", "100644")), ("added", leaf("a", "100644"))])),
            None, // lane 1 dies
        ];
        assert_chain(&[s0, s1]);
    }

    #[test]
    fn file_becomes_dir_across_states() {
        let s0 = vec![Some(dir(&[("x", leaf("i am a file", "100644"))]))];
        let s1 = vec![Some(dir(&[("x", dir(&[("in", leaf("dir entry", "100644"))]))]))];
        assert_chain(&[s0, s1]);
    }

    #[test]
    fn many_lanes_and_variants() {
        let mk = |tag: &str| -> Vec<Option<View>> {
            (0..6)
                .map(|i| {
                    Some(dir(&[
                        ("common", leaf("shared", "100644")),
                        ("per", leaf(&format!("{tag}-{i}"), "100644")),
                    ]))
                })
                .collect()
        };
        assert_chain(&[mk("a"), mk("b"), mk("c")]);
    }
}
