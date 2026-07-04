//! Single-shard state machine.
//!
//! Each [`Shard`] owns one file on disk. Operations are serialized by the
//! [`Mutex`] held in the [`crate::Pool`].

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::StrpoolError;
use crate::footer::{parse_footer, write_footer_bytes, Footer, FOOTER_SIZE};
use crate::{DictProvider, Result};

/// In-memory state for an open shard.
pub struct Shard {
    #[allow(dead_code)]
    pub shard_id: u32,
    pub path: PathBuf,
    pub tmp_path: PathBuf,
    /// `O_RDWR` handle. `None` until the first append/seal materializes the
    /// file on disk.
    pub file: Option<File>,
    /// Length of the file in bytes (cached; kept in sync with all writes).
    pub file_size: u64,
    /// Plaintext-tail length encoded in the footer.
    pub tail_len: u32,
    /// Total entries (sum across frames + tail) encoded in the footer.
    pub entry_count: u32,
    /// Dict id to use on the next [`Self::seal`]. `None` = uncompressed seal.
    pub next_dict_id: Option<u32>,
    pub dict_provider: Option<Arc<dyn DictProvider>>,
}

impl Shard {
    /// Open the shard file at `path`. See the crate-level docs for the
    /// crash-safety contract.
    pub fn open(
        shard_id: u32,
        path: PathBuf,
        dict_provider: Option<Arc<dyn DictProvider>>,
    ) -> Result<Self> {
        let tmp_path = sidecar_tmp_path(&path);
        // Always delete a leftover `<shard>.tmp` from a crashed seal.
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
        }

