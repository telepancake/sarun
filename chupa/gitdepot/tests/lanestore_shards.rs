//! §9 sharding: the union split across `2^shard-bits` path-hashed chains
//! must be INVISIBLE in everything a reader sees. Builds one multi-branch
//! repo, encodes it unsharded and with 4 shards, and checks:
//!
//! - every revision's reconstructed tree oid is SHA-exact in both stores
//!   (the gather across shards reproduces identity, §9);
//! - `checkout_entries` yields byte-identical entries IN THE SAME ORDER
//!   from both stores (the k-way shard merge reproduces the unsharded
//!   canonical container order);
//! - an incremental update onto a sharded store stays SHA-exact (per-shard
//!   boundary reconstruction);
//! - a sharded store reopens from disk alone with its shard count.
//!
//! Needs a `git` binary.

use std::path::Path;
use std::process::Command;

use gitdepot::lanestore::LaneStore;

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

/// Build a repo with enough distinct paths that 4 shards all get some,
/// two branches so lanes coexist and die, and edits/removals/mode bits.
fn build_repo(repo: &Path) {
    init(repo);
    for i in 0..12 {
        write(repo, &format!("dir{}/f{}.txt", i % 4, i), &format!("v0 of {i}\n"));
    }
    write(repo, "shared.txt", "same everywhere\n");
    commit(repo, "base");
    sh_git(repo, &["checkout", "-q", "-b", "side"]);
    for i in 0..6 {
        write(repo, &format!("dir{}/f{}.txt", i % 4, i), &format!("side of {i}\n"));
    }
    write(repo, "side-only.bin", "side\n");
    commit(repo, "side edits");
    sh_git(repo, &["checkout", "-q", "main"]);
    for i in 6..12 {
        write(repo, &format!("dir{}/f{}.txt", i % 4, i), &format!("main v1 of {i}\n"));
    }
    std::fs::remove_file(repo.join("shared.txt")).unwrap();
    commit(repo, "main edits + removal");
    sh_git(repo, &["merge", "-q", "--no-edit", "side"]);
    write(repo, "dir1/post.txt", "post-merge\n");
    commit(repo, "post-merge tweak");
}

fn tree_oids(repo: &Path) -> Vec<(String, String)> {
    sh_git(repo, &["log", "--all", "--format=%H %T"])
        .lines()
        .map(|l| {
            let (c, t) = l.split_once(' ').unwrap();
            (c.to_string(), t.to_string())
        })
        .collect()
}

fn checkout_flat(store: &LaneStore, sha: &str) -> Vec<(Vec<u8>, u32, Vec<u8>)> {
    let mut out = Vec::new();
    store
        .checkout_entries(sha, b"", &mut |path, mode, content| {
            out.push((path.to_vec(), mode.octal().iter().fold(0u32, |a, &b| a * 8 + (b - b'0') as u32), content.to_vec()));
            Ok(())
        })
        .unwrap();
    out
}

#[test]
fn sharded_store_is_invisible_in_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    build_repo(&repo);
    let want = tree_oids(&repo);
    assert!(want.len() >= 4);

    // Unsharded reference.
    std::env::remove_var("GITDEPOT_SHARD_BITS");
    let flat = LaneStore::encode_repo_union(&repo, &tmp.path().join("s0"), 3).unwrap();
    // 4 shards.
    std::env::set_var("GITDEPOT_SHARD_BITS", "2");
    let sharded = LaneStore::encode_repo_union(&repo, &tmp.path().join("s2"), 3).unwrap();
    std::env::remove_var("GITDEPOT_SHARD_BITS");
    assert_eq!(sharded.n_shards(), 4, "shard-bits parameter must take");

    for (commit, tree) in &want {
        assert_eq!(&flat.tree_oid_of_commit(commit).unwrap(), tree, "unsharded {commit}");
        assert_eq!(&sharded.tree_oid_of_commit(commit).unwrap(), tree, "sharded {commit}");
        // The k-way shard merge must reproduce the unsharded stream exactly
        // — same entries, same canonical order, same bytes.
        assert_eq!(
            checkout_flat(&flat, commit),
            checkout_flat(&sharded, commit),
            "checkout stream diverges for {commit}"
        );
    }

    // Reopen from disk alone: shard count persisted.
    drop(sharded);
    let reopened = LaneStore::open(&tmp.path().join("s2")).unwrap();
    assert_eq!(reopened.n_shards(), 4);
    let (c0, t0) = &want[0];
    assert_eq!(&reopened.tree_oid_of_commit(c0).unwrap(), t0);

    // Incremental update onto the sharded store (per-shard boundary
    // reconstruction), then SHA-exactness of old + new revisions.
    for i in 0..4 {
        write(&repo, &format!("dir{}/new{}.txt", i, i), &format!("update {i}\n"));
    }
    write(&repo, "dir2/f6.txt", "updated body\n");
    commit(&repo, "update commit");
    let updated = LaneStore::update(&repo, &tmp.path().join("s2"), 3).unwrap();
    for (commit, tree) in &tree_oids(&repo) {
        assert_eq!(&updated.tree_oid_of_commit(commit).unwrap(), tree, "post-update {commit}");
    }
}
