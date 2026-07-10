//! Native git object-store reader: loose objects + packfiles, replacing the
//! `git cat-file --batch` subprocess on the ingest hot path (profiled: ~60%
//! of import samples were the cat-file side — zlib inflate + pipe traffic —
//! and most of the rest was waiting on that pipe).
//!
//! What it reads, and why exactly this surface:
//!
//! - **Per-pack `.idx` v2.** A fetch transfers only the `.pack` stream; the
//!   receiving `git index-pack` builds the `.idx` locally while VERIFYING
//!   the pack (every object inflated, hashed, deltas resolved). The idx is
//!   therefore always present next to its pack and is the product of the
//!   exact work we refuse to redo — rebuilding our own index would re-pay
//!   the inflate+hash pass this module exists to eliminate.
//! - **Pack v2 entries** with OFS/REF delta chains (thin packs are fixed at
//!   index time, so a REF base is always local), through a bytes-bounded
//!   base cache.
//! - **Loose objects.** Small fetches (under `transfer.unpackLimit`) are
//!   exploded to loose objects by `unpack-objects` — no new pack at all —
//!   so incremental updates routinely find their new objects loose.
//! - **`objects/info/alternates`**, recursively (depth-capped).
//! - **NOT `.midx`** (an optional acceleration over per-pack idx files that
//!   remain authoritative) and **not sha256 repos** (idx v3 / 32-byte oids
//!   are refused loudly).

use std::collections::HashMap;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Error, Result};

/// Resolved-delta-base cache budget. Bases repeat heavily along delta
/// chains within one import; final objects are returned owned, not cached
/// (the callers above keep their own tree cache).
const BASE_CACHE_BUDGET: usize = 32 << 20;

fn ge(msg: String) -> Error {
    Error::Git(msg)
}

/// The `.git` directory of `repo`: a `.git` dir, a `.git` gitfile
/// (worktrees), or the repo itself when bare.
fn git_dir(repo: &Path) -> Result<PathBuf> {
    let dot = repo.join(".git");
    if dot.is_dir() {
        return Ok(dot);
    }
    if dot.is_file() {
        let s = std::fs::read_to_string(&dot)?;
        let p = s
            .strip_prefix("gitdir:")
            .ok_or_else(|| ge(format!("malformed gitfile {}", dot.display())))?
            .trim();
        let p = PathBuf::from(p);
        return Ok(if p.is_absolute() { p } else { repo.join(p) });
    }
    Ok(repo.to_path_buf())
}

/// One pack: its `.idx` held in memory (fanout + sorted oid table +
/// offsets), the `.pack` opened for positioned reads.
struct Pack {
    file: std::fs::File,
    idx: Vec<u8>,
    n: usize,
}

const IDX_MAGIC: [u8; 4] = [0xff, 0x74, 0x4f, 0x63];

impl Pack {
    fn open(idx_path: &Path) -> Result<Pack> {
        let idx = std::fs::read(idx_path)?;
        if idx.len() < 8 + 1024 || idx[..4] != IDX_MAGIC {
            return Err(ge(format!(
                "{}: not a v2 pack index (v1 or corrupt)",
                idx_path.display()
            )));
        }
        let ver = u32::from_be_bytes(idx[4..8].try_into().unwrap());
        if ver != 2 {
            return Err(ge(format!(
                "{}: pack index v{ver} unsupported (sha256 repo?)",
                idx_path.display()
            )));
        }
        let n = u32::from_be_bytes(idx[8 + 255 * 4..8 + 256 * 4].try_into().unwrap()) as usize;
        // fanout(1024) + sha(20n) + crc(4n) + ofs(4n) + trailer(40); large
        // offsets sit between ofs and trailer.
        if idx.len() < 8 + 1024 + 28 * n + 40 {
            return Err(ge(format!("{}: truncated pack index", idx_path.display())));
        }
        let file = std::fs::File::open(idx_path.with_extension("pack"))?;
        Ok(Pack { file, idx, n })
    }

    fn sha_at(&self, i: usize) -> &[u8] {
        let base = 8 + 1024 + 20 * i;
        &self.idx[base..base + 20]
    }

