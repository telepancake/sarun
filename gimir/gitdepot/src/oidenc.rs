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
//! [`Encoder::advance`] emits the REVERSE delta (rebuild the previous union
//! state from the new one) — the chain's newest-first record. Removals are
//! HOLES (the store occludes no host), so the reader resolves them over the
//! empty backdrop. [`Encoder::full`] is the positive full-state head.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use depot::{Attrs, BlobOp, Bytes, Layer, Name, Node, View};

use crate::layer::{self, dir_key, file_key, variant_node, Mode};
use crate::reslot::{Bitmap, Occupant, SlotChange, Slots};
use crate::Result;

/// The `Mode` of a slot occupant (its variant identity carries the octal).
fn occ_mode(o: &Occupant<VarKey>) -> Mode {
    Mode::from_octal(&o.id.0).unwrap_or(Mode::File)
}

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

/// Set `node` (a variant-delta node overlaid on the NEW variant's node) to
/// carry the mode `before`, holing the tag `after` currently shows if it is a
/// different tag. Compared by tag NAME, so an `o` (non-canonical) tag's octal
/// blob is simply overwritten to `before`'s octal.
fn set_mode_tag(node: &mut Node, before: Mode, after: Mode) {
    if before == after {
        return;
    }
    if let Some(atag) = after.tag() {
        if before.tag() != Some(atag) {
            node.children.insert(atag.to_vec(), Node::hole());
        }
    }
    if let Some(btag) = before.tag() {
        let blob = if let Mode::Other(m) = before { format!("{m:o}").into_bytes() } else { Vec::new() };
        node.children.insert(btag.to_vec(), Node { blob: BlobOp::Set(blob.into()), ..Node::keep() });
    }
}

/// Trailing-zero-trimmed view of a bitmap — its canonical form for equality
/// (`set_bit`/`clear_bit` can leave trailing zero bytes).
fn trim(b: &[u8]) -> &[u8] {
    let mut n = b.len();
    while n > 0 && b[n - 1] == 0 {
        n -= 1;
    }
    &b[..n]
}

/// The §2 effective stored bitmap for a variant, given the lanes LIVE at this
/// revision: `None` (omit the `lanes` child) when the variant is present in
/// EVERY live lane — the all-ones case — else `Some(explicit bitmap)`. The
/// reader treats an absent `lanes` child as "in every lane", and since a tree
/// is only ever extracted for a lane live at its revision, the omission
/// reconstructs SHA-exact.
fn eff(bitmap: &[u8], live: &[u8]) -> Option<Vec<u8>> {
    if trim(bitmap) == trim(live) {
        None
    } else {
        Some(trim(bitmap).to_vec())
    }
}

/// The `lanes`-child op that turns the NEW variant's effective bitmap form
/// (`e_new`) into the OLD one (`e_before`) — `None` = omit that side:
/// unchanged ⇒ no child; old omitted ⇒ hole (drop the bitmap); old explicit
/// ⇒ Set it.
fn lanes_delta_child(e_before: &Option<Vec<u8>>, e_new: &Option<Vec<u8>>) -> Option<Node> {
    if e_before == e_new {
        return None;
    }
    Some(match e_before {
        None => Node::hole(),
        Some(b) => Node { blob: BlobOp::Set(b.as_slice().into()), ..Node::keep() },
    })
}

