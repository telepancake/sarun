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

    // A stable bulk: most of a real tree does NOT change per commit
    // (imagine linux.git) — this is what makes delta records small.
    for f in 0..48 {
        let dir = repo.join(format!("stable/d{}", f % 4));
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = (0..200)
            .map(|i| format!("s{f:02}-{i:03}: {}{}\n", rand_hex(), rand_hex()))
            .collect();
        std::fs::write(dir.join(format!("f{f}.txt")), body).unwrap();
    }

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

    // §8 anti-sabotage assertions: the rest form must actually render
    // the design. Near-identical successive trees ⇒ delta records cost
    // ~the edit; full records cost ~the whole tree each.
    assert!(
        r.delta_raw * 4 < r.full_raw,
        "delta records ({}) not <4x smaller than full records ({}) — diff isn't deltaing",
        r.delta_raw,
        r.full_raw
    );
    assert!(
        r.view_ref_chain * 4 < r.full_standalone,
        "stored chain ({}) not <4x smaller than full standalone ({})",
        r.view_ref_chain,
        r.full_standalone
    );
    // The stored (view-anchored) chain should be in the same league as
    // the solid bound.
    assert!(
        r.view_ref_chain < r.solid_full * 3,
        "stored chain ({}) way off the solid bound ({})",
        r.view_ref_chain,
        r.solid_full
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
fn incremental_update_appends_and_exports_sha_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("src-repo");
    let store = tmp.path().join("store");
    let out = tmp.path().join("out-repo");

    build_fixture(&repo);
    gitdepot::import(&repo, &store, 3).unwrap();
    let chain_before = std::fs::read(store.join("chain")).unwrap();

    // Two new commits on main, on top of everything.
    for i in 12..14 {
        std::fs::write(repo.join("doc.txt"), format!("rewritten in revision {i}\n")).unwrap();
        std::fs::write(repo.join(format!("new-{i}.txt")), format!("added {i}\n")).unwrap();
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-q", "-m", &format!("revision {i}")]);
    }
    let refs_now: Vec<String> = sh_git(&repo, &["for-each-ref", "--format=%(objectname) %(refname)"])
        .lines()
        .map(str::to_string)
        .collect();

    let o = gitdepot::update(&repo, &store, 3).unwrap();
    assert_eq!((o.new_commits, o.total_commits), (2, 15));

    // Incrementality is structural: the old chain minus its former
    // frame 0 must survive verbatim as the new chain's tail.
    let chain_after = std::fs::read(store.join("chain")).unwrap();
    let zlen0 = u32::from_le_bytes(chain_before[4..8].try_into().unwrap()) as usize;
    let old_tail = &chain_before[8 + zlen0..];
    assert!(
        chain_after.ends_with(old_tail),
        "old frames were rewritten — update is not incremental"
    );

    // Round-trip: export regenerates every ref SHA-exactly.
    let refs_after = gitdepot::export(&store, &out).unwrap();
    let mut after: Vec<String> = refs_after.iter().map(|r| format!("{} {}", r.sha, r.name)).collect();
    after.sort();
    let mut before = refs_now;
    before.sort();
    assert_eq!(after, before, "post-update round-trip changed commit ids");
    let head_doc = Command::new("git")
        .arg("-C")
        .arg(&out)
        .args(["show", "main:doc.txt"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&head_doc.stdout).contains("rewritten in revision 13"));

    // No new commits: update is a refs-only no-op.
    let o2 = gitdepot::update(&repo, &store, 3).unwrap();
    assert_eq!(o2.new_commits, 0);
    assert_eq!(std::fs::read(store.join("chain")).unwrap(), chain_after);

    // Rewritten history (amend) is refused, store untouched.
    sh_git(&repo, &["commit", "-q", "--amend", "-m", "amended"]);
    match gitdepot::update(&repo, &store, 3) {
        Err(gitdepot::Error::Unsupported(_)) => {}
        Err(e) => panic!("expected Unsupported, got: {e}"),
        Ok(_) => panic!("non-fast-forward update unexpectedly succeeded"),
    }
    assert_eq!(std::fs::read(store.join("chain")).unwrap(), chain_after);
}

#[test]
fn mirror_loop_clones_updates_and_survives_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");

    build_fixture(&origin);
    // A path stands in for the remote URL (same git transport surface).
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.total_commits, 13);
    assert!(!o.reimported);
    assert!(root.join("repo.git/HEAD").exists(), "bare mirror clone missing");

    // New commit on origin → incremental update through the fetch.
    std::fs::write(origin.join("more.txt"), "more\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "more"]);
    let chain_before = std::fs::read(root.join("store/chain")).unwrap();
    let o2 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!((o2.update.new_commits, o2.reimported), (1, false));
    let zlen0 = u32::from_le_bytes(chain_before[4..8].try_into().unwrap()) as usize;
    assert!(
        std::fs::read(root.join("store/chain")).unwrap().ends_with(&chain_before[8 + zlen0..]),
        "mirror update rewrote old frames"
    );

    // Origin rewrites history → mirror falls back to full re-import
    // and still exports SHA-exact against the NEW truth.
    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended"]);
    let o3 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert!(o3.reimported, "rewritten remote should force re-import");
    let out = tmp.path().join("out");
    let refs = gitdepot::export(&root.join("store"), &out).unwrap();
    let tip = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert!(refs.iter().any(|r| r.name == "refs/heads/main" && r.sha == tip));

    // The fetch buffer is DERIVED state: delete repo.git, add a commit
    // upstream — the next mirror re-seeds the buffer from the store
    // (SHA-exact export) and fetches only the delta, no re-clone/
    // re-import.
    std::fs::remove_dir_all(root.join("repo.git")).unwrap();
    std::fs::write(origin.join("even-more.txt"), "x\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "even more"]);
    let o4 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!((o4.update.new_commits, o4.reimported), (1, false),
               "reseeded buffer should yield an incremental update");
    assert!(root.join("repo.git/HEAD").exists(), "buffer not rebuilt");

    // Frugal mode: successful update leaves the store as the single
    // on-disk copy.
    std::fs::write(origin.join("last.txt"), "y\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "last"]);
    let o5 = gitdepot::mirror_opts(origin.to_str().unwrap(), &root, true)
        .unwrap();
    assert_eq!((o5.update.new_commits, o5.reimported), (1, false));
    assert!(!root.join("repo.git").exists(), "frugal left the buffer");
    let tip2 = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert!(o5.update.refs.iter()
        .any(|r| r.name == "refs/heads/main" && r.sha == tip2));
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
