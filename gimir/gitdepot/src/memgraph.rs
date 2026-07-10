//! The resident object graph: every object of a repository (or of ONE
//! received wire pack) held in RAM as a fixed-size header in one hashtable
//! plus a compact variable-size body, with pointers (u32 slot indices)
//! instead of oids everywhere.
//!
//! The shape, per the commission:
//!
//! - ids are keyed TRUNCATED to 64 bits (the sha's first 8 bytes). The
//!   hashtable is open-addressed over the headers themselves — the key is
//!   embedded in the header and appears nowhere else, and since it IS
//!   uniformly random sha bits it is its own hash. The FULL sha lives once
//!   in a side array (frame emission and export are SHA-exact, so identity
//!   must be whole), which also makes a truncation collision DETECTED — a
//!   loud error instead of silently fused objects.
//! - forward references get a DUMMY header (kind = unknown, no body) so a
//!   tree parsed before its children can already point at them; the dummy
//!   is filled in when the real object is scanned. The table is sized once
//!   from the object count, so slot indices are stable pointers.
//! - a Tree's body points at a shared LISTING — the ordered `(mode, name)`
//!   sequence, deduplicated by value in a second hashtable — plus a flat
//!   vector of child pointers in listing order.
//! - a Blob's body is only WHERE TO READ its content (pack offset). A
//!   Commit is that plus tree and parent pointers; a Tag that plus the
//!   target pointer.
//!
//! Fed directly from a self-contained (`no-thin`) wire pack, this replaces
//! `git index-pack` and the whole git-repo roundtrip: the scan inflates,
//! resolves in-pack deltas, HASHES each object itself (the pack carries no
//! ids), and the graph then serves the union encoder as its object source —
//! trees from listings, topology from parent pointers, blob bytes from the
//! transient pack file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::gitobj::{apply_delta, inflate_at, inflate_at_counted, ObjectStore};
use crate::oidenc::{Ent, Objects};
use crate::{Error, Result};

const K_EMPTY: u8 = 0;
const K_DUMMY: u8 = 1;
const K_COMMIT: u8 = 2;
const K_TREE: u8 = 3;
const K_BLOB: u8 = 4;
const K_TAG: u8 = 5;

/// The fixed-size header: the embedded truncated id (the hash key), the
/// kind, and the body index into the kind's arena.
#[derive(Clone, Copy)]
struct Head {
    id: u64,
    kind: u8,
    body: u32,
}

/// Where an object's bytes live: an offset in the scanned pack. (A graph
/// built by scanning a git REPO instead records `(pack, ofs)`; only
/// pack-built graphs serve as an `Objects` source.)
#[derive(Clone, Copy)]
struct Loc {
    pack: u32,
    ofs: u64,
}

struct TreeBody {
    listing: u32,
    /// Offset into the flat child-pointer arena; length = the listing's
    /// entry count.
    kids_off: u32,
}

struct CommitBody {
    #[allow(dead_code)]
    loc: Loc,
    tree: u32,
    par_off: u32,
    par_len: u16,
}

struct TagBody {
    #[allow(dead_code)]
    loc: Loc,
    target: u32,
}

/// The header table: open addressing, linear probing, sized ONCE (slot
/// indices are stable pointers). `shas[slot]` is the full 20-byte id.
struct HeadTable {
    slots: Vec<Head>,
    /// Full oids, 32-byte slots (SHA-1 uses the first 20).
    shas: Vec<[u8; 32]>,
    /// Oid width in bytes.
    olen: usize,
    mask: usize,
    n: usize,
}

impl HeadTable {
    fn with_capacity(objects: usize) -> HeadTable {
        let cap = (objects * 4 / 3 + 64).next_power_of_two();
        HeadTable {
            slots: vec![Head { id: 0, kind: K_EMPTY, body: u32::MAX }; cap],
            shas: vec![[0u8; 32]; cap],
            olen: 20,
            mask: cap - 1,
            n: 0,
        }
    }

