//! The streaming byte-level algebra (`depot::stream`) is proven here to be
//! byte-for-byte identical to the in-memory reference (`compose` / `diff`)
//! — the whole point of the two functions is that a driver may operate on
//! canonical frames without ever materializing a `View`.
//!
//!   compose_stream(encode(a), encode(b)) == encode(compose(a, b))
//!   diff_stream(encode(diff(None,va)), encode(diff(None,vb)))
//!                                       == encode(diff(va, vb))
//!
//! checked on hand-built corner fixtures and on the shared randomized
//! corpus (plain and backdrop-anchored), plus the end-to-end laws the
//! bytes are supposed to satisfy.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{random_layer, random_layer_anchored, Rng};
use depot::codec::{decode, encode};
use depot::stream::{compose_stream, diff_stream, overlay_full};
use depot::{apply, compose, diff, resolve, Attrs, BlobOp, Layer, Node, View};

// ------------------------------------------------------------- helpers

fn compose_bytes(a: &Layer, b: &Layer) -> Vec<u8> {
    let (ea, eb) = (encode(a), encode(b));
    let mut out = Vec::new();
    compose_stream(&ea, &eb, &mut out).expect("compose_stream");
    out
}

fn diff_bytes(a: &Layer, b: &Layer) -> Vec<u8> {
    let (ea, eb) = (encode(a), encode(b));
    let mut out = Vec::new();
    diff_stream(&ea, &eb, &mut out).expect("diff_stream");
    out
}

/// Streamed compose equals the reference compose, byte for byte — and the
/// bytes decode back to that same layer.
fn check_compose(a: &Layer, b: &Layer, ctx: &str) {
    let want = encode(&compose(a, b));
    let got = compose_bytes(a, b);
    assert_eq!(got, want, "compose_stream bytes diverge: {ctx}");
    assert_eq!(decode(&got).expect("decode"), compose(a, b), "compose_stream decode: {ctx}");
}

// ------------------------------------------------------- compose corners

#[test]
fn compose_corner_fixtures() {
    let n = |b: &[u8]| b.to_vec();
    let set = |b: &[u8]| Node { blob: BlobOp::Set(b.into()), ..Node::keep() };
    let kids = |mut node: Node, kk: Vec<(&[u8], Node)>| {
        for (k, v) in kk {
            node.children.insert(n(k), v);
        }
        node
    };
    let layer = |root: Node| Layer { root };

    // Plain override + add + inherit.
    let a = layer(kids(set(b"root"), vec![(b"keep", set(b"K")), (b"x", set(b"X"))]));
    let b = layer(kids(Node::keep(), vec![(b"x", set(b"X2")), (b"y", set(b"Y"))]));
    check_compose(&a, &b, "override+add");

    // b tombstones a child.
    let b2 = layer(kids(Node::keep(), vec![(b"keep", Node::tombstone())]));
    check_compose(&a, &b2, "tombstone");

    // Recreate over a tombstone: a deletes, b re-adds (harden path).
    let a3 = layer(kids(Node::keep(), vec![(b"g", Node::tombstone())]));
    let b3 = layer(kids(Node::keep(), vec![(b"g", set(b"fresh"))]));
    check_compose(&a3, &b3, "recreate-over-tombstone");
    // ... and a recreate that sets nothing (harden → tombstone).
    let b3b = layer(kids(Node::keep(), vec![(b"g", Node::keep())]));
    check_compose(&a3, &b3b, "recreate-nothing");

    // b is opaque: masks a's children, hardens its own.
    let mut op = kids(Node::keep(), vec![(b"z", set(b"Z"))]);
    op.opaque = true;
    let b4 = layer(kids(Node::keep(), vec![(b"d", op)]));
    let a4 = layer(kids(Node::keep(), vec![(b"d", kids(Node::keep(), vec![(b"old", set(b"O"))]))]));
    check_compose(&a4, &b4, "opaque-mask");

    // b re-bases a name on the backdrop (hole / restoration).
    let b5 = layer(kids(Node::keep(), vec![(b"keep", Node::hole())]));
    check_compose(&a, &b5, "backdrop-hole");

    // Empty-directory survival through compose.
    let empty_dir = Node { attrs: Some(Attrs::new()), ..Node::keep() };
    let a6 = layer(kids(Node::keep(), vec![(b"e", empty_dir.clone())]));
    let b6 = layer(kids(Node::keep(), vec![(b"f", set(b"F"))]));
    check_compose(&a6, &b6, "empty-dir");
}

