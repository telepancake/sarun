//! `wikimak` — the wikipedia-mirror driver CLI (MIRRORS.md phase 1).
//!
//!   wikimak discover <dbname>                  list newest complete run
//!   wikimak fetch <dbname> <root>               discover + fetch + import
//!   wikimak refresh-full <dbname> <root>        explicit full snapshot ingest
//!   wikimak reconcile-history <dbname> <root>   explicit metadata rebuild
//!   wikimak repack-f0 <root>                    retrain dictionary + repack heads
//!   wikimak import <dump.xml[.bz2]> <root>     import/refresh a dump
//!   wikimak head <root> <page_id>              newest revision meta
//!   wikimak text <root> <page_id>              newest revision text
//!   wikimak history <root> <page_id>           all revisions, newest-first
//!
//! The instance lives under <root>/ (depot chains + titles pool +
//! meta.db). Import is idempotent: already-seen (page,rev) pairs dedup.

use std::io::{Read, Write};
use std::path::PathBuf;

use wikimak_mediawiki::new_page_stream;
use crate::{Instance, InstanceConfig};

/// Open an instance. `--max-page-id` remains accepted for command-line
/// compatibility, but fresh indexes start at one slot and grow from
/// actual page ids; no value can make an import overflow below the
/// 2^40 sanity ceiling.
fn open_instance(root: PathBuf, max_page_id: Option<u64>) -> Result<Instance, String> {
    let max_chain_id =
        max_page_id.unwrap_or_else(|| crate::instance::max_chain_id_for_root(&root));
    Instance::open(InstanceConfig {
        root,
        dbname: "wiki".into(),
        max_chain_id,
        depot: wikimak_depot::DepotConfig {
            root: PathBuf::new(), // forced to <root>/depot/
            max_chain_id,
            file_size_threshold: 1 << 30,
            eviction_dead_ratio: 0.5,
        },
        // Derive from the store's persisted count (fresh roots start small
        // and grow by atomic title-pool re-sharding).
        // An explicit count here would refuse to open any store built
        // with a different one — the count is the store's property.
        title_shard_count: 0,
        title_seal_threshold_bytes: 64 << 10,
        f1_seal_threshold_bytes: 0, // default (256 KiB)
    })
    .map_err(|e| e.to_string())
}

fn cmd_import(dump: &str, root: &str, max_page_id: Option<u64>) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), max_page_id)?;
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
    // Import session over: reclaim the churn slack (dead superseded
    // heads) parked in the depot's current write files.
    inst.collect().map_err(|e| e.to_string())?;
    println!(
        "pages {}  revisions new {}  deduped {}  sha1 ok/fudged/mismatch {}/{}/{}",
        stats.pages, stats.revisions_new, stats.revisions_deduped,
        stats.sha1_ok, stats.sha1_fudged, stats.sha1_mismatch
    );
    Ok(())
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    let operator = std::env::var("SARUN_WIKIMEDIA_CONTACT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("; operator: {value}"))
        .unwrap_or_default();
    reqwest::blocking::Client::builder()
        .user_agent(format!(
            "sarun-wikimak/{} (+https://github.com/telepancake/sarun{operator})",
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(std::time::Duration::from_secs(30))
        // A full-history part can legitimately stream for hours. This is
        // finite so a dead connection cannot pin a scheduled job forever.
        .timeout(std::time::Duration::from_secs(24 * 3600))
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

fn cmd_fetch(dbname: &str, root: &str, max_page_id: Option<u64>) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), max_page_id)?;
    let client = http_client()?;
    let stats = crate::maintain(
        &inst, &client, &wikimak_mediawiki::Config::default(), dbname,
        |name, fetched| eprintln!("{} {}", if fetched { "fetch" } else { "skip " }, name),
    ).map_err(|e| e.to_string())?;
    println!(
        "daily maintenance: content parts {}/{} fetched ({} skipped)  pages {}  revisions new {}  deduped {}  history parts {}  page actions {}",
        stats.parts_fetched, stats.parts_total, stats.parts_skipped,
        stats.import.pages, stats.import.revisions_new, stats.import.revisions_deduped,
        stats.history_parts_fetched, stats.page_actions
    );
    Ok(())
}

fn cmd_refresh_full(dbname: &str, root: &str, max_page_id: Option<u64>) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), max_page_id)?;
    let client = http_client()?;
    let (run, stats) = crate::sync(
        &inst, &client, &wikimak_mediawiki::Config::default(), dbname,
        |name, fetched| eprintln!("{} {}", if fetched { "fetch" } else { "skip " }, name),
    ).map_err(|e| e.to_string())?;
    println!(
        "full content snapshot {}: parts {}/{} fetched ({} skipped)  pages {}  revisions new {}  deduped {}",
        run.date, stats.parts_fetched, stats.parts_total, stats.parts_skipped,
        stats.import.pages, stats.import.revisions_new, stats.import.revisions_deduped
    );
    Ok(())
}

fn cmd_repack_f0(root: &str, max_page_id: Option<u64>) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), max_page_id)?;
    let stats = inst.retrain_revision_dictionary().map_err(|e| e.to_string())?;
    let Some(dictionary_id) = stats.dictionary_id else {
        return Err(format!(
            "not enough revision data to train a dictionary ({} complete records, {} bytes)",
            stats.samples, stats.sample_bytes
        ));
    };
    println!(
        "f0 dictionary {dictionary_id:08x}: {} bytes trained from {} complete revisions ({} bytes); heads repacked {}",
        stats.dictionary_bytes, stats.samples, stats.sample_bytes, stats.heads_repacked
    );
    Ok(())
}

