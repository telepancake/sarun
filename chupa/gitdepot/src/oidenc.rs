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

use std::collections::BTreeMap;

use depot::{BlobOp, Bytes, Layer, Name, Node};

use crate::geostack::GeoStack;
use crate::layer::{self, dir_key, file_key, variant_node, Mode};
use crate::reslot::{Bitmap, Occupant, SlotChange, Slots};
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
    /// Object fetches actually issued — the O(new)-update work measure.
    fn reads(&self) -> usize {
        0
    }
}

/// Parse the raw bytes of a git tree object into `(name, entry)` pairs.
/// Format per entry: `<octal-mode> <name>\0<oid-width bytes>` — the width
/// is the repo's hash format (20 = SHA-1, 32 = SHA-256).
pub fn parse_tree_oids(raw: &[u8], olen: usize) -> Result<Vec<(Name, Ent)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let sp = raw[i..].iter().position(|&b| b == b' ').map(|p| i + p).ok_or_else(bad)?;
        let mode = raw[i..sp].to_vec();
        i = sp + 1;
        let nul = raw[i..].iter().position(|&b| b == 0).map(|p| i + p).ok_or_else(bad)?;
        let name = raw[i..nul].to_vec();
        i = nul + 1;
        if i + olen > raw.len() {
            return Err(bad());
        }
        let oid = hex(&raw[i..i + olen]);
        i += olen;
        let is_dir = mode == b"40000";
        out.push((name, Ent { mode, oid, is_dir }));
    }
    Ok(out)
}

