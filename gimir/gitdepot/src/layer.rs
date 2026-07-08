//! The variant tree shape (ASSEMBLY.md §2/§3): how a union of git trees maps
//! onto `depot::codec` node names, and the classification of a node name back
//! to a git entry. The single-pass layer iterator (yielding git-order
//! entries with content byte-ranges) builds on these.
//!
//! Names stay CLEAN — no sort-hack byte baked into storage:
//!
//! - a **file** at segment `name`, version (slot) `k` → `name` + `0x00` +
//!   `varint(k)`. `0x00` never appears in a git name, so it both separates
//!   the slot and marks the node as a file variant.
//! - a **directory** `name` → `name` (bare).
//! - **meta children** UNDER a file-variant node: `lanes` (blob = the lane
//!   bitmap) and at most one mode tag `x` (executable) / `l` (symlink) /
//!   `m` (gitlink). A plain file / directory carries no mode tag.
//!
//! Meta nodes (`lanes`, `x`/`l`/`m`) carry a (possibly empty) `Set` blob so
//! they are NON-identity — an identity `[0,0]` child would be pruned by
//! `compose`/`overlay`, silently dropping the mode or bitmap. Their presence
//! (and, for `lanes`, content) is the signal; a mode tag's content is empty.
//!
//! The `lanes` child is **omitted when the bitmap is all-ones** — its absence
//! means "this variant is in EVERY live lane". That is the common case (a
//! file identical across all branches), so most variants store no bitmap. A
//! bitmap-less variant is necessarily the ONLY variant at its path (if one
//! variant covers all lanes, no sibling variant can), and `extract(lane L)`
//! always matches it. A variant with siblings always carries a `lanes` child.
//!
//! ## Order: the big side never reorders
//!
//! The container's codec bytewise order (on the clean names above) is THE
//! authoritative iteration order. The full-state is hundreds of MB — it is
//! walked in a single pass in exactly that order, never re-sorted. The git
//! trees are tiny (one directory level, a few KB), so *they* adapt:
//!
//! - the **layer iterator** emits the container in its natural bytewise order
//!   ([`container_cmp`]); no reorder, no buffering of the big side.
//! - the **git-tree iterator** yields each small git tree in that same
//!   [`container_cmp`] order — and since parsed trees are cached by oid, the
//!   sort happens ONCE at cache insertion and is free on every reuse.
//! - only **reconstructing a git tree object** (extract a lane → hash) sorts
//!   a small level back into git's [`base_name_compare`](entry_cmp) order.
//!
//! `container_cmp` differs from git order only for the file-vs-dir prefix
//! cases (git treats a dir name as `name/`; we store it bare, so bytewise a
//! bare dir sorts before same-prefix files). That divergence is confined to
//! the tiny git side, never imposed on the big full-state.

/// The directory-terminator byte git synthesizes when comparing a tree name.
const DIR: u8 = b'/';
/// The file-terminator byte git synthesizes for a blob name; also our variant
/// separator (`0x00` never occurs in a git path segment).
const NUL: u8 = 0;

/// Meta child names under a variant node.
pub const LANES: &[u8] = b"lanes";
pub const TAG_EXEC: &[u8] = b"x";
pub const TAG_SYMLINK: &[u8] = b"l";
pub const TAG_GITLINK: &[u8] = b"m";

/// A git file mode, reduced to the four cases the mode tags encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    /// `100644` — a normal file (no tag).
    File,
    /// `100755` — executable (tag `x`).
    Exec,
    /// `120000` — symlink (tag `l`).
    Symlink,
    /// `160000` — gitlink / submodule (tag `m`).
    Gitlink,
}

impl Mode {
    /// The git octal mode bytes.
    pub fn octal(self) -> &'static [u8] {
        match self {
            Mode::File => b"100644",
            Mode::Exec => b"100755",
            Mode::Symlink => b"120000",
            Mode::Gitlink => b"160000",
        }
    }
    /// The mode of a git tree entry, or `None` if it is a directory (`40000`)
    /// — directories carry no variant/tag.
    pub fn from_octal(mode: &[u8]) -> Option<Mode> {
        match mode {
            b"100644" => Some(Mode::File),
            b"100755" => Some(Mode::Exec),
            b"120000" => Some(Mode::Symlink),
            b"160000" => Some(Mode::Gitlink),
            _ => None, // 40000 (dir) or anything unexpected
        }
    }
    /// The meta-child tag for this mode, if any (a plain file has none).
    pub fn tag(self) -> Option<&'static [u8]> {
        match self {
            Mode::File => None,
            Mode::Exec => Some(TAG_EXEC),
            Mode::Symlink => Some(TAG_SYMLINK),
            Mode::Gitlink => Some(TAG_GITLINK),
        }
    }
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

fn get_varint(bytes: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            // Reject trailing bytes: the varint must consume the whole slice.
            return (i + 1 == bytes.len()).then_some(v);
        }
    }
    None
}

/// Container node name for a file variant: `name` + `0x00` + `varint(slot)`.
pub fn file_key(name: &[u8], slot: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(name.len() + 6);
    k.extend_from_slice(name);
    k.push(NUL);
    put_varint(&mut k, slot as u64);
    k
}

/// Container node name for a directory: bare `name`.
pub fn dir_key(name: &[u8]) -> Vec<u8> {
    name.to_vec()
}

/// What a sibling node name denotes.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    /// A directory; `.0` is the git name.
    Dir(&'a [u8]),
    /// A file variant; `.0` is the git name, `.1` the slot.
    File(&'a [u8], u32),
}

/// Classify a sibling node name at the file/dir level: a `\0` marks a file
/// variant (`name\0<varint slot>`); otherwise it is a bare directory name.
/// (Under a variant node the children are meta — `lanes`/`x`/`l`/`m` — and
/// are read directly, not via `classify`.)
pub fn classify(name: &[u8]) -> Option<Kind<'_>> {
    match name.iter().position(|&b| b == NUL) {
        Some(nul) => {
            let slot = get_varint(&name[nul + 1..])?;
            Some(Kind::File(&name[..nul], u32::try_from(slot).ok()?))
        }
        None => Some(Kind::Dir(name)),
    }
}

/// OUR order — the container's authoritative order — over two git entries
/// `(name, is_dir)`. Bytewise on the container key, where a file's name is
/// followed by the `0x00` variant marker and a directory's is bare. This is
/// the order the codec stores children in and the layer iterator emits; the
/// git-tree iterator sorts small git trees INTO this order so the two
/// lockstep. The slot is irrelevant to cross-entry order (two variants of one
/// file share a git name), so it is omitted here.
pub fn container_cmp(a: &[u8], a_dir: bool, b: &[u8], b_dir: bool) -> std::cmp::Ordering {
    let m = a.len().min(b.len());
    match a[..m].cmp(&b[..m]) {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }
    // The next byte of the container key past the equal prefix: a real name
    // byte if the name continues; else the file's `0x00` marker, or nothing
    // for a bare dir. `None` (dir end) < `Some(0)` (file marker) < any name
    // byte — exactly bytewise order on the container keys.
    let na = a.get(m).copied().or(if a_dir { None } else { Some(0) });
    let nb = b.get(m).copied().or(if b_dir { None } else { Some(0) });
    na.cmp(&nb)
}

/// GIT tree order over two container node names (`base_name_compare`): compare
/// git names bytewise, and when one is a prefix of the other synthesize the
/// shorter's next byte as `0x2F` for a directory or `0x00` for a file. Used
/// ONLY to reconstruct a (small) git tree object from a container subtree for
/// SHA — never imposed on the big full-state.
pub fn entry_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (an, ad) = match classify(a) {
        Some(Kind::Dir(n)) => (n, true),
        Some(Kind::File(n, _)) => (n, false),
        None => (a, false),
    };
    let (bn, bd) = match classify(b) {
        Some(Kind::Dir(n)) => (n, true),
        Some(Kind::File(n, _)) => (n, false),
        None => (b, false),
    };
    let m = an.len().min(bn.len());
    match an[..m].cmp(&bn[..m]) {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }
    let c1 = an.get(m).copied().unwrap_or(if ad { DIR } else { NUL });
    let c2 = bn.get(m).copied().unwrap_or(if bd { DIR } else { NUL });
    c1.cmp(&c2)
}

