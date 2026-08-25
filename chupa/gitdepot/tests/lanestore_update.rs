//! Incremental O(new) update of the union lane store
//! (`LaneStore::update`). Imports a PREFIX of a multi-branch history, then
//! updates with the REST, and proves:
//!   1. every revision reconstructs SHA-exact from the stored bytes after the
//!      update (correctness across the prepend + boundary reconstruction);
//!   2. the updated store is EQUIVALENT to a single full encode of the whole
//!      history — every commit's reconstructed tree oid matches (both match
//!      git, hence each other), and stored commits keep their lanes (§8);
//!   3. the update touched O(new) work, not O(history): it advanced only the
//!      new revisions, and issued far fewer git object reads than a full
//!      encode of the same final repo.
//!
//! Needs a `git` binary.

use std::collections::HashMap;
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

/// Make `n` interleaved commits on main/feat starting at index `from`: a file
/// identical across branches (all-lanes), a per-branch diverging file, and a
/// growing nested tree — so multiple lanes stay live and files both agree and
/// diverge.
fn interleave(repo: &Path, from: usize, n: usize) {
    for i in from..from + n {
        let branch = if i % 2 == 0 { "main" } else { "feat" };
        sh_git(repo, &["checkout", "-q", branch]);
        write(repo, "diverge.txt", &format!("{branch} rev {i}\n"));
        write(repo, &format!("src/m{i}.rs"), &format!("fn m{i}() {{}}\n"));
        commit(repo, &format!("{branch}-{i}"));
    }
}

fn tree_oids(repo: &Path) -> HashMap<String, String> {
    sh_git(repo, &["rev-list", "--branches", "--tags"])
        .lines()
        .map(|s| (s.to_string(), sh_git(repo, &["rev-parse", &format!("{s}^{{tree}}")]).trim().to_string()))
        .collect()
}

#[test]
fn incremental_update_is_sha_exact_o_new_and_equivalent_to_full() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    init(&repo);

    // Base + a feat branch off it.
    write(&repo, "shared.txt", "same everywhere\n");
    write(&repo, "src/lib.rs", "fn base() {}\n");
    write(&repo, "diverge.txt", "base\n");
    commit(&repo, "base");
    let base = sh_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    sh_git(&repo, &["checkout", "-q", "-b", "feat", &base]);

    // PHASE 1 — a large prefix (so a full re-encode is expensive relative to
    // the small update below).
    interleave(&repo, 0, 60);
    let import_dir = tmp.path().join("store");
    let (imported, _) =
        gitdepot::lanestore::LaneStore::encode_repo_union_stats(&repo, &import_dir, 3).unwrap();
    let old_n = imported.n_rev();
    let old_lane: Vec<_> = (0..old_n).map(|r| imported.lane_of(r)).collect();
    let old_sha: Vec<String> = (0..old_n).map(|r| imported.sha_at(r).to_string()).collect();
    drop(imported);

    // PHASE 2 — a small increment: a few more commits and a merge that kills a
    // lane, so the update exercises births/deaths at the new frontier.
    interleave(&repo, 60, 6);
    sh_git(&repo, &["checkout", "-q", "main"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-X", "ours", "-m", "merge feat", "feat"]);
    sh_git(&repo, &["tag", "v1"]);

    // Incremental update of the prefix store.
    let (updated, new_revs, reads_update) =
        gitdepot::lanestore::LaneStore::update_stats(&repo, &import_dir, 3).unwrap();
    let final_n = updated.n_rev();
    drop(updated);

    // A separate FULL encode of the SAME final repo, for the equivalence +
    // O(new) comparison.
    let full_dir = tmp.path().join("full");
    let (_full, reads_full) =
        gitdepot::lanestore::LaneStore::encode_repo_union_stats(&repo, &full_dir, 3).unwrap();

    // Ground truth.
    let want = tree_oids(&repo);

    // (1)+(2) SHA-exact for every revision, from the UPDATED store reopened
    // from disk — and equal to git (hence equivalent to the full encode).
    let store = gitdepot::lanestore::LaneStore::open(&import_dir).unwrap();
    assert_eq!(store.n_rev(), want.len(), "revision count after update");
    for rev in 0..store.n_rev() {
        let sha = store.sha_at(rev).to_string();
        assert_eq!(store.tree_oid_at(rev).unwrap(), want[&sha], "updated store: commit {sha} (rev {rev})");
    }
    for (name, sha) in store.refs() {
        assert_eq!(store.tree_oid_of_ref(name).unwrap(), want[sha], "updated store: ref {name}");
    }
    // The full-encode store must reconstruct identically (proves equivalence).
    let full = gitdepot::lanestore::LaneStore::open(&full_dir).unwrap();
    assert_eq!(full.n_rev(), want.len(), "full encode revision count");
    for rev in 0..full.n_rev() {
        let sha = full.sha_at(rev).to_string();
        assert_eq!(full.tree_oid_at(rev).unwrap(), want[&sha], "full store: commit {sha}");
    }

    // (2) §8 stability: the stored prefix kept its exact lanes (no renumber).
    for rev in 0..old_n {
        assert_eq!(store.sha_at(rev), old_sha[rev], "stored commit {rev} order changed");
        assert_eq!(store.lane_of(rev), old_lane[rev], "stored commit {rev} lane renumbered");
    }

    // (3) O(new): the update advanced ONLY the new revisions...
    assert_eq!(new_revs, final_n - old_n, "update advanced exactly the new revisions");
    assert!(new_revs < old_n, "the increment is much smaller than the prefix");
    // ...and read far fewer git objects than a full encode of the same repo
    // (boundary frontier + new work, not O(history)).
    assert!(
        reads_update * 2 < reads_full,
        "update reads ({reads_update}) not << full-encode reads ({reads_full}) — not O(new)"
    );
}
