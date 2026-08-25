//! Materialization-cache tests: dedup by content, immutability, tree
//! materialization with pool sharing, idempotence over partial state,
//! rebuild-from-nothing (everything is derived), and eviction.

use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;

use depot::{Attrs, BlobOp, Layer, Node};
use depot_cache::Cache;

fn file(content: &[u8], mode: &str) -> Node {
    Node {
        blob: BlobOp::Set(content.to_vec().into()),
        attrs: Some(Attrs::from([(b"mode".to_vec(), mode.as_bytes().to_vec())])),
        ..Node::keep()
    }
}

fn view_layer() -> Layer {
    let mut dir = Node::keep();
    dir.attrs = Some(Attrs::from([(b"mode".to_vec(), b"16877".to_vec())]));
    dir.children.insert(b"same.bin".to_vec(), file(b"SHARED", "33188"));
    dir.children.insert(b"tool".to_vec(), file(b"#!/bin/sh\n", "33261"));
    let mut root = Node::keep();
    root.children.insert(b"d".to_vec(), dir);
    root.children.insert(b"top.txt".to_vec(), file(b"SHARED", "33188"));
    Layer { root }
}

#[test]
fn pool_dedup_and_immutability() {
    let tmp = tempfile::tempdir().unwrap();
    let c = Cache::open(tmp.path().into()).unwrap();
    let a = c.file_for(b"hello").unwrap();
    let b = c.file_for(b"hello").unwrap();
    assert_eq!(a, b, "same content, same file");
    let md = std::fs::metadata(&a).unwrap();
    assert_eq!(md.permissions().mode() & 0o777, 0o444, "immutable");
    assert_ne!(c.file_for(b"other").unwrap(), a);
}

#[test]
fn materialize_shares_pool_and_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let c = Cache::open(tmp.path().into()).unwrap();
    let layer = view_layer();
    let tree = c.materialize(&layer).unwrap();

    assert_eq!(std::fs::read(tree.join("top.txt")).unwrap(), b"SHARED");
    assert_eq!(std::fs::read(tree.join("d/same.bin")).unwrap(), b"SHARED");
    // Equal content = ONE pool inode, hardlinked into both names.
    let i1 = std::fs::metadata(tree.join("top.txt")).unwrap().ino();
    let i2 = std::fs::metadata(tree.join("d/same.bin")).unwrap().ino();
    assert_eq!(i1, i2, "equal blobs share the pool inode");
    assert!(std::fs::metadata(tree.join("top.txt")).unwrap().nlink() >= 3);
    // Executables get a private inode with the exec bit.
    let tm = std::fs::metadata(tree.join("d/tool")).unwrap();
    assert!(tm.permissions().mode() & 0o111 != 0, "exec bit honored");

    // Idempotent: second call returns the same finished tree.
    assert_eq!(c.materialize(&layer).unwrap(), tree);
}

#[test]
fn rebuildable_from_partial_and_from_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let c = Cache::open(tmp.path().into()).unwrap();
    let layer = view_layer();
    let tree = c.materialize(&layer).unwrap();
    let key = Cache::tree_key(&layer);

    // Simulate a crash mid-materialization: ok-marker gone, tree mangled.
    std::fs::remove_file(tmp.path().join("tree").join(format!("{key}.ok"))).unwrap();
    std::fs::remove_file(tree.join("top.txt")).unwrap();
    let tree2 = c.materialize(&layer).unwrap();
    assert_eq!(std::fs::read(tree2.join("top.txt")).unwrap(), b"SHARED");

    // Nuke the WHOLE cache: everything is derived; rebuild is clean.
    drop(c);
    std::fs::remove_dir_all(tmp.path().join("blob")).unwrap();
    std::fs::remove_dir_all(tmp.path().join("tree")).unwrap();
    let c = Cache::open(tmp.path().into()).unwrap();
    let tree3 = c.materialize(&layer).unwrap();
    assert_eq!(std::fs::read(tree3.join("d/same.bin")).unwrap(), b"SHARED");
}

#[test]
fn eviction_reclaims_only_unreferenced() {
    let tmp = tempfile::tempdir().unwrap();
    let c = Cache::open(tmp.path().into()).unwrap();
    let layer = view_layer();
    c.materialize(&layer).unwrap();
    let orphan = c.file_for(b"nobody links me").unwrap();
    assert!(orphan.exists());
    let removed = c.evict_unreferenced().unwrap();
    assert!(removed >= 1, "orphan reclaimed");
    assert!(!orphan.exists());
    // Referenced content survives eviction.
    let tree = c.materialize(&layer).unwrap();
    assert_eq!(std::fs::read(tree.join("top.txt")).unwrap(), b"SHARED");
}

#[test]
fn refuses_delta_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let c = Cache::open(tmp.path().into()).unwrap();
    let mut root = Node::keep();
    root.children.insert(b"gone".to_vec(), Node::tombstone());
    assert!(c.materialize(&Layer { root }).is_err());
    let mut root = Node::keep();
    root.children.insert(b"h".to_vec(), Node::hole());
    assert!(c.materialize(&Layer { root }).is_err());
}
