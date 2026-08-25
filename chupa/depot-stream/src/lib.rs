//! The stream depot variant — DEPOT-DESIGN.md §7.
//!
//! A depot whose entire storage IS the canonical encoding: an ordered
//! sequence of layers as zstd frames, written and read strictly in one
//! pass. It is the wire form — import/export/transfer are "transfer
//! to/from the stream variant", nothing more.
//!
//! Frame format (same shape as gitdepot's chain):
//!
//! ```text
//! [u32 raw_len LE | u32 zstd_len LE | zstd bytes]*
//! ```
//!
//! Frame 0 is compressed standalone; frame i is refPrefix-anchored on
//! record i-1 (the previous layer's canonical bytes), so a chain-shaped
//! feed — adjacent layers similar, e.g. newest-first history — costs
//! ~the delta per layer while both sides still run one-pass with one
//! record of lookback. The variant imposes no ordering semantics itself:
//! the caller's feed order is the stored order, and choosing an order
//! that compresses (adjacent-similar) is the caller's concern, like
//! everything else about layer ordering (§1: inventory belongs to the
//! caller).
//!
//! No magic, no version byte, no checksum in the stream itself —
//! integrity and versioning belong to whatever carries the bytes (§4).
//! Sink-only on write, walk-only on read: this variant is the reason
//! the trait is split in half.

use std::io::{Read, Write};

use depot::codec;
use depot::variant::{LayerSink, LayerSource};
use depot::Layer;

/// Errors from either half of the stream.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Zstd(String),
    Codec(codec::DecodeError),
    /// Input ended mid-frame.
    Truncated,
    /// A frame decompressed to a length other than its header's raw_len.
    BadFrame,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Zstd(s) => write!(f, "zstd: {s}"),
            Error::Codec(e) => write!(f, "codec: {e}"),
            Error::Truncated => write!(f, "stream truncated mid-frame"),
            Error::BadFrame => write!(f, "frame length mismatch"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
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

fn compress(src: &[u8], prefix: Option<&[u8]>, level: i32) -> Result<Vec<u8>, Error> {
    let mut cctx = zstd::zstd_safe::CCtx::create();
    cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
        .map_err(zerr)?;
    if let Some(p) = prefix {
        cctx.ref_prefix(p).map_err(zerr)?;
    }
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(src.len()));
    cctx.compress2(&mut out, src).map_err(zerr)?;
    Ok(out)
}

fn decompress(src: &[u8], prefix: Option<&[u8]>, raw_len: usize) -> Result<Vec<u8>, Error> {
    let mut dctx = zstd::zstd_safe::DCtx::create();
    if let Some(p) = prefix {
        dctx.ref_prefix(p).map_err(zerr)?;
    }
    let mut out = Vec::with_capacity(raw_len);
    dctx.decompress(&mut out, src).map_err(zerr)?;
    if out.len() != raw_len {
        return Err(Error::BadFrame);
    }
    Ok(out)
}

// ---------------------------------------------------------------- writer

/// One-pass stream writer: a [`LayerSink`] over any `io::Write`.
pub struct StreamWriter<W: Write> {
    out: W,
    prev: Option<Vec<u8>>,
    level: i32,
    written: u64,
}

impl<W: Write> StreamWriter<W> {
    pub fn new(out: W, level: i32) -> Self {
        StreamWriter { out, prev: None, level, written: 0 }
    }

    /// Total stream bytes emitted so far (headers + frames).
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Flush and return the underlying writer. (Nothing buffered beyond
    /// the last frame; this exists so callers can recover `W`.)
    pub fn finish(mut self) -> Result<W, Error> {
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> LayerSink for StreamWriter<W> {
    type Err = Error;

    fn put_layer(&mut self, layer: &Layer) -> Result<(), Error> {
        let record = codec::encode(layer);
        let frame = compress(&record, self.prev.as_deref(), self.level)?;
        self.out.write_all(&(record.len() as u32).to_le_bytes())?;
        self.out.write_all(&(frame.len() as u32).to_le_bytes())?;
        self.out.write_all(&frame)?;
        self.written += 8 + frame.len() as u64;
        self.prev = Some(record);
        Ok(())
    }
}

// ---------------------------------------------------------------- reader

/// One-pass stream reader: a [`LayerSource`] over any `io::Read`.
pub struct StreamReader<R: Read> {
    input: R,
    prev: Option<Vec<u8>>,
}

impl<R: Read> StreamReader<R> {
    pub fn new(input: R) -> Self {
        StreamReader { input, prev: None }
    }

    /// Read exactly `n` bytes, or `Ok(None)` on clean EOF at a frame
    /// boundary (`n` asked at offset 0 of a frame), or `Truncated` on
    /// EOF inside a frame.
    fn read_exact_opt(&mut self, buf: &mut [u8], at_boundary: bool) -> Result<Option<()>, Error> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.input.read(&mut buf[filled..])?;
            if n == 0 {
                if filled == 0 && at_boundary {
                    return Ok(None);
                }
                return Err(Error::Truncated);
            }
            filled += n;
        }
        Ok(Some(()))
    }
}

impl<R: Read> LayerSource for StreamReader<R> {
    type Err = Error;

    fn next_layer(&mut self) -> Result<Option<Layer>, Error> {
        let mut head = [0u8; 8];
        if self.read_exact_opt(&mut head, true)?.is_none() {
            return Ok(None);
        }
        let raw_len = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
        let zlen = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
        let mut frame = vec![0u8; zlen];
        if self.read_exact_opt(&mut frame, false)?.is_none() {
            return Err(Error::Truncated);
        }

        let record = decompress(&frame, self.prev.as_deref(), raw_len)?;
        let layer = codec::decode(&record)?;
        self.prev = Some(record);
        Ok(Some(layer))
    }
}
