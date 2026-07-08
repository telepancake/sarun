//! The commit/tree/lane encoder: turns a sequence of parallel-lane states
//! into the union frame chain, driving the per-path [`crate::reslot`] unit
//! for stable slot assignment. It holds a persistent **skeleton** — the
//! union structure as slot state per path — and mutates it one revision at
//! a time, emitting the depot delta from the slot changes. No combined
//! tree is materialized and no variant is re-keyed by position: a slot is
//! stable across edits, so an in-place file change is a content replacement
//! at an unchanged key and a lane joining a variant moves only a bitmap.
//!
//! A variant's identity here is the pair `(attrs, content)` (Arc-backed, so
//! its bytes are shared, not copied), which is exactly what a `\0v`/`\0m`
//! frame node needs — no separate content lookup, no id hashing.
//!
//! Two outputs, matching the reverse-delta chain the depot stores:
//!   * [`Encoder::advance`] moves the skeleton to a new lane state and
//!     returns the REVERSE delta (rebuild the previous state from the new) —
//!     each older chain record. Its slot `before`s carry the old content.
//!   * [`Encoder::full`] is the FORWARD delta building the current state
//!     from empty — the `f0` head and every seal boundary.

use std::collections::{BTreeMap, BTreeSet};

use depot::{Attrs, Bytes, Layer, Name, Node, View};

use crate::reslot::{Bitmap, Occupant, Slots};
use crate::variants::{content_key, content_view, dir_names, leaf_delta, meta_key, meta_view, sub_at};

/// A variant's identity: its file attrs and content. The `\0v` blob is
/// `.1`, the `\0m` attrs are `.0`; ordering (for the reslot BTree) is by
/// attrs then content.
type VarKey = (Attrs, Bytes);

/// The persistent union skeleton at one path: the file variants as slots,
/// plus subdirectory children. Both coexist (a path that is a file in some
/// lanes and a directory in others).
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

/// The path's new variant set: group the file-lanes present here by
/// `(attrs, content)`, accumulating each variant's lane bitmap.
fn group(lanes: &[Option<&View>]) -> BTreeMap<VarKey, Bitmap> {
    let mut by_key: BTreeMap<VarKey, Bitmap> = BTreeMap::new();
    for (i, v) in lanes.iter().enumerate() {
        if let Some(v) = v {
            if let Some(content) = &v.blob {
                set_bit(by_key.entry((v.attrs.clone(), content.clone())).or_default(), i);
            }
        }
    }
    by_key
}

fn occ_content(o: &Occupant<VarKey>) -> View {
    content_view(&o.id.1)
}
fn occ_meta(o: &Occupant<VarKey>) -> View {
    meta_view(&o.id.0, &o.bitmap)
}

/// Reverse-delta node at one path: reslot the skeleton to the new lane
/// state and turn each slot change into the delta that rebuilds the OLD
/// occupant from the NEW one (`leaf_delta(base = new, target = old)`).
/// Recurses into directory children; prunes skeleton nodes that empty out.
fn advance_node(node: &mut Skel, lanes: &[Option<&View>]) -> Node {
    let mut out = Node::keep();

    let new_vars = group(lanes);
    for ch in node.slots.reslot(&new_vars) {
        let base_c = ch.after.as_ref().map(occ_content);
        let tgt_c = ch.before.as_ref().map(occ_content);
        let c = leaf_delta(base_c.as_ref(), tgt_c.as_ref());
        if !c.is_identity() {
            out.children.insert(content_key(ch.slot), c);
        }
        let base_m = ch.after.as_ref().map(occ_meta);
        let tgt_m = ch.before.as_ref().map(occ_meta);
        let m = leaf_delta(base_m.as_ref(), tgt_m.as_ref());
        if !m.is_identity() {
            out.children.insert(meta_key(ch.slot), m);
        }
    }

    let mut names: BTreeSet<Name> = node.children.keys().cloned().collect();
    names.extend(dir_names(lanes));
    for name in names {
        let sub = sub_at(lanes, &name);
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

/// Forward full node: build the current skeleton state from the empty base
/// (`leaf_delta(None, current)` per slot). The `f0`/seal head.
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

/// The stateful union encoder. `lanes[i]` = lane `i`'s git tree (or `None`).
#[derive(Default)]
pub struct Encoder {
    root: Skel,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder::default()
    }

    /// Move the skeleton to `lanes` and return the reverse delta rebuilding
    /// the PREVIOUS state from it — each older chain record. At the very
    /// first revision the return is a full tombstone (there is no previous)
    /// and the caller discards it in favour of [`full`](Self::full).
    pub fn advance(&mut self, lanes: &[Option<&View>]) -> Layer {
        Layer { root: advance_node(&mut self.root, lanes) }
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
    use std::sync::Arc;

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
    fn refs(lanes: &[Option<View>]) -> Vec<Option<&View>> {
        lanes.iter().map(Option::as_ref).collect()
    }

    /// Drive a sequence of states through the encoder exactly as the store
    /// does (f0 = full of the newest; each older record = advance to the
    /// older), then reconstruct every state newest-first and extract each
    /// lane, checking it equals the input.
    fn assert_chain(states: &[Vec<Option<View>>]) {
        let mut enc = Encoder::new();
        // Encode forward, remembering each step's reverse record.
        let mut records: Vec<Layer> = Vec::new(); // record[k] rebuilds state k from k+1
        for (i, st) in states.iter().enumerate() {
            let rev = enc.advance(&refs(st));
            if i > 0 {
                records.push(rev); // rebuilds states[i-1] from states[i]
            }
        }
        let f0 = enc.full(); // newest full
        // Reconstruct newest-first: apply f0, then each record in reverse.
        let mut cur = depot::apply(None, &f0);
        let newest = states.len() - 1;
        for (i, want) in states[newest].iter().enumerate() {
            let got = cur.as_ref().map(|u| extract(u, i)).unwrap_or_default();
            assert_eq!(&got, want.as_ref().unwrap_or(&View::default()), "newest lane {i}");
        }
        for k in (0..newest).rev() {
            depot::apply_mut(&mut cur, &records[k]);
            for (i, want) in states[k].iter().enumerate() {
                let got = cur.as_ref().map(|u| extract(u, i)).unwrap_or_default();
                assert_eq!(&got, want.as_ref().unwrap_or(&View::default()), "state {k} lane {i}");
            }
        }
    }

    #[test]
    fn single_state_round_trips() {
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
    fn add_remove_paths_and_lane_absent() {
        let s0 = vec![
            Some(dir(&[("keep", leaf("k", "100644")), ("gone", dir(&[("f", leaf("f", "100644"))]))])),
            None,
        ];
        let s1 = vec![
            Some(dir(&[("keep", leaf("k", "100644")), ("added", leaf("a", "100644"))])),
            Some(dir(&[("keep", leaf("k", "100644")), ("added", leaf("a", "100644"))])),
        ];
        assert_chain(&[s0, s1]);
    }

    #[test]
    fn many_lanes_and_variants() {
        let mk = |tag: &str| {
            let lanes: Vec<Option<View>> = (0..6)
                .map(|i| {
                    Some(dir(&[
                        ("common", leaf("shared", "100644")),
                        ("per", leaf(&format!("{tag}-{i}"), "100644")),
                    ]))
                })
                .collect();
            lanes
        };
        assert_chain(&[mk("a"), mk("b"), mk("c")]);
    }
}