// ------------------------------------------------------------- encoder

/// A file variant node: content blob plus meta children. A mode tag and the
/// `lanes` node carry a (possibly empty) `Set` blob so they are NON-identity
/// and survive `compose`/`overlay` — an identity `[0,0]` child would be
/// pruned, silently dropping the mode or bitmap. Pass `bitmap = None` to omit
/// the `lanes` child (all-ones → every live lane).
pub fn variant_node(content: &[u8], mode: Mode, bitmap: Option<&[u8]>) -> depot::Node {
    let empty = || depot::Node { blob: depot::BlobOp::Set((&b""[..]).into()), ..depot::Node::keep() };
    let mut n = depot::Node { blob: depot::BlobOp::Set(content.into()), ..depot::Node::keep() };
    if let Some(tag) = mode.tag() {
        n.children.insert(tag.to_vec(), empty());
    }
    if let Some(bm) = bitmap {
        n.children.insert(LANES.to_vec(), depot::Node { blob: depot::BlobOp::Set(bm.into()), ..depot::Node::keep() });
    }
    n
}

/// Build the container bytes for a SINGLE lane's full tree: every file is one
/// variant at slot 0 with no bitmap (one lane ⇒ all-ones ⇒ omitted). This is
/// the initial full-state (the f0 refPrefix seed), built once; subsequent
/// commits are deltas, never a full rebuild. `entries` are
/// `(path, mode, content)` with `/`-separated paths, any order.
pub fn encode_lane(entries: &[(Vec<u8>, Mode, Vec<u8>)]) -> Vec<u8> {
    let mut root = depot::Node::keep();
    for (path, mode, content) in entries {
        put_variant(&mut root, path, 0, variant_node(content, *mode, None));
    }
    depot::codec::encode(&depot::Layer { root })
}

/// Descend the dir path (creating `Keep` dir nodes) and place `node` at the
/// leaf under `file_key(leaf, slot)`.
fn put_variant(root: &mut depot::Node, path: &[u8], slot: u32, node: depot::Node) {
    let parts: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
    let mut cur = root;
    for dir in &parts[..parts.len() - 1] {
        cur = cur.children.entry(dir_key(dir)).or_insert_with(depot::Node::keep);
    }
    cur.children.insert(file_key(parts[parts.len() - 1], slot), node);
}

/// The delta that turns one lane's tree `old` into `new`, as container bytes:
/// a changed/added file emits its full variant node, a removed file emits a
/// hole (single lane ⇒ slot 0, no bitmap). `overlay_full(encode_lane(old),
/// this)` equals `encode_lane(new)`. This is the trivial-reslot base case;
/// the multi-lane version matches variants by oid and moves lane bits.
pub fn delta_single_lane(
    old: &[(Vec<u8>, Mode, Vec<u8>)],
    new: &[(Vec<u8>, Mode, Vec<u8>)],
) -> Vec<u8> {
    use std::collections::BTreeMap;
    let om: BTreeMap<&[u8], (Mode, &[u8])> =
        old.iter().map(|(p, m, c)| (p.as_slice(), (*m, c.as_slice()))).collect();
    let nm: BTreeMap<&[u8], (Mode, &[u8])> =
        new.iter().map(|(p, m, c)| (p.as_slice(), (*m, c.as_slice()))).collect();
    let mut root = depot::Node::keep();
    for (path, (nmode, ncontent)) in &nm {
        let old = om.get(path);
        if old == Some(&(*nmode, ncontent)) {
            continue; // unchanged
        }
        let mut node = variant_node(ncontent, *nmode, None);
        // Same slot 0 ⇒ this delta OVERLAYS the old variant; an absent mode tag
        // would leave the old one in place. So when the old mode carried a tag
        // the new mode drops, hole that tag explicitly.
        if let Some((omode, _)) = old {
            if let Some(otag) = omode.tag() {
                if omode.tag() != nmode.tag() {
                    node.children.insert(otag.to_vec(), depot::Node::hole());
                }
            }
        }
        put_variant(&mut root, path, 0, node);
    }
    for path in om.keys() {
        if !nm.contains_key(path) {
            put_variant(&mut root, path, 0, depot::Node::hole());
        }
    }
    depot::codec::encode(&depot::Layer { root })
}

// ----------------------------------------------------- multi-lane reslot

use std::collections::{BTreeMap, BTreeSet};

/// A git object id, opaque here — the reslot matches variants by it and never
/// hashes stored content (§3). Tests use synthetic ids.
pub type Oid = Vec<u8>;

/// One lane's view of a path: the git mode, the tree entry's oid, and the file
/// content (needed only when a genuinely new variant must be written).
#[derive(Clone, Debug)]
pub struct LaneEntry {
    pub mode: Mode,
    pub oid: Oid,
    pub content: Vec<u8>,
}

/// A lane = one commit's tree, as `path -> entry` (paths `/`-separated, no
/// leading slash). Absent path ⇒ file not in that lane.
pub type LaneTree = BTreeMap<Vec<u8>, LaneEntry>;

/// Canonical little-endian bitmap bytes for a lane set: bit `l` = lane `l`, no
/// trailing zero byte (the highest set bit lands in the last byte). An empty
/// set ⇒ empty slice.
fn bitmap_bytes(set: &BTreeSet<u32>) -> Vec<u8> {
    let mut v = Vec::new();
    for &l in set {
        let byte = (l / 8) as usize;
        while v.len() <= byte {
            v.push(0);
        }
        v[byte] |= 1 << (l % 8);
    }
    v
}

/// A file variant's identity: the git tree entry's `(mode, oid)`. The SAME
/// blob appearing as a plain vs an executable file (a chmod) is TWO tree
/// entries → two variants, so mode is part of the identity, not just oid.
type VarId = (Mode, Oid);

/// Group `lanes` into per-path variants: `path -> [(id, content, laneset)]` in
/// first-appearance order (lane order). All lanes sharing a `(mode, oid)` at a
/// path are ONE variant (that is what makes them one variant).
fn union_groups(lanes: &[LaneTree]) -> BTreeMap<Vec<u8>, Vec<(VarId, Vec<u8>, BTreeSet<u32>)>> {
    let mut out: BTreeMap<Vec<u8>, Vec<(VarId, Vec<u8>, BTreeSet<u32>)>> = BTreeMap::new();
    for (j, tree) in lanes.iter().enumerate() {
        let j = j as u32;
        for (path, e) in tree {
            let id: VarId = (e.mode, e.oid.clone());
            let groups = out.entry(path.clone()).or_default();
            match groups.iter_mut().find(|(gid, ..)| *gid == id) {
                Some((_, _, set)) => {
                    set.insert(j);
                }
                None => {
                    let mut set = BTreeSet::new();
                    set.insert(j);
                    groups.push((id, e.content.clone(), set));
                }
            }
        }
    }
    out
}

/// Build the full-state (refPrefix seed) for a multi-lane frame: every path's
/// variants stored as siblings, slot = first-appearance rank, bitmap omitted
/// when it covers all lanes (all-ones → every live lane). Built once per frame;
/// subsequent layers are deltas via [`delta_multi_lane`].
pub fn encode_union(lanes: &[LaneTree]) -> Vec<u8> {
    let n = lanes.len() as u32;
    let groups = union_groups(lanes);
    let mut root = depot::Node::keep();
    for (path, variants) in &groups {
        for (slot, ((mode, _oid), content, set)) in variants.iter().enumerate() {
            let bm = (set.len() as u32 != n).then(|| bitmap_bytes(set));
            put_variant(&mut root, path, slot as u32, variant_node(content, *mode, bm.as_deref()));
        }
    }
    depot::codec::encode(&depot::Layer { root })
}

