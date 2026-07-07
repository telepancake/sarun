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
//! and rotation — promote child B over parent A — is *derived and purely
//! syntactic* (no views, no backdrop, no host I/O):
//!
//! ```text
//! B' = compose(A, B);  A' = inverse over B's recorded footprint
//! resolve_over(bd, anc ++ [B'])     == resolve_over(bd, anc ++ [A, B])   for all bd
//! resolve_over(bd, anc ++ [B', A']) == resolve_over(bd, anc ++ [A])      for all bd
//! ```
//!
//! Stacks containing backdrop-anchored nodes (holes) MUST be resolved
//! compose-then-apply ([`resolve_over`]); fold-apply ([`resolve`]) is
//! only valid for pure lower-anchored stacks.
//!
//! Names are opaque bytes, deliberately not called "paths": a layer can
//! hold a filesystem image, a tabular dataset, a git snapshot, a wiki.
//! Interior nodes may carry blobs (superset of git's tree model).

use std::collections::BTreeMap;
use std::sync::Arc;

pub mod codec;
pub mod variant;

/// A node key: opaque bytes. Ordering is byte-lexicographic in this
/// reference implementation (canonical-encoding walks depend on ONE
/// deterministic order per variant; this variant picks bytewise).
pub type Name = Vec<u8>;

/// Attribute map: source-provided data only (never derived values — the
/// implicit-id rule, DEPOT-DESIGN.md §4).
pub type Attrs = BTreeMap<Name, Vec<u8>>;

/// Blob bytes, refcounted: a blob is written once and then shared —
/// between a delta and every view it was applied into, and between all
/// the views that inherit it. `Arc<[u8]>` derefs to `&[u8]`, so reads
/// are unchanged; writers wrap fresh bytes via `From<Vec<u8>>`.
pub type Bytes = Arc<[u8]>;

/// A resolved tree — pure value, no delta semantics.
///
/// Persistent (path-copying) representation: children and blob bytes
/// are `Arc`-shared, so `clone()` is O(root fanout), forking a view is
/// effectively free, and two views that diverged by k edits share every
/// untouched subtree. All mutation goes through copy-on-write
/// (`Arc::make_mut` along the touched path — see `apply_child_mut`),
/// which is what makes pointer equality a sound subtree-identity test
/// in [`diff`]. Equality is still structural (`PartialEq` on content).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct View {
    pub blob: Option<Bytes>,
    pub attrs: Attrs,
    pub children: BTreeMap<Name, Arc<View>>,
}

/// What a delta node does to the blob of the node below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobOp {
    /// Inherit the lower node's blob (absent if there is no lower node).
    Keep,
    Set(Bytes),
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

/// What a node's own facets (blob / attrs / opaque) resolve against.
///
/// A layer is a partially occluded view of a BACKDROP — the live host
/// filesystem, or the empty filesystem for no-host stacks. The backdrop
/// is never content in a layer; it is what access resolves against.
/// Parent links between layers are an ENCODING detail: a child is stored
/// as a difference from its parent, but its meaning is always the single
/// composed occlusion over the backdrop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Facets resolve against the recorded occlusion below (the normal
    /// delta encoding).
    Lower,
    /// Facets resolve against the BACKDROP: whatever recorded occlusion
    /// sits below is erased for this node's facets. A pure such node
    /// (no blob/attrs/children set) is a **hole** — "this key is not
    /// occluded" — the artifact layer re-encoding (rotation) leaves
    /// where the new parent-encoding contains changes that were never
    /// part of this layer's occlusion. A hole documents *lack of change*;
    /// a tombstone documents deletion. Holes are absolute: they always
    /// mean "backdrop", never "skip N layers".
    Backdrop,
}

