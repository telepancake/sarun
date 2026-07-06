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

/// Recursive on-disk size of the store (depot files + sqlite).
fn store_size(store: &Path) -> u64 {
    fn walk(p: &Path, total: &mut u64) {
        for e in std::fs::read_dir(p).unwrap().flatten() {
            let md = e.metadata().unwrap();
            if md.is_dir() {
                walk(&e.path(), total);
            } else {
                *total += md.len();
            }
        }
    }
    let mut total = 0;
    walk(store, &mut total);
    total
}

#[test]
fn roundtrip_sha_exact_and_chain_compresses() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("src-repo");
    let store = tmp.path().join("store");
    let out = tmp.path().join("out-repo");

    let refs_before = build_fixture(&repo);

    let outcome = gitdepot::import_opts(&repo, &store, 3, true).unwrap();
    let r = outcome.report.as_ref().expect("report requested");
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
    // Batch invariant: N new commits land as ONE prepend per touched
    // chain (trees, commits, reflog) — never one cycle per record.
    assert!(
        o.depot_prepends <= 3,
        "2-commit update made {} depot prepends (expected ≤3, one per chain)",
        o.depot_prepends
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

    // No new commits: update is a no-op (no new records anywhere).
    let o2 = gitdepot::update(&repo, &store, 3).unwrap();
    assert_eq!(o2.new_commits, 0);
    assert_eq!(o2.total_commits, 15);
    assert_eq!(o2.depot_prepends, 0, "no-op update wrote chain records");
}

/// Batch prepend ≡ sequential prepends: the same three commits landed
/// as one 3-commit update and as three 1-commit updates must produce
/// byte-identical tree/commit records with identical stable indices.
#[test]
fn batch_update_equals_sequential_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    build_fixture(&repo);
    let tips: Vec<String> = (0..3)
        .map(|i| {
            std::fs::write(repo.join("doc.txt"), format!("batch rev {i}\n")).unwrap();
            std::fs::write(repo.join(format!("b{i}.txt")), format!("b{i}\n")).unwrap();
            sh_git(&repo, &["add", "-A"]);
            sh_git(&repo, &["commit", "-q", "-m", &format!("batch {i}")]);
            sh_git(&repo, &["rev-parse", "HEAD"]).trim().to_string()
        })
        .collect();

    // Store A: import at the pre-batch tip, then ONE update with all 3
    // commits. Store B: same import, then three 1-commit updates.
    let orig = sh_git(&repo, &["rev-parse", "main~3"]).trim().to_string();
    sh_git(&repo, &["checkout", "-q", "--detach"]); // free `main` for -f
    let a = tmp.path().join("store-a");
    let b = tmp.path().join("store-b");
    sh_git(&repo, &["branch", "-f", "main", &orig]);
    gitdepot::import(&repo, &a, 3).unwrap();
    sh_git(&repo, &["branch", "-f", "main", &tips[2]]);
    let oa = gitdepot::update(&repo, &a, 3).unwrap();
    assert_eq!(oa.new_commits, 3);
    assert!(oa.depot_prepends <= 3, "batch update made {} prepends", oa.depot_prepends);

    sh_git(&repo, &["branch", "-f", "main", &orig]);
    gitdepot::import(&repo, &b, 3).unwrap();
    for tip in &tips {
        sh_git(&repo, &["branch", "-f", "main", tip]);
        let o = gitdepot::update(&repo, &b, 3).unwrap();
        assert_eq!(o.new_commits, 1);
    }

    let sa = gitdepot::store::Store::open(&a).unwrap();
    let sb = gitdepot::store::Store::open(&b).unwrap();
    let ra = sa.commit_records().unwrap();
    let rb = sb.commit_records().unwrap();
    assert_eq!(ra, rb, "commit records diverge between batch and sequential");
    let va = sa.tree_views(None).unwrap();
    let vb = sb.tree_views(None).unwrap();
    assert_eq!(va.len(), vb.len(), "tree counts diverge");
    for (i, (x, y)) in va.iter().zip(vb.iter()).enumerate() {
        let ex = depot::codec::encode(&depot::diff(None, Some(x)));
        let ey = depot::codec::encode(&depot::diff(None, Some(y)));
        assert_eq!(ex, ey, "tree at newest-first position {i} diverges");
    }
}

