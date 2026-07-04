// Shared test helpers: a tiny deterministic xorshift and a random
// delta-tree generator, used by both the algebra-law and codec tests.

use std::collections::BTreeMap;

use depot::{Attrs, BlobOp, Layer, Node, Presence};

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub const NAMES: &[&[u8]] = &[b"a", b"b", b"c", b"d"];

pub fn random_node(rng: &mut Rng, depth: u32) -> Node {
    if rng.below(8) == 0 {
        return Node::tombstone();
    }
    let blob = match rng.below(4) {
        0 => BlobOp::Keep,
        1 => BlobOp::Remove,
        _ => BlobOp::Set(vec![b'v', rng.below(3) as u8 + b'0']),
    };
    let attrs = match rng.below(3) {
        0 => None,
        1 => Some(Attrs::new()),
        _ => Some(Attrs::from([(b"m".to_vec(), vec![rng.below(3) as u8 + b'0'])])),
    };
    let opaque = rng.below(5) == 0;
    let mut children = BTreeMap::new();
    if depth > 0 {
        for name in NAMES {
            if rng.below(2) == 0 {
                children.insert(name.to_vec(), random_node(rng, depth - 1));
            }
        }
    }
    Node { presence: Presence::Live, blob, opaque, attrs, children }
}

pub fn random_layer(rng: &mut Rng) -> Layer {
    let mut root = random_node(rng, 3);
    root.presence = Presence::Live; // layer roots are live
    Layer { root }
}
