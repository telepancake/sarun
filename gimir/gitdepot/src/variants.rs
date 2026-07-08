//! Union-variant encoding of parallel lane trees, and the streaming
//! encoder that produces a revision's frame WITHOUT ever materializing a
//! combined tree.
//!
//! ## The representation
//!
//! All lanes live in ONE tree keyed by real path. At a file path its
//! distinct versions are stored, and a version's **content** and its
//! **(attrs, lane-bitmap)** live in SEPARATE sibling keys so that adding a
//! lane rewrites only the small bitmap, never the content:
//!   * `\0v<idx>` — a node whose blob IS the version's content, nothing
//!     else. Byte-stable across lane-membership changes.
//!   * `\0m<idx>` — a node with no blob; attrs = the file's whole attr set
//!     plus a private `\0lanes` bitmap. The only thing a new lane touches.
//! Directories are ordinary real-name nodes. Git forbids empty
//! directories, so a node's children are either real names (a directory)
//! or `\0`-led keys (a file's versions) — never ambiguous, and a
//! file-in-one-lane / dir-in-another clash is representable (both kinds of
//! child coexist). A lane is pulled back out with [`extract`], which at
//! each file picks the one version whose bitmap has that lane's bit.
//!
//! ## The encoder
//!
//! A revision's frame is a depot reverse delta between two states — the
//! union at revision `i` and at `i-1`. [`reverse_delta`] computes that
//! delta by walking the two states' per-lane git trees in lockstep and
//! emitting only what differs, never building either union tree. Because
//! exactly one lane advances per revision, every path where that lane did
//! not move has identical lanes in both states and is pruned in O(1) by
//! [`lanes_equal`]; the record ends up proportional to the real churn. No
//! base, no delta-of-delta, no base-switching.

use depot::{Anchor, Attrs, BlobOp, Bytes, Layer, Name, Node, Presence, View};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Marker byte leading every non-path key. Real git path segments never
/// contain NUL, so these cannot collide with a filename.
const VAR: u8 = 0;
const CONTENT: u8 = b'v';
const META: u8 = b'm';
/// Attr on a `\0m<idx>` node: the version's lane bitmap. Leads with `\0` so
/// it can never collide with a real git attr (git attrs — `mode` and any
/// future addition — are plain ASCII), letting the meta node carry the
/// file's whole attr set verbatim alongside the bitmap.
const LANES: &[u8] = b"\0lanes";

fn content_key(idx: u32) -> Name {
    let mut k = vec![VAR, CONTENT];
    k.extend_from_slice(&idx.to_be_bytes());
    k
}
fn meta_key(idx: u32) -> Name {
    let mut k = vec![VAR, META];
    k.extend_from_slice(&idx.to_be_bytes());
    k
}
fn is_var(key: &[u8]) -> bool {
    key.first() == Some(&VAR)
}

fn set_bit(bm: &mut Vec<u8>, i: usize) {
    let byte = i / 8;
    if bm.len() <= byte {
        bm.resize(byte + 1, 0);
    }
    bm[byte] |= 1 << (i % 8);
}
fn bit(bm: &[u8], i: usize) -> bool {
    let byte = i / 8;
    byte < bm.len() && (bm[byte] >> (i % 8)) & 1 == 1
}
/// The lowest set lane bit — a variant's stable key. Panics only on an
/// all-zero bitmap, which never occurs: a version is minted only from a
/// lane that carries it, so at least one bit is set.
fn min_bit(bm: &[u8]) -> u32 {
    for (byte, &b) in bm.iter().enumerate() {
        if b != 0 {
            return (byte * 8 + b.trailing_zeros() as usize) as u32;
        }
    }
    unreachable!("a version always has at least one lane bit")
}

// -------------------------------------------------------------- encoder

/// One version at a path: a distinct `(attrs, content)` and the bitmap of
/// lanes that carry it — built locally by the walk, never stored as a tree.
struct Version {
    attrs: Attrs,
    content: Bytes,
    bitmap: Vec<u8>,
}

impl Version {
    /// The `\0v<idx>` content leaf View.
    fn content_leaf(&self) -> View {
        View { blob: Some(self.content.clone()), attrs: Attrs::new(), children: BTreeMap::new() }
    }
    /// The `\0m<idx>` meta leaf View: file attrs + the lane bitmap.
    fn meta_leaf(&self) -> View {
        let mut attrs = self.attrs.clone();
        attrs.insert(LANES.to_vec(), self.bitmap.clone());
        View { blob: None, attrs, children: BTreeMap::new() }
    }
}

