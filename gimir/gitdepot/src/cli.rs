//! CLI: `gitdepot import <git-repo> <store-dir> [--level N] [--report]`
//!      `gitdepot update <git-repo> <store-dir> [--level N]`
//!      `gitdepot mirror <url> <root-dir> [--frugal] [--whole]`
//!      `gitdepot list <mirrors-root>`
//!      `gitdepot log <store-dir>`
//!      `gitdepot export <store-dir> <new-repo-dir>`

use std::path::PathBuf;

fn usage() -> i32 {
    eprintln!(
        "usage: gitdepot import <git-repo> <store-dir> [--level N] [--shard-bits B] [--report]\n       gitdepot update <git-repo> <store-dir> [--level N]\n       gitdepot mirror <url> <root-dir> [--frugal] [--whole]\n       gitdepot list <mirrors-root>\n       gitdepot log <store-dir>\n       gitdepot export <store-dir> <new-repo-dir>\n       gitdepot union <git-repo> <store-dir> [--level N] [--shard-bits B]\n       gitdepot union-update <git-repo> <store-dir> [--level N]\n       gitdepot union-verify <git-repo> <store-dir> [stride]\n       gitdepot memgraph <git-repo>"
    );
    2
}

/// Extract `--shard-bits B` (§9, a NEW-store parameter — an existing store
/// keeps the bits it was created with) from an arg tail, exporting it for
/// the store-creation path; returns the remaining args, `None` on a bad
/// value.
fn take_shard_bits(rest: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--shard-bits" {
            let v = rest.get(i + 1)?;
            v.parse::<u32>().ok().filter(|&b| b <= 8)?;
            std::env::set_var("GITDEPOT_SHARD_BITS", v);
            i += 2;
        } else {
            out.push(rest[i].clone());
            i += 1;
        }
    }
    Some(out)
}

