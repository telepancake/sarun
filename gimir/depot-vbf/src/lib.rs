//! The VBF depot variant — DEPOT-DESIGN.md §7: a sequence of canonical
//! layers stored NEWEST-FIRST in a tiered-VBF chain (`wikimak-depot`),
//! with the compression discipline that is the design's reason to exist:
//!
//!   * f0 = the newest layer's canonical record, standalone zstd — the
//!     "read current" hot path is one small decode.
//!   * f1 = older records concatenated newest-first, zstd refPrefix-
//!     anchored on f0's record — a near-identical successor costs ~the
//!     delta.
//!   * past the seal threshold the old f1 moves verbatim into a cold
//!     frame (the depot SPEC's seal invariant).
//!
//! Contract: `put_layer` makes its layer the NEW NEWEST version;
//! `next_layer` walks newest-first. Write oldest→newest, read
//! newest→oldest — the VBF access order (current cheap, history in the
//! tail). Records are length-delimited inside accumulator frames
//! (u32 LE prefix) because canonical layers, unlike wikipedia records,
//! are not self-delimiting mid-buffer.

use std::path::PathBuf;

use depot::codec;
use depot::variant::{LayerSink, LayerSource};
use depot::Layer;
use wikimak_depot::{Depot, DepotConfig};

#[derive(Debug)]
pub enum Error {
    Depot(wikimak_depot::Error),
    Codec(codec::DecodeError),
    Zstd(String),
    Truncated,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Depot(e) => write!(f, "depot: {e}"),
            Error::Codec(e) => write!(f, "codec: {e}"),
            Error::Zstd(s) => write!(f, "zstd: {s}"),
            Error::Truncated => write!(f, "truncated record framing"),
        }
    }
}

impl std::error::Error for Error {}

impl From<wikimak_depot::Error> for Error {
    fn from(e: wikimak_depot::Error) -> Self {
        Error::Depot(e)
    }
}

impl From<codec::DecodeError> for Error {
    fn from(e: codec::DecodeError) -> Self {
        Error::Codec(e)
    }
}

fn compress(raw: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    wikimak_depot::compress_frame(raw, prefix, 3).map_err(Error::Zstd)
}

fn decompress(frame: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    wikimak_depot::decompress_frame(frame, prefix).map_err(Error::Zstd)
}

fn delimit(records: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        out.extend_from_slice(&(r.len() as u32).to_le_bytes());
        out.extend_from_slice(r);
    }
    out
}

fn undelimit(buf: &[u8], out: &mut Vec<Vec<u8>>) -> Result<(), Error> {
    let mut i = 0;
    while i < buf.len() {
        if buf.len() - i < 4 {
            return Err(Error::Truncated);
        }
        let len = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if buf.len() - i < len {
            return Err(Error::Truncated);
        }
        out.push(buf[i..i + len].to_vec());
        i += len;
    }
    Ok(())
}

/// A MULTI-chain VBF layer depot: many independent newest-first layer
/// sequences (one per `chain_id`) sharing one tiered-VBF store. The
/// mirror crates use this — one chain per page/draft/ref — with their
/// inventory (name → chain_id) in their own sqlite, per the
/// bookkeeping fence (DEPOT-DESIGN.md §3).
pub struct VbfDepot {
    depot: Depot,
    /// Decompressed-accumulator seal threshold (bytes).
    seal_threshold: u64,
}

impl VbfDepot {
    /// Open (or create) the store under `root` with chain ids in
    /// `0..max_chain_id`.
    pub fn open(root: PathBuf, max_chain_id: u64, seal_threshold: u64) -> Result<Self, Error> {
        // File rolls scale with the seal threshold so eviction (which
        // only ever targets non-current files) can actually reclaim the
        // orphaned f0/f1 frames each prepend leaves behind; a giant
        // threshold would pin every orphan inside the one write target.
        let depot = Depot::open(DepotConfig {
            root,
            max_chain_id,
            file_size_threshold: (seal_threshold * 4).max(1 << 20),
            eviction_dead_ratio: 0.5,
        })?;
        Ok(VbfDepot { depot, seal_threshold })
    }

