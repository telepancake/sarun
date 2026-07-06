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

/// §9 anti-sabotage, COST axis (the one the assertions above miss):
/// the tiered-chain design promises prepend with BOUNDED re-encode —
/// frame 0 + the accumulator only; sealed history physically untouched.
/// The flat single-file chain rewrites the ENTIRE store per update
/// (prepend_store: read whole chain, write whole chain), so a one-
/// commit update on an N-commit store costs O(N) I/O — a daily mirror
/// of a big repo rewrites gigabytes to prepend kilobytes.
///
/// This is the acceptance test for the depot-variant convergence
/// (ATTACH-CONVERGENCE.md chip 7: gitdepot's chain moves behind the
/// tiered store). Un-ignore when the store tiers; it must then pass.
#[test]
#[ignore = "SABOTAGE (known): flat chain rewrites O(history) bytes per \
            prepend; bounded-prepend arrives with the tiered depot \
            variant (ATTACH-CONVERGENCE.md chip 7)"]
fn update_io_is_bounded_not_o_history() {
    fn written_bytes() -> u64 {
        let io = std::fs::read_to_string("/proc/self/io").unwrap();
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
    let chain_len = std::fs::metadata(store.join("chain")).unwrap().len();

    std::fs::write(repo.join("doc.txt"), "one more line\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "tip"]);

    let before = written_bytes();
    gitdepot::update(&repo, &store, 3).unwrap();
    let cost = written_bytes() - before;
    // Bounded: the new frames + bridge + meta — nowhere near a store
    // rewrite. Half the old chain is a generous ceiling.
    assert!(cost < chain_len / 2,
            "one-commit update wrote {cost} bytes against a {chain_len}-byte \
             store — prepend is O(history), the tiering is sabotaged");
}

/// A pre-sqlite store (meta.json) still opens read-only, and the first
/// write converts it: sqlite appears, the json disappears, and the
/// round-trip stays SHA-exact across the conversion.
#[test]
fn legacy_json_store_reads_and_converts_on_first_write() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    gitdepot::import(&repo, &store, 3).unwrap();

    // Regress the store to the legacy format: Meta still serializes to
    // the exact meta.json shape (hex fields), so the fixture is minted
    // from the live store.
    let meta = gitdepot::chain::read_meta(&store).unwrap();
    let json = serde_json::to_string_pretty(&meta).unwrap();
    std::fs::write(store.join("meta.json"), json).unwrap();
    for f in ["meta.sqlite", "meta.sqlite-wal", "meta.sqlite-shm"] {
        let p = store.join(f);
        if p.exists() {
            std::fs::remove_file(p).unwrap();
        }
    }

    // Legacy reads: full load, point accessors, resolution.
    let back = gitdepot::chain::read_meta(&store).unwrap();
    assert_eq!(back.commits.len(), meta.commits.len());
    assert_eq!(gitdepot::commit_count(&store).unwrap(), 13);
    let (sha, pos) = gitdepot::resolve_ref(&store, "main").unwrap().unwrap();
    assert_eq!(pos, 0, "main is the newest commit");
    assert_eq!(sha, meta.commits[0].sha);

    // First write (a one-commit update) converts.
    std::fs::write(repo.join("doc.txt"), "post-legacy\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "post-legacy"]);
    let o = gitdepot::update(&repo, &store, 3).unwrap();
    assert_eq!((o.new_commits, o.total_commits), (1, 14));
    assert!(store.join("meta.sqlite").exists(), "conversion did not mint sqlite");
    assert!(!store.join("meta.json").exists(), "legacy json left behind");

    let out = tmp.path().join("out");
    let refs = gitdepot::export(&store, &out).unwrap();
    assert!(!refs.is_empty(), "post-conversion export lost refs");
}

/// resolve_ref point-lookup semantics: bare name, full refname, tag,
/// unique sha prefix (with frame index), ambiguity, and misses.
#[test]
fn resolve_ref_point_lookups() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    sh_git(&repo, &["tag", "v1", "main~1"]); // lightweight: commit type
    gitdepot::import(&repo, &store, 3).unwrap();
    let meta = gitdepot::chain::read_meta(&store).unwrap();

    let main_sha = meta.refs.iter()
        .find(|r| r.name == "refs/heads/main").unwrap().sha.clone();
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap(),
               (main_sha.clone(), 0));
    assert_eq!(gitdepot::resolve_ref(&store, "refs/heads/main").unwrap().unwrap(),
               (main_sha.clone(), 0));
    let (v1_sha, v1_pos) = gitdepot::resolve_ref(&store, "v1").unwrap().unwrap();
    assert_eq!(v1_sha, meta.commits[v1_pos].sha, "tag pos mismatched sha");
    assert_ne!(v1_pos, 0, "main~1 is not the newest frame");

    // A NON-tip commit by unique sha prefix, with its frame index.
    let target = &meta.commits[5].sha;
    let mut plen = 4;
    while meta.commits.iter().filter(|c| c.sha.starts_with(&target[..plen])).count() > 1 {
        plen += 1;
    }
    assert_eq!(gitdepot::resolve_ref(&store, &target[..plen]).unwrap().unwrap(),
               (target.clone(), 5));
    // Frame index round-trips through commit_at.
    assert_eq!(gitdepot::commit_at(&store, 5).unwrap().sha, *target);

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
/// O(new) sqlite work. The proof is structural: every pre-existing
/// commits row keeps its exact (pos, sha) key — nothing is renumbered
/// or rewritten — and the new commit lands at MIN(pos)-1.
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
            .prepare("SELECT pos, sha FROM commits ORDER BY pos ASC")
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
    assert_eq!(before[0].0, 0, "import numbers from 0");

    std::fs::write(repo.join("doc.txt"), "one more line\n").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "tip"]);
    gitdepot::update(&repo, &store, 3).unwrap();

    let after = rows(&store);
    assert_eq!(after.len(), 134);
    assert_eq!(after[1..], before[..],
               "pre-existing rows were renumbered — update is O(history)");
    assert_eq!(after[0].0, -1, "new commit must land at MIN(pos)-1");
    // And the shifted keys still read back as frame indices.
    assert_eq!(gitdepot::commit_at(&store, 0).unwrap().sha, after[0].1);
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap().1, 0);
}

