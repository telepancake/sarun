//! The refPrefix chain store: frames newest-first, each older frame
//! zstd-compressed with the next-newer record as `ZSTD_CCtx_refPrefix` —
//! the tiered-VBF anchoring discipline applied to whole tree-layers.
//!
//! `<store>/meta.json`  — refs + commit metadata (bookkeeping).
//! `<store>/chain`      — frames newest-first:
//!                        `[u32 raw_len LE | u32 zstd_len LE | zstd bytes]*`
//!
//! No magic, no version, no checksum — same division of labor as the
//! VBF design (integrity is the storage/transport layer's job).

use std::io::{Read, Write};
use std::path::Path;

use crate::{Error, Meta, Result, SizeReport};

fn zstd_err(code: zstd::zstd_safe::ErrorCode) -> Error {
    Error::Chain(zstd::zstd_safe::get_error_name(code).to_string())
}

/// Compress `src`, optionally anchored on `prefix` (the next-newer
/// record). A fresh CCtx per frame: refPrefix is consumed by one
/// compression, and correctness beats context reuse in a straightedge.
fn compress(src: &[u8], prefix: Option<&[u8]>, level: i32) -> Result<Vec<u8>> {
    let mut cctx = zstd::zstd_safe::CCtx::create();
    cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
        .map_err(zstd_err)?;
    if let Some(p) = prefix {
        cctx.ref_prefix(p).map_err(zstd_err)?;
    }
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(src.len()));
    cctx.compress2(&mut out, src).map_err(zstd_err)?;
    Ok(out)
}

fn decompress(src: &[u8], prefix: Option<&[u8]>, raw_len: usize) -> Result<Vec<u8>> {
    let mut dctx = zstd::zstd_safe::DCtx::create();
    if let Some(p) = prefix {
        dctx.ref_prefix(p).map_err(zstd_err)?;
    }
    let mut out = Vec::with_capacity(raw_len);
    dctx.decompress(&mut out, src).map_err(zstd_err)?;
    if out.len() != raw_len {
        return Err(Error::Chain(format!(
            "frame decompressed to {} bytes, expected {raw_len}",
            out.len()
        )));
    }
    Ok(out)
}

/// Write the store and produce the encoding comparison. `records` are
/// canonical layer encodings, newest-first.
pub fn write_store(
    store: &Path,
    meta: &Meta,
    records: &[Vec<u8>],
    level: i32,
) -> Result<SizeReport> {
    std::fs::create_dir_all(store)?;
    let meta_path = store.join("meta.json");
    let chain_path = store.join("chain");
    if meta_path.exists() || chain_path.exists() {
        return Err(Error::Chain(format!("store {} already populated", store.display())));
    }

    let mut chain = Vec::new();
    let mut standalone_total = 0u64;
    for (i, rec) in records.iter().enumerate() {
        let prefix = if i == 0 { None } else { Some(records[i - 1].as_slice()) };
        let frame = compress(rec, prefix, level)?;
        chain.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        chain.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        chain.extend_from_slice(&frame);
        standalone_total += compress(rec, None, level)?.len() as u64;
    }

    // Solid bound: one stream over the concatenation.
    let mut concat = Vec::new();
    for rec in records {
        concat.extend_from_slice(rec);
    }
    let solid = compress(&concat, None, level)?.len() as u64;

    let mut f = std::fs::File::create(&chain_path)?;
    f.write_all(&chain)?;
    f.sync_all()?;
    let mut f = std::fs::File::create(&meta_path)?;
    serde_json::to_writer_pretty(&mut f, meta).map_err(|e| Error::Meta(e.to_string()))?;
    f.sync_all()?;

    Ok(SizeReport {
        raw: records.iter().map(|r| r.len() as u64).sum(),
        standalone: standalone_total,
        ref_prefix_chain: chain.len() as u64,
        solid,
        commits: records.len(),
        zstd_level: level,
    })
}

/// Read the store back: meta + canonical records, newest-first.
pub fn read_store(store: &Path) -> Result<(Meta, Vec<Vec<u8>>)> {
    let mut json = String::new();
    std::fs::File::open(store.join("meta.json"))?.read_to_string(&mut json)?;
    let meta: Meta = serde_json::from_str(&json).map_err(|e| Error::Meta(e.to_string()))?;

    let buf = std::fs::read(store.join("chain"))?;
    let mut records: Vec<Vec<u8>> = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        if buf.len() - pos < 8 {
            return Err(Error::Chain("truncated frame header".into()));
        }
        let raw_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        let zlen = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        if buf.len() - pos < zlen {
            return Err(Error::Chain("truncated frame body".into()));
        }
        let prefix = records.last().map(|r: &Vec<u8>| r.as_slice());
        records.push(decompress(&buf[pos..pos + zlen], prefix, raw_len)?);
        pos += zlen;
    }
    if records.len() != meta.commits.len() {
        return Err(Error::Chain(format!(
            "{} frames but {} commits in meta",
            records.len(),
            meta.commits.len()
        )));
    }
    Ok((meta, records))
}
