//! Canonical-encoding tests: exact golden bytes (format stability),
//! round-trips over randomized layers, determinism, and rejection of
//! malformed or non-canonical input.

mod common;

use std::collections::BTreeMap;

use common::{random_layer, random_layer_anchored, Rng};
use depot::codec::{decode, encode, DecodeError};
use depot::{Attrs, BlobOp, Layer, Node, Presence};

// ---------------------------------------------------------- golden bytes

/// Pins the on-wire format. If this test breaks, the format changed —
/// which is an event with migration consequences, not a refactor.
#[test]
fn golden_encoding() {
    // root: live, blob keep, attrs {"m": "1"}, one child.
    // child "f": live, blob set "hi", opaque, no attrs, one tombstone
    // child "t".
    let child = Node {
        presence: Presence::Live,
        blob: BlobOp::Set(b"hi".to_vec()),
        opaque: true,
        attrs: None,
        anchor: depot::Anchor::Lower,
        children: BTreeMap::from([(b"t".to_vec(), Node::tombstone())]),
    };
    let root = Node {
        presence: Presence::Live,
        blob: BlobOp::Keep,
        opaque: false,
        attrs: Some(Attrs::from([(b"m".to_vec(), b"1".to_vec())])),
        anchor: depot::Anchor::Lower,
        children: BTreeMap::from([(b"f".to_vec(), child)]),
    };
    let layer = Layer { root };
    let bytes = encode(&layer);
    assert_eq!(
        bytes,
        vec![
            0b0001_0000, // root flags: attrs present
            1,           // attrs count
            1, b'm',     // key "m"
            1, b'1',     // value "1"
            1,           // children count
            1, b'f',     // child name "f"
            0b0000_1010, // child flags: blob set | opaque
            2, b'h', b'i', // blob "hi"
            1,           // children count
            1, b't',     // grandchild name "t"
            0b0000_0001, // tombstone: exactly one byte, nothing follows
        ],
        "golden bytes changed — the wire format moved"
    );
    assert_eq!(decode(&bytes).unwrap(), layer);
}

// ------------------------------------------------------------ round-trip

#[test]
fn roundtrip_empty_layer() {
    let l = Layer::empty();
    assert_eq!(decode(&encode(&l)).unwrap(), l);
    assert_eq!(encode(&l), vec![0u8, 0u8]); // flags, zero children
}

#[test]
fn roundtrip_randomized_and_deterministic() {
    for seed in 1..500u64 {
        let mut rng = Rng(seed);
        let layer = if seed % 2 == 0 {
            random_layer_anchored(&mut rng) // holes / backdrop anchors
        } else {
            random_layer(&mut rng)
        };
        let bytes = encode(&layer);
        let back = decode(&bytes).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(back, layer, "round-trip mismatch, seed {seed}");
        // One layer, one encoding: re-encoding the decoded value is
        // byte-identical.
        assert_eq!(encode(&back), bytes, "non-deterministic encoding, seed {seed}");
    }
}

#[test]
fn tombstone_encodes_one_byte_regardless_of_payload() {
    // The model says a tombstone's other fields are meaningless; the
    // canonical form must not leak them.
    let mut t = Node::tombstone();
    t.blob = BlobOp::Set(b"garbage".to_vec());
    t.opaque = true;
    t.attrs = Some(Attrs::from([(b"x".to_vec(), b"y".to_vec())]));
    let clean = Layer {
        root: Node {
            children: BTreeMap::from([(b"a".to_vec(), Node::tombstone())]),
            ..Node::keep()
        },
    };
    let dirty = Layer {
        root: Node {
            children: BTreeMap::from([(b"a".to_vec(), t)]),
            ..Node::keep()
        },
    };
    assert_eq!(encode(&dirty), encode(&clean));
}

/// A hole is one flag byte + zero children — and bit 5 is pinned.
#[test]
fn golden_hole() {
    let layer = Layer {
        root: Node {
            children: BTreeMap::from([(b"h".to_vec(), Node::hole())]),
            ..Node::keep()
        },
    };
    assert_eq!(
        encode(&layer),
        vec![
            0,           // root flags
            1,           // children count
            1, b'h',     // child name
            0b0010_0000, // backdrop anchor (hole)
            0,           // its children count
        ]
    );
    assert_eq!(decode(&encode(&layer)).unwrap(), layer);
}

// -------------------------------------------------------------- rejection

#[test]
fn rejects_malformed_input() {
    // Truncated: flags promise a blob that isn't there.
    assert_eq!(decode(&[0b0000_0010]), Err(DecodeError::Truncated));
    // Empty input.
    assert_eq!(decode(&[]), Err(DecodeError::Truncated));
    // Unknown flag bit.
    assert_eq!(decode(&[0b1000_0000, 0]), Err(DecodeError::BadFlags(0b1000_0000)));
    // Reserved blob-op value 0b11.
    assert_eq!(decode(&[0b0000_0110, 0]), Err(DecodeError::BadFlags(0b0000_0110)));
    // Tombstone with extra bits set.
    assert_eq!(decode(&[0b0000_1001]), Err(DecodeError::BadFlags(0b0000_1001)));
    // Tombstone as root.
    assert_eq!(decode(&[0b0000_0001]), Err(DecodeError::TombstoneRoot));
    // Length prefix past end of input.
    assert_eq!(decode(&[0b0000_0010, 100]), Err(DecodeError::BadLength(100)));
    // Trailing bytes after a valid root.
    assert_eq!(decode(&[0, 0, 0xff]), Err(DecodeError::TrailingBytes(1)));
    // Varint longer than u64.
    let mut long = vec![0b0000_0010];
    long.extend_from_slice(&[0xff; 10]);
    assert_eq!(decode(&long), Err(DecodeError::BadVarint));
}

#[test]
fn rejects_non_canonical_order() {
    // Two children encoded out of order: "b" before "a".
    let bytes = vec![
        0,           // root flags
        2,           // children count
        1, b'b', 0, 0, // child "b": keep, no children
        1, b'a', 0, 0, // child "a" — out of order
    ];
    assert_eq!(decode(&bytes), Err(DecodeError::NotCanonical));
    // Duplicate names are equally non-canonical.
    let dup = vec![0, 2, 1, b'a', 0, 0, 1, b'a', 0, 0];
    assert_eq!(decode(&dup), Err(DecodeError::NotCanonical));
    // Attr keys out of order.
    let attrs = vec![
        0b0001_0000, // attrs present
        2,           // attr count
        1, b'z', 1, b'1',
        1, b'a', 1, b'2',
        0,           // children count
    ];
    assert_eq!(decode(&attrs), Err(DecodeError::NotCanonical));
}