/// Upstream deletes a branch: the mirror marks the ref (deleted_at),
/// logs the prune, stops resolving the name — but the commits the ref
/// pinned STAY in the store. Local history is never destroyed.
#[test]
fn upstream_branch_deletion_marks_ref_and_keeps_history() {
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
    assert!(!o.reimported, "a ref deletion must not force a re-import");
    assert_eq!(o.update.new_commits, 0);

    // The name no longer resolves; the commit (and the count) survive.
    assert!(gitdepot::resolve_ref(&store, "side").unwrap().is_none());
    assert_eq!(gitdepot::commit_count(&store).unwrap(), n);
    assert_eq!(gitdepot::resolve_ref(&store, &side_sha).unwrap().unwrap().0,
               side_sha);
    // Live listings exclude it; the row is marked, not dropped.
    assert!(!gitdepot::chain::refs(&store).unwrap().iter()
        .any(|r| r.name == "refs/heads/side"));
    let conn = rusqlite::Connection::open(store.join("meta.sqlite")).unwrap();
    let deleted_at: Option<i64> = conn
        .query_row("SELECT deleted_at FROM refs WHERE name = 'refs/heads/side'",
                   [], |r| r.get(0))
        .unwrap();
    assert!(deleted_at.is_some(), "pruned ref must be marked, not dropped");
    // The reflog observed the deletion.
    let log = gitdepot::chain::reflog(&store).unwrap();
    let prune = log.iter().rev()
        .find(|e| e.refname == "refs/heads/side" && e.new_sha.is_none())
        .expect("no prune row in reflog");
    assert_eq!(prune.old_sha.as_deref(), Some(side_sha.as_str()));
    assert_eq!(prune.note, "pruned upstream");
    // The store still updates incrementally afterwards.
    std::fs::write(origin.join("after.txt"), "x\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "after prune"]);
    let o2 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!((o2.update.new_commits, o2.reimported), (1, false));
    assert_eq!(gitdepot::commit_count(&store).unwrap(), n + 1);
}

/// Upstream force-push: the old store is RETIRED (renamed, intact and
/// still readable), never deleted; the new store serves the new history
/// and its reflog records the rewrite naming the retired path.
#[test]
fn upstream_rewrite_retires_old_store_and_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_fixture(&origin);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let store = root.join("store");
    let old_main = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().0;
    let old_count = gitdepot::commit_count(&store).unwrap();

    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended"]);
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert!(o.reimported);

    let retired: Vec<_> = std::fs::read_dir(&root).unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("store.retired."))
        .collect();
    assert_eq!(retired.len(), 1, "old store not retired");
    let retired = retired[0].path();
    // The retired store is intact: bookkeeping AND chain read back.
    let (old_meta, old_views) = gitdepot::chain::read_store(&retired).unwrap();
    assert_eq!(old_meta.commits.len(), old_count);
    assert_eq!(old_views.len(), old_count);
    assert_eq!(gitdepot::resolve_ref(&retired, "main").unwrap().unwrap().0,
               old_main);

    // The new store serves the new history…
    let new_main = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap(),
               (new_main.clone(), 0));
    // …and its reflog records the rewrite, naming the retired path.
    let log = gitdepot::chain::reflog(&store).unwrap();
    let rw = log.iter().rev()
        .find(|e| e.refname == "refs/heads/main"
              && e.old_sha.as_deref() == Some(old_main.as_str()))
        .expect("no rewrite row in reflog");
    assert_eq!(rw.new_sha.as_deref(), Some(new_main.as_str()));
    assert!(rw.note.starts_with("rewrite"), "note {:?}", rw.note);
    assert!(rw.note.contains(retired.to_str().unwrap()),
            "note {:?} does not name the retired store", rw.note);
}
