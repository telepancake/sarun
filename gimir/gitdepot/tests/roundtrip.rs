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

    // Tags: simple annotated, unsigned multi-line with odd bytes,
    // nested (tag→tag→commit), and a lightweight one alongside.
    sh_git(repo, &["config", "tag.gpgsign", "false"]);
    sh_git(repo, &["tag", "-a", "-m", "release one", "v1.0", "main~4"]);
    sh_git(repo, &["tag", "-a", "-m",
                   "multi-line message\n\nwith odd bytes: \x01\x02\téüλ\nand a trailing line",
                   "v2.0", "main~1"]);
    sh_git(repo, &["tag", "-a", "-m", "outer of nested", "nested", "refs/tags/v1.0"]);
    sh_git(repo, &["tag", "lw", "main~2"]);

    sh_git(repo, &["for-each-ref", "--format=%(objectname) %(refname)"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Recursive on-disk size of the store (depot files + sqlite).
/// `for-each-ref`-shaped lines from export results: the ref's direct
/// object (the tag object for annotated tags) + name.
fn ref_lines(refs: &[gitdepot::RefMeta]) -> Vec<String> {
    refs.iter()
        .map(|r| {
            let point = if r.tag_sha.is_empty() { &r.sha } else { &r.tag_sha };
            format!("{point} {}", r.name)
        })
        .collect()
}

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

    // Export and verify: every ref regenerates to the SAME object id —
    // tag objects included.
    let refs_after = gitdepot::export(&store, &out).unwrap();
    let mut after = ref_lines(&refs_after);
    after.sort();
    let mut before = refs_before;
    before.sort();
    assert_eq!(after, before, "round-trip changed ref object ids");

    // The exported repo agrees byte-for-byte with the origin's
    // for-each-ref, and both tag sha and peel round-trip.
    let ffr = |repo: &Path| sh_git(repo, &["for-each-ref",
        "--format=%(objectname) %(objecttype) %(refname)"]);
    assert_eq!(ffr(&out), ffr(&repo), "exported for-each-ref differs");
    for spec in ["refs/tags/v1.0", "refs/tags/v1.0^{}", "refs/tags/v2.0",
                 "refs/tags/v2.0^{}", "refs/tags/nested", "refs/tags/nested^{}",
                 "refs/tags/lw"] {
        assert_eq!(sh_git(&out, &["rev-parse", spec]),
                   sh_git(&repo, &["rev-parse", spec]),
                   "{spec} did not round-trip");
    }
    // Nested chain really is tag→tag→commit in the export.
    let raw = sh_git(&out, &["cat-file", "tag", "refs/tags/nested"]);
    assert!(raw.contains("\ntype tag\n"), "nested tag lost its inner tag: {raw}");

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
    let mut after = ref_lines(&refs_after);
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

/// Batch prepend ≡ sequential prepends at OBJECT level: the same three
/// commits landed as one 3-commit update and as three 1-commit updates
/// must produce identical commit objects at identical stable indices,
/// identical refs rows and sha↔idx mapping, and byte-identical TREES
/// records —
/// batch-record boundaries legitimately differ (one 3-object batch vs
/// three 1-object batches).
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
    assert_eq!(ra, rb, "commit objects diverge between batch and sequential");
    assert_eq!(sa.ref_rows().unwrap(), sb.ref_rows().unwrap(), "refs diverge");
    // The derived sha → idx mapping agrees (walk-built; no sqlite index).
    let sha_pairs = |st: &gitdepot::store::Store| -> Vec<(u64, String)> {
        let mut v: Vec<(u64, String)> = st
            .commit_records()
            .unwrap()
            .into_iter()
            .map(|r| (r.idx, r.sha))
            .collect();
        v.sort();
        v
    };
    assert_eq!(sha_pairs(&sa), sha_pairs(&sb), "sha↔idx mapping diverges");
    // TREES is untouched by batching: raw records byte-identical.
    let tree_recs = |st: &gitdepot::store::Store| -> Vec<Vec<u8>> {
        let mut recs = Vec::new();
        st.walk_tree_views(None, &mut |_, rec, _| recs.push(rec.to_vec()))
            .unwrap();
        recs
    };
    assert_eq!(tree_recs(&sa), tree_recs(&sb), "tree records diverge");
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
    let old_main = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().sha().to_string();
    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended"]);
    let o3 = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o3.update.new_commits, 1, "rewrite should add exactly the amended commit");
    let out = tmp.path().join("out");
    let refs = gitdepot::export(&store, &out).unwrap();
    let tip = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert!(refs.iter().any(|r| r.name == "refs/heads/main" && r.sha == tip));
    assert_eq!(gitdepot::resolve_ref(&store, &old_main).unwrap().unwrap().sha(),
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
    let o5 = gitdepot::mirror_opts(
        origin.to_str().unwrap(),
        &root,
        gitdepot::MirrorOpts { frugal: true, ..Default::default() },
    )
    .unwrap();
    assert_eq!(o5.update.new_commits, 1);
    assert!(!root.join("repo.git").exists(), "frugal left the buffer");
    let tip2 = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert!(o5.update.refs.iter()
        .any(|r| r.name == "refs/heads/main" && r.sha == tip2));
}

/// The narrowed unsupported case: a tag whose peel ends at a BLOB is
/// refused with an error naming the ref, and the mirror loop aborts
/// cleanly on it. Tags at commits AND trees are in scope now — blob
/// tags are the only refusal (no known real-world need).
#[test]
fn import_refuses_tag_at_blob_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh_git(&repo, &["init", "-q", "-b", "main"]);
    sh_git(&repo, &["config", "commit.gpgsign", "false"]);
    sh_git(&repo, &["config", "tag.gpgsign", "false"]);
    std::fs::write(repo.join("f"), "x").unwrap();
    sh_git(&repo, &["add", "-A"]);
    sh_git(&repo, &["commit", "-q", "-m", "c"]);
    sh_git(&repo, &["tag", "-a", "-m", "blob tag", "blobtag", "main:f"]);
    match gitdepot::import(&repo, &tmp.path().join("store"), 3) {
        Err(gitdepot::Error::Unsupported(m)) => {
            assert!(m.contains("refs/tags/blobtag") && m.contains("blob"),
                    "error must name the ref and the peeled type: {m}");
        }
        Err(e) => panic!("expected Unsupported, got: {e}"),
        Ok(_) => panic!("import of a blob-tag repo unexpectedly succeeded"),
    }
    // The mirror loop surfaces the same refusal, cleanly.
    match gitdepot::mirror(repo.to_str().unwrap(), &tmp.path().join("mirror")) {
        Err(gitdepot::Error::Unsupported(m)) => assert!(m.contains("refs/tags/blobtag")),
        other => panic!("mirror should abort Unsupported, got {other:?}"),
    }
}

