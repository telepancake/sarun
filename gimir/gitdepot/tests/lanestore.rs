//! SHA-exact round-trip proof for the combined-state lockstep lane
//! encoder (`gitdepot::lanestore`). Builds several synthetic git repos —
//! linear, fork+merge, criss-cross, and an empty-tree (delete-all)
//! branch — encodes each into a lane store, then for EVERY commit
//! reconstructs its tree from the lane store and asserts the
//! reconstructed git tree oid equals `git rev-parse <commit>^{tree}`.
//! Also asserts the lockstep/prefix structure: each stored reverse-delta
//! record touches exactly one lane prefix.
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

fn init(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    sh_git(repo, &["init", "-q", "-b", "main"]);
    sh_git(repo, &["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, msg: &str) -> String {
    sh_git(repo, &["add", "-A"]);
    sh_git(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
    sh_git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

fn write(repo: &Path, path: &str, body: &str) {
    let p = repo.join(path);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

/// Every reachable commit's reconstructed tree oid must equal git's.
fn assert_roundtrip(repo: &Path, tmp: &Path, tag: &str) -> usize {
    let dir = tmp.join(format!("lanestore-{tag}"));
    let store = gitdepot::lanestore::LaneStore::encode_repo(repo, &dir, 3)
        .unwrap_or_else(|e| panic!("[{tag}] encode: {e}"));

    // Enumerate every commit git considers reachable (same scope the
    // encoder walked).
    let shas: Vec<String> =
        sh_git(repo, &["rev-list", "--branches", "--tags"]).lines().map(str::to_string).collect();
    assert!(!shas.is_empty(), "[{tag}] no commits");

    for sha in &shas {
        let want = sh_git(repo, &["rev-parse", &format!("{sha}^{{tree}}")]).trim().to_string();
        let got = store
            .tree_oid_of_commit(sha)
            .unwrap_or_else(|e| panic!("[{tag}] reconstruct {sha}: {e}"));
        assert_eq!(got, want, "[{tag}] commit {sha}: tree oid mismatch");
    }

    // Lockstep/prefix structure: every stored reverse-delta record (all
    // but the full head at position 0) touches EXACTLY ONE lane prefix,
    // and it is the prefix of the lane advanced at that revision.
    let prefixes = store.record_prefixes().unwrap();
    let n = store.n_rev();
    assert_eq!(prefixes.len(), n, "[{tag}] record count != revision count");
    for pos in 1..prefixes.len() {
        assert_eq!(
            prefixes[pos].len(),
            1,
            "[{tag}] reverse-delta record at pos {pos} touched {} lanes (expected 1) — lanes oscillate",
            prefixes[pos].len()
        );
    }
    // The record at position `advance_record_pos(rev)` is the one lane
    // advanced at `rev`, and its single prefix is that lane's prefix.
    for rev in 1..n {
        let pos = store.advance_record_pos(rev);
        let want = store.lane_prefix(store.lane_of(rev));
        assert_eq!(prefixes[pos], vec![want], "[{tag}] rev {rev} record touches the wrong lane");
    }
    shas.len()
}

#[test]
fn lane_roundtrip_linear() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("linear");
    init(&repo);
    for i in 0..6 {
        write(&repo, "doc.txt", &format!("revision {i}\n"));
        write(&repo, "src/mod.rs", &format!("const REV: u32 = {i};\n"));
        commit(&repo, &format!("r{i}"));
    }
    let n = assert_roundtrip(&repo, tmp.path(), "linear");
    assert_eq!(n, 6);
}

#[test]
fn lane_roundtrip_fork_and_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("fork");
    init(&repo);
    write(&repo, "base.txt", "base\n");
    commit(&repo, "base");
    write(&repo, "main1.txt", "m1\n");
    commit(&repo, "m1");

    // A topic branch off the base with two commits, then merge back.
    sh_git(&repo, &["checkout", "-q", "-b", "topic", "main~1"]);
    write(&repo, "topic1.txt", "t1\n");
    commit(&repo, "t1");
    write(&repo, "topic2.txt", "t2\n");
    commit(&repo, "t2");
    sh_git(&repo, &["checkout", "-q", "main"]);
    write(&repo, "main2.txt", "m2\n");
    commit(&repo, "m2");
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);

    assert_roundtrip(&repo, tmp.path(), "fork");
}

#[test]
fn lane_roundtrip_criss_cross() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("criss");
    init(&repo);
    write(&repo, "f.txt", "0\n");
    commit(&repo, "root");

    // Two concurrent branches, each advanced, then a criss-cross: each
    // branch merges the other's tip (two merges from a shared history).
    sh_git(&repo, &["branch", "a"]);
    sh_git(&repo, &["branch", "b"]);
    sh_git(&repo, &["checkout", "-q", "a"]);
    write(&repo, "a.txt", "a1\n");
    commit(&repo, "a1");
    sh_git(&repo, &["checkout", "-q", "b"]);
    write(&repo, "b.txt", "b1\n");
    commit(&repo, "b1");
    let a_tip = sh_git(&repo, &["rev-parse", "a"]).trim().to_string();
    let b_tip = sh_git(&repo, &["rev-parse", "b"]).trim().to_string();
    // a merges b's tip.
    sh_git(&repo, &["checkout", "-q", "a"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "a<-b", &b_tip]);
    write(&repo, "a.txt", "a2\n");
    commit(&repo, "a2");
    // b merges a's earlier tip.
    sh_git(&repo, &["checkout", "-q", "b"]);
    sh_git(&repo, &["merge", "-q", "--no-ff", "-m", "b<-a", &a_tip]);
    write(&repo, "b.txt", "b2\n");
    commit(&repo, "b2");
    // main advances too, so there are three concurrent lanes.
    sh_git(&repo, &["checkout", "-q", "main"]);
    write(&repo, "m.txt", "m1\n");
    commit(&repo, "m1");

    assert_roundtrip(&repo, tmp.path(), "criss");
}

#[test]
fn lane_roundtrip_empty_tree_branch() {
    let empty_oid = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("empty");
    init(&repo);

    // Initial empty commit: its tree IS the empty tree.
    commit(&repo, "empty root");
    assert_eq!(
        sh_git(&repo, &["rev-parse", "HEAD^{tree}"]).trim(),
        empty_oid
    );
    // Populate, then a commit that deletes all files (empty tree again),
    // then repopulate.
    write(&repo, "a.txt", "hello\n");
    write(&repo, "d/b.txt", "world\n");
    commit(&repo, "add files");
    sh_git(&repo, &["rm", "-q", "-r", "."]);
    let delete_all = commit(&repo, "delete everything");
    assert_eq!(
        sh_git(&repo, &["rev-parse", &format!("{delete_all}^{{tree}}")]).trim(),
        empty_oid,
        "delete-all commit is not the empty tree"
    );
    write(&repo, "c.txt", "again\n");
    commit(&repo, "re-add");

    // A concurrent branch that also empties out, to exercise an empty
    // lane child alongside a live one.
    sh_git(&repo, &["checkout", "-q", "-b", "side", "main~2"]);
    write(&repo, "side.txt", "s\n");
    commit(&repo, "side add");
    sh_git(&repo, &["rm", "-q", "-r", "."]);
    commit(&repo, "side empty");
    sh_git(&repo, &["checkout", "-q", "main"]);

    let store = gitdepot::lanestore::LaneStore::encode_repo(
        &repo,
        &tmp.path().join("ls-empty"),
        3,
    )
    .unwrap();
    // The delete-all commit reconstructs to the empty tree oid exactly.
    assert_eq!(store.tree_oid_of_commit(&delete_all).unwrap(), empty_oid);

    assert_roundtrip(&repo, tmp.path(), "empty");
}

// -------------------------------------------------------- variant-delta

/// SHA-exact round-trip over the VARIANT-DELTA path: encode with
/// `encode_repo_variant`, reconstruct every reachable commit's tree
/// (variant lanes via base+delta), assert the git tree oid. Returns the
/// store so callers can measure its frame-resident size. Does NOT assert
/// the one-lane-prefix property — variant-delta deliberately breaks it
/// (a base advance re-expresses its variants at that revision).
fn assert_variant_roundtrip(repo: &Path, tmp: &Path, tag: &str) -> gitdepot::lanestore::LaneStore {
    let dir = tmp.join(format!("variant-{tag}"));
    let store = gitdepot::lanestore::LaneStore::encode_repo_variant(repo, &dir, 3)
        .unwrap_or_else(|e| panic!("[{tag}] encode: {e}"));
    let shas: Vec<String> =
        sh_git(repo, &["rev-list", "--branches", "--tags"]).lines().map(str::to_string).collect();
    assert!(!shas.is_empty(), "[{tag}] no commits");
    for sha in &shas {
        let want = sh_git(repo, &["rev-parse", &format!("{sha}^{{tree}}")]).trim().to_string();
        let got = store
            .tree_oid_of_commit(sha)
            .unwrap_or_else(|e| panic!("[{tag}] reconstruct {sha}: {e}"));
        assert_eq!(got, want, "[{tag}] commit {sha}: tree oid mismatch");
    }
    store
}

/// A base branch plus N near-identical feature branches (each forks the
/// base tip and changes exactly ONE small file over a large shared blob).
/// All N+1 tips stay live lanes. Returns the N feature branch names.
fn build_near_identical(repo: &Path, n: usize, shared: &str) -> Vec<String> {
    init(repo);
    // A large shared blob plus several smaller shared files every branch
    // carries verbatim — the content whose N-fold duplication variant-delta
    // must collapse. Multiple shared blobs keep the base/variant Jaccard
    // overlap comfortably above the cutoff (a single unique file per branch
    // then leaves overlap well past 0.5).
    write(repo, "shared/big.dat", shared);
    for k in 0..4 {
        write(repo, &format!("shared/c{k}.txt"), &format!("common {k}\n"));
    }
    commit(repo, "base");
    let base_tip = sh_git(repo, &["rev-parse", "HEAD"]).trim().to_string();
    let mut names = Vec::new();
    for i in 0..n {
        let br = format!("feat{i}");
        sh_git(repo, &["checkout", "-q", "-b", &br, &base_tip]);
        // Change exactly one small file; the big shared blob is untouched,
        // so this branch's tree overlaps the base past the cutoff.
        write(repo, &format!("feat_{i}.txt"), &format!("feature {i} body\n"));
        commit(repo, &format!("feat {i}"));
        names.push(br);
    }
    sh_git(repo, &["checkout", "-q", "main"]);
    names
}

#[test]
fn variant_roundtrip_and_size_reduction() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("variant");
    // ~120 KB of incompressible-ish shared content so the size numbers
    // reflect real duplication, not a run of zeros.
    let shared: String = (0u32..30_000).map(|k| ((k.wrapping_mul(2654435761) >> 24) as u8 % 26 + b'a') as char).collect();
    let n = 6;
    build_near_identical(&repo, n, &shared);

    // (i) SHA-exact for every commit, variant lanes reconstructed via
    // base+delta.
    let store = assert_variant_roundtrip(&repo, tmp.path(), "near");

    // (ii) SIZE: the variant-delta store's frame-resident UNCOMPRESSED
    // content is ~one shared tree + small deltas, dramatically smaller
    // than the independents-only control that stores all N+1 full trees
    // side by side.
    let control_dir = tmp.path().join("control-indep");
    let control = gitdepot::lanestore::LaneStore::encode_repo(&repo, &control_dir, 3).unwrap();
    let variant_bytes = store.uncompressed_record_bytes().unwrap();
    let control_bytes = control.uncompressed_record_bytes().unwrap();
    let shared_len = shared.len() as u64;

    // The control holds ~ (N+1) copies of the shared blob; the variant
    // store holds ~1. Expect a multi-x reduction and an absolute bound of
    // a small multiple of a single tree.
    assert!(
        (variant_bytes as f64) < (control_bytes as f64) * 0.5,
        "variant {variant_bytes} not < half of control {control_bytes} \
         (shared blob {shared_len} B, N={n})"
    );
    assert!(
        variant_bytes < shared_len * 3,
        "variant {variant_bytes} exceeds 3x a single shared tree ({shared_len} B) \
         — variants are not delta-encoded"
    );
    assert!(
        control_bytes > shared_len * (n as u64),
        "control {control_bytes} should carry ~N+1 full copies of the shared blob ({shared_len} B)"
    );
}

