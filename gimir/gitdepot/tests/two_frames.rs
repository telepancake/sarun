//! One prepend per touched chain, immediate seal of an oversized f1.
//!
//! The mandated ingest shape: an ingest lands EXACTLY ONE prepend per
//! touched chain — f0 (head full record) + ONE f1 holding every other
//! staged record — and if that f1's raw size exceeds the seal
//! threshold it is sealed to cold immediately in the same operation
//! (`Depot::seal_f1`), so a later incremental update never
//! recompresses a huge accumulator. Frames are write-once: a
//! subsequent update must leave existing cold frames byte-identical.
//!
//! GITDEPOT_TEST_SEAL shrinks the seal threshold; the env var is
//! process-global, so this binary holds exactly ONE #[test].
//!
//! Needs a `git` binary.

use std::path::Path;
use std::process::Command;

use gitdepot::store::{COMMITS, REFLOG, TAGS};
use wikimak_depot::{Depot, DepotConfig};

fn sh_git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@x")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@x")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn build(repo: &Path, upto: usize) {
    if !repo.join(".git").exists() {
        std::fs::create_dir_all(repo).unwrap();
        sh_git(repo, &["init", "-q", "-b", "main"]);
        sh_git(repo, &["config", "commit.gpgsign", "false"]);
        sh_git(repo, &["config", "tag.gpgsign", "false"]);
    }
    let n: usize = sh_git(repo, &["rev-list", "--count", "--all"])
        .trim().parse().unwrap_or(0);
    for i in n..upto {
        std::fs::write(repo.join(format!("f{}.txt", i % 7)),
                       format!("content of revision {i}\nline\n")).unwrap();
        sh_git(repo, &["add", "-A"]);
        sh_git(repo, &["commit", "-q", "-m", &format!("c{i}")]);
        if i % 9 == 0 || i == upto - 1 {
            sh_git(repo, &["tag", "-a", "-m", "t", &format!("t{i}")]);
        }
    }
}

/// Frame census for one chain: (f0 present, f1 present, cold frames).
fn frames(store: &Path, chain: u64) -> (bool, bool, Vec<Vec<u8>>) {
    // Same config values as gitdepot's open_depot; the Store handle
    // must be dropped before this opens the depot directly.
    let depot = Depot::open(DepotConfig {
        root: store.join("depot"),
        max_chain_id: 4,
        file_size_threshold: 4 << 20,
        eviction_dead_ratio: 0.5,
    })
    .unwrap();
    let f0 = depot.read_f0(chain).is_ok();
    let f1 = depot.read_f1(chain).map(|o| o.is_some()).unwrap_or(false);
    let cold = if f0 {
        depot.cold_iter(chain).unwrap().map(|r| r.unwrap()).collect()
    } else {
        Vec::new()
    };
    (f0, f1, cold)
}

/// Frame census for the union tree store's single chain (its depot lives
/// under `store/trees/depot`, one chain).
fn union_frames(store: &Path) -> (bool, bool, Vec<Vec<u8>>) {
    let depot = Depot::open(DepotConfig {
        root: store.join("trees/depot"),
        max_chain_id: 1,
        file_size_threshold: 4 << 20,
        eviction_dead_ratio: 0.5,
    })
    .unwrap();
    let f0 = depot.read_f0(0).is_ok();
    let f1 = depot.read_f1(0).map(|o| o.is_some()).unwrap_or(false);
    let cold = if f0 {
        depot.cold_iter(0).unwrap().map(|r| r.unwrap()).collect()
    } else {
        Vec::new()
    };
    (f0, f1, cold)
}

