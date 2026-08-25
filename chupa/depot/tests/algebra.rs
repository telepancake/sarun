//! Layer-algebra laws and the DEPOT-DESIGN.md §6 corner fixtures.
//!
//! The two laws, checked both on hand-built fixtures and on randomized
//! stacks (xorshift-seeded, deterministic):
//!
//!   resolve(prefix ++ [squash(tail)]) == resolve(prefix ++ tail)
//!   apply(base, diff(base, target)) == target
//!
//! plus the derived-rotation equivalences.

mod common;

use std::collections::BTreeMap;

use common::{random_layer, Rng};
use depot::{
    apply, compose, diff, resolve, resolve_over, rotate, squash, Anchor, Attrs, BlobOp, Layer,
    Node, Presence, View,
};

/// A couple of distinct backdrops for the "for all backdrops" laws: the
/// empty filesystem and two host-ish trees that disagree everywhere.
fn backdrops() -> Vec<Option<View>> {
    let host1 = layer(with_children(
        set(b"hostroot"),
        vec![
            (b"keep", set(b"host-K")),
            (b"overwritten", set(b"host-OLD")),
            (b"hostonly", set(b"H")),
            (b"dir", with_children(live(), vec![(b"hx", set(b"HX"))])),
        ],
    ));
    let host2 = layer(with_children(
        live(),
        vec![(b"overwritten", set(b"host2")), (b"other", set(b"O"))],
    ));
    vec![None, apply(None, &host1), apply(None, &host2)]
}

fn assert_rotation_laws(ancestors: &[&Layer], a: &Layer, b: &Layer, ctx: &str) {
    let (b_new, a_new) = rotate(ancestors, a, b);
    let mut anc_b: Vec<&Layer> = ancestors.to_vec();
    anc_b.push(&b_new);
    let mut anc_ab: Vec<&Layer> = ancestors.to_vec();
    anc_ab.extend([a, b]);
    let mut anc_ba: Vec<&Layer> = anc_b.clone();
    anc_ba.push(&a_new);
    let mut anc_a: Vec<&Layer> = ancestors.to_vec();
    anc_a.push(a);
    for (i, bd) in backdrops().iter().enumerate() {
        assert_eq!(
            resolve_over(bd.as_ref(), &anc_b),
            resolve_over(bd.as_ref(), &anc_ab),
            "{ctx}: parent view broken over backdrop {i}"
        );
        assert_eq!(
            resolve_over(bd.as_ref(), &anc_ba),
            resolve_over(bd.as_ref(), &anc_a),
            "{ctx}: child view broken over backdrop {i}"
        );
    }
}

// ------------------------------------------------------------- builders

fn n(names: &[u8]) -> Vec<u8> {
    names.to_vec()
}

fn live() -> Node {
    Node::keep()
}

fn set(bytes: &[u8]) -> Node {
    Node { blob: BlobOp::Set(bytes.into()), ..Node::keep() }
}

fn with_children(mut node: Node, kids: Vec<(&[u8], Node)>) -> Node {
    for (k, v) in kids {
        node.children.insert(n(k), v);
    }
    node
}

fn layer(root: Node) -> Layer {
    Layer { root }
}

fn view_blob(bytes: &[u8]) -> std::sync::Arc<View> {
    std::sync::Arc::new(View { blob: Some(bytes.into()), ..View::default() })
}

// --------------------------------------------- explicit empty directories

