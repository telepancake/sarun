//! Union-variant representation of parallel lane trees (the design of
//! 2026-07): store all lanes in ONE tree keyed by real path, and at each
//! file store its versions as children under `\0`-prefixed subkeys, each
//! version carrying a bitmap of the lanes that hold it. A file identical
//! across all lanes is a single variant with an all-lanes bitmap — the
//! representation is uniform, so there is no "equal vs differs" special
//! case (`always store variants, even just one`).
//!
//! Directories are ordinary real-name nodes. Git forbids empty
//! directories, so a node's children are either all real names (a
//! directory) or all `\0` subkeys (a file's variants) — never ambiguous
//! (a file-vs-directory clash at one path across lanes is the one mixed
//! case, and it is representable: variant children for the file lanes
//! plus real-name children for the directory lanes).
//!
//! The problem decouples: `union` builds the treewide structure while
//! grouping, per path, the objects at that path into (content, bitmap)
//! variants; `extract` walks the union and, at each file, picks the one
//! variant whose bitmap has the wanted lane's bit — giving that lane's
//! git tree back byte-for-byte (round-trip property test below).

use depot::{Attrs, Name, View};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Marker byte for a variant subkey. Real git path segments never contain
/// NUL, so a child key beginning with it cannot collide with a filename.
const VAR: u8 = 0;
/// Attr key holding a variant's lane bitmap. `\0`-led, so it cannot
/// collide with a git leaf attr (`mode`, …).
const LANES: &[u8] = b"\0lanes";

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
/// or `None` if lane i has no tree). `None` if no lane is present.
pub fn union(lanes: &[Option<View>]) -> Option<View> {
    let refs: Vec<Option<&View>> = lanes.iter().map(Option::as_ref).collect();
    union_node(&refs)
}

