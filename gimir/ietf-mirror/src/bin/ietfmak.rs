//! `ietfmak` — the IETF internet-drafts mirror driver (MIRRORS.md
//! phase 2).
//!
//!   ietfmak update <root>              discover + fetch + import
//!   ietfmak list <root>                mirrored draft names
//!   ietfmak head <root> <draft>        newest revision label + date
//!   ietfmak text <root> <draft>        newest revision body
//!   ietfmak history <root> <draft>     all revisions, newest-first

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use ietf_mirror::{FetchConfig, Mirror, MirrorConfig};

fn open(root: &str) -> Result<Mirror, String> {
    Mirror::open(MirrorConfig::new(PathBuf::from(root))).map_err(|e| e.to_string())
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("ietfmak/0 (sarun mirror; contact: local)")
        .build()
        .map_err(|e| e.to_string())
}

fn cmd_update(root: &str) -> Result<(), String> {
    let mut m = open(root)?;
    let s = m
        .update(&client()?, &FetchConfig::default(), |label, fetched| {
            if fetched {
                eprintln!("fetch {label}");
            }
        })
        .map_err(|e| e.to_string())?;
    println!(
        "drafts {} ({} new)  revisions fetched {}  skipped {}  missing {}",
        s.drafts_seen, s.drafts_new, s.revisions_fetched, s.revisions_skipped, s.revisions_missing
    );
    Ok(())
}

fn cmd_list(root: &str) -> Result<(), String> {
    for name in open(root)?.drafts().map_err(|e| e.to_string())? {
        println!("{name}");
    }
    Ok(())
}

fn cmd_head(root: &str, draft: &str) -> Result<(), String> {
    match open(root)?.head(draft).map_err(|e| e.to_string())? {
        Some(e) => {
            println!("rev {}  date {}", e.rev, e.date.as_deref().unwrap_or("-"));
            Ok(())
        }
        None => Err(format!("no draft {draft}")),
    }
}

fn cmd_text(root: &str, draft: &str) -> Result<(), String> {
    match open(root)?.head(draft).map_err(|e| e.to_string())? {
        Some(e) => std::io::stdout().write_all(&e.text).map_err(|e| e.to_string()),
        None => Err(format!("no draft {draft}")),
    }
}

fn cmd_history(root: &str, draft: &str) -> Result<(), String> {
    let entries = open(root)?.history(draft).map_err(|e| e.to_string())?;
    if entries.is_empty() {
        return Err(format!("no draft {draft}"));
    }
    for e in entries {
        println!("rev {}\tdate {}\tlen {}", e.rev, e.date.as_deref().unwrap_or("-"), e.text.len());
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let r = match strs.as_slice() {
        ["update", root] => cmd_update(root),
        ["list", root] => cmd_list(root),
        ["head", root, draft] => cmd_head(root, draft),
        ["text", root, draft] => cmd_text(root, draft),
        ["history", root, draft] => cmd_history(root, draft),
        _ => Err("usage: ietfmak update|list <root>\n\
                  \x20      ietfmak head|text|history <root> <draft>".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ietfmak: {e}");
            ExitCode::FAILURE
        }
    }
}