/// An empty directory — `{blob:None, attrs:{}, children:{}}` — is now a
/// first-class, canonical [`View`], DISTINCT from absence. It must survive
/// every operation (diff→apply, compose, squash, codec) byte/structure
/// exactly, and be encoded differently from "not there at all".
#[test]
fn explicit_empty_directories() {
    use depot::codec::{decode, encode};
    use std::sync::Arc;

    let empty = || Arc::new(View::default());
    // A tree with: a top-level empty dir `e`; `a/b` where `b` is empty
    // (and `a` non-empty, holding it); a top-level empty dir `c`; a real
    // file `f`; and `g` holding a real file `h` beside an empty dir `i`.
    let target = View {
        blob: None,
        attrs: Attrs::new(),
        children: BTreeMap::from([
            (n(b"a"), Arc::new(View { children: BTreeMap::from([(n(b"b"), empty())]), ..View::default() })),
            (n(b"c"), empty()),
            (n(b"e"), empty()),
            (n(b"f"), view_blob(b"data")),
            (n(b"g"), Arc::new(View {
                children: BTreeMap::from([(n(b"h"), view_blob(b"x")), (n(b"i"), empty())]),
                ..View::default()
            })),
        ]),
    };

    // 1. diff→apply round-trips the empty dirs, from nothing and from a base.
    let from_nothing = diff(None, Some(&target));
    assert_eq!(apply(None, &from_nothing).as_ref(), Some(&target), "diff(None) lost an empty dir");
    let base = View { children: BTreeMap::from([(n(b"f"), view_blob(b"old")), (n(b"z"), empty())]), ..View::default() };
    let d = diff(Some(&base), Some(&target));
    assert_eq!(apply(Some(&base), &d).as_ref(), Some(&target), "diff(base) lost an empty dir");

    // 2. compose / squash preserve the empty dirs.
    assert_eq!(apply(None, &squash(&[&from_nothing])).as_ref(), Some(&target), "squash lost an empty dir");
    let composed = compose(&Layer::empty(), &from_nothing);
    assert_eq!(apply(None, &composed).as_ref(), Some(&target), "compose lost an empty dir");
    // Two-step: seed the base, then compose the base→target delta on top.
    let seed = diff(None, Some(&base));
    let combined = compose(&seed, &d);
    assert_eq!(apply(None, &combined).as_ref(), Some(&target), "compose(seed,delta) lost an empty dir");

    // 3. codec encode→decode is exact, and re-encoding is byte-identical.
    let bytes = encode(&from_nothing);
    let back = decode(&bytes).expect("decode");
    assert_eq!(back, from_nothing, "codec changed the layer");
    assert_eq!(encode(&back), bytes, "codec not deterministic");

    // 4. An empty-present node and an absent node are DISTINCT: different
    //    diff, different canonical bytes.
    let with_x = View { children: BTreeMap::from([(n(b"x"), empty())]), ..View::default() };
    let without_x = View::default(); // empty ROOT, but no child `x`
    let present = diff(None, Some(&with_x));
    let absent = diff(None, Some(&without_x));
    assert_ne!(present, absent, "empty-present and absent produced the same delta");
    assert_ne!(encode(&present), encode(&absent), "empty-present and absent encode identically");
    assert_eq!(apply(None, &present).as_ref(), Some(&with_x));
    assert_eq!(apply(None, &absent).as_ref(), Some(&View::default()), "empty ROOT view must itself be representable");
    assert!(apply(None, &present).unwrap().children.contains_key(&n(b"x")));

    // 5. A pure no-op keep over an ABSENT base still yields absence: a
    //    `Keep`/inherit node asserts nothing, so keep-over-nothing stays
    //    nothing (NOT a spurious empty node).
    let noop = layer(with_children(live(), vec![(b"x", live())]));
    assert_eq!(apply(None, &noop), None, "keep-over-absent must stay absent");
    // Whereas an explicit empty-dir assertion (replace attrs with the
    // empty map) DOES materialize the empty node.
    let assert_empty = layer(with_children(
        live(),
        vec![(b"x", Node { attrs: Some(Attrs::new()), ..Node::keep() })],
    ));
    let got = apply(None, &assert_empty).expect("empty-dir assertion must materialize");
    let x = &got.children[&n(b"x")];
    assert!(x.blob.is_none() && x.attrs.is_empty() && x.children.is_empty(), "explicit empty dir must exist and be empty");
}

// ------------------------------------------------------------ the laws

fn assert_squash_law(stack: &[&Layer]) {
    let direct = resolve(stack);
    // Squash every contiguous tail and compare.
    for split in 0..stack.len() {
        let squashed = squash(&stack[split..]);
        let mut prefix: Vec<&Layer> = stack[..split].to_vec();
        prefix.push(&squashed);
        assert_eq!(
            resolve(&prefix),
            direct,
            "squash law broken at split {split}"
        );
    }
}

fn assert_diff_law(base: Option<&View>, target: &View) {
    let d = diff(base, Some(target));
    assert_eq!(apply(base, &d).as_ref(), Some(target), "diff law broken");
}

// ------------------------------------------------------- basic resolve

