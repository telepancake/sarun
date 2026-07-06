//! Store-output equivalence for the O(changes) streaming importer: the
//! store it writes must decode OBJECT-IDENTICAL to what the old
//! O(tree × history) per-commit importer produced. The reference is
//! reconstructed VIA GIT directly (this file re-implements the old
//! commit_view path — `ls-tree -r` + `cat-file --batch` per commit —
//! as test-local code; the production path was deleted): for every
//! commit, the store's decoded tree view must equal the ls-tree-built
//! view byte-for-byte in canonical encoding, and the commit records
//! must carry exactly the raw-object metadata. Runs on a merge-heavy
//! DAG (octopus + criss-cross merges + a branch deleted after merge)
//! because the streaming walk's frontier is only exercised by real
//! merge topology. Also proves update ≡ import at object level with
//! boundary parents whose views live only in the store (seed path).
//!
//! Needs a `git` binary.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// A DAG with every merge shape the frontier must survive:
/// octopus (3 parents), criss-cross (X-shaped double merge), a branch
/// merged then DELETED (commits reachable only through the merge's
/// second parent), empty commits, file→dir and dir→file type flips,
/// deletions, mode changes, symlinks, gitlinks are out of scope here
/// (covered by roundtrip's fixture), plus tags.
fn build_merge_fixture(repo: &Path) -> (Vec<String>, String) {
    std::fs::create_dir_all(repo).unwrap();
    sh_git(repo, &["init", "-q", "-b", "main"]);
    sh_git(repo, &["config", "commit.gpgsign", "false"]);
    sh_git(repo, &["config", "tag.gpgsign", "false"]);
    let commit = |msg: &str| {
        sh_git(repo, &["add", "-A"]);
        sh_git(repo, &["commit", "-q", "-m", msg]);
    };
    for i in 0..4 {
        std::fs::write(repo.join(format!("base{i}.txt")), format!("base {i}\n")).unwrap();
        std::fs::create_dir_all(repo.join("d")).unwrap();
        std::fs::write(repo.join("d/deep.txt"), format!("deep {i}\n")).unwrap();
        commit(&format!("base {i}"));
    }
    let base = sh_git(repo, &["rev-parse", "HEAD"]).trim().to_string();

    // Three branches for the octopus.
    for b in ["oct-a", "oct-b", "oct-c"] {
        sh_git(repo, &["checkout", "-q", "-b", b, &base]);
        std::fs::write(repo.join(format!("{b}.txt")), format!("{b}\n")).unwrap();
        commit(b);
    }
    sh_git(repo, &["checkout", "-q", "main"]);
    sh_git(repo, &["merge", "-q", "--no-ff", "-m", "octopus", "oct-a", "oct-b", "oct-c"]);

    // Criss-cross: x and y merge EACH OTHER, then main takes both.
    sh_git(repo, &["checkout", "-q", "-b", "x", "main"]);
    std::fs::write(repo.join("x.txt"), "x\n").unwrap();
    commit("x0");
    sh_git(repo, &["checkout", "-q", "-b", "y", "main"]);
    std::fs::write(repo.join("y.txt"), "y\n").unwrap();
    commit("y0");
    sh_git(repo, &["merge", "-q", "--no-ff", "-m", "y takes x", "x"]);
    sh_git(repo, &["checkout", "-q", "x"]);
    std::fs::write(repo.join("x.txt"), "x1\n").unwrap();
    commit("x1");
    sh_git(repo, &["merge", "-q", "--no-ff", "-m", "x takes y", "y"]);
    sh_git(repo, &["checkout", "-q", "main"]);
    sh_git(repo, &["merge", "-q", "--no-ff", "-m", "main takes x", "x"]);

    // A branch merged, then DELETED: its commit stays reachable only
    // through the merge's second parent.
    sh_git(repo, &["checkout", "-q", "-b", "doomed", "main"]);
    std::fs::write(repo.join("doomed.txt"), "gone tomorrow\n").unwrap();
    commit("doomed work");
    sh_git(repo, &["checkout", "-q", "main"]);
    sh_git(repo, &["merge", "-q", "--no-ff", "-m", "absorb doomed", "doomed"]);
    sh_git(repo, &["branch", "-q", "-D", "doomed"]);

    // Type flips + delete + empty commit on top (streamed as raw
    // status lines D/A/T — the delta-builder corners).
    std::fs::remove_file(repo.join("base0.txt")).unwrap();
    std::fs::create_dir_all(repo.join("base0.txt")).unwrap();
    std::fs::write(repo.join("base0.txt/inner.txt"), "file became dir\n").unwrap();
    std::fs::remove_dir_all(repo.join("d")).unwrap();
    std::fs::write(repo.join("d"), "dir became file\n").unwrap();
    std::fs::remove_file(repo.join("base1.txt")).unwrap();
    std::os::unix::fs::symlink("base2.txt", repo.join("base1.txt")).unwrap();
    std::fs::remove_file(repo.join("base3.txt")).unwrap();
    commit("type flips");
    sh_git(repo, &["commit", "-q", "--allow-empty", "-m", "empty"]);
    sh_git(repo, &["tag", "-a", "-m", "merge tag", "vmerge", "main~1"]);
    sh_git(repo, &["tag", "lw", "main"]);

    let shas: Vec<String> =
        sh_git(repo, &["rev-list", "--topo-order", "--reverse", "--branches", "--tags"])
            .lines()
            .map(str::to_string)
            .collect();
    (shas, base)
}

