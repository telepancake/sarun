//! Streaming layer algebra — the two FUNDAMENTAL depot operations, done
//! directly over the canonical byte encoding (`crate::codec`), never over
//! an in-memory [`View`]/[`Node`] tree of the whole frame.
//!
//! - [`compose_stream`] — Function A. Reads two canonical layer byte
//!   streams `a` and `b`; writes the byte stream of `compose(a, b)`: the
//!   layer of `b` overlaid on the layer of `a`. Byte-for-byte identical to
//!   `codec::encode(compose(decode(a), decode(b)))`.
//! - [`diff_stream`] — Function B (the reverse). Reads two canonical
//!   **full-state** layer byte streams `a` and `b` (each a positive
//!   `diff(None, view)` record); writes the delta layer that turns the
//!   first into the second: `compose(a, diff_stream(a, b))` resolves to
//!   the same view as `b`. Byte-for-byte identical to
//!   `codec::encode(diff(view(a), view(b)))`.
//!
//! # Why streaming
//!
//! A depot full-state frame is tens of millions of objects. Rebuilding it
//! as a `View` on every commit — a `BTreeMap<Name, Arc<View>>` per node —
//! is tens of gigabytes of allocator overhead for data that is a couple of
//! compact gigabytes on the wire. These two functions never do that. Both
//! canonical encodings are a preorder walk whose children are sorted at
//! every level (`codec` enforces it on decode), so the operation is a
//! lockstep merge-walk of two sorted byte streams:
//!
//! - one-sided children are **copied verbatim** (a raw byte-range splice —
//!   no decode, no re-encode);
//! - two-sided children **recurse**;
//! - output is appended **strictly forward**, and both input cursors
//!   advance **monotonically** — so an mmap driver can `MADV_DONTNEED` the
//!   consumed prefix of each input and never doubles memory.
//!
//! Per-node working set is `O(fan-out)` (names + byte-ranges, not
//! subtrees); recursion is `O(depth)`. The only place a bounded subtree is
//! briefly decoded is `harden` under a tombstone-recreate or an opaque
//! mask (a small, rare corner) — reusing the exact in-memory `harden` so
//! the result cannot drift from the reference semantics.
//!
//! # The child-count property
//!
//! The canonical format prefixes children with a count, so the merge must
//! know how many children survive *before* it emits them. It does, without
//! composing them first: **`compose` of two non-identity, non-tombstone,
//! non-backdrop nodes is never the identity delta** (a one-sided
//! non-identity child always survives the merge, so the recursion can
//! never bottom out in identity). Thus every two-sided child survives
//! compose, and every one-sided child's fate is a facet-only decision. For
//! `diff`, two-sided children are pruned exactly when their byte-ranges are
//! equal (positive full-state form is a bijection view⇔bytes, so
//! equal-bytes ⇔ equal-view ⇔ identity diff) — a cheap `==` on the ranges,
//! the streaming twin of `diff`'s `Arc::ptr_eq` fast path.

use crate::codec::{
    self, put_bytes, put_varint, BLOB_KEEP, BLOB_MASK, BLOB_REMOVE, BLOB_SET, FLAG_ATTRS,
    FLAG_BACKDROP, FLAG_OPAQUE, FLAG_TOMBSTONE, KNOWN_FLAGS,
};
use crate::{harden, Presence};

pub use codec::DecodeError;

/// The canonical encoding of the identity delta (`Node::keep()`): flags 0,
/// zero children. `compose` prunes a child that reduces to this; the
/// streaming merge recognizes it as two bytes without decoding.
const ID: &[u8] = &[0, 0];

// -------------------------------------------------------------- cursor