/// Group the file-lanes at one path into ordered versions (sorted by
/// `(attrs, content)` so the index is deterministic), each with its lane
/// bitmap. Directory-lanes and absent lanes contribute nothing here.
fn versions_at(lanes: &[Option<&View>]) -> Vec<Version> {
    let mut by_key: BTreeMap<(Attrs, Bytes), Vec<u8>> = BTreeMap::new();
    for (i, v) in lanes.iter().enumerate() {
        if let Some(v) = v {
            if let Some(content) = &v.blob {
                set_bit(by_key.entry((v.attrs.clone(), content.clone())).or_default(), i);
            }
        }
    }
    by_key
        .into_iter()
        .map(|((attrs, content), bitmap)| Version { attrs, content, bitmap })
        .collect()
}

/// The sorted set of real (directory) child names across the dir-lanes at
/// one path.
fn dir_names(lanes: &[Option<&View>]) -> BTreeSet<Name> {
    let mut names = BTreeSet::new();
    for v in lanes.iter().flatten() {
        if v.blob.is_none() {
            names.extend(v.children.keys().cloned());
        }
    }
    names
}

/// Each lane's child View at `name` (only dir-lanes that have it), with
/// lane indices preserved so the recursion stays aligned.
fn sub_at<'a>(lanes: &[Option<&'a View>], name: &[u8]) -> Vec<Option<&'a View>> {
    lanes
        .iter()
        .map(|v| v.and_then(|v| if v.blob.is_none() { v.children.get(name).map(Arc::as_ref) } else { None }))
        .collect()
}

/// True iff every lane presents the identical View (by pointer or value) in
/// both states — then the whole union subtree here is unchanged and the
/// walk prunes it without descending. Pointer equality gives the O(1) skip
/// that makes a one-lane-advance revision cost O(its own churn).
fn lanes_equal(base: &[Option<&View>], target: &[Option<&View>]) -> bool {
    base.len() == target.len()
        && base.iter().zip(target).all(|(b, t)| match (b, t) {
            (None, None) => true,
            (Some(b), Some(t)) => std::ptr::eq(*b, *t) || b == t,
            _ => false,
        })
}

/// A leaf delta child reconstructing `target` from `base` (either may be
/// absent). A `\0v`/`\0m` version node is a leaf, so this is a pure
/// blob/attrs/tombstone op with no children.
fn leaf_delta(base: Option<&View>, target: Option<&View>) -> Node {
    match target {
        None => Node::tombstone(), // gone in target → removed when rebuilt
        Some(t) => {
            if base == Some(t) {
                return Node::keep(); // identity — pruned by the caller
            }
            Node {
                presence: Presence::Live,
                blob: match &t.blob {
                    Some(bytes) => BlobOp::Set(bytes.clone()),
                    None => BlobOp::Remove,
                },
                opaque: false,
                // `Some` even when empty: the minimal existence witness, so a
                // no-blob `\0m` node still materializes with its attrs.
                attrs: Some(t.attrs.clone()),
                anchor: Anchor::Lower,
                children: BTreeMap::new(),
            }
        }
    }
}

/// The depot reverse-delta [`Layer`] that turns `union(base)` into
/// `union(target)`, computed by the lockstep lane walk — no union tree is
/// ever built. `base[i]`/`target[i]` are lane `i`'s git tree (or `None`).
/// The `f0` full frame is this with `base` all-`None`.
pub fn reverse_delta(base: &[Option<View>], target: &[Option<View>]) -> Layer {
    let b: Vec<Option<&View>> = base.iter().map(Option::as_ref).collect();
    let t: Vec<Option<&View>> = target.iter().map(Option::as_ref).collect();
    Layer { root: rdelta(&b, &t) }
}

