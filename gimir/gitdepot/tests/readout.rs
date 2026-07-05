//! `TipReadout` — the RO-attachment readout over a gitdepot store
//! (ATTACH-CONVERGENCE.md chip 2): tip from frame 0 alone, prefix
//! nesting, and the paid-up-front `for_commit` history walk.
//!
//! Needs a `git` binary.

use std::path::Path;
use std::process::Command;

use depot::variant::{Blob, Readout, ReadoutKind};
use gitdepot::readout::TipReadout;

fn sh_git(repo: &Path, args: &[&str]) {
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
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Two commits: v1 writes README + src/main.rs, v2 (tip) edits
/// src/main.rs and adds docs/guide.md.
fn build_store(tmp: &Path) -> gitdepot::ImportOutcome {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    sh_git(&repo, &["init", "-q", "-b", "main"]);
    sh_git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README"), "v1 readme\n").unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "v1"]);
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() { println!(); }\n").unwrap();
    std::fs::write(repo.join("docs/guide.md"), "guide\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "v2"]);
    gitdepot::import(&repo, &tmp.join("store"), 3).unwrap()
}

#[test]
fn tip_readout_serves_frame_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    build_store(tmp.path());
    let r = TipReadout::new(&tmp.path().join("store"), "");

    assert_eq!(
        r.children(&[]),
        vec![b"README".to_vec(), b"docs".to_vec(), b"src".to_vec()]
    );
    assert_eq!(r.entry(&[b"src"]).unwrap().kind, ReadoutKind::Branch);
    let main = r.entry(&[b"src", b"main.rs"]).unwrap();
    assert_eq!(main.kind, ReadoutKind::Leaf);
    // Tip content — the v2 edit, not v1.
    assert_eq!(
        r.blob(&[b"src", b"main.rs"]),
        Some(Blob::Bytes(b"fn main() { println!(); }\n".to_vec()))
    );
    assert_eq!(r.blob(&[b"docs", b"guide.md"]), Some(Blob::Bytes(b"guide\n".to_vec())));
    // Misses.
    assert_eq!(r.entry(&[b"nope"]), None);
    assert_eq!(r.blob(&[b"src"]), None);
    assert!(r.children(&[b"README"]).is_empty());
}

#[test]
fn prefix_nests_the_tree() {
    let tmp = tempfile::TempDir::new().unwrap();
    build_store(tmp.path());
    let r = TipReadout::new(&tmp.path().join("store"), "deps/repo");

    assert_eq!(r.children(&[]), vec![b"deps".to_vec()]);
    assert_eq!(
        r.blob(&[b"deps", b"repo", b"README"]),
        Some(Blob::Bytes(b"v1 readme\n".to_vec()))
    );
    // The un-nested location no longer resolves.
    assert_eq!(r.entry(&[b"README"]), None);
}

#[test]
fn for_commit_walks_to_history() {
    let tmp = tempfile::TempDir::new().unwrap();
    build_store(tmp.path());
    let store = tmp.path().join("store");
    let (meta, _) = gitdepot::chain::read_store(&store).unwrap();
    // meta.commits is newest-first: [v2, v1].
    let v1_sha = &meta.commits[1].sha;

    let r = TipReadout::for_commit(&store, v1_sha, "").unwrap().unwrap();
    assert_eq!(r.children(&[]), vec![b"README".to_vec(), b"src".to_vec()]);
    assert_eq!(
        r.blob(&[b"src", b"main.rs"]),
        Some(Blob::Bytes(b"fn main() {}\n".to_vec()))
    );
    assert_eq!(r.entry(&[b"docs"]), None, "v2's addition must not leak into v1");

    assert!(
        TipReadout::for_commit(&store, "0000000000", "").unwrap().is_none(),
        "unknown sha is a miss"
    );
}

#[test]
fn unreadable_store_is_a_miss() {
    let tmp = tempfile::TempDir::new().unwrap();
    let r = TipReadout::new(&tmp.path().join("no-such-store"), "");
    assert_eq!(r.entry(&[]), None);
    assert!(r.children(&[]).is_empty());
}