#[test]
fn resolve_override_and_inherit() {
    let lower = layer(with_children(
        set(b"rootblob"),
        vec![(b"a", set(b"A1")), (b"b", set(b"B1"))],
    ));
    let upper = layer(with_children(live(), vec![(b"a", set(b"A2"))]));
    let v = resolve(&[&lower, &upper]).unwrap();
    assert_eq!(v.blob.as_deref(), Some(&b"rootblob"[..]));
    assert_eq!(v.children[&n(b"a")].blob.as_deref(), Some(&b"A2"[..]));
    assert_eq!(v.children[&n(b"b")].blob.as_deref(), Some(&b"B1"[..]));
}

#[test]
fn tombstone_masks_lower() {
    let lower = layer(with_children(live(), vec![(b"a", set(b"A1"))]));
    let upper = layer(with_children(live(), vec![(b"a", Node::tombstone())]));
    // Canonical form: with its only child masked the root is empty, and
    // empty nodes do not exist.
    assert_eq!(resolve(&[&lower, &upper]), None);
}

#[test]
fn opaque_masks_children_not_blob() {
    let lower = layer(with_children(
        set(b"keepme"),
        vec![(b"a", set(b"A1")), (b"b", set(b"B1"))],
    ));
    let mut op = with_children(live(), vec![(b"c", set(b"C2"))]);
    op.opaque = true;
    let upper = layer(op);
    let v = resolve(&[&lower, &upper]).unwrap();
    // opaque masks lower children only; the node's own blob inherits.
    assert_eq!(v.blob.as_deref(), Some(&b"keepme"[..]));
    assert_eq!(v.children.len(), 1);
    assert_eq!(v.children[&n(b"c")].blob.as_deref(), Some(&b"C2"[..]));
}

#[test]
fn interior_node_blob_and_children() {
    // A node carrying BOTH bytes and children — the git-superset case.
    let l = layer(with_children(
        live(),
        vec![(b"dir", with_children(set(b"dirblob"), vec![(b"leaf", set(b"x"))]))],
    ));
    let v = resolve(&[&l]).unwrap();
    let dir = &v.children[&n(b"dir")];
    assert_eq!(dir.blob.as_deref(), Some(&b"dirblob"[..]));
    assert_eq!(dir.children[&n(b"leaf")].blob.as_deref(), Some(&b"x"[..]));
}

// --------------------------------------------------- §6 corner fixtures

#[test]
fn corner_interior_blob_removed_children_kept() {
    let lower = layer(with_children(
        live(),
        vec![(b"dir", with_children(set(b"bytes"), vec![(b"kid", set(b"k"))]))],
    ));
    let upper = layer(with_children(
        live(),
        vec![(b"dir", Node { blob: BlobOp::Remove, ..Node::keep() })],
    ));
    let v = resolve(&[&lower, &upper]).unwrap();
    let dir = &v.children[&n(b"dir")];
    assert_eq!(dir.blob, None);
    assert_eq!(dir.children[&n(b"kid")].blob.as_deref(), Some(&b"k"[..]));
    assert_squash_law(&[&lower, &upper]);
}

#[test]
fn corner_metadata_only_change() {
    let mut attrs = Attrs::new();
    attrs.insert(n(b"mode"), n(b"0644"));
    let lower = layer(with_children(
        live(),
        vec![(b"f", Node { blob: BlobOp::Set(n(b"data").into()), attrs: Some(attrs), ..Node::keep() })],
    ));
    let mut attrs2 = Attrs::new();
    attrs2.insert(n(b"mode"), n(b"0755"));
    let upper = layer(with_children(
        live(),
        vec![(b"f", Node { attrs: Some(attrs2.clone()), ..Node::keep() })],
    ));
    let v = resolve(&[&lower, &upper]).unwrap();
    let f = &v.children[&n(b"f")];
    assert_eq!(f.blob.as_deref(), Some(&b"data"[..]));
    assert_eq!(f.attrs, attrs2);
    assert_squash_law(&[&lower, &upper]);
}