/// A forward-only reader over one canonical byte stream. Never seeks back;
/// slices it returns borrow the input, so verbatim copies are splices.
struct Cur<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cur { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        let b = *self.buf.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
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

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.buf.len() - self.pos {
            return Err(DecodeError::BadLength(n as u64));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// A length-prefixed byte slice (name, blob, attr key/value).
    fn slice(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.varint()? as usize;
        self.take(n)
    }
}

/// One node's facets, parsed with the cursor left at its first child. The
/// subtree (`child_count` children) is still in the stream.
struct Head<'a> {
    /// The whole node is a tombstone (a single flag byte, no children).
    tombstone: bool,
    blob: Blob<'a>,
    opaque: bool,
    backdrop: bool,
    /// The raw encoded attrs block (count varint + entries), copyable
    /// verbatim; `None` = inherit (attrs bit clear).
    attrs: Option<&'a [u8]>,
    child_count: u64,
}

#[derive(Clone, Copy)]
enum Blob<'a> {
    Keep,
    Set(&'a [u8]),
    Remove,
}

/// Read one node's facets, leaving `cur` at the first child.
fn read_head<'a>(cur: &mut Cur<'a>) -> Result<Head<'a>, DecodeError> {
    let flags = cur.u8()?;
    if flags & !KNOWN_FLAGS != 0 {
        return Err(DecodeError::BadFlags(flags));
    }
    if flags & FLAG_TOMBSTONE != 0 {
        if flags != FLAG_TOMBSTONE {
            return Err(DecodeError::BadFlags(flags));
        }
        return Ok(Head {
            tombstone: true,
            blob: Blob::Keep,
            opaque: false,
            backdrop: false,
            attrs: None,
            child_count: 0,
        });
    }
    let blob = match flags & BLOB_MASK {
        BLOB_KEEP => Blob::Keep,
        BLOB_SET => Blob::Set(cur.slice()?),
        BLOB_REMOVE => Blob::Remove,
        _ => return Err(DecodeError::BadFlags(flags)),
    };
    let attrs = if flags & FLAG_ATTRS != 0 {
        let start = cur.pos;
        let count = cur.varint()?;
        for _ in 0..count {
            cur.slice()?; // key
            cur.slice()?; // value
        }
        Some(&cur.buf[start..cur.pos])
    } else {
        None
    };
    let child_count = cur.varint()?;
    Ok(Head {
        tombstone: false,
        blob,
        opaque: flags & FLAG_OPAQUE != 0,
        backdrop: flags & FLAG_BACKDROP != 0,
        attrs,
        child_count,
    })
}

/// Advance `cur` past a whole node subtree (facets + all descendants).
fn skip_node(cur: &mut Cur) -> Result<(), DecodeError> {
    let head = read_head(cur)?;
    for _ in 0..head.child_count {
        cur.slice()?; // name
        skip_node(cur)?;
    }
    Ok(())
}

/// The `(name, subtree-bytes)` list of a node's children, in the canonical
/// ascending order — names and byte-ranges only, never a decoded subtree.
fn collect_children<'a>(
    cur: &mut Cur<'a>,
    count: u64,
) -> Result<Vec<(&'a [u8], &'a [u8])>, DecodeError> {
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = cur.slice()?;
        let start = cur.pos;
        skip_node(cur)?;
        out.push((name, &cur.buf[start..cur.pos]));
    }
    Ok(out)
}

// -------------------------------------------------------- flag emission

/// Emit a live node's facet bytes (flags + optional blob + optional attrs).
/// The caller writes the child count and children next.
fn emit_facets(out: &mut Vec<u8>, blob: Blob, opaque: bool, backdrop: bool, attrs: Option<&[u8]>) {
    let mut flags = match blob {
        Blob::Keep => BLOB_KEEP,
        Blob::Set(_) => BLOB_SET,
        Blob::Remove => BLOB_REMOVE,
    };
    if opaque {
        flags |= FLAG_OPAQUE;
    }
    if attrs.is_some() {
        flags |= FLAG_ATTRS;
    }
    if backdrop {
        flags |= FLAG_BACKDROP;
    }
    out.push(flags);
    if let Blob::Set(bytes) = blob {
        put_bytes(out, bytes);
    }
    if let Some(a) = attrs {
        out.extend_from_slice(a); // raw: count varint + entries
    }
}

// ============================================================= compose