#[test]
fn mirror_loop_clones_updates_and_follows_rewrites() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");

    build_fixture(&origin);
    // A path stands in for the remote URL (same git transport surface).
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.total_commits, 13);
    assert!(root.join("repo.git/HEAD").exists(), "bare mirror clone missing");

    // New commit on origin → incremental update through the fetch.
    std::fs::write(origin.join("more.txt"), "more\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "more"]);
    let o2 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o2.update.new_commits, 1);

    // Origin rewrites history → still just an update: new records +
    // repointed refs; the store keeps the old tip resolvable and the
    // export serves the NEW truth SHA-exact.
    let store = root.join("store");
    let old_main = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().0;
    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended"]);
    let o3 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o3.update.new_commits, 1, "rewrite should add exactly the amended commit");
    let out = tmp.path().join("out");
    let refs = gitdepot::export(&store, &out).unwrap();
    let tip = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert!(refs.iter().any(|r| r.name == "refs/heads/main" && r.sha == tip));
    assert_eq!(gitdepot::resolve_ref(&store, &old_main).unwrap().unwrap().0,
               old_main, "rewrite destroyed local history");

    // The fetch buffer is DERIVED state: delete repo.git, add a commit
    // upstream — the next mirror re-seeds the buffer from the store
    // (SHA-exact export) and fetches only the delta, no re-clone.
    std::fs::remove_dir_all(root.join("repo.git")).unwrap();
    std::fs::write(origin.join("even-more.txt"), "x\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "even more"]);
    let o4 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o4.update.new_commits, 1,
               "reseeded buffer should yield an incremental update");
    assert!(root.join("repo.git/HEAD").exists(), "buffer not rebuilt");

    // Frugal mode: successful update leaves the store as the single
    // on-disk copy.
    std::fs::write(origin.join("last.txt"), "y\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "last"]);
    let o5 = gitdepot::mirror_opts(origin.to_str().unwrap(), &root, true)
        .unwrap();
    assert_eq!(o5.update.new_commits, 1);
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

/// §9 anti-sabotage, COST axis: the tiered-chain store promises prepend
/// with BOUNDED re-encode — f0 + the accumulator only; sealed history
/// physically untouched. A one-commit update on a fat store must write
/// nowhere near the store's size (the v1 flat chain rewrote ALL of it).
#[test]
fn update_io_is_bounded_not_o_history() {
    // Per-THREAD accounting: the harness runs tests concurrently in one
    // process, and update() does its store I/O on this thread.
    fn written_bytes() -> u64 {
        let io = std::fs::read_to_string("/proc/thread-self/io").unwrap();
        io.lines().find_map(|l| l.strip_prefix("write_bytes: "))
            .unwrap().trim().parse().unwrap()
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    // Fatten history so O(N) vs O(1) is unmistakable.
    for i in 0..120 {
        std::fs::write(repo.join("doc.txt"), format!("pad rev {i}\n")).unwrap();
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-q", "-m", &format!("pad {i}")]);
    }
    gitdepot::import(&repo, &store, 3).unwrap();
    let store_len = store_size(&store);

    let one_commit_update_cost = |i: u32| -> u64 {
        std::fs::write(repo.join("doc.txt"), format!("tip line {i}\n")).unwrap();
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-q", "-m", &format!("tip {i}")]);
        let before = written_bytes();
        gitdepot::update(&repo, &store, 3).unwrap();
        written_bytes() - before
    };

    // Cost of one prepend = new f0 (one full tree frame) + the
    // accumulator re-encode + sqlite — O(tree + accumulator), NOT
    // O(history). Sanity ceiling first (v1's flat chain rewrote the
    // whole store and then some).
    let cost1 = one_commit_update_cost(0);
    assert!(cost1 < store_len,
            "one-commit update wrote {cost1} bytes against a {store_len}-byte \
             store — prepend rewrote the store");

    // The teeth: DOUBLE the history, prepend once more — the cost must
    // not follow (sealed cold history is physically untouched).
    for i in 120..240 {
        std::fs::write(repo.join("doc.txt"), format!("pad rev {i}\n")).unwrap();
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-q", "-m", &format!("pad {i}")]);
    }
    gitdepot::update(&repo, &store, 3).unwrap();
    let cost2 = one_commit_update_cost(1);
    eprintln!("bounded-update: store={store_len}B cost1={cost1}B cost2={cost2}B");
    assert!(cost2 < cost1 + cost1 / 2,
            "one-commit update cost grew with history ({cost1} -> {cost2} \
             bytes after doubling the commit count) — prepend is O(history)");
}

/// A v1 store (flat chain + legacy bookkeeping) migrates to v2 on open:
/// depot + schema=2 sqlite appear, the chain file disappears, and the
/// round-trip stays SHA-exact across the migration.
#[test]
fn v1_store_migrates_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    let v1 = tmp.path().join("store-v1");
    build_fixture(&repo);
    gitdepot::import(&repo, &store, 3).unwrap();
    gitdepot::store::legacy::write_v1_from_v2_for_tests(&store, &v1, 3).unwrap();
    assert!(v1.join("chain").exists() && v1.join("meta.json").exists());

    // Any open migrates: a read path suffices.
    assert_eq!(gitdepot::commit_count(&v1).unwrap(), 13);
    assert!(v1.join("meta.sqlite").exists(), "migration did not mint sqlite");
    assert!(v1.join("depot").is_dir(), "migration did not build the depot");
    assert!(!v1.join("chain").exists(), "v1 chain left behind");
    assert!(!v1.join("meta.json").exists(), "legacy json left behind");

    // Resolution + stable indices work post-migration.
    let meta = gitdepot::store::read_meta(&v1).unwrap();
    let (sha, idx) = gitdepot::resolve_ref(&v1, "main").unwrap().unwrap();
    assert_eq!(sha, meta.commits[0].sha);
    assert_eq!(idx, 12, "main is the newest commit (stable index N-1)");

    // Writes keep working (a one-commit update) and export is SHA-exact.
    std::fs::write(repo.join("doc.txt"), "post-migration\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "post-migration"]);
    let o = gitdepot::update(&repo, &v1, 3).unwrap();
    assert_eq!((o.new_commits, o.total_commits), (1, 14));
    let out = tmp.path().join("out");
    let refs = gitdepot::export(&v1, &out).unwrap();
    assert!(!refs.is_empty(), "post-migration export lost refs");
}

/// resolve_ref point-lookup semantics: bare name, full refname, tag,
/// unique sha prefix (with STABLE index), ambiguity, and misses.
#[test]
fn resolve_ref_point_lookups() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    sh_git(&repo, &["tag", "v1", "main~1"]); // lightweight: commit type
    gitdepot::import(&repo, &store, 3).unwrap();
    let meta = gitdepot::store::read_meta(&store).unwrap();
    let n = meta.commits.len(); // 13; meta.commits is newest-first

    let main_sha = meta.refs.iter()
        .find(|r| r.name == "refs/heads/main").unwrap().sha.clone();
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap(),
               (main_sha.clone(), n - 1));
    assert_eq!(gitdepot::resolve_ref(&store, "refs/heads/main").unwrap().unwrap(),
               (main_sha.clone(), n - 1));
    let (v1_sha, v1_idx) = gitdepot::resolve_ref(&store, "v1").unwrap().unwrap();
    assert_eq!(v1_sha, meta.commits[n - 1 - v1_idx].sha, "tag idx mismatched sha");
    assert_ne!(v1_idx, n - 1, "main~1 is not the newest record");

    // A NON-tip commit by unique sha prefix, with its stable index.
    let target = &meta.commits[5].sha; // newest-first pos 5 = stable idx n-6
    let mut plen = 4;
    while meta.commits.iter().filter(|c| c.sha.starts_with(&target[..plen])).count() > 1 {
        plen += 1;
    }
    assert_eq!(gitdepot::resolve_ref(&store, &target[..plen]).unwrap().unwrap(),
               (target.clone(), n - 6));
    // Stable index round-trips through commit_at.
    assert_eq!(gitdepot::commit_at(&store, n - 6).unwrap().sha, *target);

    // Empty prefix matches every commit: ambiguous, exact message.
    match gitdepot::resolve_ref(&store, "") {
        Err(gitdepot::Error::Meta(m)) => assert_eq!(m, "commit prefix  is ambiguous"),
        other => panic!("expected ambiguity error, got {other:?}"),
    }
    // No such ref or commit.
    assert!(gitdepot::resolve_ref(&store, "zzzz").unwrap().is_none());
    // LIKE wildcards in the query are literals, not patterns.
    assert!(gitdepot::resolve_ref(&store, "%").unwrap().is_none());
}

