//! The union-variant REPRESENTATION and its reader. The encoder that
//! produces union frames lives in [`crate::unionenc`]; this module owns the
//! on-disk shape and how a lane is read back out of it.
//!
//! All lanes live in ONE tree keyed by real path. At a file path its
//! distinct versions are stored, and a version's **content** and its
//! **(attrs, lane-bitmap)** live in SEPARATE sibling keys so that adding a
//! lane rewrites only the small bitmap, never the content:
//!   * `\0v<slot>` — a node whose blob IS the version's content, nothing
//!     else. Byte-stable across lane-membership changes.
//!   * `\0m<slot>` — a node with no blob; attrs = the file's whole attr set
//!     plus a private `\0lanes` bitmap.
//! `<slot>` is a stable per-path key the encoder assigns (see
//! [`crate::reslot`]); the reader never interprets its value. Directories
//! are ordinary real-name nodes. Git forbids empty directories, so a node's
//! children are either real names (a directory) or `\0`-led keys (a file's
//! versions) — never ambiguous, and a file-in-one-lane / dir-in-another
//! clash is representable (both kinds of child coexist). [`extract`] pulls a
//! lane back out: at each file it picks the one version whose bitmap has
//! that lane's bit.

use depot::{Anchor, Attrs, BlobOp, Bytes, Name, Node, Presence, View};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Marker byte leading every non-path key. Real git path segments never
/// contain NUL, so these cannot collide with a filename.
const VAR: u8 = 0;
const CONTENT: u8 = b'v';
const META: u8 = b'm';
/// Attr on a `\0m<slot>` node: the version's lane bitmap. Leads with `\0` so
/// it can never collide with a real git attr (git attrs — `mode` and any
/// future addition — are plain ASCII), letting the meta node carry the
/// file's whole attr set verbatim alongside the bitmap.
const LANES: &[u8] = b"\0lanes";

pub(crate) fn content_key(slot: u32) -> Name {
    let mut k = vec![VAR, CONTENT];
    k.extend_from_slice(&slot.to_be_bytes());
    k
}
pub(crate) fn meta_key(slot: u32) -> Name {
    let mut k = vec![VAR, META];
    k.extend_from_slice(&slot.to_be_bytes());
    k
}
/// The `\0v<slot>` content-leaf View for a variant: its blob is the content,
/// nothing else.
pub(crate) fn content_view(content: &Bytes) -> View {
    View { blob: Some(content.clone()), attrs: Attrs::new(), children: BTreeMap::new() }
}
/// The `\0m<slot>` meta-leaf View for a variant: no blob; attrs = the file's
/// attrs plus the private `\0lanes` bitmap.
pub(crate) fn meta_view(attrs: &Attrs, bitmap: &[u8]) -> View {
    let mut a = attrs.clone();
    a.insert(LANES.to_vec(), bitmap.to_vec());
    View { blob: None, attrs: a, children: BTreeMap::new() }
}

fn is_var(key: &[u8]) -> bool {
    key.first() == Some(&VAR)
}
fn bit(bm: &[u8], i: usize) -> bool {
    let byte = i / 8;
    byte < bm.len() && (bm[byte] >> (i % 8)) & 1 == 1
}

