//! The resident object graph — an EXPERIMENT ("we are messing around now:
//! let's see how big that is"): every object of a repository held in RAM as
//! a fixed-size header in ONE hashtable plus a compact variable-size body,
//! with pointers (u32 slot indices) instead of oids everywhere.
//!
//! The shape, per the commission:
//!
//! - ids are TRUNCATED to 64 bits (the sha's first 8 bytes). The hashtable
//!   is open-addressed over the headers themselves — the id is embedded in
//!   the header and appears NOWHERE else (no duplicate key storage), and
//!   since the id IS uniformly random sha bits it is its own hash.
//! - forward references get a DUMMY header (kind = unknown, no body) so a
//!   tree parsed before its children can already point at them; the dummy
//!   is filled in when the real object is scanned. The table is sized once
//!   from the pack index counts, so slot indices are stable pointers.
//! - a Tree's body points at a shared LISTING — the ordered `(mode, name)`
//!   sequence, deduplicated by value in a second hashtable (siblings across
//!   history share it) — plus a vector of child pointers in listing order.
//! - a Blob's body is only WHERE TO READ its content (pack + offset, or
//!   loose). A Commit is that plus tree and parent pointers; a Tag that
//!   plus the target pointer.
//!
//! Truncation caveat, stated: two distinct shas sharing a 64-bit prefix
//! would fuse (undetected here); the odds are ~n²/2⁶⁵ — about 2·10⁻⁷ for
//! ten million objects.

use std::path::Path;

use crate::gitobj::ObjectStore;
use crate::{Error, Result};

const K_EMPTY: u8 = 0;
const K_DUMMY: u8 = 1;
const K_COMMIT: u8 = 2;
const K_TREE: u8 = 3;
const K_BLOB: u8 = 4;
const K_TAG: u8 = 5;

/// The fixed-size header: the embedded truncated id (the hash key), the
/// kind, and the body index into the kind's arena. 16 bytes as laid out.
#[derive(Clone, Copy)]
struct Head {
    id: u64,
    kind: u8,
    body: u32,
}

/// Where an object's bytes live on disk: `pack == u32::MAX` ⇒ loose.
#[derive(Clone, Copy)]
struct Loc {
    pack: u32,
    ofs: u64,
}

struct TreeBody {
    listing: u32,
    /// Offset into the flat child-pointer arena; the length is the
    /// listing's entry count.
    kids_off: u32,
}

struct CommitBody {
    loc: Loc,
    tree: u32,
    par_off: u32,
    par_len: u16,
}

struct TagBody {
    loc: Loc,
    target: u32,
}

/// The header table: open addressing, linear probing, sized ONCE.
struct HeadTable {
    slots: Vec<Head>,
    mask: usize,
    n: usize,
}

impl HeadTable {
    fn with_capacity(objects: usize) -> HeadTable {
        let cap = (objects * 4 / 3 + 64).next_power_of_two();
        HeadTable {
            slots: vec![Head { id: 0, kind: K_EMPTY, body: u32::MAX }; cap],
            mask: cap - 1,
            n: 0,
        }
    }

