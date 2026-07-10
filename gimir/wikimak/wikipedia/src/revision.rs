//! Per-revision binary record codec. SPEC §"Per-revision storage in
//! the depot" and PHASES §"Per-revision record codec" pin the wire
//! format. The implementer must produce these exact bytes.
//!
//! Layout (all multi-byte ints little-endian, varint = unsigned LEB128):
//!
//! ```text
//! [ u32 schema_version | u32 flags | u64 rev_id | u64 parent_id
//! | u64 ts_unix_micros | u64 contributor_user_id | u8 contributor_kind
//! | varint contributor_len | contributor_bytes
//! | varint comment_len    | comment_bytes
//! | varint sha1_len       | sha1_bytes
//! | varint text_len       | text_bytes ]
//! ```
//!
//! Encoder/decoder are `pub` so the acceptance suite can pin the byte
//! layout. The implementer keeps the bodies; the tester pins the
//! signatures + constants.

use chrono::{DateTime, TimeZone, Utc};

use crate::error::{Error, Result};
use crate::instance::{ContributorMeta, RevisionMeta};

/// Schema version stamp. Bump on incompatible codec changes.
pub const REVISION_SCHEMA_VERSION: u32 = 1;

// Flag bits — SPEC §"Per-revision storage in the depot". Pinned.
pub const FLAG_TEXT_HIDDEN: u32 = 0x01;
pub const FLAG_COMMENT_HIDDEN: u32 = 0x02;
pub const FLAG_CONTRIBUTOR_HIDDEN: u32 = 0x04;
pub const FLAG_SUPPRESSED: u32 = 0x08;
pub const FLAG_SHA1_MISMATCH: u32 = 0x10;

// Contributor kind bytes — SPEC. Pinned.
pub const KIND_ANONYMOUS: u8 = 0;
pub const KIND_NAMED: u8 = 1;
pub const KIND_HIDDEN: u8 = 2;

/// Encode one revision record (meta + text) into the wire format.
///
/// The result is a single contiguous byte blob that the implementer
/// will hand to the depot as one frame's payload (after refPrefix-zstd
/// compression — the depot itself is opaque to compression).
pub fn encode_revision(meta: &RevisionMeta, text: &[u8]) -> Vec<u8> {
    let (kind, user_id, contrib_bytes) = contributor_wire(&meta.contributor);
    let ts_micros = meta.ts.timestamp_micros() as u64;

    let mut out = Vec::with_capacity(
        4 + 4
            + 8
            + 8
            + 8
            + 8
            + 1
            + 5
            + contrib_bytes.len()
            + 5
            + meta.comment.len()
            + 5
            + meta.sha1.len()
            + 5
            + text.len(),
    );
    out.extend_from_slice(&REVISION_SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&meta.flags.to_le_bytes());
    out.extend_from_slice(&meta.rev_id.to_le_bytes());
    out.extend_from_slice(&meta.parent_id.to_le_bytes());
    out.extend_from_slice(&ts_micros.to_le_bytes());
    out.extend_from_slice(&user_id.to_le_bytes());
    out.push(kind);
    encode_varint(contrib_bytes.len() as u64, &mut out);
    out.extend_from_slice(contrib_bytes);
    encode_varint(meta.comment.len() as u64, &mut out);
    out.extend_from_slice(meta.comment.as_bytes());
    encode_varint(meta.sha1.len() as u64, &mut out);
    out.extend_from_slice(meta.sha1.as_bytes());
    encode_varint(text.len() as u64, &mut out);
    out.extend_from_slice(text);
    out
}

/// Decode one revision record. Returns the metadata + an owned copy of
/// the text bytes. Prefer [`decode_revision_view`] anywhere the text is
/// not (or not yet) needed — meta-only consumers must never pay the
/// text copy (read paths walk records by the thousand).
pub fn decode_revision(buf: &[u8]) -> Result<(RevisionMeta, Vec<u8>)> {
    let (meta, text) = decode_revision_view(buf)?;
    Ok((meta, text.to_vec()))
}

/// Decode one revision record's metadata, BORROWING the text bytes —
/// the meta view for walk-style readers. Nothing text-sized is copied;
/// extract the one text a read actually wants with `.to_vec()` on the
/// returned slice.
pub fn decode_revision_view(buf: &[u8]) -> Result<(RevisionMeta, &[u8])> {
    let mut off = 0usize;
    let ver = read_u32_le(buf, &mut off)?;
    if ver != REVISION_SCHEMA_VERSION {
        return Err(Error::Codec("unknown schema_version"));
    }
    let flags = read_u32_le(buf, &mut off)?;
    let rev_id = read_u64_le(buf, &mut off)?;
    let parent_id = read_u64_le(buf, &mut off)?;
    let ts_micros = read_u64_le(buf, &mut off)? as i64;
    let user_id = read_u64_le(buf, &mut off)?;
    let kind = read_u8(buf, &mut off)?;

    let (contrib_len, n) = decode_varint(buf, off)?;
    off += n;
    let contrib_bytes = read_slice(buf, &mut off, contrib_len as usize)?;
    let contributor = match kind {
        KIND_ANONYMOUS => ContributorMeta::Anonymous {
            ip: utf8_owned(contrib_bytes)?,
        },
        KIND_NAMED => ContributorMeta::Named {
            username: utf8_owned(contrib_bytes)?,
            user_id,
        },
        KIND_HIDDEN => ContributorMeta::Hidden,
        _ => return Err(Error::Codec("unknown contributor_kind")),
    };

    let (comment_len, n) = decode_varint(buf, off)?;
    off += n;
    let comment = utf8_owned(read_slice(buf, &mut off, comment_len as usize)?)?;

    let (sha1_len, n) = decode_varint(buf, off)?;
    off += n;
    let sha1 = utf8_owned(read_slice(buf, &mut off, sha1_len as usize)?)?;

    let (text_len, n) = decode_varint(buf, off)?;
    off += n;
    let text = read_slice(buf, &mut off, text_len as usize)?;

    let ts: DateTime<Utc> = Utc
        .timestamp_micros(ts_micros)
        .single()
        .ok_or(Error::Codec("invalid timestamp"))?;

    let meta = RevisionMeta {
        rev_id,
        parent_id,
        ts,
        contributor,
        comment,
        sha1,
        flags,
        text_len,
    };
    Ok((meta, text))
}

