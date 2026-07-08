//! The union encoder, working straight off the git object store — no
//! materialized per-commit trees, no blob content held in memory.
//!
//! A lane's tree is a tree OID. To diff the advancing lane old→new, or to
//! walk a dying lane away, the encoder reads tree objects by oid on demand
//! ([`Objects::tree`]) and prunes any subtree whose oid is unchanged in
//! O(1) — git's own content addressing is the structural sharing, so an
//! identical subtree is one object and never re-read. The persistent
//! skeleton holds, per path, the file variants as `(mode, blob-oid)` slots
//! with lane bitmaps (see [`crate::reslot`]) plus subdirectory children —
//! oids and bits only, independent of blob size. Blob CONTENT is fetched by
//! oid ([`Objects::blob`]) only when a `\0v` node is actually emitted into a
//! frame; it is never retained.
//!
//! [`Encoder::advance`] emits the FORWARD delta (advance the previous union
//! state to the new one); the driver overlays it onto a running full-state
//! byte blob ([`depot::stream::overlay_full`]) and derives the chain's
//! newest-first reverse record from two full-states
//! ([`depot::stream::diff_stream`]) — so no whole union is ever materialized
//! as a `View`.

use std::collections::{BTreeMap, BTreeSet};

use depot::{Attrs, Bytes, Layer, Name, Node, View};

use crate::reslot::{Bitmap, Occupant, Slots};
use crate::variants::{content_key, content_view, leaf_delta, meta_key, meta_view};
use crate::Result;

/// A tree entry as it stands for one lane at one path: the git mode, the
/// object oid (hex), and whether it is a subdirectory (mode `40000`).
#[derive(Clone, Debug)]
pub struct Ent {
    pub mode: Vec<u8>,
    pub oid: String,
    pub is_dir: bool,
}

impl Ent {
    /// A directory entry pointing at a tree oid (the shape a lane's whole
    /// tree presents at the root).
    pub fn dir(oid: String) -> Ent {
        Ent { mode: b"40000".to_vec(), oid, is_dir: true }
    }
}

/// The git object store, addressed by oid. `tree` yields a directory's
/// entries (a shared handle — implementations cache parsed trees by oid so
/// a content-addressed tree is fetched once, not re-read per traversal);
/// `blob` yields a leaf's raw content (not cached — fetched only to emit).
pub trait Objects {
    fn tree(&mut self, oid: &str) -> Result<std::sync::Arc<Vec<(Name, Ent)>>>;
    fn blob(&mut self, oid: &str) -> Result<Bytes>;
}

/// Parse the raw bytes of a git tree object into `(name, entry)` pairs.
/// Format per entry: `<octal-mode> <name>\0<20-byte oid>`.
pub fn parse_tree(raw: &[u8]) -> Result<Vec<(Name, Ent)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let sp = raw[i..].iter().position(|&b| b == b' ').map(|p| i + p).ok_or_else(bad)?;
        let mode = raw[i..sp].to_vec();
        i = sp + 1;
        let nul = raw[i..].iter().position(|&b| b == 0).map(|p| i + p).ok_or_else(bad)?;
        let name = raw[i..nul].to_vec();
        i = nul + 1;
        if i + 20 > raw.len() {
            return Err(bad());
        }
        let oid = hex(&raw[i..i + 20]);
        i += 20;
        let is_dir = mode == b"40000";
        out.push((name, Ent { mode, oid, is_dir }));
    }
    Ok(out)
}

fn bad() -> crate::Error {
    crate::Error::Chain("malformed git tree object".into())
}
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A variant's identity: `(mode, blob-oid)`. Two files are the same variant
/// iff both match — content identity is the oid, no bytes compared.
type VarKey = (Vec<u8>, String);

/// One lane's change this revision: `(lane, old_entry, new_entry)` at the
/// root (the whole tree). A dying lane has `new = None`; a lane's birth has
/// `old = None`.
pub type Trans<'a> = (usize, Option<&'a Ent>, Option<&'a Ent>);

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
fn clear_bit(bm: &mut [u8], i: usize) {
    let byte = i / 8;
    if byte < bm.len() {
        bm[byte] &= !(1 << (i % 8));
    }
}