/// The reverse-delta node at one path: turn `base`'s union node into
/// `target`'s. Emits changed `\0v`/`\0m` version leaves and recurses into
/// changed directory children. Only ever called with a `target` that still
/// exists at this path (removal is decided by the parent), so its own root
/// is never a tombstone.
fn rdelta(base: &[Option<&View>], target: &[Option<&View>]) -> Node {
    let mut node = Node::keep();
    if lanes_equal(base, target) {
        return node; // identity: nothing at or below this path changed
    }

    // Version leaves, keyed by variant IDENTITY — the lowest lane in the
    // variant's bitmask. Variants partition the lanes, so bitmasks are
    // disjoint and the min bit is a unique, STABLE key derivable from the
    // bitmask alone: an unchanged variant keeps its key, so a variant present
    // in both states is emitted as nothing; a content edit that keeps the
    // lane-set reuses the key (only `\0v` changes); a lane joining a variant
    // above its min keeps the key (only `\0m`'s bitmap changes, `\0v` content
    // stays). Positional indexing did none of this — it re-keyed on every set
    // change. (`min_bit` exists because every version has ≥1 lane bit.)
    let bv: BTreeMap<u32, Version> =
        versions_at(base).into_iter().map(|v| (min_bit(&v.bitmap), v)).collect();
    let tv: BTreeMap<u32, Version> =
        versions_at(target).into_iter().map(|v| (min_bit(&v.bitmap), v)).collect();
    for key in bv.keys().chain(tv.keys()).copied().collect::<BTreeSet<_>>() {
        let (b, t) = (bv.get(&key), tv.get(&key));
        let c = leaf_delta(b.map(Version::content_leaf).as_ref(), t.map(Version::content_leaf).as_ref());
        if !c.is_identity() {
            node.children.insert(content_key(key), c);
        }
        let m = leaf_delta(b.map(Version::meta_leaf).as_ref(), t.map(Version::meta_leaf).as_ref());
        if !m.is_identity() {
            node.children.insert(meta_key(key), m);
        }
    }

    // Directory children over the union of real names in both states.
    // Removal (present in base, gone in target) is decided HERE and emitted
    // as a tombstone, so a child's own `rdelta` is only ever called with a
    // still-present target.
    let mut names = dir_names(base);
    names.extend(dir_names(target));
    for name in names {
        let bs = sub_at(base, &name);
        let ts = sub_at(target, &name);
        let t_present = ts.iter().any(Option::is_some);
        let b_present = bs.iter().any(Option::is_some);
        if b_present && !t_present {
            node.children.insert(name, Node::tombstone());
        } else if t_present {
            let child = rdelta(&bs, &ts);
            if !child.is_identity() {
                node.children.insert(name, child);
            }
        }
    }
    node
}

// ------------------------------------------------------------ extract

/// Reconstruct lane `l`'s git tree from a materialized union View. A
/// present lane contributing nothing at the root is the empty tree, so the
/// top level returns that, not "absent".
pub fn extract(u: &View, l: usize) -> View {
    extract_node(u, l).unwrap_or_default()
}

fn extract_node(u: &View, l: usize) -> Option<View> {
    // A file for lane l iff some meta sibling carries its bit.
    for (key, meta) in &u.children {
        if key.len() >= 2 && key[0] == VAR && key[1] == META {
            if meta.attrs.get(LANES).is_some_and(|bm| bit(bm, l)) {
                let idx = u32::from_be_bytes(key[2..].try_into().ok()?);
                let content = u.children.get(&content_key(idx))?;
                let mut attrs = meta.attrs.clone();
                attrs.remove(LANES); // the file's attrs, minus our private key
                return Some(View { blob: content.blob.clone(), attrs, children: BTreeMap::new() });
            }
        }
    }
    // Otherwise a directory: recurse real-name children.
    let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
    for (key, child) in &u.children {
        if !is_var(key) {
            if let Some(sub) = extract_node(child, l) {
                children.insert(key.clone(), Arc::new(sub));
            }
        }
    }
    if children.is_empty() {
        None
    } else {
        Some(View { blob: None, attrs: Attrs::new(), children })
    }
}

