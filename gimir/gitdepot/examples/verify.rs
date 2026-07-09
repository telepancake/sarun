//! Mode-faithful SHA-exact probe: reconstruct every commit's tree from
//! stored bytes and hash via the store's own `git_tree_oid_of_view` (raw
//! mode attrs), printing `<commit_sha> <tree_oid>`. Usage: verify <store>
use gitdepot::git_tree_oid_of_view;
use gitdepot::store::Store;

fn main() {
    let store = std::path::PathBuf::from(std::env::args().nth(1).expect("store"));
    let st = Store::open(&store).unwrap();
    let recs = st.commit_records().unwrap();
    let views_nf = st.tree_views(None).unwrap();
    let n = views_nf.len();
    for r in &recs {
        let view = &views_nf[n - 1 - r.tree_idx as usize];
        let oid = git_tree_oid_of_view(view).unwrap();
        println!("{} {oid}", r.sha);
    }
}