fn union_node(lanes: &[Option<&View>]) -> Option<View> {
    // Group leaf versions by (full leaf attrs, content); collect the names
    // of every subdirectory any lane has here.
    let mut variants: BTreeMap<(Vec<(Name, Vec<u8>)>, Vec<u8>), Vec<u8>> = BTreeMap::new();
    let mut dir_names: BTreeSet<Name> = BTreeSet::new();
    let mut any = false;
    for (i, v) in lanes.iter().enumerate() {
        let Some(v) = v else { continue };
        any = true;
        if let Some(content) = &v.blob {
            let attrs: Vec<(Name, Vec<u8>)> =
                v.attrs.iter().map(|(k, val)| (k.clone(), val.clone())).collect();
            set_bit(variants.entry((attrs, content.to_vec())).or_default(), i);
        } else {
            dir_names.extend(v.children.keys().cloned());
        }
    }
    if !any {
        return None;
    }
    let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
    for (idx, ((attr_pairs, content), bm)) in variants.into_iter().enumerate() {
        let mut key = vec![VAR];
        key.extend_from_slice(&(idx as u32).to_be_bytes());
        let mut attrs: Attrs = attr_pairs.into_iter().collect();
        attrs.insert(LANES.to_vec(), bm);
        children.insert(
            key,
            Arc::new(View { blob: Some(content.into()), attrs, children: BTreeMap::new() }),
        );
    }
    for name in dir_names {
        // A lane contributes this child only where it is itself a directory.
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

/// Reconstruct lane `l`'s git tree from a union-variant tree. A present
/// lane that contributes nothing at the root is the empty tree (git's
/// `4b825dc…`), so the top level returns that rather than "absent".
pub fn extract(u: &View, l: usize) -> View {
    extract_node(u, l).unwrap_or_default()
}

fn extract_node(u: &View, l: usize) -> Option<View> {
    // A file for lane l iff some variant child carries its bit.
    for (key, child) in &u.children {
        if key.first() == Some(&VAR) {
            if let Some(bm) = child.attrs.get(LANES) {
                if bit(bm, l) {
                    let mut attrs = child.attrs.clone();
                    attrs.remove(LANES);
                    return Some(View { blob: child.blob.clone(), attrs, children: BTreeMap::new() });
                }
            }
        }
    }
    // Otherwise a directory: recurse the real-name children.
    let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
    for (key, child) in &u.children {
        if key.first() != Some(&VAR) {
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
        let children = entries
            .iter()
            .map(|(n, v)| (n.as_bytes().to_vec(), Arc::new(v.clone())))
            .collect();
        View { blob: None, attrs: Attrs::new(), children }
    }

    /// Every lane round-trips: extract(union(lanes), i) == lanes[i].
    fn assert_roundtrip(lanes: &[View]) {
        let opt: Vec<Option<View>> = lanes.iter().cloned().map(Some).collect();
        let u = union(&opt).expect("non-empty");
        for (i, want) in lanes.iter().enumerate() {
            assert_eq!(&extract(&u, i), want, "lane {i} mis-reconstructed");
        }
    }

    #[test]
    fn single_lane_is_all_variants_of_one() {
        assert_roundtrip(&[dir(&[("a.txt", leaf("a", "100644")), ("b.txt", leaf("b", "100644"))])]);
    }

    #[test]
    fn shared_file_one_variant_differing_file_two() {
        let l0 = dir(&[("same", leaf("x", "100644")), ("diff", leaf("v0", "100644"))]);
        let l1 = dir(&[("same", leaf("x", "100644")), ("diff", leaf("v1", "100644"))]);
        assert_roundtrip(&[l0, l1]);
    }

    #[test]
    fn nested_dirs_partial_overlap() {
        let l0 = dir(&[
            ("src", dir(&[("main.rs", leaf("fn main0", "100644")), ("lib.rs", leaf("L", "100644"))])),
            ("README", leaf("r0", "100644")),
        ]);
        let l1 = dir(&[
            ("src", dir(&[("main.rs", leaf("fn main1", "100644")), ("lib.rs", leaf("L", "100644"))])),
            ("README", leaf("r0", "100644")),
        ]);
        let l2 = dir(&[("src", dir(&[("main.rs", leaf("fn main0", "100644"))]))]);
        assert_roundtrip(&[l0, l1, l2]);
    }

    #[test]
    fn mode_difference_is_a_distinct_variant() {
        let l0 = dir(&[("s", leaf("#!/bin/sh", "100644"))]);
        let l1 = dir(&[("s", leaf("#!/bin/sh", "100755"))]); // same content, exec bit
        assert_roundtrip(&[l0, l1]);
    }

    #[test]
    fn file_in_one_lane_dir_in_another() {
        let l0 = dir(&[("x", leaf("i am a file", "100644"))]);
        let l1 = dir(&[("x", dir(&[("inside", leaf("i am a dir entry", "100644"))]))]);
        assert_roundtrip(&[l0, l1]);
    }

    #[test]
    fn empty_tree_lane_round_trips() {
        let empty = View::default();
        let full = dir(&[("a", leaf("a", "100644"))]);
        // union of an empty tree and a full tree; lane 0 extracts empty.
        let opt = vec![Some(empty.clone()), Some(full.clone())];
        let u = union(&opt).unwrap();
        assert_eq!(extract(&u, 0), empty);
        assert_eq!(extract(&u, 1), full);
    }

    #[test]
    fn absent_in_a_lane_stays_absent() {
        // lane 1 lacks `only0`, has `only1`.
        let l0 = dir(&[("only0", leaf("0", "100644"))]);
        let l1 = dir(&[("only1", leaf("1", "100644"))]);
        assert_roundtrip(&[l0, l1]);
    }

    #[test]
    fn many_lanes_wide_bitmap() {
        // 20 lanes (bitmap spans 3 bytes); half share a file, half differ.
        let lanes: Vec<View> = (0..20)
            .map(|i| {
                dir(&[
                    ("common", leaf("shared", "100644")),
                    ("per", leaf(&format!("lane{i}"), "100644")),
                ])
            })
            .collect();
        assert_roundtrip(&lanes);
    }
}
