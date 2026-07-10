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

/// The effective current variants — the base full-state lockstep-merged with
/// EVERY live delta layer (§5: "refPrefix plus the live stack, read by
/// lockstep iteration") — yielded per file in container order. Nothing is
/// materialized: no composed intermediate buffer (the k-way merge replaces
/// the old compose-then-2-way read, which re-composed the stack into a fresh
/// allocation on every call), no owned copies — every facet handed to
/// `visit` is a borrowed slice into one of the input buffers.
///
/// Sources are ordered bottom→top: the base, then the stack layers oldest
/// first. Records are TOTAL per KEY (§4): at each key of the container the
/// newest recorded value wins whole (a hole removes the key; a backdrop
/// occludes everything below it); a key a layer does not mention falls
/// through. The merge resolves keys generically — [`assemble`], one level
/// above it, is the only place key names are interpreted as variant facets.
/// Directory nodes merge children across sources. Untouched base subtrees
/// stream straight through. `k` is the geostack depth (~log(revisions since
/// the last seal)), so the merge stays narrow.
pub fn visit_stacked(
    base: &[u8],
    stack: &[Vec<u8>],
    mut visit: impl FnMut(&[u8], Mode, u32, Option<&[u8]>, &[u8]),
) -> Result<(), WErr> {
    let mut srcs: Vec<Cursor> = Vec::with_capacity(1 + stack.len());
    srcs.push(Cursor::new(base));
    for l in stack {
        srcs.push(Cursor::new(l));
    }
    let mut active = Vec::with_capacity(srcs.len());
    for (i, c) in srcs.iter_mut().enumerate() {
        let n = c.node()?;
        active.push((i, n.child_count));
    }
    let mut path = Vec::new();
    klevel(&mut srcs, &active, &mut path, &mut visit)
}

/// Consume and discard the remaining `count` children of a level of `cur`
/// (name + whole subtree each) — the erased-below-a-backdrop case.
fn skip_children(cur: &mut Cursor, count: u64) -> Result<(), WErr> {
    for _ in 0..count {
        cur.name()?;
        cur.skip()?;
    }
    Ok(())
}

/// Stream one BASE-ONLY child (no delta touches it): a file resolves and
/// emits its facets directly (a one-participant fold); a directory streams
/// its whole subtree. The fast path — no merging, no scans, no allocation.
fn base_only_child<'a>(
    cur: &mut Cursor<'a>,
    name: &[u8],
    path: &mut Vec<u8>,
    keys: &mut NodeKeys<'a>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<&[u8]>, &[u8]),
) -> Result<(), WErr> {
    match classify(name) {
        Some(Kind::File(gitname, slot)) => {
            let keep = push_seg(path, gitname);
            keys.clear();
            fold_node_keys(cur, keys)?;
            if let Some((mode, bitmap, content)) = assemble(keys) {
                visit(&path[..], mode, slot, bitmap, content);
            }
            path.truncate(keep);
        }
        Some(Kind::Dir(gitname)) => {
            let n = cur.node()?;
            let keep = push_seg(path, gitname);
            for _ in 0..n.child_count {
                let child = cur.name()?;
                base_only_child(cur, child, path, keys, visit)?;
            }
            path.truncate(keep);
        }
        None => cur.skip()?,
    }
    Ok(())
}

