//! VBF variant tests: newest-first round-trip through the trait halves,
//! cross-variant transfer (stream → VBF), sealing, and the §9
//! anti-sabotage size assertion.

mod common {
    include!("../../depot/tests/common/mod.rs");
}

use std::collections::BTreeMap;

use common::{random_layer, Rng};
use depot::variant::{transfer, LayerSink, LayerSource};
use depot::{Attrs, BlobOp, Layer, Node};
use depot_vbf::{VbfReader, VbfStore};

#[test]
fn roundtrip_newest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = VbfStore::open(tmp.path().into(), 0, 256 * 1024).unwrap();
    let mut rng = Rng(7);
    let layers: Vec<Layer> = (0..25).map(|_| random_layer(&mut rng)).collect();
    for l in &layers {
        store.put_layer(l).unwrap();
    }
    store.flush().unwrap();
    // Read newest-first: reverse of write order.
    let mut r = VbfReader::new(&store).unwrap();
    let mut got = Vec::new();
    while let Some(l) = r.next_layer().unwrap() {
        got.push(l);
    }
    let want: Vec<Layer> = layers.into_iter().rev().collect();
    assert_eq!(got, want);
}

#[test]
fn stream_to_vbf_transfer() {
    // stream (wire) → VBF (rest): the transfer composition the canonical
    // encoding exists for — no per-pair code.
    let mut rng = Rng(99);
    let layers: Vec<Layer> = (0..10).map(|_| random_layer(&mut rng)).collect();
    let mut w = depot_stream::StreamWriter::new(Vec::new(), 3);
    for l in &layers {
        w.put_layer(l).unwrap();
    }
    let bytes = w.finish().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let mut store = VbfStore::open(tmp.path().into(), 0, 256 * 1024).unwrap();
    let mut src = depot_stream::StreamReader::new(&bytes[..]);
    assert_eq!(transfer(&mut src, &mut store).unwrap(), 10);
    let got = store.layers_newest_first().unwrap();
    assert_eq!(got.len(), 10);
    assert_eq!(&got[0], layers.last().unwrap(), "newest = last fed");
    assert_eq!(&got[9], &layers[0], "oldest = first fed");
}

/// §9: near-identical successive layers must land in the head-plus-deltas
/// regime, sealing must fire (cold frames form), and every version reads
/// back exactly.
#[test]
fn near_identical_layers_compress_and_seal() {
    let tmp = tempfile::tempdir().unwrap();
    // Small seal threshold so the ~64K spilled head crosses it quickly.
    let mut store = VbfStore::open(tmp.path().into(), 0, 160 * 1024).unwrap();
    let mut rng = Rng(1234);
    let mut blob: Vec<u8> = (0..65536).map(|_| rng.next() as u8).collect();
    let make = |blob: &[u8], rev: u64| Layer {
        root: Node {
            children: BTreeMap::from([(b"big".to_vec(), Node {
                blob: BlobOp::Set(blob.to_vec().into()),
                attrs: Some(Attrs::from([(b"rev".to_vec(),
                                          rev.to_le_bytes().to_vec())])),
                ..Node::keep()
            })]),
            ..Node::keep()
        },
    };
    let mut versions = Vec::new();
    for rev in 0..120u64 {
        for _ in 0..16 {
            let at = (rng.next() as usize) % blob.len();
            blob[at] = rng.next() as u8;
        }
        let l = make(&blob, rev);
        store.put_layer(&l).unwrap();
        versions.push(l);
    }
    store.flush().unwrap();

    // Fidelity, newest-first.
    let got = store.layers_newest_first().unwrap();
    assert_eq!(got.len(), 120);
    for (i, l) in got.iter().enumerate() {
        assert_eq!(l, &versions[119 - i], "version {i} newest-first");
    }
    // Sealing fired.
    let cold = tmp.path().join("cold").join("cold");
    assert!(cold.metadata().map(|m| m.len()).unwrap_or(0) > 0,
            "no cold frames — sealing never fired");
    // Size: 120 x ~64K incompressible layers = ~7.9 MB raw. Live bytes
    // are ~one head + a bounded accumulator + per-version deltas; the
    // un-evictable residue (dead frames in the CURRENT f0/f1 files) is
    // O(file_size_threshold), a constant — so the ratio must widen as
    // the history deepens. The sabotage-shaped store grows ~linearly.
    fn dir_size(p: &std::path::Path) -> u64 {
        let mut t = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let q = e.path();
                t += if q.is_dir() { dir_size(&q) }
                     else { q.metadata().map(|m| m.len()).unwrap_or(0) };
            }
        }
        t
    }
    let raw: u64 = 120 * 65536;
    let disk = dir_size(tmp.path());
    eprintln!("raw {raw} B, vbf on disk {disk} B ({}x)", raw / disk.max(1));
    assert!(disk * 4 < raw,
            "vbf on disk ({disk}) not <1/4 of raw ({raw}) — discipline not rendered");
}

/// The normative batch form: put_layers(oldest→newest) must land ONE
/// prepend and read back record-for-record identical to sequential
/// put_layer calls — never split by the seal threshold (sealing is a
/// between-prepends decision against the OLD accumulator).
#[test]
fn put_layers_batch_equals_sequential() {
    fn layer(n: u32, body: &[u8]) -> depot::Layer {
        let mut children = std::collections::BTreeMap::new();
        children.insert(format!("f{n}").into_bytes(), depot::Node {
            blob: depot::BlobOp::Set(body.to_vec().into()),
            children: Default::default(),
            presence: depot::Presence::Live,
            opaque: false,
            attrs: None,
            anchor: depot::Anchor::Lower,
        });
        depot::Layer { root: depot::Node {
            blob: depot::BlobOp::Keep,
            children,
            presence: depot::Presence::Live,
            opaque: false,
            attrs: None,
            anchor: depot::Anchor::Lower,
        }}
    }
    // Bodies big enough that the batch total far exceeds the seal
    // threshold — the batch must still be one prepend.
    let layers: Vec<depot::Layer> =
        (0..24).map(|i| layer(i, &vec![b'a' + (i % 26) as u8; 4096])).collect();

    let t1 = tempfile::tempdir().unwrap();
    let mut seq = depot_vbf::VbfDepot::open(t1.path().into(), 8, 16 * 1024).unwrap();
    for l in &layers { seq.put_layer(3, l).unwrap(); }
    let t2 = tempfile::tempdir().unwrap();
    let mut bat = depot_vbf::VbfDepot::open(t2.path().into(), 8, 16 * 1024).unwrap();
    let before = bat.prepend_count();
    bat.put_layers(3, &layers).unwrap();
    // Seed (empty-chain constraint) + the batch = at most 2.
    assert!(bat.prepend_count() - before <= 2,
            "batch split into {} prepends", bat.prepend_count() - before);

    let a = seq.layers_newest_first(3).unwrap();
    let b = bat.layers_newest_first(3).unwrap();
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(depot::codec::encode(x), depot::codec::encode(y));
    }
}