#[test]
fn one_prepend_per_chain_and_immediate_seal() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");

    // ---------------------------------------------------- default seal
    // Fresh import + one large update, default threshold: exactly one
    // prepend per touched chain, f1 XOR cold per multi-record chain.
    build(&repo, 5);
    let a = tmp.path().join("store-a");
    gitdepot::import(&repo, &a, 3).unwrap();
    build(&repo, 45);
    let oa = gitdepot::update(&repo, &a, 3).unwrap();
    assert_eq!(oa.new_commits, 40);
    // ONE prepend per touched chain in the commit-side store: COMMITS,
    // TAGS, REFLOG. (The union tree store lands separately in store/trees.)
    assert_eq!(oa.depot_prepends, 3, "update must land one prepend per chain");
    assert!(!a.join("staging").exists(), "staging residue survived finish()");
    for chain in [COMMITS, TAGS, REFLOG] {
        let (f0, f1, cold) = frames(&a, chain);
        assert!(f0, "chain {chain}: no f0");
        // Small history: nothing crossed the 256K seal threshold —
        // the accumulator holds everything, no cold.
        assert!(f1, "chain {chain}: expected an f1 accumulator");
        assert!(cold.is_empty(), "chain {chain}: unexpected cold frames");
    }
    // The union tree store landed its own f0 + f1, no cold at this scale.
    let (tf0, tf1, tcold) = union_frames(&a);
    assert!(tf0, "union trees: no f0");
    assert!(tf1, "union trees: expected an f1 accumulator");
    assert!(tcold.is_empty(), "union trees: unexpected cold frames");

    // The reference store: same 55-commit end state, default seal —
    // imported BEFORE the env var below poisons the process.
    build(&repo, 55);
    let d = tmp.path().join("store-d");
    gitdepot::import(&repo, &d, 3).unwrap();

    // -------------------------------------------------- shrunken seal
    // A raw batch far above the (test-shrunk) threshold: the single
    // prepend's f1 must be sealed to cold IMMEDIATELY, leaving f0 +
    // cold and no f1.
    std::env::set_var("GITDEPOT_TEST_SEAL", "512");
    let c = tmp.path().join("store-c");
    gitdepot::import(&repo, &c, 3).unwrap();
    let (f0, f1, tree_cold_1) = union_frames(&c);
    assert!(f0, "union trees: no f0");
    assert!(!f1, "union trees: oversized f1 must be sealed immediately");
    assert_eq!(tree_cold_1.len(), 1, "union trees: exactly one immediately-sealed frame");
    // Every multi-record chain landed f0 + (f1 XOR immediate cold).
    for chain in [COMMITS, TAGS, REFLOG] {
        let (f0, f1, cold) = frames(&c, chain);
        assert!(f0, "chain {chain}: no f0");
        // Fresh import: these chains land ONE batch record as f0 (no
        // f1 exists on a chain's first prepend, so nothing to seal).
        assert!(!f1 && cold.is_empty(), "chain {chain}: fresh import must be f0-only");
    }

    // Incremental update on top of the sealed store: cold frames are
    // write-once — the update must not recompress them.
    build(&repo, 70);
    let oc = gitdepot::update(&repo, &c, 3).unwrap();
    assert_eq!(oc.new_commits, 15);
    let (_, _, tree_cold_2) = union_frames(&c);
    assert!(
        tree_cold_2.ends_with(&tree_cold_1[..]),
        "union trees: the pre-update cold frame was rewritten (must stay byte-identical)"
    );
    assert!(tree_cold_2.len() > 1, "union trees: update batch should also have sealed");
    // COMMITS: the update prepend demotes the import batch into an f1;
    // with the shrunk threshold that f1 seals immediately too.
    let (_, cf1, ccold) = frames(&c, COMMITS);
    assert!(!cf1 && ccold.len() == 1, "COMMITS: demoted batch must seal immediately");
    std::env::remove_var("GITDEPOT_TEST_SEAL");

    // ------------------------------------------------- content parity
    // The sealed store reads back identically to the default-threshold
    // reference brought to the same end state.
    let od = gitdepot::update(&repo, &d, 3).unwrap();
    assert_eq!(od.new_commits, 15);
    let sc = gitdepot::store::Store::open(&c).unwrap();
    let sd = gitdepot::store::Store::open(&d).unwrap();
    assert_eq!(sc.commit_records().unwrap(), sd.commit_records().unwrap());
    assert_eq!(sc.tag_records().unwrap(), sd.tag_records().unwrap());
    assert_eq!(sc.ref_rows().unwrap(), sd.ref_rows().unwrap());
    // Tip-biased point reads across the batch boundaries.
    let n = sc.count(COMMITS).unwrap();
    for i in 0..n {
        assert_eq!(
            sc.commit_record_at(i).unwrap(),
            sd.commit_record_at(i).unwrap(),
            "commit {i} diverges"
        );
    }
    // Every reconstructed union tree, SHA-exact — reconstruction crosses
    // the union store's frame/seal boundaries and must reproduce exactly.
    let tree_oids = |st: &gitdepot::store::Store| -> Vec<String> {
        let ls = st.union().unwrap();
        (0..ls.n_rev()).map(|r| ls.tree_oid_at(r).unwrap()).collect()
    };
    assert_eq!(tree_oids(&sc), tree_oids(&sd), "union trees diverge across seal shapes");
}