/// Tags peeling to a TREE (the linux v2.6.11-tree shape): a tag at an
/// existing commit's tree DEDUPS (no new TREES record), a tag at a
/// standalone mktree'd tree imports one new TREES record; both export
/// SHA-exact (standalone trees materialized via mktree), the mirror
/// loop runs to completion, resolve_ref yields the tag-sha pin, and
/// the readout serves the tagged tree by that pin.
#[test]
fn tree_tags_dedup_import_export_and_mirror() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    let out = tmp.path().join("out");
    build_fixture(&repo);

    // (a) The linux shape: a tag at the tip commit's tree.
    sh_git(&repo, &["tag", "-a", "-m", "tree of main", "treetag", "main^{tree}"]);
    // (b) A standalone tree equal to no commit's tree.
    let git_stdin = |args: &[&str], input: &[u8]| -> String {
        let out = Command::new("git")
            .arg("-C").arg(&repo)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn().and_then(|mut c| {
                use std::io::Write as _;
                c.stdin.take().unwrap().write_all(input)?;
                c.wait_with_output()
            }).unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let blob = git_stdin(&["hash-object", "-w", "--stdin"], b"standalone bytes\n");
    let tree = git_stdin(&["mktree"],
                         format!("100644 blob {blob}\tlone.txt\n").as_bytes());
    sh_git(&repo, &["tag", "-a", "-m", "standalone tree", "lonetree", &tree]);

    gitdepot::import(&repo, &store, 3).unwrap();
    let st = gitdepot::store::Store::open(&store).unwrap();
    // build_fixture makes 13 commits with 13 distinct trees; treetag
    // dedups against main's tree, lonetree adds exactly one record.
    assert_eq!(st.count(gitdepot::store::TREES).unwrap(), 14,
               "tree-tag dedup broke: treetag must reuse main's TREES record");
    let tip_tree = st.commit_record_at(st.count(gitdepot::store::COMMITS).unwrap() - 1)
        .unwrap().tree_idx;
    drop(st);

    // resolve_ref: the pin is the TAG sha; treetag's tree is the tip's.
    let treetag_sha = sh_git(&repo, &["rev-parse", "refs/tags/treetag"]).trim().to_string();
    let lonetree_sha = sh_git(&repo, &["rev-parse", "refs/tags/lonetree"]).trim().to_string();
    assert_eq!(gitdepot::resolve_ref(&store, "treetag").unwrap().unwrap(),
               gitdepot::Resolved::TreeTag {
                   tag_sha: treetag_sha.clone(), tree_idx: tip_tree as usize });
    match gitdepot::resolve_ref(&store, "lonetree").unwrap().unwrap() {
        gitdepot::Resolved::TreeTag { tag_sha, .. } => assert_eq!(tag_sha, lonetree_sha),
        other => panic!("lonetree must resolve TreeTag, got {other:?}"),
    }

    // Attach path: the tag-sha pin serves the tagged tree's bytes.
    use depot::variant::Readout as _;
    let ro = gitdepot::readout::TipReadout::for_commit(&store, &lonetree_sha, "")
        .unwrap().expect("tag pin must hit");
    assert_eq!(ro.blob(&[b"lone.txt"]),
               Some(depot::variant::Blob::Bytes(b"standalone bytes\n".to_vec())));

    // Export: byte-equal for-each-ref, both tags AND their peels.
    gitdepot::export(&store, &out).unwrap();
    let ffr = |repo: &Path| sh_git(repo, &["for-each-ref",
        "--format=%(objectname) %(objecttype) %(refname)"]);
    assert_eq!(ffr(&out), ffr(&repo), "exported for-each-ref differs");
    for spec in ["refs/tags/treetag", "refs/tags/treetag^{tree}",
                 "refs/tags/lonetree", "refs/tags/lonetree^{tree}"] {
        assert_eq!(sh_git(&out, &["rev-parse", spec]),
                   sh_git(&repo, &["rev-parse", spec]),
                   "{spec} did not round-trip");
    }

    // Mirror loop with tree tags runs to completion, reflog notes the
    // tag-at-tree movement with no commit fields.
    let root = tmp.path().join("mirror");
    gitdepot::mirror(repo.to_str().unwrap(), &root).unwrap();
    let mstore = root.join("store");
    assert!(matches!(gitdepot::resolve_ref(&mstore, "lonetree").unwrap().unwrap(),
                     gitdepot::Resolved::TreeTag { .. }));
    let log = gitdepot::store::reflog(&mstore).unwrap();
    let row = log.iter().rev()
        .find(|e| e.refname == "refs/tags/lonetree")
        .expect("no reflog row for the tree tag");
    assert_eq!(row.note, "tag@tree");
    assert!(row.new_commit_idx.is_none() && row.new_sha.is_none(),
            "tree-tag reflog row must carry no commit");
    // A second mirror pass is a no-op (tree-tag rows diff stable).
    let o = gitdepot::mirror(repo.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.new_commits, 0);
    assert_eq!(o.update.depot_prepends, 0, "no-op mirror wrote records");
}