        if !path.exists() {
            return Ok(Self {
                shard_id,
                path,
                tmp_path,
                file: None,
                file_size: 0,
                tail_len: 0,
                entry_count: 0,
                next_dict_id: None,
                dict_provider,
            });
        }
        let metadata = std::fs::metadata(&path)?;
        let file_size = metadata.len();
        if file_size == 0 {
            return Ok(Self {
                shard_id,
                path,
                tmp_path,
                file: None,
                file_size: 0,
                tail_len: 0,
                entry_count: 0,
                next_dict_id: None,
                dict_provider,
            });
        }
        if file_size < FOOTER_SIZE as u64 {
            return Err(StrpoolError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "shard file too small: {} bytes (need at least {})",
                    file_size, FOOTER_SIZE
                ),
            )));
        }

        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let mut buf = [0u8; FOOTER_SIZE];
        file.read_exact_at(&mut buf, file_size - FOOTER_SIZE as u64)?;
        let footer = parse_footer(&buf).expect("FOOTER_SIZE bytes parse");
        // Sanity-check: tail_start must be in [0, file_size - FOOTER_SIZE].
        let tail_end = file_size - FOOTER_SIZE as u64;
        if (footer.tail_len as u64) > tail_end {
            return Err(StrpoolError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tail_len exceeds file size",
            )));
        }
        Ok(Self {
            shard_id,
            path,
            tmp_path,
            file: Some(file),
            file_size,
            tail_len: footer.tail_len,
            entry_count: footer.entry_count,
            next_dict_id: None,
            dict_provider,
        })
    }

    /// Append one byte string. Returns the dense local id (pre-append
    /// `entry_count`).
    pub fn append(&mut self, s: &[u8]) -> Result<u32> {
        let local_id = self.entry_count;
        let n = s.len() + 1;
        let new_tail_len = checked_add_tail(self.tail_len, n)?;
        let new_entry_count = self.entry_count.checked_add(1).ok_or_else(|| {
            StrpoolError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "entry_count would overflow u32",
            ))
        })?;
        let new_footer = Footer {
            tail_len: new_tail_len,
            entry_count: new_entry_count,
        };
        let footer_bytes = write_footer_bytes(new_footer);

        let mut payload = Vec::with_capacity(n + FOOTER_SIZE);
        payload.extend_from_slice(s);
        payload.push(0);
        payload.extend_from_slice(&footer_bytes);

        let write_offset = self.write_offset();
        self.ensure_file_open()?;
        self.file
            .as_ref()
            .expect("ensured open")
            .write_all_at(&payload, write_offset)?;

        self.tail_len = new_tail_len;
        self.entry_count = new_entry_count;
        self.file_size = write_offset + payload.len() as u64;
        Ok(local_id)
    }

    /// Append many strings, assigning sequential local ids. Issues one pwrite.
    pub fn append_many(&mut self, strings: &[&[u8]]) -> Result<Vec<u32>> {
        if strings.is_empty() {
            return Ok(Vec::new());
        }
        let mut new_tail_len = self.tail_len;
        for s in strings {
            new_tail_len = checked_add_tail(new_tail_len, s.len() + 1)?;
        }
        let total_n = (new_tail_len - self.tail_len) as usize;
        let new_entry_count: u32 = (self.entry_count as u64 + strings.len() as u64)
            .try_into()
            .map_err(|_| {
                StrpoolError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "entry_count would overflow u32",
                ))
            })?;
        let footer = Footer {
            tail_len: new_tail_len,
            entry_count: new_entry_count,
        };
        let footer_bytes = write_footer_bytes(footer);

        let mut payload = Vec::with_capacity(total_n + FOOTER_SIZE);
        let start_id = self.entry_count;
        let mut ids = Vec::with_capacity(strings.len());
        for (i, s) in strings.iter().enumerate() {
            payload.extend_from_slice(s);
            payload.push(0);
            ids.push(start_id + i as u32);
        }
        payload.extend_from_slice(&footer_bytes);

        let write_offset = self.write_offset();
        self.ensure_file_open()?;
        self.file
            .as_ref()
            .expect("ensured open")
            .write_all_at(&payload, write_offset)?;

        self.tail_len = new_tail_len;
        self.entry_count = new_entry_count;
        self.file_size = write_offset + payload.len() as u64;
        Ok(ids)
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Some(f) = &self.file {
            f.sync_all()?;
        }
        Ok(())
    }

    pub fn set_next_dict(&mut self, dict_id: u32) {
        self.next_dict_id = Some(dict_id);
    }

    /// If the tail exceeds the threshold, compress it into a new frame and
    /// rename `<shard>.tmp` over `<shard>`. Returns `true` if a seal happened.
    pub fn maybe_seal(&mut self, threshold: u64) -> Result<bool> {
        if (self.tail_len as u64) <= threshold {
            return Ok(false);
        }
        self.seal()
    }

    fn seal(&mut self) -> Result<bool> {
        // Nothing to compress.
        if self.tail_len == 0 {
            return Ok(false);
        }
        let tail_start = self.tail_start();
        // Read the existing frame region and the plaintext tail.
        let frame_region = if tail_start == 0 {
            Vec::new()
        } else {
            let mut buf = vec![0u8; tail_start as usize];
            let file = self.file.as_ref().expect("file open if tail_len > 0");
            file.read_exact_at(&mut buf, 0)?;
            buf
        };
        let mut tail = vec![0u8; self.tail_len as usize];
        let file = self.file.as_ref().expect("file open if tail_len > 0");
        file.read_exact_at(&mut tail, tail_start)?;

        // Compress the tail into ONE frame.
        let frame = compress_one_frame(&tail, self.next_dict_id, self.dict_provider.as_deref())?;

        let new_footer = Footer {
            tail_len: 0,
            entry_count: self.entry_count,
        };
        let footer_bytes = write_footer_bytes(new_footer);

        // Write `<frame_region || new_frame || new_footer>` to `<shard>.tmp`.
        {
            let mut tmp = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&self.tmp_path)?;
            tmp.write_all(&frame_region)?;
            tmp.write_all(&frame)?;
            tmp.write_all(&footer_bytes)?;
            tmp.sync_all()?;
        }

        // Atomic-ish rename, then fsync the parent dir.
        std::fs::rename(&self.tmp_path, &self.path)?;
        if let Some(parent) = self.path.parent() {
            // Fsync the directory so the rename is durable.
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        // Reopen the file descriptor; the old one points at an unlinked inode.
        let new_file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let new_size = (frame_region.len() + frame.len() + FOOTER_SIZE) as u64;
        self.file = Some(new_file);
        self.file_size = new_size;
        self.tail_len = 0;
        Ok(true)
    }

    /// Iterate every string in this shard, invoking `f(local_id, &bytes)` for
    /// each. Holds no buffers across frames beyond the current frame's decoded
    /// payload. Returning `Err` from `f` stops iteration with that error.
    pub fn for_each<F: FnMut(u32, &[u8]) -> Result<()>>(&self, mut f: F) -> Result<()> {
        if self.file_size == 0 {
            return Ok(());
        }
        let file = self.file.as_ref().expect("file open if size > 0");
        let tail_start = self.tail_start();
        let mut local_id: u32 = 0;

        // Walk frames using pread to find frame sizes, then decompress each in
        // turn. We read frame headers in small chunks; if the header doesn't
        // fit we read the whole remaining frame region.
        let mut cursor: u64 = 0;
        while cursor < tail_start {
            let remaining = tail_start - cursor;
            // Read up to `remaining` bytes, capped at a header-walk window.
            // 18 bytes is enough for the zstd frame header in nearly all
            // cases; if not, expand. We just read the whole remainder when
            // small.
            let probe_len = remaining.min(64 * 1024) as usize;
            let mut probe = vec![0u8; probe_len];
            file.read_exact_at(&mut probe, cursor)?;
            let frame_len = zstd::zstd_safe::find_frame_compressed_size(&probe)
                .map_err(|_| StrpoolError::Zstd("frame header walk failed".to_string()))?;
            if frame_len == 0 || frame_len as u64 > remaining {
                return Err(StrpoolError::Zstd("invalid frame length".to_string()));
            }
            // Read the full frame.
            let frame = if frame_len <= probe.len() {
                probe[..frame_len].to_vec()
            } else {
                let mut buf = vec![0u8; frame_len];
                file.read_exact_at(&mut buf, cursor)?;
                buf
            };
            let decoded = decompress_one_frame(&frame, self.dict_provider.as_deref())?;
            let mut p = 0;
            while p < decoded.len() {
                match memchr::memchr(0, &decoded[p..]) {
                    Some(pos) => {
                        f(local_id, &decoded[p..p + pos])?;
                        local_id = local_id.wrapping_add(1);
                        p += pos + 1;
                    }
                    None => break,
                }
            }
            cursor += frame_len as u64;
        }

        // Tail.
        if self.tail_len > 0 {
            let mut tail = vec![0u8; self.tail_len as usize];
            file.read_exact_at(&mut tail, tail_start)?;
            let mut p = 0;
            while p < tail.len() {
                match memchr::memchr(0, &tail[p..]) {
                    Some(pos) => {
                        f(local_id, &tail[p..p + pos])?;
                        local_id = local_id.wrapping_add(1);
                        p += pos + 1;
                    }
                    None => break,
                }
            }
        }
        Ok(())
    }

    fn tail_start(&self) -> u64 {
        if self.file_size == 0 {
            return 0;
        }
        self.file_size - FOOTER_SIZE as u64 - self.tail_len as u64
    }

    /// Offset at which the next append's payload begins (i.e., the start of
    /// the current footer).
    fn write_offset(&self) -> u64 {
        if self.file_size == 0 {
            0
        } else {
            self.file_size - FOOTER_SIZE as u64
        }
    }

    fn ensure_file_open(&mut self) -> Result<()> {
        if self.file.is_none() {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&self.path)?;
            self.file = Some(f);
        }
        Ok(())
    }
}

