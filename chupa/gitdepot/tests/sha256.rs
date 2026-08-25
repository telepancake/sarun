//! "New format" object ids: a `git init --object-format=sha256` repository
//! (64-hex, 32-byte oids) through BOTH ingest paths — the git-repo union
//! (native pack reader: sha256 idx layout, oid-width tree entries) and the
//! direct-from-wire pack-union (self-hashed sha256 ids, format inferred
//! from the refs' oid width) — reconstructing every commit's tree oid
//! SHA-256-exact. The format is never persisted: stored oid widths ARE the
//! format.
//!
//! Needs git ≥ 2.29 (sha256 object format).

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
    std::fs::create_dir_all(repo).unwrap();
    let out = Command::new("git")
        .args(["init", "-q", "-b", "main", "--object-format=sha256"])
        .arg(repo)
        .output()
        .expect("git init");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    sh_git(repo, &["config", "commit.gpgsign", "false"]);
    for i in 0..8 {
        write(repo, &format!("d{}/f{}.txt", i % 2, i), &format!("v0 {i}\n"));
    }
    commit(repo, "base");
    sh_git(repo, &["tag", "-a", "-m", "t", "v1"]);
    sh_git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, "d0/f0.txt", "side\n");
    commit(repo, "side");
    sh_git(repo, &["checkout", "-q", "main"]);
    write(repo, "d1/f1.txt", "main\n");
    commit(repo, "main move");
    sh_git(repo, &["merge", "-q", "--no-edit", "side"]);
}

fn tree_oids(repo: &Path) -> Vec<(String, String)> {
    sh_git(repo, &["log", "--all", "--format=%H %T"])
        .lines()
        .map(|l| {
            let (c, t) = l.split_once(' ').unwrap();
            assert_eq!(c.len(), 64, "sha256 repo yields 64-hex ids");
            (c.to_string(), t.to_string())
        })
        .collect()
}

fn check(store: &LaneStore, repo: &Path) {
    for (commit, tree) in tree_oids(repo) {
        assert_eq!(store.tree_oid_of_commit(&commit).unwrap(), tree, "commit {commit}");
    }
}

#[test]
fn sha256_repo_union_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    build_repo(&repo);
    // Pack everything so the NATIVE reader exercises the sha256 idx layout
    // (loose-only would bypass it).
    sh_git(&repo, &["repack", "-adq"]);
    let store = LaneStore::encode_repo_union(&repo, &tmp.path().join("s"), 3).unwrap();
    check(&store, &repo);
    drop(store);
    let reopened = LaneStore::open(&tmp.path().join("s")).unwrap();
    assert_eq!(reopened.hash_kind(), gitdepot::HashKind::Sha256);
    check(&reopened, &repo);
}

#[test]
fn sha256_pack_union_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("r");
    build_repo(&repo);
    let pfx = tmp.path().join("wire").to_str().unwrap().to_string();
    let sha = sh_git(&repo, &["pack-objects", "--all", "-q", "--delta-base-offset", &pfx]);
    let pack = tmp.path().join(format!("wire-{}.pack", sha.trim()));
    let refs = gitdepot::parse_ref_lines(&sh_git(&repo, &["show-ref"]));
    assert!(refs.iter().all(|(_, s)| s.len() == 64));
    let (store, _) =
        LaneStore::encode_pack_union_stats(&pack, &refs, &tmp.path().join("s"), 3).unwrap();
    check(&store, &repo);
}
