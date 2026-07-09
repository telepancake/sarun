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
use std::collections::BTreeMap;
use depot::walk::{Cursor, DecodeError as WErr};

pub const LANES: &[u8] = b"lanes";
pub const TAG_EXEC: &[u8] = b"x";
pub const TAG_SYMLINK: &[u8] = b"l";
pub const TAG_GITLINK: &[u8] = b"m";
/// A non-canonical mode (e.g. `100664`) — tag `o`, whose blob carries the raw
/// octal bytes (unlike `x`/`l`/`m`, whose blobs are empty). SHA-exactness on
/// historical git trees demands the exact mode be reproduced, so a mode git's
/// porcelain would normalize away is preserved verbatim through this tag.
pub const TAG_OTHER: &[u8] = b"o";

/// A git file mode. The three canonical non-plain modes are encoded as empty
/// `x`/`l`/`m` mode-tag children; a plain file carries no tag; anything else
/// (a non-canonical historical mode such as `100664`) is `Other(raw)` and
/// encodes as an `o` tag whose blob is the octal bytes. `Other` holds the
/// numeric mode so `Mode` stays `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// `100644` — a normal file (no tag).
    File,
    /// `100755` — executable (tag `x`).
    Exec,
    /// `120000` — symlink (tag `l`).
    Symlink,
    /// `160000` — gitlink / submodule (tag `m`).
    Gitlink,
    /// Any other (non-canonical) blob mode, held as its raw numeric value
    /// (e.g. `0o100664`) — tag `o`, blob = octal bytes.
    Other(u32),
}

/// Parse octal mode bytes (e.g. `b"100664"`) to their numeric value.
fn mode_num(b: &[u8]) -> u32 {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| u32::from_str_radix(s, 8).ok())
        .unwrap_or(0)
}

impl Mode {
    /// The git octal mode bytes.
    pub fn octal(self) -> Vec<u8> {
        match self {
            Mode::File => b"100644".to_vec(),
            Mode::Exec => b"100755".to_vec(),
            Mode::Symlink => b"120000".to_vec(),
            Mode::Gitlink => b"160000".to_vec(),
            Mode::Other(m) => format!("{m:o}").into_bytes(),
        }
    }
    /// The mode of a git tree entry, or `None` if it is a directory — directories
    /// carry no variant/tag. A non-canonical blob mode becomes `Other`.
    pub fn from_octal(mode: &[u8]) -> Option<Mode> {
        match mode {
            b"100644" => Some(Mode::File),
            b"100755" => Some(Mode::Exec),
            b"120000" => Some(Mode::Symlink),
            b"160000" => Some(Mode::Gitlink),
            _ => {
                let n = mode_num(mode);
                // S_IFDIR (0o040000) tree entries carry no variant.
                if n == 0 || (n & 0o170000) == 0o040000 {
                    None
                } else {
                    Some(Mode::Other(n))
                }
            }
        }
    }
    /// The meta-child tag NAME for this mode, if any (a plain file has none).
    /// `Other` shares tag `o`; its distinguishing octal lives in the tag blob.
    pub fn tag(self) -> Option<&'static [u8]> {
        match self {
            Mode::File => None,
            Mode::Exec => Some(TAG_EXEC),
            Mode::Symlink => Some(TAG_SYMLINK),
            Mode::Gitlink => Some(TAG_GITLINK),
            Mode::Other(_) => Some(TAG_OTHER),
        }
    }
}

