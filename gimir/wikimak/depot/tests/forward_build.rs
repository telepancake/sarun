//! Bulk forward construction (SPEC §"Bulk forward construction"):
//! byte-payload tests of the `ChainBuilder` contract. The depot is
//! zstd-opaque, so these feed labeled byte slices and pin the WALK —
//! a forward-built chain must read back through `read_f0`/`read_f1`/
//! `cold_iter` exactly like a prepend+seal-built one — plus the
//! empty-chain precondition, the orphan story (an unfinished build is
//! invisible, across a reopen too), and the headless finish shapes.

use tempfile::TempDir;
use wikimak_depot::{Depot, DepotConfig, Error};

fn open(root: std::path::PathBuf) -> Depot {
    Depot::open(DepotConfig {
        root,
        max_chain_id: 64,
        file_size_threshold: 1 << 20,
        eviction_dead_ratio: 0.5,
    })
    .unwrap()
}

/// Forward-build: three history frames + f1 + f0. The walk must yield
/// the exact frame bytes newest-first: f0, f1, cold3, cold2, cold1.
#[test]
fn forward_built_chain_walks_like_a_prepend_built_one() {
    let tmp = TempDir::new().unwrap();

    // The reference store, built the prepend way: each seal moves the
    // then-current f1 to cold verbatim, so ending with cold =
    // [c3, c2, c1] (newest-first), f1 = "f1", f0 = "f0" requires
    // prepending c1..c3 as accumulators and sealing each.
    let a = open(tmp.path().join("a"));
    a.prepend(7, b"seed", None, false).unwrap();
    a.prepend(7, b"h1", Some(b"c1"), false).unwrap();
    a.prepend(7, b"h2", Some(b"c2"), true).unwrap();
    a.prepend(7, b"h3", Some(b"c3"), true).unwrap();
    a.prepend(7, b"f0", Some(b"f1"), true).unwrap();

    // The same logical chain, built forward: cold frames oldest-first.
    let b = open(tmp.path().join("b"));
    let mut builder = b.begin_chain(7).unwrap();
    for frame in [b"c1", b"c2", b"c3"] {
        b.append_history_frame(&mut builder, frame).unwrap();
    }
    assert_eq!(builder.frames_written(), 3);
    b.finish_chain(builder, b"f0", Some(b"f1")).unwrap();
    b.flush().unwrap();

    for depot in [&a, &b] {
        assert_eq!(depot.read_f0(7).unwrap(), b"f0".to_vec());
        assert_eq!(depot.read_f1(7).unwrap(), Some(b"f1".to_vec()));
        let cold: Vec<Vec<u8>> = depot.cold_iter(7).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(cold, vec![b"c3".to_vec(), b"c2".to_vec(), b"c1".to_vec()]);
    }

    // And it survives a reopen byte-identically.
    drop(b);
    let b = open(tmp.path().join("b"));
    assert_eq!(b.read_f0(7).unwrap(), b"f0".to_vec());
    let cold: Vec<Vec<u8>> = b.cold_iter(7).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(cold, vec![b"c3".to_vec(), b"c2".to_vec(), b"c1".to_vec()]);
}

/// finish with no f1 leaves f0 pointing DIRECTLY at the cold head (the
/// `seal_f1` pointer state); with neither f1 nor history it is the
/// plain virgin-chain shape. Both must walk.
#[test]
fn headless_finish_shapes() {
    let tmp = TempDir::new().unwrap();
    let d = open(tmp.path().to_path_buf());

    // f0 + history, no f1.
    let mut b = d.begin_chain(1).unwrap();
    d.append_history_frame(&mut b, b"old").unwrap();
    d.finish_chain(b, b"head", None).unwrap();
    assert_eq!(d.read_f0(1).unwrap(), b"head".to_vec());
    assert_eq!(d.read_f1(1).unwrap(), None);
    let cold: Vec<Vec<u8>> = d.cold_iter(1).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(cold, vec![b"old".to_vec()]);
    // The next prepend inherits that direct cold head.
    d.prepend(1, b"head2", Some(b"acc"), false).unwrap();
    let cold: Vec<Vec<u8>> = d.cold_iter(1).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(cold, vec![b"old".to_vec()], "prepend after headless finish keeps cold");

    // f0 only: exactly a first prepend.
    let b = d.begin_chain(2).unwrap();
    d.finish_chain(b, b"solo", None).unwrap();
    assert_eq!(d.read_f0(2).unwrap(), b"solo".to_vec());
    assert_eq!(d.read_f1(2).unwrap(), None);
    assert_eq!(d.cold_iter(2).unwrap().count(), 0);
}