/// The §2 reverse-delta node for one slot change: applied onto the NEW union
/// state's node at `file_key(name, slot)`, it rebuilds the PREVIOUS variant
/// (`before`). `None` ⇒ nothing to emit (identity). `prev_live`/`new_live` are
/// the lane sets live at the previous/new revisions — they drive the all-ones
/// `lanes` omission on each side.
///
/// - new-only slot (`before=None`) ⇒ a HOLE (removed when walking back);
/// - old-only slot (`after=None`) ⇒ the full previous variant node;
/// - both ⇒ a minimal delta: `Set` the old content only if the blob oid
///   moved, retag the mode if it changed, and rewrite the `lanes` child only
///   if the variant's effective (omission-aware) bitmap moved.
fn variant_reverse_node(
    ch: &SlotChange<VarKey>,
    obj: &mut dyn Objects,
    prev_live: &[u8],
    new_live: &[u8],
) -> Result<Option<Node>> {
    match (&ch.before, &ch.after) {
        (None, None) => Ok(None),
        (None, Some(_)) => Ok(Some(Node::hole())),
        (Some(bo), None) => {
            let content = obj.blob(&bo.id.1)?;
            let bm = eff(&bo.bitmap, prev_live);
            Ok(Some(variant_node(&content, occ_mode_id(&bo.id), bm.as_deref())))
        }
        (Some(bo), Some(ao)) => {
            let mut node = Node::keep();
            if bo.id.1 != ao.id.1 {
                node.blob = BlobOp::Set(obj.blob(&bo.id.1)?.into());
            }
            set_mode_tag(&mut node, occ_mode_id(&bo.id), occ_mode_id(&ao.id));
            let e_before = eff(&bo.bitmap, prev_live);
            let e_new = eff(&ao.bitmap, new_live);
            if let Some(child) = lanes_delta_child(&e_before, &e_new) {
                node.children.insert(layer::LANES.to_vec(), child);
            }
            Ok((!node.is_identity() || !node.children.is_empty()).then_some(node))
        }
    }
}

fn occ_mode_id(id: &VarKey) -> Mode {
    Mode::from_octal(&id.0).unwrap_or(Mode::File)
}

/// Distribute a directory's per-lane `(old_tree, new_tree)` transitions into
/// per-child-name transitions, reading each lane's tree object by oid and
/// pruning any subtree whose oid is unchanged (the O(1) oid prune, O(changed)
/// per commit). A child that is a file in one lane and a directory in another
/// simply accumulates both kinds of transition under its name.
#[allow(clippy::type_complexity)]
fn distribute_children(
    trans: &[Trans],
    obj: &mut dyn Objects,
) -> Result<BTreeMap<Name, Vec<(usize, Option<Ent>, Option<Ent>)>>> {
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
                // Mode matters as much as oid: a pure mode change (e.g. the
                // historical 100664→100644 flip) keeps the blob oid, so it is
                // invisible unless mode is compared too — else the stale mode
                // variant keeps this lane's bit and the tree oid diverges.
                (Some(a), Some(b)) => a.oid != b.oid || a.is_dir != b.is_dir || a.mode != b.mode,
                _ => true,
            };
            if changed {
                child_trans.entry(name.clone()).or_default().push((*lane, oc.cloned(), nc.cloned()));
            }
        }
    }
    Ok(child_trans)
}

/// Reverse delta for a DIRECTORY level (`children` = its child sub-skeletons):
/// diff each lane's old/new tree, and for every changed child NAME emit that
/// name's §2 contributions — file variants as `file_key(name, slot)` siblings
/// and its subdirectory as a bare `dir_key(name)` node. The returned node,
/// applied onto the NEW union state, rebuilds the previous one.
fn advance_dir(
    children: &mut BTreeMap<Name, Skel>,
    trans: &[Trans],
    obj: &mut dyn Objects,
    emit: bool,
    prev_live: &[u8],
    new_live: &[u8],
) -> Result<Node> {
    let mut out = Node::keep();
    let child_trans = distribute_children(trans, obj)?;
    for (name, sub_raw) in child_trans {
        let sr: Vec<Trans> = sub_raw.iter().map(|(l, o, n)| (*l, o.as_ref(), n.as_ref())).collect();
        let child = children.entry(name.clone()).or_default();

        // File variants of `name` → `file_key(name, slot)` siblings.
        if !child.slots.is_empty() || sr.iter().any(|(_, o, n)| is_file(*o) || is_file(*n)) {
            let new_set = new_variant_set(&child.slots, &sr);
            for ch in child.slots.reslot(&new_set) {
                if !emit {
                    continue;
                }
                if let Some(node) = variant_reverse_node(&ch, obj, prev_live, new_live)? {
                    out.children.insert(file_key(&name, ch.slot), node);
                }
            }
        }

        // Subdirectory of `name` → bare `dir_key(name)` node.
        let dnode = advance_dir(&mut child.children, &sr, obj, emit, prev_live, new_live)?;
        if !dnode.is_identity() {
            out.children.insert(dir_key(&name), dnode);
        }

        if child.is_empty() {
            children.remove(&name);
        }
    }
    Ok(out)
}

