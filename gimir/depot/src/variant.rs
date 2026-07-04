//! The trait halves (DEPOT-DESIGN.md §5) and generic transfer.
//!
//! Deliberately minimal and PROVISIONAL: method signatures are pinned
//! only after the fs-layer workload's operation frequencies are measured
//! against the live overlay (§5). What is already fixed by the design:
//! the split itself — a stream variant cannot random-read or accept
//! out-of-order writes, so ingest and readout are separate capabilities —
//! and transfer as the composition `walk source → feed sink`, which is
//! what makes import/export fall out of the abstraction instead of being
//! per-variant code.

use crate::Layer;

/// Accepts layers in canonical order. The stream variant is sink-only on
/// write; random-access variants implement both halves.
pub trait LayerSink {
    type Err: std::error::Error;
    fn put_layer(&mut self, layer: &Layer) -> Result<(), Self::Err>;
}

/// Yields layers in stored order.
pub trait LayerSource {
    type Err: std::error::Error;
    fn next_layer(&mut self) -> Result<Option<Layer>, Self::Err>;
}

/// Transfer failure: either side's error, kept distinguishable.
#[derive(Debug)]
pub enum TransferError<S, K> {
    Source(S),
    Sink(K),
}

impl<S: std::fmt::Display, K: std::fmt::Display> std::fmt::Display for TransferError<S, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::Source(e) => write!(f, "source: {e}"),
            TransferError::Sink(e) => write!(f, "sink: {e}"),
        }
    }
}

impl<S, K> std::error::Error for TransferError<S, K>
where
    S: std::error::Error,
    K: std::error::Error,
{
}

/// Depot-to-depot transfer: walk the source in order, feed the sink.
/// Returns the number of layers moved. Sharing is depot-internal (§1),
/// so nothing of the source's internal representation crosses; the sink
/// re-establishes its own.
pub fn transfer<Src: LayerSource, Snk: LayerSink>(
    src: &mut Src,
    dst: &mut Snk,
) -> Result<u64, TransferError<Src::Err, Snk::Err>> {
    let mut moved = 0u64;
    while let Some(layer) = src.next_layer().map_err(TransferError::Source)? {
        dst.put_layer(&layer).map_err(TransferError::Sink)?;
        moved += 1;
    }
    Ok(moved)
}