/// Peek a record's `rev_id` (fixed offset 8, after version + flags)
/// without decoding anything else — the walk-style readers compare ids
/// against their target for every record they pass.
pub(crate) fn peek_rev_id(rec: &[u8]) -> Result<u64> {
    if rec.len() < 16 {
        return Err(Error::Codec("truncated record fixed prefix"));
    }
    Ok(u64::from_le_bytes(rec[8..16].try_into().unwrap()))
}

/// Peek a record's timestamp (unix micros; fixed offset 24) without
/// decoding anything else.
pub(crate) fn peek_ts(rec: &[u8]) -> Result<i64> {
    if rec.len() < 32 {
        return Err(Error::Codec("truncated record fixed prefix"));
    }
    Ok(u64::from_le_bytes(rec[24..32].try_into().unwrap()) as i64)
}

/// Byte length of the record starting at `start` in a buffer of zero or
/// more concatenated records (codec fixed prefix + four varint-prefixed
/// blobs). Lets a frame walk step record-to-record without decoding —
/// and without copying — anything.
pub(crate) fn record_len(buf: &[u8], start: usize) -> Result<usize> {
    // Fixed prefix: u32 + u32 + u64 + u64 + u64 + u64 + u8 = 41 bytes.
    const FIXED: usize = 4 + 4 + 8 + 8 + 8 + 8 + 1;
    let mut i = start;
    if i + FIXED > buf.len() {
        return Err(Error::Codec("truncated record fixed prefix"));
    }
    i += FIXED;
    // Four length-prefixed byte fields (contributor, comment, sha1, text).
    for _ in 0..4 {
        let (len, n) = decode_varint(buf, i)?;
        i += n;
        let len = len as usize;
        if i + len > buf.len() {
            return Err(Error::Codec("truncated record payload"));
        }
        i += len;
    }
    Ok(i - start)
}

/// Encode an unsigned LEB128 varint. Exposed for codec tests.
pub fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decode an unsigned LEB128 varint from `buf`, starting at `offset`.
/// Returns `(value, bytes_consumed)`. Exposed for codec tests.
pub fn decode_varint(buf: &[u8], offset: usize) -> Result<(u64, usize)> {
    let mut val: u64 = 0;
    let mut shift: u32 = 0;
    let mut i = offset;
    loop {
        if i >= buf.len() {
            return Err(Error::Codec("truncated varint"));
        }
        let b = buf[i];
        i += 1;
        val |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((val, i - offset));
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Codec("varint overflow"));
        }
    }
}

// ContributorMeta helpers — `contributor_user_id` is `0` for Anonymous
// and Hidden; `contributor_bytes` is the IP for Anonymous, the username
// for Named, and empty for Hidden.

/// Pull `(kind, user_id, bytes)` out of a `ContributorMeta`. Exposed
/// for the codec tests.
pub fn contributor_wire(c: &ContributorMeta) -> (u8, u64, &[u8]) {
    match c {
        ContributorMeta::Anonymous { ip } => (KIND_ANONYMOUS, 0, ip.as_bytes()),
        ContributorMeta::Named { username, user_id } => (KIND_NAMED, *user_id, username.as_bytes()),
        ContributorMeta::Hidden => (KIND_HIDDEN, 0, &[]),
    }
}

// --- private helpers ---

fn read_u32_le(buf: &[u8], off: &mut usize) -> Result<u32> {
    if *off + 4 > buf.len() {
        return Err(Error::Codec("truncated u32"));
    }
    let v = u32::from_le_bytes(buf[*off..*off + 4].try_into().unwrap());
    *off += 4;
    Ok(v)
}

fn read_u64_le(buf: &[u8], off: &mut usize) -> Result<u64> {
    if *off + 8 > buf.len() {
        return Err(Error::Codec("truncated u64"));
    }
    let v = u64::from_le_bytes(buf[*off..*off + 8].try_into().unwrap());
    *off += 8;
    Ok(v)
}

fn read_u8(buf: &[u8], off: &mut usize) -> Result<u8> {
    if *off >= buf.len() {
        return Err(Error::Codec("truncated u8"));
    }
    let v = buf[*off];
    *off += 1;
    Ok(v)
}

fn read_slice<'a>(buf: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *off + n > buf.len() {
        return Err(Error::Codec("truncated payload"));
    }
    let s = &buf[*off..*off + n];
    *off += n;
    Ok(s)
}

fn utf8_owned(b: &[u8]) -> Result<String> {
    std::str::from_utf8(b)
        .map(|s| s.to_string())
        .map_err(|_| Error::Codec("non-utf8 string"))
}
