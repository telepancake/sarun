//! SHA-exact round-trip of the union-variant store path
//! (`LaneStore::encode_repo_union`). Builds a small multi-branch repo so
//! several lanes coexist and files both agree and diverge across them,
//! encodes it as ONE path-keyed union tree per revision, reopens the
//! store from disk alone, and reconstructs every commit's — and every
//! ref's — git tree oid, checking it equals the real object. A bug in
//! `variants::union`/`extract` or in how the temporal reverse-delta chain
//! carries the union states would mis-reconstruct some lane.
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
fn union_store_roundtrip_is_sha_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    init(&repo);

    // A file shared byte-identical across all branches (single version,
    // all-lanes bitmap), a file that diverges per branch (distinct
    // versions), and a nested dir. Interleave commits on two branches so
    // multiple lanes are live at once.
    write(&repo, "shared.txt", "same everywhere\n");
    write(&repo, "src/lib.rs", "fn base() {}\n");
    write(&repo, "diverge.txt", "base\n");
    commit(&repo, "base");
    let base = sh_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    sh_git(&repo, &["checkout", "-q", "-b", "feat", &base]);

    for i in 0..12 {
        let branch = if i % 2 == 0 { "main" } else { "feat" };
        sh_git(&repo, &["checkout", "-q", branch]);
        // diverge.txt differs per branch; src grows; shared stays put.
        write(&repo, "diverge.txt", &format!("{branch} rev {i}\n"));
        write(&repo, &format!("src/m{i}.rs", ), &format!("fn m{i}() {{}}\n"));
        commit(&repo, &format!("{branch}-{i}"));
    }
    // A merge, so a lane dies (absorbed as a non-first parent).
    sh_git(&repo, &["checkout", "-q", "main"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-X", "ours", "-m", "merge feat", "feat"]);
    // A lightweight tag, to exercise ref reconstruction.
    sh_git(&repo, &["tag", "v1"]);

    // Ground truth: every reachable commit's tree oid, and every ref's.
    let shas: Vec<String> =
        sh_git(&repo, &["rev-list", "--branches", "--tags"]).lines().map(str::to_string).collect();
    let want_tree: std::collections::HashMap<String, String> = shas
        .iter()
        .map(|s| (s.clone(), sh_git(&repo, &["rev-parse", &format!("{s}^{{tree}}")]).trim().to_string()))
        .collect();

    let dir = tmp.path().join("store");
    gitdepot::lanestore::LaneStore::encode_repo_union(&repo, &dir, 3).unwrap();
    // Reopen from disk alone — proves the persisted repr picks the union
    // extractor with no repo access.
    let store = gitdepot::lanestore::LaneStore::open(&dir).unwrap();

    assert_eq!(store.n_rev(), shas.len(), "revision count");
    for rev in 0..store.n_rev() {
        let sha = store.sha_at(rev).to_string();
        let got = store.tree_oid_at(rev).unwrap();
        assert_eq!(got, want_tree[&sha], "commit {sha} at rev {rev} mis-reconstructed");
    }

    // Every persisted ref resolves to the right tree from disk alone.
    for (name, sha) in store.refs() {
        let got = store.tree_oid_of_ref(name).unwrap();
        assert_eq!(got, want_tree[sha], "ref {name} mis-reconstructed");
    }
}