/// §9 anti-sabotage, METADATA cost axis: a one-commit update must do
/// O(new) bookkeeping. The proof is structural: every pre-existing
/// sha_idx row keeps its exact (sha, index) pair — stable indices are
/// never renumbered — and the new commit lands at index N.
#[test]
fn update_metadata_is_o_new_not_o_history() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    for i in 0..120 {
        std::fs::write(repo.join("doc.txt"), format!("pad rev {i}\n")).unwrap();
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-q", "-m", &format!("pad {i}")]);
    }
    gitdepot::import(&repo, &store, 3).unwrap();

    let rows = |store: &Path| -> Vec<(i64, String)> {
        let conn = rusqlite::Connection::open(store.join("meta.sqlite")).unwrap();
        let mut stmt = conn
            .prepare("SELECT commit_idx, sha FROM sha_idx ORDER BY commit_idx ASC")
            .unwrap();
        let v = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        v
    };
    let before = rows(&store);
    assert_eq!(before.len(), 133);
    assert_eq!(before[0].0, 0, "import numbers from 0 (oldest)");

    std::fs::write(repo.join("doc.txt"), "one more line\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "tip"]);
    gitdepot::update(&repo, &store, 3).unwrap();

    let after = rows(&store);
    assert_eq!(after.len(), 134);
    assert_eq!(after[..133], before[..],
               "pre-existing rows were renumbered — update is O(history)");
    assert_eq!(after[133].0, 133, "new commit must land at index N");
    // And the stable index reads back through the point accessors.
    assert_eq!(gitdepot::commit_at(&store, 133).unwrap().sha, after[133].1);
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap().1, 133);
}