// ------------------------------------------------- reference view (old path)
// Test-local re-implementation of the deleted per-commit importer:
// full ls-tree of the commit, every blob piped, view built from scratch.

fn reference_view(repo: &Path, commitish: &str) -> depot::View {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-tree", "-r", "-z", "--full-tree", commitish])
        .output()
        .expect("ls-tree");
    assert!(out.status.success());
    // (mode, oid, path) triples.
    let mut entries: Vec<(String, String, Vec<u8>)> = Vec::new();
    for rec in out.stdout.split(|&b| b == 0) {
        if rec.is_empty() {
            continue;
        }
        let tab = rec.iter().position(|&b| b == b'\t').expect("tab");
        let head = std::str::from_utf8(&rec[..tab]).unwrap();
        let mut it = head.split(' ');
        let (mode, _typ, oid) = (
            it.next().unwrap().to_string(),
            it.next().unwrap(),
            it.next().unwrap().to_string(),
        );
        entries.push((mode, oid, rec[tab + 1..].to_vec()));
    }
    // One cat-file --batch for the blobs (request stream is small here;
    // fixtures fit the pipe).
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reqs = String::new();
    for (mode, oid, _) in &entries {
        if mode != "160000" {
            reqs.push_str(oid);
            reqs.push('\n');
        }
    }
    child.stdin.take().unwrap().write_all(reqs.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut buf = &out.stdout[..];
    while !buf.is_empty() {
        let nl = buf.iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&buf[..nl]).unwrap();
        let mut it = header.split(' ');
        let (oid, _typ, size) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let size: usize = size.parse().unwrap();
        blobs.insert(oid.to_string(), buf[nl + 1..nl + 1 + size].to_vec());
        buf = &buf[nl + 1 + size + 1..];
    }
    let mut root = depot::Node::keep();
    for (mode, oid, path) in &entries {
        let content = if mode == "160000" {
            oid.clone().into_bytes()
        } else {
            blobs[oid].clone()
        };
        let mut node = &mut root;
        let mut segs = path.split(|&b| b == b'/').peekable();
        while let Some(seg) = segs.next() {
            node = node.children.entry(seg.to_vec()).or_insert_with(depot::Node::keep);
            if segs.peek().is_none() {
                node.blob = depot::BlobOp::Set(content.clone());
                node.attrs = Some(depot::Attrs::from([(
                    b"mode".to_vec(),
                    mode.clone().into_bytes(),
                )]));
            }
        }
    }
    depot::apply(None, &depot::Layer { root }).expect("non-empty tree")
}

fn canon(v: &depot::View) -> Vec<u8> {
    depot::codec::encode(&depot::diff(None, Some(v)))
}

