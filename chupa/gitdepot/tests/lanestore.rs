//! git.git-scale proof for the union lane store (`encode_repo_union`):
//! encode a real git.git mirror, reopen from disk, and reconstruct EVERY
//! commit's tree SHA-exact — reporting wall/RSS/size. Plus a cheap
//! topology-only stat. Both `#[ignore]` so the normal suite never pays for
//! them; the small-repo SHA-exact round-trips live in `lanestore_union.rs`.
//!
//! Run by name:
//!   cargo test -p gitdepot --release --test lanestore -- --ignored --nocapture gitgit_proof_union
//! Repo/scratch overridable via GITGIT_REPO / GITGIT_SCRATCH.
//!
//! Needs a `git` binary.

use std::path::Path;
use std::process::Command;

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let m = e.metadata().unwrap();
            total += if m.is_dir() { dir_size(&e.path()) } else { m.len() };
        }
    }
    total
}

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmHWM:")
                .map(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().unwrap_or(0))
        })
        .unwrap_or(0)
}

fn gitgit_repo() -> String {
    std::env::var("GITGIT_REPO").unwrap_or_else(|_| {
        "/tmp/claude-0/-home-user-sarun/5df6fa05-fb5d-5959-9227-2afc1158ad07/scratchpad/gitgit.git"
            .to_string()
    })
}

/// The hard gate: encode git.git via the union encoder, reopen from disk,
/// and reconstruct every commit's tree SHA-exact from persisted state.
#[test]
#[ignore]
fn gitgit_proof_union() {
    let repo = gitgit_repo();
    let repo = Path::new(&repo);
    assert!(repo.exists(), "git.git mirror not found at {}", repo.display());
    let scratch = std::env::var("GITGIT_SCRATCH").unwrap_or_else(|_| {
        "/tmp/claude-0/-home-user-sarun/5df6fa05-fb5d-5959-9227-2afc1158ad07/scratchpad/gitgit-lane-union"
            .to_string()
    });
    let dir = Path::new(&scratch);
    let _ = std::fs::remove_dir_all(dir);

    let pack = dir_size(&repo.join("objects/pack"));

    // (1) encode.
    let t0 = std::time::Instant::now();
    let store =
        gitdepot::lanestore::LaneStore::encode_repo_union(repo, dir, 3).expect("encode git.git");
    let encode_secs = t0.elapsed().as_secs_f64();
    let n_rev = store.n_rev();
    let n_lanes = (0..n_rev).map(|r| store.lane_of(r)).max().map(|m| m + 1).unwrap_or(0);
    drop(store); // prove reopen serves from disk

    // (2) reopen and reconstruct EVERY commit SHA-exact.
    let store = gitdepot::lanestore::LaneStore::open(dir).expect("reopen git.git store");
    assert_eq!(store.n_rev(), n_rev, "reopened rev count");
    let map_out = sh_git(repo, &["log", "--branches", "--tags", "--format=%H %T"]);
    let want: std::collections::HashMap<&str, &str> =
        map_out.lines().filter_map(|l| l.split_once(' ')).collect();
    let t1 = std::time::Instant::now();
    let mut checked = 0usize;
    for rev in 0..store.n_rev() {
        let sha = store.sha_at(rev).to_string();
        let got = store
            .tree_oid_at(rev)
            .unwrap_or_else(|e| panic!("reconstruct rev {rev} ({sha}): {e}"));
        let w = want.get(sha.as_str()).unwrap_or_else(|| panic!("no expected tree for {sha}"));
        assert_eq!(&got, w, "commit {sha}: tree oid mismatch");
        checked += 1;
    }
    let recon_secs = t1.elapsed().as_secs_f64();

    let store_bytes = dir_size(dir);
    let uncompressed = store.uncompressed_record_bytes().unwrap();
    let rss = peak_rss_kb();

    println!("\n==== git.git union lane-store proof ====");
    println!("commits (revisions):   {n_rev}");
    println!("commits reconstructed: {checked}  (SHA-exact: {})", checked == n_rev);
    println!("lanes (compacted):     {n_lanes}");
    println!("encode wall:           {encode_secs:.1}s");
    println!("reconstruct wall:      {recon_secs:.1}s (every commit, from disk)");
    println!("peak RSS:              {:.1} MB", rss as f64 / 1024.0);
    println!("git pack:              {:.1} MB", pack as f64 / 1e6);
    println!("union store on disk:   {:.1} MB", store_bytes as f64 / 1e6);
    println!("  uncompressed records:{:.1} MB", uncompressed as f64 / 1e6);
    println!("store/pack ratio:      {:.2}x", store_bytes as f64 / pack as f64);
    println!("========================================\n");
    assert_eq!(checked, n_rev, "not every commit reconstructed");
}

/// Cheap topology-only stats (no tree building): total lanes, peak
/// concurrent live lanes (the compacted bitmap width), and lanes live at
/// the end.
#[test]
#[ignore]
fn gitgit_lane_stats() {
    let repo = gitgit_repo();
    let repo = Path::new(&repo);
    let out =
        sh_git(repo, &["rev-list", "--parents", "--topo-order", "--reverse", "--branches", "--tags"]);
    let mut idx = std::collections::HashMap::new();
    let mut parents: Vec<Vec<usize>> = Vec::new();
    for line in out.lines() {
        let mut it = line.split(' ');
        let sha = it.next().unwrap();
        let i = parents.len();
        idx.insert(sha.to_string(), i);
        parents.push(it.filter_map(|p| idx.get(p).copied()).collect());
    }
    let n = parents.len();
    let a = gitdepot::lanes::assign_lanes(&parents);
    let n_lanes = a.n_lanes() as usize;
    let birth: Vec<usize> = a.span.iter().map(|s| s.0).collect();
    let mut death = vec![n; n_lanes];
    for i in 0..n {
        for &p in parents[i].iter().skip(1) {
            let l = a.lane_of[p] as usize;
            if i < death[l] {
                death[l] = i;
            }
        }
    }
    let (_, width) = gitdepot::lanes::compact_lanes(&birth, &death, n);
    let end_live = (0..n_lanes).filter(|&l| death[l] == n).count();
    println!("\n==== git.git lane topology ====");
    println!("commits:              {n}");
    println!("total lanes (monotonic): {n_lanes}");
    println!("compacted width (peak concurrent): {width}");
    println!("lanes live at end:    {end_live}");
    println!("===============================\n");
}

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
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}
