//! Spill-vs-RAM staging equivalence: the GITDEPOT_SPILL_BOUND
//! threshold selects the staging MEDIUM only — an update (not just an
//! import) forced past the bound must land the same objects with the
//! same prepend count as the same update staged fully in RAM. Lives
//! in its own test binary: the env var is process-global, so no other
//! test may share the process.
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
        if i == upto - 1 {
            sh_git(repo, &["tag", "-a", "-m", "t", &format!("t{i}")]);
        }
    }
}

#[test]
fn spilled_update_equals_in_ram_update_with_one_prepend_per_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    build(&repo, 5);

    // Both stores share the same base import (in RAM — bound unset).
    let a = tmp.path().join("store-a");
    let b = tmp.path().join("store-b");
    gitdepot::import(&repo, &a, 3).unwrap();
    gitdepot::import(&repo, &b, 3).unwrap();

    // A LARGE update (40 commits + a tag): store A stages in RAM,
    // store B is forced to spill at 1 byte — every record goes through
    // the scratch file.
    build(&repo, 45);
    let oa = gitdepot::update(&repo, &a, 3).unwrap();
    std::env::set_var("GITDEPOT_SPILL_BOUND", "1");
    let ob = gitdepot::update(&repo, &b, 3).unwrap();
    std::env::remove_var("GITDEPOT_SPILL_BOUND");
    assert_eq!(oa.new_commits, 40);
    assert_eq!(ob.new_commits, 40);
    // The batch invariant, both media: COMMITS + TAGS + REFLOG = 3
    // prepends in the commit-side store for this update, never more
    // under spill. (The union tree store lands separately in store/trees.)
    assert_eq!(oa.depot_prepends, 3);
    assert_eq!(ob.depot_prepends, 3, "spill must not change prepend structure");
    // No staging residue.
    assert!(!a.join("staging").exists() && !b.join("staging").exists());

    // Object-identical stores (same history, same order ⇒ same
    // records, same indices, byte-identical TREES records).
    let sa = gitdepot::store::Store::open(&a).unwrap();
    let sb = gitdepot::store::Store::open(&b).unwrap();
    assert_eq!(sa.commit_records().unwrap(), sb.commit_records().unwrap());
    assert_eq!(sa.tag_records().unwrap(), sb.tag_records().unwrap());
    assert_eq!(sa.ref_rows().unwrap(), sb.ref_rows().unwrap());
    let tree_oids = |st: &gitdepot::store::Store| -> Vec<String> {
        let ls = st.union().unwrap();
        (0..ls.n_rev()).map(|r| ls.tree_oid_at(r).unwrap()).collect()
    };
    assert_eq!(tree_oids(&sa), tree_oids(&sb), "union trees diverge under spill");
}
