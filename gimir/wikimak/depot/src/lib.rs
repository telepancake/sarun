//! # wikimak-depot
//!
//! The chain depot. Three tiers (f0/f1/cold), one shard format, fixed-size
//! chain-id-keyed index. See `wikimak/depot/SPEC.md` for the on-disk format
//! and durability contract.
//!
//! Scope of this crate: storage primitive. It does NOT know about Wikipedia,
//! mediawiki, or revisions. It stores opaque byte blobs ("frames") in chains
//! identified by `u64` chain ids, across three tiers, with prepend + GC.

use std::path::PathBuf;
use std::sync::Mutex;

use thiserror::Error;

mod inner;

use inner::DepotInner;

/// Result alias for depot operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the depot.
#[derive(Debug, Error)]
pub enum Error {
    /// IO failure from the underlying filesystem.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The chain has no f0 frame yet (was never prepended to).
    #[error("chain has no frame")]
    NoFrame,

    /// `chain_id` is >= `max_chain_id`.
    #[error("chain id out of range")]
    ChainIdOutOfRange,

    /// First prepend must pass `new_f1_bytes = None`.
    #[error("first prepend must not supply f1 bytes")]
    FirstPrependHasF1,

    /// Subsequent prepend must pass `new_f1_bytes = Some(_)`.
    #[error("non-first prepend requires f1 bytes")]
    MissingF1,

    /// Cannot seal on the first prepend (no f1 to seal).
    #[error("cannot seal: chain has no f1")]
    CannotSealNoF1,

    /// Index file size on disk disagrees with the configured `max_chain_id`.
    #[error("index size mismatch")]
    IndexSizeMismatch,

    /// A frame would push a data file past the 48-bit pointer offset
    /// space (256TB per file).
    #[error("frame too large")]
    FrameTooLarge,

    /// The depot on disk was written by a different on-disk format
    /// version (the `format` file at the depot root is missing or
    /// mismatched). No migrations: delete and re-import.
    #[error("{0}")]
    Format(String),

    /// Catch-all for invariant violations the depot detects on disk.
    #[error("corrupt: {0}")]
    Corrupt(&'static str),
}

/// Configuration for opening a depot.
pub struct DepotConfig {
    /// Root directory holding `format`, `index`, `f0/`, `f1/`, `cold/cold`.
    pub root: PathBuf,
    /// Maximum chain id; the index is sized at `max_chain_id * 8` bytes.
    pub max_chain_id: u64,
    /// Roll to a fresh f0/f1 file once the current target hits this size.
    pub file_size_threshold: u64,
    /// Eviction triggers once a file's `bytes_deprecated / file_size` exceeds
    /// this ratio.
    pub eviction_dead_ratio: f32,
}

/// Split a newest-first record batch into prepend-sized chunks: index
/// ranges into the slice, OLDEST chunk first (the prepend order), each
/// chunk's byte total capped at `seal_threshold` (a single oversized
/// record still gets its own chunk). Prepend count thus scales with
/// batch BYTES, not record count, and every accumulator/cold frame
/// stays ~threshold-sized.
pub fn chunk_newest_first(
    sizes_newest_first: &[usize],
    seal_threshold: u64,
) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut end = sizes_newest_first.len(); // exclusive; walk from the oldest
    while end > 0 {
        let mut start = end;
        let mut total = 0u64;
        while start > 0 {
            let s = sizes_newest_first[start - 1] as u64;
            if start != end && total + s > seal_threshold {
                break;
            }
            total += s;
            start -= 1;
        }
        out.push(start..end);
        end = start;
    }
    out
}

/// The depot.
pub struct Depot {
    inner: Mutex<DepotInner>,
    prepends: std::sync::atomic::AtomicU64,
}