fn occ_meta(o: &Occupant<VarKey>) -> View {
    let mut attrs = Attrs::new();
    attrs.insert(b"mode".to_vec(), o.id.0.clone());
    meta_view(&attrs, &o.bitmap)
}
/// The `\0v` content leaf for an occupant — its blob fetched by oid.
fn occ_content(obj: &mut dyn Objects, o: &Occupant<VarKey>) -> Result<View> {
    Ok(content_view(&obj.blob(&o.id.1)?))
}

/// The new variant set at a path: the skeleton's current variants (all
/// lanes), with each transitioning lane's bit moved to whatever FILE it now
/// carries here (removed if it no longer has a file here). Non-transitioning
/// lanes' bits stay in the slots untouched.
fn new_variant_set(slots: &Slots<VarKey>, trans: &[Trans]) -> BTreeMap<VarKey, Bitmap> {
    let mut set: BTreeMap<VarKey, Bitmap> = BTreeMap::new();
    for (_, occ) in slots.iter() {
        set.insert(occ.id.clone(), occ.bitmap.clone());
    }
    for (lane, _old, new) in trans {
        for bm in set.values_mut() {
            clear_bit(bm, *lane);
        }
        if let Some(e) = new {
            if !e.is_dir {
                set_bit(set.entry((e.mode.clone(), e.oid.clone())).or_default(), *lane);
            }
        }
    }
    set.retain(|_, bm| bm.iter().any(|&b| b != 0));
    set
}

/// Forward delta at one path: reslot the file variants and recurse into the
/// changed directory children, reading tree objects only where a subtree oid
/// actually changed. The returned node, applied to the previous union state,
/// yields the new one.
fn advance_node(skel: &mut Skel, trans: &[Trans], obj: &mut dyn Objects) -> Result<Node> {
    let mut out = Node::keep();

    // File variants at this path.
    if !skel.slots.is_empty() || trans.iter().any(|(_, o, n)| is_file(*o) || is_file(*n)) {
        let new_set = new_variant_set(&skel.slots, trans);
        for ch in skel.slots.reslot(&new_set) {
            // forward: build `after` (new) from `before` (old) — the change
            // this revision makes. The chain's reverse record is derived
            // downstream by `diff_stream(new_full, old_full)`.
            let bc = opt_content(obj, ch.before.as_ref())?;
            let ac = opt_content(obj, ch.after.as_ref())?;
            let c = leaf_delta(bc.as_ref(), ac.as_ref());
            if !c.is_identity() {
                out.children.insert(content_key(ch.slot), c);
            }
            let bm = ch.before.as_ref().map(occ_meta);
            let am = ch.after.as_ref().map(occ_meta);
            let m = leaf_delta(bm.as_ref(), am.as_ref());
            if !m.is_identity() {
                out.children.insert(meta_key(ch.slot), m);
            }
        }
    }

    // Directory children: read each dir-transitioning lane's old/new tree
    // (skipping any whose oid is unchanged — that subtree is identical), diff
    // by name, and recurse only the children that changed.
    let mut child_trans: BTreeMap<Name, Vec<(usize, Option<Ent>, Option<Ent>)>> = BTreeMap::new();
    for (lane, o, n) in trans {
        let od = o.filter(|e| e.is_dir);
        let nd = n.filter(|e| e.is_dir);
        if od.is_none() && nd.is_none() {
            continue; // a file (or absent) on both sides — no children here
        }
        if let (Some(a), Some(b)) = (od, nd) {
            if a.oid == b.oid {
                continue; // identical subtree — the O(1) oid prune
            }
        }
        let om: BTreeMap<Name, Ent> = match od {
            Some(e) => obj.tree(&e.oid)?.iter().cloned().collect(),
            None => BTreeMap::new(),
        };
        let nm: BTreeMap<Name, Ent> = match nd {
            Some(e) => obj.tree(&e.oid)?.iter().cloned().collect(),
            None => BTreeMap::new(),
        };
        let names: BTreeSet<&Name> = om.keys().chain(nm.keys()).collect();
        for name in names {
            let oc = om.get(name);
            let nc = nm.get(name);
            let changed = match (oc, nc) {
                (Some(a), Some(b)) => a.oid != b.oid || a.is_dir != b.is_dir,
                _ => true,
            };
            if changed {
                child_trans.entry(name.clone()).or_default().push((*lane, oc.cloned(), nc.cloned()));
            }
        }
    }
    for (name, sub) in child_trans {
        let sr: Vec<Trans> = sub.iter().map(|(l, o, n)| (*l, o.as_ref(), n.as_ref())).collect();
        let (cnode, empty) = {
            let child = skel.children.entry(name.clone()).or_default();
            let cn = advance_node(child, &sr, obj)?;
            (cn, child.is_empty())
        };
        if empty {
            skel.children.remove(&name);
        }
        if !cnode.is_identity() {
            out.children.insert(name, cnode);
        }
    }
    Ok(out)
}