fn cmd_reconcile_history(
    dbname: &str,
    root: &str,
    max_page_id: Option<u64>,
) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), max_page_id)?;
    let client = http_client()?;
    let stats = crate::reconcile_history(
        &inst, &client, &wikimak_mediawiki::Config::default(), dbname,
        |name, fetched| eprintln!("{} {}", if fetched { "fetch" } else { "skip " }, name),
    ).map_err(|e| e.to_string())?;
    println!(
        "MediaWiki History reconciliation: parts {}  page actions {}",
        stats.history_parts_fetched, stats.page_actions
    );
    Ok(())
}

fn cmd_pages(root: &str, filter: Option<&str>) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), None)?;
    for (id, title) in inst.pages(filter, 200).map_err(|e| e.to_string())? {
        println!("{id:>8}  {title}");
    }
    Ok(())
}

fn cmd_head(root: &str, page: u64) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), None)?;
    match inst.page_head(page).map_err(|e| e.to_string())? {
        Some(m) => {
            println!("rev {} parent {} ts {} comment {:?}",
                     m.rev_id, m.parent_id, m.ts, m.comment);
            Ok(())
        }
        None => Err(format!("no page {page}")),
    }
}

fn cmd_text(root: &str, page: u64, asof_micros: Option<i64>) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), None)?;
    let text = match asof_micros {
        None => inst.page_head_text(page),
        Some(ts) => inst.page_text_at(page, Some(ts)),
    }
    .map_err(|e| e.to_string())?;
    match text {
        Some(t) => {
            std::io::stdout().write_all(&t).map_err(|e| e.to_string())?;
            Ok(())
        }
        None => Err(format!("no page {page}")),
    }
}

#[cfg(feature = "serve")]
fn cmd_serve(root: &str, addr: &str) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), None)?;
    let cfg = crate::serve::ServeConfig {
        root: PathBuf::from(root),
        addr: addr.to_string(),
        media_cache: PathBuf::from(root).join("media"),
    };
    crate::serve::serve(inst, cfg)
}

fn cmd_history(root: &str, page: u64) -> Result<(), String> {
    let inst = open_instance(PathBuf::from(root), None)?;
    for entry in inst.page_history(page).map_err(|e| e.to_string())? {
        let e = entry.map_err(|e| e.to_string())?;
        println!("rev {}\tts {}\tlen {}\t{:?}",
                 e.meta.rev_id, e.meta.ts, e.meta.text_len, e.meta.comment);
    }
    Ok(())
}

/// The `wikimak` CLI entry, callable in-process: the sarun engine binary
/// embeds this crate (with `fetch`) and dispatches here on
/// `sarun wikimak …` / an argv[0] symlink named `wikimak`.
pub fn cli_main(args: &[String]) -> i32 {
    // Strip `--max-page-id N` / `--max-page-id=N` (any position): the
    // page-id bound for `import`/`fetch` on a FRESH root. Existing
    // roots derive the bound from their depot index; the flag against
    // a mismatched existing index fails loudly (IndexSizeMismatch).
    let mut max_page_id: Option<u64> = None;
    let mut strs: Vec<&str> = Vec::with_capacity(args.len());
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        let v = if a == "--max-page-id" {
            it.next()
        } else if let Some(v) = a.strip_prefix("--max-page-id=") {
            Some(v)
        } else {
            strs.push(a);
            continue;
        };
        match v.and_then(|v| v.parse::<u64>().ok()).filter(|&n| n > 0) {
            Some(n) => max_page_id = Some(n),
            None => {
                eprintln!("wikimak: --max-page-id wants a positive integer");
                return 1;
            }
        }
    }
    let r = match strs.as_slice() {
        ["discover", dbname] => cmd_discover(dbname),
        ["fetch", dbname, root] => cmd_fetch(dbname, root, max_page_id),
        ["refresh-full", dbname, root] => cmd_refresh_full(dbname, root, max_page_id),
        ["repack-f0", root] => cmd_repack_f0(root, max_page_id),
        ["reconcile-history", dbname, root] => {
            cmd_reconcile_history(dbname, root, max_page_id)
        }
        ["import", dump, root] => cmd_import(dump, root, max_page_id),
        ["pages", root] => cmd_pages(root, None),
        ["pages", root, filter] => cmd_pages(root, Some(filter)),
        #[cfg(feature = "serve")]
        ["serve", root] => cmd_serve(root, "127.0.0.1:8642"),
        #[cfg(feature = "serve")]
        ["serve", root, addr] => cmd_serve(root, addr),
        ["head", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_head(root, p)),
        ["text", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_text(root, p, None)),
        ["text", root, page, asof] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| Ok((p, asof.parse::<i64>().map_err(|e| format!("asof: {e}"))?)))
            .and_then(|(p, ts)| cmd_text(root, p, Some(ts))),
        ["history", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_history(root, p)),
        _ => Err("usage: wikimak discover <dbname>\n\
                  \x20      wikimak pages <root> [filter]\n\
                  \x20      wikimak fetch <dbname> <root> [--max-page-id N]\n\
                  \x20      wikimak refresh-full <dbname> <root> [--max-page-id N]\n\
                  \x20      wikimak repack-f0 <root> [--max-page-id N]\n\
                  \x20      wikimak reconcile-history <dbname> <root> [--max-page-id N]\n\
                  \x20      wikimak import <dump.xml[.bz2]> <root> [--max-page-id N]\n\
                  \x20      wikimak serve <root> [addr]        (default 127.0.0.1:8642)\n\
                  \x20      wikimak head|history <root> <page_id>\n\
                  \x20      wikimak text <root> <page_id> [asof-unix-micros]".into()),
    };
    match r {
        Ok(()) => 0,
        Err(e) => { eprintln!("wikimak: {e}"); 1 }
    }
}
