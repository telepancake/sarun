// Reader for tv's inramfs shared-memory store (tv/sud/inramfs/internal.h,
// magic "INRF" version 1) — the engine side of the cooperative
// shared-memory transfer (engine/DESIGN-sud.md, WIP). A sud box's /tmp is
// an inramfs mount: every process in the box serves /tmp syscalls from
// the shared region (64-bit maps it; 32-bit does pread/pwrite on the shm
// fd — it cannot map the 8 GiB data space); at sweep time the engine
// parses the region FROM OUTSIDE and ingests the tree.
//
// The region is offset-addressed ("we use offsets for portability"), so
// no fixed-address mapping is needed to read it. Layout constants below
// are pinned against a C offsetof probe of internal.h:
//   super 88 B (inode_count@24, inode_table_off@32, small_shm_size@80)
//   inode 80 B (size@24, union@64), dirent 72 B, dirblock ents@64
// Backing objects: /dev/shm/sud-inramfs.<key>.meta, .smalldata, and
// per-file LARGE shms at .f.<file_idx>.<file_gen>.

use std::io::Read;
use std::io::Seek;
use std::path::Path;
use std::path::PathBuf;

const MAGIC: u32 = 0x494E5246; // "INRF"
const VERSION: u32 = 1;
const BLOCK_SIZE: u64 = 4096;
const INODE_SIZE: usize = 80;
const DIRENT_SIZE: usize = 72;
const DIRBLOCK_ENTS_OFF: usize = 64;
const DIRENTS_PER_BLOCK: usize = 62;

const T_REG: u32 = 1;
const T_DIR: u32 = 2;
const T_LNK: u32 = 3;
const REG_SMALL: u32 = 0;
const REG_LARGE: u32 = 1;

pub enum IrKind {
    File { mode: u32, data: Vec<u8> },
    Dir { mode: u32 },
    Symlink { target: PathBuf },
}

pub struct IrEntry {
    pub rel: String, // relative to the mount root, no leading slash
    pub kind: IrKind,
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}

/// Read the whole store keyed `key` under `shm_dir` (normally /dev/shm).
/// Returns the entries depth-first (parents before children). A missing
/// meta object yields Ok(empty) — the box may simply never have booted.
pub fn read_store(shm_dir: &Path, key: &str) -> Result<Vec<IrEntry>, String> {
    let meta_path = shm_dir.join(format!("sud-inramfs.{key}.meta"));
    let meta = match std::fs::read(&meta_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vec![]);
        }
        Err(e) => return Err(format!("{}: {e}", meta_path.display())),
    };
    if u32_at(&meta, 0) != MAGIC {
        return Err(format!("{}: bad magic", meta_path.display()));
    }
    if u32_at(&meta, 4) != VERSION {
        return Err(format!("{}: unsupported version {}",
                           meta_path.display(), u32_at(&meta, 4)));
    }
    let inode_count = u32_at(&meta, 24) as usize;
    let table_off = u32_at(&meta, 32) as usize;
    let small_path = shm_dir.join(format!("sud-inramfs.{key}.smalldata"));
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    // Root dir is inode slot 1; its own mode is the mount root's and is
    // not emitted (the mount point exists in the box regardless).
    walk_dir(&meta, inode_count, table_off, shm_dir, key, &small_path,
             1, "", &mut out, &mut seen, 0)?;
    Ok(out)
}