/// One node of a delta tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub presence: Presence,
    pub blob: BlobOp,
    /// Masks lower *children* — recorded AND backdrop (AUFS opaque-dir).
    /// The node's own blob/attrs still inherit unless overridden —
    /// opaque is its own axis, not a kind of tombstone.
    pub opaque: bool,
    /// `None` = inherit attrs (from the anchor); `Some(map)` = replace.
    pub attrs: Option<Attrs>,
    pub anchor: Anchor,
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
            anchor: Anchor::Lower,
            children: BTreeMap::new(),
        }
    }

    pub fn tombstone() -> Self {
        Node { presence: Presence::Tombstone, ..Node::keep() }
    }

    /// A hole: this key is not occluded — the backdrop shows through,
    /// erasing any recorded occlusion below under composition.
    pub fn hole() -> Self {
        Node { anchor: Anchor::Backdrop, ..Node::keep() }
    }

    /// True if this node is exactly the identity delta (and so can be
    /// pruned from a parent's children without changing meaning). A
    /// backdrop-anchored node is NEVER identity: it erases under compose.
    pub fn is_identity(&self) -> bool {
        self.presence == Presence::Live
            && self.blob == BlobOp::Keep
            && !self.opaque
            && self.attrs.is_none()
            && self.anchor == Anchor::Lower
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

/// A resolved node with no content of its own: no blob, no attrs, no
/// children. Such a node is a legitimate, canonical [`View`] — "this
/// directory exists and is empty" — as long as it is *present* in its
/// parent's `children` map (or is the `Some` root). It is DISTINCT from
/// absence (the key not being in the map at all).
fn is_empty_view(v: &View) -> bool {
    v.blob.is_none() && v.attrs.is_empty() && v.children.is_empty()
}

/// Does this delta node *positively assert its own existence*, at its own
/// scope, independent of what sits below? A `Set`/`Remove` blob, replaced
/// attrs (even the empty map — the minimal existence witness), or an
/// opaque mask each assert "this node exists"; a pure `Keep`/inherit with
/// no attrs and no opaque asserts nothing. (Children are handled
/// separately — a surviving child gives the node content directly.)
fn asserts_self(node: &Node) -> bool {
    !matches!(node.blob, BlobOp::Keep) || node.attrs.is_some() || node.opaque
}

/// Overlay one delta node on an optional base view. `None` in = the name
/// does not exist below; `None` out = the name does not exist after.
fn apply_node(base: Option<&View>, node: &Node) -> Option<View> {
    if node.presence == Presence::Tombstone {
        return None;
    }
    // Whether the base was itself an explicitly-present empty node: its
    // presence is a carried existence assertion a pure inherit preserves.
    let base_empty_present = base.is_some_and(is_empty_view);
    let blob = match &node.blob {
        BlobOp::Keep => base.and_then(|b| b.blob.clone()),
        BlobOp::Set(bytes) => Some(bytes.clone()),
        BlobOp::Remove => None,
    };
    let attrs = match &node.attrs {
        Some(map) => map.clone(),
        None => base.map(|b| b.attrs.clone()).unwrap_or_default(),
    };
    let lower_children: Option<&BTreeMap<Name, Arc<View>>> = if node.opaque {
        None // opaque: lower children masked, as if there were none
    } else {
        base.map(|b| &b.children)
    };
    let mut children = BTreeMap::new();
    // Lower children not touched by the delta pass through (shared, not
    // copied — the Arc clone is the sharing).
    if let Some(lower) = lower_children {
        for (name, child) in lower {
            if !node.children.contains_key(name) {
                children.insert(name.clone(), Arc::clone(child));
            }
        }
    }
    // Delta children apply over their lower counterpart (masked if opaque).
    for (name, child_node) in &node.children {
        let lower_child = lower_children.and_then(|l| l.get(name));
        if let Some(v) = apply_node(lower_child.map(|a| a.as_ref()), child_node) {
            children.insert(name.clone(), Arc::new(v));
        }
    }
    // Canonical-form rule: existence is EXPLICIT, not implied by content
    // (DEPOT-DESIGN.md §6, revised). A node exists in the result iff it
    // has content of its own (a blob, attrs, or a surviving child), OR the
    // delta positively asserts it (`asserts_self`: a Set/Remove blob,
    // replaced attrs, or an opaque mask), OR the base was itself an
    // explicitly-present empty node this delta merely inherited. Only a
    // pure no-op inherit over an absent (or vacuously-emptied) base
    // collapses to `None` — "keep over nothing stays nothing." An empty
    // directory that was asserted (here or below) survives, and is a
    // DISTINCT canonical form from absence. This is what lets the depot
    // represent an empty directory — a filesystem has them — without
    // reintroducing spurious nodes.
    let exists = blob.is_some()
        || !attrs.is_empty()
        || !children.is_empty()
        || asserts_self(node)
        || base_empty_present;
    if !exists {
        return None;
    }
    Some(View { blob, attrs, children })
}

/// Overlay a layer on a base view.
pub fn apply(base: Option<&View>, layer: &Layer) -> Option<View> {
    apply_node(base, &layer.root)
}

/// In-place [`apply`]: only the names the delta records are touched, so
/// the cost is O(delta), not O(view). Must agree with `apply` bit-for-bit
/// (property-tested); `apply` stays the semantic reference.
fn apply_node_mut(slot: &mut Option<View>, node: &Node) {
    if node.presence == Presence::Tombstone {
        *slot = None;
        return;
    }
    let base_empty_present = slot.as_ref().is_some_and(is_empty_view);
    let mut v = slot.take().unwrap_or_default();
    mutate_view(&mut v, node);
    // Same explicit-existence rule as apply_node.
    let exists = v.blob.is_some()
        || !v.attrs.is_empty()
        || !v.children.is_empty()
        || asserts_self(node)
        || base_empty_present;
    *slot = exists.then_some(v);
}

/// Path-copying recursion of [`apply_node_mut`] below the root: only the
/// nodes on delta-touched paths are copied (`Arc::make_mut` — a no-op
/// move when the view is unshared, a one-node shallow clone when it is);
/// every untouched subtree stays shared with whatever views also hold it.
fn apply_child_mut(slot: &mut Option<Arc<View>>, node: &Node) {
    if node.presence == Presence::Tombstone {
        *slot = None;
        return;
    }
    let base_empty_present = slot.as_ref().is_some_and(|a| is_empty_view(a));
    let mut arc = slot.take().unwrap_or_default();
    mutate_view(Arc::make_mut(&mut arc), node);
    // Same explicit-existence rule as apply_node.
    let exists = arc.blob.is_some()
        || !arc.attrs.is_empty()
        || !arc.children.is_empty()
        || asserts_self(node)
        || base_empty_present;
    *slot = exists.then_some(arc);
}

/// The per-node mutation shared by `apply_node_mut` (root) and
/// `apply_child_mut` (interior): must mirror `apply_node` facet-for-facet.
fn mutate_view(v: &mut View, node: &Node) {
    match &node.blob {
        BlobOp::Keep => {}
        BlobOp::Set(bytes) => v.blob = Some(bytes.clone()),
        BlobOp::Remove => v.blob = None,
    }
    if let Some(map) = &node.attrs {
        v.attrs = map.clone();
    }
    if node.opaque {
        v.children.clear();
    }
    for (name, child_node) in &node.children {
        let mut child = v.children.remove(name);
        apply_child_mut(&mut child, child_node);
        if let Some(c) = child {
            v.children.insert(name.clone(), c);
        }
    }
}

/// Overlay a layer on a view in place — `apply`, O(delta).
pub fn apply_mut(base: &mut Option<View>, layer: &Layer) {
    apply_node_mut(base, &layer.root);
}

/// Resolve a stack of layers, listed lower-first, into its effective view
/// over NOTHING (the empty backdrop), by folding `apply`. ONLY valid for
/// stacks with no backdrop-anchored nodes: `apply` cannot distinguish
/// "recorded lower" from "backdrop", so a hole applied mid-fold would
/// wrongly inherit the accumulated view. Stacks that may contain anchors
/// must use [`resolve_over`] (compose-then-apply).
pub fn resolve(stack: &[&Layer]) -> Option<View> {
    let mut view = None;
    for layer in stack {
        view = apply(view.as_ref(), layer);
    }
    view
}

/// Resolve a stack over an explicit backdrop, compose-then-apply: fold the
/// deltas with `compose` (where backdrop-anchored facets erase recorded
/// occlusion below), then apply the net occlusion to the backdrop once.
/// This is the semantically correct resolution for any stack; sarun's
/// per-name chain walk is this, per name.
pub fn resolve_over(backdrop: Option<&View>, stack: &[&Layer]) -> Option<View> {
    apply(backdrop, &squash(stack))
}

// -------------------------------------------------------------- compose

/// Merge two delta nodes: `compose(a, b)` behaves, under `apply`, exactly
/// like applying `a` then `b`. This is the whiteout juggling of
/// DEPOT-DESIGN.md §6, in one place:
///
/// - `b` tombstone wins outright.
/// - `b` backdrop-anchored: `a`'s facets are erased (the backdrop shows
///   through them); `a`'s children still compose per name — anchoring is
///   facet-local, subtree erasure is expressed by explicit per-name holes
///   (rotation enumerates them; all footprints are recorded data).
/// - `b` live over `a` tombstone is a *recreate*: nothing below `a` may
///   show through, so the result is opaque, `Keep` hardens to `Remove`,
///   and inherit-attrs hardens to replace-with-what-`b`-sees (empty).
/// - `b` opaque masks `a`'s children as well as lower ones.
fn compose_node(a: &Node, b: &Node) -> Node {
    if b.presence == Presence::Tombstone {
        return Node::tombstone();
    }
    if b.anchor == Anchor::Backdrop {
        // b re-bases this name on the backdrop: NOTHING recorded below
        // survives — not facets, not children, not a tombstone. b's own
        // facets and explicitly-listed children are the entire recorded
        // occlusion here. (This wholesale scope is what keeps squash
        // confluent with later holes: erasure never half-survives.)
        return b.clone();
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
    Node { presence: Presence::Live, blob, opaque, attrs, anchor: a.anchor, children }
}

/// Would this delta node materialize a view when applied over an absent
/// base? Mirrors `apply_node`'s explicit-existence rule: a live node
/// materializes iff it asserts itself (a Set/Remove blob, replaced attrs
/// — even empty — or an opaque mask) or has a materializing child.
fn materializes(n: &Node) -> bool {
    n.presence == Presence::Live
        && (asserts_self(n) || n.children.values().any(materializes))
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
        anchor: Anchor::Lower,
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
        // (ptr_eq first: same COW-shared bytes are equal without a scan.)
        (Some(old), Some(new)) if Arc::ptr_eq(old, new) || old == new => BlobOp::Keep,
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
        // Fast path — sound because all View mutation is copy-on-write
        // (`Arc::make_mut` path-copying): two slots holding the SAME Arc
        // can only hold identical subtrees, whose diff is the identity
        // delta, which this loop prunes anyway. Pure short-circuit; the
        // emitted Layer (and its canonical encoding) is byte-identical.
        if bchild.is_some_and(|b| Arc::ptr_eq(b, tchild)) {
            continue;
        }
        let d = diff_node(bchild.map(|a| a.as_ref()), tchild);
        if !d.is_identity() {
            children.insert(name.clone(), d);
        }
    }
    let mut node = Node { presence: Presence::Live, blob, opaque: false, attrs,
                          anchor: Anchor::Lower, children };
    // Existence witness: `target` is an explicitly-present EMPTY node (no
    // blob, no attrs, no children). The natural delta reaching it (Keep /
    // inherit, plus per-child tombstones for anything the base had) asserts
    // nothing of its own, so `apply` would collapse it to absence — unless
    // the base was *already* an explicitly-present empty node (existence
    // carried) or the delta already asserts itself. Otherwise force a
    // minimal existence assertion (replace attrs with `target`'s — the
    // empty map) so `apply(base, diff) == target` round-trips the empty
    // directory exactly, distinct from absence.
    if is_empty_view(target)
        && !asserts_self(&node)
        && !base.is_some_and(is_empty_view)
    {
        node.attrs = Some(target.attrs.clone());
    }
    node
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
                anchor: Anchor::Lower,
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
/// Rotation, derived and purely syntactic (DEPOT-DESIGN.md §6): given
/// parent `a` and child `b` (child stacked over parent), promote the
/// child. Returns `(b', a')` where `b'` carries the old stack's total
/// occlusion and `a'` restores `a`'s occlusion when stacked on `b'`.
///
/// Rotation rewrites ENCODINGS; no layer's occlusion changes — which is
/// why it needs no view, no backdrop, and no host I/O. `ancestors` are
/// `a`'s own encoding ancestors (recorded data), consulted only to
/// replicate older changes at names `b` touched: where the old chain
/// recorded something, `a'` replicates it (backdrop-anchored, erasing
/// `b'`'s contribution); where it recorded nothing, `a'` writes a hole —
/// the backdrop shows through, LIVE at access time. Grandparent changes
/// replicate; holes never mean "skip N layers".
///
/// Law (checked over multiple backdrops in the tests):
///   resolve_over(bd, ancestors ++ [b'])      == resolve_over(bd, ancestors ++ [a, b])
///   resolve_over(bd, ancestors ++ [b', a'])  == resolve_over(bd, ancestors ++ [a])
pub fn rotate(ancestors: &[&Layer], a: &Layer, b: &Layer) -> (Layer, Layer) {
    let b_new = compose(a, b);
    // The old child stack's net recorded occlusion (encoding vs backdrop).
    let mut chain: Vec<&Layer> = ancestors.to_vec();
    chain.push(a);
    let net = squash(&chain);
    let a_root = inverse_node(Some(&net.root), &b.root);
    (b_new, Layer { root: a_root })
}

/// Does this delta node record anything at all (itself or below)?
fn records(n: &Node) -> bool {
    !n.is_identity()
}

/// Did the node record anything at its own scope (facets, presence,
/// opaque, anchoring)?
fn facets_recorded(n: &Node) -> bool {
    n.presence == Presence::Tombstone
        || !matches!(n.blob, BlobOp::Keep)
        || n.attrs.is_some()
        || n.opaque
        || n.anchor == Anchor::Backdrop
}

/// Replicate a net-occlusion subtree as a backdrop-anchored (re-based)
/// restoration: the top node erases everything recorded below it; its
/// facets and children ARE the occlusion, verbatim from net. Keep/None
/// facets still mean "backdrop", which is exactly what they meant at the
/// chain root, and the backdrop stays LIVE — nothing is snapshotted.
fn replicate(net: &Node) -> Node {
    if net.presence == Presence::Tombstone {
        return Node::tombstone();
    }
    Node { anchor: Anchor::Backdrop, ..net.clone() }
}

/// The anti-`b` node: restores the old chain's net occlusion (`net`) at
/// every scope `b` recorded.
fn inverse_node(net: Option<&Node>, b: &Node) -> Node {
    if facets_recorded(b) {
        // b recorded at this scope: re-base it on the old chain's net
        // occlusion — or on nothing (a pure hole) if the chain had none.
        return match net {
            Some(n) => replicate(n),
            None => Node::hole(),
        };
    }
    if net.is_some_and(|n| n.presence == Presence::Tombstone) {
        // b recorded only deeper, but the old chain deleted this whole
        // subtree; restore the deletion (it subsumes the names below).
        return Node::tombstone();
    }
    // Carrier: recurse into the children b recorded.
    let mut out = Node::keep();
    for (name, bc) in &b.children {
        if !records(bc) {
            continue;
        }
        let net_child = net.and_then(|n| n.children.get(name));
        let inv = inverse_node(net_child, bc);
        if !inv.is_identity() {
            out.children.insert(name.clone(), inv);
        }
    }
    out
}