/// Mirror loop end-to-end with annotated tags: create resolves peeled;
/// upstream MOVES a tag → one reflog row (peeled movement, note "tag")
/// + repointed ref + a new tag object; upstream DELETES a tag → ref
/// row gone, reflog deletion row present, tag object still in chain.
#[test]
fn mirror_follows_tag_moves_and_deletions() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let root = tmp.path().join("mirror");
    build_fixture(&origin);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let store = root.join("store");

    // resolve_ref by tag name returns the PEELED commit.
    let peel = |name: &str| sh_git(&origin, &["rev-parse", &format!("{name}^{{}}")])
        .trim().to_string();
    let v1_sha = gitdepot::resolve_ref(&store, "v1.0").unwrap().unwrap().sha().to_string();
    assert_eq!(v1_sha, peel("refs/tags/v1.0"));
    assert_eq!(gitdepot::resolve_ref(&store, "nested").unwrap().unwrap().sha(),
               peel("refs/tags/nested"));
    let n_tags_0 = {
        let st = gitdepot::store::Store::open(&store).unwrap();
        st.tag_records().unwrap().len()
    };
    // nested's inner object IS v1.0's tag object — dedup'd by sha.
    assert_eq!(n_tags_0, 3, "expected v1.0 + v2.0 + nested outer");

    // Upstream moves v2.0 to the tip.
    sh_git(&origin, &["tag", "-f", "-a", "-m", "moved", "v2.0", "main"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let new_peel = peel("refs/tags/v2.0");
    assert_eq!(gitdepot::resolve_ref(&store, "v2.0").unwrap().unwrap().sha(), new_peel);
    let log = gitdepot::store::reflog(&store).unwrap();
    let mv = log.iter().rev()
        .find(|e| e.refname == "refs/tags/v2.0" && e.new_commit_idx.is_some()
              && e.old_commit_idx.is_some())
        .expect("no move row for refs/tags/v2.0");
    assert_eq!(mv.new_sha.as_deref(), Some(new_peel.as_str()));
    assert_eq!(mv.note, "tag");
    let st = gitdepot::store::Store::open(&store).unwrap();
    assert_eq!(st.tag_records().unwrap().len(), n_tags_0 + 1,
               "moved tag must mint one new tag object");
    drop(st);

    // Upstream deletes v1.0 (nested still holds the inner object).
    let v1_tag = sh_git(&origin, &["rev-parse", "refs/tags/v1.0"]).trim().to_string();
    sh_git(&origin, &["tag", "-d", "v1.0"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert!(gitdepot::resolve_ref(&store, "v1.0").unwrap().is_none());
    let conn = rusqlite::Connection::open(store.join("meta.sqlite")).unwrap();
    let live: i64 = conn.query_row(
        "SELECT COUNT(*) FROM refs WHERE name = 'refs/tags/v1.0'", [], |r| r.get(0)).unwrap();
    assert_eq!(live, 0, "deleted tag must leave the refs table");
    let log = gitdepot::store::reflog(&store).unwrap();
    assert!(log.iter().rev().any(|e| e.refname == "refs/tags/v1.0"
                                 && e.new_commit_idx.is_none()),
            "no deletion row for refs/tags/v1.0");
    let st = gitdepot::store::Store::open(&store).unwrap();
    assert!(st.tag_records().unwrap().iter().any(|t| t.sha == v1_tag),
            "deleted tag's object must stay in the chain");
    drop(st);

    // Export still carries the surviving tags SHA-exact (and the mirror
    // reseed path rides export, so this covers buffer reseeding too).
    let out = tmp.path().join("out");
    let refs = gitdepot::export(&store, &out).unwrap();
    assert!(refs.iter().any(|r| r.name == "refs/tags/v2.0" && !r.tag_sha.is_empty()));
    assert_eq!(sh_git(&out, &["rev-parse", "refs/tags/nested"]),
               sh_git(&origin, &["rev-parse", "refs/tags/nested"]));
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

/// Object reads across batch-record boundaries: each update lands one
/// batch per touched chain, so several updates make several batches —
/// commit_at must return the right object at every index (first/last
/// of every batch included), and the reflog must come back complete
/// and ordered across its batches.
#[test]
fn object_reads_cross_batch_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    // Import batch: 13 commits → indices 0..=12 in one batch.
    gitdepot::import(&repo, &store, 3).unwrap();
    let mut expected: Vec<String> = {
        let meta = gitdepot::store::read_meta(&store).unwrap();
        meta.commits.iter().rev().map(|c| c.sha.clone()).collect()
    };
    // Three more batches of 3, 1, 2 commits.
    for (batch, n) in [(0u32, 3u32), (1, 1), (2, 2)] {
        for i in 0..n {
            std::fs::write(repo.join("doc.txt"), format!("b{batch} c{i}\n")).unwrap();
            sh_git(&repo, &["add", "-A"]);
            sh_git(&repo, &["commit", "-q", "-m", &format!("b{batch} c{i}")]);
            expected.push(sh_git(&repo, &["rev-parse", "HEAD"]).trim().to_string());
        }
        gitdepot::update(&repo, &store, 3).unwrap();
    }
    assert_eq!(gitdepot::commit_count(&store).unwrap(), expected.len());
    // Every index — boundaries 0/12/13/15/16/17/18 included.
    for (idx, sha) in expected.iter().enumerate() {
        assert_eq!(
            &gitdepot::commit_at(&store, idx).unwrap().sha,
            sha,
            "commit_at({idx}) crossed a batch boundary wrong"
        );
    }
    // Reflog: complete, oldest-first, indices consistent across batches.
    let log = gitdepot::store::reflog(&store).unwrap();
    assert!(log.len() >= 2 + 3, "reflog lost rows across batches: {}", log.len());
    let mains: Vec<_> = log.iter().filter(|e| e.refname == "refs/heads/main").collect();
    assert_eq!(mains.last().unwrap().new_sha.as_deref(), Some(expected.last().unwrap().as_str()));
    for w in mains.windows(2) {
        assert_eq!(w[1].old_commit_idx, w[0].new_commit_idx,
                   "reflog rows out of order across batches");
    }
}

/// Tree dedup is parent-oid comparison + an intra-ingest map, NOT a
/// persistent index: an empty commit (tree oid == parent's) reuses the
/// parent's tree_idx with no new TREES record — across the update path
/// too (parent tree oid fetched from the buffer) — while a tree
/// bit-identical to a DISTANT ancestor (revert) deliberately mints a
/// new (small) record.
#[test]
fn tree_dedup_reuses_parent_not_distant_ancestors() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    sh_git(&repo, &["commit", "-q", "--allow-empty", "-m", "same tree"]);
    gitdepot::import(&repo, &store, 3).unwrap();

    let st = gitdepot::store::Store::open(&store).unwrap();
    let n_commits = st.count(gitdepot::store::COMMITS).unwrap();
    assert_eq!(n_commits, 14);
    assert_eq!(st.count(gitdepot::store::TREES).unwrap(), 13,
               "identical-to-parent tree minted a fresh TREES record");
    let tip = st.commit_record_at(n_commits - 1).unwrap();
    let parent = st.commit_record_at(tip.parent_idxs[0]).unwrap();
    assert_eq!(tip.tree_idx, parent.tree_idx);
    drop(st);

    // Update path: empty commit on top, parent already in the store.
    sh_git(&repo, &["commit", "-q", "--allow-empty", "-m", "same tree again"]);
    gitdepot::update(&repo, &store, 3).unwrap();
    let st = gitdepot::store::Store::open(&store).unwrap();
    assert_eq!(st.count(gitdepot::store::TREES).unwrap(), 13);
    let tip = st.commit_record_at(14).unwrap();
    assert_eq!(tip.tree_idx, 12);
    drop(st);

    // Revert to a distant ancestor's exact tree: same view, NEW record
    // (the explicit trade — no persistent all-history oid index).
    sh_git(&repo, &["rm", "-r", "-q", "."]);
    sh_git(&repo, &["checkout", "-q", "main~6", "--", "."]);
    sh_git(&repo, &["commit", "-q", "-m", "revert to ancestor"]);
    assert_eq!(
        sh_git(&repo, &["rev-parse", "main^{tree}"]),
        sh_git(&repo, &["rev-parse", "main~7^{tree}"]),
        "fixture: revert did not reproduce the ancestor tree"
    );
    gitdepot::update(&repo, &store, 3).unwrap();
    let st = gitdepot::store::Store::open(&store).unwrap();
    assert_eq!(st.count(gitdepot::store::TREES).unwrap(), 14,
               "revert-to-ancestor should mint a new TREES record");
}

/// Exactly ONE supported on-disk format: a mismatched kv schema value
/// (a store written by older code) fails open loudly.
#[test]
fn open_refuses_mismatched_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    gitdepot::import(&repo, &store, 3).unwrap();
    let conn = rusqlite::Connection::open(store.join("meta.sqlite")).unwrap();
    conn.execute("UPDATE kv SET value = '2' WHERE key = 'schema'", []).unwrap();
    drop(conn);
    match gitdepot::store::Store::open(&store) {
        Err(gitdepot::Error::Chain(m)) => {
            assert!(m.contains("older code") && m.contains("re-import"), "weak error: {m}")
        }
        Err(e) => panic!("expected schema error, got: {e}"),
        Ok(_) => panic!("open succeeded on a store written by older code"),
    }
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
    let commit = |sha: &str, idx: usize| gitdepot::Resolved::Commit {
        sha: sha.to_string(), idx,
    };
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap(),
               commit(&main_sha, n - 1));
    assert_eq!(gitdepot::resolve_ref(&store, "refs/heads/main").unwrap().unwrap(),
               commit(&main_sha, n - 1));
    let (v1_sha, v1_idx) = match gitdepot::resolve_ref(&store, "v1").unwrap().unwrap() {
        gitdepot::Resolved::Commit { sha, idx } => (sha, idx),
        other => panic!("lightweight tag must resolve to a commit, got {other:?}"),
    };
    assert_eq!(v1_sha, meta.commits[n - 1 - v1_idx].sha, "tag idx mismatched sha");
    assert_ne!(v1_idx, n - 1, "main~1 is not the newest record");

    // A NON-tip commit by unique sha prefix, with its stable index.
    let target = &meta.commits[5].sha; // newest-first pos 5 = stable idx n-6
    let mut plen = 4;
    while meta.commits.iter().filter(|c| c.sha.starts_with(&target[..plen])).count() > 1 {
        plen += 1;
    }
    assert_eq!(gitdepot::resolve_ref(&store, &target[..plen]).unwrap().unwrap(),
               commit(target, n - 6));
    // Both prefix parities resolve (a longer unique prefix stays unique).
    assert_eq!(gitdepot::resolve_ref(&store, &target[..plen + 1]).unwrap().unwrap(),
               commit(target, n - 6));
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
/// commit keeps its exact (sha, index) pair — stable indices are never
/// renumbered — and the new commit lands at index N.
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

    let rows = |store: &Path| -> Vec<(u64, String)> {
        gitdepot::store::Store::open(store)
            .unwrap()
            .commit_records()
            .unwrap()
            .into_iter()
            .map(|r| (r.idx, r.sha))
            .collect()
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
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap()
                   .commit().unwrap().1, 133);
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
    let side_sha = gitdepot::resolve_ref(&store, "side").unwrap().unwrap().sha().to_string();
    let n = gitdepot::commit_count(&store).unwrap();

    sh_git(&origin, &["branch", "-D", "side"]);
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.new_commits, 0);

    // The name no longer resolves; the commit (and the count) survive.
    assert!(gitdepot::resolve_ref(&store, "side").unwrap().is_none());
    assert_eq!(gitdepot::commit_count(&store).unwrap(), n);
    assert_eq!(gitdepot::resolve_ref(&store, &side_sha).unwrap().unwrap().sha(),
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
    let old_main = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().sha().to_string();
    let old_count = gitdepot::commit_count(&store).unwrap();

    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();

    // Old commits still resolvable by sha…
    assert_eq!(gitdepot::resolve_ref(&store, &old_main).unwrap().unwrap().sha(),
               old_main);
    assert_eq!(gitdepot::commit_count(&store).unwrap(), old_count + 1);
    // …and still exportable: the export stream carries the whole store,
    // rewritten-away commits included.
    let new_main = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap().sha(),
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
    let tip0 = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().sha().to_string();

    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended once"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    let tip1 = gitdepot::resolve_ref(&store, "main").unwrap().unwrap().sha().to_string();

    sh_git(&origin, &["commit", "-q", "--amend", "-m", "amended twice"]);
    gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();

    let extra: Vec<_> = std::fs::read_dir(&root).unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // .lock is the per-root run guard, not a store copy.
        .filter(|n| n != "store" && n != "repo.git" && n != ".lock")
        .collect();
    assert!(extra.is_empty(), "rewrites left store copies: {extra:?}");
    for old in [&tip0, &tip1] {
        assert_eq!(gitdepot::resolve_ref(&store, old).unwrap().unwrap().sha(), old.as_str(),
                   "rewritten-away tip no longer resolvable");
    }
}