    /// The slot of `sha`, inserting a dummy header if absent. The truncated
    /// id is its own hash; the full sha disambiguates (and detects) 64-bit
    /// prefix collisions.
    fn find_or_insert(&mut self, sha: &[u8]) -> Result<u32> {
        debug_assert_eq!(sha.len(), self.olen);
        let id = u64::from_be_bytes(sha[..8].try_into().unwrap());
        let mut i = (id as usize) & self.mask;
        loop {
            let s = self.slots[i];
            if s.kind == K_EMPTY {
                if self.n * 4 >= self.slots.len() * 3 {
                    return Err(Error::Chain(
                        "memgraph: header table over capacity (unexpected object count)".into(),
                    ));
                }
                self.slots[i] = Head { id, kind: K_DUMMY, body: u32::MAX };
                self.shas[i][..sha.len()].copy_from_slice(sha);
                self.n += 1;
                return Ok(i as u32);
            }
            if s.id == id {
                if &self.shas[i][..sha.len()] != sha {
                    return Err(Error::Chain(format!(
                        "memgraph: 64-bit id collision: {} vs {}",
                        hex::encode(&self.shas[i][..sha.len()]),
                        hex::encode(sha)
                    )));
                }
                return Ok(i as u32);
            }
            i = (i + 1) & self.mask;
        }
    }

