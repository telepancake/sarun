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

fn zerr(code: zstd::zstd_safe::ErrorCode) -> Error {
    Error::Zstd(zstd::zstd_safe::get_error_name(code).to_string())
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
    /// walked eagerly).
    pub fn layers_newest_first(&self, chain_id: u64) -> Result<Vec<Layer>, Error> {
        let mut records: Vec<Vec<u8>> = Vec::new();
        match self.depot.read_f0(chain_id) {
            Ok(frame) => records.push(decompress(&frame, None)?),
            Err(wikimak_depot::Error::NoFrame) => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        }
        if let Some(f1) = self.depot.read_f1(chain_id)? {
            let anchor = records[0].clone();
            let mut raw_records = Vec::new();
            undelimit(&decompress(&f1, Some(&anchor))?, &mut raw_records)?;
            records.extend(raw_records);
        }
        for cold in self.depot.cold_iter(chain_id)? {
            let frame = cold?;
            let anchor = records.last().expect("cold after f1").clone();
            let mut raw_records = Vec::new();
            undelimit(&decompress(&frame, Some(&anchor))?, &mut raw_records)?;
            records.extend(raw_records);
        }
        records.iter().map(|r| Ok(codec::decode(r)?)).collect()
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

    /// Prepend `layers` (oldest → newest) as ONE prepend: one f0 swap,
    /// one f1 re-encode, one seal check — the normative multi-record
    /// form (wikimak/depot SPEC §Prepend). Never splits the batch;
    /// sealing is decided against the OLD accumulator (compose_f1).
    pub fn put_layers(&mut self, chain_id: u64, layers: &[Layer]) -> Result<(), Error> {
        let Some((newest, older_new)) = layers.split_last() else {
            return Ok(());
        };
        let record = codec::encode(newest);
        let prev_f0 = match self.depot.read_f0(chain_id) {
            Ok(b) => Some(b),
            Err(wikimak_depot::Error::NoFrame) => None,
            Err(e) => return Err(e.into()),
        };
        // Accumulator entries newest-first: the older NEW records, then
        // the demoted old head (verbatim — full-snapshot records).
        let mut entries_owned: Vec<Vec<u8>> = older_new.iter().rev()
            .map(codec::encode).collect();
        let (prev_record, old_f1_raw) = match &prev_f0 {
            Some(frame) => {
                let prev_record = decompress(frame, None)?;
                let old_f1_raw = match self.depot.read_f1(chain_id)? {
                    Some(f) => decompress(&f, Some(&prev_record))?,
                    None => Vec::new(),
                };
                entries_owned.push(prev_record.clone());
                (Some(prev_record), old_f1_raw)
            }
            None => {
                // Empty chain: the depot forbids f1 on the first
                // prepend — seed with the OLDEST record, then absorb
                // the rest as one batch.
                if let Some(oldest) = entries_owned.pop() {
                    self.depot.prepend(chain_id, &compress(&oldest, None)?,
                                       None, false)?;
                    entries_owned.insert(0, oldest);
                    // Re-run now that the chain has a head. The seed
                    // record is entries_owned's oldest again: rebuild
                    // the batch minus the seed, plus the seed as head
                    // demotee — simplest is recursion with the already
                    // seeded chain.
                    entries_owned.clear();
                    return self.put_layers_seeded(chain_id, layers);
                }
                // Single layer onto empty chain.
                return Ok(self.depot.prepend(chain_id, &compress(&record, None)?,
                                             None, false)?);
            }
        };
        let _ = prev_record;
        let delimited: Vec<Vec<u8>> = entries_owned.iter()
            .map(|r| delimit(std::slice::from_ref(r))).collect();
        let refs: Vec<&[u8]> = delimited.iter().map(|e| e.as_slice()).collect();
        let (new_f1_raw, seal) = wikimak_depot::compose_f1(
            &refs,
            if old_f1_raw.is_empty() { None } else { Some(&old_f1_raw) },
            self.seal_threshold,
        );
        let new_f0 = compress(&record, None)?;
        let new_f1 = compress(&new_f1_raw, Some(&record))?;
        self.depot.prepend(chain_id, &new_f0, Some(&new_f1), seal)?;
        Ok(())
    }

    /// `put_layers` continuation once the chain has its seeded head.
    fn put_layers_seeded(&mut self, chain_id: u64, layers: &[Layer]) -> Result<(), Error> {
        let rest = &layers[1..];
        if rest.is_empty() { return Ok(()); }
        self.put_layers(chain_id, rest)
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