#[test]
fn corner_recreate_over_tombstone_composes_opaque() {
    // deepest: /a/kid exists.
    let deep = layer(with_children(
        live(),
        vec![(b"a", with_children(set(b"old"), vec![(b"kid", set(b"deep"))]))],
    ));
    // middle: /a tombstoned.
    let mid = layer(with_children(live(), vec![(b"a", Node::tombstone())]));
    // top: /a recreated with BlobOp::Keep (inherits nothing — the
    // tombstone below it killed the blob).
    let top = layer(with_children(live(), vec![(b"a", live())]));

    // Squashing mid+top must still mask `deep` when stacked over it.
    assert_squash_law(&[&deep, &mid, &top]);
    // The recreate sets nothing, so it materializes nothing: `a` must not
    // reappear, and nothing from below it may resurrect.
    assert_eq!(resolve(&[&deep, &mid, &top]), None);

    // A recreate that DOES set something masks the old content but exists.
    let top2 = layer(with_children(live(), vec![(b"a", set(b"fresh"))]));
    assert_squash_law(&[&deep, &mid, &top2]);
    let v = resolve(&[&deep, &mid, &top2]).unwrap();
    let a = &v.children[&n(b"a")];
    assert_eq!(a.blob.as_deref(), Some(&b"fresh"[..]));
    assert!(a.children.is_empty(), "recreate must not resurrect old children");
}

#[test]
fn corner_tombstone_of_tombstone() {
    let deep = layer(with_children(live(), vec![(b"a", set(b"x"))]));
    let mid = layer(with_children(live(), vec![(b"a", Node::tombstone())]));
    let top = layer(with_children(live(), vec![(b"a", Node::tombstone())]));
    assert_squash_law(&[&deep, &mid, &top]);
    assert_eq!(resolve(&[&deep, &mid, &top]), None);
    // Partial squash of the two tombstones must KEEP the mask.
    let sq = squash(&[&mid, &top]);
    assert_eq!(resolve(&[&deep, &sq]), None);
}

#[test]
fn corner_partial_squash_keeps_tombstones() {
    let base = layer(with_children(live(), vec![(b"gone", set(b"g"))]));
    let l1 = layer(with_children(live(), vec![(b"gone", Node::tombstone())]));
    let l2 = layer(with_children(live(), vec![(b"new", set(b"n"))]));
    let sq = squash(&[&l1, &l2]);
    // Squash-of-partial-stack semantics: the tombstone survives.
    let v = resolve(&[&base, &sq]).unwrap();
    assert!(!v.children.contains_key(&n(b"gone")));
    assert!(v.children.contains_key(&n(b"new")));
}

#[test]
fn corner_opaque_then_additions_compose() {
    let deep = layer(with_children(
        live(),
        vec![(b"d", with_children(live(), vec![(b"x", set(b"X")), (b"y", set(b"Y"))]))],
    ));
    let mut op = with_children(live(), vec![(b"z", set(b"Z"))]);
    op.opaque = true;
    let mid = layer(with_children(live(), vec![(b"d", op)]));
    // top adds a child that, under the opaque mid, must not see deep's x.
    let top = layer(with_children(
        live(),
        vec![(b"d", with_children(live(), vec![(b"x", live())]))],
    ));
    assert_squash_law(&[&deep, &mid, &top]);
    let v = resolve(&[&deep, &mid, &top]).unwrap();
    let d = &v.children[&n(b"d")];
    assert_eq!(d.children[&n(b"z")].blob.as_deref(), Some(&b"Z"[..]));
    // top's `x` is a pure-inherit node over the opaque mask: it sees
    // nothing and sets nothing, so it materializes nothing
    // (inherit-absence rule).
    assert!(!d.children.contains_key(&n(b"x")));
    assert!(!d.children.contains_key(&n(b"y")));
}

// ------------------------------------------------------------ diff law

#[test]
fn diff_roundtrip_basics() {
    let base = View {
        blob: None,
        attrs: Attrs::new(),
        children: BTreeMap::from([
            (n(b"same"), view_blob(b"s")),
            (n(b"changed"), view_blob(b"old")),
            (n(b"removed"), view_blob(b"r")),
        ]),
    };
    let target = View {
        blob: Some(n(b"rootnow").into()),
        attrs: Attrs::from([(n(b"k"), n(b"v"))]),
        children: BTreeMap::from([
            (n(b"same"), view_blob(b"s")),
            (n(b"changed"), view_blob(b"new")),
            (n(b"added"), view_blob(b"a")),
        ]),
    };
    assert_diff_law(Some(&base), &target);
    assert_diff_law(None, &target);
    assert_diff_law(Some(&target), &base);

    // Minimality spot-checks: unchanged child absent from the delta,
    // removed child is a tombstone.
    let d = diff(Some(&base), Some(&target));
    assert!(!d.root.children.contains_key(&n(b"same")));
    assert_eq!(d.root.children[&n(b"removed")].presence, Presence::Tombstone);
    assert_eq!(d.root.children[&n(b"changed")].blob, BlobOp::Set(n(b"new").into()));
}

