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

/// Encode records (newest-first) as a refPrefix chain: frame 0
/// standalone, frame i anchored on record i-1.
fn chain_bytes(records: &[Vec<u8>], level: i32) -> Result<Vec<u8>> {
    let mut chain = Vec::new();
    for (i, rec) in records.iter().enumerate() {
        let prefix = if i == 0 { None } else { Some(records[i - 1].as_slice()) };
        let frame = compress(rec, prefix, level)?;
        chain.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        chain.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        chain.extend_from_slice(&frame);
    }
    Ok(chain)
}

fn standalone_total(records: &[Vec<u8>], level: i32) -> Result<u64> {
    let mut total = 0u64;
    for rec in records {
        total += compress(rec, None, level)?.len() as u64;
    }
    Ok(total)
}

fn solid_total(records: &[Vec<u8>], level: i32) -> Result<u64> {
    let mut concat = Vec::new();
    for rec in records {
        concat.extend_from_slice(rec);
    }
    Ok(compress(&concat, None, level)?.len() as u64)
}

/// The stored (view-anchored) chain: frame 0 = the newest full record
/// standalone; frame i = delta record i compressed with the previous
/// commit's FULL record — the canonical bytes of its view — as
/// refPrefix. The decoder recomputes that anchor from the reconstructed
/// view via `diff(None, view)`; both sides go through the one canonical
/// encoding, whose bit-exactness is load-bearing.
fn view_chain_bytes(
    delta_records: &[Vec<u8>],
    full_records: &[Vec<u8>],
    level: i32,
) -> Result<Vec<u8>> {
    let mut chain = Vec::new();
    for (i, rec) in delta_records.iter().enumerate() {
        let prefix = if i == 0 { None } else { Some(full_records[i - 1].as_slice()) };
        let frame = compress(rec, prefix, level)?;
        chain.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        chain.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        chain.extend_from_slice(&frame);
    }
    Ok(chain)
}

/// Write the store (the view-anchored chain is the rest form) and
/// produce the encoding comparison over both record families.
pub fn write_store(
    store: &Path,
    meta: &Meta,
    delta_records: &[Vec<u8>],
    full_records: &[Vec<u8>],
    level: i32,
    report: bool,
) -> Result<Option<SizeReport>> {
    std::fs::create_dir_all(store)?;
    let meta_path = store.join("meta.json");
    let chain_path = store.join("chain");
    if meta_path.exists() || chain_path.exists() {
        return Err(Error::Chain(format!("store {} already populated", store.display())));
    }

    let view_chain = view_chain_bytes(delta_records, full_records, level)?;

    let mut f = std::fs::File::create(&chain_path)?;
    f.write_all(&view_chain)?;
    f.sync_all()?;
    let mut f = std::fs::File::create(&meta_path)?;
    serde_json::to_writer_pretty(&mut f, meta).map_err(|e| Error::Meta(e.to_string()))?;
    f.sync_all()?;

    if !report {
        return Ok(None);
    }
    Ok(Some(SizeReport {
        commits: delta_records.len(),
        zstd_level: level,
        full_raw: full_records.iter().map(|r| r.len() as u64).sum(),
        full_standalone: standalone_total(full_records, level)?,
        full_ref_chain: chain_bytes(full_records, level)?.len() as u64,
        delta_raw: delta_records.iter().map(|r| r.len() as u64).sum(),
        delta_standalone: standalone_total(delta_records, level)?,
        delta_ref_chain: chain_bytes(delta_records, level)?.len() as u64,
        view_ref_chain: view_chain.len() as u64,
        solid_full: solid_total(full_records, level)?,
    }))
}

/// Read `meta.json` alone (no chain walk).
pub fn read_meta(store: &Path) -> Result<Meta> {
    let mut json = String::new();
    std::fs::File::open(store.join("meta.json"))?.read_to_string(&mut json)?;
    serde_json::from_str(&json).map_err(|e| Error::Meta(e.to_string()))
}

