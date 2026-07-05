//! CLI: `gitdepot import <git-repo> <store-dir> [--level N]`
//!      `gitdepot update <git-repo> <store-dir> [--level N]`
//!      `gitdepot export <store-dir> <new-repo-dir>`

use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: gitdepot import <git-repo> <store-dir> [--level N]\n       gitdepot update <git-repo> <store-dir> [--level N]\n       gitdepot export <store-dir> <new-repo-dir>"
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
            let level = match rest {
                [] => 3,
                [flag, n] if flag == "--level" => match n.parse() {
                    Ok(v) => v,
                    Err(_) => return usage(),
                },
                _ => return usage(),
            };
            gitdepot::import(&PathBuf::from(repo), &PathBuf::from(store), level).map(|o| {
                let r = &o.report;
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