// ------------------------------------------------------------- rotation

#[test]
fn rotation_preserves_both_views() {
    // Parent A: a base tree. Child B: adds, overwrites, deletes, opaques.
    let a = layer(with_children(
        set(b"root"),
        vec![
            (b"keep", set(b"K")),
            (b"overwritten", set(b"OLD")),
            (b"deleted", set(b"D")),
            (b"dir", with_children(live(), vec![(b"x", set(b"X")), (b"y", set(b"Y"))])),
        ],
    ));
    let mut opdir = with_children(live(), vec![(b"z", set(b"Z"))]);
    opdir.opaque = true;
    let b = layer(with_children(
        live(),
        vec![
            (b"overwritten", set(b"NEW")),
            (b"deleted", Node::tombstone()),
            (b"added", set(b"A")),
            (b"dir", opdir),
        ],
    ));

    assert_rotation_laws(&[], &a, &b, "basic");
}

#[test]
fn rotation_opaque_inversion_relists_children() {
    // The §6 "opaque inversion" fixture: B opaques a dir; the inverse
    // layer must re-list the masked children explicitly and tombstone
    // B's additions.
    let a = layer(with_children(
        live(),
        vec![(b"d", with_children(live(), vec![(b"x", set(b"X")), (b"y", set(b"Y"))]))],
    ));
    let mut op = with_children(live(), vec![(b"z", set(b"Z"))]);
    op.opaque = true;
    let b = layer(with_children(live(), vec![(b"d", op)]));

    assert_rotation_laws(&[], &a, &b, "opaque-inversion");
    let (_b_new, a_new) = rotate(&[], &a, &b);

    // The inverse re-bases `d` on the backdrop (erasing B's opaque AND
    // B's z wholesale) and re-lists A's own children explicitly. The
    // backdrop's children under d reappear live — nothing snapshotted.
    let d = &a_new.root.children[&n(b"d")];
    assert_eq!(d.anchor, Anchor::Backdrop, "re-based, not layered over B");
    assert!(!d.opaque, "A never opaqued d");
    assert_eq!(d.children[&n(b"x")].blob, BlobOp::Set(n(b"X").into()));
    assert_eq!(d.children[&n(b"y")].blob, BlobOp::Set(n(b"Y").into()));
    assert!(!d.children.contains_key(&n(b"z")), "B's addition erased by the re-base");
}

#[test]
fn rotation_of_rotation_is_identity_on_views() {
    let a = layer(with_children(
        live(),
        vec![(b"f", set(b"F")), (b"g", set(b"G"))],
    ));
    let b = layer(with_children(
        live(),
        vec![(b"f", Node::tombstone()), (b"h", set(b"H"))],
    ));
    let (b1, a1) = rotate(&[], &a, &b);
    let (a2, b2) = rotate(&[], &b1, &a1);
    // Rotating back: both views restored, over every backdrop.
    for bd in backdrops() {
        assert_eq!(resolve_over(bd.as_ref(), &[&a2]),
                   resolve_over(bd.as_ref(), &[&a]));
        assert_eq!(resolve_over(bd.as_ref(), &[&a2, &b2]),
                   resolve_over(bd.as_ref(), &[&a, &b]));
    }
}

// -------------------------------------------------- randomized law check

#[test]
fn randomized_squash_and_compose_laws() {
    for seed in 1..200u64 {
        let mut rng = Rng(seed);
        let layers: Vec<Layer> = (0..4).map(|_| random_layer(&mut rng)).collect();
        let refs: Vec<&Layer> = layers.iter().collect();
        assert_squash_law(&refs);
        // Pairwise compose associativity via views: (a∘b)∘c == a∘(b∘c).
        let ab_c = compose(&compose(refs[0], refs[1]), refs[2]);
        let a_bc = compose(refs[0], &compose(refs[1], refs[2]));
        {
            let base_layer = &refs[3];
            let base = resolve(&[base_layer]);
            assert_eq!(
                apply(base.as_ref(), &ab_c),
                apply(base.as_ref(), &a_bc),
                "compose associativity broken for seed {seed}"
            );
        }
    }
}