/// The `gitdepot` CLI entry, callable in-process: the sarun engine binary
/// embeds this crate and dispatches here on `sarun gitdepot …` / an argv[0]
/// symlink named `gitdepot` (multi-call, like oaita).
pub fn cli_main(args: &[String]) -> i32 {
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => return usage(),
    };
    let result = match (cmd, rest) {
        ("import", [repo, store, rest @ ..]) => {
            let Some(rest) = take_shard_bits(rest) else { return usage() };
            let (level, report) = match &rest[..] {
                [] => (3, false),
                [f] if f == "--report" => (3, true),
                [flag, n] if flag == "--level" => match n.parse() {
                    Ok(v) => (v, false),
                    Err(_) => return usage(),
                },
                [flag, n, f] if flag == "--level" && f == "--report" => match n.parse() {
                    Ok(v) => (v, true),
                    Err(_) => return usage(),
                },
                _ => return usage(),
            };
            crate::import_opts(&PathBuf::from(repo), &PathBuf::from(store),
                                  level, report).map(|o| {
                // Peak RSS on stderr: imports are memory-bound (the
                // frontier of live views), so the bench gate tracks it.
                if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
                    if let Some(l) = s.lines().find(|l| l.starts_with("VmHWM")) {
                        eprintln!("{}", l.split_whitespace().collect::<Vec<_>>().join(" "));
                    }
                }
                let Some(r) = &o.report else {
                    println!("{} commits imported (max frontier {})",
                             o.new_commits, o.max_frontier);
                    return;
                };
                println!(
                    "{} commits, zstd level {}\n\
                     \x20 full records:  raw {:>12}  standalone {:>12}  refPrefix chain {:>12}\n\
                     \x20 delta records: raw {:>12}  standalone {:>12}  refPrefix chain {:>12}\n\
                     \x20 view-anchored chain (delta records, prev full view as prefix): {:>12}  (stored)\n\
                     \x20 solid stream over full records: {:>12}  (bound)",
                    r.commits,
                    r.zstd_level,
                    r.full_raw,
                    r.full_standalone,
                    r.full_ref_chain,
                    r.delta_raw,
                    r.delta_standalone,
                    r.delta_ref_chain,
                    r.view_ref_chain,
                    r.solid_full,
                );
            })
        }
        ("update", [repo, store, rest @ ..]) => {
            let level = match rest {
                [] => 3,
                [flag, n] if flag == "--level" => match n.parse() {
                    Ok(v) => v,
                    Err(_) => return usage(),
                },
                _ => return usage(),
            };
            crate::update(&PathBuf::from(repo), &PathBuf::from(store), level).map(|o| {
                println!("{} new commits ({} total)", o.new_commits, o.total_commits);
                for r in &o.refs {
                    println!("{} {}", r.sha, r.name);
                }
            })
        }
        ("list", [root]) => {
            crate::list_mirrors(&PathBuf::from(root)).map(|entries| {
                for e in &entries {
                    // Branches + tags are the navigable surface; the
                    // refs/pull/* forest is noise here.
                    let mut heads: Vec<&str> = e.refs.iter()
                        .filter_map(|r| r.name.strip_prefix("refs/heads/"))
                        .collect();
                    let tags = e.refs.iter()
                        .filter(|r| r.name.starts_with("refs/tags/")).count();
                    let other = e.refs.len() - heads.len() - tags;
                    if heads.len() > 6 {
                        heads.truncate(6);
                        heads.push("…");
                    }
                    println!("{:<16} {:>5} commits  [{}]{}{}  {}",
                             e.label, e.commits, heads.join(", "),
                             if tags > 0 { format!("  +{tags} tags") }
                             else { String::new() },
                             if other > 0 { format!("  +{other} other refs") }
                             else { String::new() },
                             e.url);
                }
            })
        }
        ("log", [store]) => {
            crate::store::read_meta(&PathBuf::from(store)).map(|meta| {
                for (i, c) in meta.commits.iter().enumerate() {
                    let at: Vec<&str> = meta.refs.iter()
                        .filter(|r| r.sha == c.sha)
                        .map(|r| r.name.strip_prefix("refs/heads/")
                                  .unwrap_or(&r.name))
                        .collect();
                    println!("{i:>4}  {}  {}{}", &c.sha[..8], c.subject(),
                             if at.is_empty() { String::new() }
                             else { format!("  [{}]", at.join(", ")) });
                }
            })
        }
        ("mirror", [url, root, rest @ ..])
            if rest.iter().all(|f| f == "--frugal" || f == "--whole") =>
        {
            let opts = crate::MirrorOpts {
                frugal: rest.iter().any(|f| f == "--frugal"),
                whole: rest.iter().any(|f| f == "--whole"),
                ..Default::default()
            };
            crate::mirror_opts(url, &PathBuf::from(root), opts).map(|o| {
                println!(
                    "{} new commits ({} total)",
                    o.update.new_commits,
                    o.update.total_commits,
                );
                for r in &o.update.refs {
                    println!("{} {}", r.sha, r.name);
                }
            })
        }
        ("export", [store, repo]) => {
            crate::export(&PathBuf::from(store), &PathBuf::from(repo)).map(|refs| {
                for r in &refs {
                    println!("{} {} (verified)", r.sha, r.name);
                }
            })
        }
        ("union", [repo, store, rest @ ..]) => {
            let Some(rest) = take_shard_bits(rest) else { return usage() };
            let level = match &rest[..] {
                [] => 3,
                [flag, n] if flag == "--level" => match n.parse() {
                    Ok(v) => v,
                    Err(_) => return usage(),
                },
                _ => return usage(),
            };
            crate::union_import(&PathBuf::from(repo), &PathBuf::from(store), level).map(|o| {
                println!("{} revisions, {} lanes, {} bytes on disk", o.n_rev, o.n_lanes, o.on_disk);
            })
        }
        ("union-update", [repo, store, rest @ ..]) => {
            let level = match rest {
                [] => 3,
                [flag, n] if flag == "--level" => match n.parse() {
                    Ok(v) => v,
                    Err(_) => return usage(),
                },
                _ => return usage(),
            };
            crate::union_update(&PathBuf::from(repo), &PathBuf::from(store), level).map(|o| {
                println!("{} revisions, {} lanes, {} bytes on disk", o.n_rev, o.n_lanes, o.on_disk);
            })
        }
        ("memgraph", [repo]) => {
            crate::memgraph::report(&PathBuf::from(repo)).map(|s| println!("{s}"))
        }
        ("union-verify", [repo, store, rest @ ..]) => {
            let stride = match rest {
                [] => 1,
                [n] => n.parse().unwrap_or(1),
                _ => return usage(),
            };
            crate::union_verify(&PathBuf::from(repo), &PathBuf::from(store), stride).map(|(n, bad)| {
                println!("{n} commit trees checked, {bad} SHA mismatches");
            })
        }
        _ => return usage(),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("gitdepot: {e}");
            1
        }
    }
}
