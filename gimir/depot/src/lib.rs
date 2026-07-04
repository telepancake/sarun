//! The depot node model and layer algebra — DEPOT-DESIGN.md §2 and §6,
//! made concrete as an in-memory reference implementation.
//!
//! Two kinds of tree live here and must not be conflated:
//!
//! - [`View`] — a *resolved* tree: the effective content a stack of layers
//!   presents. No tombstones, no opaque marks, no "inherit" — just values.
//! - [`Node`] (the root of a [`Layer`]) — a *delta* tree: every field is an
//!   instruction relative to whatever sits below (`Keep`/`Set`/`Remove`
//!   blobs, live/tombstone presence, opaque child-masking, attrs replace
//!   or inherit).
//!
//! The algebra:
//!
//! - [`apply`]   : View? × Layer → View?   (overlay one delta on a view)
//! - [`resolve`] : [Layer] → View?         (fold `apply` from nothing)
//! - [`compose`] : Layer × Layer → Layer   (merge two deltas into one)
//! - [`squash`]  : [Layer] → Layer         (fold `compose`)
//! - [`diff`]    : View? × View? → Layer   (the delta turning one view
//!   into another)
//!
//! The laws the tests hold these to (DEPOT-DESIGN.md §6):
//!
//! ```text
//! resolve(stack ++ [squash(tail)]) == resolve(stack ++ tail)
//! apply(base, diff(base, target)) == target
//! ```
//!
//! and rotation — promote child B over parent A — is *derived*:
//!
//! ```text
//! B' = squash([A, B]);  A' = diff(view(B'), view(A))
//! resolve([B'])     == resolve([A, B])
//! resolve([B', A']) == resolve([A])
//! ```
//!
//! Names are opaque bytes, deliberately not called "paths": a layer can
//! hold a filesystem image, a tabular dataset, a git snapshot, a wiki.
//! Interior nodes may carry blobs (superset of git's tree model).

use std::collections::BTreeMap;

pub mod codec;

/// A node key: opaque bytes. Ordering is byte-lexicographic in this
/// reference implementation (canonical-encoding walks depend on ONE
/// deterministic order per variant; this variant picks bytewise).
pub type Name = Vec<u8>;

/// Attribute map: source-provided data only (never derived values — the
/// implicit-id rule, DEPOT-DESIGN.md §4).
pub type Attrs = BTreeMap<Name, Vec<u8>>;

/// A resolved tree — pure value, no delta semantics.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct View {
    pub blob: Option<Vec<u8>>,
    pub attrs: Attrs,
    pub children: BTreeMap<Name, View>,
}

/// What a delta node does to the blob of the node below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobOp {
    /// Inherit the lower node's blob (absent if there is no lower node).
    Keep,
    Set(Vec<u8>),
    /// The lower node's blob is removed; children are unaffected. This is
    /// the "interior node loses its blob but keeps children" corner.
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    Live,
    /// First-class deletion: masks the name in every lower layer. A
    /// tombstone node's other fields are meaningless and ignored.
    Tombstone,
}

/// One node of a delta tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub presence: Presence,
    pub blob: BlobOp,
    /// Masks lower-layer *children* only (AUFS opaque-dir). The node's own
    /// blob/attrs still inherit unless overridden — opaque is a third
    /// axis, not a kind of tombstone.
    pub opaque: bool,
    /// `None` = inherit lower attrs; `Some(map)` = replace them wholesale.
    pub attrs: Option<Attrs>,
    pub children: BTreeMap<Name, Node>,
}

impl Node {
    /// The identity delta: changes nothing, masks nothing.
    pub fn keep() -> Self {
        Node {
            presence: Presence::Live,
            blob: BlobOp::Keep,
            opaque: false,
            attrs: None,
            children: BTreeMap::new(),
        }
    }

    pub fn tombstone() -> Self {
        Node { presence: Presence::Tombstone, ..Node::keep() }
    }

    /// True if this node is exactly the identity delta (and so can be
    /// pruned from a parent's children without changing meaning).
    pub fn is_identity(&self) -> bool {
        self.presence == Presence::Live
            && self.blob == BlobOp::Keep
            && !self.opaque
            && self.attrs.is_none()
            && self.children.is_empty()
    }
}

/// One layer: a delta tree rooted at an unnamed node. The root's presence
/// must be `Live` (a depot layer cannot tombstone its own root).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub root: Node,
}

impl Layer {
    pub fn empty() -> Self {
        Layer { root: Node::keep() }
    }
}

// ---------------------------------------------------------------- apply