/// Function A: overlay the layer of `b` on the layer of `a`, entirely over
/// the canonical byte encoding. Output equals
/// `codec::encode(&compose(&decode(a)?, &decode(b)?))`, byte for byte.
pub fn compose_stream(a: &[u8], b: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
    let mut ca = Cur::new(a);
    let mut cb = Cur::new(b);
    compose_node(&mut ca, &mut cb, out)
}

/// One node of `compose`, mirroring `crate::compose_node` branch for
/// branch. On return both cursors sit just past their node.
fn compose_node(a: &mut Cur, b: &mut Cur, out: &mut Vec<u8>) -> Result<(), DecodeError> {
    let b_start = b.pos;
    let bh = read_head(b)?;

    // `b` tombstone wins outright: emit a tombstone, drop both subtrees.
    if bh.tombstone {
        skip_node(a)?;
        out.push(FLAG_TOMBSTONE);
        return Ok(());
    }
    // `b` backdrop re-bases this name: nothing recorded below survives.
    // Emit `b` verbatim, drop `a`.
    if bh.backdrop {
        for _ in 0..bh.child_count {
            b.slice()?;
            skip_node(b)?;
        }
        skip_node(a)?;
        out.extend_from_slice(&b.buf[b_start..b.pos]);
        return Ok(());
    }

    let ah = read_head(a)?;

    // Recreate over a whiteout: harden every inherit in `b`. `a` is a
    // tombstone (no children); the recreated subtree of `b` is bounded, so
    // decode it and reuse the reference `harden` verbatim.
    if ah.tombstone {
        for _ in 0..bh.child_count {
            b.slice()?;
            skip_node(b)?;
        }
        let (bnode, _) = codec::decode_node(b.buf, b_start)?;
        codec::encode_node(out, &harden(&bnode));
        return Ok(());
    }

    // Normal compose. Facets: blob is `b`'s unless `b` keeps (then `a`'s);
    // attrs are `b`'s unless `b` inherits; opaque is `b`'s mask or `a`'s;
    // anchor follows `a`.
    let blob = match bh.blob {
        Blob::Keep => ah.blob,
        other => other,
    };
    let attrs = bh.attrs.or(ah.attrs);
    let a_children = collect_children(a, ah.child_count)?;
    let b_children = collect_children(b, bh.child_count)?;

    if bh.opaque {
        // `a`'s children are masked; only `b`'s survive, each hardened —
        // behind the mask a child inherits nothing. Non-materializing ones
        // (harden → tombstone) are dropped: the opaque already masks them.
        let mut kids: Vec<(&[u8], Vec<u8>)> = Vec::new();
        for (name, bsub) in &b_children {
            let (bnode, _) = codec::decode_node(bsub, 0)?;
            let h = harden(&bnode);
            if h.presence == Presence::Live {
                let mut v = Vec::new();
                codec::encode_node(&mut v, &h);
                kids.push((name, v));
            }
        }
        emit_facets(out, blob, true, ah.backdrop, attrs);
        put_varint(out, kids.len() as u64);
        for (name, bytes) in kids {
            put_bytes(out, name);
            out.extend_from_slice(&bytes);
        }
        return Ok(());
    }

    // Not opaque: merge `a` and `b` children by name. One-sided `a`
    // children pass through verbatim; one-sided `b` children pass through
    // (or, behind `a`'s mask, are hardened); two-sided children recurse.
    // Every survivor's fate is known here, so the count is exact.
    enum Src<'a> {
        CopyA(&'a [u8]),
        CopyB(&'a [u8]),
        Both(&'a [u8], &'a [u8]),
        Owned(Vec<u8>),
    }
    let mut items: Vec<(&[u8], Src)> = Vec::new();
    let mut ia = a_children.iter().peekable();
    let mut ib = b_children.iter().peekable();
    loop {
        match (ia.peek(), ib.peek()) {
            (Some((an, asub)), Some((bn, bsub))) => {
                use std::cmp::Ordering::*;
                match an.cmp(bn) {
                    Less => {
                        items.push((an, Src::CopyA(asub)));
                        ia.next();
                    }
                    Greater => {
                        push_compose_b_only(&mut items, bn, bsub, ah.opaque)?;
                        ib.next();
                    }
                    Equal => {
                        // compose(ac, bc) is identity only when BOTH sides
                        // are already identity (a one-sided non-identity
                        // survives); prune exactly that, matching
                        // `compose_node`'s `if !c.is_identity()`.
                        if !(*asub == ID && *bsub == ID) {
                            items.push((an, Src::Both(asub, bsub)));
                        }
                        ia.next();
                        ib.next();
                    }
                }
            }
            (Some((an, asub)), None) => {
                items.push((an, Src::CopyA(asub)));
                ia.next();
            }
            (None, Some((bn, bsub))) => {
                push_compose_b_only(&mut items, bn, bsub, ah.opaque)?;
                ib.next();
            }
            (None, None) => break,
        }
    }

    emit_facets(out, blob, ah.opaque, ah.backdrop, attrs);
    put_varint(out, items.len() as u64);
    for (name, src) in items {
        put_bytes(out, name);
        match src {
            Src::CopyA(s) | Src::CopyB(s) => out.extend_from_slice(s),
            Src::Owned(v) => out.extend_from_slice(&v),
            Src::Both(asub, bsub) => {
                compose_node(&mut Cur::new(asub), &mut Cur::new(bsub), out)?;
            }
        }
    }
    return Ok(());

    // A `b`-only child in the non-opaque branch. Behind `a`'s opaque mask
    // it inherits nothing and must be hardened (dropped if it materializes
    // nothing); otherwise it passes through verbatim — a canonical layer
    // never stores an identity child, so it always survives.
    fn push_compose_b_only<'a>(
        items: &mut Vec<(&'a [u8], Src<'a>)>,
        name: &'a [u8],
        bsub: &'a [u8],
        a_opaque: bool,
    ) -> Result<(), DecodeError> {
        if a_opaque {
            let (bnode, _) = codec::decode_node(bsub, 0)?;
            let h = harden(&bnode);
            if h.presence == Presence::Live {
                let mut v = Vec::new();
                codec::encode_node(&mut v, &h);
                items.push((name, Src::Owned(v)));
            }
        } else if bsub != ID {
            // `compose_node` drops a `b`-only identity child.
            items.push((name, Src::CopyB(bsub)));
        }
        Ok(())
    }
}