/// Rewrite `meta.json` (tmp + rename) leaving the chain untouched.
pub fn write_meta(store: &Path, meta: &Meta) -> Result<()> {
    let tmp = store.join("meta.json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut f, meta).map_err(|e| Error::Meta(e.to_string()))?;
    f.sync_all()?;
    std::fs::rename(&tmp, store.join("meta.json"))?;
    Ok(())
}

/// The newest record (frame 0 — standalone by construction) without
/// walking the rest of the chain.
pub fn read_head_record(store: &Path) -> Result<Vec<u8>> {
    let buf = std::fs::read(store.join("chain"))?;
    let (raw_len, zlen) = frame_header(&buf, 0)?;
    decompress(&buf[8..8 + zlen], None, raw_len)
}

fn frame_header(buf: &[u8], pos: usize) -> Result<(usize, usize)> {
    if buf.len() - pos < 8 {
        return Err(Error::Chain("truncated frame header".into()));
    }
    let raw_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
    let zlen = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
    if buf.len() - pos - 8 < zlen {
        return Err(Error::Chain("truncated frame body".into()));
    }
    Ok((raw_len, zlen))
}

/// Prepend `k` new commits to the front of the chain (the incremental
/// append — MIRRORS.md phase 3). `delta_records` is `k+1` records
/// newest-first: the new commits' deltas plus, last, the BRIDGE delta
/// that rebuilds the former head view from the oldest new view.
/// `full_records` is the `k` new commits' full records; new frame `i`
/// is anchored on `full_records[i-1]`, frame 0 standalone — so the
/// former frame 0 (the old head's standalone full record) is REPLACED
/// by the bridge frame, and every remaining old frame keeps its anchor
/// (the old full records are unchanged) and is copied verbatim.
pub fn prepend_store(
    store: &Path,
    meta: &Meta,
    delta_records: &[Vec<u8>],
    full_records: &[Vec<u8>],
    level: i32,
) -> Result<()> {
    if delta_records.len() != full_records.len() + 1 {
        return Err(Error::Chain("prepend: need k+1 delta records for k commits".into()));
    }
    let old = std::fs::read(store.join("chain"))?;
    let (_, zlen) = frame_header(&old, 0)?;
    let tail = &old[8 + zlen..];

    let mut chain = view_chain_bytes(delta_records, full_records, level)?;
    chain.extend_from_slice(tail);

    // tmp + rename on both files; chain first, then meta — a crash
    // between the two leaves a chain longer than meta.commits, which
    // read_store rejects loudly rather than serving a half-update.
    let chain_tmp = store.join("chain.tmp");
    let mut f = std::fs::File::create(&chain_tmp)?;
    f.write_all(&chain)?;
    f.sync_all()?;
    std::fs::rename(&chain_tmp, store.join("chain"))?;
    write_meta(store, meta)
}

/// Read the store back: meta + the reconstructed VIEWS, newest-first.
/// Each frame's refPrefix anchor is recomputed from the previous view's
/// canonical full record.
pub fn read_store(store: &Path) -> Result<(Meta, Vec<depot::View>)> {
    let mut json = String::new();
    std::fs::File::open(store.join("meta.json"))?.read_to_string(&mut json)?;
    let meta: Meta = serde_json::from_str(&json).map_err(|e| Error::Meta(e.to_string()))?;

    let buf = std::fs::read(store.join("chain"))?;
    let mut views: Vec<depot::View> = Vec::new();
    let mut prev_full: Option<Vec<u8>> = None;
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
        let record = decompress(&buf[pos..pos + zlen], prev_full.as_deref(), raw_len)?;
        pos += zlen;

        let layer = depot::codec::decode(&record)?;
        let view = depot::apply(views.last(), &layer).ok_or_else(|| {
            Error::Chain(format!("frame {} resolves to nothing", views.len()))
        })?;
        prev_full = Some(depot::codec::encode(&depot::diff(None, Some(&view))));
        views.push(view);
    }
    if views.len() != meta.commits.len() {
        return Err(Error::Chain(format!(
            "{} frames but {} commits in meta",
            views.len(),
            meta.commits.len()
        )));
    }
    Ok((meta, views))
}