/// Overlay one delta node on an optional base view. `None` in = the name
/// does not exist below; `None` out = the name does not exist after.
fn apply_node(base: Option<&View>, node: &Node) -> Option<View> {
    if node.presence == Presence::Tombstone {
        return None;
    }
    let blob = match &node.blob {
        BlobOp::Keep => base.and_then(|b| b.blob.clone()),
        BlobOp::Set(bytes) => Some(bytes.clone()),
        BlobOp::Remove => None,
    };
    let attrs = match &node.attrs {
        Some(map) => map.clone(),
        None => base.map(|b| b.attrs.clone()).unwrap_or_default(),
    };
    let lower_children: Option<&BTreeMap<Name, View>> = if node.opaque {
        None // opaque: lower children masked, as if there were none
    } else {
        base.map(|b| &b.children)
    };
    let mut children = BTreeMap::new();
    // Lower children not touched by the delta pass through.
    if let Some(lower) = lower_children {
        for (name, child) in lower {
            if !node.children.contains_key(name) {
                children.insert(name.clone(), child.clone());
            }
        }
    }
    // Delta children apply over their lower counterpart (masked if opaque).
    for (name, child_node) in &node.children {
        let lower_child = lower_children.and_then(|l| l.get(name));
        if let Some(v) = apply_node(lower_child, child_node) {
            children.insert(name.clone(), v);
        }
    }
    // Canonical-form rule: an empty node — no blob, no attrs, no
    // children — does not exist. This is what makes views canonical
    // (existence = content), the identity delta prunable, `diff`
    // minimal, and compose/apply commute even when later layers empty
    // out a node an earlier layer created ("materialize but inherit" is
    // deliberately inexpressible). A variant that needs empty
    // directories to exist carries attrs on them (an fs layer's mode
    // already does).
    if blob.is_none() && attrs.is_empty() && children.is_empty() {
        return None;
    }
    Some(View { blob, attrs, children })
}

/// Overlay a layer on a base view.
pub fn apply(base: Option<&View>, layer: &Layer) -> Option<View> {
    apply_node(base, &layer.root)
}

/// Resolve a stack of layers, listed lower-first, into its effective view.
/// `None` means the stack presents nothing at all (e.g. empty stack).
pub fn resolve(stack: &[&Layer]) -> Option<View> {
    let mut view = None;
    for layer in stack {
        view = apply(view.as_ref(), layer);
    }
    view
}

// -------------------------------------------------------------- compose

/// Merge two delta nodes: `compose(a, b)` behaves, under `apply`, exactly
/// like applying `a` then `b`. This is the whiteout juggling of
/// DEPOT-DESIGN.md §6, in one place:
///
/// - `b` tombstone wins outright.
/// - `b` live over `a` tombstone is a *recreate*: nothing below `a` may
///   show through, so the result is opaque, `Keep` hardens to `Remove`,
///   and inherit-attrs hardens to replace-with-what-`b`-sees (empty).
/// - `b` opaque masks `a`'s children as well as lower ones.
fn compose_node(a: &Node, b: &Node) -> Node {
    if b.presence == Presence::Tombstone {
        return Node::tombstone();
    }
    if a.presence == Presence::Tombstone {
        // Recreate over a whiteout: harden every inherit in `b` so the
        // composed delta still masks everything below `a`. If `b` sets
        // nothing, the recreate materializes nothing and the tombstone
        // stands (harden returns it).
        return harden(b);
    }
    let blob = match &b.blob {
        BlobOp::Keep => a.blob.clone(),
        other => other.clone(),
    };
    let attrs = match &b.attrs {
        Some(map) => Some(map.clone()),
        None => a.attrs.clone(),
    };
    let (opaque, children) = if b.opaque {
        // a's children are masked; only b's children survive, but each b
        // child must be hardened: with a's contribution gone there is
        // nothing between it and the (also masked) lower layers.
        let kids = b
            .children
            .iter()
            .filter_map(|(k, v)| {
                // Behind the opaque mask a child inherits nothing;
                // non-materializing ones (harden → tombstone) are
                // dropped outright — the opaque already masks the name.
                let h = harden(v);
                (h.presence == Presence::Live).then(|| (k.clone(), h))
            })
            .collect();
        (true, kids)
    } else {
        let mut kids: BTreeMap<Name, Node> = BTreeMap::new();
        for (name, ac) in &a.children {
            match b.children.get(name) {
                Some(bc) => {
                    let c = compose_node(ac, bc);
                    if !c.is_identity() {
                        kids.insert(name.clone(), c);
                    }
                }
                None => {
                    kids.insert(name.clone(), ac.clone());
                }
            }
        }
        for (name, bc) in &b.children {
            if !a.children.contains_key(name) {
                if a.opaque {
                    // This child of `b` cannot inherit anything through
                    // `a`'s mask — harden it; if it materializes nothing,
                    // drop it (the composed node keeps `a`'s opaque).
                    let h = harden(bc);
                    if h.presence == Presence::Live {
                        kids.insert(name.clone(), h);
                    }
                } else if !bc.is_identity() {
                    kids.insert(name.clone(), bc.clone());
                }
            }
        }
        (a.opaque, kids)
    };
    Node { presence: Presence::Live, blob, opaque, attrs, children }
}