/// Materialize the current skeleton state as the union View (fetching each
/// live variant's content). The `f0`/seal head is then `diff(None, view)` —
/// see [`Encoder::full`] for why it is produced that way.
fn full_view(skel: &Skel, obj: &mut dyn Objects) -> Result<View> {
    let mut children: BTreeMap<Name, std::sync::Arc<View>> = BTreeMap::new();
    for (slot, occ) in skel.slots.iter() {
        children.insert(content_key(slot), std::sync::Arc::new(occ_content(obj, occ)?));
        children.insert(meta_key(slot), std::sync::Arc::new(occ_meta(occ)));
    }
    for (name, child) in &skel.children {
        children.insert(name.clone(), std::sync::Arc::new(full_view(child, obj)?));
    }
    Ok(View { blob: None, attrs: Attrs::new(), children })
}

fn is_file(e: Option<&Ent>) -> bool {
    e.is_some_and(|e| !e.is_dir)
}
fn opt_content(obj: &mut dyn Objects, occ: Option<&Occupant<VarKey>>) -> Result<Option<View>> {
    match occ {
        Some(o) => Ok(Some(occ_content(obj, o)?)),
        None => Ok(None),
    }
}

/// The stateful union encoder, driven off the git object store.
#[derive(Default)]
pub struct Encoder {
    root: Skel,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder::default()
    }

    /// Apply this revision's lane transitions and return the forward delta
    /// advancing the previous union state to the new one — the input the
    /// mmap updater overlays onto the running full-state. Reads tree objects
    /// for the changed subtrees only.
    pub fn advance(&mut self, trans: &[Trans], obj: &mut dyn Objects) -> Result<Layer> {
        Ok(Layer { root: advance_node(&mut self.root, trans, obj)? })
    }

    /// The forward full delta of the current state — the `f0` head and every
    /// seal boundary. Produced as `diff(None, materialized-view)` (not by
    /// emitting nodes directly) so it is byte-identical to the anchor the
    /// reader recomputes for a cold frame — `diff(None, reconstructed-view)`.
    /// Any non-canonical divergence there fails the frame's refPrefix check.
    pub fn full(&self, obj: &mut dyn Objects) -> Result<Layer> {
        Ok(depot::diff(None, Some(&full_view(&self.root, obj)?)))
    }

    /// (total variant slots, total directory nodes) in the live skeleton —
    /// for instrumentation. Both are bounded by the current union's size.
    pub fn stats(&self) -> (usize, usize) {
        fn go(s: &Skel, slots: &mut usize, nodes: &mut usize) {
            *nodes += 1;
            *slots += s.slots.iter().count();
            for c in s.children.values() {
                go(c, slots, nodes);
            }
        }
        let (mut slots, mut nodes) = (0, 0);
        go(&self.root, &mut slots, &mut nodes);
        (slots, nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variants::extract;
    use depot::stream::{diff_stream_holes, overlay_full};
    use std::collections::HashMap;

    /// Empty positive full-state (a present empty root) — what the driver
    /// seeds the running blob with before the first revision.
    fn empty_full() -> Vec<u8> {
        depot::codec::encode(&depot::diff(None, Some(&View::default())))
    }
    /// Overlay a forward delta onto the running full-state blob.
    fn overlay(full: &[u8], fwd: &Layer) -> Vec<u8> {
        let mut out = Vec::new();
        overlay_full(full, &depot::codec::encode(fwd), &mut out).unwrap();
        out
    }
    /// The union View a full-state blob resolves to.
    fn view_of(full: &[u8]) -> View {
        depot::apply(None, &depot::codec::decode(full).unwrap()).unwrap_or_default()
    }

    /// An in-memory object store: oid → tree entries or blob bytes.
    #[derive(Default)]
    struct Mem {
        trees: HashMap<String, Vec<(Name, Ent)>>,
        blobs: HashMap<String, Bytes>,
    }
    impl Objects for Mem {
        fn tree(&mut self, oid: &str) -> Result<std::sync::Arc<Vec<(Name, Ent)>>> {
            Ok(std::sync::Arc::new(self.trees.get(oid).cloned().unwrap_or_default()))
        }
        fn blob(&mut self, oid: &str) -> Result<Bytes> {
            Ok(self.blobs.get(oid).cloned().unwrap_or_else(|| Bytes::from(&b""[..])))
        }
    }
    impl Mem {
        fn blob_ent(&mut self, content: &str) -> Ent {
            let oid = format!("b_{content}");
            self.blobs.insert(oid.clone(), content.as_bytes().into());
            Ent { mode: b"100644".to_vec(), oid, is_dir: false }
        }
        /// Register a tree by a caller-chosen oid and return its dir entry.
        fn tree_ent(&mut self, oid: &str, entries: Vec<(&str, Ent)>) -> Ent {
            self.trees.insert(oid.to_string(), entries.into_iter().map(|(n, e)| (n.as_bytes().to_vec(), e)).collect());
            Ent::dir(oid.to_string())
        }
    }

    /// A lane's git tree, for building the oracle: name → content|subtree.
    fn extract_lane(u: &View, l: usize) -> View {
        extract(u, l)
    }

    #[test]
    fn two_lanes_shared_and_divergent() {
        let mut m = Mem::default();
        // lane 0: {shared: "x", f: "A"}; lane 1: {shared: "x", f: "B"}.
        let a = m.blob_ent("A");
        let b = m.blob_ent("B");
        let x = m.blob_ent("x");
        let t0 = m.tree_ent("t0", vec![("shared", x.clone()), ("f", a.clone())]);
        let t1 = m.tree_ent("t1", vec![("shared", x.clone()), ("f", b.clone())]);

        let mut enc = Encoder::new();
        // Birth both lanes (one advance each, from empty), driving the
        // running full-state by streaming overlay.
        let mut full = empty_full();
        full = overlay(&full, &enc.advance(&[(0, None, Some(&t0))], &mut m).unwrap());
        full = overlay(&full, &enc.advance(&[(1, None, Some(&t1))], &mut m).unwrap());
        // The overlay-maintained full-state matches the materialized oracle.
        assert_eq!(depot::codec::encode(&enc.full(&mut m).unwrap()), full);
        let u = view_of(&full);

        let l0 = extract_lane(&u, 0);
        let l1 = extract_lane(&u, 1);
        assert_eq!(l0.children.len(), 2);
        assert_eq!(l0.children[b"f".as_slice()].blob.as_deref(), Some(&b"A"[..]));
        assert_eq!(l1.children[b"f".as_slice()].blob.as_deref(), Some(&b"B"[..]));
        assert_eq!(l0.children[b"shared".as_slice()].blob.as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn edit_forward_advances_and_reverse_reconstructs() {
        let mut m = Mem::default();
        let old = m.blob_ent("old");
        let new = m.blob_ent("new");
        let x = m.blob_ent("x");
        let t_old = m.tree_ent("told", vec![("shared", x.clone()), ("f", old.clone())]);
        let t_new = m.tree_ent("tnew", vec![("shared", x.clone()), ("f", new.clone())]);

        let mut enc = Encoder::new();
        let mut full = empty_full();
        full = overlay(&full, &enc.advance(&[(0, None, Some(&t_old))], &mut m).unwrap()); // state old
        let old_full = full.clone();
        // Forward delta advances old→new; overlay must match the oracle.
        full = overlay(&full, &enc.advance(&[(0, Some(&t_old), Some(&t_new))], &mut m).unwrap());
        assert_eq!(depot::codec::encode(&enc.full(&mut m).unwrap()), full, "overlay != materialized");
        let new_full = full;

        // newest (state new)
        assert_eq!(extract(&view_of(&new_full), 0).children[b"f".as_slice()].blob.as_deref(), Some(&b"new"[..]));
        // The chain's reverse record (hole-diff of the two full-states),
        // overlaid on the new full-state, rebuilds state old — only `f`
        // changed, `shared` inherited.
        let mut rev = Vec::new();
        diff_stream_holes(&new_full, &old_full, &mut rev).unwrap();
        let mut back = Vec::new();
        overlay_full(&new_full, &rev, &mut back).unwrap();
        let cur = Some(view_of(&back));
        let l = extract(cur.as_ref().unwrap(), 0);
        assert_eq!(l.children[b"f".as_slice()].blob.as_deref(), Some(&b"old"[..]));
        assert_eq!(l.children[b"shared".as_slice()].blob.as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn nested_subtree_prune_and_change() {
        let mut m = Mem::default();
        let l = m.blob_ent("L");
        let m0 = m.blob_ent("m0");
        let m1 = m.blob_ent("m1");
        // src/{l, m}; only m changes between the two states.
        let src0 = m.tree_ent("src0", vec![("l", l.clone()), ("m", m0.clone())]);
        let src1 = m.tree_ent("src1", vec![("l", l.clone()), ("m", m1.clone())]);
        let r0 = m.tree_ent("r0", vec![("src", src0)]);
        let r1 = m.tree_ent("r1", vec![("src", src1)]);

        let mut enc = Encoder::new();
        let mut full = empty_full();
        full = overlay(&full, &enc.advance(&[(0, None, Some(&r0))], &mut m).unwrap());
        let old_full = full.clone();
        full = overlay(&full, &enc.advance(&[(0, Some(&r0), Some(&r1))], &mut m).unwrap());
        assert_eq!(depot::codec::encode(&enc.full(&mut m).unwrap()), full, "overlay != materialized");
        let new_full = full;
        // newest: src/m == m1
        let nl = extract(&view_of(&new_full), 0);
        assert_eq!(nl.children[b"src".as_slice()].children[b"m".as_slice()].blob.as_deref(), Some(&b"m1"[..]));
        // reverse record (overlaid on the new full-state) rebuilds the old
        // state — src/l pruned by oid.
        let mut rev = Vec::new();
        diff_stream_holes(&new_full, &old_full, &mut rev).unwrap();
        let mut back = Vec::new();
        overlay_full(&new_full, &rev, &mut back).unwrap();
        let old_view = view_of(&back);
        let ol = extract(&old_view, 0);
        assert_eq!(ol.children[b"src".as_slice()].children[b"m".as_slice()].blob.as_deref(), Some(&b"m0"[..]));
        assert_eq!(ol.children[b"src".as_slice()].children[b"l".as_slice()].blob.as_deref(), Some(&b"L"[..]));
    }

    #[test]
    fn parse_tree_roundtrips_a_real_entry() {
        // "100644 a\0" + 20 bytes + "40000 d\0" + 20 bytes
        let mut raw = Vec::new();
        raw.extend_from_slice(b"100644 a\0");
        raw.extend_from_slice(&[0x11; 20]);
        raw.extend_from_slice(b"40000 d\0");
        raw.extend_from_slice(&[0x22; 20]);
        let ents = parse_tree(&raw).unwrap();
        assert_eq!(ents.len(), 2);
        assert_eq!(ents[0].0, b"a");
        assert_eq!(ents[0].1.oid, "11".repeat(20));
        assert!(!ents[0].1.is_dir);
        assert_eq!(ents[1].0, b"d");
        assert!(ents[1].1.is_dir);
        assert_eq!(ents[1].1.oid, "22".repeat(20));
    }
}