/// The current variants stored in `old_full`, per path: `(slot, oid, raw stored
/// bitmap bytes)`. The oid is obtained for FREE from `old_lanes` — any lane in
/// the variant's bitmap has this file at that oid — never by hashing the
/// stored content (§3).
fn current_variants(
    base: &[u8],
    stack: &[Vec<u8>],
    old_lanes: &[LaneTree],
) -> BTreeMap<Vec<u8>, Vec<(u32, VarId, Option<Vec<u8>>)>> {
    let mut cur: BTreeMap<Vec<u8>, Vec<(u32, VarId, Option<Vec<u8>>)>> = BTreeMap::new();
    visit_current(base, stack, |path, mode, slot, bitmap| {
        // A representative lane carrying this variant: the lowest set bit, or
        // (bitmap omitted ⇒ all lanes) lane 0.
        let rep = match &bitmap {
            Some(b) => (0..(b.len() * 8) as u32).find(|&i| b[(i / 8) as usize] & (1 << (i % 8)) != 0),
            None => Some(0),
        };
        let oid = rep
            .and_then(|l| old_lanes.get(l as usize))
            .and_then(|t| t.get(path))
            .map(|le| le.oid.clone())
            .expect("a variant's representative lane must carry the file");
        // mode comes from the stored variant (its mode tag), oid from the lane.
        let id: VarId = (mode, oid);
        cur.entry(path.to_vec()).or_default().push((slot, id, bitmap));
    })
    .expect("base + stack are canonical");
    cur
}

/// The delta turning the multi-lane full-state `old_full` (whose variants are
/// identified via `old_lanes`) into the union over `new_lanes`, matching
/// variants by oid and keeping slots stable (§3):
///
/// - an oid present in both ⇒ same slot; emit ONLY a `lanes`-child update if
///   the stored bitmap form changed (content/mode untouched — never re-fetched),
///   else emit nothing (pruned);
/// - an oid only in `new_lanes` ⇒ a fresh slot (past the max at that path) with
///   its full variant node (the one place content is read from git);
/// - an oid only in `old_full` ⇒ carried by no new lane ⇒ a hole at its slot.
///
/// `overlay_full(old_full, delta)` is the union over `new_lanes`: each new
/// lane extracted from it reconstructs that lane's tree exactly.
pub fn delta_multi_lane(old_full: &[u8], old_lanes: &[LaneTree], new_lanes: &[LaneTree]) -> Vec<u8> {
    delta_multi_lane_stacked(old_full, &[], old_lanes, new_lanes)
}

/// As [`delta_multi_lane`], but reads the current state from `base` overlaid
/// with the live delta `stack` (via [`visit_current`]) instead of a
/// pre-materialized full-state — the between-seals path where the union is
/// never built. `old_lanes` supplies the current variants' oids (§3).
pub fn delta_multi_lane_stacked(
    base: &[u8],
    stack: &[Vec<u8>],
    old_lanes: &[LaneTree],
    new_lanes: &[LaneTree],
) -> Vec<u8> {
    let new_n = new_lanes.len() as u32;
    let newg = union_groups(new_lanes);
    let cur = current_variants(base, stack, old_lanes);

    let mut root = depot::Node::keep();
    let paths: BTreeSet<&Vec<u8>> = newg.keys().chain(cur.keys()).collect();
    for path in paths {
        let empty_c = Vec::new();
        let empty_g = Vec::new();
        let cvars = cur.get(path).unwrap_or(&empty_c);
        let nvars = newg.get(path).unwrap_or(&empty_g);

        let old_by_id: BTreeMap<&VarId, (u32, &Option<Vec<u8>>)> =
            cvars.iter().map(|(s, id, bm)| (id, (*s, bm))).collect();
        let new_ids: BTreeSet<&VarId> = nvars.iter().map(|(id, ..)| id).collect();
        let mut next_slot = cvars.iter().map(|(s, ..)| *s + 1).max().unwrap_or(0);

        for (id, content, set) in nvars {
            let (mode, _oid) = id;
            let bm = (set.len() as u32 != new_n).then(|| bitmap_bytes(set));
            match old_by_id.get(id) {
                Some((slot, old_bm)) => {
                    if old_bm.as_deref() == bm.as_deref() {
                        continue; // unchanged — pruned
                    }
                    // Minimal update: keep content/mode, rewrite only `lanes`.
                    let mut node = depot::Node::keep();
                    let child = match &bm {
                        Some(b) => depot::Node {
                            blob: depot::BlobOp::Set(b.as_slice().into()),
                            ..depot::Node::keep()
                        },
                        None => depot::Node::hole(), // now all-ones ⇒ drop the child
                    };
                    node.children.insert(LANES.to_vec(), child);
                    put_variant(&mut root, path, *slot, node);
                }
                None => {
                    let slot = next_slot;
                    next_slot += 1;
                    put_variant(&mut root, path, slot, variant_node(content, *mode, bm.as_deref()));
                }
            }
        }
        for (slot, id, _) in cvars {
            if !new_ids.contains(id) {
                put_variant(&mut root, path, *slot, depot::Node::hole());
            }
        }
    }
    depot::codec::encode(&depot::Layer { root })
}

// ------------------------------------------- streaming current-state reader

use depot::walk::{Cursor, DecodeError as WErr};

/// A meta child that REMOVES the facet: a pure hole (backdrop-anchored, no blob,
/// no children). A backdrop node WITH a blob is a restoration (compose can
/// re-establish a holed tag), so its facet is PRESENT — only the pure hole
/// removes.
fn is_removal(n: &depot::walk::Node) -> bool {
    n.backdrop && n.blob.is_none() && n.child_count == 0
}

/// The effective current variants — the base full-state overlaid with the live
/// delta stack — yielded per file in container order WITHOUT materializing the
/// union (§3). Directories are walked in lockstep; only a TOUCHED leaf variant
/// is resolved (into a few owned facets), an untouched base subtree is walked
/// straight through. `visit` gets `(path, mode, slot, bitmap)` — content is not
/// needed for the reslot (a variant's oid comes from the lane trees).
///
/// The stack is collapsed first with `compose_stream` (holes survive); the
/// geostack keeps it to ~log(n) small layers, so the collapse and the second
/// lockstep stream stay bounded — the big `base` is never re-sorted or copied.
pub fn visit_current(
    base: &[u8],
    stack: &[Vec<u8>],
    mut visit: impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>),
) -> Result<(), WErr> {
    let mut combined = Vec::new();
    if let Some((first, rest)) = stack.split_first() {
        combined = first.clone();
        for layer in rest {
            let mut next = Vec::new();
            depot::stream::compose_stream(&combined, layer, &mut next)?; // lower, upper
            combined = next;
        }
    }
    let mut path = Vec::new();
    let mut bc = Cursor::new(base);
    if combined.is_empty() {
        let bn = bc.node()?;
        return base_subtree(&mut bc, bn.child_count, &mut path, &mut visit);
    }
    let mut dc = Cursor::new(&combined);
    let bn = bc.node()?;
    let dn = dc.node()?;
    merge_children(Some(&mut bc), bn.child_count, Some(&mut dc), dn.child_count, &mut path, &mut visit)
}

/// Read the next child name from a side with children remaining (owned copy —
/// names are short); `None` once exhausted.
fn next_name(cur: &mut Option<&mut Cursor>, remaining: &mut u64) -> Result<Option<Vec<u8>>, WErr> {
    if *remaining == 0 {
        return Ok(None);
    }
    *remaining -= 1;
    Ok(Some(cur.as_mut().unwrap().name()?.to_vec()))
}