    /// Binary-search the sorted oid table via the fanout.
    fn lookup(&self, oid: &[u8; 20]) -> Option<u64> {
        let f = 8usize;
        let hi_end = u32::from_be_bytes(
            self.idx[f + oid[0] as usize * 4..f + oid[0] as usize * 4 + 4].try_into().unwrap(),
        ) as usize;
        let lo_end = if oid[0] == 0 {
            0
        } else {
            u32::from_be_bytes(
                self.idx[f + (oid[0] as usize - 1) * 4..f + oid[0] as usize * 4].try_into().unwrap(),
            ) as usize
        };
        let (mut lo, mut hi) = (lo_end, hi_end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.sha_at(mid).cmp(&oid[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.offset_at(mid)),
            }
        }
        None
    }

    fn offset_at(&self, i: usize) -> u64 {
        let ofs_base = 8 + 1024 + 24 * self.n;
        let v = u32::from_be_bytes(self.idx[ofs_base + 4 * i..ofs_base + 4 * i + 4].try_into().unwrap());
        if v & 0x8000_0000 == 0 {
            v as u64
        } else {
            let big = (v & 0x7fff_ffff) as usize;
            let big_base = 8 + 1024 + 28 * self.n + 8 * big;
            u64::from_be_bytes(self.idx[big_base..big_base + 8].try_into().unwrap())
        }
    }
}

/// A positioned reader over a pack's zlib stream: `read_at` chunks fed to a
/// raw `flate2::Decompress` until StreamEnd (the compressed length is not
/// recorded — the stream is self-terminating).
fn inflate_at(file: &std::fs::File, mut pos: u64, size_hint: usize) -> Result<Vec<u8>> {
    let mut z = flate2::Decompress::new(true);
    let mut out = Vec::with_capacity(size_hint.max(64));
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = file.read_at(&mut buf, pos)?;
        let before_in = z.total_in();
        let before_out = z.total_out();
        // At EOF the stream may still owe buffered OUTPUT (the last input
        // chunk was consumed while `out` was full): flush, don't fail.
        let status = z
            .decompress_vec(&buf[..n], &mut out, if n == 0 {
                flate2::FlushDecompress::Finish
            } else {
                flate2::FlushDecompress::None
            })
            .map_err(|e| ge(format!("object zlib: {e}")))?;
        pos += z.total_in() - before_in;
        match status {
            flate2::Status::StreamEnd => return Ok(out),
            flate2::Status::Ok | flate2::Status::BufError => {
                if out.len() == out.capacity() {
                    out.reserve(64 * 1024);
                } else if n == 0 && z.total_out() == before_out {
                    return Err(ge("object: truncated zlib stream".into()));
                }
            }
        }
    }
}

/// Apply a git pack delta (`base` + delta ops → object bytes).
fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut p = 0usize;
    let mut varint = || -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = *delta.get(p).ok_or_else(|| ge("delta: truncated varint".into()))?;
            p += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    };
    let src_size = varint()? as usize;
    let dst_size = varint()? as usize;
    if src_size != base.len() {
        return Err(ge("delta: base size mismatch".into()));
    }
    let mut out = Vec::with_capacity(dst_size);
    while p < delta.len() {
        let cmd = delta[p];
        p += 1;
        if cmd & 0x80 != 0 {
            // Copy from base: offset from bits 0-3, size from bits 4-6.
            let mut ofs = 0usize;
            let mut sz = 0usize;
            for (bit, shift) in [(0x01, 0), (0x02, 8), (0x04, 16), (0x08, 24)] {
                if cmd & bit != 0 {
                    ofs |= (*delta.get(p).ok_or_else(|| ge("delta: truncated copy".into()))? as usize) << shift;
                    p += 1;
                }
            }
            for (bit, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
                if cmd & bit != 0 {
                    sz |= (*delta.get(p).ok_or_else(|| ge("delta: truncated copy".into()))? as usize) << shift;
                    p += 1;
                }
            }
            if sz == 0 {
                sz = 0x10000;
            }
            let end = ofs.checked_add(sz).filter(|&e| e <= base.len())
                .ok_or_else(|| ge("delta: copy out of range".into()))?;
            out.extend_from_slice(&base[ofs..end]);
        } else if cmd != 0 {
            // Insert literal.
            let end = p.checked_add(cmd as usize).filter(|&e| e <= delta.len())
                .ok_or_else(|| ge("delta: truncated insert".into()))?;
            out.extend_from_slice(&delta[p..end]);
            p = end;
        } else {
            return Err(ge("delta: zero opcode".into()));
        }
    }
    if out.len() != dst_size {
        return Err(ge("delta: result size mismatch".into()));
    }
    Ok(out)
}

