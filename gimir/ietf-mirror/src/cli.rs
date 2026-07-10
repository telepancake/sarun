//! `ietfmak` — the IETF internet-drafts mirror driver (MIRRORS.md
//! phase 2).
//!
//!   ietfmak update <root> [--delay-ms N]   discover + fetch + import
//!   ietfmak list <root>                mirrored draft names
//!   ietfmak head <root> <draft>        newest revision label + date
//!   ietfmak text <root> <draft>        newest revision body
//!   ietfmak history <root> <draft>     all revisions, newest-first

use std::io::Write;
use std::path::PathBuf;

use crate::{FetchConfig, Mirror, MirrorConfig};

fn open(root: &str) -> Result<Mirror, String> {
    Mirror::open(MirrorConfig::new(PathBuf::from(root))).map_err(|e| e.to_string())
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("ietfmak/0 (sarun mirror; contact: local)")
        // A hung connection must fail (and retry), not stall the run.
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

fn cmd_update(root: &str, delay_ms: Option<u64>) -> Result<(), String> {
    let mut m = open(root)?;
    // Test/override seam: point the fetch at a stand-in host.
    let mut cfg = FetchConfig::default();
    if let Ok(u) = std::env::var("IETFMAK_BASE_URL") {
        if !u.is_empty() {
            cfg.base_url = u;
        }
    }
    if let Some(ms) = delay_ms {
        cfg.delay = std::time::Duration::from_millis(ms);
    }
    let s = m
        .update(&client()?, &cfg, |label, fetched| {
            if fetched {
                eprintln!("fetch {label}");
            }
        })
        .map_err(|e| e.to_string())?;
    if s.index_not_modified {
        println!("index unchanged (304): nothing to do");
        return Ok(());
    }
    println!(
        "drafts {} ({} new)  revisions fetched {}  skipped {}  missing {}  \
         reconciled {}  chains rebuilt {}",
        s.drafts_seen, s.drafts_new, s.revisions_fetched, s.revisions_skipped,
        s.revisions_missing, s.revisions_reconciled, s.chains_rebuilt
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

/// The `ietfmak` CLI entry, callable in-process: the sarun engine binary
/// embeds this crate (with `fetch`) and dispatches here on
/// `sarun ietfmak …` / an argv[0] symlink named `ietfmak`.
pub fn cli_main(args: &[String]) -> i32 {
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let r = match strs.as_slice() {
        ["update", root] => cmd_update(root, None),
        ["update", root, "--delay-ms", ms] => ms
            .parse()
            .map_err(|_| format!("bad --delay-ms {ms:?}"))
            .and_then(|ms| cmd_update(root, Some(ms))),
        ["list", root] => cmd_list(root),
        ["head", root, draft] => cmd_head(root, draft),
        ["text", root, draft] => cmd_text(root, draft),
        ["history", root, draft] => cmd_history(root, draft),
        _ => Err("usage: ietfmak update <root> [--delay-ms N]\n\
                  \x20      ietfmak list <root>\n\
                  \x20      ietfmak head|text|history <root> <draft>".into()),
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ietfmak: {e}");
            1
        }
    }
}