/// The empty-chain precondition is checked at begin AND at finish, and
/// an abandoned build stays invisible — even across a reopen (the
/// orphan frames cost cold bytes, nothing else).
#[test]
fn non_empty_chain_rejected_and_orphans_invisible() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let d = open(root.clone());
    d.prepend(3, b"live", None, false).unwrap();
    assert!(matches!(d.begin_chain(3), Err(Error::ChainNotEmpty)));

    // Race shape: builder began while empty, chain grew before finish.
    let b = d.begin_chain(4).unwrap();
    d.prepend(4, b"sneaky", None, false).unwrap();
    assert!(matches!(d.finish_chain(b, b"f0", None), Err(Error::ChainNotEmpty)));
    assert_eq!(d.read_f0(4).unwrap(), b"sneaky".to_vec(), "live head untouched");

    // Abandoned build: frames appended, builder dropped before finish.
    {
        let mut b = d.begin_chain(5).unwrap();
        d.append_history_frame(&mut b, b"orphan-1").unwrap();
        d.append_history_frame(&mut b, b"orphan-2").unwrap();
    }
    assert!(matches!(d.read_f0(5), Err(Error::NoFrame)), "unfinished build must be invisible");
    d.flush().unwrap();
    drop(d);

    let d = open(root.clone());
    assert!(matches!(d.read_f0(5), Err(Error::NoFrame)), "still invisible after reopen");
    // The orphan bytes really are in the cold file (dead weight until
    // instance delete), and a fresh build of the same chain succeeds.
    let cold_len = std::fs::metadata(root.join("cold/cold")).unwrap().len();
    assert!(cold_len > 0, "orphan frames occupy cold bytes");
    let mut b = d.begin_chain(5).unwrap();
    d.append_history_frame(&mut b, b"real-1").unwrap();
    d.finish_chain(b, b"real-head", Some(b"real-f1")).unwrap();
    assert_eq!(d.read_f0(5).unwrap(), b"real-head".to_vec());
    let cold: Vec<Vec<u8>> = d.cold_iter(5).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(cold, vec![b"real-1".to_vec()], "walk sees only the finished build");
}

/// The instrumentation the write-amplification measurement stands on:
/// a forward build's bytes_written ≈ the store's on-disk data bytes
/// (every frame written once), and cold bytes land once per frame.
#[test]
fn forward_build_writes_each_byte_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let d = open(root.clone());
    let mut b = d.begin_chain(9).unwrap();
    for i in 0..10u8 {
        d.append_history_frame(&mut b, &vec![i; 1000]).unwrap();
    }
    d.finish_chain(b, &[0xAA; 500], Some(&[0xBB; 800])).unwrap();
    d.flush().unwrap();

    let mut disk = 0u64;
    for sub in ["f0", "f1", "cold"] {
        for e in std::fs::read_dir(root.join(sub)).unwrap().flatten() {
            disk += e.metadata().unwrap().len();
        }
    }
    let written = d.bytes_written();
    // Every data byte on disk was written exactly once; the only
    // extras are index flips (8 bytes each, counted).
    assert!(
        written >= disk && written <= disk + 64,
        "forward build must write each byte once: wrote {written}, {disk} on disk"
    );
}