// ================================================================ diff

/// Function B: the delta layer that turns full-state `a` into full-state
/// `b`. Both inputs MUST be canonical full-state records (a positive
/// `diff(None, view)` form: live nodes, `Set`/absent blobs, no tombstones,
/// no opaque, no backdrop). Output equals
/// `codec::encode(&diff(view(a), view(b)))`, byte for byte, where
/// `view(x) = apply(None, decode(x))`.
pub fn diff_stream(a: &[u8], b: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
    let mut ca = Cur::new(a);
    let mut cb = Cur::new(b);
    diff_node(&mut ca, &mut cb, out)
}

/// Normalize an attrs block to "the view's attrs": an absent block and an
/// explicitly-empty one both mean "no attrs" (`diff` compares view attrs,
/// where `None`/`{}` are indistinguishable).
fn norm_attrs(attrs: Option<&[u8]>) -> Option<&[u8]> {
    match attrs {
        // count 0 (the empty-dir existence witness) reads as no attrs.
        Some([0]) => None,
        other => other,
    }
}

/// One node of `diff(Some(view(a)), Some(view(b)))`. Both cursors sit on a
/// present node (the parent only recurses on two-sided survivors, and the
/// roots are present); on return both sit just past their node.
fn diff_node(a: &mut Cur, b: &mut Cur, out: &mut Vec<u8>) -> Result<(), DecodeError> {
    let ah = read_head(a)?;
    let bh = read_head(b)?;

    // A full-state node's view-blob is exactly its `Set` payload.
    let va_blob = match ah.blob {
        Blob::Set(x) => Some(x),
        _ => None,
    };
    let vb_blob = match bh.blob {
        Blob::Set(x) => Some(x),
        _ => None,
    };
    let dblob = match (va_blob, vb_blob) {
        (Some(o), Some(n)) if o == n => Blob::Keep,
        (_, Some(n)) => Blob::Set(n),
        (Some(_), None) => Blob::Remove,
        (None, None) => Blob::Keep,
    };

    let va_attrs = norm_attrs(ah.attrs);
    let vb_attrs = norm_attrs(bh.attrs);
    // `Inherit` = attrs unchanged; `Set` = replace with b's (raw); `Empty`
    // = replace with the empty map (attrs bit + count 0).
    enum DAttrs<'a> {
        Inherit,
        Set(&'a [u8]),
        Empty,
    }
    let mut dattrs = if va_attrs == vb_attrs {
        DAttrs::Inherit
    } else {
        match vb_attrs {
            Some(raw) => DAttrs::Set(raw),
            None => DAttrs::Empty,
        }
    };

    let a_children = collect_children(a, ah.child_count)?;
    let b_children = collect_children(b, bh.child_count)?;

    // Merge: `a`-only → per-entry tombstone; `b`-only → the child's
    // full-state bytes verbatim (they already ARE `diff(None, child)`);
    // two-sided → prune if byte-equal (equal view), else recurse.
    enum Src<'a> {
        Tomb,
        CopyB(&'a [u8]),
        Both(&'a [u8], &'a [u8]),
    }
    let mut items: Vec<(&[u8], Src)> = Vec::new();
    let mut ia = a_children.iter().peekable();
    let mut ib = b_children.iter().peekable();
    loop {
        match (ia.peek(), ib.peek()) {
            (Some((an, asub)), Some((bn, bsub))) => {
                use std::cmp::Ordering::*;
                match an.cmp(bn) {
                    Less => {
                        items.push((an, Src::Tomb));
                        ia.next();
                    }
                    Greater => {
                        items.push((bn, Src::CopyB(bsub)));
                        ib.next();
                    }
                    Equal => {
                        if asub != bsub {
                            items.push((an, Src::Both(asub, bsub)));
                        }
                        ia.next();
                        ib.next();
                    }
                }
            }
            (Some((an, _)), None) => {
                items.push((an, Src::Tomb));
                ia.next();
            }
            (None, Some((bn, bsub))) => {
                items.push((bn, Src::CopyB(bsub)));
                ib.next();
            }
            (None, None) => break,
        }
    }

    // Existence witness: `b`'s view is an explicitly-present empty node and
    // the natural delta asserts nothing of its own — force the minimal
    // assertion (replace attrs with b's empty map) unless `a`'s view was
    // already an empty present node (existence carried).
    let b_empty_view = vb_blob.is_none() && vb_attrs.is_none() && b_children.is_empty();
    let a_empty_view = va_blob.is_none() && va_attrs.is_none() && a_children.is_empty();
    let asserts_self = !matches!(dblob, Blob::Keep) || !matches!(dattrs, DAttrs::Inherit);
    if b_empty_view && !asserts_self && !a_empty_view {
        dattrs = DAttrs::Empty;
    }

    // Emit facets (diff never produces opaque or backdrop).
    let attrs_raw: Option<&[u8]> = match dattrs {
        DAttrs::Inherit => None,
        DAttrs::Set(raw) => Some(raw),
        DAttrs::Empty => Some(&[0]),
    };
    emit_facets(out, dblob, false, false, attrs_raw);
    put_varint(out, items.len() as u64);
    for (name, src) in items {
        put_bytes(out, name);
        match src {
            Src::Tomb => out.push(FLAG_TOMBSTONE),
            Src::CopyB(s) => out.extend_from_slice(s),
            Src::Both(asub, bsub) => {
                diff_node(&mut Cur::new(asub), &mut Cur::new(bsub), out)?;
            }
        }
    }
    Ok(())
}