/// Lockstep-merge one directory level of the base and the delta by container
/// order (raw name bytewise — the codec's stored order). Each cursor, when
/// present, is positioned just past its dir header (at its first child name).
fn merge_children(
    mut b: Option<&mut Cursor>,
    mut brem: u64,
    mut d: Option<&mut Cursor>,
    mut drem: u64,
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>),
) -> Result<(), WErr> {
    let mut bname = next_name(&mut b, &mut brem)?;
    let mut dname = next_name(&mut d, &mut drem)?;
    loop {
        let order = match (&bname, &dname) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(bn), Some(dn)) => bn.cmp(dn),
        };
        match order {
            std::cmp::Ordering::Less => {
                let name = bname.take().unwrap();
                base_child(b.as_mut().unwrap(), &name, path, visit)?;
                bname = next_name(&mut b, &mut brem)?;
            }
            std::cmp::Ordering::Greater => {
                let name = dname.take().unwrap();
                delta_only_child(d.as_mut().unwrap(), &name, path, visit)?;
                dname = next_name(&mut d, &mut drem)?;
            }
            std::cmp::Ordering::Equal => {
                let name = bname.take().unwrap();
                both_child(b.as_mut().unwrap(), d.as_mut().unwrap(), &name, path, visit)?;
                bname = next_name(&mut b, &mut brem)?;
                dname = next_name(&mut d, &mut drem)?;
            }
        }
    }
    Ok(())
}

/// Read a variant node's mode + bitmap facets from `cur` (positioned at the
/// node), consuming it whole. Content is skipped — not needed for reslot.
fn read_facets(cur: &mut Cursor) -> Result<(Mode, Option<Vec<u8>>), WErr> {
    let n = cur.node()?;
    let mut mode = Mode::File;
    let mut bitmap = None;
    for _ in 0..n.child_count {
        let m = cur.name()?.to_vec();
        let mn = cur.node()?;
        match m.as_slice() {
            LANES => bitmap = Some(mn.blob.unwrap_or(&[]).to_vec()),
            TAG_EXEC => mode = Mode::Exec,
            TAG_SYMLINK => mode = Mode::Symlink,
            TAG_GITLINK => mode = Mode::Gitlink,
            _ => {}
        }
        for _ in 0..mn.child_count {
            cur.name()?;
            cur.skip()?;
        }
    }
    Ok((mode, bitmap))
}

/// A child present only in the base (delta does not touch it): walk it as-is.
fn base_child(
    cur: &mut Cursor,
    name: &[u8],
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>),
) -> Result<(), WErr> {
    match classify(name) {
        Some(Kind::File(gitname, slot)) => {
            let base = push_seg(path, gitname);
            let (mode, bitmap) = read_facets(cur)?;
            visit(&path[..], mode, slot, bitmap);
            path.truncate(base);
        }
        Some(Kind::Dir(gitname)) => {
            let base = push_seg(path, gitname);
            let n = cur.node()?;
            base_subtree(cur, n.child_count, path, visit)?;
            path.truncate(base);
        }
        None => cur.skip()?,
    }
    Ok(())
}

/// Walk a whole base subtree (no delta), yielding every variant.
fn base_subtree(
    cur: &mut Cursor,
    count: u64,
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>),
) -> Result<(), WErr> {
    for _ in 0..count {
        let name = cur.name()?.to_vec();
        base_child(cur, &name, path, visit)?;
    }
    Ok(())
}

/// Read a delta file node's meta as ABSOLUTE facets (base is empty — a fresh
/// variant or a backdrop restoration), consuming the node's children, and emit
/// it if it is a real variant (carries content). A pure hole (no blob, no
/// children) is consumed and yields nothing.
fn emit_absolute_file(
    cur: &mut Cursor,
    node: &depot::walk::Node,
    gitname: &[u8],
    slot: u32,
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>),
) -> Result<(), WErr> {
    let mut mode = Mode::File;
    let mut bitmap = None;
    for _ in 0..node.child_count {
        let m = cur.name()?.to_vec();
        let mn = cur.node()?;
        if !is_removal(&mn) {
            match m.as_slice() {
                LANES => bitmap = Some(mn.blob.unwrap_or(&[]).to_vec()),
                TAG_EXEC => mode = Mode::Exec,
                TAG_SYMLINK => mode = Mode::Symlink,
                TAG_GITLINK => mode = Mode::Gitlink,
                _ => {}
            }
        }
        for _ in 0..mn.child_count {
            cur.name()?;
            cur.skip()?;
        }
    }
    if node.blob.is_some() {
        let base = push_seg(path, gitname);
        visit(&path[..], mode, slot, bitmap);
        path.truncate(base);
    }
    Ok(())
}

/// A child present only in the delta: its current value is the delta resolved
/// over nothing — a pure hole vanishes, a set (or backdrop restoration)
/// appears, a subtree recurses.
fn delta_only_child(
    cur: &mut Cursor,
    name: &[u8],
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>),
) -> Result<(), WErr> {
    match classify(name) {
        Some(Kind::File(gitname, slot)) => {
            let n = cur.node()?;
            emit_absolute_file(cur, &n, gitname, slot, path, visit)?;
        }
        Some(Kind::Dir(gitname)) => {
            // A backdrop dir is a restoration/hole: over nothing it is just its
            // own (possibly empty) positive children.
            let n = cur.node()?;
            let base = push_seg(path, gitname);
            merge_children(None, 0, Some(cur), n.child_count, path, visit)?;
            path.truncate(base);
        }
        None => cur.skip()?,
    }
    Ok(())
}