/// Every store object the importer wrote, decoded and checked against
/// git's own answers for the same repo.
fn assert_store_matches_git(repo: &Path, store: &Path, shas: &[String]) {
    let st = gitdepot::store::Store::open(store).unwrap();
    let recs = st.commit_records().unwrap();
    assert_eq!(recs.len(), shas.len(), "commit count");
    let idx_of: BTreeMap<&str, u64> = recs.iter().map(|r| (r.sha.as_str(), r.idx)).collect();
    for (i, sha) in shas.iter().enumerate() {
        let r = &recs[i];
        assert_eq!(&r.sha, sha, "stable index {i} holds the wrong commit");
        // Metadata straight from the raw object.
        let raw = sh_git(repo, &["cat-file", "commit", sha]);
        let hdr = |k: &str| -> String {
            raw.lines()
                .take_while(|l| !l.is_empty())
                .find_map(|l| l.strip_prefix(&format!("{k} ")))
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(String::from_utf8_lossy(&r.author), hdr("author"), "{sha} author");
        assert_eq!(String::from_utf8_lossy(&r.committer), hdr("committer"), "{sha} committer");
        let msg = raw.split_once("\n\n").map(|(_, m)| m).unwrap_or("");
        assert_eq!(String::from_utf8_lossy(&r.message), msg, "{sha} message");
        assert!(r.extra_headers.is_empty() && r.raw.is_empty(), "{sha} extra headers");
        // Parent EDGES as stable indices.
        let want_parents: Vec<u64> = raw
            .lines()
            .take_while(|l| !l.is_empty())
            .filter_map(|l| l.strip_prefix("parent "))
            .map(|p| idx_of[p])
            .collect();
        assert_eq!(r.parent_idxs, want_parents, "{sha} parent indices");
        // The decoded tree view equals the from-scratch ls-tree build,
        // byte-for-byte in canonical encoding.
        assert_eq!(
            canon(&st.tree_view(r.tree_idx).unwrap()),
            canon(&reference_view(repo, sha)),
            "{sha} tree view diverges from the ls-tree reference"
        );
        // Tree dedup discipline preserved: same oid as a parent ⇒ same
        // tree_idx as that parent.
        let tree_oid = hdr("tree");
        for p in raw.lines().take_while(|l| !l.is_empty()).filter_map(|l| l.strip_prefix("parent ")) {
            if sh_git(repo, &["rev-parse", &format!("{p}^{{tree}}")]).trim() == tree_oid {
                assert_eq!(r.tree_idx, recs[idx_of[p] as usize].tree_idx,
                           "{sha} minted a new tree despite a same-tree parent");
            }
        }
    }
    // Refs agree with for-each-ref (peeled commits for annotated tags).
    let mut want: Vec<String> = sh_git(repo, &["for-each-ref",
        "--format=%(refname) %(objectname) %(*objectname)", "refs/heads", "refs/tags"])
        .lines()
        .map(|l| {
            let mut it = l.split(' ');
            let (name, sha, peeled) = (it.next().unwrap(), it.next().unwrap(),
                                       it.next().unwrap_or(""));
            format!("{name} {}", if peeled.is_empty() { sha } else { peeled })
        })
        .collect();
    want.sort();
    let mut got: Vec<String> = st
        .refs_meta()
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.name, r.sha))
        .collect();
    got.sort();
    assert_eq!(got, want, "refs diverge from for-each-ref");
}

#[test]
fn streaming_import_matches_git_reference_on_merge_heavy_dag() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    let (shas, _) = build_merge_fixture(&repo);
    assert!(shas.len() >= 15, "fixture too small: {}", shas.len());

    gitdepot::import(&repo, &store, 3).unwrap();
    assert_store_matches_git(&repo, &store, &shas);

    // End-to-end: the merge DAG exports SHA-exact.
    let out = tmp.path().join("out");
    gitdepot::export(&store, &out).unwrap();
    let ffr = |r: &Path| sh_git(r, &["for-each-ref", "--format=%(objectname) %(refname)"]);
    assert_eq!(ffr(&out), ffr(&repo), "merge-heavy export not SHA-exact");
}

/// update ≡ import at object level when the update's stream STARTS at
/// boundary parents whose views exist only in the store (the
/// seed_views path), merges included: a two-phase build must produce
/// the same commit objects at the same indices, byte-identical TREES
/// records, and the same refs as a one-shot import.
#[test]
fn incremental_update_equals_full_import_across_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let (shas, base) = build_merge_fixture(&repo);

    // Store A: one-shot import of everything.
    let a = tmp.path().join("store-a");
    gitdepot::import(&repo, &a, 3).unwrap();

    // Store B: import truncated at `base` (pre-merge history), then one
    // update to the full DAG — every merge lands with old parents.
    let b = tmp.path().join("store-b");
    let full_refs: Vec<(String, String)> = sh_git(&repo, &["for-each-ref",
        "--format=%(refname) %(objectname)", "refs/heads", "refs/tags"])
        .lines()
        .map(|l| {
            let (n, s) = l.split_once(' ').unwrap();
            (n.to_string(), s.to_string())
        })
        .collect();
    sh_git(&repo, &["checkout", "-q", "--detach"]);
    for (name, _) in &full_refs {
        sh_git(&repo, &["update-ref", "-d", name]);
    }
    sh_git(&repo, &["update-ref", "refs/heads/main", &base]);
    gitdepot::import(&repo, &b, 3).unwrap();
    for (name, sha) in &full_refs {
        sh_git(&repo, &["update-ref", name, sha]);
    }
    let o = gitdepot::update(&repo, &b, 3).unwrap();
    assert_eq!(o.new_commits, shas.len() - 4, "update walked the wrong slice");

    let sa = gitdepot::store::Store::open(&a).unwrap();
    let sb = gitdepot::store::Store::open(&b).unwrap();
    assert_eq!(sa.commit_records().unwrap(), sb.commit_records().unwrap(),
               "commit objects diverge between import and update");
    assert_eq!(sa.ref_rows().unwrap(), sb.ref_rows().unwrap(), "refs diverge");
    let tree_recs = |st: &gitdepot::store::Store| -> Vec<Vec<u8>> {
        let mut recs = Vec::new();
        st.walk_tree_views(None, &mut |_, rec, _| recs.push(rec.to_vec())).unwrap();
        recs
    };
    assert_eq!(tree_recs(&sa), tree_recs(&sb), "TREES records diverge");
    drop((sa, sb));
    assert_store_matches_git(&repo, &b, &shas);
}
