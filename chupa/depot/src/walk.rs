//! A public, allocation-free forward cursor over the canonical codec bytes
//! (`crate::codec`), for building external iterators that need content byte
//! ranges without materializing a [`crate::Layer`].
//!
//! The git-mirror layer iterator walks a hundreds-of-MB frame in a single
//! forward pass, yielding each node's blob as a `&[u8]` slice INTO the input
//! (an mmap), never copying. This cursor is the substrate: read a node header
//! ([`Cursor::node`]), then its children by name ([`Cursor::name`] then
//! recurse), or skip a whole subtree ([`Cursor::skip`]). Children come in the
//! codec's stored bytewise order — which for the mirror is the authoritative
//! container order (the big side is never re-sorted).

use crate::codec::{
    BLOB_KEEP, BLOB_MASK, BLOB_REMOVE, BLOB_SET, FLAG_ATTRS, FLAG_BACKDROP, FLAG_OPAQUE,
    FLAG_TOMBSTONE, KNOWN_FLAGS,
};
pub use crate::codec::DecodeError;

/// One node's facets, parsed with the cursor left at its first child.
#[derive(Debug)]
pub struct Node<'a> {
    /// A whole-node deletion (a single flag byte, no children).
    pub tombstone: bool,
    /// Backdrop-anchored (a removal hole, or a restoration).
    pub backdrop: bool,
    /// Masks lower children (opaque). The mirror does not use it, but the
    /// grammar carries it.
    pub opaque: bool,
    /// The node's blob (`Set`) as a slice into the input — the file content
    /// for a variant node. `None` for `Keep`/`Remove` (a directory keeps no
    /// blob).
    pub blob: Option<&'a [u8]>,
    /// Number of children following, in bytewise order.
    pub child_count: u64,
}

/// Forward cursor over canonical codec bytes. Never seeks back; every slice
/// it hands out borrows the input.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    /// Current byte offset — used to record a subtree's byte range.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The whole underlying buffer (to turn recorded ranges into slices).
    pub fn buf(&self) -> &'a [u8] {
        self.buf
    }

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

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.buf.len() - self.pos {
            return Err(DecodeError::BadLength(n as u64));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// A length-prefixed slice (blob, name, attr key/value).
    fn slice(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.varint()? as usize;
        self.take(n)
    }

    /// Read one node header, leaving the cursor at its first child.
    pub fn node(&mut self) -> Result<Node<'a>, DecodeError> {
        let flags = self.byte()?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(DecodeError::BadFlags(flags));
        }
        if flags & FLAG_TOMBSTONE != 0 {
            if flags != FLAG_TOMBSTONE {
                return Err(DecodeError::BadFlags(flags));
            }
            return Ok(Node {
                tombstone: true,
                backdrop: false,
                opaque: false,
                blob: None,
                child_count: 0,
            });
        }
        let blob = match flags & BLOB_MASK {
            BLOB_KEEP => None,
            BLOB_SET => Some(self.slice()?),
            BLOB_REMOVE => None,
            _ => return Err(DecodeError::BadFlags(flags)),
        };
        if flags & FLAG_ATTRS != 0 {
            // The mirror never sets attrs, but skip them for a general reader.
            let count = self.varint()?;
            for _ in 0..count {
                self.slice()?; // key
                self.slice()?; // value
            }
        }
        let child_count = self.varint()?;
        Ok(Node {
            tombstone: false,
            backdrop: flags & FLAG_BACKDROP != 0,
            opaque: flags & FLAG_OPAQUE != 0,
            blob,
            child_count,
        })
    }

    /// Read the next child's name, leaving the cursor at that child's node.
    pub fn name(&mut self) -> Result<&'a [u8], DecodeError> {
        self.slice()
    }

    /// Skip a whole node subtree (header + all descendants).
    pub fn skip(&mut self) -> Result<(), DecodeError> {
        let node = self.node()?;
        for _ in 0..node.child_count {
            self.name()?;
            self.skip()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{codec, BlobOp, Layer, Node as DNode};

    fn leaf(content: &[u8]) -> DNode {
        DNode { blob: BlobOp::Set(content.into()), ..DNode::keep() }
    }
    fn dir(kids: Vec<(&[u8], DNode)>) -> DNode {
        let mut n = DNode::keep();
        for (k, v) in kids {
            n.children.insert(k.to_vec(), v);
        }
        n
    }

    /// Walk the cursor over a known layer and collect (path, content); it must
    /// match a manual preorder, in bytewise child order, with content slices
    /// borrowing the input.
    #[test]
    fn walks_in_bytewise_order_with_content_ranges() {
        let root = dir(vec![
            (b"a", leaf(b"AAA")),
            (b"dir", dir(vec![(b"x", leaf(b"X")), (b"y", leaf(b"YY"))])),
            (b"z", leaf(b"zzzz")),
        ]);
        let bytes = codec::encode(&Layer { root });

        fn go(cur: &mut Cursor, prefix: &[u8], out: &mut Vec<(Vec<u8>, Vec<u8>)>) {
            let node = cur.node().unwrap();
            if let Some(b) = node.blob {
                out.push((prefix.to_vec(), b.to_vec()));
            }
            for _ in 0..node.child_count {
                let name = cur.name().unwrap();
                let mut p = prefix.to_vec();
                p.push(b'/');
                p.extend_from_slice(name);
                go(cur, &p, out);
            }
        }
        let mut out = Vec::new();
        go(&mut Cursor::new(&bytes), b"", &mut out);
        assert_eq!(
            out,
            vec![
                (b"/a".to_vec(), b"AAA".to_vec()),
                (b"/dir/x".to_vec(), b"X".to_vec()),
                (b"/dir/y".to_vec(), b"YY".to_vec()),
                (b"/z".to_vec(), b"zzzz".to_vec()),
            ]
        );
    }

    #[test]
    fn skip_lands_past_the_subtree() {
        let root = dir(vec![(b"a", dir(vec![(b"deep", leaf(b"D"))])), (b"b", leaf(b"B"))]);
        let bytes = codec::encode(&Layer { root });
        let mut cur = Cursor::new(&bytes);
        let r = cur.node().unwrap();
        assert_eq!(r.child_count, 2);
        // First child "a": skip its whole subtree, then read "b".
        assert_eq!(cur.name().unwrap(), b"a");
        cur.skip().unwrap();
        assert_eq!(cur.name().unwrap(), b"b");
        let b = cur.node().unwrap();
        assert_eq!(b.blob, Some(&b"B"[..]));
    }

    #[test]
    fn empty_and_tombstone() {
        // A tombstone child parses as a childless deletion.
        let mut root = DNode::keep();
        root.children.insert(b"gone".to_vec(), DNode::tombstone());
        let bytes = codec::encode(&Layer { root });
        let mut cur = Cursor::new(&bytes);
        let _ = cur.node().unwrap();
        assert_eq!(cur.name().unwrap(), b"gone");
        let t = cur.node().unwrap();
        assert!(t.tombstone && t.child_count == 0);
        // A pure hole (backdrop) is flagged.
        let mut r2 = DNode::keep();
        r2.children.insert(b"h".to_vec(), DNode::hole());
        let bytes2 = codec::encode(&Layer { root: r2 });
        let mut c2 = Cursor::new(&bytes2);
        let _ = c2.node().unwrap();
        assert_eq!(c2.name().unwrap(), b"h");
        assert!(c2.node().unwrap().backdrop);
    }
}
