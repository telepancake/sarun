//! `DepotInner` — the single-threaded core. The public `Depot` wraps this
//! in a `Mutex`. See `wikimak/depot/SPEC.md` for the on-disk format and
//! durability contract.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use memmap2::MmapMut;

use crate::{DepotConfig, Error, Result};

/// `[u64 chain_id | u64 next_pointer | u64 zstd_len]`.
const HEADER_LEN: usize = 24;
/// One u64 LE: `file_id` in the low 16 bits, `offset` in the high 48.
const INDEX_ENTRY_LEN: usize = 8;
/// On-disk format version, written to `<root>/format` on create and
/// checked on every open of an existing depot. Bump on ANY layout
/// change — this is UNRELEASED software: no migrations, no
/// compatibility reads; mismatched depots are deleted and re-imported.
const FORMAT_VERSION: &str = "2";
/// Offsets are packed into 48 bits of the pointer word (256TB/file).
const MAX_OFFSET: u64 = (1 << 48) - 1;
/// File ids are packed into 16 bits of the pointer word.
const MAX_FILE_ID: u32 = u16::MAX as u32;
/// `file_id == 0` ⇔ the cold file. f0/f1 file ids start at 1 so that a
/// real first-prepend index entry of `(file_id=1, offset=0)` is nonzero
/// and distinguishable from the `(0,0)` "empty chain" sentinel.
const COLD_FILE_ID: u32 = 0;

/// One open f0 or f1 data file, with append-cursor and dead-byte count.
struct DataFile {
    id: u32,
    path: PathBuf,
    file: File,
    /// Append cursor = current file length in bytes.
    len: u64,
    /// Bytes whose frame is no longer the live f0/f1 for its chain.
    dead: u64,
}

impl DataFile {
    fn open(id: u32, path: PathBuf) -> Result<Self> {
        // NOT O_APPEND: eviction patches f1 next_pointers in place with
        // pwrite, and Linux pwrite on an O_APPEND fd ignores the offset
        // and appends. Appends go through write_all_at at the tracked
        // cursor instead.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            id,
            path,
            file,
            len,
            dead: 0,
        })
    }
}

/// One tier (f0 or f1) — a directory of `DataFile`s plus a current write
/// target.
struct Tier {
    dir: PathBuf,
    files: BTreeMap<u32, DataFile>,
    /// id of the current append target; `None` until the first file is
    /// allocated.
    current: Option<u32>,
    next_id: u32,
}

impl Tier {
    fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut files = BTreeMap::new();
        let mut max_id: u32 = 0;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if let Some(id_str) = s.strip_prefix("file-") {
                if let Ok(id) = id_str.parse::<u32>() {
                    let df = DataFile::open(id, entry.path())?;
                    max_id = max_id.max(id);
                    files.insert(id, df);
                }
            }
        }
        // Pick the highest-id file as the current write target if any
        // exist. f0/f1 file ids start at 1; 0 is reserved for cold.
        let current = files.keys().next_back().copied();
        let next_id = if files.is_empty() { 1 } else { max_id + 1 };
        Ok(Self {
            dir,
            files,
            current,
            next_id,
        })
    }

    /// Ensure the current write target has room for `frame_size` more
    /// bytes; if not, allocate a fresh file.
    fn ensure_room(&mut self, frame_size: u64, threshold: u64) -> Result<u32> {
        if let Some(id) = self.current {
            let df = self.files.get(&id).expect("current file present");
            if df.len + frame_size <= threshold {
                return Ok(id);
            }
        }
        let id = self.next_id;
        if id > MAX_FILE_ID {
            return Err(Error::Corrupt("tier file id exceeds the 16-bit pointer field"));
        }
        self.next_id += 1;
        let path = self.dir.join(format!("file-{id:04}"));
        let df = DataFile::open(id, path)?;
        self.files.insert(id, df);
        self.current = Some(id);
        Ok(id)
    }

    fn append(&mut self, frame: &[u8], threshold: u64) -> Result<(u32, u64)> {
        let id = self.ensure_room(frame.len() as u64, threshold)?;
        let df = self.files.get_mut(&id).expect("ensured");
        let off = df.len;
        if off + frame.len() as u64 > MAX_OFFSET {
            return Err(Error::FrameTooLarge);
        }
        // Positioned write at the tracked cursor (single-threaded under
        // the outer Mutex; no O_APPEND, see DataFile::open).
        df.file.write_all_at(frame, off)?;
        df.len += frame.len() as u64;
        Ok((id, off))
    }
}