/// One directory level of the k-way lockstep. `active` lists the sources
/// present at this level bottom→top as `(source index, child count)`; the
/// cursors sit just past their level's node header. Names advance in
/// bytewise (container) order across all sources at once.
///
/// Comparison discipline: the untouched majority (base head strictly below
/// the cached delta minimum) costs ONE comparison and streams straight
/// through; a delta-touched name costs one comparison per delta source —
/// the same comparisons any merge must make — with the participant set
/// recorded DURING the min scan and reused by the read/skip and advance
/// phases, which perform no name comparisons at all.
fn klevel<'a>(
    srcs: &mut [Cursor<'a>],
    active: &[(usize, u64)],
    path: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], Mode, u32, Option<&[u8]>, &[u8]),
) -> Result<(), WErr> {
    let mut rem: Vec<u64> = active.iter().map(|&(_, n)| n).collect();
    let mut heads: Vec<Option<&'a [u8]>> = Vec::with_capacity(active.len());
    for (k, &(si, _)) in active.iter().enumerate() {
        heads.push(if rem[k] > 0 {
            rem[k] -= 1;
            Some(srcs[si].name()?)
        } else {
            None
        });
    }
    // Position of the base source at this level, if it participates.
    let kb = active.iter().position(|&(si, _)| si == 0);
    // Min over the DELTA heads only — refreshed only when a delta advances.
    let dmin = |heads: &[Option<&'a [u8]>]| -> Option<&'a [u8]> {
        heads
            .iter()
            .enumerate()
            .filter(|&(k, _)| Some(k) != kb)
            .filter_map(|(_, h)| *h)
            .min()
    };
    let mut delta_min = dmin(&heads);
    // The participant positions at the current minimum, and the per-key
    // resolution scratch — both reused across iterations (cleared, never
    // reallocated).
    let mut group: Vec<usize> = Vec::with_capacity(active.len());
    let mut keys = NodeKeys::default();
    loop {
        // Fast path: the base owns the minimum outright — one comparison.
        if let (Some(kb), Some(bh)) = (kb, kb.and_then(|k| heads[k])) {
            if delta_min.map(|d| bh < d).unwrap_or(true) {
                base_only_child(&mut srcs[active[kb].0], bh, path, &mut keys, visit)?;
                heads[kb] = if rem[kb] > 0 {
                    rem[kb] -= 1;
                    Some(srcs[active[kb].0].name()?)
                } else {
                    None
                };
                continue;
            }
        }
        // One scan: min AND its participant set together.
        group.clear();
        let mut min: Option<&'a [u8]> = None;
        for (k, h) in heads.iter().enumerate() {
            let Some(h) = *h else { continue };
            match min {
                None => {
                    min = Some(h);
                    group.push(k);
                }
                Some(m) => match h.cmp(m) {
                    std::cmp::Ordering::Less => {
                        min = Some(h);
                        group.clear();
                        group.push(k);
                    }
                    std::cmp::Ordering::Equal => group.push(k),
                    std::cmp::Ordering::Greater => {}
                },
            }
        }
        let Some(min) = min else { break };
        match classify(min) {
            Some(Kind::Dir(gitname)) => {
                // Participants bottom→top; a BACKDROP dir erases every
                // participant below it (their subtrees consumed and dropped)
                // and re-resolves over nothing.
                let mut sub: Vec<(usize, u64)> = Vec::with_capacity(group.len());
                for &k in &group {
                    let si = active[k].0;
                    let n = srcs[si].node()?;
                    if n.backdrop {
                        for (sj, cnt) in sub.drain(..) {
                            skip_children(&mut srcs[sj], cnt)?;
                        }
                    }
                    sub.push((si, n.child_count));
                }
                let keep = push_seg(path, gitname);
                klevel(srcs, &sub, path, visit)?;
                path.truncate(keep);
            }
            Some(Kind::File(gitname, slot)) => {
                // Resolve the participants per KEY, bottom→top (§4: each
                // key's newest record wins whole), then interpret the
                // resolved keys one level above the merge.
                keys.clear();
                for &k in &group {
                    fold_node_keys(&mut srcs[active[k].0], &mut keys)?;
                }
                if let Some((mode, bitmap, content)) = assemble(&keys) {
                    let keep = push_seg(path, gitname);
                    visit(&path[..], mode, slot, bitmap, content);
                    path.truncate(keep);
                }
            }
            None => {
                // Unclassifiable name (never produced by the encoder).
                for &k in &group {
                    srcs[active[k].0].skip()?;
                }
            }
        }
        // Advance exactly the participants; the delta-min cache refreshes
        // only here (a delta advanced).
        for &k in &group {
            heads[k] = if rem[k] > 0 {
                rem[k] -= 1;
                Some(srcs[active[k].0].name()?)
            } else {
                None
            };
        }
        delta_min = dmin(&heads);
    }
    Ok(())
}

/// The resolved per-key values of one file node across the participants at
/// its name: the node's own blob, plus each child KEY's value. A `None`
/// value records the key as removed; a key never recorded is absent. Purely
/// structural — no key name is interpreted here (that is [`assemble`]'s
/// job, one level above). All slices borrow the source buffers.
#[derive(Default)]
struct NodeKeys<'a> {
    blob: Option<&'a [u8]>,
    kids: Vec<(&'a [u8], Option<&'a [u8]>)>,
}

impl NodeKeys<'_> {
    fn clear(&mut self) {
        self.blob = None;
        self.kids.clear();
    }
}

/// Fold one participant's node into the per-key resolution. Callers fold
/// bottom→top, so a later write at a key IS the newest record winning —
/// §4 totality is per KEY: a recorded value replaces the accumulated one
/// outright, a hole removes the key, and a backdrop node occludes every
/// key accumulated below it. Generic: nothing here knows what a key means.
fn fold_node_keys<'a>(cur: &mut Cursor<'a>, acc: &mut NodeKeys<'a>) -> Result<(), WErr> {
    let n = cur.node()?;
    if n.backdrop {
        acc.clear();
    }
    if let Some(b) = n.blob {
        acc.blob = Some(b);
    }
    for _ in 0..n.child_count {
        let name = cur.name()?;
        let cn = cur.node()?;
        // A hole (backdrop, no value, no children) removes the key; any
        // recorded value replaces it whole.
        let val = if cn.backdrop && cn.blob.is_none() && cn.child_count == 0 {
            None
        } else {
            Some(cn.blob.unwrap_or(&[]))
        };
        match acc.kids.iter_mut().find(|(k, _)| *k == name) {
            Some(kid) => kid.1 = val,
            None => acc.kids.push((name, val)),
        }
        skip_children(cur, cn.child_count)?;
    }
    Ok(())
}

/// Interpret the resolved keys of a variant node (§2) — the ONLY place the
/// stacked read gives key names a meaning: the node's own blob is the
/// content (no content ⇒ the variant does not exist here), the `lanes` key
/// is the bitmap (absent/removed ⇒ omitted, all-ones), and a mode-tag key
/// holding a value sets the mode.
fn assemble<'a>(keys: &NodeKeys<'a>) -> Option<(Mode, Option<&'a [u8]>, &'a [u8])> {
    let content = keys.blob?;
    let mut mode = Mode::File;
    let mut bitmap = None;
    for &(name, val) in &keys.kids {
        if name == LANES {
            bitmap = val;
        } else if val.is_some() {
            if let Some(tm) = tag_mode(name, val) {
                mode = tm;
            }
        }
    }
    Some((mode, bitmap, content))
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