/// Upstream deletes a branch: the ref row is GONE from the refs table,
/// a reflog record observes the deletion (new_* absent), and the
/// commits the ref pinned stay resolvable. Local history is never
/// destroyed.
#[test]
fn upstream_branch_deletion_drops_ref_and_keeps_history() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_fixture(&origin);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let store = root.join("store");
    let side_sha = gitdepot::resolve_ref(&store, "side").unwrap().unwrap().0;
    let n = gitdepot::commit_count(&store).unwrap();

    sh_git(&origin, &["branch", "-D", "side"]);
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.new_commits, 0);

    // The name no longer resolves; the commit (and the count) survive.
    assert!(gitdepot::resolve_ref(&store, "side").unwrap().is_none());
    assert_eq!(gitdepot::commit_count(&store).unwrap(), n);
    assert_eq!(gitdepot::resolve_ref(&store, &side_sha).unwrap().unwrap().0,
               side_sha);
    assert!(!gitdepot::store::refs(&store).unwrap().iter()
        .any(|r| r.name == "refs/heads/side"));
    // The row is deleted outright — CURRENT refs only, no deleted_at.
    let conn = rusqlite::Connection::open(store.join("meta.sqlite")).unwrap();
    let live: i64 = conn
        .query_row("SELECT COUNT(*) FROM refs WHERE name = 'refs/heads/side'",
                   [], |r| r.get(0))
        .unwrap();
    assert_eq!(live, 0, "deleted ref must leave the refs table");
    // The reflog chain observed the deletion.
    let log = gitdepot::store::reflog(&store).unwrap();
    let prune = log.iter().rev()
        .find(|e| e.refname == "refs/heads/side" && e.new_commit_idx.is_none())
        .expect("no deletion row in reflog");
    assert_eq!(prune.old_sha.as_deref(), Some(side_sha.as_str()));
    assert_eq!(prune.note, "pruned upstream");
    // The store still updates incrementally afterwards.
    std::fs::write(origin.join("after.txt"), "x\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "after prune"]);
    let o2 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o2.update.new_commits, 1);
    assert_eq!(gitdepot::commit_count(&store).unwrap(), n + 1);
}

/// Upstream force-push: history survives IN PLACE. Old commits stay
/// resolvable by sha and exportable; the reflog records the move; no
/// second store, ever.
#[test]
fn upstream_rewrite_keeps_history_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_fixture(&origin);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let store = root.join("store");
    let old_main = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().0;
    let old_count = gitdepot::commit_count(&store).unwrap();

    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();

    // Old commits still resolvable by sha…
    assert_eq!(gitdepot::resolve_ref(&store, &old_main).unwrap().unwrap().0,
               old_main);
    assert_eq!(gitdepot::commit_count(&store).unwrap(), old_count + 1);
    // …and still exportable: the export stream carries the whole store,
    // rewritten-away commits included.
    let new_main = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap().0,
               new_main);
    let out = tmp.path().join("out");
    gitdepot::export(&store, &out).unwrap();
    let have_old = Command::new("git")
        .arg("-C").arg(&out)
        .args(["cat-file", "-e", &old_main])
        .status().unwrap();
    assert!(have_old.success(), "old tip missing from export");
    // The reflog records the move old→new.
    let log = gitdepot::store::reflog(&store).unwrap();
    let rw = log.iter().rev()
        .find(|e| e.refname == "refs/heads/main"
              && e.old_sha.as_deref() == Some(old_main.as_str()))
        .expect("no rewrite row in reflog");
    assert_eq!(rw.new_sha.as_deref(), Some(new_main.as_str()));
}

/// Two successive rewrites: NO store copies, no store.retired.* ever,
/// and both old tips remain resolvable — a rewrite is records + a
/// repoint, not a new store.
#[test]
fn successive_rewrites_never_copy_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_fixture(&origin);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let store = root.join("store");
    let tip0 = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().0;

    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended once"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let tip1 = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().0;

    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended twice"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();

    let extra: Vec<_> = std::fs::read_dir(&root).unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "store" && n != "repo.git")
        .collect();
    assert!(extra.is_empty(), "rewrites left store copies: {extra:?}");
    for old in [&tip0, &tip1] {
        assert_eq!(gitdepot::resolve_ref(&store, old).unwrap().unwrap().0, **old,
                   "rewritten-away tip no longer resolvable");
    }
}