fn inode_field(meta: &[u8], table_off: usize, idx: usize, off: usize) -> u32 {
    u32_at(meta, table_off + idx * INODE_SIZE + off)
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(meta: &[u8], inode_count: usize, table_off: usize,
            shm_dir: &Path, key: &str, small_path: &Path,
            dir_idx: usize, rel: &str,
            out: &mut Vec<IrEntry>,
            seen: &mut std::collections::HashSet<usize>,
            depth: u32) -> Result<(), String> {
    if depth > 64 || !seen.insert(dir_idx) {
        return Ok(()); // cycle/depth guard — corrupt store, stop quietly
    }
    let mut block_off = inode_field(meta, table_off, dir_idx, 64) as usize;
    let mut guard = 0;
    while block_off != 0 {
        guard += 1;
        if guard > 1 << 20 { break; }
        let used = u32_at(meta, block_off + 4) as usize;
        for i in 0..used.min(DIRENTS_PER_BLOCK) {
            let e = block_off + DIRBLOCK_ENTS_OFF + i * DIRENT_SIZE;
            let ino = u32_at(meta, e) as usize;
            let name_len = *meta.get(e + 5).unwrap_or(&0) as usize;
            if ino == 0 || ino >= inode_count || name_len == 0 {
                continue;
            }
            let name = String::from_utf8_lossy(
                &meta[e + 8..e + 8 + name_len.min(63)]).into_owned();
            if name == "." || name == ".." || name.contains('/') {
                continue;
            }
            let crel = if rel.is_empty() { name }
                       else { format!("{rel}/{name}") };
            let ty = inode_field(meta, table_off, ino, 0);
            let mode = inode_field(meta, table_off, ino, 8) & 0o7777;
            match ty {
                T_DIR => {
                    out.push(IrEntry { rel: crel.clone(),
                                       kind: IrKind::Dir { mode } });
                    walk_dir(meta, inode_count, table_off, shm_dir, key,
                             small_path, ino, &crel, out, seen,
                             depth + 1)?;
                }
                T_REG => {
                    let size = u64_at(meta,
                        table_off + ino * INODE_SIZE + 24);
                    let data = read_file(meta, table_off, shm_dir, key,
                                         small_path, ino, size)
                        .map_err(|e| format!("{crel}: {e}"))?;
                    out.push(IrEntry { rel: crel,
                                       kind: IrKind::File { mode, data } });
                }
                T_LNK => {
                    let toff = inode_field(meta, table_off, ino, 64)
                        as usize;
                    let tlen = inode_field(meta, table_off, ino, 68)
                        as usize;
                    if toff == 0 || tlen > 4096 { continue; }
                    let target = String::from_utf8_lossy(
                        &meta[toff..toff + tlen]).into_owned();
                    out.push(IrEntry { rel: crel, kind: IrKind::Symlink {
                        target: PathBuf::from(target) } });
                }
                _ => {}
            }
        }
        block_off = u32_at(meta, block_off) as usize;
    }
    Ok(())
}

fn read_file(meta: &[u8], table_off: usize, shm_dir: &Path, key: &str,
             small_path: &Path, ino: usize, size: u64)
             -> Result<Vec<u8>, String> {
    if size == 0 { return Ok(vec![]); }
    let tag = inode_field(meta, table_off, ino, 64);
    if tag == REG_SMALL {
        let start_block = inode_field(meta, table_off, ino, 68) as u64;
        if start_block == 0 { return Ok(vec![]); }
        let mut f = std::fs::File::open(small_path)
            .map_err(|e| format!("smalldata: {e}"))?;
        f.seek(std::io::SeekFrom::Start((start_block - 1) * BLOCK_SIZE))
            .map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; size as usize];
        f.read_exact(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    } else if tag == REG_LARGE {
        let idx = inode_field(meta, table_off, ino, 68);
        let generation = inode_field(meta, table_off, ino, 72);
        let p = shm_dir.join(
            format!("sud-inramfs.{key}.f.{idx}.{generation}"));
        let mut buf = std::fs::read(&p)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        buf.truncate(size as usize); // shm is block-granular + sparse
        if buf.len() < size as usize {
            buf.resize(size as usize, 0); // sparse tail reads as zeros
        }
        Ok(buf)
    } else {
        Err(format!("inode {ino}: unknown storage tag {tag}"))
    }
}

/// Remove every backing object of store `key` (meta, smalldata, per-file
/// LARGE shms). Called after the sweep ingested the tree.
pub fn unlink_store(shm_dir: &Path, key: &str) {
    let prefix = format!("sud-inramfs.{key}.");
    if let Ok(rd) = std::fs::read_dir(shm_dir) {
        for ent in rd.flatten() {
            if ent.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(ent.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn w64(b: &mut [u8], off: usize, v: u64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Build a minimal synthetic store: root dir containing one small
    /// file, one large file, one symlink, and a subdir with a file.
    #[test]
    fn parses_synthetic_store() {
        let dir = std::env::temp_dir().join(format!(
            "sudir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = "t1";
        let table_off = 0x100usize;
        let blk1 = 0x2000usize; // root dirblock
        let blk2 = 0x4000usize; // subdir dirblock
        let lnkblk = 0x6000usize; // symlink target block
        let mut meta = vec![0u8; 0x8000];
        w32(&mut meta, 0, MAGIC);
        w32(&mut meta, 4, VERSION);
        w32(&mut meta, 24, 16);              // inode_count
        w32(&mut meta, 32, table_off as u32); // inode_table_off
        let ino = |i: usize| table_off + i * INODE_SIZE;
        // root dir (slot 1) → blk1, 4 entries
        w32(&mut meta, ino(1), T_DIR);
        w32(&mut meta, ino(1) + 64, blk1 as u32);
        // small file (slot 2): 5 bytes at block 1
        w32(&mut meta, ino(2), T_REG);
        w32(&mut meta, ino(2) + 8, 0o644);
        w64(&mut meta, ino(2) + 24, 5);
        w32(&mut meta, ino(2) + 64, REG_SMALL);
        w32(&mut meta, ino(2) + 68, 1); // start_block (1-based)
        // large file (slot 3): 10 bytes in per-file shm idx 3 gen 7
        w32(&mut meta, ino(3), T_REG);
        w32(&mut meta, ino(3) + 8, 0o755);
        w64(&mut meta, ino(3) + 24, 10);
        w32(&mut meta, ino(3) + 64, REG_LARGE);
        w32(&mut meta, ino(3) + 68, 3);
        w32(&mut meta, ino(3) + 72, 7);
        // symlink (slot 4) → "small.txt"
        w32(&mut meta, ino(4), T_LNK);
        w32(&mut meta, ino(4) + 64, lnkblk as u32);
        w32(&mut meta, ino(4) + 68, 9);
        meta[lnkblk..lnkblk + 9].copy_from_slice(b"small.txt");
        // subdir (slot 5) → blk2 with one file (slot 6)
        w32(&mut meta, ino(5), T_DIR);
        w32(&mut meta, ino(5) + 8, 0o700);
        w32(&mut meta, ino(5) + 64, blk2 as u32);
        w32(&mut meta, ino(6), T_REG);
        w32(&mut meta, ino(6) + 8, 0o600);
        w64(&mut meta, ino(6) + 24, 3);
        w32(&mut meta, ino(6) + 64, REG_SMALL);
        w32(&mut meta, ino(6) + 68, 2); // block 2
        // root dirblock: 4 used entries
        w32(&mut meta, blk1 + 4, 4);
        let dent = |b: &mut Vec<u8>, blk: usize, i: usize, ino_i: u32,
                    name: &str| {
            let e = blk + DIRBLOCK_ENTS_OFF + i * DIRENT_SIZE;
            w32(b, e, ino_i);
            b[e + 5] = name.len() as u8;
            b[e + 8..e + 8 + name.len()].copy_from_slice(name.as_bytes());
        };
        dent(&mut meta, blk1, 0, 2, "small.txt");
        dent(&mut meta, blk1, 1, 3, "large.bin");
        dent(&mut meta, blk1, 2, 4, "link");
        dent(&mut meta, blk1, 3, 5, "sub");
        w32(&mut meta, blk2 + 4, 1);
        dent(&mut meta, blk2, 0, 6, "inner.txt");
        std::fs::write(dir.join(format!("sud-inramfs.{key}.meta")), &meta)
            .unwrap();
        // smalldata: block 1 = "hello", block 2 = "abc"
        let mut small = vec![0u8; 3 * BLOCK_SIZE as usize];
        small[0..5].copy_from_slice(b"hello");
        small[BLOCK_SIZE as usize..BLOCK_SIZE as usize + 3]
            .copy_from_slice(b"abc");
        std::fs::write(dir.join(format!("sud-inramfs.{key}.smalldata")),
                       &small).unwrap();
        std::fs::write(dir.join(format!("sud-inramfs.{key}.f.3.7")),
                       b"largebytes-and-slack").unwrap();

        let mut got = read_store(&dir, key).unwrap();
        got.sort_by(|a, b| a.rel.cmp(&b.rel));
        let rels: Vec<&str> = got.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, ["large.bin", "link", "small.txt", "sub",
                          "sub/inner.txt"]);
        for e in &got {
            match (e.rel.as_str(), &e.kind) {
                ("small.txt", IrKind::File { mode, data }) => {
                    assert_eq!(*mode, 0o644);
                    assert_eq!(data, b"hello");
                }
                ("large.bin", IrKind::File { mode, data }) => {
                    assert_eq!(*mode, 0o755);
                    assert_eq!(data, b"largebytes"); // truncated to size
                }
                ("link", IrKind::Symlink { target }) => {
                    assert_eq!(target.to_str().unwrap(), "small.txt");
                }
                ("sub", IrKind::Dir { mode }) => assert_eq!(*mode, 0o700),
                ("sub/inner.txt", IrKind::File { data, .. }) => {
                    assert_eq!(data, b"abc");
                }
                other => panic!("unexpected entry {:?}", other.0),
            }
        }
        unlink_store(&dir, key);
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Missing store is not an error (box may never have booted).
    #[test]
    fn missing_store_is_empty() {
        let dir = std::env::temp_dir();
        assert!(read_store(&dir, "no-such-key-xyzzy").unwrap().is_empty());
    }
}