/// A child present in both. A `backdrop` delta node ERASES the base and
/// resolves over nothing (a hole → gone, a restoration → its own content); a
/// non-backdrop node overlays onto the base (keep/replace blob, override meta).
fn both_child(
    b: &mut Cursor,
    d: &mut Cursor,
    name: &[u8],
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>),
) -> Result<(), WErr> {
    match classify(name) {
        Some(Kind::File(gitname, slot)) => {
            let dn = d.node()?;
            if dn.backdrop {
                // Base erased; emit the delta node's own positive form.
                b.skip()?;
                emit_absolute_file(d, &dn, gitname, slot, path, visit)?;
                return Ok(());
            }
            // Overlay: base facets, then delta meta as overrides (deltas carry
            // only the CHANGED meta; an absent meta child keeps the base's). A
            // non-backdrop node never removes — the variant exists.
            let (mut mode, mut bitmap) = read_facets(b)?;
            for _ in 0..dn.child_count {
                let m = d.name()?.to_vec();
                let mn = d.node()?;
                let tag = match m.as_slice() {
                    LANES => {
                        bitmap = if is_removal(&mn) { None } else { Some(mn.blob.unwrap_or(&[]).to_vec()) };
                        None
                    }
                    TAG_EXEC => Some(Mode::Exec),
                    TAG_SYMLINK => Some(Mode::Symlink),
                    TAG_GITLINK => Some(Mode::Gitlink),
                    _ => None,
                };
                if let Some(t) = tag {
                    if is_removal(&mn) {
                        // Holing a tag reverts to plain ONLY if it was the
                        // active one — a hole of some other mode's leftover tag
                        // must not clobber a tag Set (or restored) by another
                        // child here.
                        if mode == t {
                            mode = Mode::File;
                        }
                    } else {
                        mode = t;
                    }
                }
                for _ in 0..mn.child_count {
                    d.name()?;
                    d.skip()?;
                }
            }
            let base = push_seg(path, gitname);
            visit(&path[..], mode, slot, bitmap);
            path.truncate(base);
        }
        Some(Kind::Dir(gitname)) => {
            let dn = d.node()?;
            if dn.backdrop {
                // Base subtree erased; the delta subtree resolves over nothing.
                b.skip()?;
                let base = push_seg(path, gitname);
                merge_children(None, 0, Some(d), dn.child_count, path, visit)?;
                path.truncate(base);
                return Ok(());
            }
            let bn = b.node()?;
            let base = push_seg(path, gitname);
            merge_children(Some(b), bn.child_count, Some(d), dn.child_count, path, visit)?;
            path.truncate(base);
        }
        None => {
            b.skip()?;
            d.skip()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------- SHA reconstruction

/// A reconstructed git tree node while rebuilding one lane's tree for its oid.
enum TNode {
    File(Mode, Vec<u8>),
    Dir(BTreeMap<Vec<u8>, TNode>),
}

fn tnode_insert(dir: &mut BTreeMap<Vec<u8>, TNode>, path: &[u8], mode: Mode, content: Vec<u8>) {
    match path.iter().position(|&b| b == b'/') {
        Some(sl) => {
            let seg = path[..sl].to_vec();
            let sub = dir.entry(seg).or_insert_with(|| TNode::Dir(BTreeMap::new()));
            if let TNode::Dir(m) = sub {
                tnode_insert(m, &path[sl + 1..], mode, content);
            }
        }
        None => {
            dir.insert(path.to_vec(), TNode::File(mode, content));
        }
    }
}

/// The git tree oid of a reconstructed level, bottom-up. Entries are ordered by
/// git's `base_name_compare` (a directory sorts as `name/`); a gitlink's blob
/// content IS its pinned commit id (matching the store's convention).
fn tree_oid(dir: &BTreeMap<Vec<u8>, TNode>) -> Result<String, WErr> {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // (git sort key, raw)
    for (name, node) in dir {
        let (mode_bytes, oid, is_dir) = match node {
            TNode::File(mode, content) => {
                let oid = if *mode == Mode::Gitlink {
                    String::from_utf8_lossy(content).into_owned()
                } else {
                    crate::git_obj_oid("blob", content)
                };
                (mode.octal().to_vec(), oid, false)
            }
            TNode::Dir(sub) => (b"40000".to_vec(), tree_oid(sub)?, true),
        };
        let mut raw = mode_bytes;
        raw.push(b' ');
        raw.extend_from_slice(name);
        raw.push(0);
        raw.extend_from_slice(&hex::decode(&oid).map_err(|_| WErr::Truncated)?);
        let mut key = name.clone();
        if is_dir {
            key.push(b'/');
        }
        entries.push((key, raw));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let body: Vec<u8> = entries.into_iter().flat_map(|(_, r)| r).collect();
    Ok(crate::git_obj_oid("tree", &body))
}

/// The canonical encoding of an EMPTY full-state (a keep root, no children).
/// `overlay_full` can emit zero bytes when a whole tree is removed; a
/// full-state must stay decodable, so callers substitute this.
pub fn empty_union() -> Vec<u8> {
    depot::codec::encode(&depot::Layer { root: depot::Node::keep() })
}

/// The git tree oid of a set of flat `(path, mode, content)` entries, built
/// bottom-up in git order. Used to hash a lane whose entries were gathered
/// across shards (§1) as well as a single union.
pub fn tree_oid_of_entries(entries: &[(Vec<u8>, Mode, Vec<u8>)]) -> Result<String, WErr> {
    let mut root = BTreeMap::new();
    for (path, mode, content) in entries {
        tnode_insert(&mut root, path, *mode, content.clone());
    }
    tree_oid(&root)
}

/// The git tree oid of a single lane tree, built directly from its entries
/// (no union) — the reference/expected value a union reconstruction must match.
pub fn lanetree_tree_oid(tree: &LaneTree) -> Result<String, WErr> {
    let entries: Vec<_> = tree.iter().map(|(p, e)| (p.clone(), e.mode, e.content.clone())).collect();
    tree_oid_of_entries(&entries)
}

/// Extract lane `lane`'s flat `(path, mode, content)` entries from a union —
/// the variant whose bitmap includes `lane` (or the sole all-ones variant) at
/// each path. Sharding gathers a lane's entries across shards before hashing.
pub fn extract_lane_entries(union: &[u8], lane: u32) -> Result<Vec<(Vec<u8>, Mode, Vec<u8>)>, WErr> {
    let mut out = Vec::new();
    visit_entries(union, |e| {
        let inl = match e.bitmap {
            Some(b) => (b.get((lane / 8) as usize).copied().unwrap_or(0) & (1 << (lane % 8))) != 0,
            None => true,
        };
        if inl {
            out.push((e.path.to_vec(), e.mode, e.content.to_vec()));
        }
    })?;
    Ok(out)
}

/// Reconstruct lane `lane`'s git tree from the union bytes and return its
/// root tree oid — the SHA-exact ground-truth check: this must equal the git
/// commit's recorded tree oid.
pub fn reconstruct_lane_tree_oid(union: &[u8], lane: u32) -> Result<String, WErr> {
    tree_oid_of_entries(&extract_lane_entries(union, lane)?)
}

// ------------------------------------------------------------ iterator

/// One file entry yielded by the layer iterator. `content`/`bitmap` borrow
/// the input buffer (an mmap) — nothing copied; `path` borrows the reused
/// accumulation buffer for the duration of the call.
pub struct Entry<'p, 'b> {
    /// Full git path, e.g. `src/main.rs` (no leading slash).
    pub path: &'p [u8],
    pub mode: Mode,
    /// Which variant (slot) this is at its path.
    pub slot: u32,
    /// The lane bitmap, or `None` when omitted (all-ones → every live lane).
    pub bitmap: Option<&'b [u8]>,
    /// The file content.
    pub content: &'b [u8],
}

/// Walk a layer's canonical bytes in a single forward pass, calling `visit`
/// on each file variant in container order. No `View`, no per-node
/// allocation beyond the reused path buffer and O(depth) recursion; content
/// and bitmap are slices into `bytes`.
pub fn visit_entries<'b>(
    bytes: &'b [u8],
    mut visit: impl FnMut(Entry<'_, 'b>),
) -> Result<(), depot::walk::DecodeError> {
    let mut cur = depot::walk::Cursor::new(bytes);
    let mut path = Vec::new();
    visit_level(&mut cur, &mut path, &mut visit)
}

fn visit_level<'b>(
    cur: &mut depot::walk::Cursor<'b>,
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(Entry<'_, 'b>),
) -> Result<(), depot::walk::DecodeError> {
    let node = cur.node()?; // a directory (or the root): no blob, has children
    for _ in 0..node.child_count {
        let name = cur.name()?;
        match classify(name) {
            Some(Kind::Dir(gitname)) => {
                let base = push_seg(path, gitname);
                visit_level(cur, path, visit)?;
                path.truncate(base);
            }
            Some(Kind::File(gitname, slot)) => {
                let vnode = cur.node()?; // the variant node: blob = content
                let content = vnode.blob.unwrap_or(&[]);
                // Meta children: `lanes` bitmap and at most one mode tag.
                let mut mode = Mode::File;
                let mut bitmap = None;
                for _ in 0..vnode.child_count {
                    let mname = cur.name()?;
                    let mnode = cur.node()?;
                    match mname {
                        LANES => bitmap = Some(mnode.blob.unwrap_or(&[])),
                        TAG_EXEC => mode = Mode::Exec,
                        TAG_SYMLINK => mode = Mode::Symlink,
                        TAG_GITLINK => mode = Mode::Gitlink,
                        _ => {}
                    }
                    // Meta nodes are leaves; drain any children defensively.
                    for _ in 0..mnode.child_count {
                        cur.name()?;
                        cur.skip()?;
                    }
                }
                let base = push_seg(path, gitname);
                visit(Entry { path: &path[..], mode, slot, bitmap, content });
                path.truncate(base);
            }
            None => {}
        }
    }
    Ok(())
}

/// Push `/seg` (or `seg` at the root) onto the path; returns the prior length
/// to truncate back to.
fn push_seg(path: &mut Vec<u8>, seg: &[u8]) -> usize {
    let base = path.len();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(seg);
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git's `base_name_compare` (read-cache.c): bytewise, with the byte past
    /// the shorter name synthesized as `/` for a dir and `0` for a file.
    fn base_name_compare(a: &[u8], a_dir: bool, b: &[u8], b_dir: bool) -> std::cmp::Ordering {
        let n = a.len().min(b.len());
        match a[..n].cmp(&b[..n]) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
        let c1 = a.get(n).copied().unwrap_or(if a_dir { DIR } else { 0 });
        let c2 = b.get(n).copied().unwrap_or(if b_dir { DIR } else { 0 });
        c1.cmp(&c2)
    }

    fn container_name(name: &[u8], is_dir: bool) -> Vec<u8> {
        if is_dir {
            dir_key(name)
        } else {
            file_key(name, 0)
        }
    }

    /// The iterator's `entry_cmp` over clean container names must equal git's
    /// `base_name_compare` — including the file-vs-dir same-name and prefix
    /// cases. (The codec's own bytewise storage order is DIFFERENT for bare
    /// dir names; the iterator reorders to git order, which is what this
    /// proves.)
    #[test]
    fn entry_cmp_matches_git() {
        let mut rng = 0x9e37_79b9u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let alpha: Vec<u8> = b"ab.-_x".to_vec(); // '.' '-' '_' bracket 0x2F, forced collisions
        let mut saw_reorder = false;
        for _ in 0..3000 {
            let mut entries: Vec<(Vec<u8>, bool)> = Vec::new();
            let m = 2 + (next() % 5) as usize;
            for _ in 0..m {
                let len = 1 + (next() % 4) as usize;
                let name: Vec<u8> = (0..len).map(|_| alpha[(next() as usize) % alpha.len()]).collect();
                entries.push((name, next() % 2 == 0));
            }
            let git = |es: &[(Vec<u8>, bool)]| {
                let mut v = es.to_vec();
                v.sort_by(|(a, ad), (b, bd)| base_name_compare(a, *ad, b, *bd).then(ad.cmp(bd)));
                v
            };
            let ours = |es: &[(Vec<u8>, bool)]| {
                let mut v = es.to_vec();
                v.sort_by(|(a, ad), (b, bd)| {
                    entry_cmp(&container_name(a, *ad), &container_name(b, *bd)).then(ad.cmp(bd))
                });
                v
            };
            assert_eq!(ours(&entries), git(&entries), "entry_cmp != git");
            // Confirm the codec's raw bytewise order really does differ, so
            // the reorder is doing work (not a no-op equivalence).
            let mut bytewise = entries.clone();
            bytewise.sort_by(|(a, ad), (b, bd)| {
                container_name(a, *ad).cmp(&container_name(b, *bd)).then(ad.cmp(bd))
            });
            if bytewise != git(&entries) {
                saw_reorder = true;
            }
        }
        assert!(saw_reorder, "test never exercised a case where bytewise != git");
    }

    // ---- round-trip: build a union, iterate it back in container order ----

    use depot::{codec, Layer, Node};

    #[test]
    fn encode_lane_round_trips() {
        let entries = vec![
            (b"a.txt".to_vec(), Mode::File, b"hello".to_vec()),
            (b"src/main.rs".to_vec(), Mode::File, b"fn main".to_vec()),
            (b"src/run.sh".to_vec(), Mode::Exec, b"#!".to_vec()),
            (b"dir/sub/deep".to_vec(), Mode::Symlink, b"target".to_vec()),
        ];
        let bytes = encode_lane(&entries);
        let mut got: Vec<(Vec<u8>, Mode, Vec<u8>)> = Vec::new();
        visit_entries(&bytes, |e| {
            assert_eq!(e.slot, 0);
            assert_eq!(e.bitmap, None, "single lane omits the bitmap");
            got.push((e.path.to_vec(), e.mode, e.content.to_vec()));
        })
        .unwrap();
        // Container order == git order here (no file-vs-dir prefix collisions):
        // a.txt, dir/sub/deep, src/main.rs, src/run.sh.
        let mut want = entries.clone();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, want);
    }

    #[test]
    fn iterator_round_trips_a_union() {
        // Root: a universal file, a dir with a normal + an exec file, and a
        // two-variant file. `Owned` container keys keep child order canonical.
        let mut root = Node::keep();
        root.children.insert(file_key(b"a.txt", 0), variant_node(b"hello", Mode::File, None));
        let mut src = Node::keep();
        src.children.insert(file_key(b"main.rs", 0), variant_node(b"fn main", Mode::File, None));
        src.children.insert(file_key(b"run.sh", 0), variant_node(b"#!", Mode::Exec, Some(&[0b11])));
        root.children.insert(dir_key(b"src"), src);
        root.children.insert(file_key(b"z", 0), variant_node(b"v0", Mode::File, Some(&[0b01])));
        root.children.insert(file_key(b"z", 1), variant_node(b"v1", Mode::Symlink, Some(&[0b10])));

        let bytes = codec::encode(&Layer { root });

        let mut got: Vec<(Vec<u8>, Mode, u32, Option<Vec<u8>>, Vec<u8>)> = Vec::new();
        visit_entries(&bytes, |e| {
            got.push((e.path.to_vec(), e.mode, e.slot, e.bitmap.map(|b| b.to_vec()), e.content.to_vec()));
        })
        .unwrap();

        // Container (bytewise) order: a.txt, src/main.rs, src/run.sh, z#0, z#1.
        assert_eq!(
            got,
            vec![
                (b"a.txt".to_vec(), Mode::File, 0, None, b"hello".to_vec()),
                (b"src/main.rs".to_vec(), Mode::File, 0, None, b"fn main".to_vec()),
                (b"src/run.sh".to_vec(), Mode::Exec, 0, Some(vec![0b11]), b"#!".to_vec()),
                (b"z".to_vec(), Mode::File, 0, Some(vec![0b01]), b"v0".to_vec()),
                (b"z".to_vec(), Mode::Symlink, 1, Some(vec![0b10]), b"v1".to_vec()),
            ]
        );
    }

    /// The write-side invariant: applying `delta_single_lane(old,new)` to the
    /// full-state of `old` reproduces the full-state of `new`, byte-identical.
    /// Both sides are the canonical positive full-state of `new`, so equality
    /// is exact. Covers content change, add, remove, and mode change.
    #[test]
    fn delta_single_lane_overlays_to_new() {
        let old = vec![
            (b"keep.txt".to_vec(), Mode::File, b"same".to_vec()),
            (b"change.rs".to_vec(), Mode::File, b"old".to_vec()),
            (b"src/run.sh".to_vec(), Mode::Exec, b"#!old".to_vec()),
            (b"gone/away".to_vec(), Mode::File, b"bye".to_vec()),
            (b"modeflip".to_vec(), Mode::File, b"body".to_vec()),
        ];
        let new = vec![
            (b"keep.txt".to_vec(), Mode::File, b"same".to_vec()),
            (b"change.rs".to_vec(), Mode::File, b"new".to_vec()),
            (b"src/run.sh".to_vec(), Mode::Exec, b"#!new".to_vec()),
            (b"added.md".to_vec(), Mode::File, b"hi".to_vec()),
            (b"modeflip".to_vec(), Mode::Symlink, b"body".to_vec()),
        ];
        let old_full = encode_lane(&old);
        let new_full = encode_lane(&new);
        let delta = delta_single_lane(&old, &new);
        let mut got = Vec::new();
        depot::stream::overlay_full(&old_full, &delta, &mut got).unwrap();
        assert_eq!(got, new_full, "overlay_full(old, delta) must equal new full-state");
    }

    /// A no-op delta (old == new) overlays to the same bytes and is minimal
    /// (an empty root: no children touched).
    #[test]
    fn delta_single_lane_noop_is_empty() {
        let same = vec![
            (b"a".to_vec(), Mode::File, b"x".to_vec()),
            (b"d/b".to_vec(), Mode::Exec, b"y".to_vec()),
        ];
        let full = encode_lane(&same);
        let delta = delta_single_lane(&same, &same);
        let mut got = Vec::new();
        depot::stream::overlay_full(&full, &delta, &mut got).unwrap();
        assert_eq!(got, full);
        // The delta is a bare Keep root with no children.
        let empty = codec::encode(&Layer { root: Node::keep() });
        assert_eq!(delta, empty, "unchanged inputs produce an empty delta");
    }

    // ---- multi-lane reslot: extract each lane back, SHA-relevant oracle ----

    fn lane(entries: &[(&[u8], Mode, &[u8], &[u8])]) -> LaneTree {
        entries
            .iter()
            .map(|(p, m, oid, c)| (p.to_vec(), LaneEntry { mode: *m, oid: oid.to_vec(), content: c.to_vec() }))
            .collect()
    }

    /// Reconstruct lane `j`'s tree from a union's bytes: at each path the sole
    /// variant whose bitmap includes `j` (omitted bitmap ⇒ every lane).
    fn view_lane(bytes: &[u8], j: u32) -> BTreeMap<Vec<u8>, (Mode, Vec<u8>)> {
        let mut m = BTreeMap::new();
        visit_entries(bytes, |e| {
            let inl = match e.bitmap {
                Some(b) => (b.get((j / 8) as usize).copied().unwrap_or(0) & (1 << (j % 8))) != 0,
                None => true,
            };
            if inl {
                m.insert(e.path.to_vec(), (e.mode, e.content.to_vec()));
            }
        })
        .unwrap();
        m
    }

    fn expect_lane(t: &LaneTree) -> BTreeMap<Vec<u8>, (Mode, Vec<u8>)> {
        t.iter().map(|(p, e)| (p.clone(), (e.mode, e.content.clone()))).collect()
    }

    /// `encode_union` then extract every lane must reproduce each input tree.
    #[test]
    fn encode_union_reconstructs_every_lane() {
        let lanes = vec![
            lane(&[(b"a", Mode::File, b"A", b"A"), (b"b", Mode::File, b"B", b"B"), (b"c", Mode::File, b"C", b"C")]),
            lane(&[(b"a", Mode::File, b"A", b"A"), (b"b", Mode::File, b"B2", b"B2"), (b"c", Mode::File, b"C", b"C")]),
            lane(&[(b"a", Mode::Exec, b"A", b"A"), (b"c", Mode::File, b"C", b"C")]),
        ];
        let bytes = encode_union(&lanes);
        for (j, t) in lanes.iter().enumerate() {
            assert_eq!(view_lane(&bytes, j as u32), expect_lane(t), "lane {j}");
        }
    }

    /// The multi-lane write invariant: `overlay_full(old_full, delta)` is the
    /// union over `new_lanes` — extracting each new lane reconstructs its tree.
    /// Exercises: bitmap grow (new lane joins a variant), variant appear (new
    /// oid → fresh slot), variant vanish (oid in no new lane → hole), path
    /// removed in one lane, and an all-ones prune.
    #[test]
    fn delta_multi_lane_overlays_to_new_union() {
        let old_lanes = vec![
            lane(&[(b"a", Mode::File, b"A", b"A"), (b"b", Mode::File, b"B", b"B"), (b"common", Mode::File, b"C", b"C")]),
            lane(&[(b"a", Mode::File, b"A", b"A"), (b"b", Mode::File, b"B2", b"B2"), (b"common", Mode::File, b"C", b"C")]),
        ];
        let new_lanes = vec![
            lane(&[(b"a", Mode::File, b"A", b"A"), (b"b", Mode::File, b"B", b"B"), (b"common", Mode::File, b"C", b"C")]),
            lane(&[(b"b", Mode::File, b"B2", b"B2"), (b"common", Mode::File, b"C", b"C"), (b"d", Mode::File, b"D", b"D")]),
            lane(&[(b"a", Mode::File, b"A", b"A"), (b"b", Mode::File, b"B", b"B"), (b"common", Mode::File, b"C2", b"C2")]),
        ];
        let old_full = encode_union(&old_lanes);
        let delta = delta_multi_lane(&old_full, &old_lanes, &new_lanes);
        let mut new_full = Vec::new();
        depot::stream::overlay_full(&old_full, &delta, &mut new_full).unwrap();

        for (j, t) in new_lanes.iter().enumerate() {
            assert_eq!(view_lane(&new_full, j as u32), expect_lane(t), "lane {j}");
        }
        // It must match a from-scratch union of the new lanes (semantically):
        let fresh = encode_union(&new_lanes);
        for j in 0..new_lanes.len() as u32 {
            assert_eq!(view_lane(&new_full, j), view_lane(&fresh, j), "lane {j} vs fresh");
        }
    }

    /// An unchanged variant (`b2` in lane 1, same oid, same lane set) is pruned
    /// — its slot appears nowhere in the delta.
    #[test]
    fn delta_multi_lane_prunes_unchanged() {
        let old_lanes = vec![
            lane(&[(b"b", Mode::File, b"B", b"B")]),
            lane(&[(b"b", Mode::File, b"B2", b"B2")]),
        ];
        // Only lane 0's `b` changes; lane 1's `b` (oid B2, lane {1}) is untouched.
        let new_lanes = vec![
            lane(&[(b"b", Mode::File, b"BX", b"BX")]),
            lane(&[(b"b", Mode::File, b"B2", b"B2")]),
        ];
        let old_full = encode_union(&old_lanes);
        let delta = delta_multi_lane(&old_full, &old_lanes, &new_lanes);
        // Reconstruct and check correctness first.
        let mut new_full = Vec::new();
        depot::stream::overlay_full(&old_full, &delta, &mut new_full).unwrap();
        for (j, t) in new_lanes.iter().enumerate() {
            assert_eq!(view_lane(&new_full, j as u32), expect_lane(t));
        }
        // The B2 variant kept slot 1 in old (B=slot0{0}, B2=slot1{1}); the delta
        // must not touch slot 1 — only slot 0's content changed.
        let mut slots = Vec::new();
        visit_entries(&delta, |e| slots.push(e.slot)).unwrap();
        assert!(!slots.contains(&1), "unchanged B2 (slot 1) must be pruned, saw {slots:?}");
    }

    // ---- streaming current-state reader vs materialized overlay oracle ----

    type CurRow = (Vec<u8>, Mode, u32, Option<Vec<u8>>);

    /// Materialize current state by sequential overlay, then read it back.
    fn oracle_current(base: &[u8], stack: &[Vec<u8>]) -> Vec<CurRow> {
        let mut mat = base.to_vec();
        for layer in stack {
            let mut next = Vec::new();
            depot::stream::overlay_full(&mat, layer, &mut next).unwrap();
            mat = next;
        }
        let mut rows = Vec::new();
        visit_entries(&mat, |e| rows.push((e.path.to_vec(), e.mode, e.slot, e.bitmap.map(|b| b.to_vec())))).unwrap();
        rows
    }

    fn stream_current(base: &[u8], stack: &[Vec<u8>]) -> Vec<CurRow> {
        let mut rows = Vec::new();
        visit_current(base, stack, |p, m, s, bm| rows.push((p.to_vec(), m, s, bm))).unwrap();
        rows
    }

    /// Nested paths, add / remove / change / mode-flip across a two-deep stack:
    /// the streaming reader must equal the materialized overlay exactly.
    #[test]
    fn visit_current_matches_overlay_single_lane() {
        let t0 = vec![
            (b"dir/a".to_vec(), Mode::File, b"A".to_vec()),
            (b"dir/b".to_vec(), Mode::Exec, b"B".to_vec()),
            (b"top".to_vec(), Mode::File, b"T".to_vec()),
            (b"gone".to_vec(), Mode::File, b"G".to_vec()),
        ];
        let t1 = vec![
            (b"dir/a".to_vec(), Mode::File, b"A2".to_vec()),
            (b"dir/b".to_vec(), Mode::Exec, b"B".to_vec()),
            (b"top".to_vec(), Mode::Symlink, b"T".to_vec()),
            (b"new".to_vec(), Mode::File, b"N".to_vec()),
        ];
        let t2 = vec![
            (b"dir/a".to_vec(), Mode::File, b"A2".to_vec()),
            (b"dir/c".to_vec(), Mode::File, b"C".to_vec()),
            (b"new".to_vec(), Mode::File, b"N2".to_vec()),
        ];
        let base = encode_lane(&t0);
        let d0 = delta_single_lane(&t0, &t1);
        let d1 = delta_single_lane(&t1, &t2);
        let stack = vec![d0, d1];

        assert_eq!(stream_current(&base, &stack), oracle_current(&base, &stack));
        // And with an empty stack the reader is just the base walk.
        assert_eq!(stream_current(&base, &[]), oracle_current(&base, &[]));
    }

    /// Multi-variant paths with a lanes-only bitmap update (a variant absorbs a
    /// lane, another vanishes): exercises `both_child`'s meta override and the
    /// all-ones bitmap dropping to `None`.
    #[test]
    fn visit_current_matches_overlay_multi_lane() {
        let la = lane(&[(b"same", Mode::File, b"S", b"S"), (b"split", Mode::File, b"P0", b"P0"), (b"d/n", Mode::File, b"N", b"N")]);
        let lb = lane(&[(b"same", Mode::File, b"S", b"S"), (b"split", Mode::File, b"P1", b"P1"), (b"d/n", Mode::File, b"N", b"N")]);
        let old_lanes = vec![la.clone(), lb.clone()];
        // Lane 1's `split` changes to match lane 0 → the two variants collapse
        // to one all-ones variant (lanes child dropped); `d/n` untouched.
        let lb2 = lane(&[(b"same", Mode::File, b"S", b"S"), (b"split", Mode::File, b"P0", b"P0"), (b"d/n", Mode::File, b"N", b"N")]);
        let new_lanes = vec![la.clone(), lb2];

        let base = encode_union(&old_lanes);
        let delta = delta_multi_lane(&base, &old_lanes, &new_lanes);
        let stack = vec![delta];

        assert_eq!(stream_current(&base, &stack), oracle_current(&base, &stack));
    }

    /// Randomized: random single-lane states chained into a stack; the
    /// streaming reader must match the materialized overlay for every seed.
    #[test]
    fn visit_current_matches_overlay_randomized() {
        fn next(rng: &mut u64) -> u64 {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            *rng
        }
        let paths: [&[u8]; 6] = [b"a", b"d/x", b"d/y", b"d/e/f", b"m", b"z"];
        let modes = [Mode::File, Mode::Exec, Mode::Symlink, Mode::Gitlink];
        let gen_state = |rng: &mut u64| {
            let mut s: Vec<(Vec<u8>, Mode, Vec<u8>)> = Vec::new();
            for p in paths {
                if next(rng) % 3 != 0 {
                    let mode = modes[(next(rng) % 4) as usize];
                    let content = vec![(next(rng) % 7) as u8];
                    s.push((p.to_vec(), mode, content));
                }
            }
            s
        };
        let mut rng = 0xda3e_39cbu64;
        for _ in 0..400 {
            let depth = 1 + (next(&mut rng) % 4) as usize;
            let mut states = vec![gen_state(&mut rng)];
            for _ in 0..depth {
                states.push(gen_state(&mut rng));
            }
            let base = encode_lane(&states[0]);
            let stack: Vec<Vec<u8>> =
                (1..states.len()).map(|i| delta_single_lane(&states[i - 1], &states[i])).collect();
            assert_eq!(
                stream_current(&base, &stack),
                oracle_current(&base, &stack),
                "mismatch depth {depth}, states={states:?}"
            );
        }
    }

    /// Reconstructing each lane's tree oid from the union must equal building
    /// that tree oid straight from the input lane — the union encode + lane
    /// extract preserve exact git tree identity (SHA-exact). Independent of the
    /// synthetic `oid` field: both sides hash the real content.
    #[test]
    fn union_reconstructs_exact_tree_oids() {
        let gl = b"0123456789abcdef0123456789abcdef01234567"; // a valid 40-hex gitlink id
        let lanes = vec![
            lane(&[
                (b"README", Mode::File, b"r0", b"hello\n"),
                (b"src/main.rs", Mode::File, b"m0", b"fn main() {}\n"),
                (b"src/run.sh", Mode::Exec, b"x0", b"#!/bin/sh\n"),
                (b"link", Mode::Symlink, b"l0", b"src/main.rs"),
                (b"dep", Mode::Gitlink, b"g0", gl),
            ]),
            lane(&[
                (b"README", Mode::File, b"r1", b"hello world\n"),
                (b"src/main.rs", Mode::File, b"m0", b"fn main() {}\n"),
                (b"src/lib.rs", Mode::File, b"lib", b"pub fn f() {}\n"),
            ]),
        ];
        let union = encode_union(&lanes);

        // Reference: tree oid built directly from each input lane's tree.
        for (j, t) in lanes.iter().enumerate() {
            let mut root = BTreeMap::new();
            for (path, e) in t {
                tnode_insert(&mut root, path, e.mode, e.content.clone());
            }
            let want = tree_oid(&root).unwrap();
            let got = reconstruct_lane_tree_oid(&union, j as u32).unwrap();
            assert_eq!(got, want, "lane {j} tree oid");
        }
        // The two lanes genuinely differ, so the oids must too.
        assert_ne!(
            reconstruct_lane_tree_oid(&union, 0).unwrap(),
            reconstruct_lane_tree_oid(&union, 1).unwrap(),
        );
    }

    /// Ground-truth anchors: the reconstruction's hashing must match git's
    /// real object ids, not just itself. These constants are git's canonical
    /// empty-tree oid and the blob oid of "hello\n".
    #[test]
    fn tree_oid_matches_real_git_constants() {
        assert_eq!(tree_oid(&BTreeMap::new()).unwrap(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(crate::git_obj_oid("blob", b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
        // A one-file tree `hello` (100644) → `git write-tree` value.
        let mut root = BTreeMap::new();
        root.insert(b"hello".to_vec(), TNode::File(Mode::File, b"hello\n".to_vec()));
        assert_eq!(tree_oid(&root).unwrap(), "b4d01e9b0c4a9356736dfddf8830ba9a54f5271c");
    }

    #[test]
    fn keys_and_classify_roundtrip() {
        for slot in [0u32, 1, 127, 128, 300, 70000] {
            let k = file_key(b"foo.rs", slot);
            assert_eq!(classify(&k), Some(Kind::File(b"foo.rs", slot)));
        }
        let d = dir_key(b"src");
        assert_eq!(classify(&d), Some(Kind::Dir(b"src")));
        // A file whose git name would be a prefix of another still splits at
        // the first NUL only.
        let k = file_key(b"a", 5);
        assert_eq!(classify(&k), Some(Kind::File(b"a", 5)));
    }

    /// `container_cmp` must equal bytewise comparison of the actual container
    /// keys (dir = bare name, file = name + 0x00 + varint) — the order the
    /// codec stores and the layer iterator emits.
    #[test]
    fn container_cmp_is_bytewise_on_keys() {
        let mut rng = 0x51ed_270bu64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let alpha: Vec<u8> = b"ab.-_x".to_vec();
        for _ in 0..5000 {
            let alen = 1 + (next() % 4) as usize;
            let an: Vec<u8> = (0..alen).map(|_| alpha[(next() as usize) % alpha.len()]).collect();
            let ad = next() % 2 == 0;
            let blen = 1 + (next() % 4) as usize;
            let bn: Vec<u8> = (0..blen).map(|_| alpha[(next() as usize) % alpha.len()]).collect();
            let bd = next() % 2 == 0;
            let ka = if ad { dir_key(&an) } else { file_key(&an, next() as u32 % 500) };
            let kb = if bd { dir_key(&bn) } else { file_key(&bn, next() as u32 % 500) };
            // container_cmp (slot-independent) must agree with bytewise on the
            // full keys wherever the entries differ by name/kind.
            let want = ka.cmp(&kb);
            let got = container_cmp(&an, ad, &bn, bd);
            if an != bn || ad != bd {
                assert_eq!(got, want, "container_cmp != bytewise keys: {an:?}/{ad} vs {bn:?}/{bd}");
            }
        }
    }

    #[test]
    fn file_vs_dir_same_name_both_orders() {
        use std::cmp::Ordering::*;
        // OUR order (container/bytewise): bare dir `foo` before file `foo`.
        assert_eq!(container_cmp(b"foo", true, b"foo", false), Less);
        assert!(dir_key(b"foo") < file_key(b"foo", 0), "codec stores dir first");
        // GIT order (reconstruction): file `foo` before dir `foo`.
        assert_eq!(entry_cmp(&file_key(b"foo", 0), &dir_key(b"foo")), Less);
        assert_eq!(entry_cmp(&file_key(b"foo", 999), &dir_key(b"foo")), Less);
        assert_eq!(entry_cmp(&dir_key(b"foo"), &file_key(b"foo.txt", 0)), Greater);
    }
}
