//! The direct-from-wire pipeline: a SELF-CONTAINED pack (what a `no-thin`
//! fetch delivers) encoded straight into a union store — no git repo read
//! anywhere in the path. Ground truth is the repo the pack was made from:
//!
//! - every commit's reconstructed tree oid is SHA-exact (ids were hashed by
//!   OUR scan, so this also proves the self-hashing);
//! - both delta flavors work: `--delta-base-offset` packs (OFS_DELTA) and
//!   default packs (REF_DELTA, exercising the deferred-base retry and the
//!   graph-rebuilt tree bases);
//! - annotated tags peel through the graph; a sharded pack store stays
//!   SHA-exact;
//! - a thin-shaped failure is LOUD (truncated ref chain ⇒ error, not a
//!   silently wrong store).
//!
//! Needs a `git` binary (as the ground truth and pack producer only).

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

fn build_repo(repo: &Path) {
    init(repo);
    for i in 0..10 {
        write(repo, &format!("dir{}/f{}.txt", i % 3, i), &format!("v0 of {i}\n"));
    }
    commit(repo, "base");
    sh_git(repo, &["tag", "-a", "-m", "annotated", "v1.0"]);
    sh_git(repo, &["checkout", "-q", "-b", "side"]);
    for i in 0..5 {
        write(repo, &format!("dir{}/f{}.txt", i % 3, i), &format!("side {i}\n"));
    }
    commit(repo, "side edits");
    sh_git(repo, &["checkout", "-q", "main"]);
    for i in 5..10 {
        write(repo, &format!("dir{}/f{}.txt", i % 3, i), &format!("main v1 {i}\n"));
    }
    commit(repo, "main edits");
    sh_git(repo, &["merge", "-q", "--no-edit", "side"]);
    write(repo, "dir0/post.txt", "post-merge\n");
    commit(repo, "post");
}

/// One self-contained pack of the whole repo. `ofs_delta` picks the delta
/// encoding on the wire.
fn make_pack(repo: &Path, out_dir: &Path, ofs_delta: bool) -> std::path::PathBuf {
    let pfx = out_dir.join("wire");
    let mut args = vec!["pack-objects", "--all", "-q"];
    if ofs_delta {
        args.push("--delta-base-offset");
    }
    let pfx_s = pfx.to_str().unwrap().to_string();
    args.push(&pfx_s);
    let sha = sh_git(repo, &args);
    out_dir.join(format!("wire-{}.pack", sha.trim()))
}

fn refs_of(repo: &Path) -> Vec<(String, String)> {
    gitdepot::parse_ref_lines(&sh_git(repo, &["show-ref"]))
        .into_iter()
        .map(|(name, sha)| (name, sha))
        .collect()
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

fn check_store(store: &LaneStore, repo: &Path) {
    for (commit, tree) in tree_oids(repo) {
        assert_eq!(store.tree_oid_of_commit(&commit).unwrap(), tree, "commit {commit}");
    }
}

#[test]
fn pack_union_is_sha_exact_both_delta_flavors() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    build_repo(&repo);
    let refs = refs_of(&repo);
    assert!(refs.iter().any(|(n, _)| n == "refs/tags/v1.0"), "annotated tag present");

    // show-ref lists the TAG object sha for refs/tags/v1.0 — peeling is the
    // graph's job.
    for (flavor, ofs) in [("ofs-delta", true), ("ref-delta", false)] {
        let dir = tmp.path().join(format!("s-{flavor}"));
        let pack = make_pack(&repo, tmp.path(), ofs);
        let (store, reads) =
            LaneStore::encode_pack_union_stats(&pack, &refs, &dir, 3).unwrap();
        assert!(reads > 0);
        check_store(&store, &repo);
        // Reopen from disk alone.
        drop(store);
        check_store(&LaneStore::open(&dir).unwrap(), &repo);
        std::fs::remove_file(&pack).unwrap();
    }
}

#[test]
fn pack_union_sharded_is_sha_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    build_repo(&repo);
    let pack = make_pack(&repo, tmp.path(), true);
    std::env::set_var("GITDEPOT_SHARD_BITS", "2");
    let got = LaneStore::encode_pack_union_stats(&pack, &refs_of(&repo), &tmp.path().join("s"), 3);
    std::env::remove_var("GITDEPOT_SHARD_BITS");
    let (store, _) = got.unwrap();
    assert_eq!(store.n_shards(), 4);
    check_store(&store, &repo);
}

#[test]
fn missing_ref_object_is_loud() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    build_repo(&repo);
    let pack = make_pack(&repo, tmp.path(), true);
    let refs = vec![(
        "refs/heads/ghost".to_string(),
        "1234567890123456789012345678901234567890".to_string(),
    )];
    let err = LaneStore::encode_pack_union_stats(&pack, &refs, &tmp.path().join("s"), 3)
        .err()
        .expect("ref outside the pack must refuse");
    assert!(format!("{err}").contains("not in pack"), "got: {err}");
}
