//! Early-landing equivalence: an ingest driven past the chunked-flush
//! thresholds (GITDEPOT_TEST_FRAME_BOUND / GITDEPOT_TEST_BATCH_BOUND
//! forced tiny) must land the SAME store content as the same history
//! staged and flushed once — early-landed chunks are byte-for-byte
//! ordinary prepends, only the prepend count scales with batch bytes
//! (wikimak/depot SPEC, compose_f1 doc: "A batch whose entries alone
//! dwarf the seal threshold must not land as one frame"). Also proves
//! multi-batch COMMITS/TAGS records read back with stable contiguous
//! indices, and that no dirty flag or staging residue survives a
//! completed ingest. Lives in its own test binary: the env vars are
//! process-global, so no other test may share the process.
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
    assert!(out.status.success(), "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn build(repo: &Path, upto: usize) {
    if !repo.join(".git").exists() {
        std::fs::create_dir_all(repo).unwrap();
        sh_git(repo, &["init", "-q", "-b", "main"]);
        sh_git(repo, &["config", "commit.gpgsign", "false"]);
        sh_git(repo, &["config", "tag.gpgsign", "false"]);
    }
    let n: usize = sh_git(repo, &["rev-list", "--count", "--all"])
        .trim().parse().unwrap_or(0);
    for i in n..upto {
        std::fs::write(repo.join(format!("f{}.txt", i % 7)),
                       format!("content of revision {i}\nline\n")).unwrap();
        sh_git(repo, &["add", "-A"]);
        sh_git(repo, &["commit", "-q", "-m", &format!("c{i}")]);
        if i % 9 == 0 || i == upto - 1 {
            sh_git(repo, &["tag", "-a", "-m", "t", &format!("t{i}")]);
        }
    }
}

#[test]
fn early_landed_ingest_equals_single_flush_ingest() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    build(&repo, 5);

    // Both stores share the same small base import (below thresholds).
    let a = tmp.path().join("store-a");
    let b = tmp.path().join("store-b");
    gitdepot::import(&repo, &a, 3).unwrap();
    gitdepot::import(&repo, &b, 3).unwrap();

    // A LARGE update (40 commits + tags): store A stages and lands as
    // one flush per chain; store B is forced to early-land at every
    // record — TREES chunks per delta, COMMITS/TAGS batch records per
    // object.
    build(&repo, 45);
    let oa = gitdepot::update(&repo, &a, 3).unwrap();
    std::env::set_var("GITDEPOT_TEST_FRAME_BOUND", "1");
    std::env::set_var("GITDEPOT_TEST_BATCH_BOUND", "1");
    let ob = gitdepot::update(&repo, &b, 3).unwrap();
    std::env::remove_var("GITDEPOT_TEST_FRAME_BOUND");
    std::env::remove_var("GITDEPOT_TEST_BATCH_BOUND");
    assert_eq!(oa.new_commits, 40);
    assert_eq!(ob.new_commits, 40);
    // A never early-lands: one prepend per touched chain, as ever.
    assert_eq!(oa.depot_prepends, 4);
    // B chunked: prepend count scales with batch bytes per the SPEC —
    // strictly more prepends, one-ish per record here.
    assert!(ob.depot_prepends > oa.depot_prepends,
            "forced thresholds produced no early landing ({} prepends)",
            ob.depot_prepends);
    // A completed ingest leaves no crash state behind.
    assert!(!a.join("staging").exists() && !b.join("staging").exists());
    assert!(!gitdepot::store::ingest_interrupted(&b).unwrap(),
            "dirty_ingest survived a completed ingest");

    // Full content equivalence. Store::open also re-runs the
    // count/head-batch integrity check over the multi-batch chains.
    let sa = gitdepot::store::Store::open(&a).unwrap();
    let sb = gitdepot::store::Store::open(&b).unwrap();
    // (b) multi-batch COMMITS/TAGS read back with stable contiguous
    // indices: commit_records()/tag_records() assert idx == position.
    assert_eq!(sa.commit_records().unwrap(), sb.commit_records().unwrap());
    assert_eq!(sa.tag_records().unwrap(), sb.tag_records().unwrap());
    assert_eq!(sa.ref_rows().unwrap(), sb.ref_rows().unwrap());
    // Tip-biased point reads across every batch boundary.
    let n = sa.count(gitdepot::store::COMMITS).unwrap();
    for i in 0..n {
        assert_eq!(
            sa.commit_record_at(i).unwrap(),
            sb.commit_record_at(i).unwrap(),
            "commit {i} diverges across batch boundaries"
        );
    }
    // Every tree record AND every reconstructed view, byte-for-byte:
    // an early-landed head is replaced by the same bridge delta the
    // single flush would have staged, so even the chain records match.
    let tree_walk = |st: &gitdepot::store::Store| -> (Vec<Vec<u8>>, Vec<depot::View>) {
        let (mut recs, mut views) = (Vec::new(), Vec::new());
        st.walk_tree_views(None, &mut |_, rec, v| {
            recs.push(rec.to_vec());
            views.push(v.clone());
        })
        .unwrap();
        (recs, views)
    };
    let (ra, va) = tree_walk(&sa);
    let (rb, vb) = tree_walk(&sb);
    assert_eq!(va, vb, "tree views diverge under early landing");
    assert_eq!(ra, rb, "TREES records diverge under early landing");
}
