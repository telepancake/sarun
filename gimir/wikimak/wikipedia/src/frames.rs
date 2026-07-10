//! Frame compression — the discipline the depot expects and the whole
//! design exists for (tiered-VBF doc §8: "a depot whose on-disk size
//! matches its uncompressed input has not rendered this design").
//!
//! * f0 holds the newest revision's record, standalone zstd.
//! * f1 holds the older records (newest-first, concatenated), zstd with
//!   `ZSTD_CCtx_refPrefix` anchored on f0's RECORD — successive
//!   revisions are ~99% identical, so the frame costs ~the delta.
//! * A sealed cold frame keeps its f1 bytes verbatim; its anchor is the
//!   oldest record of the next-newer frame (depot SPEC chain walk).
//!
//! Per-chain pretrained dictionaries (the other half of the design's
//! compression story) are deliberately NOT built here yet — dict
//! training wants sizing against the real corpus (tiered-VBF doc §9);
//! refPrefix carries the cross-revision redundancy on its own.

use crate::error::{Error, Result};

const LEVEL: i32 = 3;

/// Compress `raw`, optionally refPrefix-anchored on `prefix`.
pub(crate) fn compress(raw: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>> {
    wikimak_depot::compress_frame(raw, prefix, LEVEL)
        .map_err(|_| Error::Codec("zstd compress"))
}

/// Decompress a frame produced by [`compress`] with the same `prefix`.
pub(crate) fn decompress(frame: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>> {
    wikimak_depot::decompress_frame(frame, prefix)
        .map_err(|_| Error::Codec("zstd decompress"))
}
