//! Roundtrip soundness probe: for every commit in a store, reconstruct
//! its tree FROM STORED BYTES (`Store::tree_view` — reverse-delta walk of
//! the TREES chain) and hash it to a git tree oid purely in-process
//! (`layer::tree_oid_of_entries`). Prints `<commit_sha> <tree_oid>` per
//! line so a shell harness can diff against `git rev-parse <sha>^{tree}`.
//!
//! Usage: treecheck <store> [stride]
//! stride N: check every Nth commit (default 1 = all).

use std::sync::Arc;

use depot::View;
use gitdepot::layer::{tree_oid_of_entries, Mode};
use gitdepot::store::Store;

fn walk(view: &View, prefix: &[u8], out: &mut Vec<(Vec<u8>, Mode, Vec<u8>)>) {
    for (name, child) in &view.children {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name);
        match &child.blob {
            Some(content) => {
                let mode = child
                    .attrs
                    .get(&b"mode"[..])
                    .and_then(|m| Mode::from_octal(m))
                    .expect("file node without valid mode attr");
                out.push((path, mode, content.to_vec()));
            }
            None => walk_arc(child, &path, out),
        }
    }
}

fn walk_arc(view: &Arc<View>, prefix: &[u8], out: &mut Vec<(Vec<u8>, Mode, Vec<u8>)>) {
    walk(view, prefix, out)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let store = std::path::PathBuf::from(args.next().expect("store path"));
    let stride: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(1);

    let st = Store::open(&store).expect("open store");
    let recs = st.commit_records().expect("commit records");
    let n = recs.len();
    // ONE newest-first pass reconstructs every tree from stored bytes
    // (reverse-delta walk, O(history) total, not O(history^2)).
    let views_nf = st.tree_views(None).expect("tree_views");
    let n_trees = st.count(gitdepot::store::TREES).expect("count trees") as usize;
    assert_eq!(views_nf.len(), n_trees, "view count != n_trees");
    eprintln!("store has {n} commit records, {n_trees} tree records; stride {stride}");
    let mut checked = 0usize;
    for (i, rec) in recs.iter().enumerate() {
        if i % stride != 0 {
            continue;
        }
        let view = &views_nf[n_trees - 1 - rec.tree_idx as usize];
        let mut entries = Vec::new();
        walk(view, b"", &mut entries);
        let oid = tree_oid_of_entries(&entries).expect("hash tree");
        println!("{} {}", rec.sha, oid);
        checked += 1;
    }
    eprintln!("emitted {checked} (commit_sha, reconstructed_tree_oid) pairs");
}