/// Decode a mode-tag child (`x`/`l`/`m`/`o`) into its `Mode`, reading the `o`
/// tag's blob for the exact octal. Returns `None` if `name` is not a mode tag.
fn tag_mode(name: &[u8], blob: Option<&[u8]>) -> Option<Mode> {
    match name {
        TAG_EXEC => Some(Mode::Exec),
        TAG_SYMLINK => Some(Mode::Symlink),
        TAG_GITLINK => Some(Mode::Gitlink),
        TAG_OTHER => Some(Mode::Other(mode_num(blob.unwrap_or(&[])))),
        _ => None,
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
    let tagged = |blob: Vec<u8>| depot::Node { blob: depot::BlobOp::Set(blob.into()), ..depot::Node::keep() };
    let mut n = depot::Node { blob: depot::BlobOp::Set(content.into()), ..depot::Node::keep() };
    if let Some(tag) = mode.tag() {
        // Canonical tags (`x`/`l`/`m`) carry an empty non-identity blob; the
        // `o` tag carries the exact octal so the mode reconstructs verbatim.
        let blob = if let Mode::Other(m) = mode { format!("{m:o}").into_bytes() } else { Vec::new() };
        n.children.insert(tag.to_vec(), tagged(blob));
    }
    if let Some(bm) = bitmap {
        n.children.insert(LANES.to_vec(), depot::Node { blob: depot::BlobOp::Set(bm.into()), ..depot::Node::keep() });
    }
    n
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
                (mode.octal(), oid, false)
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

// --------------------------------------------- stacked current-state reader

/// A hole in the byte encoding: backdrop anchor, no blob, no children.
fn is_removal(n: &depot::walk::Node) -> bool {
    n.backdrop && n.blob.is_none() && n.child_count == 0
}

/// The effective current variants — the base full-state overlaid with the live
/// delta stack (§5: `refPrefix` + stack) — yielded per file in container order
/// WITHOUT materializing the union. Directories are walked in lockstep; only a
/// TOUCHED leaf variant is resolved (into a few owned facets), an untouched
/// base subtree streams straight through. `visit` gets `(path, mode, slot,
/// bitmap, content)` — over the encoder's content-free state the content is
/// the variant's git oid hex, so the §6 `(mode, oid)` identity is read directly
/// from the stream, never by hashing.
///
/// The stack is collapsed first with `compose_stream` (delta ∘ delta — holes
/// survive); the geometric stack keeps it to ~log(n) small layers, so the
/// collapse and the second lockstep stream stay bounded — the big `base` is
/// never re-sorted or copied.
pub fn visit_stacked(
    base: &[u8],
    stack: &[Vec<u8>],
    mut visit: impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>, Vec<u8>),
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
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>, Vec<u8>),
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

/// Read a variant node's content + mode + bitmap facets from `cur` (positioned
/// at the node), consuming it whole.
fn read_facets(cur: &mut Cursor) -> Result<(Mode, Option<Vec<u8>>, Vec<u8>), WErr> {
    let n = cur.node()?;
    let content = n.blob.unwrap_or(&[]).to_vec();
    let mut mode = Mode::File;
    let mut bitmap = None;
    for _ in 0..n.child_count {
        let m = cur.name()?.to_vec();
        let mn = cur.node()?;
        if m.as_slice() == LANES {
            bitmap = Some(mn.blob.unwrap_or(&[]).to_vec());
        } else if let Some(tm) = tag_mode(&m, mn.blob) {
            mode = tm;
        }
        for _ in 0..mn.child_count {
            cur.name()?;
            cur.skip()?;
        }
    }
    Ok((mode, bitmap, content))
}

/// A child present only in the base (delta does not touch it): walk it as-is.
fn base_child(
    cur: &mut Cursor,
    name: &[u8],
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>, Vec<u8>),
) -> Result<(), WErr> {
    match classify(name) {
        Some(Kind::File(gitname, slot)) => {
            let base = push_seg(path, gitname);
            let (mode, bitmap, content) = read_facets(cur)?;
            visit(&path[..], mode, slot, bitmap, content);
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
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>, Vec<u8>),
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
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>, Vec<u8>),
) -> Result<(), WErr> {
    let mut mode = Mode::File;
    let mut bitmap = None;
    for _ in 0..node.child_count {
        let m = cur.name()?.to_vec();
        let mn = cur.node()?;
        if !is_removal(&mn) {
            if m.as_slice() == LANES {
                bitmap = Some(mn.blob.unwrap_or(&[]).to_vec());
            } else if let Some(tm) = tag_mode(&m, mn.blob) {
                mode = tm;
            }
        }
        for _ in 0..mn.child_count {
            cur.name()?;
            cur.skip()?;
        }
    }
    if let Some(content) = node.blob {
        let base = push_seg(path, gitname);
        visit(&path[..], mode, slot, bitmap, content.to_vec());
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
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>, Vec<u8>),
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
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<Vec<u8>>, Vec<u8>),
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
            let (mut mode, mut bitmap, mut content) = read_facets(b)?;
            if let Some(blob) = dn.blob {
                content = blob.to_vec();
            }
            for _ in 0..dn.child_count {
                let m = d.name()?.to_vec();
                let mn = d.node()?;
                if m.as_slice() == LANES {
                    bitmap = if is_removal(&mn) { None } else { Some(mn.blob.unwrap_or(&[]).to_vec()) };
                } else if tag_mode(&m, None).is_some() {
                    // A mode tag child. A removal reverts to plain ONLY if it
                    // was the active tag (compare by tag NAME, so an `o` tag's
                    // blob is irrelevant to the revert); otherwise it sets the
                    // decoded mode (reading the `o` blob for the exact octal).
                    if is_removal(&mn) {
                        if mode.tag() == Some(m.as_slice()) {
                            mode = Mode::File;
                        }
                    } else if let Some(tm) = tag_mode(&m, mn.blob) {
                        mode = tm;
                    }
                }
                for _ in 0..mn.child_count {
                    d.name()?;
                    d.skip()?;
                }
            }
            let base = push_seg(path, gitname);
            visit(&path[..], mode, slot, bitmap, content);
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
                    if mname == LANES {
                        bitmap = Some(mnode.blob.unwrap_or(&[]));
                    } else if let Some(tm) = tag_mode(mname, mnode.blob) {
                        mode = tm;
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