/// Regression for the two-sided-collapse corner: a child that composes to
/// the identity delta even though neither side encodes as `[0,0]` — one
/// side is identity, the other a facetless keep whose only child is itself
/// identity (and so is dropped, leaving an empty keep). `compose_node`
/// prunes the whole child; the streaming merge must too.
#[test]
fn compose_two_sided_collapse_to_identity() {
    let n = |b: &[u8]| b.to_vec();
    let kids = |kk: Vec<(&[u8], Node)>| {
        let mut node = Node::keep();
        for (k, v) in kk {
            node.children.insert(n(k), v);
        }
        node
    };
    // a: child c = identity keep. b: child c = keep holding an identity
    // grandchild g. compose(a,b).c = keep with g dropped = identity → the
    // whole c must vanish, leaving the empty root.
    let a = Layer { root: kids(vec![(b"c", Node::keep())]) };
    let b = Layer { root: kids(vec![(b"c", kids(vec![(b"g", Node::keep())]))]) };
    check_compose(&a, &b, "two-sided-collapse");
    assert_eq!(compose_bytes(&a, &b), encode(&Layer::empty()), "did not collapse to empty root");

    // Deeper: c/d both facetless keeps that cancel through two levels.
    let a2 = Layer { root: kids(vec![(b"c", kids(vec![(b"d", Node::keep())]))]) };
    let b2 = Layer { root: kids(vec![(b"c", kids(vec![(b"d", kids(vec![(b"e", Node::keep())]))]))]) };
    check_compose(&a2, &b2, "two-sided-collapse-deep");

    // Control: a surviving facet anywhere keeps the child (must NOT prune).
    let a3 = Layer { root: kids(vec![(b"c", Node::keep())]) };
    let set = Node { blob: BlobOp::Set(b"x".to_vec().into()), ..Node::keep() };
    let b3 = Layer { root: kids(vec![(b"c", kids(vec![(b"g", set)]))]) };
    check_compose(&a3, &b3, "two-sided-survivor");
    assert_ne!(compose_bytes(&a3, &b3), encode(&Layer::empty()), "wrongly pruned a survivor");
}

#[test]
fn compose_matches_reference_randomized() {
    for seed in 1..400u64 {
        let mut rng = Rng(seed);
        let a = random_layer(&mut rng);
        let b = random_layer(&mut rng);
        check_compose(&a, &b, &format!("plain seed {seed}"));
    }
}

#[test]
fn compose_matches_reference_anchored() {
    for seed in 1..400u64 {
        let mut rng = Rng(seed ^ 0xa9c);
        let a = random_layer_anchored(&mut rng);
        let b = random_layer_anchored(&mut rng);
        check_compose(&a, &b, &format!("anchored seed {seed}"));
    }
}

/// Composition is associative on the bytes, exactly as on the trees: the
/// streamed left- and right-nestings agree with the reference.
#[test]
fn compose_stream_is_associative() {
    for seed in 1..200u64 {
        let mut rng = Rng(seed ^ 0x5e);
        let a = random_layer(&mut rng);
        let b = random_layer(&mut rng);
        let c = random_layer(&mut rng);
        // (a∘b)∘c via streaming, compared to the reference tree compose.
        let ab = decode(&compose_bytes(&a, &b)).unwrap();
        let left = compose_bytes(&ab, &c);
        assert_eq!(left, encode(&compose(&compose(&a, &b), &c)), "assoc left, seed {seed}");
        let bc = decode(&compose_bytes(&b, &c)).unwrap();
        let right = compose_bytes(&a, &bc);
        assert_eq!(right, encode(&compose(&a, &compose(&b, &c))), "assoc right, seed {seed}");
    }
}

// ---------------------------------------------------------- diff corners

/// Streamed diff of two full-state records equals the reference diff of
/// their views, byte for byte, and satisfies the turn-first-into-second
/// law under both `apply` and the streamed compose.
fn check_diff(va: &View, vb: &View, ctx: &str) {
    let fa = diff(None, Some(va)); // positive full-state records
    let fb = diff(None, Some(vb));
    let want = encode(&diff(Some(va), Some(vb)));
    let got = diff_bytes(&fa, &fb);
    assert_eq!(got, want, "diff_stream bytes diverge: {ctx}");
    // Law: apply(va, delta) == vb.
    let delta = decode(&got).expect("decode delta");
    assert_eq!(apply(Some(va), &delta).as_ref(), Some(vb), "diff law (apply): {ctx}");
    // Law via the streamed compose: compose(full_a, delta) resolves to vb.
    let composed = decode(&compose_bytes(&fa, &delta)).expect("decode composed");
    assert_eq!(apply(None, &composed).as_ref(), Some(vb), "diff law (compose): {ctx}");
}

