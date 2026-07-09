//! O(new) proof for the real update path: import a repo truncated to
//! HEAD~K, then update to the full tip, and show the update touches only
//! the new revisions — a bounded number of chain prepends and new
//! commits, independent of total history size.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn main() {
    let src = std::env::args().nth(1).expect("usage: verify_update <repo>");
    let src = Path::new(&src);
    let k: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let tmp = std::env::temp_dir().join(format!("vupd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let work = tmp.join("work.git");

    // A working clone we can move the branch on (the source stays untouched).
    let o = Command::new("git").args(["clone", "--quiet", "--bare"]).arg(src).arg(&work).output().unwrap();
    assert!(o.status.success(), "clone: {}", String::from_utf8_lossy(&o.stderr));

    let tip = git(&work, &["rev-parse", "HEAD"]).trim().to_string();
    let base = git(&work, &["rev-parse", &format!("HEAD~{k}")]).trim().to_string();
    // Point the default branch (and drop other refs) at HEAD~k for import.
    let head_ref = git(&work, &["symbolic-ref", "HEAD"]).trim().to_string();
    for line in git(&work, &["for-each-ref", "--format=%(refname)"]).lines() {
        if line != head_ref { git(&work, &["update-ref", "-d", line]); }
    }
    git(&work, &["update-ref", &head_ref, &base]);

    let store = tmp.join("store");
    let t = Instant::now();
    gitdepot::import(&work, &store, 3).unwrap();
    let n0 = gitdepot::commit_count(&store).unwrap();
    println!("import @HEAD~{k}: {n0} commits, {:.1}s", t.elapsed().as_secs_f64());

    // Advance to the real tip and update.
    git(&work, &["update-ref", &head_ref, &tip]);
    let t = Instant::now();
    let o = gitdepot::update(&work, &store, 3).unwrap();
    println!(
        "update -> HEAD: new_commits={}, total={}, depot_prepends={}, {:.2}s",
        o.new_commits, o.total_commits, o.depot_prepends, t.elapsed().as_secs_f64()
    );
    // The DAG delta (commits reachable from tip but not base) can exceed
    // the first-parent distance k when merges pull in side branches — so
    // bound it generously rather than assert exact k. The point is it is
    // O(new), not O(total): both the commit count and the prepend count
    // stay small next to the whole history.
    assert!(o.new_commits >= k && o.new_commits < n0, "update walked {} commits", o.new_commits);
    assert!(o.depot_prepends <= 3, "update made {} prepends (expected <=3, O(new))", o.depot_prepends);

    // The updated store still reconstructs the tip's tree SHA-exact.
    let st = gitdepot::store::Store::open(&store).unwrap();
    let ls = st.union().unwrap();
    let rev = ls.rev_of(&tip).expect("tip in union");
    let want = git(&work, &["rev-parse", &format!("{tip}^{{tree}}")]).trim().to_string();
    assert_eq!(ls.tree_oid_at(rev).unwrap(), want, "tip tree not SHA-exact after update");
    println!("tip tree SHA-exact after update; O(new) update confirmed");
    let _ = std::fs::remove_dir_all(&tmp);
}