/// Would this delta node materialize a view when applied over an absent
/// base? Mirrors `apply_node`'s inherit-absence rule.
fn materializes(n: &Node) -> bool {
    n.presence == Presence::Live
        && (matches!(n.blob, BlobOp::Set(_))
            || n.attrs.as_ref().is_some_and(|a| !a.is_empty())
            || n.children.values().any(materializes))
}

/// Harden a delta so it inherits nothing — used when whatever is composed
/// below guarantees there is nothing to inherit (a tombstone, or an
/// opaque parent masking the name). A node that would not materialize
/// over nothing collapses to a tombstone (it must still mask deeper
/// stacks the composed layer may sit on); a materializing node gets
/// `Keep`/`Remove` → `Remove`, inherit-attrs → replace-with-what-it-sees
/// (empty), opaque forced, children hardened with the non-materializing
/// ones dropped (the forced opaque already masks their names).
fn harden(n: &Node) -> Node {
    if !materializes(n) {
        return Node::tombstone();
    }
    Node {
        presence: Presence::Live,
        blob: match &n.blob {
            BlobOp::Keep | BlobOp::Remove => BlobOp::Remove,
            set => set.clone(),
        },
        opaque: true,
        attrs: Some(n.attrs.clone().unwrap_or_default()),
        children: n
            .children
            .iter()
            .filter_map(|(k, v)| {
                let h = harden(v);
                (h.presence == Presence::Live).then(|| (k.clone(), h))
            })
            .collect(),
    }
}

/// Compose two layers: applying the result equals applying `a` then `b`.
pub fn compose(a: &Layer, b: &Layer) -> Layer {
    Layer { root: compose_node(&a.root, &b.root) }
}

/// Squash a stack (lower-first) into one layer. Squash of a *partial*
/// stack keeps tombstones and opaque marks — the result must still mask
/// deeper layers it may be stacked on (DEPOT-DESIGN.md §6).
pub fn squash(stack: &[&Layer]) -> Layer {
    let mut acc = Layer::empty();
    for layer in stack {
        acc = compose(&acc, layer);
    }
    acc
}

// ----------------------------------------------------------------- diff

/// The delta that turns `base` into `target` under `apply`. Emits
/// per-entry tombstones for removed children (never generates opaque —
/// note in DEPOT-DESIGN.md §6: opaque inversion is handled by re-listing).
fn diff_node(base: Option<&View>, target: &View) -> Node {
    let blob = match (base.and_then(|b| b.blob.as_ref()), &target.blob) {
        (Some(old), Some(new)) if old == new => BlobOp::Keep,
        (_, Some(new)) => BlobOp::Set(new.clone()),
        (Some(_), None) => BlobOp::Remove,
        (None, None) => BlobOp::Keep,
    };
    let attrs = match base {
        Some(b) if b.attrs == target.attrs => None,
        None if target.attrs.is_empty() => None,
        _ => Some(target.attrs.clone()),
    };
    let mut children = BTreeMap::new();
    if let Some(b) = base {
        for name in b.children.keys() {
            if !target.children.contains_key(name) {
                children.insert(name.clone(), Node::tombstone());
            }
        }
    }
    for (name, tchild) in &target.children {
        let bchild = base.and_then(|b| b.children.get(name));
        let d = diff_node(bchild, tchild);
        if !d.is_identity() {
            children.insert(name.clone(), d);
        }
    }
    // Canonical views contain no empty nodes (apply prunes them), so a
    // live target always sets something and the delta materializes it.
    Node { presence: Presence::Live, blob, opaque: false, attrs, children }
}

/// Delta from one optional view to another. `target = None` yields a
/// layer whose root tombstones everything (only meaningful for children
/// of a real diff; a layer root itself stays live).
pub fn diff(base: Option<&View>, target: Option<&View>) -> Layer {
    match target {
        Some(t) => Layer { root: diff_node(base, t) },
        None => Layer {
            root: Node {
                // No target at all: mask the base entirely.
                presence: Presence::Live,
                blob: BlobOp::Remove,
                opaque: true,
                attrs: Some(Attrs::new()),
                children: BTreeMap::new(),
            },
        },
    }
}

// ------------------------------------------------------------- rotation

/// Rotation, derived (DEPOT-DESIGN.md §6): given parent `a` and child `b`
/// (child stacked over parent), promote the child. Returns `(b', a')`
/// where `b'` is the new parent (effective content of the old stack) and
/// `a'` the new child (restores the old parent's view when stacked on
/// `b'`).
pub fn rotate(a: &Layer, b: &Layer) -> (Layer, Layer) {
    let b_new = squash(&[a, b]);
    let view_b = resolve(&[&b_new]);
    let view_a = resolve(&[a]);
    let a_new = diff(view_b.as_ref(), view_a.as_ref());
    (b_new, a_new)
}