fn checked_add_tail(tail_len: u32, n: usize) -> Result<u32> {
    let n_u32: u32 = u32::try_from(n).map_err(|_| {
        StrpoolError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tail_len would overflow u32",
        ))
    })?;
    tail_len.checked_add(n_u32).ok_or_else(|| {
        StrpoolError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tail_len would overflow u32",
        ))
    })
}

fn sidecar_tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Compress `payload` into one zstd frame, optionally with a dict.
fn compress_one_frame(
    payload: &[u8],
    dict_id: Option<u32>,
    provider: Option<&dyn DictProvider>,
) -> Result<Vec<u8>> {
    use zstd::bulk::Compressor;
    use zstd::dict::EncoderDictionary;

    let level = 3;
    match dict_id {
        None => {
            let mut c = Compressor::new(level).map_err(map_zstd_err)?;
            c.compress(payload).map_err(map_zstd_err)
        }
        Some(id) => {
            let provider = provider.ok_or(StrpoolError::MissingDict(id))?;
            let bytes = provider.dict(id)?.ok_or(StrpoolError::MissingDict(id))?;
            let ed = EncoderDictionary::copy(&bytes, level);
            let mut c = Compressor::with_prepared_dictionary(&ed).map_err(map_zstd_err)?;
            c.compress(payload).map_err(map_zstd_err)
        }
    }
}

/// Decompress one zstd frame (the slice MUST be exactly one frame).
fn decompress_one_frame(frame: &[u8], provider: Option<&dyn DictProvider>) -> Result<Vec<u8>> {
    use std::io::Read;
    use zstd::dict::DecoderDictionary;
    use zstd::stream::read::Decoder;

    let dict_id = zstd::zstd_safe::get_dict_id_from_frame(frame);
    let content_size = zstd::zstd_safe::get_frame_content_size(frame)
        .map_err(|_| StrpoolError::Zstd("unknown frame content size".to_string()))?;
    let initial_cap: usize = match content_size {
        Some(n) if n <= (usize::MAX as u64) => n as usize,
        _ => frame.len().saturating_mul(4),
    };
    let mut out = Vec::with_capacity(initial_cap);
    match dict_id {
        None => {
            let mut d = Decoder::new(frame).map_err(map_zstd_err)?;
            d.read_to_end(&mut out).map_err(map_zstd_err)?;
        }
        Some(nz) => {
            let id: u32 = nz.into();
            let provider = provider.ok_or(StrpoolError::MissingDict(id))?;
            let bytes = provider.dict(id)?.ok_or(StrpoolError::MissingDict(id))?;
            let dd = DecoderDictionary::copy(&bytes);
            let mut d = Decoder::with_prepared_dictionary(frame, &dd).map_err(map_zstd_err)?;
            d.read_to_end(&mut out).map_err(map_zstd_err)?;
        }
    }
    Ok(out)
}

fn map_zstd_err(e: std::io::Error) -> StrpoolError {
    StrpoolError::Zstd(e.to_string())
}