#[test]
fn randomized_diff_and_rotation_laws() {
    for seed in 1..200u64 {
        let mut rng = Rng(seed);
        let a = random_layer(&mut rng);
        let b = random_layer(&mut rng);
        // diff law between two random views.
        let va = resolve(&[&a]);
        let vb = resolve(&[&b]);
        if let Some(target) = &vb {
            let d = diff(va.as_ref(), Some(target));
            assert_eq!(apply(va.as_ref(), &d).as_ref(), Some(target), "diff law, seed {seed}");
        }
        // rotation equivalences (Lower-only layers here; the anchored
        // randomized test covers holes).
        assert_rotation_laws(&[], &a, &b, &format!("plain seed {seed}"));
    }
}

#[test]
fn randomized_apply_mut_matches_apply() {
    // apply is the reference; apply_mut must agree on every intermediate
    // view of a folded stack (random_layer covers set/remove/tombstone/
    // opaque/attrs/interior-blob nodes).
    for seed in 1..300u64 {
        let mut rng = Rng(seed);
        let mut view: Option<View> = if rng.below(4) == 0 {
            None
        } else {
            resolve(&[&random_layer(&mut rng)])
        };
        for step in 0..4 {
            let layer = random_layer(&mut rng);
            let reference = apply(view.as_ref(), &layer);
            depot::apply_mut(&mut view, &layer);
            assert_eq!(view, reference, "apply_mut diverges, seed {seed} step {step}");
        }
    }
}

// ----------------------------------------------------- holes / backdrop

/// A hole is "this key is not occluded": the backdrop shows through LIVE
/// — the same encoding resolves differently over different backdrops.
#[test]
fn hole_reveals_live_backdrop() {
    let lower = layer(with_children(live(), vec![(b"overwritten", set(b"MINE"))]));
    let upper = layer(with_children(live(), vec![(b"overwritten", Node::hole())]));
    let bds = backdrops();
    // Over the empty backdrop: nothing there.
    assert_eq!(resolve_over(bds[0].as_ref(), &[&lower, &upper]), None);
    // Over host1 and host2: each backdrop's own bytes, unfrozen.
    let v1 = resolve_over(bds[1].as_ref(), &[&lower, &upper]).unwrap();
    assert_eq!(v1.children[&n(b"overwritten")].blob.as_deref(), Some(&b"host-OLD"[..]));
    let v2 = resolve_over(bds[2].as_ref(), &[&lower, &upper]).unwrap();
    assert_eq!(v2.children[&n(b"overwritten")].blob.as_deref(), Some(&b"host2"[..]));
}

/// A hole cancels recorded deletion too: the tombstone was occlusion, and
/// the hole says "not occluded".
#[test]
fn hole_cancels_tombstone() {
    let lower = layer(with_children(live(), vec![(b"overwritten", Node::tombstone())]));
    let upper = layer(with_children(live(), vec![(b"overwritten", Node::hole())]));
    let bds = backdrops();
    let v = resolve_over(bds[1].as_ref(), &[&lower, &upper]).unwrap();
    assert_eq!(v.children[&n(b"overwritten")].blob.as_deref(), Some(&b"host-OLD"[..]));
    assert_eq!(resolve_over(bds[0].as_ref(), &[&lower, &upper]), None);
}

/// Rotation where B changed something A never touched: the inverse is a
/// hole, and A's rotated view tracks the LIVE backdrop.
#[test]
fn rotation_holes_where_a_never_changed() {
    let a = layer(with_children(live(), vec![(b"keep", set(b"A-KEEP"))]));
    let b = layer(with_children(live(), vec![(b"overwritten", set(b"B-NEW"))]));
    assert_rotation_laws(&[], &a, &b, "hole-side");
    let (_b_new, a_new) = rotate(&[], &a, &b);
    let inv = &a_new.root.children[&n(b"overwritten")];
    assert_eq!(inv.anchor, Anchor::Backdrop);
    assert_eq!(inv.blob, BlobOp::Keep, "a hole, not frozen content");
}

