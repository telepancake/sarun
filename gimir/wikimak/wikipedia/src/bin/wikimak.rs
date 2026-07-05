//! `wikimak` — the wikipedia-mirror driver CLI (MIRRORS.md phase 1).
//!
//!   wikimak discover <dbname>                  list newest complete run
//!   wikimak fetch <dbname> <root>               discover + fetch + import
//!   wikimak import <dump.xml[.bz2]> <root>     import/refresh a dump
//!   wikimak head <root> <page_id>              newest revision meta
//!   wikimak text <root> <page_id>              newest revision text
//!   wikimak history <root> <page_id>           all revisions, newest-first
//!
//! The instance lives under <root>/ (depot chains + titles pool +
//! meta.db). Import is idempotent: already-seen (page,rev) pairs dedup.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use wikimak_mediawiki::new_page_stream;
use wikimak_wikipedia::{Instance, InstanceConfig};

fn open_instance(root: PathBuf) -> Result<Instance, String> {
    Instance::open(InstanceConfig {
        root,
        dbname: "wiki".into(),
        // Sized for full non-en wikis; enwiki wants ~1e8 (index = 8B/page).
        max_chain_id: 4_000_000,
        depot: wikimak_depot::DepotConfig {
            root: PathBuf::new(), // forced to <root>/depot/
            max_chain_id: 4_000_000,
            file_size_threshold: 1 << 30,
            eviction_dead_ratio: 0.5,
        },
        title_shard_count: 4,
        title_seal_threshold_bytes: 8 << 20,
        f1_seal_threshold_bytes: 0, // default (256 KiB)
    })
    .map_err(|e| e.to_string())
}

fn cmd_import(dump: &str, root: &str) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root))?;
    let f = std::fs::File::open(dump).map_err(|e| format!("{dump}: {e}"))?;
    let reader: Box<dyn Read + Send> = if dump.ends_with(".bz2") {
        Box::new(wikimak_mediawiki::bz2::new_bz2_reader(
            f, wikimak_mediawiki::bz2::Bz2Options { workers: 0 }))
    } else {
        Box::new(f)
    };
    let mut stream = new_page_stream(reader);
    let stats = inst.import(&mut stream).map_err(|e| e.to_string())?;
    inst.flush().map_err(|e| e.to_string())?;
    println!(
        "pages {}  revisions new {}  deduped {}  sha1 ok/fudged/mismatch {}/{}/{}",
        stats.pages, stats.revisions_new, stats.revisions_deduped,
        stats.sha1_ok, stats.sha1_fudged, stats.sha1_mismatch
    );
    Ok(())
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("wikimak/0 (sarun mirror; contact: local)")
        .build()
        .map_err(|e| e.to_string())
}

fn cmd_discover(dbname: &str) -> Result<(), String> {
    let client = http_client()?;
    let run = wikimak_mediawiki::discover(&client, dbname).map_err(|e| e.to_string())?;
    println!("run {} ({:?}), {} parts", run.date, run.source, run.parts.len());
    for p in &run.parts {
        println!("  {}\t{} bytes\t{}", p.filename, p.size_bytes,
                 p.sha256.as_deref().or(p.sha1.as_deref()).unwrap_or("-"));
    }
    Ok(())
}

fn cmd_fetch(dbname: &str, root: &str) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root))?;
    let client = http_client()?;
    let (run, stats) = wikimak_wikipedia::sync(
        &inst, &client, &wikimak_mediawiki::Config::default(), dbname,
        |name, fetched| eprintln!("{} {}", if fetched { "fetch" } else { "skip " }, name),
    ).map_err(|e| e.to_string())?;
    println!(
        "run {}  parts {}/{} fetched ({} skipped)  pages {}  revisions new {}  deduped {}",
        run.date, stats.parts_fetched, stats.parts_total, stats.parts_skipped,
        stats.import.pages, stats.import.revisions_new, stats.import.revisions_deduped
    );
    Ok(())
}

fn cmd_pages(root: &str, filter: Option<&str>) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root))?;
    for (id, title) in inst.pages(filter, 200).map_err(|e| e.to_string())? {
        println!("{id:>8}  {title}");
    }
    Ok(())
}

fn cmd_head(root: &str, page: u64) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root))?;
    match inst.page_head(page).map_err(|e| e.to_string())? {
        Some(m) => {
            println!("rev {} parent {} ts {} comment {:?}",
                     m.rev_id, m.parent_id, m.ts, m.comment);
            Ok(())
        }
        None => Err(format!("no page {page}")),
    }
}

fn cmd_text(root: &str, page: u64) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root))?;
    match inst.page_head_text(page).map_err(|e| e.to_string())? {
        Some(t) => {
            std::io::stdout().write_all(&t).map_err(|e| e.to_string())?;
            Ok(())
        }
        None => Err(format!("no page {page}")),
    }
}

fn cmd_history(root: &str, page: u64) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root))?;
    for entry in inst.page_history(page).map_err(|e| e.to_string())? {
        let e = entry.map_err(|e| e.to_string())?;
        println!("rev {}\tts {}\tlen {}\t{:?}",
                 e.meta.rev_id, e.meta.ts, e.meta.text_len, e.meta.comment);
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let r = match strs.as_slice() {
        ["discover", dbname] => cmd_discover(dbname),
        ["fetch", dbname, root] => cmd_fetch(dbname, root),
        ["import", dump, root] => cmd_import(dump, root),
        ["pages", root] => cmd_pages(root, None),
        ["pages", root, filter] => cmd_pages(root, Some(filter)),
        ["head", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_head(root, p)),
        ["text", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_text(root, p)),
        ["history", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_history(root, p)),
        _ => Err("usage: wikimak discover <dbname>\n\
                  \x20      wikimak pages <root> [filter]\n\
                  \x20      wikimak fetch <dbname> <root>\n\
                  \x20      wikimak import <dump.xml[.bz2]> <root>\n\
                  \x20      wikimak head|text|history <root> <page_id>".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("wikimak: {e}"); ExitCode::FAILURE }
    }
}