    /// The slot of `id`, inserting a dummy header if absent. The id is its
    /// own hash (uniform sha bits).
    fn find_or_insert(&mut self, id: u64) -> Result<u32> {
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
                self.n += 1;
                return Ok(i as u32);
            }
            if s.id == id {
                return Ok(i as u32);
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

fn id64(sha: &[u8; 20]) -> u64 {
    u64::from_be_bytes(sha[..8].try_into().unwrap())
}

fn hex40_id(hex: &[u8]) -> Option<u64> {
    if hex.len() < 16 {
        return None;
    }
    let mut v: u64 = 0;
    for &c in &hex[..16] {
        v = (v << 4) | (c as char).to_digit(16)? as u64;
    }
    Some(v)
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
            + self.bytes_tree_bodies
            + self.bytes_kids
            + self.bytes_listings
            + self.bytes_commit_bodies
            + self.bytes_parents
            + self.bytes_blob_locs
            + self.bytes_tag_bodies
    }
}

struct MemGraph {
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
    fn add(&mut self, sha: &[u8; 20], typ: u8, loc: Loc, data: &[u8]) -> Result<()> {
        let slot = self.heads.find_or_insert(id64(sha))?;
        let h = self.heads.slots[slot as usize];
        if h.kind != K_DUMMY {
            self.duplicates += 1; // same object in several packs: keep the first
            return Ok(());
        }
        let (kind, body) = match typ {
            3 => {
                self.blobs.push(loc);
                (K_BLOB, self.blobs.len() as u32 - 1)
            }
            2 => {
                // Tree entries: `<mode> <name>\0<20-byte oid>`.
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
                    if nul + 21 > data.len() {
                        return Err(Error::Chain("memgraph: truncated tree entry".into()));
                    }
                    self.scratch.extend_from_slice(&data[i..nul + 1]);
                    let mut child = [0u8; 20];
                    child.copy_from_slice(&data[nul + 1..nul + 21]);
                    let kid = self.heads.find_or_insert(id64(&child))?;
                    self.kids.push(kid);
                    entries += 1;
                    i = nul + 21;
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
                for line in data.split(|&b| b == b'\n') {
                    if let Some(hexs) = line.strip_prefix(b"tree ") {
                        let id = hex40_id(hexs)
                            .ok_or_else(|| Error::Chain("memgraph: bad tree line".into()))?;
                        tree = self.heads.find_or_insert(id)?;
                    } else if let Some(hexs) = line.strip_prefix(b"parent ") {
                        let id = hex40_id(hexs)
                            .ok_or_else(|| Error::Chain("memgraph: bad parent line".into()))?;
                        let p = self.heads.find_or_insert(id)?;
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
                        if let Some(id) = hex40_id(hexs) {
                            target = self.heads.find_or_insert(id)?;
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
}

/// Scan every object of `repo` (packs in offset order, then loose) into the
/// resident graph and report its shape and memory.
pub fn build(repo: &Path) -> Result<Stats> {
    let mut st = ObjectStore::open(repo)?;
    let loose = st.loose_oids();
    let packed: usize = (0..st.pack_count()).map(|p| st.pack_len(p)).sum();
    // Sized once: slot indices are stable pointers. Slack covers out-of-repo
    // references (gitlinks, grafted parents) that only ever exist as dummies.
    let mut g = MemGraph {
        heads: HeadTable::with_capacity(packed + loose.len() + (packed / 8) + 1024),
        trees: Vec::new(),
        kids: Vec::new(),
        listings: Listings::default(),
        commits: Vec::new(),
        parents: Vec::new(),
        blobs: Vec::new(),
        tags: Vec::new(),
        duplicates: 0,
        scratch: Vec::new(),
    };
    for p in 0..st.pack_count() {
        // Offset order: one forward sweep through the pack file.
        let mut order: Vec<(u64, [u8; 20])> = (0..st.pack_len(p))
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

    let dummies =
        g.heads.slots.iter().filter(|h| h.kind == K_DUMMY).count();
    Ok(Stats {
        commits: g.commits.len(),
        trees: g.trees.len(),
        blobs: g.blobs.len(),
        tags: g.tags.len(),
        dummies,
        duplicates: g.duplicates,
        listings: g.listings.spans.len(),
        listing_refs: g.listings.hits + g.listings.spans.len() as u64,
        head_slots: g.heads.slots.len(),
        bytes_heads: g.heads.slots.len() * std::mem::size_of::<Head>(),
        bytes_tree_bodies: g.trees.len() * std::mem::size_of::<TreeBody>(),
        bytes_kids: g.kids.len() * 4,
        bytes_listings: g.listings.bytes.len()
            + g.listings.spans.len() * 12
            + g.listings.table.len() * 4,
        bytes_commit_bodies: g.commits.len() * std::mem::size_of::<CommitBody>(),
        bytes_parents: g.parents.len() * 4,
        bytes_blob_locs: g.blobs.len() * std::mem::size_of::<Loc>(),
        bytes_tag_bodies: g.tags.len() * std::mem::size_of::<TagBody>(),
    })
}

/// Human-readable report for the CLI.
pub fn report(repo: &Path) -> Result<String> {
    let t0 = std::time::Instant::now();
    let s = build(repo)?;
    let dt = t0.elapsed();
    let mb = |b: usize| format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0));
    Ok(format!(
        "objects: {} commits, {} trees, {} blobs, {} tags; {} external refs (dummies), {} pack duplicates\n\
         listings: {} distinct for {} tree references ({:.2}x shared)\n\
         memory: heads {} ({} slots x 16B) | tree bodies {} | child ptrs {} | listings {}\n\
         \x20        commit bodies {} | parent ptrs {} | blob locs {} | tag bodies {}\n\
         total resident graph: {}   (built in {:.2}s)",
        s.commits, s.trees, s.blobs, s.tags, s.dummies, s.duplicates,
        s.listings, s.listing_refs,
        s.listing_refs as f64 / s.listings.max(1) as f64,
        mb(s.bytes_heads), s.head_slots,
        mb(s.bytes_tree_bodies), mb(s.bytes_kids), mb(s.bytes_listings),
        mb(s.bytes_commit_bodies), mb(s.bytes_parents), mb(s.bytes_blob_locs), mb(s.bytes_tag_bodies),
        mb(s.total_bytes()), dt.as_secs_f64(),
    ))
}