/// [`parse_tree_oids`] for a SHA-1 repo (the historical shape).
pub fn parse_tree(raw: &[u8]) -> Result<Vec<(Name, Ent)>> {
    parse_tree_oids(raw, 20)
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

/// A per-path lane transition (a file appearing / changing / vanishing at a
/// path for one lane), owned — as distributed by [`distribute_children`].
type OwnedTrans = (usize, Option<Ent>, Option<Ent>);

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
    // Clear FIRST, set SECOND — order-independent, because one lane may
    // arrive as two events at the same name (a dir→file flip is a remove of
    // the dir-side key plus an add of the file-side key in git's tree
    // order): a sequential clear-then-set per event would erase the bit the
    // earlier event just set.
    for (lane, _old, _new) in trans {
        for bm in set.values_mut() {
            clear_bit(bm, *lane);
        }
    }
    for (lane, _old, new) in trans {
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
            let content = variant_content(obj, &bo.id)?;
            let bm = eff(&bo.bitmap, prev_live);
            Ok(Some(variant_node(&content, occ_mode_id(&bo.id), bm.as_deref())))
        }
        (Some(bo), Some(ao)) => {
            let mut node = Node::keep();
            if bo.id.1 != ao.id.1 {
                node.blob = BlobOp::Set(variant_content(obj, &bo.id)?.into());
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

/// A variant's frame content: the blob bytes fetched by oid — except a
/// GITLINK, whose "content" IS the oid hex itself (the submodule commit
/// lives in another repository and must never be fetched here).
fn variant_content(obj: &mut dyn Objects, id: &VarKey) -> Result<Bytes> {
    if occ_mode_id(id) == Mode::Gitlink {
        return Ok(id.1.as_bytes().to_vec().into());
    }
    obj.blob(&id.1)
}

/// git's tree-entry order (`base_name_compare`): bytewise on the names,
/// with the just-past-the-end character being `/` for a directory and NUL
/// for a file — the order tree objects are STORED in, so two cached trees
/// diff by a straight 2-way merge with no maps and no copies.
fn git_entry_cmp(an: &[u8], ad: bool, bn: &[u8], bd: bool) -> std::cmp::Ordering {
    let min = an.len().min(bn.len());
    match an[..min].cmp(&bn[..min]) {
        std::cmp::Ordering::Equal => {}
        o => return o,
    }
    let ca = an.get(min).copied().unwrap_or(if ad { b'/' } else { 0 });
    let cb = bn.get(min).copied().unwrap_or(if bd { b'/' } else { 0 });
    ca.cmp(&cb)
}

/// One lane's old→new directory diff: 2-way merge of the two SORTED cached
/// tree slices (git stores tree entries in `base_name_compare` order), with
/// the "mode matters as much as oid" rule — a pure mode change (e.g. the
/// historical 100664→100644 flip) keeps the blob oid, so it is invisible
/// unless mode is compared too. `emit(name, old, new)` gets borrowed
/// entries; only CHANGED names are ever touched beyond the walk itself.
fn diff_sorted<'e>(
    old: &'e [(Name, Ent)],
    new: &'e [(Name, Ent)],
    mut emit: impl FnMut(&'e [u8], Option<&'e Ent>, Option<&'e Ent>),
) {
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let ord = if i == old.len() {
            std::cmp::Ordering::Greater
        } else if j == new.len() {
            std::cmp::Ordering::Less
        } else {
            let (an, a) = &old[i];
            let (bn, b) = &new[j];
            git_entry_cmp(an, a.is_dir, bn, b.is_dir)
        };
        match ord {
            std::cmp::Ordering::Less => {
                emit(&old[i].0, Some(&old[i].1), None);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                emit(&new[j].0, None, Some(&new[j].1));
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let (a, b) = (&old[i].1, &new[j].1);
                if a.oid != b.oid || a.is_dir != b.is_dir || a.mode != b.mode {
                    emit(&old[i].0, Some(a), Some(b));
                }
                i += 1;
                j += 1;
            }
        }
    }
}

/// Recursively distribute the revision's root transitions into per-PATH file
/// transitions (§6, the git side of the lockstep): `full path → [(lane, old
/// entry, new entry)]` for every path where some lane's FILE appears, changes,
/// or vanishes. O(changed): an unchanged subtree is pruned by oid at every
/// level, and a changed directory is diffed by 2-way merge over the two
/// SORTED cached tree slices — never copied into maps. Owned data is created
/// only for the changed paths recorded in `out`.
fn collect_trans(
    trans: &[Trans],
    obj: &mut dyn Objects,
    path: &mut Vec<u8>,
    out: &mut BTreeMap<Vec<u8>, Vec<OwnedTrans>>,
) -> Result<()> {
    // Pass 1: resolve each lane's dir sides (oid-pruned), HOLDING the cached
    // tree Arcs for this frame — borrows below stay valid across the cache's
    // own eviction.
    let mut sides: Vec<(usize, Option<std::sync::Arc<Vec<(Name, Ent)>>>, Option<std::sync::Arc<Vec<(Name, Ent)>>>)> =
        Vec::new();
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
        let oa = match od {
            Some(e) => Some(obj.tree(&e.oid)?),
            None => None,
        };
        let na = match nd {
            Some(e) => Some(obj.tree(&e.oid)?),
            None => None,
        };
        sides.push((*lane, oa, na));
    }
    // Pass 2: per lane, 2-way merge the sorted slices; group the CHANGED
    // names across lanes (borrowed keys — the only per-entry work at
    // unchanged names is the comparison in the merge itself).
    let mut changed: BTreeMap<&[u8], Vec<(usize, Option<&Ent>, Option<&Ent>)>> = BTreeMap::new();
    for (lane, oa, na) in &sides {
        let old: &[(Name, Ent)] = oa.as_deref().map(|v| &v[..]).unwrap_or(&[]);
        let new: &[(Name, Ent)] = na.as_deref().map(|v| &v[..]).unwrap_or(&[]);
        diff_sorted(old, new, |name, oc, nc| {
            changed.entry(name).or_default().push((*lane, oc, nc));
        });
    }
    for (name, sub) in changed {
        let base = path.len();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name);
        if sub.iter().any(|(_, o, n)| is_file(*o) || is_file(*n)) {
            out.insert(
                path.clone(),
                sub.iter().map(|(l, o, n)| (*l, o.cloned(), n.cloned())).collect(),
            );
        }
        collect_trans(&sub, obj, path, out)?;
        path.truncate(base);
    }
    Ok(())
}