// --------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

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
    fn some(lanes: &[View]) -> Vec<Option<View>> {
        lanes.iter().cloned().map(Some).collect()
    }

    /// The union View as it only ever exists in practice: the result of
    /// applying the `f0` full frame (base = all absent). Never built directly.
    fn union_view(lanes: &[Option<View>]) -> Option<View> {
        let none: Vec<Option<View>> = vec![None; lanes.len()];
        depot::apply(None, &reverse_delta(&none, lanes))
    }
    fn assert_roundtrip(lanes: &[View]) {
        let u = union_view(&some(lanes)).expect("non-empty");
        for (i, want) in lanes.iter().enumerate() {
            assert_eq!(&extract(&u, i), want, "lane {i} mis-reconstructed");
        }
    }

    #[test]
    fn single_lane_all_versions_of_one() {
        assert_roundtrip(&[dir(&[("a.txt", leaf("a", "100644")), ("b.txt", leaf("b", "100644"))])]);
    }
    #[test]
    fn shared_one_version_differing_two() {
        let l0 = dir(&[("same", leaf("x", "100644")), ("diff", leaf("v0", "100644"))]);
        let l1 = dir(&[("same", leaf("x", "100644")), ("diff", leaf("v1", "100644"))]);
        assert_roundtrip(&[l0, l1]);
    }
    #[test]
    fn nested_dirs_partial_overlap() {
        let l0 = dir(&[
            ("src", dir(&[("m.rs", leaf("fn m0", "100644")), ("l.rs", leaf("L", "100644"))])),
            ("R", leaf("r0", "100644")),
        ]);
        let l1 = dir(&[
            ("src", dir(&[("m.rs", leaf("fn m1", "100644")), ("l.rs", leaf("L", "100644"))])),
            ("R", leaf("r0", "100644")),
        ]);
        let l2 = dir(&[("src", dir(&[("m.rs", leaf("fn m0", "100644"))]))]);
        assert_roundtrip(&[l0, l1, l2]);
    }
    #[test]
    fn mode_difference_is_a_distinct_version() {
        assert_roundtrip(&[
            dir(&[("s", leaf("#!/bin/sh", "100644"))]),
            dir(&[("s", leaf("#!/bin/sh", "100755"))]),
        ]);
    }
    #[test]
    fn file_in_one_lane_dir_in_another() {
        let l0 = dir(&[("x", leaf("i am a file", "100644"))]);
        let l1 = dir(&[("x", dir(&[("in", leaf("dir entry", "100644"))]))]);
        assert_roundtrip(&[l0, l1]);
    }
    #[test]
    fn empty_tree_lane_round_trips() {
        let empty = View::default();
        let full = dir(&[("a", leaf("a", "100644"))]);
        let u = union_view(&some(&[empty.clone(), full.clone()])).unwrap();
        assert_eq!(extract(&u, 0), empty);
        assert_eq!(extract(&u, 1), full);
    }
    #[test]
    fn absent_in_a_lane_stays_absent() {
        assert_roundtrip(&[dir(&[("only0", leaf("0", "100644"))]), dir(&[("only1", leaf("1", "100644"))])]);
    }
    #[test]
    fn many_lanes_wide_bitmap() {
        let lanes: Vec<View> = (0..20)
            .map(|i| dir(&[("common", leaf("shared", "100644")), ("per", leaf(&format!("l{i}"), "100644"))]))
            .collect();
        assert_roundtrip(&lanes);
    }
    #[test]
    fn arbitrary_attrs_round_trip_exactly() {
        let bare = View { blob: Some("x".as_bytes().into()), attrs: Attrs::new(), children: BTreeMap::new() };
        let mut a = Attrs::new();
        a.insert(b"mode".to_vec(), b"100644".to_vec());
        a.insert(b"xattr".to_vec(), b"whatever".to_vec());
        let rich = View { blob: Some("x".as_bytes().into()), attrs: a, children: BTreeMap::new() };
        assert_roundtrip(&[dir(&[("bare", bare)]), dir(&[("bare", rich)])]);
    }

    /// A real two-state transition — the encoder's actual job. Build the
    /// newest state as `f0`, then the reverse delta to the older state, and
    /// reconstruct BOTH by applying the chain newest-first. Exercises the
    /// walk's pruning and the version reindex on a changed path.
    fn assert_transition(older: &[View], newer: &[View]) {
        let (o, n) = (some(older), some(newer));
        let f0 = reverse_delta(&vec![None; n.len()], &n);
        let rec = reverse_delta(&n, &o); // base = newer, target = older
        // Newest state from f0.
        let u_new = depot::apply(None, &f0).expect("f0 non-empty");
        for (i, want) in newer.iter().enumerate() {
            assert_eq!(&extract(&u_new, i), want, "newer lane {i}");
        }
        // Older state by applying the reverse delta on top.
        let u_old = depot::apply(Some(&u_new), &rec).expect("older non-empty");
        for (i, want) in older.iter().enumerate() {
            assert_eq!(&extract(&u_old, i), want, "older lane {i}");
        }
    }

    #[test]
    fn transition_one_lane_advances() {
        // Two lanes; between states only lane 1's `diff` file moves. Lane 0
        // and the shared file are byte-identical, so the walk prunes them.
        let older = [
            dir(&[("same", leaf("x", "100644")), ("d", leaf("old", "100644"))]),
            dir(&[("same", leaf("x", "100644")), ("d", leaf("l1-old", "100644"))]),
        ];
        let newer = [
            dir(&[("same", leaf("x", "100644")), ("d", leaf("old", "100644"))]),
            dir(&[("same", leaf("x", "100644")), ("d", leaf("l1-new", "100644"))]),
        ];
        assert_transition(&older, &newer);
    }

    #[test]
    fn transition_add_and_remove_paths() {
        // A file appears in one state and a whole subdir disappears — checks
        // the add (Set) and the parent-level tombstone removal paths.
        let older = [dir(&[("keep", leaf("k", "100644")), ("gone", dir(&[("f", leaf("f", "100644"))]))])];
        let newer = [dir(&[("keep", leaf("k", "100644")), ("added", leaf("a", "100644"))])];
        assert_transition(&older, &newer);
    }

    /// A pure lane-set change (a lane joins a variant whose content already
    /// exists) emits the `\0m` bitmap update but NO `\0v` content leaf — the
    /// stable min-bit key means the content is inherited, not rewritten.
    #[test]
    fn lane_joins_variant_emits_meta_not_content() {
        // older: lane0 f=X, lane1 f=Y (two variants). newer: lane1 f=X too
        // (both share X; Y is gone). The reverse delta rebuilding older from
        // newer must Set Y back and re-point lane1's bit, but must NOT carry
        // a fresh copy of X's content — X is unchanged.
        let older = [dir(&[("f", leaf("X", "100644"))]), dir(&[("f", leaf("Y", "100644"))])];
        let newer = [dir(&[("f", leaf("X", "100644"))]), dir(&[("f", leaf("X", "100644"))])];
        let (o, n) = (some(&older), some(&newer));
        let rec = reverse_delta(&n, &o); // rebuild older from newer
        // Find the delta node at path "f" and inspect its version children.
        let fnode = rec.root.children.get(b"f".as_slice()).expect("f changed");
        let mut set_content = 0;
        for (k, ch) in &fnode.children {
            if k.len() >= 2 && k[0] == VAR && k[1] == CONTENT {
                // Only Y (min-bit key = lane 1) may be Set back; X's key
                // (lane 0) must be identity/absent — content unchanged.
                if matches!(ch.blob, BlobOp::Set(_)) {
                    set_content += 1;
                }
            }
        }
        assert_eq!(set_content, 1, "only the reintroduced variant's content is Set, not X's");
        // And it round-trips.
        assert_transition(&older, &newer);
    }

    /// Adding a lane leaves every CONTENT node byte-identical — only the
    /// bitmap (meta) siblings change.
    #[test]
    fn adding_a_lane_touches_only_bitmaps_not_content() {
        let a = dir(&[("f", leaf("shared", "100644")), ("g", leaf("A", "100644"))]);
        let b = dir(&[("f", leaf("shared", "100644")), ("g", leaf("B", "100644"))]);
        let c = dir(&[("f", leaf("shared", "100644")), ("g", leaf("A", "100644"))]); // == a's g
        let before = union_view(&some(&[a.clone(), b.clone()])).unwrap();
        let after = union_view(&some(&[a.clone(), b.clone(), c.clone()])).unwrap();

        fn contents(v: &View, path: Vec<u8>, out: &mut BTreeMap<Vec<u8>, Option<Bytes>>) {
            for (k, ch) in &v.children {
                let mut p = path.clone();
                p.extend_from_slice(k);
                if k.len() >= 2 && k[0] == VAR && k[1] == CONTENT {
                    out.insert(p, ch.blob.clone());
                } else if !is_var(k) {
                    contents(ch, p, out);
                }
            }
        }
        let (mut cb, mut ca) = (BTreeMap::new(), BTreeMap::new());
        contents(&before, vec![], &mut cb);
        contents(&after, vec![], &mut ca);
        assert_eq!(cb, ca, "adding a lane changed content nodes, not just bitmaps");
        for (i, want) in [&a, &b, &c].iter().enumerate() {
            assert_eq!(&extract(&after, i), *want);
        }
    }
}