    fn lookup(&self, sha: &[u8]) -> Option<u32> {
        let id = u64::from_be_bytes(sha[..8].try_into().unwrap());
        let mut i = (id as usize) & self.mask;
        loop {
            let s = self.slots[i];
            if s.kind == K_EMPTY {
                return None;
            }
            if s.id == id && &self.shas[i][..sha.len()] == sha {
                return Some(i as u32);
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// Deduplicated listings: the byte form is the tree object's entries minus
/// the child oids — `<mode> <name>\0` per entry, order preserved.
#[derive(Default)]
struct Listings {
    bytes: Vec<u8>,
    /// (offset, length, entry count) per distinct listing.
    spans: Vec<(u32, u32, u32)>,
    /// Open-addressed value-dedup table: span index + 1 (0 = empty).
    table: Vec<u32>,
    hits: u64,
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl Listings {
    fn intern(&mut self, listing: &[u8], entries: u32) -> u32 {
        if self.table.len() < (self.spans.len() + 1) * 2 {
            let cap = ((self.spans.len() + 1) * 4).next_power_of_two().max(1024);
            let mut t = vec![0u32; cap];
            for (si, &(off, len, _)) in self.spans.iter().enumerate() {
                let b = &self.bytes[off as usize..(off + len) as usize];
                let mut i = (fnv64(b) as usize) & (cap - 1);
                while t[i] != 0 {
                    i = (i + 1) & (cap - 1);
                }
                t[i] = si as u32 + 1;
            }
            self.table = t;
        }
        let mask = self.table.len() - 1;
        let mut i = (fnv64(listing) as usize) & mask;
        loop {
            match self.table[i] {
                0 => {
                    let off = self.bytes.len() as u32;
                    self.bytes.extend_from_slice(listing);
                    self.spans.push((off, listing.len() as u32, entries));
                    self.table[i] = self.spans.len() as u32;
                    return self.spans.len() as u32 - 1;
                }
                si => {
                    let (off, len, _) = self.spans[si as usize - 1];
                    if self.bytes[off as usize..(off + len) as usize] == *listing {
                        self.hits += 1;
                        return si - 1;
                    }
                }
            }
            i = (i + 1) & mask;
        }
    }
}

fn hex_sha(hex: &[u8], olen: usize) -> Option<Vec<u8>> {
    if hex.len() < olen * 2 {
        return None;
    }
    let mut sha = vec![0u8; olen];
    hex::decode_to_slice(std::str::from_utf8(&hex[..olen * 2]).ok()?, &mut sha).ok()?;
    Some(sha)
}

#[derive(Default)]
pub struct Stats {
    pub commits: usize,
    pub trees: usize,
    pub blobs: usize,
    pub tags: usize,
    pub dummies: usize,
    pub duplicates: usize,
    pub listings: usize,
    pub listing_refs: u64,
    pub head_slots: usize,
    pub bytes_heads: usize,
    pub bytes_shas: usize,
    pub bytes_tree_bodies: usize,
    pub bytes_kids: usize,
    pub bytes_listings: usize,
    pub bytes_commit_bodies: usize,
    pub bytes_parents: usize,
    pub bytes_blob_locs: usize,
    pub bytes_tag_bodies: usize,
}

impl Stats {
    pub fn total_bytes(&self) -> usize {
        self.bytes_heads
            + self.bytes_shas
            + self.bytes_tree_bodies
            + self.bytes_kids
            + self.bytes_listings
            + self.bytes_commit_bodies
            + self.bytes_parents
            + self.bytes_blob_locs
            + self.bytes_tag_bodies
    }
}

pub(crate) struct MemGraph {
    kind: crate::HashKind,
    heads: HeadTable,
    trees: Vec<TreeBody>,
    kids: Vec<u32>,
    listings: Listings,
    commits: Vec<CommitBody>,
    parents: Vec<u32>,
    blobs: Vec<Loc>,
    tags: Vec<TagBody>,
    duplicates: usize,
    scratch: Vec<u8>,
}

impl MemGraph {
    fn with_capacity(kind: crate::HashKind, objects: usize) -> MemGraph {
        let mut heads = HeadTable::with_capacity(objects);
        heads.olen = kind.oid_len();
        MemGraph {
            kind,
            heads,
            trees: Vec::new(),
            kids: Vec::new(),
            listings: Listings::default(),
            commits: Vec::new(),
            parents: Vec::new(),
            blobs: Vec::new(),
            tags: Vec::new(),
            duplicates: 0,
            scratch: Vec::new(),
        }
    }

    fn add(&mut self, sha: &[u8], typ: u8, loc: Loc, data: &[u8]) -> Result<()> {
        let slot = self.heads.find_or_insert(sha)?;
        let h = self.heads.slots[slot as usize];
        if h.kind != K_DUMMY {
            self.duplicates += 1; // same object seen twice: keep the first
            return Ok(());
        }
        let (kind, body) = match typ {
            3 => {
                self.blobs.push(loc);
                (K_BLOB, self.blobs.len() as u32 - 1)
            }
            2 => {
                // Tree entries: `<mode> <name>\0<oid-width bytes>`.
                let olen = self.kind.oid_len();
                self.scratch.clear();
                let kids_off = self.kids.len() as u32;
                let mut entries = 0u32;
                let mut i = 0usize;
                while i < data.len() {
                    let nul = data[i..]
                        .iter()
                        .position(|&b| b == 0)
                        .ok_or_else(|| Error::Chain("memgraph: malformed tree".into()))?
                        + i;
                    if nul + 1 + olen > data.len() {
                        return Err(Error::Chain("memgraph: truncated tree entry".into()));
                    }
                    self.scratch.extend_from_slice(&data[i..nul + 1]);
                    let kid = self.heads.find_or_insert(&data[nul + 1..nul + 1 + olen])?;
                    self.kids.push(kid);
                    entries += 1;
                    i = nul + 1 + olen;
                }
                let scratch = std::mem::take(&mut self.scratch);
                let listing = self.listings.intern(&scratch, entries);
                self.scratch = scratch;
                self.trees.push(TreeBody { listing, kids_off });
                (K_TREE, self.trees.len() as u32 - 1)
            }
            1 => {
                // Commit text: `tree <hex>` then `parent <hex>` lines.
                let mut tree = u32::MAX;
                let par_off = self.parents.len() as u32;
                let olen = self.kind.oid_len();
                for line in data.split(|&b| b == b'\n') {
                    if let Some(hexs) = line.strip_prefix(b"tree ") {
                        let sha = hex_sha(hexs, olen)
                            .ok_or_else(|| Error::Chain("memgraph: bad tree line".into()))?;
                        tree = self.heads.find_or_insert(&sha)?;
                    } else if let Some(hexs) = line.strip_prefix(b"parent ") {
                        let sha = hex_sha(hexs, olen)
                            .ok_or_else(|| Error::Chain("memgraph: bad parent line".into()))?;
                        let p = self.heads.find_or_insert(&sha)?;
                        self.parents.push(p);
                    } else if line.is_empty() {
                        break; // headers end at the blank line
                    }
                }
                let par_len = (self.parents.len() as u32 - par_off) as u16;
                self.commits.push(CommitBody { loc, tree, par_off, par_len });
                (K_COMMIT, self.commits.len() as u32 - 1)
            }
            4 => {
                let mut target = u32::MAX;
                for line in data.split(|&b| b == b'\n') {
                    if let Some(hexs) = line.strip_prefix(b"object ") {
                        if let Some(sha) = hex_sha(hexs, self.kind.oid_len()) {
                            target = self.heads.find_or_insert(&sha)?;
                        }
                        break;
                    }
                }
                self.tags.push(TagBody { loc, target });
                (K_TAG, self.tags.len() as u32 - 1)
            }
            other => return Err(Error::Chain(format!("memgraph: object type {other}"))),
        };
        self.heads.slots[slot as usize] = Head { id: h.id, kind, body };
        Ok(())
    }

    // ------------------------------------------------ graph accessors

    pub(crate) fn slot_of_hex(&self, oid: &str) -> Option<u32> {
        if oid.len() != self.kind.hex_len() {
            return None;
        }
        self.heads.lookup(&hex_sha(oid.as_bytes(), self.kind.oid_len())?)
    }

    fn kind(&self, slot: u32) -> u8 {
        self.heads.slots[slot as usize].kind
    }

    pub(crate) fn sha_hex(&self, slot: u32) -> String {
        hex::encode(&self.heads.shas[slot as usize][..self.heads.olen])
    }

    pub(crate) fn is_commit(&self, slot: u32) -> bool {
        self.kind(slot) == K_COMMIT
    }
    pub(crate) fn is_tree(&self, slot: u32) -> bool {
        self.kind(slot) == K_TREE
    }
    pub(crate) fn is_tag(&self, slot: u32) -> bool {
        self.kind(slot) == K_TAG
    }
    pub(crate) fn is_blob(&self, slot: u32) -> bool {
        self.kind(slot) == K_BLOB
    }

    /// A commit's tree slot and parent slots.
    pub(crate) fn commit_parts(&self, slot: u32) -> (u32, &[u32]) {
        let b = &self.commits[self.heads.slots[slot as usize].body as usize];
        (b.tree, &self.parents[b.par_off as usize..(b.par_off + b.par_len as u32) as usize])
    }

    /// A tag's target slot.
    pub(crate) fn tag_target(&self, slot: u32) -> u32 {
        self.tags[self.heads.slots[slot as usize].body as usize].target
    }

    /// A tree's `(name, entry)` pairs, listing order (= git tree order).
    fn tree_ents(&self, slot: u32) -> Vec<(Vec<u8>, Ent)> {
        let b = &self.trees[self.heads.slots[slot as usize].body as usize];
        let (off, len, n) = self.listings.spans[b.listing as usize];
        let bytes = &self.listings.bytes[off as usize..(off + len) as usize];
        let mut out = Vec::with_capacity(n as usize);
        let mut i = 0usize;
        for k in 0..n {
            let sp = bytes[i..].iter().position(|&b| b == b' ').unwrap() + i;
            let nul = bytes[sp..].iter().position(|&b| b == 0).unwrap() + sp;
            let mode = bytes[i..sp].to_vec();
            let name = bytes[sp + 1..nul].to_vec();
            let kid = self.kids[b.kids_off as usize + k as usize];
            let is_dir = mode == b"40000";
            out.push((name, Ent { mode, oid: self.sha_hex(kid), is_dir }));
            i = nul + 1;
        }
        out
    }

    fn blob_ofs(&self, slot: u32) -> Option<u64> {
        (self.kind(slot) == K_BLOB)
            .then(|| self.blobs[self.heads.slots[slot as usize].body as usize].ofs)
    }

    fn stats(&self) -> Stats {
        Stats {
            commits: self.commits.len(),
            trees: self.trees.len(),
            blobs: self.blobs.len(),
            tags: self.tags.len(),
            dummies: self.heads.slots.iter().filter(|h| h.kind == K_DUMMY).count(),
            duplicates: self.duplicates,
            listings: self.listings.spans.len(),
            listing_refs: self.listings.hits + self.listings.spans.len() as u64,
            head_slots: self.heads.slots.len(),
            bytes_heads: self.heads.slots.len() * std::mem::size_of::<Head>(),
            bytes_shas: self.heads.shas.len() * self.heads.olen,
            bytes_tree_bodies: self.trees.len() * std::mem::size_of::<TreeBody>(),
            bytes_kids: self.kids.len() * 4,
            bytes_listings: self.listings.bytes.len()
                + self.listings.spans.len() * 12
                + self.listings.table.len() * 4,
            bytes_commit_bodies: self.commits.len() * std::mem::size_of::<CommitBody>(),
            bytes_parents: self.parents.len() * 4,
            bytes_blob_locs: self.blobs.len() * std::mem::size_of::<Loc>(),
            bytes_tag_bodies: self.tags.len() * std::mem::size_of::<TagBody>(),
        }
    }
}

// ---------------------------------------------------------- wire pack scan

/// Parse a pack entry header at `ofs`: `(type, size_hint, header_len)`.
fn entry_header(file: &std::fs::File, ofs: u64) -> Result<(u8, u64, usize)> {
    use std::os::unix::fs::FileExt as _;
    let mut hdr = [0u8; 32];
    let got = file.read_at(&mut hdr, ofs)?;
    if got == 0 {
        return Err(Error::Chain("pack: offset past end".into()));
    }
    let mut p = 0usize;
    let b0 = hdr[p];
    p += 1;
    let typ = (b0 >> 4) & 0x7;
    let mut size = (b0 & 0x0f) as u64;
    let mut shift = 4;
    let mut cont = b0 & 0x80 != 0;
    while cont {
        let b = hdr[p];
        p += 1;
        size |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        cont = b & 0x80 != 0;
    }
    Ok((typ, size, p))
}

/// Read the OFS_DELTA negative-offset varint at `pos`: `(rel, len)`.
fn negofs(file: &std::fs::File, pos: u64) -> Result<(u64, usize)> {
    use std::os::unix::fs::FileExt as _;
    let mut buf = [0u8; 16];
    file.read_at(&mut buf, pos)?;
    let mut p = 0usize;
    let mut b = buf[p];
    p += 1;
    let mut rel = (b & 0x7f) as u64;
    while b & 0x80 != 0 {
        b = buf[p];
        p += 1;
        rel = ((rel + 1) << 7) | (b & 0x7f) as u64;
    }
    Ok((rel, p))
}

/// A reader over ONE pack file with a bounded FIFO base cache — delta
/// chains resolve recursively through it, exactly like the indexed-pack
/// reader, but ref-delta bases resolve through the GRAPH (sha → offset)
/// instead of an `.idx`.
struct PackReader {
    file: std::fs::File,
    cache: std::collections::HashMap<u64, (u8, Arc<Vec<u8>>)>,
    order: std::collections::VecDeque<u64>,
    bytes: usize,
}

const BASE_CACHE_BYTES: usize = 32 << 20;

impl PackReader {
    fn open(pack: &Path) -> Result<PackReader> {
        Ok(PackReader {
            file: std::fs::File::open(pack)?,
            cache: Default::default(),
            order: Default::default(),
            bytes: 0,
        })
    }

    /// The typed body at `ofs`. `base_of(sha)` maps a REF_DELTA base to its
    /// pack offset (from the graph); unknown ⇒ `None` (a not-yet-scanned
    /// forward reference — the scan defers the entry). `olen` is the pack's
    /// oid width (its repo's hash format).
    fn entry_at(
        &mut self,
        ofs: u64,
        olen: usize,
        base_of: &dyn Fn(&[u8]) -> Option<u64>,
    ) -> Result<Option<(u8, Arc<Vec<u8>>)>> {
        if let Some(hit) = self.cache.get(&ofs) {
            return Ok(Some(hit.clone()));
        }
        use std::os::unix::fs::FileExt as _;
        let (typ, size, hlen) = entry_header(&self.file, ofs)?;
        let resolved = match typ {
            1..=4 => {
                let data = inflate_at(&self.file, ofs + hlen as u64, size as usize)?;
                (typ, Arc::new(data))
            }
            6 => {
                let (rel, nlen) = negofs(&self.file, ofs + hlen as u64)?;
                let base_ofs = ofs
                    .checked_sub(rel)
                    .ok_or_else(|| Error::Chain("pack: ofs-delta before start".into()))?;
                let Some((bt, base)) = self.entry_at(base_ofs, olen, base_of)? else {
                    return Ok(None);
                };
                let delta =
                    inflate_at(&self.file, ofs + (hlen + nlen) as u64, size as usize)?;
                (bt, Arc::new(apply_delta(&base, &delta).map_err(|e| Error::Chain(e.to_string()))?))
            }
            7 => {
                let mut sha = [0u8; 32];
                self.file.read_at(&mut sha[..olen], ofs + hlen as u64)?;
                let Some(base_ofs) = base_of(&sha[..olen]) else { return Ok(None) };
                let Some((bt, base)) = self.entry_at(base_ofs, olen, base_of)? else {
                    return Ok(None);
                };
                let delta =
                    inflate_at(&self.file, ofs + hlen as u64 + olen as u64, size as usize)?;
                (bt, Arc::new(apply_delta(&base, &delta).map_err(|e| Error::Chain(e.to_string()))?))
            }
            t => return Err(Error::Chain(format!("pack: entry type {t}"))),
        };
        self.bytes += resolved.1.len();
        self.cache.insert(ofs, resolved.clone());
        self.order.push_back(ofs);
        while self.bytes > BASE_CACHE_BYTES {
            let Some(old) = self.order.pop_front() else { break };
            if let Some((_, d)) = self.cache.remove(&old) {
                self.bytes -= d.len();
            }
        }
        Ok(Some(resolved))
    }

    /// The compressed span of the entry at `ofs` (header + zlib stream),
    /// i.e. where the NEXT entry starts — a sequential-scan step.
    fn entry_span(&mut self, ofs: u64, olen: usize) -> Result<u64> {
        let (typ, size, hlen) = entry_header(&self.file, ofs)?;
        let extra = match typ {
            1..=4 => 0,
            6 => negofs(&self.file, ofs + hlen as u64)?.1 as u64,
            7 => olen as u64,
            t => return Err(Error::Chain(format!("pack: entry type {t}"))),
        };
        let mut probe = [0u8; 0];
        let _ = probe; // (kept for symmetry; nothing to pre-read)
        let (_, consumed) =
            inflate_at_counted(&self.file, ofs + hlen as u64 + extra, size as usize)?;
        Ok(hlen as u64 + extra + consumed)
    }
}

/// One received wire pack, scanned into a graph: the graph plus the pack
/// path (blob bytes stay pack-resident for the life of the import run).
pub(crate) struct PackGraph {
    pub(crate) graph: MemGraph,
    pub(crate) pack: PathBuf,
}

/// Scan a SELF-CONTAINED pack (the `no-thin` fetch guarantee) into the
/// resident graph: one forward sweep — inflate, resolve in-pack deltas,
/// HASH each object in the repo's format (the wire carries no ids) — with
/// ref-delta forward references deferred and retried until stable.
pub(crate) fn build_pack(pack: &Path, kind: crate::HashKind) -> Result<PackGraph> {
    use std::os::unix::fs::FileExt as _;
    let mut rd = PackReader::open(pack)?;
    let mut hdr = [0u8; 12];
    rd.file.read_at(&mut hdr, 0)?;
    if &hdr[..4] != b"PACK" {
        return Err(Error::Chain("not a pack file".into()));
    }
    let ver = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
    if ver != 2 && ver != 3 {
        return Err(Error::Chain(format!("pack version {ver} unsupported")));
    }
    let count = u32::from_be_bytes(hdr[8..12].try_into().unwrap()) as usize;
    let mut g = MemGraph::with_capacity(kind, count + count / 8 + 1024);

    let olen = kind.oid_len();
    let hash_add = |g: &mut MemGraph, rd: &mut PackReader, ofs: u64| -> Result<bool> {
        // Ref-delta bases resolve through what the graph has scanned so far.
        let (typ, data) = {
            let base_of = |sha: &[u8]| -> Option<u64> {
                let slot = g.heads.lookup(sha)?;
                let h = g.heads.slots[slot as usize];
                match h.kind {
                    K_BLOB => Some(g.blobs[h.body as usize].ofs),
                    K_COMMIT => Some(g.commits[h.body as usize].loc.ofs),
                    K_TAG => Some(g.tags[h.body as usize].loc.ofs),
                    // Trees keep no Loc — their bytes are fully represented
                    // in the graph; rebuild for the delta base.
                    K_TREE => None,
                    _ => None,
                }
            };
            match rd.entry_at(ofs, olen, &base_of)? {
                Some(r) => r,
                None => {
                    // The base may be a TREE (no Loc) — rebuild its bytes.
                    let base_of_tree = |sha: &[u8]| -> Option<Vec<u8>> {
                        let slot = g.heads.lookup(sha)?;
                        g.is_tree(slot).then(|| tree_bytes(g, slot))
                    };
                    match rd_entry_with_tree_base(rd, ofs, olen, &base_of_tree)? {
                        Some(r) => r,
                        None => return Ok(false),
                    }
                }
            }
        };
        let typ_name: &str = match typ {
            1 => "commit",
            2 => "tree",
            3 => "blob",
            4 => "tag",
            t => return Err(Error::Chain(format!("pack: resolved type {t}"))),
        };
        let digest = kind.obj_digest(typ_name, &data);
        g.add(&digest[..olen], typ, Loc { pack: 0, ofs }, &data)?;
        Ok(true)
    };

    let mut ofs = 12u64;
    let mut deferred: Vec<u64> = Vec::new();
    for _ in 0..count {
        let span = rd.entry_span(ofs, olen)?;
        if !hash_add(&mut g, &mut rd, ofs)? {
            deferred.push(ofs);
        }
        ofs += span;
    }
    // Ref-delta forward references: retry until no progress.
    while !deferred.is_empty() {
        let before = deferred.len();
        let mut still = Vec::new();
        for ofs in deferred {
            if !hash_add(&mut g, &mut rd, ofs)? {
                still.push(ofs);
            }
        }
        if still.len() == before {
            return Err(Error::Chain(format!(
                "pack: {} entries with unresolvable delta bases (thin pack? fetch must be no-thin)",
                still.len()
            )));
        }
        deferred = still;
    }
    Ok(PackGraph { graph: g, pack: pack.to_path_buf() })
}

/// Rebuild a tree object's canonical bytes from the graph (listing + child
/// shas) — the delta-base case for trees, whose bodies keep no pack Loc.
fn tree_bytes(g: &MemGraph, slot: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let b = &g.trees[g.heads.slots[slot as usize].body as usize];
    let (off, len, n) = g.listings.spans[b.listing as usize];
    let bytes = &g.listings.bytes[off as usize..(off + len) as usize];
    let mut i = 0usize;
    for k in 0..n {
        let nul = bytes[i..].iter().position(|&b| b == 0).unwrap() + i;
        out.extend_from_slice(&bytes[i..nul + 1]);
        let kid = g.kids[b.kids_off as usize + k as usize];
        out.extend_from_slice(&g.heads.shas[kid as usize][..g.heads.olen]);
        i = nul + 1;
    }
    out
}

/// `PackReader::entry_at` for the one case it cannot serve alone: a
/// REF_DELTA whose base is a tree already absorbed into the graph.
fn rd_entry_with_tree_base(
    rd: &mut PackReader,
    ofs: u64,
    olen: usize,
    base_of_tree: &dyn Fn(&[u8]) -> Option<Vec<u8>>,
) -> Result<Option<(u8, Arc<Vec<u8>>)>> {
    use std::os::unix::fs::FileExt as _;
    let (typ, size, hlen) = entry_header(&rd.file, ofs)?;
    if typ != 7 {
        return Ok(None);
    }
    let mut sha = [0u8; 32];
    rd.file.read_at(&mut sha[..olen], ofs + hlen as u64)?;
    let Some(base) = base_of_tree(&sha[..olen]) else { return Ok(None) };
    let delta = inflate_at(&rd.file, ofs + hlen as u64 + olen as u64, size as usize)?;
    let body = apply_delta(&base, &delta).map_err(|e| Error::Chain(e.to_string()))?;
    Ok(Some((2, Arc::new(body))))
}

// ------------------------------------------------- the encoder's Objects

/// The graph as the union encoder's object source: trees from listings +
/// child pointers (parsed once at scan, never re-inflated), blob bytes from
/// the transient pack file. The empty tree is virtual, as in git itself.
pub(crate) struct GraphObjects {
    pg: Arc<PackGraph>,
    rd: PackReader,
    /// Parsed-tree cache, BOUNDED like the cat-file reader's: the parsed
    /// form (per-entry heap strings) is ~40x the graph's compact listing,
    /// so an unbounded cache would dwarf the graph itself (measured 12.5GB
    /// on git.git). oid → (entries, approx bytes).
    trees: std::collections::HashMap<String, (Arc<Vec<(Vec<u8>, Ent)>>, usize)>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
    reads: usize,
}

/// Parsed-tree cache budget (see `lanestore`'s TREE_CACHE_BUDGET twin).
const GRAPH_TREE_CACHE_BUDGET: usize = 64 << 20;

fn ents_bytes(ents: &[(Vec<u8>, Ent)]) -> usize {
    ents.iter().map(|(n, e)| 96 + n.len() + e.mode.len() + e.oid.len()).sum::<usize>() + 64
}

impl GraphObjects {
    pub(crate) fn new(pg: Arc<PackGraph>) -> Result<GraphObjects> {
        let rd = PackReader::open(&pg.pack)?;
        Ok(GraphObjects {
            pg,
            rd,
            trees: Default::default(),
            order: Default::default(),
            bytes: 0,
            reads: 0,
        })
    }
}

impl Objects for GraphObjects {
    fn tree(&mut self, oid: &str) -> Result<Arc<Vec<(Vec<u8>, Ent)>>> {
        if let Some((hit, _)) = self.trees.get(oid) {
            return Ok(hit.clone());
        }
        if oid == self.pg.graph.kind.empty_tree_hex() {
            return Ok(Arc::new(Vec::new()));
        }
        let slot = self
            .pg
            .graph
            .slot_of_hex(oid)
            .filter(|&s| self.pg.graph.is_tree(s))
            .ok_or_else(|| Error::Chain(format!("memgraph: tree {oid} not in pack")))?;
        self.reads += 1;
        let ents = Arc::new(self.pg.graph.tree_ents(slot));
        let sz = ents_bytes(&ents);
        while self.bytes + sz > GRAPH_TREE_CACHE_BUDGET {
            let Some(old) = self.order.pop_front() else { break };
            if let Some((_, osz)) = self.trees.remove(&old) {
                self.bytes -= osz;
            }
        }
        self.trees.insert(oid.to_string(), (ents.clone(), sz));
        self.order.push_back(oid.to_string());
        self.bytes += sz;
        Ok(ents)
    }

    fn blob(&mut self, oid: &str) -> Result<depot::Bytes> {
        let g = &self.pg.graph;
        let slot = g
            .slot_of_hex(oid)
            .filter(|&s| g.is_blob(s))
            .ok_or_else(|| Error::Chain(format!("memgraph: blob {oid} not in pack")))?;
        let ofs = g.blob_ofs(slot).expect("blob slot has a loc");
        self.reads += 1;
        let base_of = |sha: &[u8]| -> Option<u64> {
            let s = g.heads.lookup(sha)?;
            let h = g.heads.slots[s as usize];
            match h.kind {
                K_BLOB => Some(g.blobs[h.body as usize].ofs),
                K_COMMIT => Some(g.commits[h.body as usize].loc.ofs),
                K_TAG => Some(g.tags[h.body as usize].loc.ofs),
                _ => None,
            }
        };
        let (_, data) = self
            .rd
            .entry_at(ofs, g.kind.oid_len(), &base_of)?
            .ok_or_else(|| Error::Chain(format!("memgraph: blob {oid} base unresolvable")))?;
        Ok(data.as_ref().clone().into())
    }

    fn reads(&self) -> usize {
        self.reads
    }
}

// --------------------------------------------------------------- reports

/// Scan every object of `repo` (packs in offset order, then loose) into the
/// resident graph — the repo-based measurement path.
pub fn build(repo: &Path) -> Result<Stats> {
    let mut st = ObjectStore::open(repo)?;
    let kind = crate::HashKind::of_repo(repo);
    let loose = st.loose_oids();
    let packed: usize = (0..st.pack_count()).map(|p| st.pack_len(p)).sum();
    let mut g = MemGraph::with_capacity(kind, packed + loose.len() + (packed / 8) + 1024);
    for p in 0..st.pack_count() {
        // Offset order: one forward sweep through the pack file.
        let mut order: Vec<(u64, Vec<u8>)> = (0..st.pack_len(p))
            .map(|i| {
                let (sha, ofs) = st.pack_entry(p, i);
                (ofs, sha)
            })
            .collect();
        order.sort_unstable();
        for (ofs, sha) in order {
            let (typ, data) = st.entry_at(p, ofs)?;
            g.add(&sha, typ, Loc { pack: p as u32, ofs }, &data)?;
        }
    }
    for sha in loose {
        if let Some((typ, data)) = st.loose_typed(&sha)? {
            g.add(&sha, typ, Loc { pack: u32::MAX, ofs: 0 }, &data)?;
        }
    }
    Ok(g.stats())
}

/// [`build`] straight off ONE pack file — no repo, no `.idx`: ids are
/// hashed during the scan. A bare pack does not name its repo's hash
/// format, so try SHA-1 and fall back to SHA-256 (the tree-entry oid
/// width makes a wrong guess fail loudly, not silently).
pub fn build_pack_stats(pack: &Path) -> Result<Stats> {
    match build_pack(pack, crate::HashKind::Sha1) {
        Ok(pg) => Ok(pg.graph.stats()),
        Err(_) => Ok(build_pack(pack, crate::HashKind::Sha256)?.graph.stats()),
    }
}

fn render(s: &Stats, dt: std::time::Duration) -> String {
    let mb = |b: usize| format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0));
    format!(
        "objects: {} commits, {} trees, {} blobs, {} tags; {} external refs (dummies), {} duplicates\n\
         listings: {} distinct for {} tree references ({:.2}x shared)\n\
         memory: heads {} ({} slots x 16B) + shas {} | tree bodies {} | child ptrs {} | listings {}\n\
         \x20        commit bodies {} | parent ptrs {} | blob locs {} | tag bodies {}\n\
         total resident graph: {}   (built in {:.2}s)",
        s.commits, s.trees, s.blobs, s.tags, s.dummies, s.duplicates,
        s.listings, s.listing_refs,
        s.listing_refs as f64 / s.listings.max(1) as f64,
        mb(s.bytes_heads), s.head_slots, mb(s.bytes_shas),
        mb(s.bytes_tree_bodies), mb(s.bytes_kids), mb(s.bytes_listings),
        mb(s.bytes_commit_bodies), mb(s.bytes_parents), mb(s.bytes_blob_locs), mb(s.bytes_tag_bodies),
        mb(s.total_bytes()), dt.as_secs_f64(),
    )
}

/// Human-readable report for the CLI: a repo dir, or a bare `.pack` file.
pub fn report(target: &Path) -> Result<String> {
    let t0 = std::time::Instant::now();
    let s = if target.extension().is_some_and(|x| x == "pack") {
        build_pack_stats(target)?
    } else {
        build(target)?
    };
    Ok(render(&s, t0.elapsed()))
}