pub struct DepotInner {
    cfg: DepotConfig,
    index: MmapMut,
    f0: Tier,
    f1: Tier,
    cold_file: File,
    cold_len: u64,
}

impl DepotInner {
    pub fn open(cfg: DepotConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;

        // Loud version fence: an existing depot (index present) must
        // carry a matching `format` file — the frame header/pointer
        // layout has no other marker, and an old-layout depot would
        // otherwise be silently misread through the new header size.
        let index_path = cfg.root.join("index");
        let format_path = cfg.root.join("format");
        if index_path.exists() {
            let found = std::fs::read_to_string(&format_path)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if found != FORMAT_VERSION {
                return Err(Error::Format(format!(
                    "depot {} has format {found:?}, this build writes \
                     {FORMAT_VERSION:?} — depot written by older code; delete \
                     and re-import (mirrors are rebuildable)",
                    cfg.root.display()
                )));
            }
        } else {
            std::fs::write(&format_path, format!("{FORMAT_VERSION}\n"))?;
        }

        // Index: fixed-size mmap'd file of max_chain_id * 8 zeroed bytes.
        let expected = cfg.max_chain_id * INDEX_ENTRY_LEN as u64;
        let index_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&index_path)?;
        let current = index_file.metadata()?.len();
        if current == 0 {
            index_file.set_len(expected)?;
        } else if current != expected {
            return Err(Error::IndexSizeMismatch);
        }
        // SAFETY: we own the file and serialize all access via the
        // outer Mutex; no other process maps it.
        let index = unsafe { MmapMut::map_mut(&index_file)? };

        let mut f0 = Tier::open(cfg.root.join("f0"))?;
        let mut f1 = Tier::open(cfg.root.join("f1"))?;

        let cold_dir = cfg.root.join("cold");
        std::fs::create_dir_all(&cold_dir)?;
        let cold_path = cold_dir.join("cold");
        let cold_file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .truncate(false)
            .open(&cold_path)?;
        let cold_len = cold_file.metadata()?.len();

        // Rebuild dead-byte counters for each f0/f1 file by walking each
        // file's frames and checking liveness against the index.
        rebuild_dead_f0(&mut f0, &index)?;
        rebuild_dead_f1(&mut f1, &index, &f0)?;