/// Compose the f1 accumulator for a prepend of one or more records —
/// the normative multi-record prepend construction (SPEC §"Prepend
/// multiple records"). The invariant it exists for: prepending N
/// records is ONE f0 swap + ONE f1 re-encode + ONE seal check, never N
/// cycles.
///
/// The depot stores opaque frame bytes, so this operates on the
/// caller's RAW (uncompressed) accumulator contents; framing of the
/// individual entries (self-delimiting records, length prefixes, …) and
/// the zstd anchoring of the result are the caller's, as ever.
///
/// * `entries_newest_first` — the accumulator-form bytes joining f1 in
///   this prepend, newest-first: the N-1 older new records, then the
///   DEMOTED old head (verbatim for stores whose records stand alone;
///   a caller-computed replacement — e.g. a bridge delta — otherwise).
///   Must be non-empty (an empty prepend has no f1 to compose; the
///   first prepend on a chain passes `new_f1_bytes = None` instead).
/// * `old_f1_raw` — the decompressed current accumulator, `None`/empty
///   if the chain has no f1 yet.
/// * `seal_threshold` — decompressed-accumulator seal point: when
///   absorbing the entries would push the EXISTING accumulator past it,
///   the old f1 seals to cold (pass the returned flag to
///   [`Depot::prepend`]) and the fresh accumulator holds the new
///   entries alone.
///
/// Returns `(new_f1_raw, seal_old_f1)`.
///
/// A batch whose entries alone dwarf the seal threshold must not land
/// as one frame (the accumulator — and the cold frame it seals into —
/// must stay ~threshold-sized): split it with [`chunk_newest_first`]
/// and prepend chunk by chunk, oldest chunk first.
pub fn compose_f1(
    entries_newest_first: &[&[u8]],
    old_f1_raw: Option<&[u8]>,
    seal_threshold: u64,
) -> (Vec<u8>, bool) {
    let entries_len: usize = entries_newest_first.iter().map(|e| e.len()).sum();
    let old = old_f1_raw.unwrap_or(&[]);
    let seal = !old.is_empty() && (old.len() + entries_len) as u64 > seal_threshold;
    let mut raw = Vec::with_capacity(entries_len + if seal { 0 } else { old.len() });
    for e in entries_newest_first {
        raw.extend_from_slice(e);
    }
    if !seal {
        raw.extend_from_slice(old);
    }
    (raw, seal)
}

/// Iterator over cold frames for a chain, newest-first. Each item is a
/// `Result<Vec<u8>>` of the cold frame's opaque zstd bytes.
pub struct ColdIter<'a> {
    depot: &'a Depot,
    next: u64,
}

impl<'a> Iterator for ColdIter<'a> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == 0 {
            return None;
        }
        let mut g = self.depot.inner.lock().expect("depot mutex poisoned");
        match g.read_cold_frame(self.next) {
            Ok((bytes, next)) => {
                self.next = next;
                Some(Ok(bytes))
            }
            Err(e) => {
                self.next = 0;
                Some(Err(e))
            }
        }
    }
}

impl Depot {
    /// Open or create a depot at `cfg.root`.
    pub fn open(cfg: DepotConfig) -> Result<Self> {
        let inner = DepotInner::open(cfg)?;
        Ok(Self {
            inner: Mutex::new(inner),
            prepends: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Number of `prepend` calls since open — instrumentation for the
    /// batch-prepend invariant (N records = one prepend per chain).
    pub fn prepend_count(&self) -> u64 {
        self.prepends.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Replace the chain's f0 and f1 with new bytes. See SPEC §"Prepend".
    pub fn prepend(
        &self,
        chain_id: u64,
        new_f0_bytes: &[u8],
        new_f1_bytes: Option<&[u8]>,
        seal_old_f1: bool,
    ) -> Result<()> {
        self.prepends
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        g.prepend(chain_id, new_f0_bytes, new_f1_bytes, seal_old_f1)
    }

    /// Read the current f0 frame's opaque zstd bytes (header stripped).
    pub fn read_f0(&self, chain_id: u64) -> Result<Vec<u8>> {
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        g.read_f0(chain_id)
    }

    /// Read the current f1 frame's opaque zstd bytes; `Ok(None)` if no f1.
    pub fn read_f1(&self, chain_id: u64) -> Result<Option<Vec<u8>>> {
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        g.read_f1(chain_id)
    }

    /// Iterate cold frames newest-first.
    pub fn cold_iter(&self, chain_id: u64) -> Result<ColdIter<'_>> {
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        let head = g.cold_head(chain_id)?;
        drop(g);
        Ok(ColdIter {
            depot: self,
            next: head,
        })
    }

    /// Seal the chain's CURRENT f1 to cold immediately: the f1's zstd
    /// bytes move verbatim to a new cold frame (inheriting the f1's
    /// cold-head pointer) and the chain is left with f0 and no f1 —
    /// the walk continues f0 → cold identically. This is the
    /// "just-written accumulator already dwarfs the seal threshold"
    /// escape hatch: `prepend`'s `seal_old_f1` seals the PREVIOUS f1;
    /// this seals the one written by the LAST prepend so a later
    /// incremental prepend never recompresses it. Sealing a chain
    /// with no f1 is an error ([`Error::CannotSealNoF1`]); an empty
    /// chain errors [`Error::NoFrame`].
    pub fn seal_f1(&self, chain_id: u64) -> Result<()> {
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        g.seal_f1(chain_id)
    }

    /// Flush all pending writes to durable storage. Also opportunistically
    /// runs eviction on any ROLLED f0/f1 file whose dead ratio exceeds the
    /// threshold; the current write target keeps its slack (that slack is
    /// what bounds per-prepend I/O mid-session — see `collect`).
    pub fn flush(&self) -> Result<()> {
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        g.flush()?;
        g.maybe_evict()?;
        g.flush()
    }

    /// Session-end compaction: eviction with the current write file
    /// included (rolled first). Call when a batch of updates is done —
    /// dead frames parked in the under-threshold current file are pure
    /// waste at rest. Leaves every tier file at or under the dead ratio.
    pub fn collect(&self) -> Result<()> {
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        g.collect()?;
        g.flush()
    }

    /// Unlink the depot's data files and zero the index.
    pub fn delete_all(self) -> Result<()> {
        let mut g = self.inner.into_inner().expect("depot mutex poisoned");
        g.delete_all()
    }
}

/// The NORMATIVE frame codec for chain users (gitdepot, wikipedia,
/// depot-vbf all frame through here). The load-bearing part is the
/// WINDOW: an accumulator/cold frame is a solid stream whose
/// redundancy (the same logical record across revisions) sits far
/// apart — a level-default window (~2MB at level 3) makes those
/// matches unreachable at ANY search level (measured 5x size on a
/// real corpus). Window-log therefore covers the frame PLUS its
/// refPrefix anchor (see [`frame_window_log`]), capped at 27 (the
/// decoder's default limit — readers need no configuration), with
/// long-distance matching on.
pub fn compress_frame(
    src: &[u8],
    prefix: Option<&[u8]>,
    level: i32,
) -> std::result::Result<Vec<u8>, String> {
    let err = |c| zstd::zstd_safe::get_error_name(c).to_string();
    let mut cctx = zstd::zstd_safe::CCtx::create();
    cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level)).map_err(err)?;
    let wlog = frame_window_log(src.len() as u64, prefix.map_or(0, |p| p.len() as u64));
    cctx.set_parameter(zstd::zstd_safe::CParameter::WindowLog(wlog)).map_err(err)?;
    cctx.set_parameter(zstd::zstd_safe::CParameter::EnableLongDistanceMatching(true))
        .map_err(err)?;
    if let Some(p) = prefix {
        cctx.ref_prefix(p).map_err(err)?;
    }
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(src.len()));
    cctx.compress2(&mut out, src).map_err(err)?;
    Ok(out)
}