#[test]
fn tree_walk_matches_apply_reference_byte_exact() {
    // Read fidelity: the in-place walk (apply_mut, one working view)
    // must reconstruct, byte-for-byte in canonical encoding, exactly
    // what depot::apply-based reference reconstruction yields from the
    // same stored records — the write path is untouched, so this pins
    // byte-compatibility with pre-existing v2 stores.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    build_fixture(&repo);
    gitdepot::import(&repo, &store, 3).unwrap();

    let st = gitdepot::store::Store::open(&store).unwrap();
    let mut recs: Vec<Vec<u8>> = Vec::new();
    let mut walked: Vec<depot::View> = Vec::new();
    st.walk_tree_views(None, &mut |_, rec, view| {
        recs.push(rec.to_vec());
        walked.push(view.clone());
    })
    .unwrap();
    assert_eq!(recs.len() as u64, st.count(gitdepot::store::TREES).unwrap());

    let mut reference: Option<depot::View> = None;
    for (i, rec) in recs.iter().enumerate() {
        let layer = depot::codec::decode(rec).unwrap();
        reference = depot::apply(reference.as_ref(), &layer);
        let want = reference.as_ref().expect("reference view resolves");
        assert_eq!(
            depot::codec::encode(&depot::diff(None, Some(&walked[i]))),
            depot::codec::encode(&depot::diff(None, Some(want))),
            "walked view at newest-first position {i} diverges from apply reference"
        );
    }

    // Point reads (the deep-access path) agree with the full walk.
    let n = recs.len();
    for idx in [0usize, n / 2, n - 1] {
        let v = st.tree_view(idx as u64).unwrap();
        assert_eq!(
            depot::codec::encode(&depot::diff(None, Some(&v))),
            depot::codec::encode(&depot::diff(None, Some(&walked[n - 1 - idx]))),
            "tree_view({idx}) diverges from the full walk"
        );
    }
}

