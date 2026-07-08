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

/// The per-commit change list between `a` and `b` (as `git diff-tree -r`):
/// `(path, Some((mode, content)))` for an add/modify/typechange,
/// `(path, None)` for a delete. Only changed blobs are fetched. Feed straight
/// to [`crate::layer::encode_single_lane_delta`].
pub fn diff_changes(
    repo: &std::path::Path,
    a: &str,
    b: &str,
) -> Result<Vec<(Vec<u8>, Option<(Mode, Vec<u8>)>)>, String> {
    // ":<om> <nm> <oo> <no> <status>\0<path>\0" records, no rename detection.
    let raw = git(repo, &["diff-tree", "-r", "-z", "--no-renames", a, b])?;
    let mut out = Vec::new();
    let mut it = raw.split(|&b| b == 0).peekable();
    while let Some(meta) = it.next() {
        if meta.is_empty() {
            break;
        }
        // meta starts with ':'
        let fields: Vec<&[u8]> = meta[1..].split(|&b| b == b' ').collect();
        let new_mode = fields.get(1).copied().unwrap_or(b"");
        let new_oid = fields.get(3).copied().unwrap_or(b"");
        let status = fields.get(4).and_then(|s| s.first()).copied().unwrap_or(b'?');
        let path = it.next().ok_or("diff-tree: no path")?.to_vec();
        if status == b'D' {
            out.push((path, None));
        } else {
            let mode = mode_from_git(new_mode).ok_or_else(|| format!("bad mode {new_mode:?}"))?;
            let content = if mode == Mode::Gitlink {
                new_oid.to_vec()
            } else {
                let oid_s = std::str::from_utf8(new_oid).map_err(|_| "oid utf8")?;
                git(repo, &["cat-file", "blob", oid_s])?
            };
            out.push((path, Some((mode, content))));
        }
    }
    Ok(out)
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

    fn zstd_len(bytes: &[u8], level: i32) -> usize {
        zstd::bulk::compress(bytes, level).unwrap().len()
    }

    /// Measure the engine on a REAL repo's first-parent history: sum of
    /// per-commit single-lane deltas (built from `git diff-tree`, only changed
    /// blobs) + the tip refPrefix, raw and zstd, vs git's own pack. Point it at
    /// a repo with GITDEPOT_MEASURE_REPO; skips if unset. Prints numbers.
    #[test]
    fn measure_against_real_history() {
        let repo = match std::env::var("GITDEPOT_MEASURE_REPO") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => return,
        };
        // First-parent history oldest→newest.
        let out = git(&repo, &["rev-list", "--reverse", "--first-parent", "HEAD"]).unwrap();
        let commits: Vec<String> =
            String::from_utf8_lossy(&out).lines().map(|s| s.to_string()).collect();
        assert!(commits.len() > 1);

        // All delta layers concatenated (the f1 frame), and their raw total.
        let mut all_deltas: Vec<u8> = Vec::new();
        let mut delta_raw = 0usize;
        for w in commits.windows(2) {
            let changes = diff_changes(&repo, &w[0], &w[1]).unwrap();
            let d = layer::encode_single_lane_delta(&changes);
            delta_raw += d.len();
            all_deltas.extend_from_slice(&d);
        }
        // The tip refPrefix (f0): the full tree of HEAD as a single-lane union.
        let tip = read_commit_tree(&repo, "HEAD").unwrap();
        // Reconstruct sanity: the tip encodes to git's tree oid.
        assert_eq!(layer::lanetree_tree_oid(&tip).unwrap(), commit_tree_oid(&repo, "HEAD").unwrap());
        let tip_entries: Vec<_> =
            tip.iter().map(|(p, e)| (p.clone(), e.mode, e.content.clone())).collect();
        let refprefix = layer::encode_lane(&tip_entries);

        let git_pack = git(&repo, &["count-objects", "-v"]).unwrap();
        let pack_kib: usize = String::from_utf8_lossy(&git_pack)
            .lines()
            .find_map(|l| l.strip_prefix("size-pack: ").and_then(|v| v.trim().parse().ok()))
            .unwrap_or(0);

        let lvl = 19;
        let deltas_z = zstd_len(&all_deltas, lvl);
        let refprefix_z = zstd_len(&refprefix, lvl);
        let mut combined = all_deltas.clone();
        combined.extend_from_slice(&refprefix);
        let combined_z = zstd_len(&combined, lvl);
        eprintln!("=== gitdepot size measurement ({} commits) ===", commits.len());
        eprintln!("delta layers (f1):  raw {:>10}  zstd {:>10}", delta_raw, deltas_z);
        eprintln!("refPrefix   (f0):  raw {:>10}  zstd {:>10}", refprefix.len(), refprefix_z);
        eprintln!("f0+f1 separate zstd-19:           {:>10}", deltas_z + refprefix_z);
        eprintln!("f0+f1 one zstd-19 stream:         {:>10}", combined_z);
        eprintln!("f1 alone (complete store) zstd-19:{:>10}", deltas_z);
        eprintln!("git pack (size-pack):             {:>10} ({} KiB)", pack_kib * 1024, pack_kib);
    }
}