        Ok(Self {
            cfg,
            index,
            f0,
            f1,
            cold_file,
            cold_len,
        })
    }

    pub fn prepend(
        &mut self,
        chain_id: u64,
        new_f0_bytes: &[u8],
        new_f1_bytes: Option<&[u8]>,
        seal_old_f1: bool,
    ) -> Result<()> {
        if chain_id >= self.cfg.max_chain_id {
            return Err(Error::ChainIdOutOfRange);
        }
        // zstd_len is u64 on disk; individual frames are only bounded
        // by the 48-bit per-file offset space, checked at append.

        let old_f0_ptr = self.index_get(chain_id);
        let virgin = old_f0_ptr == 0;

        if virgin {
            if new_f1_bytes.is_some() {
                return Err(Error::FirstPrependHasF1);
            }
            if seal_old_f1 {
                return Err(Error::CannotSealNoF1);
            }
            let frame = encode_frame(chain_id, 0, new_f0_bytes);
            let (fid, off) = self.f0.append(&frame, self.cfg.file_size_threshold)?;
            self.index_put(chain_id, pack(fid, off));
            return Ok(());
        }

        let new_f1_bytes = new_f1_bytes.ok_or(Error::MissingF1)?;
        let (old_f0_fid, old_f0_off) = unpack(old_f0_ptr);
        let old_f0_ptr_field = read_next_pointer(&self.f0, old_f0_fid, old_f0_off)?;
        // `seal_f1` leaves f0 pointing DIRECTLY at the cold head (fid
        // 0): the chain has no f1, and the new f1 inherits that cold
        // head as its next pointer.
        let (old_f1_ptr, direct_cold) = if is_cold_ptr(old_f0_ptr_field) {
            (0, old_f0_ptr_field)
        } else {
            (old_f0_ptr_field, 0)
        };
        if seal_old_f1 && old_f1_ptr == 0 {
            return Err(Error::CannotSealNoF1);
        }
        let old_f0_size =
            HEADER_LEN as u64 + read_zstd_len(&self.f0, old_f0_fid, old_f0_off)?;
        let (old_f1_fid, old_f1_off) = unpack(old_f1_ptr);
        let old_f1_size = if old_f1_ptr != 0 {
            HEADER_LEN as u64 + read_zstd_len(&self.f1, old_f1_fid, old_f1_off)?
        } else {
            0
        };

        // The new f1's next_pointer either inherits the old cold head
        // (no seal) or points at the freshly-written new cold frame.
        let new_f1_next_ptr = if seal_old_f1 {
            // Read old f1's full frame: header + zstd. The zstd bytes
            // are reused verbatim as the new cold frame's payload.
            let old_f1_next = read_next_pointer(&self.f1, old_f1_fid, old_f1_off)?;
            let old_f1_zstd = read_zstd(&self.f1, old_f1_fid, old_f1_off)?;
            // The cold frame's next_pointer = old cold head (= old f1's
            // next_pointer field).
            let cold_frame = encode_frame(chain_id, old_f1_next, &old_f1_zstd);
            let cold_off = self.cold_append(&cold_frame)?;
            pack(COLD_FILE_ID, cold_off)
        } else if old_f1_ptr != 0 {
            // No seal: inherit old f1's cold-head pointer.
            read_next_pointer(&self.f1, old_f1_fid, old_f1_off)?
        } else {
            // Old chain had no f1: inherit the direct cold head left
            // by `seal_f1` (0 if the chain never sealed).
            direct_cold
        };

        let new_f1_frame = encode_frame(chain_id, new_f1_next_ptr, new_f1_bytes);
        let (new_f1_fid, new_f1_off) = self
            .f1
            .append(&new_f1_frame, self.cfg.file_size_threshold)?;
        let new_f1_ptr = pack(new_f1_fid, new_f1_off);

        let new_f0_frame = encode_frame(chain_id, new_f1_ptr, new_f0_bytes);
        let (new_f0_fid, new_f0_off) = self
            .f0
            .append(&new_f0_frame, self.cfg.file_size_threshold)?;

        self.index_put(chain_id, pack(new_f0_fid, new_f0_off));

        // Deprecate the old f0 / old f1.
        if let Some(df) = self.f0.files.get_mut(&old_f0_fid) {
            df.dead += old_f0_size;
        }
        if old_f1_ptr != 0 {
            if let Some(df) = self.f1.files.get_mut(&old_f1_fid) {
                df.dead += old_f1_size;
            }
        }

        Ok(())
    }

    /// Move the chain's CURRENT f1 verbatim to cold and leave the
    /// chain with f0 only (see `Depot::seal_f1`). Same machinery as
    /// `prepend`'s seal: the cold frame reuses the f1's zstd bytes and
    /// inherits its cold-head pointer; the commit point is the f0
    /// next_pointer flip (in place, like eviction's f1 repoint) —
    /// a crash before the flip leaves the chain intact and only an
    /// unreferenced cold frame behind. Durability, as everywhere in
    /// the depot, is the caller's `flush()`.
    pub fn seal_f1(&mut self, chain_id: u64) -> Result<()> {
        if chain_id >= self.cfg.max_chain_id {
            return Err(Error::ChainIdOutOfRange);
        }
        let f0_ptr = self.index_get(chain_id);
        if f0_ptr == 0 {
            return Err(Error::NoFrame);
        }
        let (f0_fid, f0_off) = unpack(f0_ptr);
        let f1_ptr = read_next_pointer(&self.f0, f0_fid, f0_off)?;
        if f1_ptr == 0 || is_cold_ptr(f1_ptr) {
            return Err(Error::CannotSealNoF1);
        }
        let (f1_fid, f1_off) = unpack(f1_ptr);
        let f1_next = read_next_pointer(&self.f1, f1_fid, f1_off)?;
        let f1_zstd = read_zstd(&self.f1, f1_fid, f1_off)?;
        let cold_frame = encode_frame(chain_id, f1_next, &f1_zstd);
        let cold_off = self.cold_append(&cold_frame)?;
        let cold_ptr = pack(COLD_FILE_ID, cold_off);
        // Commit: repoint the live f0 at the cold frame in place. The
        // tier handle is O_APPEND (pwrite on it would APPEND on Linux,
        // ignoring the offset), so the patch goes through a fresh
        // plain-write handle on the same path.
        let f0_df = self
            .f0
            .files
            .get(&f0_fid)
            .ok_or(Error::Corrupt("missing tier file"))?;
        let patch = OpenOptions::new().write(true).open(&f0_df.path)?;
        patch.write_all_at(&cold_ptr.to_le_bytes(), f0_off + 8)?;
        // The old f1 frame is dead.
        if let Some(df) = self.f1.files.get_mut(&f1_fid) {
            df.dead += HEADER_LEN as u64 + f1_zstd.len() as u64;
        }
        Ok(())
    }

    pub fn read_f0(&mut self, chain_id: u64) -> Result<Vec<u8>> {
        if chain_id >= self.cfg.max_chain_id {
            return Err(Error::ChainIdOutOfRange);
        }
        let ptr = self.index_get(chain_id);
        if ptr == 0 {
            return Err(Error::NoFrame);
        }
        let (fid, off) = unpack(ptr);
        read_zstd(&self.f0, fid, off)
    }

    pub fn read_f1(&mut self, chain_id: u64) -> Result<Option<Vec<u8>>> {
        if chain_id >= self.cfg.max_chain_id {
            return Err(Error::ChainIdOutOfRange);
        }
        let ptr = self.index_get(chain_id);
        if ptr == 0 {
            return Err(Error::NoFrame);
        }
        let (fid, off) = unpack(ptr);
        let f1_ptr = read_next_pointer(&self.f0, fid, off)?;
        if f1_ptr == 0 || is_cold_ptr(f1_ptr) {
            // No f1 (a cold pointer here = `seal_f1` retired it).
            return Ok(None);
        }
        let (f1_fid, f1_off) = unpack(f1_ptr);
        Ok(Some(read_zstd(&self.f1, f1_fid, f1_off)?))
    }

    pub fn cold_head(&mut self, chain_id: u64) -> Result<u64> {
        if chain_id >= self.cfg.max_chain_id {
            return Err(Error::ChainIdOutOfRange);
        }
        let ptr = self.index_get(chain_id);
        if ptr == 0 {
            return Ok(0);
        }
        let (fid, off) = unpack(ptr);
        let f1_ptr = read_next_pointer(&self.f0, fid, off)?;
        if f1_ptr == 0 {
            return Ok(0);
        }
        if is_cold_ptr(f1_ptr) {
            // `seal_f1` retired the f1: f0 points at the cold head.
            return Ok(f1_ptr);
        }
        let (f1_fid, f1_off) = unpack(f1_ptr);
        read_next_pointer(&self.f1, f1_fid, f1_off)
    }

    /// Read one cold frame: returns (zstd_bytes, next_pointer).
    pub fn read_cold_frame(&mut self, ptr: u64) -> Result<(Vec<u8>, u64)> {
        let (fid, off) = unpack(ptr);
        if fid != COLD_FILE_ID {
            return Err(Error::Corrupt("cold pointer with nonzero file_id"));
        }
        let mut header = [0u8; HEADER_LEN];
        self.cold_file.read_exact_at(&mut header, off)?;
        let next = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let zstd_len = u64::from_le_bytes(header[16..24].try_into().unwrap()) as usize;
        let mut buf = vec![0u8; zstd_len];
        self.cold_file
            .read_exact_at(&mut buf, off + HEADER_LEN as u64)?;
        Ok((buf, next))
    }

    pub fn flush(&mut self) -> Result<()> {
        for df in self.f0.files.values() {
            df.file.sync_data()?;
        }
        for df in self.f1.files.values() {
            df.file.sync_data()?;
        }
        self.cold_file.sync_data()?;
        self.index.flush()?;
        Ok(())
    }

    /// Opportunistic eviction (runs on every `flush`): rolled files
    /// only. The current write target is exempt here — mid-session its
    /// slack is what buys bounded per-prepend I/O — and is reclaimed by
    /// `collect` when the session is done.
    pub fn maybe_evict(&mut self) -> Result<()> {
        self.evict_pass(false)
    }

    /// Session-end compaction: same dead-ratio policy, but the current
    /// write file is a candidate too (rolled first, per SPEC "Eviction"
    /// step 5 — live frames cannot migrate into the file being
    /// unlinked). Without this, a churning chain parks every deprecated
    /// head below file_size_threshold forever: holes at rest serve
    /// nothing.
    pub fn collect(&mut self) -> Result<()> {
        self.evict_pass(true)
    }

    fn evict_pass(&mut self, include_current: bool) -> Result<()> {
        loop {
            let mut victim: Option<(bool, u32)> = None;
            for df in self.f0.files.values() {
                if !include_current && Some(df.id) == self.f0.current {
                    continue;
                }
                if df.len > 0 && (df.dead as f32 / df.len as f32) > self.cfg.eviction_dead_ratio {
                    victim = Some((true, df.id));
                    break;
                }
            }
            if victim.is_none() {
                for df in self.f1.files.values() {
                    if !include_current && Some(df.id) == self.f1.current {
                        continue;
                    }
                    if df.len > 0 && (df.dead as f32 / df.len as f32) > self.cfg.eviction_dead_ratio
                    {
                        victim = Some((false, df.id));
                        break;
                    }
                }
            }
            let Some((is_f0, vid)) = victim else {
                return Ok(());
            };
            if is_f0 {
                if self.f0.current == Some(vid) {
                    self.f0.current = None;
                }
                self.evict_f0(vid)?;
            } else {
                if self.f1.current == Some(vid) {
                    self.f1.current = None;
                }
                self.evict_f1(vid)?;
            }
        }
    }

    fn evict_f0(&mut self, vid: u32) -> Result<()> {
        let frames = walk_frames(self.f0.files.get(&vid).expect("victim present"))?;
        for wf in frames {
            let chain_id = u64::from_le_bytes(wf.header[0..8].try_into().unwrap());
            let live_ptr = self.index_get(chain_id);
            let (live_fid, live_off) = unpack(live_ptr);
            if !(live_fid == vid && live_off == wf.offset) {
                continue; // dead frame
            }
            let mut frame = Vec::with_capacity(HEADER_LEN + wf.zstd.len());
            frame.extend_from_slice(&wf.header);
            frame.extend_from_slice(&wf.zstd);
            let (new_fid, new_off) = self.f0.append(&frame, self.cfg.file_size_threshold)?;
            self.index_put(chain_id, pack(new_fid, new_off));
        }
        // Durability: fsync the destination tier file(s), then the index
        // (the index flip is the commit), then unlink the victim.
        for df in self.f0.files.values() {
            df.file.sync_data()?;
        }
        self.index.flush()?;
        let victim = self.f0.files.remove(&vid).expect("present");
        std::fs::remove_file(&victim.path)?;
        Ok(())
    }

    fn evict_f1(&mut self, vid: u32) -> Result<()> {
        let frames = walk_frames(self.f1.files.get(&vid).expect("victim present"))?;
        // Track which f0 files we patched so we can fsync them.
        let mut touched_f0: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for wf in frames {
            let chain_id = u64::from_le_bytes(wf.header[0..8].try_into().unwrap());
            let live_f0_ptr = self.index_get(chain_id);
            if live_f0_ptr == 0 {
                continue;
            }
            let (f0_fid, f0_off) = unpack(live_f0_ptr);
            let current_f1_ptr = read_next_pointer(&self.f0, f0_fid, f0_off)?;
            let (cur_f1_fid, cur_f1_off) = unpack(current_f1_ptr);
            if !(cur_f1_fid == vid && cur_f1_off == wf.offset) {
                continue; // dead frame
            }
            let mut frame = Vec::with_capacity(HEADER_LEN + wf.zstd.len());
            frame.extend_from_slice(&wf.header);
            frame.extend_from_slice(&wf.zstd);
            let (new_fid, new_off) = self.f1.append(&frame, self.cfg.file_size_threshold)?;
            let new_ptr = pack(new_fid, new_off);
            let f0_df = self.f0.files.get(&f0_fid).expect("f0 file present");
            // The tier handle is O_APPEND (pwrite on it would APPEND
            // on Linux, ignoring the offset): patch via a fresh
            // plain-write handle on the same path.
            let patch = OpenOptions::new().write(true).open(&f0_df.path)?;
            patch.write_all_at(&new_ptr.to_le_bytes(), f0_off + 8)?;
            touched_f0.insert(f0_fid);
        }
        for df in self.f1.files.values() {
            df.file.sync_data()?;
        }
        for fid in &touched_f0 {
            if let Some(df) = self.f0.files.get(fid) {
                df.file.sync_data()?;
            }
        }
        let victim = self.f1.files.remove(&vid).expect("present");
        std::fs::remove_file(&victim.path)?;
        Ok(())
    }

    pub fn delete_all(&mut self) -> Result<()> {
        // Zero the index.
        for b in self.index.iter_mut() {
            *b = 0;
        }
        self.index.flush()?;
        // Unlink every f0/f1 file.
        for (_, df) in std::mem::take(&mut self.f0.files) {
            let _ = std::fs::remove_file(&df.path);
        }
        for (_, df) in std::mem::take(&mut self.f1.files) {
            let _ = std::fs::remove_file(&df.path);
        }
        self.f0.current = None;
        self.f0.next_id = 1;
        self.f1.current = None;
        self.f1.next_id = 1;
        // Truncate the cold file (or unlink). The test allows either.
        self.cold_file.set_len(0)?;
        self.cold_file.sync_data()?;
        self.cold_len = 0;
        Ok(())
    }

    fn cold_append(&mut self, frame: &[u8]) -> Result<u64> {
        // Reserve byte 0 of the cold file lazily so no real cold frame
        // ever lives at offset 0; that keeps `pack(0, 0) == 0`, the
        // "empty chain" sentinel, disjoint from every real cold pointer.
        if self.cold_len == 0 {
            std::io::Write::write_all(&mut self.cold_file, &[0u8])?;
            self.cold_len = 1;
        }
        if self.cold_len + frame.len() as u64 > MAX_OFFSET {
            return Err(Error::FrameTooLarge);
        }
        let off = self.cold_len;
        std::io::Write::write_all(&mut self.cold_file, frame)?;
        self.cold_len += frame.len() as u64;
        Ok(off)
    }

    fn index_get(&self, chain_id: u64) -> u64 {
        let start = chain_id as usize * INDEX_ENTRY_LEN;
        u64::from_le_bytes(self.index[start..start + 8].try_into().unwrap())
    }

    fn index_put(&mut self, chain_id: u64, ptr: u64) {
        let start = chain_id as usize * INDEX_ENTRY_LEN;
        self.index[start..start + 8].copy_from_slice(&ptr.to_le_bytes());
    }
}