/// The native reader: object directories (main + alternates), their packs,
/// and a bytes-bounded resolved-base cache.
pub(crate) struct ObjectStore {
    dirs: Vec<PathBuf>,
    packs: Vec<Pack>,
    cache: HashMap<(usize, u64), (u8, Arc<Vec<u8>>)>,
    cache_order: std::collections::VecDeque<(usize, u64)>,
    cache_bytes: usize,
}

impl ObjectStore {
    pub(crate) fn open(repo: &Path) -> Result<ObjectStore> {
        let mut dirs = Vec::new();
        let mut pending = vec![git_dir(repo)?.join("objects")];
        // Alternates, recursively, depth-capped.
        for _ in 0..6 {
            let mut next = Vec::new();
            for d in pending.drain(..) {
                if dirs.contains(&d) {
                    continue;
                }
                if let Ok(alt) = std::fs::read_to_string(d.join("info/alternates")) {
                    for line in alt.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
                        let p = PathBuf::from(line);
                        next.push(if p.is_absolute() { p } else { d.join(p) });
                    }
                }
                dirs.push(d);
            }
            if next.is_empty() {
                break;
            }
            pending = next;
        }
        let mut st = ObjectStore {
            dirs,
            packs: Vec::new(),
            cache: HashMap::new(),
            cache_order: Default::default(),
            cache_bytes: 0,
        };
        st.scan_packs()?;
        Ok(st)
    }

    /// (Re)list `pack/*.idx` under every object dir — also called once on a
    /// lookup miss, since a fetch can land a new pack mid-run.
    fn scan_packs(&mut self) -> Result<()> {
        self.packs.clear();
        self.cache.clear();
        self.cache_order.clear();
        self.cache_bytes = 0;
        for d in &self.dirs {
            let Ok(rd) = std::fs::read_dir(d.join("pack")) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "idx") {
                    self.packs.push(Pack::open(&p)?);
                }
            }
        }
        Ok(())
    }

    /// The raw body of object `oid` (header stripped): a commit/tag's text,
    /// a tree's binary entries, a blob's content.
    pub(crate) fn get(&mut self, oid: &str) -> Result<Vec<u8>> {
        Ok(self.get_typed(oid)?.1)
    }

    fn get_typed(&mut self, oid: &str) -> Result<(u8, Vec<u8>)> {
        let mut bin = [0u8; 20];
        hex::decode_to_slice(oid, &mut bin)
            .map_err(|_| ge(format!("bad oid {oid:?} (sha256 repo?)")))?;
        if let Some(hit) = self.get_bin(&bin)? {
            return Ok(hit);
        }
        // Miss: a new pack may have landed since open (ladder rungs fetch
        // mid-run); rescan once, then try loose-vs-pack again.
        self.scan_packs()?;
        self.get_bin(&bin)?
            .ok_or_else(|| ge(format!("object {oid} not found (loose or packed)")))
    }

    fn get_bin(&mut self, oid: &[u8; 20]) -> Result<Option<(u8, Vec<u8>)>> {
        for i in 0..self.packs.len() {
            if let Some(ofs) = self.packs[i].lookup(oid) {
                let (t, data) = self.read_entry(i, ofs)?;
                return Ok(Some((t, data.as_ref().clone())));
            }
        }
        self.read_loose(oid)
    }

    /// A loose object: one zlib stream holding `<type> <size>\0<body>`.
    fn read_loose(&self, oid: &[u8; 20]) -> Result<Option<(u8, Vec<u8>)>> {
        let hexid = hex::encode(oid);
        for d in &self.dirs {
            let p = d.join(&hexid[..2]).join(&hexid[2..]);
            let Ok(f) = std::fs::File::open(&p) else { continue };
            let raw = inflate_at(&f, 0, 4096)?;
            let nul = raw
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| ge(format!("{}: no header", p.display())))?;
            let header = std::str::from_utf8(&raw[..nul])
                .map_err(|_| ge(format!("{}: bad header", p.display())))?;
            let t = match header.split(' ').next().unwrap_or("") {
                "commit" => 1,
                "tree" => 2,
                "blob" => 3,
                "tag" => 4,
                other => return Err(ge(format!("{}: object type {other:?}", p.display()))),
            };
            return Ok(Some((t, raw[nul + 1..].to_vec())));
        }
        Ok(None)
    }

    /// One pack entry: parse the header, inflate, resolve delta chains
    /// (recursively, through the base cache).
    fn read_entry(&mut self, pack: usize, ofs: u64) -> Result<(u8, Arc<Vec<u8>>)> {
        if let Some(hit) = self.cache.get(&(pack, ofs)) {
            return Ok(hit.clone());
        }
        let mut hdr = [0u8; 32];
        let got = self.packs[pack].file.read_at(&mut hdr, ofs)?;
        if got == 0 {
            return Err(ge("pack: offset past end".into()));
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
        let resolved: (u8, Arc<Vec<u8>>) = match typ {
            1..=4 => {
                let data = inflate_at(&self.packs[pack].file, ofs + p as u64, size as usize)?;
                (typ, Arc::new(data))
            }
            6 => {
                // OFS_DELTA: negative offset in git's chained varint form.
                let mut b = hdr[p];
                p += 1;
                let mut rel = (b & 0x7f) as u64;
                while b & 0x80 != 0 {
                    b = hdr[p];
                    p += 1;
                    rel = ((rel + 1) << 7) | (b & 0x7f) as u64;
                }
                let base_ofs = ofs
                    .checked_sub(rel)
                    .ok_or_else(|| ge("pack: ofs-delta base before pack start".into()))?;
                let (bt, base) = self.read_entry(pack, base_ofs)?;
                let delta = inflate_at(&self.packs[pack].file, ofs + p as u64, size as usize)?;
                (bt, Arc::new(apply_delta(&base, &delta)?))
            }
            7 => {
                // REF_DELTA: 20-byte base oid (local — thin packs are fixed
                // by index-pack).
                let mut base_oid = [0u8; 20];
                base_oid.copy_from_slice(&hdr[p..p + 20]);
                p += 20;
                let (bt, base) = self
                    .get_bin(&base_oid)?
                    .ok_or_else(|| ge(format!("pack: delta base {} missing", hex::encode(base_oid))))?;
                let delta = inflate_at(&self.packs[pack].file, ofs + p as u64, size as usize)?;
                (bt, Arc::new(apply_delta(&base, &delta)?))
            }
            t => return Err(ge(format!("pack: entry type {t} unsupported"))),
        };
        // Cache as a potential delta base, bytes-bounded FIFO.
        self.cache_bytes += resolved.1.len();
        self.cache.insert((pack, ofs), resolved.clone());
        self.cache_order.push_back((pack, ofs));
        while self.cache_bytes > BASE_CACHE_BUDGET {
            let Some(k) = self.cache_order.pop_front() else { break };
            if let Some((_, v)) = self.cache.remove(&k) {
                self.cache_bytes -= v.len();
            }
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh_git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Ground truth: every reachable object (loose AND packed, with real
    /// ofs-delta chains from `repack -adf`) must read byte-identical to
    /// `git cat-file`.
    #[test]
    fn reads_every_object_byte_identical_to_git() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        sh_git(&repo, &["init", "-q", "-b", "main"]);
        sh_git(&repo, &["config", "commit.gpgsign", "false"]);
        // Successive edits of one file → real delta chains after repack.
        let mut text: Vec<String> = (0..400).map(|i| format!("line {i:04} aaaaaaaa\n")).collect();
        for c in 0..30 {
            text[c * 13 % 400] = format!("edit {c}\n");
            std::fs::write(repo.join("big.txt"), text.concat()).unwrap();
            std::fs::write(repo.join(format!("f{c}.txt")), format!("file {c}\n")).unwrap();
            sh_git(&repo, &["add", "-A"]);
            sh_git(&repo, &["commit", "-q", "-m", &format!("c{c}")]);
        }
        // Pack everything (forces deltas), then add LOOSE objects on top.
        sh_git(&repo, &["repack", "-adfq"]);
        std::fs::write(repo.join("loose.txt"), "loose\n").unwrap();
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-q", "-m", "loose"]);

        let oids: Vec<String> = sh_git(&repo, &["rev-list", "--objects", "--all"])
            .lines()
            .map(|l| l.split(' ').next().unwrap().to_string())
            .collect();
        assert!(oids.len() > 90, "fixture too small: {}", oids.len());

        let mut st = ObjectStore::open(&repo).unwrap();
        for oid in &oids {
            // Ground truth without trusting any comparator: SHA-1 of
            // `<type> <len>\0<body>` must reproduce the oid — only the
            // exact bytes hash right.
            let (t, ours) = st.get_typed(oid).unwrap();
            let typ = match t {
                1 => "commit",
                2 => "tree",
                3 => "blob",
                4 => "tag",
                _ => unreachable!(),
            };
            let rehash = crate::git_obj_oid(typ, &ours);
            assert_eq!(&rehash, oid, "byte mismatch for {typ} {oid}");
        }
    }
}
