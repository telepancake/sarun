//! CLI: `gitdepot import <git-repo> <store-dir> [--level N] [--report]`
//!      `gitdepot update <git-repo> <store-dir> [--level N]`
//!      `gitdepot mirror <url> <root-dir> [--frugal]`
//!      `gitdepot list <mirrors-root>`
//!      `gitdepot log <store-dir>`
//!      `gitdepot export <store-dir> <new-repo-dir>`

use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: gitdepot import <git-repo> <store-dir> [--level N] [--report]\n       gitdepot update <git-repo> <store-dir> [--level N]\n       gitdepot mirror <url> <root-dir> [--frugal]\n       gitdepot list <mirrors-root>\n       gitdepot log <store-dir>\n       gitdepot export <store-dir> <new-repo-dir>"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => return usage(),
    };
    let result = match (cmd, rest) {
        ("import", [repo, store, rest @ ..]) => {
            let (level, report) = match rest {
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
            gitdepot::import_opts(&PathBuf::from(repo), &PathBuf::from(store),
                                  level, report).map(|o| {
                let Some(r) = &o.report else {
                    println!("{} commits imported", o.meta.commits.len());
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
            gitdepot::update(&PathBuf::from(repo), &PathBuf::from(store), level).map(|o| {
                println!("{} new commits ({} total)", o.new_commits, o.total_commits);
                for r in &o.refs {
                    println!("{} {}", r.sha, r.name);
                }
            })
        }
        ("list", [root]) => {
            gitdepot::list_mirrors(&PathBuf::from(root)).map(|entries| {
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
            gitdepot::chain::read_meta(&PathBuf::from(store)).map(|meta| {
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
            if rest.is_empty() || rest == ["--frugal"] =>
        {
            gitdepot::mirror_opts(url, &PathBuf::from(root),
                                  !rest.is_empty()).map(|o| {
                println!(
                    "{} new commits ({} total){}",
                    o.update.new_commits,
                    o.update.total_commits,
                    if o.reimported { "  [re-imported: remote rewrote history]" } else { "" }
                );
                for r in &o.update.refs {
                    println!("{} {}", r.sha, r.name);
                }
            })
        }
        ("export", [store, repo]) => {
            gitdepot::export(&PathBuf::from(store), &PathBuf::from(repo)).map(|refs| {
                for r in &refs {
                    println!("{} {} (verified)", r.sha, r.name);
                }
            })
        }
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gitdepot: {e}");
            ExitCode::FAILURE
        }
    }
}