    pub fn flush(&self) -> Result<(), Error> {
        // The depot runs one opportunistic eviction pass per flush;
        // evicting one victim can push ANOTHER file over the dead
        // ratio (live frames migrate to the write target, orphaning
        // their old slots), so run to a small fixed point.
        for _ in 0..4 {
            self.depot.flush()?;
        }
        Ok(())
    }

    /// Every layer on `chain_id`, newest-first (owned; the chain is
    /// walked eagerly — the WHOLE decoded chain is resident).
    pub fn layers_newest_first(&self, chain_id: u64) -> Result<Vec<Layer>, Error> {
        let mut out = Vec::new();
        self.scan_newest_first(chain_id, |layer| {
            out.push(layer);
            false
        })?;
        Ok(out)
    }

    /// Walk `chain_id` newest-first with BOUNDED residency: one
    /// decompressed frame plus the running refPrefix anchor at a time,
    /// never the whole chain. `visit` gets each layer newest-first;
    /// return `true` to stop early (e.g. found the record you wanted).
    pub fn scan_newest_first(
        &self,
        chain_id: u64,
        mut visit: impl FnMut(Layer) -> bool,
    ) -> Result<(), Error> {
        let mut stop = false;
        let mut err: Option<Error> = None;
        self.scan_frames(chain_id, |records| {
            for r in records {
                match codec::decode(r) {
                    Ok(layer) => stop = visit(layer),
                    Err(e) => {
                        err = Some(e.into());
                        stop = true;
                    }
                }
                if stop {
                    break;
                }
            }
            stop
        })?;
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Decompressed byte length of each COLD frame on `chain_id`,
    /// newest-first — instrumentation, like [`Self::prepend_count`]:
    /// the seal-threshold discipline (frames stay ~threshold-sized,
    /// oversized batches split) is observable, not folklore.
    pub fn cold_frame_raw_lens(&self, chain_id: u64) -> Result<Vec<usize>, Error> {
        // Frame 0 of the walk is f0 (one bare record); the f1
        // accumulator follows iff the chain has one — cold frames are
        // the rest.
        let warm = 1 + match self.depot.read_f1(chain_id) {
            Ok(f1) => f1.is_some() as usize,
            Err(wikimak_depot::Error::NoFrame)
            | Err(wikimak_depot::Error::ChainIdOutOfRange) => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut lens = Vec::new();
        let mut frame_no = 0usize;
        self.scan_frames(chain_id, |records| {
            if frame_no >= warm {
                lens.push(records.iter().map(|r| r.len() + 4).sum());
            }
            frame_no += 1;
            false
        })?;
        Ok(lens)
    }

    /// Newest-first FRAME walk: `visit` gets each frame's records
    /// (newest-first within the frame; f0 is a single bare record) and
    /// returns `true` to stop. Residency: the current frame's records
    /// plus the one carried anchor record.
    fn scan_frames(
        &self,
        chain_id: u64,
        mut visit: impl FnMut(&[Vec<u8>]) -> bool,
    ) -> Result<(), Error> {
        let head = match self.depot.read_f0(chain_id) {
            Ok(frame) => decompress(&frame, None)?,
            Err(wikimak_depot::Error::NoFrame)
            | Err(wikimak_depot::Error::ChainIdOutOfRange) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // The anchor is always the record immediately NEWER than the
        // next frame's newest record: f1 was compressed against the
        // head, and a sealed accumulator moved to cold verbatim — its
        // anchor is the record demoted right after it (the previous
        // frame's oldest).
        let mut anchor = head;
        if visit(std::slice::from_ref(&anchor)) {
            return Ok(());
        }
        if let Some(f1) = self.depot.read_f1(chain_id)? {
            let mut records = Vec::new();
            undelimit(&decompress(&f1, Some(&anchor))?, &mut records)?;
            let stop = visit(&records);
            if let Some(last) = records.pop() {
                anchor = last;
            }
            if stop {
                return Ok(());
            }
        }
        for cold in self.depot.cold_iter(chain_id)? {
            let frame = cold?;
            let mut records = Vec::new();
            undelimit(&decompress(&frame, Some(&anchor))?, &mut records)?;
            let stop = visit(&records);
            if let Some(last) = records.pop() {
                anchor = last;
            }
            if stop {
                return Ok(());
            }
        }
        Ok(())
    }

    /// The newest layer on `chain_id` alone (one small standalone
    /// decode — the VBF hot path), or `None` for an empty chain.
    pub fn head_layer(&self, chain_id: u64) -> Result<Option<Layer>, Error> {
        match self.depot.read_f0(chain_id) {
            Ok(frame) => Ok(Some(codec::decode(&decompress(&frame, None)?)?)),
            Err(wikimak_depot::Error::NoFrame) => Ok(None),
            Err(wikimak_depot::Error::ChainIdOutOfRange) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Total prepends performed by the underlying depot (test/
    /// instrumentation surface — the batch invariant is observable).
    pub fn prepend_count(&self) -> u64 {
        self.depot.prepend_count()
    }

    /// The layer becomes chain `chain_id`'s NEW NEWEST version.
    pub fn put_layer(&mut self, chain_id: u64, layer: &Layer) -> Result<(), Error> {
        self.put_layers(chain_id, std::slice::from_ref(layer))
    }

    /// Prepend `layers` (oldest → newest), the normative multi-record
    /// form (wikimak/depot SPEC §Prepend): a batch is ONE f0 swap, one
    /// f1 re-encode, one seal check — EXCEPT that a batch whose records
    /// dwarf the seal threshold must not land as one frame (compose_f1's
    /// documented contract: the accumulator, and the cold frame it seals
    /// into, must stay ~threshold-sized). Such a batch is split by
    /// [`wikimak_depot::chunk_newest_first`] and prepended chunk by
    /// chunk, oldest first — prepend count scales with batch BYTES.
    pub fn put_layers(&mut self, chain_id: u64, layers: &[Layer]) -> Result<(), Error> {
        if layers.is_empty() {
            return Ok(());
        }
        // Records newest-first (layers arrive oldest → newest). Sizes
        // include the u32 length delimiter each record carries inside
        // accumulator frames — the bytes the seal budget actually sees.
        let records: Vec<Vec<u8>> = layers.iter().rev().map(codec::encode).collect();
        let sizes: Vec<usize> = records.iter().map(|r| r.len() + 4).collect();
        for range in wikimak_depot::chunk_newest_first(&sizes, self.seal_threshold) {
            self.prepend_records(chain_id, &records[range])?;
        }
        Ok(())
    }

    /// One prepend of `records` (newest-first, non-empty): one f0 swap,
    /// one f1 re-encode, one seal check. Sealing is decided against the
    /// OLD accumulator (compose_f1).
    fn prepend_records(&mut self, chain_id: u64, records: &[Vec<u8>]) -> Result<(), Error> {
        let (newest, older_new) = records.split_first().expect("non-empty record chunk");
        let prev_f0 = match self.depot.read_f0(chain_id) {
            Ok(b) => Some(b),
            Err(wikimak_depot::Error::NoFrame) => None,
            Err(e) => return Err(e.into()),
        };
        let Some(prev_f0) = prev_f0 else {
            // Empty chain: the depot forbids f1 on the first prepend —
            // seed with the OLDEST record alone, then absorb the rest
            // (now a plain non-empty-chain prepend) in one go.
            let (oldest, newer) = records.split_last().expect("non-empty record chunk");
            self.depot.prepend(chain_id, &compress(oldest, None)?, None, false)?;
            if newer.is_empty() {
                return Ok(());
            }
            return self.prepend_records(chain_id, newer);
        };
        // Accumulator entries newest-first: the older NEW records, then
        // the demoted old head (verbatim — full-snapshot records), each
        // length-delimited.
        let prev_record = decompress(&prev_f0, None)?;
        let old_f1_raw = match self.depot.read_f1(chain_id)? {
            Some(f) => decompress(&f, Some(&prev_record))?,
            None => Vec::new(),
        };
        let delimited: Vec<Vec<u8>> = older_new
            .iter()
            .chain(std::iter::once(&prev_record))
            .map(|r| delimit(std::slice::from_ref(r)))
            .collect();
        let refs: Vec<&[u8]> = delimited.iter().map(|e| e.as_slice()).collect();
        let (new_f1_raw, seal) = wikimak_depot::compose_f1(
            &refs,
            if old_f1_raw.is_empty() { None } else { Some(&old_f1_raw) },
            self.seal_threshold,
        );
        let new_f0 = compress(newest, None)?;
        let new_f1 = compress(&new_f1_raw, Some(newest))?;
        self.depot.prepend(chain_id, &new_f0, Some(&new_f1), seal)?;
        Ok(())
    }

    /// Session-end compaction ([`wikimak_depot::Depot::collect`]): rolls
    /// the current write files into eviction so dead frames parked there
    /// stop being waste at rest. What it reclaims: DEPRECATED frames —
    /// old f0/f1 versions left behind by every prepend (and by a
    /// rebuild's writes). What it can NOT reclaim: frames of chains the
    /// caller's inventory no longer references (a rebuild-orphaned
    /// chain's frames are still index-LIVE to the depot — dead weight
    /// until something truncates the chain), and cold-file bytes (the
    /// cold tier is append-only, never compacted). Cheap when there is
    /// nothing to reclaim; call once per update run, not per chain.
    pub fn collect(&self) -> Result<(), Error> {
        Ok(self.depot.collect()?)
    }
}

/// Single-chain convenience wrapper over [`VbfDepot`] — the original
/// `VbfStore` surface, kept for the transfer/`LayerSink` tests and
/// single-sequence callers.
pub struct VbfStore {
    inner: VbfDepot,
    chain_id: u64,
}

impl VbfStore {
    /// Open (or create) the chain store under `root`, storing the layer
    /// sequence on `chain_id`.
    pub fn open(root: PathBuf, chain_id: u64, seal_threshold: u64) -> Result<Self, Error> {
        Ok(VbfStore {
            inner: VbfDepot::open(root, chain_id + 1, seal_threshold)?,
            chain_id,
        })
    }

    pub fn flush(&self) -> Result<(), Error> {
        self.inner.flush()
    }

    /// Every layer, newest-first (owned; the chain is walked eagerly).
    pub fn layers_newest_first(&self) -> Result<Vec<Layer>, Error> {
        self.inner.layers_newest_first(self.chain_id)
    }
}

impl LayerSink for VbfStore {
    type Err = Error;

    /// The layer becomes the chain's NEW NEWEST version.
    fn put_layer(&mut self, layer: &Layer) -> Result<(), Error> {
        self.inner.put_layer(self.chain_id, layer)
    }
}

/// Newest-first reader over a store (eager walk, lazy decode was already
/// paid in `layers_newest_first`).
pub struct VbfReader {
    layers: std::vec::IntoIter<Layer>,
}

impl VbfReader {
    pub fn new(store: &VbfStore) -> Result<Self, Error> {
        Ok(VbfReader { layers: store.layers_newest_first()?.into_iter() })
    }
}

impl LayerSource for VbfReader {
    type Err = Error;
    fn next_layer(&mut self) -> Result<Option<Layer>, Error> {
        Ok(self.layers.next())
    }
}