/// Emit, into `out`, a `lanes`-child reverse-delta for every UNTOUCHED variant
/// whose all-ones OMISSION status flips because the live-lane set changed
/// (a lane born or died) between the previous and new revision — the variants
/// the reslot transition does not report because their membership did not
/// change, yet whose stored bitmap form must change so folding reconstructs the
/// previous revision's omissions exactly. Walked over the PRE-advance skeleton;
/// touched variants are later overwritten by `advance_dir`'s own deltas.
fn remat_flips(dir: &Skel, prev_live: &[u8], new_live: &[u8], path: &mut Vec<Name>, out: &mut Node) {
    for (name, sub) in &dir.children {
        for (slot, occ) in sub.slots.iter() {
            let e_before = eff(&occ.bitmap, prev_live);
            let e_new = eff(&occ.bitmap, new_live);
            if let Some(child) = lanes_delta_child(&e_before, &e_new) {
                let mut vn = Node::keep();
                vn.children.insert(layer::LANES.to_vec(), child);
                insert_variant(out, path, &file_key(name, slot), vn);
            }
        }
        if !sub.children.is_empty() {
            path.push(name.clone());
            remat_flips(sub, prev_live, new_live, path, out);
            path.pop();
        }
    }
}

/// Descend `out` along `dirs` (creating bare `dir_key` Keep nodes) and insert
/// `node` at the leaf `key`.
fn insert_variant(out: &mut Node, dirs: &[Name], key: &[u8], node: Node) {
    let mut cur = out;
    for d in dirs {
        cur = cur.children.entry(dir_key(d)).or_insert_with(Node::keep);
    }
    cur.children.insert(key.to_vec(), node);
}

/// Merge delta tree `from` into `into`, with `from` winning on any leaf key
/// collision. Directory keys (bare names) present on both sides recurse; file
/// variant keys (`\0`-led) never collide across the two callers (touched vs
/// omission-flipped are disjoint), and `from` replaces if they ever did.
fn merge_delta(into: &mut Node, from: Node) {
    for (k, v) in from.children {
        let is_dir = matches!(layer::classify(&k), Some(layer::Kind::Dir(_)));
        match into.children.get_mut(&k) {
            Some(existing) if is_dir => merge_delta(existing, v),
            _ => {
                into.children.insert(k, v);
            }
        }
    }
}

/// The §2 variant View for an occupant — mirrors [`variant_node`] as a
/// materialized View so the folded reverse-delta state and this fresh
/// full-state are byte-identical under `diff(None, view)`. `bitmap = None`
/// omits the `lanes` child (the all-ones case).
fn variant_view(content: Bytes, mode: Mode, bitmap: Option<&[u8]>) -> View {
    let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
    if let Some(tag) = mode.tag() {
        let blob: Bytes = if let Mode::Other(m) = mode {
            format!("{m:o}").into_bytes().into()
        } else {
            (&b""[..]).into()
        };
        children.insert(tag.to_vec(), Arc::new(View { blob: Some(blob), attrs: Attrs::new(), children: BTreeMap::new() }));
    }
    if let Some(bm) = bitmap {
        children.insert(
            layer::LANES.to_vec(),
            Arc::new(View { blob: Some(bm.to_vec().into()), attrs: Attrs::new(), children: BTreeMap::new() }),
        );
    }
    View { blob: Some(content), attrs: Attrs::new(), children }
}

/// Materialize the current skeleton state as the §2 union View, with the
/// all-ones `lanes` omission applied against `live` (the lanes live at this —
/// the newest — revision). Each child name emits its file variants as
/// `file_key(name, slot)` siblings and its subtree as `dir_key(name)`. The
/// `f0`/seal head is `diff(None, view)`.
fn full_view_dir(dir: &Skel, obj: &mut dyn Objects, live: &[u8]) -> Result<View> {
    let mut children: BTreeMap<Name, Arc<View>> = BTreeMap::new();
    for (name, sub) in &dir.children {
        for (slot, occ) in sub.slots.iter() {
            let content = obj.blob(&occ.id.1)?;
            let bm = eff(&occ.bitmap, live);
            children.insert(file_key(name, slot), Arc::new(variant_view(content.into(), occ_mode(occ), bm.as_deref())));
        }
        if !sub.children.is_empty() {
            children.insert(dir_key(name), Arc::new(full_view_dir(sub, obj, live)?));
        }
    }
    Ok(View { blob: None, attrs: Attrs::new(), children })
}