#[test]
fn diff_corner_fixtures() {
    let n = |b: &[u8]| b.to_vec();
    let blob = |b: &[u8]| Arc::new(View { blob: Some(b.into()), ..View::default() });
    let empty = || Arc::new(View::default());

    let va = View {
        blob: None,
        attrs: Attrs::new(),
        children: BTreeMap::from([
            (n(b"same"), blob(b"s")),
            (n(b"changed"), blob(b"old")),
            (n(b"removed"), blob(b"r")),
            (n(b"emptydir"), empty()),
        ]),
    };
    let vb = View {
        blob: Some(n(b"rootnow").into()),
        attrs: Attrs::from([(n(b"k"), n(b"v"))]),
        children: BTreeMap::from([
            (n(b"same"), blob(b"s")),
            (n(b"changed"), blob(b"new")),
            (n(b"added"), blob(b"a")),
            (n(b"emptydir"), empty()), // survives unchanged
        ]),
    };
    check_diff(&va, &vb, "basic");
    check_diff(&vb, &va, "reverse");
    check_diff(&va, &va, "identity");

    // Empty-dir appearing / disappearing (the existence-witness corner).
    let with_e = View { children: BTreeMap::from([(n(b"e"), empty())]), ..View::default() };
    let without_e = View::default();
    check_diff(&without_e, &with_e, "empty-dir-appears");
    check_diff(&with_e, &without_e, "empty-dir-vanishes");
}

#[test]
fn diff_matches_reference_randomized() {
    let mut checked = 0;
    for seed in 1..600u64 {
        let mut rng = Rng(seed ^ 0xd1ff);
        let va = resolve(&[&random_layer(&mut rng)]);
        let vb = resolve(&[&random_layer(&mut rng)]);
        // diff_stream's contract is two present full-state views.
        if let (Some(va), Some(vb)) = (va, vb) {
            check_diff(&va, &vb, &format!("seed {seed}"));
            checked += 1;
        }
    }
    assert!(checked > 100, "too few randomized diff cases: {checked}");
}

// --------------------------------------------------------- overlay_full

/// Streamed overlay of `d` on full-state `base` equals the positive
/// full-state of the applied view, byte for byte — and never carries a
/// tombstone (it is a proper full-state).
fn check_overlay(base_view: &View, d: &Layer, ctx: &str) {
    let base = diff(None, Some(base_view)); // positive full-state record
    let want = encode(&diff(None, apply(Some(base_view), d).as_ref()));
    let mut got = Vec::new();
    overlay_full(&encode(&base), &encode(d), &mut got).expect("overlay_full");
    assert_eq!(got, want, "overlay_full bytes diverge: {ctx}");
    // The applied view matches, and the record is canonical (a fixpoint of
    // diff-from-nothing), so a seal of it needs no recompute.
    let applied = apply(None, &decode(&got).expect("decode"));
    assert_eq!(applied, apply(Some(base_view), d), "overlay view: {ctx}");
}

#[test]
fn overlay_corner_fixtures() {
    let n = |b: &[u8]| b.to_vec();
    let blob = |b: &[u8]| Arc::new(View { blob: Some(b.into()), ..View::default() });
    let empty = || Arc::new(View::default());
    let set = |b: &[u8]| Node { blob: BlobOp::Set(b.into()), ..Node::keep() };
    let kids = |mut node: Node, kk: Vec<(&[u8], Node)>| {
        for (k, v) in kk {
            node.children.insert(n(k), v);
        }
        node
    };

    let base = View {
        blob: None,
        attrs: Attrs::new(),
        children: BTreeMap::from([
            (n(b"keep"), blob(b"K")),
            (n(b"edit"), blob(b"old")),
            (n(b"del"), blob(b"D")),
            (n(b"edir"), empty()),
        ]),
    };
    // Edit one file, delete another, add a third — the delta a commit makes.
    let d = Layer {
        root: kids(
            Node::keep(),
            vec![
                (b"edit", set(b"new")),
                (b"del", Node::tombstone()), // deletion must VANISH, not tombstone
                (b"add", set(b"A")),
            ],
        ),
    };
    check_overlay(&base, &d, "edit+del+add");
    // Deleting the only-empty-dir; adding a nested tree.
    let d2 = Layer {
        root: kids(
            Node::keep(),
            vec![
                (b"edir", Node::tombstone()),
                (b"sub", kids(Node::keep(), vec![(b"x", set(b"X"))])),
            ],
        ),
    };
    check_overlay(&base, &d2, "del-emptydir+add-subtree");
    // An opaque delta node masks the base's children under that name.
    let base2 = View {
        children: BTreeMap::from([(
            n(b"d"),
            Arc::new(View { children: BTreeMap::from([(n(b"old"), blob(b"O"))]), ..View::default() }),
        )]),
        ..View::default()
    };
    let mut opq = kids(Node::keep(), vec![(b"z", set(b"Z"))]);
    opq.opaque = true;
    let d3 = Layer { root: kids(Node::keep(), vec![(b"d", opq)]) };
    check_overlay(&base2, &d3, "opaque-mask");
    // No-op delta leaves the full-state identical.
    check_overlay(&base, &Layer::empty(), "noop");
}

