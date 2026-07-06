//! Stub-lifecycle tests (THE STUB CONTRACT, lib.rs): the persistent
//! fetch buffer is gone — repo.git at rest is a KB-scale shallow stub
//! rebuilt from the store, updates fetch only the delta, a missing
//! stub is regenerated (sha-asserted) from the store, and the laddered
//! bootstrap bounds the buffer peak by the rung, not the history.
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

fn dir_kb(dir: &Path) -> u64 {
    fn walk(d: &Path, acc: &mut u64) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, acc);
            } else if let Ok(m) = e.metadata() {
                *acc += m.len();
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n / 1024
}

/// History-heavy, tip-light: each commit REPLACES a ~64KB
/// incompressible file, so the full clone carries every revision but
/// any single snapshot is one file. Three date-spread annotated tags
/// give the ladder its rungs.
fn build_heavy_history(repo: &Path, commits: usize) {
    std::fs::create_dir_all(repo).unwrap();
    sh_git(repo, &["init", "-q", "-b", "main"]);
    sh_git(repo, &["config", "commit.gpgsign", "false"]);
    sh_git(repo, &["config", "tag.gpgsign", "false"]);
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for i in 0..commits {
        let mut body = String::new();
        for _ in 0..4000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            body.push_str(&format!("{x:016x}"));
        }
        std::fs::write(repo.join("big.bin"), body).unwrap();
        std::fs::write(repo.join("small.txt"), format!("rev {i}\n")).unwrap();
        sh_git(repo, &["add", "-A"]);
        sh_git(repo, &["commit", "-q", "-m", &format!("c{i}")]);
        match i {
            9 => { sh_git(repo, &["tag", "-a", "-m", "v1", "v1"]); }
            19 => { sh_git(repo, &["tag", "-a", "-m", "v2", "v2"]); }
            _ => {}
        }
    }
    sh_git(repo, &["tag", "-a", "-m", "v3", "v3"]);
}

#[test]
fn stub_lifecycle_bounds_the_buffer_and_fetches_only_the_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_heavy_history(&origin, 30);
    // Reference: what the old persistent buffer would have cost.
    let full = tmp.path().join("full.git");
    sh_git(tmp.path(), &["clone", "-q", "--mirror",
                         origin.to_str().unwrap(), full.to_str().unwrap()]);
    let full_kb = dir_kb(&full);
    assert!(full_kb > 1000, "fixture too light to prove anything: {full_kb}K");

    // First contact (laddered by default).
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.total_commits, 30);
    let repo = root.join("repo.git");
    // At rest: KB-scale stub, shallow-pinned at the tips.
    let stub_kb = dir_kb(&repo);
    assert!(stub_kb <= 256, "stub at rest is {stub_kb}K (> 256K)");
    assert!(repo.join("shallow").exists(), "stub must be shallow-pinned");
    assert_eq!(sh_git(&repo, &["rev-parse", "refs/heads/main"]).trim(),
               sh_git(&origin, &["rev-parse", "main"]).trim());

    // Update after shrink: one new commit; the transient buffer peak
    // (snapshot + fetched pack) must stay well under the full clone —
    // only delta objects cross the wire.
    std::fs::write(origin.join("small.txt"), "rev 30\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "c30"]);
    let o2 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o2.update.new_commits, 1);
    assert!(o2.buffer_peak_kb > 0 && o2.buffer_peak_kb < full_kb / 3,
            "update peak {}K should be a fraction of the full clone {full_kb}K",
            o2.buffer_peak_kb);
    assert!(dir_kb(&repo) <= 256, "buffer not re-pinned to stub size");

    // No-op tick: nothing advertised → nothing fetched, stub untouched.
    let o3 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o3.update.new_commits, 0);
    assert_eq!(o3.buffer_peak_kb, 0);
}

#[test]
fn missing_stub_is_rebuilt_from_the_store_sha_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_heavy_history(&origin, 12);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let repo = root.join("repo.git");
    std::fs::remove_dir_all(&repo).unwrap();

    // Advance upstream so the tick has real work: the rebuild path
    // (assemble tip commit bytes from the store, assert shas) must
    // yield a stub the fetch/update runs against.
    std::fs::write(origin.join("small.txt"), "after\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "after"]);
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.new_commits, 1);
    assert!(repo.join("HEAD").exists() && repo.join("shallow").exists());
    for name in ["refs/heads/main", "refs/tags/v1", "refs/tags/v3"] {
        assert_eq!(sh_git(&repo, &["rev-parse", name]).trim(),
                   sh_git(&origin, &["rev-parse", name]).trim(),
                   "{name} diverges after rebuild");
    }
    // Regenerated tag object peels identically (chain objects intact).
    assert_eq!(sh_git(&repo, &["rev-parse", "refs/tags/v3^{}"]).trim(),
               sh_git(&origin, &["rev-parse", "refs/tags/v3^{}"]).trim());
}

#[test]
fn laddered_bootstrap_bounds_the_peak_buffer_by_the_rung() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_heavy_history(&origin, 30);
    let full = tmp.path().join("full.git");
    sh_git(tmp.path(), &["clone", "-q", "--mirror",
                         origin.to_str().unwrap(), full.to_str().unwrap()]);
    let full_kb = dir_kb(&full);

    let o = gitdepot::mirror_opts(
        origin.to_str().unwrap(),
        &root,
        gitdepot::MirrorOpts { tag_wave: 1, ..Default::default() },
    )
    .unwrap();
    assert_eq!(o.update.total_commits, 30);
    // 3 tag rungs + the converge rung.
    assert_eq!(o.rungs.len(), 4, "rungs: {:?}", o.rungs);
    // Each rung ≈ a third of the history; no moment may hold the
    // whole clone. Bound: largest rung's share plus stub/snapshot
    // overhead, asserted as ⅔ of the full clone.
    for (i, r) in o.rungs.iter().enumerate() {
        assert!(r.buffer_peak_kb < full_kb * 2 / 3,
                "rung {i} peaked at {}K (full clone {full_kb}K): {:?}",
                r.buffer_peak_kb, o.rungs);
    }
    assert_eq!(o.rungs.iter().map(|r| r.new_commits).sum::<usize>(), 30);
    // Equivalence of outcome: export is SHA-exact against the origin.
    let out = tmp.path().join("out");
    gitdepot::export(&root.join("store"), &out).unwrap();
    let ffr = |r: &Path| sh_git(r, &["for-each-ref", "--format=%(objectname) %(refname)"]);
    assert_eq!(ffr(&out), ffr(&origin));
}
