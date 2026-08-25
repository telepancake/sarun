//! Stream-variant tests: transfer round-trips, malformed-input handling,
//! and the §8 anti-sabotage storage assertion (near-identical layers must
//! cost ~the delta each, not ~the layer).

mod common {
    include!("../../depot/tests/common/mod.rs");
}

use std::collections::BTreeMap;

use common::{random_layer, Rng};
use depot::variant::{transfer, LayerSink, LayerSource};
use depot::{Attrs, BlobOp, Layer, Node};
use depot_stream::{Error, StreamReader, StreamWriter};

/// Trivial in-memory depot: the simplest LayerSource/LayerSink pair,
/// used as the "other side" of transfers.
#[derive(Default)]
struct MemDepot {
    layers: Vec<Layer>,
    cursor: usize,
}

impl LayerSink for MemDepot {
    type Err = Error; // never fails
    fn put_layer(&mut self, layer: &Layer) -> Result<(), Error> {
        self.layers.push(layer.clone());
        Ok(())
    }
}

impl LayerSource for MemDepot {
    type Err = Error;
    fn next_layer(&mut self) -> Result<Option<Layer>, Error> {
        let l = self.layers.get(self.cursor).cloned();
        self.cursor += 1;
        Ok(l)
    }
}

#[test]
fn transfer_roundtrip_randomized() {
    let mut rng = Rng(42);
    let mut src = MemDepot::default();
    for _ in 0..40 {
        src.layers.push(random_layer(&mut rng));
    }
    let original = src.layers.clone();

    // mem → stream
    let mut w = StreamWriter::new(Vec::new(), 3);
    assert_eq!(transfer(&mut src, &mut w).unwrap(), 40);
    let bytes = w.finish().unwrap();

    // stream → mem
    let mut r = StreamReader::new(&bytes[..]);
    let mut dst = MemDepot::default();
    assert_eq!(transfer(&mut r, &mut dst).unwrap(), 40);
    assert_eq!(dst.layers, original);
}

#[test]
fn empty_stream_yields_nothing() {
    let mut r = StreamReader::new(&[][..]);
    assert!(r.next_layer().unwrap().is_none());
    // And keeps yielding None.
    assert!(r.next_layer().unwrap().is_none());
}

#[test]
fn truncation_is_an_error_not_a_panic() {
    let mut w = StreamWriter::new(Vec::new(), 3);
    let mut rng = Rng(7);
    w.put_layer(&random_layer(&mut rng)).unwrap();
    w.put_layer(&random_layer(&mut rng)).unwrap();
    let bytes = w.finish().unwrap();

    // Cut inside the second frame: first layer reads, second errors.
    let cut = &bytes[..bytes.len() - 3];
    let mut r = StreamReader::new(cut);
    assert!(r.next_layer().unwrap().is_some());
    assert!(matches!(r.next_layer(), Err(Error::Truncated)));

    // Cut inside the header too.
    let mut r = StreamReader::new(&bytes[..2]);
    assert!(matches!(r.next_layer(), Err(Error::Truncated)));
}

/// §8: ingesting N near-identical layers must grow the stream
/// sublinearly — bounded by the delta, not the layer. A stream whose
/// size tracks N × layer-size has not rendered the design.
#[test]
fn near_identical_layers_cost_the_delta() {
    // One ~64 KiB incompressible blob, mutated slightly per layer.
    let mut rng = Rng(1234);
    let mut blob: Vec<u8> = (0..65536).map(|_| rng.next() as u8).collect();

    let make_layer = |blob: &[u8], rev: u64| -> Layer {
        let file = Node {
            blob: BlobOp::Set((blob.to_vec()).into()),
            attrs: Some(Attrs::from([(b"rev".to_vec(), rev.to_le_bytes().to_vec())])),
            ..Node::keep()
        };
        Layer {
            root: Node {
                children: BTreeMap::from([(b"big".to_vec(), file)]),
                ..Node::keep()
            },
        }
    };

    let mut w = StreamWriter::new(Vec::new(), 3);
    let mut one_frame_size = 0usize;
    for rev in 0..50u64 {
        // Mutate 16 scattered bytes.
        for _ in 0..16 {
            let at = (rng.next() as usize) % blob.len();
            blob[at] = rng.next() as u8;
        }
        w.put_layer(&make_layer(&blob, rev)).unwrap();
        if rev == 0 {
            one_frame_size = w.bytes_written() as usize;
        }
    }
    let bytes = w.finish().unwrap();

    // 50 near-identical ~64K layers. Standalone each would be ~64K
    // (incompressible) ⇒ ~3.2 MB. The refPrefix stream must be in the
    // "first layer + 49 deltas" regime.
    assert!(
        bytes.len() < one_frame_size + 49 * 4096,
        "stream ({}) not in the delta regime (first frame {})",
        bytes.len(),
        one_frame_size
    );

    // And it still reads back exactly.
    let mut r = StreamReader::new(&bytes[..]);
    let mut n = 0;
    while let Some(layer) = r.next_layer().unwrap() {
        let big = &layer.root.children[&b"big".to_vec()];
        assert!(matches!(&big.blob, BlobOp::Set(b) if b.len() == 65536));
        n += 1;
    }
    assert_eq!(n, 50);
}
