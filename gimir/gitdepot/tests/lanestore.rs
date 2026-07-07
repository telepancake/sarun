//! SHA-exact round-trip proof for the combined-state lockstep lane
//! encoder (`gitdepot::lanestore`). Builds several synthetic git repos —
//! linear, fork+merge, criss-cross, and an empty-tree (delete-all)
//! branch — encodes each into a lane store, then for EVERY commit
//! reconstructs its tree from the lane store and asserts the
//! reconstructed git tree oid equals `git rev-parse <commit>^{tree}`.
//! Also asserts the lockstep/prefix structure: each stored reverse-delta
//! record touches exactly one lane prefix.
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
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@x")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    sh_git(repo, &["init", "-q", "-b", "main"]);
    sh_git(repo, &["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, msg: &str) -> String {
    sh_git(repo, &["add", "-A"]);
    sh_git(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
    sh_git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

fn write(repo: &Path, path: &str, body: &str) {
    let p = repo.join(path);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

/// Every reachable commit's reconstructed tree oid must equal git's.
fn assert_roundtrip(repo: &Path, tmp: &Path, tag: &str) -> usize {
    let dir = tmp.join(format!("lanestore-{tag}"));
    let store = gitdepot::lanestore::LaneStore::encode_repo(repo, &dir, 3)
        .unwrap_or_else(|e| panic!("[{tag}] encode: {e}"));

    // Enumerate every commit git considers reachable (same scope the
    // encoder walked).
    let shas: Vec<String> =
        sh_git(repo, &["rev-list", "--branches", "--tags"]).lines().map(str::to_string).collect();
    assert!(!shas.is_empty(), "[{tag}] no commits");

    for sha in &shas {
        let want = sh_git(repo, &["rev-parse", &format!("{sha}^{{tree}}")]).trim().to_string();
        let got = store
            .tree_oid_of_commit(sha)
            .unwrap_or_else(|e| panic!("[{tag}] reconstruct {sha}: {e}"));
        assert_eq!(got, want, "[{tag}] commit {sha}: tree oid mismatch");
    }

    // Lockstep/prefix structure: every stored reverse-delta record (all
    // but the full head at position 0) touches EXACTLY ONE lane prefix,
    // and it is the prefix of the lane advanced at that revision.
    let prefixes = store.record_prefixes().unwrap();
    let n = store.n_rev();
    assert_eq!(prefixes.len(), n, "[{tag}] record count != revision count");
    for pos in 1..prefixes.len() {
        assert_eq!(
            prefixes[pos].len(),
            1,
            "[{tag}] reverse-delta record at pos {pos} touched {} lanes (expected 1) — lanes oscillate",
            prefixes[pos].len()
        );
    }
    // The record at position `advance_record_pos(rev)` is the one lane
    // advanced at `rev`, and its single prefix is that lane's prefix.
    for rev in 1..n {
        let pos = store.advance_record_pos(rev);
        let want = store.lane_prefix(store.lane_of(rev));
        assert_eq!(prefixes[pos], vec![want], "[{tag}] rev {rev} record touches the wrong lane");
    }
    shas.len()
}

#[test]
fn lane_roundtrip_linear() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("linear");
    init(&repo);
    for i in 0..6 {
        write(&repo, "doc.txt", &format!("revision {i}\n"));
        write(&repo, "src/mod.rs", &format!("const REV: u32 = {i};\n"));
        commit(&repo, &format!("r{i}"));
    }
    let n = assert_roundtrip(&repo, tmp.path(), "linear");
    assert_eq!(n, 6);
}

#[test]
fn lane_roundtrip_fork_and_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("fork");
    init(&repo);
    write(&repo, "base.txt", "base\n");
    commit(&repo, "base");
    write(&repo, "main1.txt", "m1\n");
    commit(&repo, "m1");

    // A topic branch off the base with two commits, then merge back.
    sh_git(&repo, &["checkout", "-q", "-b", "topic", "main~1"]);
    write(&repo, "topic1.txt", "t1\n");
    commit(&repo, "t1");
    write(&repo, "topic2.txt", "t2\n");
    commit(&repo, "t2");
    sh_git(&repo, &["checkout", "-q", "main"]);
    write(&repo, "main2.txt", "m2\n");
    commit(&repo, "m2");
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);

    assert_roundtrip(&repo, tmp.path(), "fork");
}

#[test]
fn lane_roundtrip_criss_cross() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("criss");
    init(&repo);
    write(&repo, "f.txt", "0\n");
    commit(&repo, "root");

    // Two concurrent branches, each advanced, then a criss-cross: each
    // branch merges the other's tip (two merges from a shared history).
    sh_git(&repo, &["branch", "a"]);
    sh_git(&repo, &["branch", "b"]);
    sh_git(&repo, &["checkout", "-q", "a"]);
    write(&repo, "a.txt", "a1\n");
    commit(&repo, "a1");
    sh_git(&repo, &["checkout", "-q", "b"]);
    write(&repo, "b.txt", "b1\n");
    commit(&repo, "b1");
    let a_tip = sh_git(&repo, &["rev-parse", "a"]).trim().to_string();
    let b_tip = sh_git(&repo, &["rev-parse", "b"]).trim().to_string();
    // a merges b's tip.
    sh_git(&repo, &["checkout", "-q", "a"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "a<-b", &b_tip]);
    write(&repo, "a.txt", "a2\n");
    commit(&repo, "a2");
    // b merges a's earlier tip.
    sh_git(&repo, &["checkout", "-q", "b"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "b<-a", &a_tip]);
    write(&repo, "b.txt", "b2\n");
    commit(&repo, "b2");
    // main advances too, so there are three concurrent lanes.
    sh_git(&repo, &["checkout", "-q", "main"]);
    write(&repo, "m.txt", "m1\n");
    commit(&repo, "m1");

    assert_roundtrip(&repo, tmp.path(), "criss");
}

#[test]
fn lane_roundtrip_empty_tree_branch() {
    let empty_oid = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("empty");
    init(&repo);

    // Initial empty commit: its tree IS the empty tree.
    commit(&repo, "empty root");
    assert_eq!(
        sh_git(&repo, &["rev-parse", "HEAD^{tree}"]).trim(),
        empty_oid
    );
    // Populate, then a commit that deletes all files (empty tree again),
    // then repopulate.
    write(&repo, "a.txt", "hello\n");
    write(&repo, "d/b.txt", "world\n");
    commit(&repo, "add files");
    sh_git(&repo, &["rm", "-q", "-r", "."]);
    let delete_all = commit(&repo, "delete everything");
    assert_eq!(
        sh_git(&repo, &["rev-parse", &format!("{delete_all}^{{tree}}")]).trim(),
        empty_oid,
        "delete-all commit is not the empty tree"
    );
    write(&repo, "c.txt", "again\n");
    commit(&repo, "re-add");

    // A concurrent branch that also empties out, to exercise an empty
    // lane child alongside a live one.
    sh_git(&repo, &["checkout", "-q", "-b", "side", "main~2"]);
    write(&repo, "side.txt", "s\n");
    commit(&repo, "side add");
    sh_git(&repo, &["rm", "-q", "-r", "."]);
    commit(&repo, "side empty");
    sh_git(&repo, &["checkout", "-q", "main"]);

    let store = gitdepot::lanestore::LaneStore::encode_repo(
        &repo,
        &tmp.path().join("ls-empty"),
        3,
    )
    .unwrap();
    // The delete-all commit reconstructs to the empty tree oid exactly.
    assert_eq!(store.tree_oid_of_commit(&delete_all).unwrap(), empty_oid);

    assert_roundtrip(&repo, tmp.path(), "empty");
}
