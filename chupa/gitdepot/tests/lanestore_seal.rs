//! Multi-cold-frame coverage for the lane store's sealed chain — the
//! blind review's finding #1: with production thresholds every fixture
//! is tiny, so `seal_prepend`'s seal-old branch and the cold-frame
//! anchor recompute in the reverse-delta walk never ran. This forces
//! them by shrinking the batch and seal thresholds
//! (`GITDEPOT_TEST_BATCH` / `GITDEPOT_TEST_SEAL`) so a small repo crosses
//! them repeatedly, producing >= 2 cold frames, then reconstructs every
//! commit SHA-exact ACROSS those frames. A bug in the seal bridging or a
//! wrong per-frame anchor would mis-reconstruct past the first cold
//! boundary — which no other lanestore test can catch.
//!
//! Own test binary: the env overrides are process-global, so isolating
//! them here keeps them out of the parallel `lanestore.rs` fixtures.
//!
//! Needs a `git` binary.

use std::path::Path;
use std::process::Command;

fn sh_git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@x")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    sh_git(repo, &["init", "-q", "-b", "main"]);
    sh_git(repo, &["config", "commit.gpgsign", "false"]);
}

fn write(repo: &Path, path: &str, body: &str) {
    let p = repo.join(path);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

fn commit(repo: &Path, msg: &str) {
    sh_git(repo, &["add", "-A"]);
    sh_git(repo, &["commit", "-q", "-m", msg]);
}

#[test]
fn multi_cold_frame_roundtrip_is_sha_exact() {
    // Force many small sealed frames: batch flushes every ~256 B of
    // staged reverse deltas, and each flush whose accumulator exceeds
    // ~128 B seals to cold.
    std::env::set_var("GITDEPOT_TEST_BATCH", "256");
    std::env::set_var("GITDEPOT_TEST_SEAL", "128");

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("seal");
    init(&repo);

    // ~40 commits, each rewriting a few hundred bytes across two files on
    // two interleaved branches (so lanes + variants participate), giving
    // reverse deltas that cross the 256 B batch bound many times.
    write(&repo, "big.txt", &"seed\n".repeat(64));
    write(&repo, "a.txt", "a\n");
    commit(&repo, "base");
    let base = sh_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    sh_git(&repo, &["checkout", "-q", "-b", "feat", &base]);

    for i in 0..40 {
        let (branch, file) = if i % 2 == 0 { ("main", "a.txt") } else { ("feat", "b.txt") };
        sh_git(&repo, &["checkout", "-q", branch]);
        let body: String = (0..30).map(|k| format!("line {i} {k} xxxxxxxxxxxx\n")).collect();
        write(&repo, file, &body);
        commit(&repo, &format!("c{i}"));
    }
    sh_git(&repo, &["checkout", "-q", "main"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "merge feat", "feat"]);

    // Expected trees while the repo exists.
    let shas: Vec<String> =
        sh_git(&repo, &["rev-list", "--branches", "--tags"]).lines().map(str::to_string).collect();
    let want: std::collections::HashMap<String, String> = shas
        .iter()
        .map(|s| (s.clone(), sh_git(&repo, &["rev-parse", &format!("{s}^{{tree}}")]).trim().to_string()))
        .collect();

    let dir = tmp.path().join("store");
    {
        gitdepot::lanestore::LaneStore::encode_repo_union(&repo, &dir, 3).unwrap();
    }
    let store = gitdepot::lanestore::LaneStore::open(&dir).unwrap();

    // The point of the test: the seal path actually fired, more than once.
    let cold = store.cold_frame_count().unwrap();
    assert!(cold >= 2, "expected >= 2 cold frames to exercise the seal path, got {cold}");

    // Every commit reconstructs SHA-exact from disk, across the cold
    // frames — proving the seal bridging and per-frame anchor are correct.
    for rev in 0..store.n_rev() {
        let sha = store.sha_at(rev).to_string();
        let got = store.tree_oid_at(rev).unwrap();
        assert_eq!(got, want[&sha], "commit {sha} at rev {rev} mis-reconstructed across cold frames");
    }

    std::env::remove_var("GITDEPOT_TEST_BATCH");
    std::env::remove_var("GITDEPOT_TEST_SEAL");
}
