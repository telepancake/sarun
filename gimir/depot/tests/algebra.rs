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
    apply, compose, diff, resolve, rotate, squash, Attrs, BlobOp, Layer, Node, Presence, View,
};

// ------------------------------------------------------------- builders

fn n(names: &[u8]) -> Vec<u8> {
    names.to_vec()
}

fn live() -> Node {
    Node::keep()
}

fn set(bytes: &[u8]) -> Node {
    Node { blob: BlobOp::Set(bytes.to_vec()), ..Node::keep() }
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

fn view_blob(bytes: &[u8]) -> View {
    View { blob: Some(bytes.to_vec()), ..View::default() }
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
        vec![(b"f", Node { blob: BlobOp::Set(n(b"data")), attrs: Some(attrs), ..Node::keep() })],
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
        blob: Some(n(b"rootnow")),
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
    assert_eq!(d.root.children[&n(b"changed")].blob, BlobOp::Set(n(b"new")));
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

    let (b_new, a_new) = rotate(&a, &b);
    // resolve(B') == resolve(A + B)
    assert_eq!(resolve(&[&b_new]), resolve(&[&a, &b]));
    // resolve(B' + A') == resolve(A)
    assert_eq!(resolve(&[&b_new, &a_new]), resolve(&[&a]));
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

    let (b_new, a_new) = rotate(&a, &b);
    assert_eq!(resolve(&[&b_new]), resolve(&[&a, &b]));
    assert_eq!(resolve(&[&b_new, &a_new]), resolve(&[&a]));

    // And the inverse really is explicit re-listing, not opaque.
    let d = &a_new.root.children[&n(b"d")];
    assert!(!d.opaque, "diff never generates opaque");
    assert_eq!(d.children[&n(b"z")].presence, Presence::Tombstone);
    assert_eq!(d.children[&n(b"x")].blob, BlobOp::Set(n(b"X")));
    assert_eq!(d.children[&n(b"y")].blob, BlobOp::Set(n(b"Y")));
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
    let (b1, a1) = rotate(&a, &b);
    let (a2, b2) = rotate(&b1, &a1);
    // Rotating back: parent view again == old A+... check both.
    assert_eq!(resolve(&[&a2]), resolve(&[&a]));
    assert_eq!(resolve(&[&a2, &b2]), resolve(&[&a, &b]));
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
        // rotation equivalences.
        let (b_new, a_new) = rotate(&a, &b);
        assert_eq!(resolve(&[&b_new]), resolve(&[&a, &b]), "rotation parent view, seed {seed}");
        assert_eq!(resolve(&[&b_new, &a_new]), resolve(&[&a]), "rotation child view, seed {seed}");
    }
}