#[test]
fn variant_roundtrip_empty_tree_variant() {
    // A base branch with content and a variant branch that empties out to
    // the empty tree — the variant delta must reconstruct the empty tree
    // SHA-exact (existence fix), reconstructed as base+delta.
    let empty_oid = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("variant-empty");
    init(&repo);
    write(&repo, "shared/big.dat", &"x".repeat(4096));
    write(&repo, "a.txt", "a\n");
    write(&repo, "b.txt", "b\n");
    commit(&repo, "base");
    let base_tip = sh_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    // A near-identical variant (one changed file) to force a real
    // variant group alongside the empty one.
    sh_git(&repo, &["checkout", "-q", "-b", "feat", &base_tip]);
    write(&repo, "c.txt", "c\n");
    commit(&repo, "feat");

    // A branch that deletes everything -> empty tree.
    sh_git(&repo, &["checkout", "-q", "-b", "wipe", &base_tip]);
    sh_git(&repo, &["rm", "-q", "-r", "."]);
    let wipe = commit(&repo, "wipe all");
    assert_eq!(
        sh_git(&repo, &["rev-parse", &format!("{wipe}^{{tree}}")]).trim(),
        empty_oid
    );
    sh_git(&repo, &["checkout", "-q", "main"]);

    let store = assert_variant_roundtrip(&repo, tmp.path(), "empty-variant");
    assert_eq!(store.tree_oid_of_commit(&wipe).unwrap(), empty_oid);
}
