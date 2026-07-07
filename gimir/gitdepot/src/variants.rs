//! Union-variant representation of parallel lane trees (design of
//! 2026-07). All lanes live in ONE tree keyed by real path. At each file
//! path its distinct versions are stored, and — crucially — a version's
//! **content** and its **(mode, lane-bitmap)** live in SEPARATE sibling
//! keys, so adding a lane rewrites only the small bitmap sibling, never
//! the content node (the content stays byte-identical, so the reverse
//! delta and zstd both stay small). A file identical across all lanes is
//! a single version whose bitmap names every lane — stored uniformly, no
//! "equal vs differs" special case.
//!
//! Layout at a file path `P` (a node with only `\0`-led children):
//!   * `\0v<idx>` → a node whose blob IS the version's content, nothing
//!     else. Byte-stable across lane-membership changes.
//!   * `\0m<idx>` → a node with no blob, attrs `{mode, lanes}` — the
//!     version's mode and lane bitmap. This is the only thing a new lane
//!     touches.
//! Directories are ordinary real-name nodes. Git forbids empty
//! directories, so a node's children are either real names (a directory)
//! or `\0`-led keys (a file's versions) — never ambiguous; a file-vs-dir
//! clash across lanes is representable (version children for the file
//! lanes + real-name children for the directory lanes).
//!
//! `union` builds the treewide structure while grouping, per path, the
//! objects at that path into (mode, content) versions with lane bitmaps;
//! `extract` walks the union and at each file picks the one version whose
//! bitmap has the wanted lane's bit — giving that lane's git tree back
//! exactly (round-trip property tests below). No base, no delta-of-delta,
//! no base-switching.

use depot::{Attrs, Bytes, Name, View};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Marker byte leading every non-path key. Real git path segments never
/// contain NUL, so these cannot collide with a filename.
const VAR: u8 = 0;
const CONTENT: u8 = b'v';
const META: u8 = b'm';
/// Attr on a `\0m<idx>` node: the version's lane bitmap. Leads with `\0`
/// so it can never collide with a real git attr (git attrs — `mode` and
/// any future addition — are plain ASCII), letting the meta node carry
/// the file's whole attr set verbatim alongside the bitmap.
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

/// Build the union-variant tree of `lanes` (`lanes[i]` = lane i's git tree,
/// or `None`). `None` if no lane is present.
pub fn union(lanes: &[Option<View>]) -> Option<View> {
    let refs: Vec<Option<&View>> = lanes.iter().map(Option::as_ref).collect();
    union_node(&refs)
}

fn union_node(lanes: &[Option<&View>]) -> Option<View> {
    // A version = a distinct (attrs, content); its value is the lane bitmap.
    let mut versions: BTreeMap<(Attrs, Bytes), Vec<u8>> = BTreeMap::new();
    let mut dir_names: BTreeSet<Name> = BTreeSet::new();
    let mut any = false;
    for (i, v) in lanes.iter().enumerate() {
        let Some(v) = v else { continue };
        any = true;
        if let Some(content) = &v.blob {
            set_bit(versions.entry((v.attrs.clone(), content.clone())).or_default(), i);
        } else {
            dir_names.extend(v.children.keys().cloned());
        }
    }
    if !any {
        return None;
    }
    let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
    for (idx, ((file_attrs, content), bm)) in versions.into_iter().enumerate() {
        let idx = idx as u32;
        // Content node: blob only, byte-stable across lane changes.
        children.insert(
            content_key(idx),
            Arc::new(View { blob: Some(content), attrs: Attrs::new(), children: BTreeMap::new() }),
        );
        // Meta sibling: the file's whole attr set + bitmap. Adding a lane
        // rewrites only the bitmap; the file attrs travel verbatim so the
        // round-trip is exact for any attrs, not just `mode`.
        let mut attrs = file_attrs;
        attrs.insert(LANES.to_vec(), bm);
        children.insert(
            meta_key(idx),
            Arc::new(View { blob: None, attrs, children: BTreeMap::new() }),
        );
    }
    for name in dir_names {
        let sub: Vec<Option<&View>> = lanes
            .iter()
            .map(|v| {
                v.and_then(|v| {
                    if v.blob.is_none() {
                        v.children.get(&name).map(Arc::as_ref)
                    } else {
                        None
                    }
                })
            })
            .collect();
        if let Some(u) = union_node(&sub) {
            children.insert(name, Arc::new(u));
        }
    }
    Some(View { blob: None, attrs: Attrs::new(), children })
}

