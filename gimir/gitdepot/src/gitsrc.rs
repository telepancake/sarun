//! A live git object source (ASSEMBLY.md §6, final integration): read a real
//! repository's commit trees into [`LaneTree`]s via git plumbing, so the union
//! engine can be driven from actual git objects and its reconstruction checked
//! against git's own recorded tree oids.
//!
//! This uses the `git` CLI (`ls-tree` + `cat-file`) — the simplest correct
//! blob source. ASSEMBLY notes the eventual swap to `fetch-pack
//! --shallow-since` + `unpack-objects`; the [`read_commit_tree`] interface is
//! the seam that swap lives behind.

use crate::layer::{LaneEntry, LaneTree, Mode};
use std::process::Command;

fn git(repo: &std::path::Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {:?}: {}", args, String::from_utf8_lossy(&out.stderr)));
    }
    Ok(out.stdout)
}

fn mode_from_git(m: &[u8]) -> Option<Mode> {
    Mode::from_octal(m)
}

/// Read `commit`'s full recursive tree as a [`LaneTree`]: every file entry with
/// its mode, git oid, and content (a gitlink's "content" is its pinned commit
/// id, matching the store convention). Directories are implicit in the paths.
pub fn read_commit_tree(repo: &std::path::Path, commit: &str) -> Result<LaneTree, String> {
    // `-z`: NUL-terminated records "<mode> <type> <oid>\t<path>\0".
    let raw = git(repo, &["ls-tree", "-r", "-z", commit])?;
    let mut tree = LaneTree::new();
    for rec in raw.split(|&b| b == 0) {
        if rec.is_empty() {
            continue;
        }
        let tab = rec.iter().position(|&b| b == b'\t').ok_or("ls-tree: no tab")?;
        let path = rec[tab + 1..].to_vec();
        let head = &rec[..tab];
        // "<mode> <type> <oid>"
        let mut it = head.splitn(3, |&b| b == b' ');
        let mode_b = it.next().ok_or("ls-tree: no mode")?;
        let _typ = it.next().ok_or("ls-tree: no type")?;
        let oid_b = it.next().ok_or("ls-tree: no oid")?;
        let oid = oid_b.to_vec();
        let mode = mode_from_git(mode_b).ok_or_else(|| format!("bad mode {mode_b:?}"))?;
        let content = if mode == Mode::Gitlink {
            oid.clone() // the pinned submodule commit id
        } else {
            let oid_s = std::str::from_utf8(oid_b).map_err(|_| "oid utf8")?;
            git(repo, &["cat-file", "blob", oid_s])?
        };
        tree.insert(path, LaneEntry { mode, oid, content });
    }
    Ok(tree)
}

/// The git tree oid git itself records for `commit` — the ground truth a
/// reconstruction must match.
pub fn commit_tree_oid(repo: &std::path::Path, commit: &str) -> Result<String, String> {
    let out = git(repo, &["rev-parse", &format!("{commit}^{{tree}}")])?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer;
    use crate::shards::Shards;

    fn run(repo: &std::path::Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    fn run_env(repo: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    }

    /// Build a REAL git repo with two branches sharing most files (including an
    /// executable and a symlink), read both tips into lane trees, drive the
    /// sharded union engine, and check each branch reconstructs to git's own
    /// tree oid — the live-git SHA-exact end-to-end proof.
    #[test]
    fn real_git_repo_reconstructs_sha_exact() {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(_) => return, // no tmp: skip
        };
        let repo = dir.path();
        run(repo, &["init", "-q", "-b", "main"]);

        // main: README, an exec script, a symlink, a source file.
        std::fs::write(repo.join("README.md"), b"hello\n").unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(repo.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        run(repo, &["add", "-A"]);
        run(repo, &["update-index", "--chmod=+x", "run.sh"]);
        std::os::unix::fs::symlink("README.md", repo.join("link")).unwrap();
        run(repo, &["add", "link"]);
        run_env(repo, &["commit", "-qm", "c0"]);

        // topic branch: edit main.rs, add a file, drop the symlink.
        run(repo, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(repo.join("src/main.rs"), b"fn main() { work(); }\n").unwrap();
        std::fs::write(repo.join("NOTES"), b"notes\n").unwrap();
        std::fs::remove_file(repo.join("link")).unwrap();
        run(repo, &["rm", "-q", "link"]);
        run(repo, &["add", "-A"]);
        run_env(repo, &["commit", "-qm", "c1"]);

        // Read both branch tips as lane trees (lane 0 = main, lane 1 = topic).
        let main_tree = read_commit_tree(repo, "main").unwrap();
        let topic_tree = read_commit_tree(repo, "topic").unwrap();
        let lanes = vec![main_tree, topic_tree];

        // Ground truth from git.
        let want_main = commit_tree_oid(repo, "main").unwrap();
        let want_topic = commit_tree_oid(repo, "topic").unwrap();
        assert_ne!(want_main, want_topic);

        // Drive the sharded union engine across a few shard counts.
        for bits in [0u32, 2, 3] {
            let mut s = Shards::seed(bits, lanes.clone());
            assert_eq!(s.reconstruct_tree_oid(0).unwrap(), want_main, "main, {bits} bits");
            assert_eq!(s.reconstruct_tree_oid(1).unwrap(), want_topic, "topic, {bits} bits");

            // Advance main to topic's tree (a real delta) and re-check.
            let advanced = vec![lanes[1].clone(), lanes[1].clone()];
            s.advance(advanced);
            s.seal();
            assert_eq!(s.reconstruct_tree_oid(0).unwrap(), want_topic, "advanced main, {bits} bits");
        }

        // Also confirm the direct reference path matches git for a single tree.
        assert_eq!(layer::lanetree_tree_oid(&lanes[0]).unwrap(), want_main);
    }
}