/// Two schedulers on one root: the second run must refuse up front with
/// the named lock error — never race git against the same buffer (the
/// observed failure: clone --mirror and remote update --prune in
/// parallel → "BUG: refs/files-backend.c: initial ref transaction …").
#[test]
fn mirror_refuses_while_another_run_holds_the_root() {
    use std::os::fd::AsRawFd;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("mirror");
    std::fs::create_dir_all(&root).unwrap();
    let f = std::fs::OpenOptions::new()
        .create(true).write(true)
        .open(root.join(".lock")).unwrap();
    assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);

    // The lock check precedes any git; the URL never gets dialed.
    let err = gitdepot::mirror("file:///nonexistent", &root).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("another mirror run holds"), "wrong error: {msg}");
    assert!(msg.contains(root.to_str().unwrap()), "error must name the root: {msg}");

    // Lock released → the same root works again (with a real origin).
    drop(f);
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    sh_git(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("a.txt"), "a\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "a"]);
    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.total_commits, 1);
}

/// A crashed clone must never poison the root: the clone lands in
/// repo.git.new and only a COMPLETE clone is renamed to repo.git, so a
/// planted partial scratch is discarded and repo.git's existence keeps
/// implying completeness.
#[test]
fn mirror_discards_a_stale_partial_clone_scratch() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    sh_git(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("a.txt"), "a\n").unwrap();
    sh_git(&origin, &["add", "-A"]);
    sh_git(&origin, &["commit", "-q", "-m", "a"]);

    // A killed `git clone --mirror` leaves HEAD but no refs/objects.
    let root = tmp.path().join("mirror");
    let scratch = root.join("repo.git.new");
    std::fs::create_dir_all(scratch.join("objects")).unwrap();
    std::fs::write(scratch.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(scratch.join("junk"), "partial\n").unwrap();

    let o = gitdepot::mirror(origin.to_str().unwrap(), &root).unwrap();
    assert_eq!(o.update.total_commits, 1);
    assert!(!scratch.exists(), "stale scratch must be gone");
    assert!(root.join("repo.git/HEAD").exists());
    assert!(!root.join("repo.git/junk").exists(), "partial clone leaked into the buffer");
    let store = root.join("store");
    let tip = sh_git(&origin, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(gitdepot::resolve_ref(&store, "main").unwrap().unwrap().sha(), tip);
}
