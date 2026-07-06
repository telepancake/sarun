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

    /// A frame's zstd payload is too large for the on-disk `u32` length.
    #[error("frame too large")]
    FrameTooLarge,

    /// Catch-all for invariant violations the depot detects on disk.
    #[error("corrupt: {0}")]
    Corrupt(&'static str),
}

/// Configuration for opening a depot.
pub struct DepotConfig {
    /// Root directory holding `index`, `f0/`, `f1/`, `cold/cold`.
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

    /// Flush all pending writes to durable storage. Also opportunistically
    /// runs eviction on any f0/f1 file whose dead ratio exceeds the threshold.
    pub fn flush(&self) -> Result<()> {
        let mut g = self.inner.lock().expect("depot mutex poisoned");
        g.flush()?;
        g.maybe_evict()?;
        g.flush()
    }

    /// Unlink the depot's data files and zero the index.
    pub fn delete_all(self) -> Result<()> {
        let mut g = self.inner.into_inner().expect("depot mutex poisoned");
        g.delete_all()
    }
}