/// The §9 stable path→shard router: the TOP `bits` of a 64-bit FNV-1a of
/// the full git path (the `\0slot` variant tag is never part of the hash),
/// so every version of a path lands in the same shard and the split is
/// stable across a re-shard. `bits = 0` ⇒ a single shard. The hash is
/// swappable without a format change (§9).
pub fn shard_of(path: &[u8], bits: u32) -> usize {
    if bits == 0 {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in path {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h >> (64 - bits)) as usize
}

/// One shard's per-path file transitions for a revision — the routed slice
/// of the §6 git side, opaque to callers (paths + owned entries; `Send`,
/// so a §9 shard thread can own it).
pub struct PathTrans(BTreeMap<Vec<u8>, Vec<OwnedTrans>>);

impl PathTrans {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The §6 git side once per revision, routed §9-style: distribute the lane
/// transitions into per-path file transitions (O(changed), oid-pruned) and
/// split them by [`shard_of`] into `2^bits` per-shard sets.
pub fn route_trans(
    trans: &[Trans],
    obj: &mut dyn Objects,
    bits: u32,
) -> Result<Vec<PathTrans>> {
    let mut all: BTreeMap<Vec<u8>, Vec<OwnedTrans>> = BTreeMap::new();
    collect_trans(trans, obj, &mut Vec::new(), &mut all)?;
    let mut per: Vec<BTreeMap<Vec<u8>, Vec<OwnedTrans>>> =
        (0..1usize << bits).map(|_| BTreeMap::new()).collect();
    for (path, t) in all {
        let s = shard_of(&path, bits);
        per[s].insert(path, t);
    }
    Ok(per.into_iter().map(PathTrans).collect())
}

/// Descend `out` along the `/`-separated `path` (creating bare `dir_key` Keep
/// nodes) and insert `node` at `file_key(leaf, slot)`.
fn put_variant(out: &mut Node, path: &[u8], slot: u32, node: Node) {
    let parts: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
    let mut cur = out;
    for dir in &parts[..parts.len() - 1] {
        cur = cur.children.entry(dir_key(dir)).or_insert_with(Node::keep);
    }
    cur.children.insert(file_key(parts[parts.len() - 1], slot), node);
}

/// The FORWARD state-delta node for one slot change — applied onto the current
/// content-free state it produces the new one. The mirror of
/// [`variant_reverse_node`], with the oid hex as the content and the bitmap
/// always explicit (the resident state is omission-independent; the all-ones
/// omission is applied only on emission into a frame, against that revision's
/// live set).
fn variant_forward_node(ch: &SlotChange<VarKey>) -> Option<Node> {
    match (&ch.before, &ch.after) {
        (None, None) => None,
        (Some(_), None) => Some(Node::hole()),
        (None, Some(ao)) => {
            Some(variant_node(ao.id.1.as_bytes(), occ_mode_id(&ao.id), Some(&ao.bitmap)))
        }
        (Some(bo), Some(ao)) => {
            let mut node = Node::keep();
            if bo.id.1 != ao.id.1 {
                node.blob = BlobOp::Set(ao.id.1.as_bytes().into());
            }
            set_mode_tag(&mut node, occ_mode_id(&ao.id), occ_mode_id(&bo.id));
            if bo.bitmap != ao.bitmap {
                node.children.insert(
                    layer::LANES.to_vec(),
                    Node { blob: BlobOp::Set(ao.bitmap.as_slice().into()), ..Node::keep() },
                );
            }
            (!node.is_identity() || !node.children.is_empty()).then_some(node)
        }
    }
}

fn is_file(e: Option<&Ent>) -> bool {
    e.is_some_and(|e| !e.is_dir)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn walk_err(e: depot::walk::DecodeError) -> crate::Error {
    crate::Error::Chain(format!("state stream: {e:?}"))
}

/// Emit one directory level of the seal record from the folded state bytes:
/// the header and every name are span-copied verbatim (they are identical in
/// state and record); each file variant is rewritten by [`emit_variant`].
fn emit_dir(cur: &mut depot::walk::Cursor, obj: &mut dyn Objects, live: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let h0 = cur.pos();
    let n = cur.node().map_err(walk_err)?;
    out.extend_from_slice(&cur.buf()[h0..cur.pos()]);
    for _ in 0..n.child_count {
        let n0 = cur.pos();
        let name = cur.name().map_err(walk_err)?;
        out.extend_from_slice(&cur.buf()[n0..cur.pos()]);
        match layer::classify(name) {
            Some(layer::Kind::Dir(_)) => emit_dir(cur, obj, live, out)?,
            Some(layer::Kind::File(..)) => emit_variant(cur, obj, live, out)?,
            None => {
                // Never produced by the encoder; preserve verbatim.
                let a = cur.pos();
                cur.skip().map_err(walk_err)?;
                out.extend_from_slice(&cur.buf()[a..cur.pos()]);
            }
        }
    }
    Ok(())
}

/// Rewrite one variant node from state form (blob = oid hex, bitmap always
/// explicit) to record form (blob = content — the oid itself for a GITLINK —
/// `lanes` trimmed and omitted when all-ones vs `live`). The flags byte and
/// the mode-tag children are copied verbatim; meta children stay in their
/// stored (canonical) order.
fn emit_variant(cur: &mut depot::walk::Cursor, obj: &mut dyn Objects, live: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let f0 = cur.pos();
    let vn = cur.node().map_err(walk_err)?;
    let flags = cur.buf()[f0];
    let oid_hex = vn.blob.unwrap_or(&[]);
    // Children: remember spans; find the lanes bitmap and gitlink-ness.
    let buf = cur.buf();
    let mut kids: Vec<(usize, usize, usize, Option<&[u8]>)> = Vec::with_capacity(vn.child_count as usize);
    let mut is_gitlink = false;
    for _ in 0..vn.child_count {
        let s0 = cur.pos();
        let cname = cur.name().map_err(walk_err)?;
        let h = cur.pos();
        let cn = cur.node().map_err(walk_err)?;
        for _ in 0..cn.child_count {
            cur.name().map_err(walk_err)?;
            cur.skip().map_err(walk_err)?;
        }
        let s1 = cur.pos();
        if cname == layer::TAG_GITLINK {
            is_gitlink = true;
        }
        let bm = (cname == layer::LANES).then(|| cn.blob.unwrap_or(&[]));
        kids.push((s0, h, s1, bm));
    }
    let bitmap = kids.iter().find_map(|&(.., bm)| bm).unwrap_or(&[]);
    let ebm = eff(bitmap, live);
    let content: Bytes = if is_gitlink {
        oid_hex.to_vec().into()
    } else {
        let oid = std::str::from_utf8(oid_hex)
            .map_err(|_| crate::Error::Chain("state: non-utf8 oid".into()))?;
        obj.blob(oid)?
    };
    out.push(flags);
    put_varint(out, content.len() as u64);
    out.extend_from_slice(&content);
    let omitted = ebm.is_none() as u64;
    put_varint(out, vn.child_count - omitted);
    for &(s0, h, s1, bm) in &kids {
        match (bm, &ebm) {
            (Some(_), None) => {} // all-ones: lanes child omitted
            (Some(_), Some(trimmed)) => {
                // name span verbatim, node re-emitted with the trimmed blob.
                out.extend_from_slice(&buf[s0..h]);
                out.push(buf[h]); // flags: BLOB_SET, no attrs — same shape
                put_varint(out, trimmed.len() as u64);
                out.extend_from_slice(trimmed);
                put_varint(out, 0);
            }
            (None, _) => out.extend_from_slice(&buf[s0..s1]), // tag child verbatim
        }
    }
    Ok(())
}

/// delta ∘ delta for the stack (§4 compose — holes survive), at the byte level.
fn compose2(lower: Vec<u8>, upper: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    depot::stream::compose_stream(&lower, &upper, &mut out)
        .expect("compose_stream on canonical state layers");
    out
}

/// The canonical bytes of the empty state (an empty union is a real object).
fn empty_state() -> Vec<u8> {
    depot::codec::encode(&Layer { root: Node::keep() })
}

/// The stateful union encoder (§5). Its resident state is the byte encoding:
/// `refprefix` — the sealed full-state, canonical §2 layer bytes of the
/// content-free union (the git oid hex in each variant's content slot, lane
/// bitmaps explicit) — plus the live forward-delta `stack`, geometric-compacted
/// (70% rule) with `compose_stream`. Current state is read by ONE lockstep
/// stream over `refprefix` + stack ([`layer::visit_stacked`]); a full union is
/// never built to make the next delta, and no whole-repo node tree exists.
pub struct Encoder {
    refprefix: Vec<u8>,
    stack: GeoStack<Vec<u8>>,
}

/// One variant read back from a stored boundary union, to seed the encoder for
/// an incremental update: its path, the SLOT KEY it occupied (read from the
/// stored frame so a prepended reverse delta reconstructs that frame exactly),
/// its git mode octal + blob oid (the `(mode, oid)` identity, sourced from the
/// boundary lane trees per §6 — never by hashing stored content), and its lane
/// bitmap (an omitted `lanes` child is expanded to the boundary live set by the
/// caller before seeding).
pub struct SeedVariant {
    pub path: Vec<u8>,
    pub slot: u32,
    pub mode: Vec<u8>,
    pub oid: String,
    pub bitmap: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder { refprefix: empty_state(), stack: GeoStack::new() }
    }

    /// Reconstruct an encoder whose state IS a stored boundary union: place
    /// each variant back at its recorded slot with its `(mode, oid)` identity
    /// and bitmap. The result equals the encoder state at the end of the
    /// original encode, so `advance`-ing new revisions onto it and prepending
    /// the reverse deltas reproduces the stored boundary byte-for-byte.
    pub fn seed(variants: Vec<SeedVariant>) -> Encoder {
        let mut root = Node::keep();
        for v in variants {
            let mode = Mode::from_octal(&v.mode).unwrap_or(Mode::File);
            put_variant(&mut root, &v.path, v.slot, variant_node(v.oid.as_bytes(), mode, Some(&v.bitmap)));
        }
        Encoder { refprefix: depot::codec::encode(&Layer { root }), stack: GeoStack::new() }
    }

    /// Apply this revision's lane transitions and return the reverse delta
    /// rebuilding the PREVIOUS state from the new one — each older chain
    /// record (removals as holes). §6, both sides of the lockstep:
    ///
    /// - **git side** — [`collect_trans`] distributes the lane transitions into
    ///   per-path file transitions, O(changed) by the oid prune;
    /// - **stored side** — ONE lockstep stream over `refprefix` + stack
    ///   ([`layer::visit_stacked`]) yields the current variants (slot, `(mode,
    ///   oid)`, bitmap) at the changed paths — the oid read from the stream,
    ///   never by hashing — and, when the live-lane set moved (a lane born or
    ///   died), the untouched variants whose all-ones omission flips.
    ///
    /// Each changed path is reconciled by the §6 reslot; each slot change emits
    /// a reverse node into the chain record (content fetched by oid only here)
    /// and a forward node into the state delta, which is pushed on the
    /// geometric stack (§5 — push and compact, holes survive).
    pub fn advance(
        &mut self,
        trans: &[Trans],
        obj: &mut dyn Objects,
        prev_live: &[u8],
        new_live: &[u8],
    ) -> Result<Layer> {
        // §6 git side: the changed paths and their per-lane transitions.
        let mut path_trans: BTreeMap<Vec<u8>, Vec<OwnedTrans>> = BTreeMap::new();
        collect_trans(trans, obj, &mut Vec::new(), &mut path_trans)?;
        self.advance_paths(PathTrans(path_trans), obj, prev_live, new_live)
    }

    /// The stored side of [`Self::advance`] for an already-routed per-path
    /// transition set — the §9 per-shard entry point: the git side ran once
    /// globally ([`route_trans`]) and each shard advances its own state
    /// with its slice. An empty slice is fine and expected (§9 lockstep —
    /// every shard writes a layer per revision).
    pub fn advance_paths(
        &mut self,
        paths: PathTrans,
        obj: &mut dyn Objects,
        prev_live: &[u8],
        new_live: &[u8],
    ) -> Result<Layer> {
        let path_trans = paths.0;
        // §6 stored side: one lockstep pass over refPrefix + stack.
        let live_changed = trim(prev_live) != trim(new_live);
        if path_trans.is_empty() && !live_changed {
            // Untouched shard, unchanged live set: the state cannot move —
            // no scan, an empty lockstep layer (§9).
            return Ok(Layer { root: Node::keep() });
        }
        let mut rev_root = Node::keep();
        let mut cur_vars: BTreeMap<Vec<u8>, Vec<(u32, VarKey, Bitmap)>> = BTreeMap::new();
        layer::visit_stacked(&self.refprefix, self.stack.layers(), |path, mode, slot, bitmap, content| {
            // All params are borrowed slices into the state buffers; owned
            // copies are made ONLY at the O(changed) paths retained below.
            let bm = bitmap.unwrap_or(&[]);
            if path_trans.contains_key(path) {
                let oid = String::from_utf8_lossy(content).into_owned();
                cur_vars
                    .entry(path.to_vec())
                    .or_default()
                    .push((slot, (mode.octal(), oid), bm.to_vec()));
            } else if live_changed {
                // Untouched path: emit the omission flip if the variant's
                // stored `lanes` form moves with the live set.
                if let Some(child) = lanes_delta_child(&eff(bm, prev_live), &eff(bm, new_live)) {
                    let mut vn = Node::keep();
                    vn.children.insert(layer::LANES.to_vec(), child);
                    put_variant(&mut rev_root, path, slot, vn);
                }
            }
        })
        .map_err(|e| crate::Error::Chain(format!("state stream: {e:?}")))?;

        // Per-path reslot: reverse chain node + forward state node per change.
        let mut fwd_root = Node::keep();
        for (path, tlist) in &path_trans {
            let cvars = cur_vars.remove(path).unwrap_or_default();
            let mut slots: Slots<VarKey> = Slots::default();
            for (s, id, bm) in &cvars {
                slots.set(*s, Occupant { id: id.clone(), bitmap: bm.clone() });
            }
            let tr: Vec<Trans> = tlist.iter().map(|(l, o, n)| (*l, o.as_ref(), n.as_ref())).collect();
            let new_set = new_variant_set(&slots, &tr);
            let changes = slots.reslot(&new_set);
            let changed: std::collections::BTreeSet<u32> = changes.iter().map(|c| c.slot).collect();
            for ch in &changes {
                if let Some(node) = variant_reverse_node(ch, obj, prev_live, new_live)? {
                    put_variant(&mut rev_root, path, ch.slot, node);
                }
                if let Some(node) = variant_forward_node(ch) {
                    put_variant(&mut fwd_root, path, ch.slot, node);
                }
            }
            if live_changed {
                // Slots at this path the reslot did not touch still flip their
                // omission form when the live set moved.
                for (s, _, bm) in &cvars {
                    if changed.contains(s) {
                        continue;
                    }
                    if let Some(child) = lanes_delta_child(&eff(bm, prev_live), &eff(bm, new_live)) {
                        let mut vn = Node::keep();
                        vn.children.insert(layer::LANES.to_vec(), child);
                        put_variant(&mut rev_root, path, *s, vn);
                    }
                }
            }
        }

        // §5 push and compact (holes survive under compose).
        if !fwd_root.children.is_empty() {
            let delta = depot::codec::encode(&Layer { root: fwd_root });
            self.stack.push(delta, |l| l.len() as u64, compose2);
        }
        Ok(Layer { root: rev_root })
    }

    /// The §5 frame write, as ONE streaming byte operation: fold the live
    /// stack into `refPrefix` (holes dissolve — [`Self::seal_state`]) and
    /// emit the full-state head RECORD directly from the folded bytes. The
    /// content-free state and the record share their tree structure, so the
    /// emitter span-copies structural bytes verbatim and rewrites only each
    /// variant node — blob fetched by oid, written out, dropped — and its
    /// `lanes` child (trimmed, omitted when all-ones vs `live`). No View,
    /// no Layer, no node tree: peak memory is the output record plus one
    /// blob. Canonicality is enforced loudly downstream — the record doubles
    /// as the cold-frame zstd refPrefix anchor, where any non-canonical byte
    /// fails decompression.
    pub fn seal_record(&mut self, obj: &mut dyn Objects, live: &[u8]) -> Result<Vec<u8>> {
        self.seal_state();
        let mut out = Vec::with_capacity(self.refprefix.len());
        let mut cur = depot::walk::Cursor::new(&self.refprefix);
        emit_dir(&mut cur, obj, live, &mut out)?;
        Ok(out)
    }

    /// §5 seal (a frame write): flatten the live stack into a fresh
    /// `refPrefix` — overlay, holes dissolve to removals — and clear it.
    pub fn seal_state(&mut self) {
        let stack = std::mem::take(&mut self.stack);
        if let Some(combined) = stack.collapse(compose2) {
            let mut out = Vec::new();
            depot::stream::overlay_full(&self.refprefix, &combined, &mut out)
                .expect("overlay_full on canonical state layers");
            // A stack that empties the whole state resolves to the canonical
            // empty state, never zero bytes (the root always exists, §2).
            self.refprefix = if out.is_empty() { empty_state() } else { out };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use depot::{Attrs, View};
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
    /// The newest union View, decoded from the streamed §5 seal record.
    fn newest(enc: &mut Encoder, m: &mut Mem, live: &[u8]) -> View {
        let rec = enc.seal_record(m, live).unwrap();
        depot::apply(None, &depot::codec::decode(&rec).unwrap()).unwrap_or_default()
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
        let u = newest(&mut enc, &mut m, &bm(&[0, 1]));

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
        let mut cur = Some(newest(&mut enc, &mut m, &bm(&[0])));
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
        let mut cur = Some(newest(&mut enc, &mut m, &bm(&[0])));
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
