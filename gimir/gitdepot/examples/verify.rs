//! Mode-faithful SHA-exact probe: reconstruct every commit's tree from the
//! union store's stored bytes (`LaneStore::tree_view_at`, reached through
//! `Store::union` — the mirror IS the union, §1) and hash via the store's own
//! `git_tree_oid_of_view` (raw mode attrs), printing `<commit_sha> <tree_oid>`.
//! Usage: verify <store>
use gitdepot::git_tree_oid_of_view;
use gitdepot::store::Store;

fn main() {
    let store = std::path::PathBuf::from(std::env::args().nth(1).expect("store"));
    let st = Store::open(&store).unwrap();
    let recs = st.commit_records().unwrap();
    let union = st.union().unwrap();
    for r in &recs {
        let view = union.tree_view_at(r.idx as usize).unwrap();
        let oid = git_tree_oid_of_view(&view).unwrap();
        println!("{} {oid}", r.sha);
    }
}
