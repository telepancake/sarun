//! End-to-end straightedge test: build a real git repo, import it into a
//! store, export it back, and check SHA-exact fidelity — plus the
//! anti-sabotage storage assertion (DEPOT-DESIGN.md §8): the refPrefix
//! chain over near-identical trees must be far smaller than standalone
//! per-layer compression, or the encoding has not rendered the design.
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
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repo with revision-chain shape: a sizable file receiving small
/// successive edits (the workload the refPrefix chain exists for), plus
/// adds/removes/mode changes/symlinks/subdirs and a branch.
fn build_fixture(repo: &Path) -> Vec<String> {
    std::fs::create_dir_all(repo).unwrap();
    sh_git(repo, &["init", "-q", "-b", "main"]);
    sh_git(repo, &["config", "commit.gpgsign", "false"]);

    // A ~50 KB base text edited slightly each commit. Lines are
    // pseudo-random hex — barely compressible within one revision, but
    // ~identical across revisions: the shape refPrefix chaining exists
    // for (standalone must pay ~full size per frame, the chain ~the
    // delta).
    let mut x: u64 = 0x243f6a8885a308d3;
    let mut rand_hex = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        format!("{x:016x}")
    };
    let base: String = (0..800)
        .map(|i| format!("line {i:04}: {}{}{}{}\n", rand_hex(), rand_hex(), rand_hex(), rand_hex()))
        .collect();

    for i in 0..12 {
        let mut doc = base.clone();
        // Small edit per revision: replace one line, append one.
        doc = doc.replace(
            &format!("line {:04}:", i * 7),
            &format!("LINE {:04}!", i * 7),
        );
        doc.push_str(&format!("appended in revision {i}\n"));
        std::fs::write(repo.join("doc.txt"), &doc).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/mod.rs"), format!("pub const REV: u32 = {i};\n")).unwrap();
        if i == 3 {
            std::fs::write(repo.join("tool.sh"), "#!/bin/sh\necho hi\n").unwrap();
            sh_git(repo, &["add", "tool.sh"]);
            sh_git(repo, &["update-index", "--chmod=+x", "tool.sh"]);
        }
        if i == 5 {
            std::os::unix::fs::symlink("doc.txt", repo.join("link")).unwrap();
        }
        if i == 8 {
            std::fs::remove_file(repo.join("tool.sh")).unwrap();
            sh_git(repo, &["rm", "-q", "tool.sh"]);
        }
        sh_git(repo, &["add", "-A"]);
        sh_git(repo, &["commit", "-q", "-m", &format!("revision {i}\n\nbody of {i}")]);
    }
    // A branch with its own commit (exercises multiple refs + topo order).
    sh_git(repo, &["checkout", "-q", "-b", "side", "main~2"]);
    std::fs::write(repo.join("side.txt"), "side branch content\n").unwrap();
    sh_git(repo, &["add", "-A"]);
    sh_git(repo, &["commit", "-q", "-m", "side commit"]);
    sh_git(repo, &["checkout", "-q", "main"]);

    sh_git(repo, &["for-each-ref", "--format=%(objectname) %(refname)"])
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn roundtrip_sha_exact_and_chain_compresses() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("src-repo");
    let store = tmp.path().join("store");
    let out = tmp.path().join("out-repo");

    let refs_before = build_fixture(&repo);

    let outcome = gitdepot::import(&repo, &store, 3).unwrap();
    let r = &outcome.report;
    assert_eq!(r.commits, 13);

    // §8 anti-sabotage assertions: the chain must actually render the
    // design. Near-identical successive trees ⇒ refPrefix frames cost
    // ~the delta; standalone frames cost ~the full tree each.
    assert!(
        r.ref_prefix_chain * 4 < r.standalone,
        "refPrefix chain ({}) not <4x smaller than standalone ({})",
        r.ref_prefix_chain,
        r.standalone
    );
    assert!(r.standalone < r.raw, "zstd made things bigger?");
    // The chain should be in the same league as the solid bound.
    assert!(
        r.ref_prefix_chain < r.solid * 3,
        "chain ({}) way off the solid bound ({})",
        r.ref_prefix_chain,
        r.solid
    );

    // Export and verify: every ref regenerates to the SAME commit id.
    let refs_after = gitdepot::export(&store, &out).unwrap();
    let mut after: Vec<String> = refs_after
        .iter()
        .map(|r| format!("{} {}", r.sha, r.name))
        .collect();
    after.sort();
    let mut before = refs_before;
    before.sort();
    assert_eq!(after, before, "round-trip changed commit ids");

    // And the exported repo is a valid, checkout-able repo.
    let head_doc = Command::new("git")
        .arg("-C")
        .arg(&out)
        .args(["show", "main:doc.txt"])
        .output()
        .unwrap();
    assert!(head_doc.status.success());
    assert!(String::from_utf8_lossy(&head_doc.stdout).contains("appended in revision 11"));
}

#[test]
fn import_refuses_unsupported_shapes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh_git(&repo, &["init", "-q", "-b", "main"]);
    sh_git(&repo, &["config", "commit.gpgsign", "false"]);
    sh_git(&repo, &["config", "tag.gpgsign", "false"]);
    std::fs::write(repo.join("f"), "x").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "c"]);
    sh_git(&repo, &["tag", "-a", "-m", "annotated", "v1"]);
    match gitdepot::import(&repo, &tmp.path().join("store"), 3) {
        Err(gitdepot::Error::Unsupported(_)) => {}
        Err(e) => panic!("expected Unsupported, got: {e}"),
        Ok(_) => panic!("import of annotated-tag repo unexpectedly succeeded"),
    }
}