/// Decode counterpart of [`compress_frame`] (the wlog-27 cap is what
/// keeps a default DCtx sufficient).
pub fn decompress_frame(
    frame: &[u8],
    prefix: Option<&[u8]>,
) -> std::result::Result<Vec<u8>, String> {
    let err = |c| zstd::zstd_safe::get_error_name(c).to_string();
    let raw_len = zstd::zstd_safe::get_frame_content_size(frame)
        .map_err(|_| "zstd frame content size".to_string())?
        .ok_or_else(|| "zstd frame without content size".to_string())?
        as usize;
    let mut dctx = zstd::zstd_safe::DCtx::create();
    if let Some(p) = prefix {
        dctx.ref_prefix(p).map_err(err)?;
    }
    let mut out = Vec::with_capacity(raw_len);
    dctx.decompress(&mut out, frame).map_err(err)?;
    Ok(out)
}

/// The window log [`compress_frame`] picks for a frame of
/// `total_raw_len` bytes anchored on `prefix_len` bytes — exposed so the
/// streaming encoder (which cannot see the whole input) reproduces the
/// bulk choice exactly. The window must cover input PLUS prefix: a match
/// from stream position `p` into the anchor spans a distance of `p` plus
/// the un-matched anchor tail, up to `total + prefix`. Sizing the window
/// to the input alone leaves the anchor unreachable from everywhere past
/// `window − prefix` — refPrefix silently degrades to a no-op for most
/// of a large frame. Still capped at 27 (the decoder's default limit —
/// readers need no configuration); beyond 128 MB combined, the far end
/// of the anchor degrades again, by that documented trade.
pub fn frame_window_log(total_raw_len: u64, prefix_len: u64) -> u32 {
    (64 - (total_raw_len + prefix_len).max(1 << 20).leading_zeros()).min(27)
}

/// Streaming form of [`compress_frame`]: IDENTICAL parameters (window
/// log from the caller-declared total raw length, long-distance
/// matching, refPrefix) fed through `ZSTD_compressStream2`, producing
/// byte-identical output to the bulk call for the same
/// `(input, prefix, level, total)`. The caller MUST know the total
/// raw size upfront (`total_raw_len`) — it pins both the window log
/// and the frame header's content size (pledged; writing a different
/// number of bytes errors at `finish`). The prefix must outlive the
/// encoder, exactly like zstd's refPrefix contract.
pub struct FrameEncoder<'p> {
    cctx: zstd::zstd_safe::CCtx<'p>,
    out: Vec<u8>,
    scratch: Vec<u8>,
    written: u64,
    total: u64,
    /// The frame was closed (`ZSTD_e_end` issued). Byte parity with
    /// the bulk call requires the end directive to travel WITH the
    /// final input bytes (a trailing empty `e_end` emits a different
    /// last block) — `write` closes the frame the moment `written`
    /// reaches the declared total.
    ended: bool,
}