fn is_file(e: Option<&Ent>) -> bool {
    e.is_some_and(|e| !e.is_dir)
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

    /// Apply this revision's lane transitions and return the reverse delta
    /// rebuilding the PREVIOUS state from the new one — each older chain
    /// record (removals as holes). Reads tree objects for the changed
    /// subtrees only. `prev_live`/`new_live` are the lane sets live at the
    /// previous/new revisions; they drive the all-ones `lanes` omission and,
    /// when they differ (a lane born/died), the untouched-variant
    /// re-materialization (`remat_flips`) so folding reconstructs the previous
    /// revision's omissions exactly.
    pub fn advance(
        &mut self,
        trans: &[Trans],
        obj: &mut dyn Objects,
        prev_live: &[u8],
        new_live: &[u8],
    ) -> Result<Layer> {
        let mut root = Node::keep();
        if trim(prev_live) != trim(new_live) {
            remat_flips(&self.root, prev_live, new_live, &mut Vec::new(), &mut root);
        }
        let changed = advance_dir(&mut self.root.children, trans, obj, true, prev_live, new_live)?;
        merge_delta(&mut root, changed); // reslot deltas win over remat flips
        Ok(Layer { root })
    }

    /// Update ONLY the skeleton for this revision — reslot the slots without
    /// fetching any blob content or building a delta. For memory measurement:
    /// it grows the skeleton to the full union with no blob traffic.
    pub fn advance_skel(&mut self, trans: &[Trans], obj: &mut dyn Objects) -> Result<()> {
        advance_dir(&mut self.root.children, trans, obj, false, &[], &[])?;
        Ok(())
    }

    /// Exact heap footprint of the live skeleton: `(nodes, slots,
    /// owned_heap_bytes, malloc_objects)`. `owned_heap_bytes` sums the
    /// capacities of every `Vec`/`String` the skeleton owns (mode, oid,
    /// bitmap, child names); `malloc_objects` counts them. BTreeMap internal
    /// node allocations are NOT included (they add on top).
    pub fn mem_report(&self) -> (usize, usize, usize, usize) {
        fn go(s: &Skel, nodes: &mut usize, slots: &mut usize, bytes: &mut usize, mallocs: &mut usize) {
            *nodes += 1;
            for (_slot, occ) in s.slots.iter() {
                *slots += 1;
                for cap in [occ.id.0.capacity(), occ.id.1.capacity(), occ.bitmap.capacity()] {
                    if cap > 0 {
                        *bytes += cap;
                        *mallocs += 1;
                    }
                }
            }
            for (name, child) in &s.children {
                if name.capacity() > 0 {
                    *bytes += name.capacity();
                    *mallocs += 1;
                }
                go(child, nodes, slots, bytes, mallocs);
            }
        }
        let (mut nodes, mut slots, mut bytes, mut mallocs) = (0, 0, 0, 0);
        go(&self.root, &mut nodes, &mut slots, &mut bytes, &mut mallocs);
        (nodes, slots, bytes, mallocs)
    }

    /// The forward full delta of the current state — the `f0` head and every
    /// seal boundary. Produced as `diff(None, materialized-view)` (not by
    /// emitting nodes directly) so it is byte-identical to the anchor the
    /// reader recomputes for a cold frame — `diff(None, reconstructed-view)`.
    /// Any non-canonical divergence there fails the frame's refPrefix check.
    pub fn full(&self, obj: &mut dyn Objects, live: &[u8]) -> Result<Layer> {
        Ok(depot::diff(None, Some(&full_view_dir(&self.root, obj, live)?)))
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
    use std::collections::HashMap;

    /// Reconstruct lane `l` from a §2 union View as a flat `path -> (mode,
    /// content)` map, via the authoritative `layer` extractor on the canonical
    /// union bytes.
    fn lane_map(u: &View, l: u32) -> BTreeMap<Vec<u8>, (Mode, Vec<u8>)> {
        let bytes = depot::codec::encode(&depot::diff(None, Some(u)));
        layer::extract_lane_entries(&bytes, l)
            .unwrap()
            .into_iter()
            .map(|(p, m, c)| (p, (m, c)))
            .collect()
    }
    fn content(u: &View, l: u32, path: &[u8]) -> Option<Vec<u8>> {
        lane_map(u, l).get(path).map(|(_, c)| c.clone())
    }

    /// A little-endian compact-lane bitmap for a set of live lanes.
    fn bm(lanes: &[usize]) -> Vec<u8> {
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
    /// The newest union View: materialize the encoder's full state at `live`.
    fn newest(enc: &Encoder, m: &mut Mem, live: &[u8]) -> View {
        depot::apply(None, &enc.full(m, live).unwrap()).unwrap_or_default()
    }
    /// Fold a reverse-delta record into the working view, resolving removal
    /// holes as tombstones over the empty backdrop (the reader's rule).
    fn apply_reverse(cur: &mut Option<View>, rec: &Layer) {
        let mut layer = rec.clone();
        fn h2t(node: &mut Node) {
            if node.anchor == depot::Anchor::Backdrop
                && node.presence == depot::Presence::Live
                && node.blob == depot::BlobOp::Keep
                && node.attrs.is_none()
                && node.children.is_empty()
            {
                *node = Node::tombstone();
                return;
            }
            for c in node.children.values_mut() {
                h2t(c);
            }
        }
        h2t(&mut layer.root);
        depot::apply_mut(cur, &layer);
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
        // Birth both lanes (one advance each, from empty); live grows {0}→{0,1}.
        enc.advance(&[(0, None, Some(&t0))], &mut m, &bm(&[]), &bm(&[0])).unwrap();
        enc.advance(&[(1, None, Some(&t1))], &mut m, &bm(&[0]), &bm(&[0, 1])).unwrap();
        let u = newest(&enc, &mut m, &bm(&[0, 1]));

        assert_eq!(lane_map(&u, 0).len(), 2);
        assert_eq!(content(&u, 0, b"f").as_deref(), Some(&b"A"[..]));
        assert_eq!(content(&u, 1, b"f").as_deref(), Some(&b"B"[..]));
        assert_eq!(content(&u, 0, b"shared").as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn edit_reverse_reconstructs_old_state() {
        let mut m = Mem::default();
        let old = m.blob_ent("old");
        let new = m.blob_ent("new");
        let x = m.blob_ent("x");
        let t_old = m.tree_ent("told", vec![("shared", x.clone()), ("f", old.clone())]);
        let t_new = m.tree_ent("tnew", vec![("shared", x.clone()), ("f", new.clone())]);

        let mut enc = Encoder::new();
        enc.advance(&[(0, None, Some(&t_old))], &mut m, &bm(&[]), &bm(&[0])).unwrap(); // birth = state old
        let rec = enc.advance(&[(0, Some(&t_old), Some(&t_new))], &mut m, &bm(&[0]), &bm(&[0])).unwrap(); // → state new

        // newest (state new)
        let mut cur = Some(newest(&enc, &mut m, &bm(&[0])));
        assert_eq!(content(cur.as_ref().unwrap(), 0, b"f").as_deref(), Some(&b"new"[..]));
        // reverse record rebuilds state old — only `f` changed, `shared`
        // inherited (holes resolve as tombstones over the empty backdrop).
        apply_reverse(&mut cur, &rec);
        assert_eq!(content(cur.as_ref().unwrap(), 0, b"f").as_deref(), Some(&b"old"[..]));
        assert_eq!(content(cur.as_ref().unwrap(), 0, b"shared").as_deref(), Some(&b"x"[..]));
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
        enc.advance(&[(0, None, Some(&r0))], &mut m, &bm(&[]), &bm(&[0])).unwrap();
        let rec = enc.advance(&[(0, Some(&r0), Some(&r1))], &mut m, &bm(&[0]), &bm(&[0])).unwrap();
        let mut cur = Some(newest(&enc, &mut m, &bm(&[0])));
        // newest: src/m == m1
        assert_eq!(content(cur.as_ref().unwrap(), 0, b"src/m").as_deref(), Some(&b"m1"[..]));
        // reverse record rebuilds the old state — src/l pruned by oid.
        apply_reverse(&mut cur, &rec);
        assert_eq!(content(cur.as_ref().unwrap(), 0, b"src/m").as_deref(), Some(&b"m0"[..]));
        assert_eq!(content(cur.as_ref().unwrap(), 0, b"src/l").as_deref(), Some(&b"L"[..]));
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