#[test]
fn overlay_matches_reference_randomized() {
    let mut checked = 0;
    for seed in 1..600u64 {
        let mut rng = Rng(seed ^ 0x0003);
        let base = resolve(&[&random_layer(&mut rng)]);
        let d = random_layer(&mut rng);
        if let Some(base) = base {
            check_overlay(&base, &d, &format!("seed {seed}"));
            checked += 1;
        }
    }
    assert!(checked > 100, "too few overlay cases: {checked}");
}

/// The depot updater's real loop, purely streaming: keep a positive
/// full-state, overlay each revision's delta to advance it, and derive the
/// stored reverse delta by diffing the new full-state against the old —
/// reconstruct every intermediate view exactly, no `View` held for the
/// frame.
#[test]
fn overlay_then_diff_is_the_updater_loop() {
    for seed in 1..200u64 {
        let mut rng = Rng(seed ^ 0x100f);
        // Seed the full-state from the first view.
        let mut view = match resolve(&[&random_layer(&mut rng)]) {
            Some(v) => v,
            None => continue,
        };
        let mut full = encode(&diff(None, Some(&view)));
        for step in 0..4 {
            let d = random_layer(&mut rng);
            let next = match apply(Some(&view), &d) {
                Some(v) => v,
                None => break, // whole state deleted; stop this chain
            };
            // Advance the full-state by streaming overlay.
            let mut new_full = Vec::new();
            overlay_full(&full, &encode(&d), &mut new_full).unwrap();
            assert_eq!(new_full, encode(&diff(None, Some(&next))), "overlay drift, seed {seed} step {step}");
            // The chain's reverse delta: rebuild the old full-state from the new.
            let mut rev = Vec::new();
            diff_stream(&new_full, &full, &mut rev).unwrap();
            assert_eq!(
                apply(Some(&next), &decode(&rev).unwrap()).as_ref(),
                Some(&view),
                "reverse delta wrong, seed {seed} step {step}"
            );
            full = new_full;
            view = next;
        }
    }
}

/// The two functions compose end-to-end the way the depot updater uses
/// them: a full-state, a delta produced by diff, streamed-composed back,
/// must reproduce the target view — with no tree ever materialized by the
/// primitives.
#[test]
fn compose_of_streamed_diff_round_trips() {
    for seed in 1..300u64 {
        let mut rng = Rng(seed ^ 0x11d);
        let va = resolve(&[&random_layer(&mut rng)]);
        let vb = resolve(&[&random_layer(&mut rng)]);
        if let (Some(va), Some(vb)) = (va, vb) {
            let fa = diff(None, Some(&va)); // positive full-state of va
            // Delta va→vb, produced purely by streaming over the two
            // full-state records.
            let mut delta = Vec::new();
            diff_stream(&encode(&fa), &encode(&diff(None, Some(&vb))), &mut delta).unwrap();
            // Overlay it back onto the full-state, again purely streaming.
            let mut composed = Vec::new();
            compose_stream(&encode(&fa), &delta, &mut composed).unwrap();
            // The overlay resolves to the target view — the end-to-end law,
            // with no `View` ever built by the two primitives.
            let view = apply(None, &decode(&composed).unwrap());
            assert_eq!(view.as_ref(), Some(&vb), "round-trip seed {seed}");
            // And the anchor the reader recomputes from the overlay (diff
            // from nothing over its view) is exactly the target's positive
            // full-state — the fixpoint the cold-frame seal depends on.
            assert_eq!(
                encode(&diff(None, view.as_ref())),
                encode(&diff(None, Some(&vb))),
                "recomputed anchor diverges, seed {seed}"
            );
        }
    }
}