impl<'p> FrameEncoder<'p> {
    pub fn new(
        total_raw_len: u64,
        prefix: Option<&'p [u8]>,
        level: i32,
    ) -> std::result::Result<Self, String> {
        let err = |c| zstd::zstd_safe::get_error_name(c).to_string();
        let mut cctx = zstd::zstd_safe::CCtx::create();
        cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
            .map_err(err)?;
        cctx.set_parameter(zstd::zstd_safe::CParameter::WindowLog(frame_window_log(
            total_raw_len,
            prefix.map_or(0, |p| p.len() as u64),
        )))
        .map_err(err)?;
        cctx.set_parameter(zstd::zstd_safe::CParameter::EnableLongDistanceMatching(true))
            .map_err(err)?;
        cctx.set_pledged_src_size(Some(total_raw_len)).map_err(err)?;
        if let Some(p) = prefix {
            cctx.ref_prefix(p).map_err(err)?;
        }
        Ok(Self {
            cctx,
            out: Vec::new(),
            scratch: vec![0u8; zstd::zstd_safe::CCtx::out_size()],
            written: 0,
            total: total_raw_len,
            ended: false,
        })
    }

    fn pump(&mut self, src: &[u8], end: bool) -> std::result::Result<(), String> {
        let err = |c| zstd::zstd_safe::get_error_name(c).to_string();
        let mut input = zstd::zstd_safe::InBuffer::around(src);
        let dir = if end {
            zstd::zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end
        } else {
            zstd::zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_continue
        };
        loop {
            let mut ob = zstd::zstd_safe::OutBuffer::around(&mut self.scratch[..]);
            let remaining = self.cctx.compress_stream2(&mut ob, &mut input, dir).map_err(err)?;
            let n = ob.pos();
            self.out.extend_from_slice(&self.scratch[..n]);
            if end {
                if remaining == 0 {
                    return Ok(());
                }
            } else if input.pos() == src.len() && n < self.scratch.len() {
                return Ok(());
            }
        }
    }

    /// Feed the next chunk of raw frame bytes. The chunk that reaches
    /// the declared total closes the frame.
    pub fn write(&mut self, chunk: &[u8]) -> std::result::Result<(), String> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.written += chunk.len() as u64;
        if self.written > self.total {
            return Err("frame encoder: more bytes than declared".into());
        }
        let last = self.written == self.total;
        if last {
            self.ended = true;
        }
        self.pump(chunk, last)
    }

    /// Verify the declared total was written, close the frame (only
    /// the empty frame is still open here), and return the complete
    /// compressed bytes.
    pub fn finish(mut self) -> std::result::Result<Vec<u8>, String> {
        if self.written != self.total {
            return Err(format!(
                "frame encoder: wrote {} of declared {} bytes",
                self.written, self.total
            ));
        }
        if !self.ended {
            self.pump(&[], true)?;
        }
        Ok(self.out)
    }
}

/// Streaming decode counterpart of [`decompress_frame`]: reads the
/// compressed frame bytes (with the optional refPrefix set before the
/// first read) and yields the raw bytes incrementally via
/// [`std::io::Read`] — never materializing the whole decompressed
/// frame. Decodes frames produced by either the bulk or the streaming
/// encoder.
pub struct FrameDecoder<'a> {
    dctx: zstd::zstd_safe::DCtx<'a>,
    frame: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> FrameDecoder<'a> {
    pub fn new(
        frame: &'a [u8],
        prefix: Option<&'a [u8]>,
    ) -> std::result::Result<Self, String> {
        let err = |c| zstd::zstd_safe::get_error_name(c).to_string();
        let mut dctx = zstd::zstd_safe::DCtx::create();
        if let Some(p) = prefix {
            dctx.ref_prefix(p).map_err(err)?;
        }
        Ok(Self { dctx, frame, pos: 0, done: false })
    }
}

impl std::io::Read for FrameDecoder<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        let mut input = zstd::zstd_safe::InBuffer::around(&self.frame[self.pos..]);
        let mut output = zstd::zstd_safe::OutBuffer::around(&mut buf[..]);
        loop {
            let hint = self
                .dctx
                .decompress_stream(&mut output, &mut input)
                .map_err(|c| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        zstd::zstd_safe::get_error_name(c),
                    )
                })?;
            if hint == 0 {
                self.done = true;
                break;
            }
            if output.pos() == output.capacity() || output.pos() > 0 && input.pos() == input.src.len()
            {
                break;
            }
            if input.pos() == input.src.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated zstd frame",
                ));
            }
        }
        self.pos += input.pos();
        Ok(output.pos())
    }
}