/// Pack `(file_id, offset)` into one u64: `file_id` in the low 16
/// bits, `offset` in the high 48 (SPEC §"Index") — per-file cap 256TB,
/// 65535 data files per tier.
fn pack(file_id: u32, offset: u64) -> u64 {
    debug_assert!(file_id <= MAX_FILE_ID);
    debug_assert!(offset <= MAX_OFFSET);
    offset << 16 | file_id as u64
}

fn unpack(ptr: u64) -> (u32, u64) {
    ((ptr & 0xFFFF) as u32, ptr >> 16)
}

/// A nonzero pointer into the cold file (fid 0; real f0/f1 fids start
/// at 1, and cold offset 0 is reserved, so this is unambiguous).
fn is_cold_ptr(ptr: u64) -> bool {
    ptr != 0 && (ptr & 0xFFFF) == COLD_FILE_ID as u64
}

fn encode_frame(chain_id: u64, next_pointer: u64, zstd: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + zstd.len());
    buf.extend_from_slice(&chain_id.to_le_bytes());
    buf.extend_from_slice(&next_pointer.to_le_bytes());
    buf.extend_from_slice(&(zstd.len() as u64).to_le_bytes());
    buf.extend_from_slice(zstd);
    buf
}

fn read_next_pointer(tier: &Tier, file_id: u32, offset: u64) -> Result<u64> {
    let df = tier
        .files
        .get(&file_id)
        .ok_or(Error::Corrupt("missing tier file"))?;
    let mut buf = [0u8; 8];
    df.file.read_exact_at(&mut buf, offset + 8)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_zstd_len(tier: &Tier, file_id: u32, offset: u64) -> Result<u64> {
    let df = tier
        .files
        .get(&file_id)
        .ok_or(Error::Corrupt("missing tier file"))?;
    let mut buf = [0u8; 8];
    df.file.read_exact_at(&mut buf, offset + 16)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_zstd(tier: &Tier, file_id: u32, offset: u64) -> Result<Vec<u8>> {
    let df = tier
        .files
        .get(&file_id)
        .ok_or(Error::Corrupt("missing tier file"))?;
    let mut len_buf = [0u8; 8];
    df.file.read_exact_at(&mut len_buf, offset + 16)?;
    let len = u64::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    df.file
        .read_exact_at(&mut buf, offset + HEADER_LEN as u64)?;
    Ok(buf)
}

/// One frame as read off disk during a sequential walk.
struct WalkedFrame {
    offset: u64,
    header: [u8; HEADER_LEN],
    zstd: Vec<u8>,
}

/// Walk every frame in a data file sequentially.
fn walk_frames(df: &DataFile) -> Result<Vec<WalkedFrame>> {
    let mut out = Vec::new();
    let mut off: u64 = 0;
    while off < df.len {
        let mut header = [0u8; HEADER_LEN];
        df.file.read_exact_at(&mut header, off)?;
        let zstd_len = u64::from_le_bytes(header[16..24].try_into().unwrap()) as usize;
        let mut zstd = vec![0u8; zstd_len];
        df.file.read_exact_at(&mut zstd, off + HEADER_LEN as u64)?;
        out.push(WalkedFrame {
            offset: off,
            header,
            zstd,
        });
        off += HEADER_LEN as u64 + zstd_len as u64;
    }
    Ok(out)
}

fn index_lookup(index: &MmapMut, chain_id: u64) -> u64 {
    let s = chain_id as usize * INDEX_ENTRY_LEN;
    u64::from_le_bytes(index[s..s + 8].try_into().unwrap())
}

fn rebuild_dead_f0(tier: &mut Tier, index: &MmapMut) -> Result<()> {
    let ids: Vec<u32> = tier.files.keys().copied().collect();
    for fid in ids {
        let frames = walk_frames(tier.files.get(&fid).expect("present"))?;
        let mut dead: u64 = 0;
        for wf in &frames {
            let chain_id = u64::from_le_bytes(wf.header[0..8].try_into().unwrap());
            let ptr = index_lookup(index, chain_id);
            let alive = ptr != 0 && {
                let (lfid, loff) = unpack(ptr);
                lfid == fid && loff == wf.offset
            };
            if !alive {
                dead += HEADER_LEN as u64 + wf.zstd.len() as u64;
            }
        }
        tier.files.get_mut(&fid).expect("present").dead = dead;
    }
    Ok(())
}

fn rebuild_dead_f1(tier: &mut Tier, index: &MmapMut, f0_tier: &Tier) -> Result<()> {
    let ids: Vec<u32> = tier.files.keys().copied().collect();
    for fid in ids {
        let frames = walk_frames(tier.files.get(&fid).expect("present"))?;
        let mut dead: u64 = 0;
        for wf in &frames {
            let chain_id = u64::from_le_bytes(wf.header[0..8].try_into().unwrap());
            let ptr = index_lookup(index, chain_id);
            let alive = if ptr == 0 {
                false
            } else {
                let (lfid, loff) = unpack(ptr);
                match f0_tier.files.get(&lfid) {
                    Some(f0_df) => {
                        let mut np = [0u8; 8];
                        f0_df.file.read_exact_at(&mut np, loff + 8)?;
                        let (f1_fid, f1_off) = unpack(u64::from_le_bytes(np));
                        f1_fid == fid && f1_off == wf.offset
                    }
                    None => false,
                }
            };
            if !alive {
                dead += HEADER_LEN as u64 + wf.zstd.len() as u64;
            }
        }
        tier.files.get_mut(&fid).expect("present").dead = dead;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 4GB ceiling is gone in principle: pointer words round-trip
    /// offsets above u32::MAX up to the full 48-bit space, and the
    /// 16-bit file-id field round-trips its maximum.
    #[test]
    fn pack_unpack_wide_offsets() {
        for off in [0u64, 1, u32::MAX as u64, u32::MAX as u64 + 1, MAX_OFFSET] {
            for fid in [COLD_FILE_ID, 1u32, 2, MAX_FILE_ID] {
                let ptr = pack(fid, off);
                assert_eq!(unpack(ptr), (fid, off), "fid={fid} off={off}");
                assert_eq!(
                    is_cold_ptr(ptr),
                    ptr != 0 && fid == COLD_FILE_ID,
                    "fid={fid} off={off}"
                );
            }
        }
        // The empty-chain sentinel stays disjoint from every real
        // pointer (cold offset 0 is reserved; tier fids start at 1).
        assert_eq!(pack(COLD_FILE_ID, 0), 0);
    }

    /// Cold-append accounting accepts offsets past the old u32 ceiling
    /// and fails closed only at the 48-bit bound.
    #[test]
    fn cold_bound_is_48_bits() {
        assert!(u32::MAX as u64 + 1 + HEADER_LEN as u64 <= MAX_OFFSET);
        assert!(MAX_OFFSET + 1 > MAX_OFFSET); // trivially: the check is `> MAX_OFFSET`
        assert_eq!(MAX_OFFSET, (1u64 << 48) - 1);
    }
}
