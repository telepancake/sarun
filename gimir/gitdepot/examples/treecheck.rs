//! Roundtrip soundness probe: for every commit in a store, reconstruct its
//! tree FROM STORED BYTES via the union (`Store::union().tree_oid_at` — a
//! reverse-delta walk of the union chain, then hash the reconstructed entries
//! to a git tree oid purely in-process). The mirror IS the union (§1), so
//! there is no per-commit TREES chain to walk. Prints `<commit_sha>
//! <tree_oid>` per line so a shell harness can diff against
//! `git rev-parse <sha>^{tree}`.
//!
//! Usage: treecheck <store> [stride]
//! stride N: check every Nth commit (default 1 = all).

use gitdepot::store::Store;

fn main() {
    let mut args = std::env::args().skip(1);
    let store = std::path::PathBuf::from(args.next().expect("store path"));
    let stride: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(1);

    let st = Store::open(&store).expect("open store");
    let recs = st.commit_records().expect("commit records");
    let n = recs.len();
    let union = st.union().expect("open union");
    eprintln!("store has {n} commit records; stride {stride}");
    let mut checked = 0usize;
    for (i, rec) in recs.iter().enumerate() {
        if i % stride != 0 {
            continue;
        }
        let oid = union.tree_oid_at(rec.idx as usize).expect("reconstruct tree");
        println!("{} {}", rec.sha, oid);
        checked += 1;
    }
    eprintln!("emitted {checked} (commit_sha, reconstructed_tree_oid) pairs");
}
