//! Canonical layer encoding — DEPOT-DESIGN.md §4, the keystone.
//!
//! One deterministic, lossless, self-delimiting serialization of a
//! [`Layer`], consumed three ways: depot-to-depot transfer, the stream
//! variant's storage form, and the records a VBF chain stores
//! newest-first.
//!
//! Shape: a preorder walk of the delta tree. Every reference is
//! structural (nesting and order); per the implicit-id rule there are no
//! content hashes, no object ids, no offsets — nothing derived. Integers
//! are LEB128 varints; names sort bytewise (this variant's one
//! deterministic order), which the decoder enforces so that every layer
//! has exactly one encoding.
//!
//! Per node:
//!
//! ```text
//! flags: u8      bit 0  tombstone (all other bits must be 0)
//!                bits 1-2  blob op: 00 keep, 01 set, 10 remove
//!                bit 3  opaque
//!                bit 4  attrs present (replace) — else inherit
//!                bit 5  backdrop anchor (facets resolve against the
//!                       backdrop; a pure such node is a hole)
//! [blob]         if op = set: varint len, bytes
//! [attrs]        if present: varint count, then (varint len key,
//!                varint len value) per entry, keys strictly ascending
//! children       varint count, then (varint len name, node) per child,
//!                names strictly ascending
//! ```
//!
//! There is no magic number and no version byte: framing, versioning,
//! and integrity belong to whatever transport or store carries the
//! bytes (the stream variant, a VBF frame) — same division of labor as
//! the tiered-VBF design.

use std::collections::BTreeMap;

use crate::{Attrs, BlobOp, Layer, Name, Node, Presence};

const FLAG_TOMBSTONE: u8 = 1 << 0;
const BLOB_SHIFT: u32 = 1;
const BLOB_MASK: u8 = 0b11 << BLOB_SHIFT;
const BLOB_KEEP: u8 = 0b00 << BLOB_SHIFT;
const BLOB_SET: u8 = 0b01 << BLOB_SHIFT;
const BLOB_REMOVE: u8 = 0b10 << BLOB_SHIFT;
const FLAG_OPAQUE: u8 = 1 << 3;
const FLAG_ATTRS: u8 = 1 << 4;
/// Backdrop anchor (a pure such node is a hole). FORMAT MIGRATION NOTE:
/// bit 5 was added with the anchor axis; encodings written before it
/// decode unchanged (the bit reads as Lower), and no view-anchored
/// stores existed at the flag's introduction.
const FLAG_BACKDROP: u8 = 1 << 5;
const KNOWN_FLAGS: u8 =
    FLAG_TOMBSTONE | BLOB_MASK | FLAG_OPAQUE | FLAG_ATTRS | FLAG_BACKDROP;

/// Decode failure. The input is untrusted bytes (a transfer source, a
/// cold frame); every malformation is a structured error, never a panic.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Input ended mid-structure.
    Truncated,
    /// A varint ran past 10 bytes or overflowed u64.
    BadVarint,
    /// Unknown flag bits, a reserved blob-op value, or payload bits set
    /// on a tombstone.
    BadFlags(u8),
    /// A length prefix exceeds the remaining input.
    BadLength(u64),
    /// Attr keys or child names not strictly ascending — the input is
    /// not canonical (or not ours).
    NotCanonical,
    /// A layer root must be live.
    TombstoneRoot,
    /// Bytes left over after the root node.
    TrailingBytes(usize),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "input truncated"),
            DecodeError::BadVarint => write!(f, "malformed varint"),
            DecodeError::BadFlags(b) => write!(f, "bad flag byte {b:#04x}"),
            DecodeError::BadLength(n) => write!(f, "length {n} exceeds input"),
            DecodeError::NotCanonical => write!(f, "names/keys not strictly ascending"),
            DecodeError::TombstoneRoot => write!(f, "layer root is a tombstone"),
            DecodeError::TrailingBytes(n) => write!(f, "{n} trailing bytes"),
        }
    }
}

impl std::error::Error for DecodeError {}

