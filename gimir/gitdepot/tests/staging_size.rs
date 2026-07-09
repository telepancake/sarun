//! Record-size invariant: a TREES reverse delta is sized by the bytes
//! the commit (or, at a walk-order branch boundary, the branch) really
//! touched — NEVER by the size of the stable bulk of the tree. This is
//! the regression pin for the git.git ENOSPC blowup: chain records are
//! reverse deltas between chain-NEIGHBORING trees, so a walk order that
//! interleaves diverged lines (or a diff that stops short-circuiting
//! shared subtrees) re-embeds whole-tree divergence per record. With
//! the lineage-grouped walk order and COW-shared views, this fixture's
//! deltas must stay within a small multiple of the touched bytes.
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
    assert!(out.status.success(), "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn tree_records_scale_with_touched_bytes_not_tree_size() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh_git(&repo, &["init", "-q", "-b", "main"]);
    sh_git(&repo, &["config", "commit.gpgsign", "false"]);

    // 100KB of STABLE BULK no later commit touches: any record that
    // degenerates toward a full tree encode drags this in and blows
    // the budget immediately.
    let mut x: u64 = 0x9e3779b97f4a7c15;
    let mut rand_hex = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        format!("{x:016x}")
    };
    let mut bulk_bytes = 0usize;
    for f in 0..25 {
        let body: String = (0..250).map(|_| format!("{}\n", rand_hex())).collect();
        bulk_bytes += body.len();
        let dir = repo.join(format!("stable/d{}", f % 5));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("f{f}.txt")), body).unwrap();
    }
    assert!(bulk_bytes > 100_000);
    let commit = |msg: &str, date: &str| {
        sh_git(&repo, &["add", "-A"]);
        let out = Command::new("git")
            .arg("-C").arg(&repo)
            .args(["commit", "-q", "-m", msg])
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output().unwrap();
        assert!(out.status.success(), "commit: {}",
                String::from_utf8_lossy(&out.stderr));
    };
    commit("bulk", "2026-01-01T00:00:00Z");

    // Two branches, 8 commits each, TIMESTAMP-INTERLEAVED (a date- or
    // topo-ordered walk lands them alternating), each touching one
    // small file; then a merge. Touched bytes stay ~KBs total.
    let mut touched = 0usize;
    for i in 0..8 {
        for b in ["a", "b"] {
            if i == 0 {
                sh_git(&repo, &["checkout", "-q", "-B", b, "main"]);
            } else {
                sh_git(&repo, &["checkout", "-q", b]);
            }
            let body = format!("branch {b} revision {i}: {}\n", "x".repeat(48));
            touched += body.len();
            std::fs::write(repo.join(format!("topic-{b}.txt")), body).unwrap();
            let date = format!("2026-02-01T00:{:02}:{}0Z", i * 2 + (b == "b") as usize, 1);
            commit(&format!("{b}{i}"), &date);
        }
    }
    sh_git(&repo, &["checkout", "-q", "main"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "merge a", "a"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "merge b", "b"]);

    let store = tmp.path().join("store");
    gitdepot::import(&repo, &store, 3).unwrap();

    // The union tree store's TOTAL on-disk size must scale with the
    // touched bytes, not with N × full-tree. Its head full record
    // legitimately carries the whole tree (the ~100KB bulk) ONCE; every
    // other revision's reverse delta is sized by what that revision
    // touched. A degenerate encode that re-embeds the bulk per revision
    // blows this budget immediately.
    let st = gitdepot::store::Store::open(&store).unwrap();
    let ls = st.union().unwrap();
    let n_revs = ls.n_rev();
    assert!(n_revs >= 16, "fixture landed too few revisions: {n_revs}");
    fn dir_bytes(dir: &Path) -> usize {
        let mut total = 0;
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                total += dir_bytes(&e.path());
            } else {
                total += m.len() as usize;
            }
        }
        total
    }
    let on_disk = dir_bytes(&store.join("trees"));
    // One full record (~bulk) + a generous per-revision constant + a
    // small multiple of the truly-touched bytes.
    let budget = 2 * bulk_bytes + 4096 * n_revs + 8 * touched;
    assert!(
        on_disk < budget,
        "union tree store no longer scales with touched bytes: {on_disk} bytes \
         over {n_revs} revisions (touched ~{touched}, bulk {bulk_bytes}, budget {budget})"
    );
}