/// Rotation where a GRANDPARENT recorded a change at the name B touched:
/// the older change is recorded data and gets replicated into the
/// inverse — holes never mean "skip N layers".
#[test]
fn rotation_replicates_grandparent_changes() {
    let g = layer(with_children(live(), vec![(b"overwritten", set(b"G-OLD"))]));
    let a = layer(with_children(live(), vec![(b"unrelated", set(b"A"))]));
    let b = layer(with_children(live(), vec![(b"overwritten", set(b"B-NEW"))]));
    assert_rotation_laws(&[&g], &a, &b, "grandparent-replication");
    let (_b_new, a_new) = rotate(&[&g], &a, &b);
    let inv = &a_new.root.children[&n(b"overwritten")];
    assert_eq!(inv.blob, BlobOp::Set(n(b"G-OLD").into()), "replicated, not holed");
    assert_eq!(inv.anchor, Anchor::Backdrop, "replica erases B's entry");
}

/// Randomized rotation laws over anchored layers (holes included), with
/// and without ancestors, across every backdrop.
#[test]
fn randomized_rotation_laws_with_holes() {
    for seed in 1..150u64 {
        let mut rng = Rng(seed ^ 0xda7a);
        let g = common::random_layer_anchored(&mut rng);
        let a = common::random_layer_anchored(&mut rng);
        let b = common::random_layer_anchored(&mut rng);
        assert_rotation_laws(&[], &a, &b, &format!("seed {seed} no-anc"));
        assert_rotation_laws(&[&g], &a, &b, &format!("seed {seed} anc"));
    }
}

/// Compose-then-apply consistency for anchored stacks: composing any
/// adjacent pair first never changes the resolved view.
#[test]
fn randomized_compose_consistency_with_holes() {
    for seed in 1..150u64 {
        let mut rng = Rng(seed ^ 0xc0);
        let layers: Vec<Layer> =
            (0..3).map(|_| common::random_layer_anchored(&mut rng)).collect();
        let all: Vec<&Layer> = layers.iter().collect();
        let ab = compose(all[0], all[1]);
        let bc = compose(all[1], all[2]);
        for bd in backdrops() {
            let direct = resolve_over(bd.as_ref(), &all);
            assert_eq!(
                resolve_over(bd.as_ref(), &[&ab, all[2]]),
                direct,
                "left-compose broke view, seed {seed}"
            );
            assert_eq!(
                resolve_over(bd.as_ref(), &[all[0], &bc]),
                direct,
                "right-compose broke view, seed {seed}"
            );
        }
    }
}

// ------------------------------------------------- sharing transparency

/// Rebuild a view with NO Arc sharing anywhere (fresh allocation per
/// node and per blob) — the "as if deep-copied" reference shape.
fn unshare(v: &View) -> View {
    View {
        blob: v.blob.as_deref().map(|b| b.to_vec().into()),
        attrs: v.attrs.clone(),
        children: v
            .children
            .iter()
            .map(|(k, c)| (k.clone(), std::sync::Arc::new(unshare(c))))
            .collect(),
    }
}

/// Arc sharing is representation, never meaning: over random stacks
/// built with the sharing-heavy path (`apply_mut` forking a common
/// ancestor), `diff` of the shared views — where the `Arc::ptr_eq`
/// fast path fires constantly — must produce the SAME Layer (and the
/// same canonical bytes) as `diff` of fully unshared deep copies.
#[test]
fn randomized_diff_ignores_sharing() {
    for seed in 1..300u64 {
        let mut rng = Rng(seed ^ 0x5a5a);
        let mut base: Option<View> = None;
        for _ in 0..3 {
            depot::apply_mut(&mut base, &random_layer(&mut rng));
        }
        // Fork the frontier twice (cheap Arc clones), advance each side.
        let mut a = base.clone();
        let mut b = base.clone();
        depot::apply_mut(&mut a, &random_layer(&mut rng));
        for _ in 0..2 {
            depot::apply_mut(&mut b, &random_layer(&mut rng));
        }
        let ua = a.as_ref().map(unshare);
        let ub = b.as_ref().map(unshare);
        let shared = diff(a.as_ref(), b.as_ref());
        let unshared = diff(ua.as_ref(), ub.as_ref());
        assert_eq!(shared, unshared, "sharing changed diff, seed {seed}");
        assert_eq!(
            depot::codec::encode(&shared),
            depot::codec::encode(&unshared),
            "sharing changed canonical bytes, seed {seed}"
        );
        // And the full-record anchor (diff from nothing) likewise.
        assert_eq!(
            depot::codec::encode(&diff(None, b.as_ref())),
            depot::codec::encode(&diff(None, ub.as_ref())),
            "full record differs, seed {seed}"
        );
    }
}