// --------------------------------------------------------------- encode

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
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

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn encode_node(out: &mut Vec<u8>, node: &Node) {
    if node.presence == Presence::Tombstone {
        // A tombstone carries nothing: its other fields are meaningless
        // by the model, and the canonical form does not encode them.
        out.push(FLAG_TOMBSTONE);
        return;
    }
    let mut flags = match &node.blob {
        BlobOp::Keep => BLOB_KEEP,
        BlobOp::Set(_) => BLOB_SET,
        BlobOp::Remove => BLOB_REMOVE,
    };
    if node.opaque {
        flags |= FLAG_OPAQUE;
    }
    if node.attrs.is_some() {
        flags |= FLAG_ATTRS;
    }
    if node.anchor == crate::Anchor::Backdrop {
        flags |= FLAG_BACKDROP;
    }
    out.push(flags);
    if let BlobOp::Set(bytes) = &node.blob {
        put_bytes(out, bytes);
    }
    if let Some(attrs) = &node.attrs {
        put_varint(out, attrs.len() as u64);
        for (k, v) in attrs {
            put_bytes(out, k);
            put_bytes(out, v);
        }
    }
    put_varint(out, node.children.len() as u64);
    for (name, child) in &node.children {
        put_bytes(out, name);
        encode_node(out, child);
    }
}

/// Serialize a layer to its one canonical byte form.
pub fn encode(layer: &Layer) -> Vec<u8> {
    let mut out = Vec::new();
    encode_node(&mut out, &layer.root);
    out
}

// --------------------------------------------------------------- decode

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let b = *self.buf.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            let low = (b & 0x7f) as u64;
            if shift == 63 && low > 1 {
                return Err(DecodeError::BadVarint);
            }
            v |= low << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::BadVarint)
    }

    fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.varint()?;
        let remaining = (self.buf.len() - self.pos) as u64;
        if len > remaining {
            return Err(DecodeError::BadLength(len));
        }
        let s = &self.buf[self.pos..self.pos + len as usize];
        self.pos += len as usize;
        Ok(s)
    }

    fn node(&mut self) -> Result<Node, DecodeError> {
        let flags = self.byte()?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(DecodeError::BadFlags(flags));
        }
        if flags & FLAG_TOMBSTONE != 0 {
            if flags != FLAG_TOMBSTONE {
                return Err(DecodeError::BadFlags(flags));
            }
            return Ok(Node::tombstone());
        }
        let blob = match flags & BLOB_MASK {
            BLOB_KEEP => BlobOp::Keep,
            BLOB_SET => BlobOp::Set(self.bytes()?.to_vec()),
            BLOB_REMOVE => BlobOp::Remove,
            _ => return Err(DecodeError::BadFlags(flags)),
        };
        let attrs = if flags & FLAG_ATTRS != 0 {
            let count = self.varint()?;
            let mut map = Attrs::new();
            let mut prev: Option<Name> = None;
            for _ in 0..count {
                let k = self.bytes()?.to_vec();
                if prev.as_ref().is_some_and(|p| *p >= k) {
                    return Err(DecodeError::NotCanonical);
                }
                let v = self.bytes()?.to_vec();
                prev = Some(k.clone());
                map.insert(k, v);
            }
            Some(map)
        } else {
            None
        };
        let count = self.varint()?;
        let mut children: BTreeMap<Name, Node> = BTreeMap::new();
        let mut prev: Option<Name> = None;
        for _ in 0..count {
            let name = self.bytes()?.to_vec();
            if prev.as_ref().is_some_and(|p| *p >= name) {
                return Err(DecodeError::NotCanonical);
            }
            let child = self.node()?;
            prev = Some(name.clone());
            children.insert(name, child);
        }
        Ok(Node {
            presence: Presence::Live,
            blob,
            opaque: flags & FLAG_OPAQUE != 0,
            attrs,
            anchor: if flags & FLAG_BACKDROP != 0 {
                crate::Anchor::Backdrop
            } else {
                crate::Anchor::Lower
            },
            children,
        })
    }
}

/// Decode one canonical layer, consuming the whole input.
pub fn decode(buf: &[u8]) -> Result<Layer, DecodeError> {
    let mut r = Reader { buf, pos: 0 };
    let root = r.node()?;
    if root.presence == Presence::Tombstone {
        return Err(DecodeError::TombstoneRoot);
    }
    if r.pos != buf.len() {
        return Err(DecodeError::TrailingBytes(buf.len() - r.pos));
    }
    Ok(Layer { root })
}