/// Reconstruct lane `l`'s git tree. A present lane contributing nothing at
/// the root is the empty tree, so the top level returns that, not "absent".
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
                // The file's attrs are everything on the meta node except
                // our private bitmap key.
                let mut attrs = meta.attrs.clone();
                attrs.remove(LANES);
                return Some(View {
                    blob: content.blob.clone(),
                    attrs,
                    children: BTreeMap::new(),
                });
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
            children: entries
                .iter()
                .map(|(n, v)| (n.as_bytes().to_vec(), Arc::new(v.clone())))
                .collect(),
        }
    }
    fn assert_roundtrip(lanes: &[View]) {
        let opt: Vec<Option<View>> = lanes.iter().cloned().map(Some).collect();
        let u = union(&opt).expect("non-empty");
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
        assert_roundtrip(&[dir(&[("s", leaf("#!/bin/sh", "100644"))]), dir(&[("s", leaf("#!/bin/sh", "100755"))])]);
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
        let u = union(&[Some(empty.clone()), Some(full.clone())]).unwrap();
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

    /// A file with no attrs round-trips to no attrs (not `{mode:[]}`), and
    /// an arbitrary extra attr survives — the meta node carries the whole
    /// attr set verbatim, so the contract holds beyond just `mode`.
    #[test]
    fn arbitrary_attrs_round_trip_exactly() {
        let bare = View { blob: Some("x".as_bytes().into()), attrs: Attrs::new(), children: BTreeMap::new() };
        let mut a = Attrs::new();
        a.insert(b"mode".to_vec(), b"100644".to_vec());
        a.insert(b"xattr".to_vec(), b"whatever".to_vec());
        let rich = View { blob: Some("x".as_bytes().into()), attrs: a, children: BTreeMap::new() };
        assert_roundtrip(&[dir(&[("bare", bare)]), dir(&[("bare", rich)])]);
    }

    /// The point of the split: adding a lane leaves every CONTENT node
    /// byte-identical — only the bitmap (meta) siblings change.
    #[test]
    fn adding_a_lane_touches_only_bitmaps_not_content() {
        let a = dir(&[("f", leaf("shared", "100644")), ("g", leaf("A", "100644"))]);
        let b = dir(&[("f", leaf("shared", "100644")), ("g", leaf("B", "100644"))]);
        let c = dir(&[("f", leaf("shared", "100644")), ("g", leaf("A", "100644"))]); // == a's g
        let before = union(&[Some(a.clone()), Some(b.clone())]).unwrap();
        let after = union(&[Some(a.clone()), Some(b.clone()), Some(c.clone())]).unwrap();

        // Collect every content node (\0v*) keyed by its variant slot.
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
        let mut cb = BTreeMap::new();
        let mut ca = BTreeMap::new();
        contents(&before, vec![], &mut cb);
        contents(&after, vec![], &mut ca);
        // Every content node present before is byte-identical after (adding
        // lane c introduced no new content — it reused a's version of g and
        // the shared f — so the content maps are equal).
        assert_eq!(cb, ca, "adding a lane changed content nodes, not just bitmaps");

        // And it still round-trips.
        for (i, want) in [&a, &b, &c].iter().enumerate() {
            assert_eq!(&extract(&after, i), *want);
        }
    }
}