/// A leaf delta child reconstructing `target` from `base` (either may be
/// absent). A `\0v`/`\0m` version node is a leaf, so this is a pure
/// blob/attrs/tombstone op with no children. [`crate::unionenc`] builds
/// every version node with it.
pub(crate) fn leaf_delta(base: Option<&View>, target: Option<&View>) -> Node {
    match target {
        None => Node::tombstone(), // gone in target → removed when rebuilt
        Some(t) => {
            if base == Some(t) {
                return Node::keep(); // identity — pruned by the caller
            }
            Node {
                presence: Presence::Live,
                // Match `depot::diff`'s canonical form exactly — the seal
                // path recomputes a cold frame's anchor as `diff(None, cur)`
                // and it MUST be byte-identical to the sealed full state, so
                // a no-blob node over a no-blob base is `Keep`, not `Remove`
                // (both reconstruct to no blob, but only `Keep` is canonical).
                blob: match (&t.blob, base.and_then(|b| b.blob.as_ref())) {
                    (Some(bytes), _) => BlobOp::Set(bytes.clone()),
                    (None, Some(_)) => BlobOp::Remove, // base had a blob → drop it
                    (None, None) => BlobOp::Keep,       // neither has a blob (a `\0m` node)
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

/// Reconstruct lane `l`'s git tree from a materialized union View. A present
/// lane contributing nothing at the root is the empty tree, so the top level
/// returns that, not "absent".
pub fn extract(u: &View, l: usize) -> View {
    extract_node(u, l).unwrap_or_default()
}

fn extract_node(u: &View, l: usize) -> Option<View> {
    // A file for lane l iff some meta sibling carries its bit.
    for (key, meta) in &u.children {
        if key.len() >= 2 && key[0] == VAR && key[1] == META {
            if meta.attrs.get(LANES).is_some_and(|bm| bit(bm, l)) {
                let slot = u32::from_be_bytes(key[2..].try_into().ok()?);
                let content = u.children.get(&content_key(slot))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(lanes: &[usize]) -> Vec<u8> {
        let mut b = Vec::new();
        for &l in lanes {
            let byte = l / 8;
            if b.len() <= byte {
                b.resize(byte + 1, 0);
            }
            b[byte] |= 1 << (l % 8);
        }
        b
    }
    fn mode(m: &str) -> Attrs {
        let mut a = Attrs::new();
        a.insert(b"mode".to_vec(), m.as_bytes().to_vec());
        a
    }
    /// Build a union file node from `(slot, attrs, content, lanes)` variants.
    fn file(vars: &[(u32, Attrs, &str, &[usize])]) -> View {
        let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
        for (slot, attrs, content, lanes) in vars {
            children.insert(content_key(*slot), Arc::new(content_view(&(*content).as_bytes().into())));
            children.insert(meta_key(*slot), Arc::new(meta_view(attrs, &bitmap(lanes))));
        }
        View { blob: None, attrs: Attrs::new(), children }
    }
    fn dir(entries: &[(&str, View)]) -> View {
        View {
            blob: None,
            attrs: Attrs::new(),
            children: entries.iter().map(|(n, v)| (n.as_bytes().to_vec(), Arc::new(v.clone()))).collect(),
        }
    }
    fn leaf(content: &str, m: &str) -> View {
        View { blob: Some(content.as_bytes().into()), attrs: mode(m), children: BTreeMap::new() }
    }

    #[test]
    fn extract_picks_the_lane_bit_variant() {
        // Path "f": variant A (slot 0) for lanes 0,2; variant B (slot 1) for
        // lane 1. Extract each lane's file.
        let u = dir(&[(
            "f",
            file(&[(0, mode("100644"), "A", &[0, 2]), (1, mode("100644"), "B", &[1])]),
        )]);
        assert_eq!(extract(&u, 0), dir(&[("f", leaf("A", "100644"))]));
        assert_eq!(extract(&u, 1), dir(&[("f", leaf("B", "100644"))]));
        assert_eq!(extract(&u, 2), dir(&[("f", leaf("A", "100644"))]));
    }

    #[test]
    fn extract_absent_lane_prunes_the_path() {
        // Lane 3 carries no variant at "f" → "f" is absent for it, and the
        // directory prunes to the empty tree.
        let u = dir(&[("f", file(&[(0, mode("100644"), "A", &[0])]))]);
        assert_eq!(extract(&u, 3), View::default());
    }

    #[test]
    fn extract_recovers_full_attrs_minus_bitmap() {
        let mut attrs = mode("100755");
        attrs.insert(b"xattr".to_vec(), b"v".to_vec());
        let u = dir(&[("s", file(&[(0, attrs.clone(), "#!/bin/sh", &[0])]))]);
        let got = extract(&u, 0);
        let want = dir(&[("s", View { blob: Some("#!/bin/sh".as_bytes().into()), attrs, children: BTreeMap::new() })]);
        assert_eq!(got, want);
    }

    #[test]
    fn extract_file_in_one_lane_dir_in_another() {
        // Same path "x": a file for lane 0, a directory for lane 1.
        let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
        let f = file(&[(0, mode("100644"), "i am a file", &[0])]);
        for (k, v) in f.children {
            children.insert(k, v);
        }
        children.insert(b"in".to_vec(), Arc::new(file(&[(0, mode("100644"), "dir entry", &[1])])));
        let u = dir(&[("x", View { blob: None, attrs: Attrs::new(), children })]);
        assert_eq!(extract(&u, 0), dir(&[("x", leaf("i am a file", "100644"))]));
        assert_eq!(extract(&u, 1), dir(&[("x", dir(&[("in", leaf("dir entry", "100644"))]))]));
    }
}
