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
//!   wikimak archive-export <root> <file>        portable ordered event stream
//!   wikimak archive-import <file> <root>        initialize depot from archive
//!   wikimak archive-build-direct <db> <file> <scratch>
//!   wikimak archive-build-update <db> <base> <file> <scratch>
//!   wikimak archive-fetch-siteinfo <api-url> <file>
//!   wikimak archive-title-index <archive> <index>
//!   wikimak archive-repack <input> <output> <frame-bytes> <level> [settings]
//!   wikimak archive-merge <output> <frame-bytes> <level> <inputs...>
//!   wikimak archive-inspect <file>              validate and summarize stream
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

fn cmd_archive_import(
    archive: &str,
    root: &str,
    max_page_id: Option<u64>,
) -> Result<(), String> {
    let root_path = PathBuf::from(root);
    if !root_path.join("meta.db").exists()
        && root_path.exists()
        && std::fs::read_dir(&root_path)
            .map_err(|error| format!("{}: {error}", root_path.display()))?
            .next()
            .is_some()
    {
        return Err(format!(
            "archive-import initializes a new depot; {} is not empty",
            root_path.display()
        ));
    }
    let instance = open_instance(root_path, max_page_id)?;
    if instance
        .sync_state("full_snapshot_date")
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("archive-import initializes a new depot; this depot is complete".into());
    }
    let stats = crate::archive::import_instance(&instance, archive, |stats| {
        if stats.pages != 0 {
            eprintln!(
                "archive import: {} pages, {} revisions, {} page actions, {} user records",
                stats.pages, stats.revisions, stats.page_actions, stats.user_records
            );
        }
    })
    .map_err(|error| error.to_string())?;
    instance.collect().map_err(|error| error.to_string())?;
    println!(
        "archive import complete: {} pages, {} revisions, {} page actions, {} user records",
        stats.pages, stats.revisions, stats.page_actions, stats.user_records
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
        "daily maintenance: content parts {}/{} fetched ({} skipped)  pages {}  revisions new {}  deduped {}  history parts {}  page/user actions {}/{}",
        stats.parts_fetched, stats.parts_total, stats.parts_skipped,
        stats.import.pages, stats.import.revisions_new, stats.import.revisions_deduped,
        stats.history_parts_fetched, stats.page_actions, stats.user_actions
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

fn cmd_experiment_split_revisions(
    root: &str,
    workers: usize,
    packed_shards: usize,
) -> Result<(), String> {
    let inst = Instance::open_read(crate::read_config(PathBuf::from(root)))
        .map_err(|e| e.to_string())?;
    let stats = inst
        .experiment_split_revision_storage(workers, packed_shards)
        .map_err(|e| e.to_string())?;
    let current = stats.current_live_f0_bytes
        + stats.current_live_f1_bytes
        + stats.current_live_cold_bytes;
    let split =
        stats.metadata_frame_bytes + stats.head_text_frame_bytes + stats.history_text_frame_bytes;
    let packed = split - stats.packed_small_split_frame_bytes + stats.packed_small_file_bytes;
    let combined = stats.combined_f0_frame_bytes + stats.combined_history_frame_bytes;
    let packed_combined =
        combined - stats.packed_small_combined_frame_bytes + stats.packed_small_file_bytes;
    println!(
        "pages {}  revisions {}  workers {}\n\
         revisions per page (1,2,3,4,5-7,8-15,...,4096-8191,8192+): {:?}\n\
         text dictionary {:08x}: {} bytes from {} full heads ({} bytes)\n\
         metadata dictionary {:08x}: {} bytes from {} full SHA-free metadata records ({} bytes)\n\
         metadata raw/compressed/framed: {}/{}/{}\n\
         head text raw/compressed/framed: {}/{}/{}\n\
         head text length buckets (0,1-31,32-63,...,131072-262143,262144+): {:?}\n\
         history text raw/compressed/framed: {}/{}/{} in {} frames\n\
         combined dictionary {:08x}: {} bytes from {} full SHA-free records ({} bytes)\n\
         combined f0 raw/compressed/framed: {}/{}/{}\n\
         combined history raw/compressed/framed: {}/{}/{}\n\
         combined standalone total: {}\n\
         packed small (max 65536 decoded bytes per page group):\n\
           pages/revisions/raw: {}/{}/{}\n\
           compressed/file bytes: {}/{} in {} shards\n\
           compressed shard p50/p95/p99/max: {}/{}/{}/{}\n\
           replaced split bytes: {}  total with packed small: {}  delta from split: {:+}\n\
           replaced combined bytes: {}  combined total with packed small: {}  delta: {:+}\n\
           decoded shard scan mean/p50/p95/p99/max: {}/{}/{}/{}/{}\n\
           representative shard pages/raw/compressed/iterations: {}/{}/{}/{}\n\
           streaming extract first/middle/last ns: {}/{}/{}\n\
           latest small head timestamp micros: {}\n\
           dirty pages/shards/rewrite bytes in latest 1d: {}/{}/{}\n\
           dirty pages/shards/rewrite bytes in latest 7d: {}/{}/{}\n\
         current live f0/f1/cold/total: {}/{}/{}/{}\n\
         experimental framed total: {}  delta: {:+}",
        stats.pages,
        stats.revisions,
        workers,
        stats.revision_count_buckets,
        stats.text_dictionary_id,
        stats.text_dictionary_bytes,
        stats.text_samples,
        stats.text_sample_bytes,
        stats.metadata_dictionary_id,
        stats.metadata_dictionary_bytes,
        stats.metadata_samples,
        stats.metadata_sample_bytes,
        stats.metadata_raw_bytes,
        stats.metadata_compressed_bytes,
        stats.metadata_frame_bytes,
        stats.head_text_raw_bytes,
        stats.head_text_compressed_bytes,
        stats.head_text_frame_bytes,
        stats.head_text_length_buckets,
        stats.history_text_raw_bytes,
        stats.history_text_compressed_bytes,
        stats.history_text_frame_bytes,
        stats.history_text_frames,
        stats.combined_dictionary_id,
        stats.combined_dictionary_bytes,
        stats.combined_samples,
        stats.combined_sample_bytes,
        stats.combined_f0_raw_bytes,
        stats.combined_f0_compressed_bytes,
        stats.combined_f0_frame_bytes,
        stats.combined_history_raw_bytes,
        stats.combined_history_compressed_bytes,
        stats.combined_history_frame_bytes,
        combined,
        stats.packed_small_pages,
        stats.packed_small_revisions,
        stats.packed_small_raw_bytes,
        stats.packed_small_compressed_bytes,
        stats.packed_small_file_bytes,
        stats.packed_small_materialized_shards,
        stats.packed_small_p50_compressed_shard_bytes,
        stats.packed_small_p95_compressed_shard_bytes,
        stats.packed_small_p99_compressed_shard_bytes,
        stats.packed_small_max_compressed_shard_bytes,
        stats.packed_small_split_frame_bytes,
        packed,
        packed as i128 - split as i128,
        stats.packed_small_combined_frame_bytes,
        packed_combined,
        packed_combined as i128 - combined as i128,
        stats.packed_small_mean_scan_bytes,
        stats.packed_small_p50_scan_bytes,
        stats.packed_small_p95_scan_bytes,
        stats.packed_small_p99_scan_bytes,
        stats.packed_small_max_scan_bytes,
        stats.packed_small_benchmark_pages,
        stats.packed_small_benchmark_raw_bytes,
        stats.packed_small_benchmark_compressed_bytes,
        stats.packed_small_benchmark_iterations,
        stats.packed_small_first_extract_ns,
        stats.packed_small_middle_extract_ns,
        stats.packed_small_last_extract_ns,
        stats.packed_small_latest_head_ts_micros,
        stats.packed_small_dirty_1d_pages,
        stats.packed_small_dirty_1d_shards,
        stats.packed_small_rewrite_1d_bytes,
        stats.packed_small_dirty_7d_pages,
        stats.packed_small_dirty_7d_shards,
        stats.packed_small_rewrite_7d_bytes,
        stats.current_live_f0_bytes,
        stats.current_live_f1_bytes,
        stats.current_live_cold_bytes,
        current,
        split,
        split as i128 - current as i128,
    );
    Ok(())
}

fn cmd_experiment_packed_f0(
    root: &str,
    workers: usize,
    packed_shards: usize,
) -> Result<(), String> {
    let inst = Instance::open_read(crate::read_config(PathBuf::from(root)))
        .map_err(|e| e.to_string())?;
    let stats = inst
        .experiment_packed_f0(workers, packed_shards)
        .map_err(|e| e.to_string())?;
    let hybrid = stats.all_standalone_frame_bytes
        - stats.replaced_standalone_frame_bytes
        + stats.packed_file_bytes;
    let standalone_total = stats.all_standalone_frame_bytes + stats.history_frame_bytes;
    let hybrid_total = hybrid + stats.history_frame_bytes;
    println!(
        "pages/small/big: {}/{}/{}\n\
         small raw bytes: {}\n\
         all standalone/replaced/packed/hybrid bytes: {}/{}/{}/{}\n\
         combined history raw/compressed/framed: {}/{}/{}\n\
         standalone total/hybrid total/delta: {}/{}/{:+}\n\
         packed compressed/file bytes in {} shards: {}/{}\n\
         compressed shard p50/p95/p99/max: {}/{}/{}/{}\n\
         shards over 1 MiB: {}\n\
         decoded shard scan mean/p50/p95/p99/max: {}/{}/{}/{}/{}\n\
         representative shard pages/raw/compressed/iterations: {}/{}/{}/{}\n\
         streaming extract first/middle/last ns: {}/{}/{}\n\
         hysteresis lower thresholds bytes: [65536,61440,57344,49152,32768]\n\
         pages with transitions: {:?}\n\
         total transitions: {:?}\n\
         current small pages after replay: {:?}",
        stats.pages,
        stats.small_pages,
        stats.big_pages,
        stats.small_raw_bytes,
        stats.all_standalone_frame_bytes,
        stats.replaced_standalone_frame_bytes,
        stats.packed_file_bytes,
        hybrid,
        stats.history_raw_bytes,
        stats.history_compressed_bytes,
        stats.history_frame_bytes,
        standalone_total,
        hybrid_total,
        hybrid_total as i128 - standalone_total as i128,
        stats.materialized_shards,
        stats.packed_compressed_bytes,
        stats.packed_file_bytes,
        stats.p50_compressed_shard_bytes,
        stats.p95_compressed_shard_bytes,
        stats.p99_compressed_shard_bytes,
        stats.max_compressed_shard_bytes,
        stats.oversized_1m_shards,
        stats.mean_scan_bytes,
        stats.p50_scan_bytes,
        stats.p95_scan_bytes,
        stats.p99_scan_bytes,
        stats.max_scan_bytes,
        stats.benchmark_pages,
        stats.benchmark_raw_bytes,
        stats.benchmark_compressed_bytes,
        stats.benchmark_iterations,
        stats.first_extract_ns,
        stats.middle_extract_ns,
        stats.last_extract_ns,
        stats.hysteresis_transition_pages,
        stats.hysteresis_transitions,
        stats.hysteresis_current_small_pages,
    );
    Ok(())
}

fn cmd_metadata_example(root: &str, page: u64) -> Result<(), String> {
    let inst = Instance::open_read(crate::read_config(PathBuf::from(root)))
        .map_err(|e| e.to_string())?;
    let meta = inst
        .page_head(page)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no page {page}"))?;
    let text = inst
        .page_head_text(page)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no text for page {page}"))?;
    let record = crate::revision::encode_revision(&meta, &text);
    let prefix = &record[..record.len() - text.len()];
    println!(
        "page {page}\nmeta {meta:?}\ntext bytes {}\nmetadata bytes {}\nhex {}",
        text.len(),
        prefix.len(),
        hex::encode(prefix),
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
        "MediaWiki History reconciliation: parts {}  page/user actions {}/{}",
        stats.history_parts_fetched, stats.page_actions, stats.user_actions
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

#[cfg(feature = "serve")]
fn cmd_archive_serve(path: &str, addr: &str) -> Result<(), String> {
    let started = std::time::Instant::now();
    let title_index = PathBuf::from(path).with_extension("swtitle");
    let archive = crate::archive_browse::ArchiveBrowseIndex::open(path, &title_index)
        .map_err(|error| error.to_string())?;
    eprintln!(
        "wikimak archive-serve: opened {} title intervals, {} frames in {:.3}s",
        archive.title_count(),
        archive.frame_count(),
        started.elapsed().as_secs_f64(),
    );
    let path = PathBuf::from(path);
    let media_cache = path.with_extension("media");
    crate::serve::serve_archive(std::sync::Arc::new(archive), addr.to_string(), media_cache)
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

fn cmd_archive_export(root: &str, output: &str) -> Result<(), String> {
    let inst = Instance::open_read(crate::read_config(PathBuf::from(root)))
        .map_err(|e| e.to_string())?;
    let output_path = std::path::Path::new(output);
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let stats = {
        let buffered = std::io::BufWriter::new(temporary.as_file_mut());
        crate::archive::export_instance(
            &inst,
            buffered,
            crate::archive::DEFAULT_FRAME_TARGET,
        )
        .map_err(|e| e.to_string())?
    };
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| format!("{}: {e}", temporary.path().display()))?;
    temporary
        .persist(output_path)
        .map_err(|e| format!("{output}: {}", e.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output_path, std::fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("{output}: {e}"))?;
    }
    let bytes = std::fs::metadata(output)
        .map_err(|e| format!("{output}: {e}"))?
        .len();
    println!(
        "archive pages {}  revisions {}  page actions {}  user actions {}  frames {}  bytes {}",
        stats.pages,
        stats.revisions,
        stats.page_actions,
        stats.user_actions,
        stats.frames,
        bytes
    );
    Ok(())
}

fn cmd_archive_build_direct(dbname: &str, output: &str, scratch: &str) -> Result<(), String> {
    let client = http_client()?;
    let stats = crate::build_direct_archive(
        &client, &wikimak_mediawiki::Config::default(), dbname,
        output, scratch, |message| eprintln!("{message}"),
    ).map_err(|error| error.to_string())?;
    println!(
        "content: {} parts, {} pages, {} revisions, {} bytes, {} frames",
        stats.content_parts, stats.pages, stats.revisions,
        stats.content_archive_bytes, stats.content_frames,
    );
    println!(
        "history: {} parts, {} events (page/user/global {}/{}/{}), {} bytes, {} frames",
        stats.history_parts, stats.history_events, stats.page_history_events,
        stats.user_history_events, stats.global_history_events,
        stats.history_archive_bytes, stats.history_frames,
    );
    println!(
        "output: {} bytes, {} frames; scratch peak {} bytes; elapsed {}.{:03}s",
        stats.output_bytes, stats.output_frames, stats.scratch_peak_bytes,
        stats.elapsed_millis / 1000, stats.elapsed_millis % 1000,
    );
    Ok(())
}

fn cmd_archive_build_update(
    dbname: &str,
    base: &str,
    output: &str,
    scratch: &str,
) -> Result<(), String> {
    let client = http_client()?;
    let stats = crate::build_update_archive(
        &client,
        &wikimak_mediawiki::Config::default(),
        dbname,
        base,
        output,
        scratch,
        3,
        crate::archive::DEFAULT_FRAME_TARGET,
        crate::archive::CompressionSettings::default(),
        |message| eprintln!("{message}"),
    )
    .map_err(|error| error.to_string())?;
    println!(
        "update {} through {}: {} daily runs/{} parts, {} pages, {} revisions; \
         history {} ({} parts); output {} records, {} frames, {} bytes; \
         scratch peak {} bytes; elapsed {}.{:03}s",
        stats.content_from,
        stats.content_through,
        stats.incremental_runs,
        stats.content_parts,
        stats.pages,
        stats.revisions,
        stats.metadata_snapshot,
        stats.history_parts,
        stats.output_records,
        stats.output_frames,
        stats.output_bytes,
        stats.scratch_peak_bytes,
        stats.elapsed_millis / 1000,
        stats.elapsed_millis % 1000,
    );
    Ok(())
}

fn cmd_archive_fetch_siteinfo(api_url: &str, output: &str) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("sarun-wikimak/0.1 (https://github.com/telepancake/sarun)")
        .build()
        .map_err(|error| error.to_string())?;
    crate::siteinfo::fetch_siteinfo_archive(
        &client,
        api_url,
        output,
    )
    .map_err(|error| error.to_string())
}

fn cmd_archive_title_index(
    archive: &str,
    output: &str,
) -> Result<(), String> {
    let entries =
        crate::title_index::build(archive, output).map_err(|error| error.to_string())?;
    println!(
        "title index {entries} entries, {} bytes",
        std::fs::metadata(output)
            .map_err(|error| error.to_string())?
            .len()
    );
    Ok(())
}

fn cmd_archive_repack(args: &[&str]) -> Result<(), String> {
    let [input, output, frame_target, level, settings @ ..] = args else {
        return Err(
            "archive-repack wants <input> <output> <frame-bytes> <zstd-level> \
             [--checksum] [--long-distance] [--window-log N] [--target-block-size N]"
                .into(),
        );
    };
    let frame_target = frame_target
        .parse::<usize>()
        .map_err(|error| format!("frame bytes: {error}"))?;
    if frame_target == 0 {
        return Err("frame bytes must be positive".into());
    }
    let mut compression = crate::archive::CompressionSettings {
        level: level
            .parse::<i32>()
            .map_err(|error| format!("zstd level: {error}"))?,
        ..crate::archive::CompressionSettings::default()
    };
    let mut options = settings.iter();
    while let Some(option) = options.next() {
        match *option {
            "--checksum" => compression.checksum = true,
            "--long-distance" => compression.long_distance_matching = true,
            "--window-log" => {
                compression.window_log = Some(
                    options
                        .next()
                        .ok_or("--window-log wants an integer")?
                        .parse()
                        .map_err(|error| format!("window log: {error}"))?,
                );
            }
            "--target-block-size" => {
                compression.target_block_size = Some(
                    options
                        .next()
                        .ok_or("--target-block-size wants an integer")?
                        .parse()
                        .map_err(|error| format!("target block size: {error}"))?,
                );
            }
            unknown => return Err(format!("unknown archive-repack setting {unknown}")),
        }
    }

    let input_file = std::fs::File::open(input).map_err(|error| format!("{input}: {error}"))?;
    let output_path = std::path::Path::new(output);
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let (_, stats) = crate::archive::repack(
        std::io::BufReader::new(input_file),
        temporary.as_file_mut(),
        frame_target,
        compression,
    )
    .map_err(|error| error.to_string())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("{}: {error}", temporary.path().display()))?;
    temporary
        .persist(output_path)
        .map_err(|error| format!("{output}: {}", error.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output_path, std::fs::Permissions::from_mode(0o644))
            .map_err(|error| format!("{output}: {error}"))?;
    }
    let output_bytes = std::fs::metadata(output_path)
        .map_err(|error| format!("{output}: {error}"))?
        .len();
    println!(
        "repacked {} records, frames {} -> {}, input compressed payload {} bytes, \
         output file {} bytes",
        stats.records,
        stats.input_frames,
        stats.output_frames,
        stats.input_compressed_bytes,
        output_bytes,
    );
    Ok(())
}

fn cmd_archive_merge(args: &[&str]) -> Result<(), String> {
    let [output, frame_target, level, rest @ ..] = args else {
        return Err(
            "archive-merge wants <output> <frame-bytes> <zstd-level> \
             [--checksum] [--long-distance] [--window-log N] \
             [--target-block-size N] [--scratch-dir PATH] <input>..."
                .into(),
        );
    };
    let frame_target = frame_target
        .parse::<usize>()
        .map_err(|error| format!("frame bytes: {error}"))?;
    if frame_target == 0 {
        return Err("frame bytes must be positive".into());
    }
    let mut compression = crate::archive::CompressionSettings {
        level: level
            .parse::<i32>()
            .map_err(|error| format!("zstd level: {error}"))?,
        ..crate::archive::CompressionSettings::default()
    };
    let mut inputs = Vec::new();
    let mut scratch = std::env::temp_dir();
    let mut options = rest.iter();
    while let Some(value) = options.next() {
        match *value {
            "--checksum" => compression.checksum = true,
            "--long-distance" => compression.long_distance_matching = true,
            "--window-log" => {
                compression.window_log = Some(
                    options
                        .next()
                        .ok_or("--window-log wants an integer")?
                        .parse()
                        .map_err(|error| format!("window log: {error}"))?,
                );
            }
            "--target-block-size" => {
                compression.target_block_size = Some(
                    options
                        .next()
                        .ok_or("--target-block-size wants an integer")?
                        .parse()
                        .map_err(|error| format!("target block size: {error}"))?,
                );
            }
            "--scratch-dir" => {
                scratch = std::path::PathBuf::from(
                    options.next().ok_or("--scratch-dir wants a path")?,
                );
            }
            input if input.starts_with("--") => {
                return Err(format!("unknown archive-merge setting {input}"));
            }
            input => inputs.push(std::path::PathBuf::from(input)),
        }
    }
    if inputs.is_empty() {
        return Err("archive-merge wants at least one input".into());
    }

    let output_path = std::path::Path::new(output);
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let (_, frames, records) = crate::archive::merge_many_archives_with_compression_in(
        &inputs,
        temporary.as_file_mut(),
        frame_target,
        compression,
        scratch,
    )
    .map_err(|error| error.to_string())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("{}: {error}", temporary.path().display()))?;
    temporary
        .persist(output_path)
        .map_err(|error| format!("{output}: {}", error.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output_path, std::fs::Permissions::from_mode(0o644))
            .map_err(|error| format!("{output}: {error}"))?;
    }
    let bytes = std::fs::metadata(output_path)
        .map_err(|error| format!("{output}: {error}"))?
        .len();
    println!("merged {records} records into {frames} frames, {bytes} bytes");
    Ok(())
}

fn cmd_archive_inspect(path: &str) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut reader = crate::archive::ArchiveReader::new(file).map_err(|e| e.to_string())?;
    let mut frames = 0_u64;
    let mut records = 0_u64;
    let mut revisions = 0_u64;
    let mut page_actions = 0_u64;
    let mut user_actions = 0_u64;
    let mut user_states = 0_u64;
    let mut unknown = 0_u64;
    let mut raw_bytes = 0_u64;
    let mut compressed_bytes = 0_u64;
    let mut max_raw_frame = 0_u64;
    let mut max_compressed_frame = 0_u64;
    let mut max_raw_range = None;
    let mut max_compressed_range = None;
    while let Some(mut frame) = reader.next_frame().map_err(|e| e.to_string())? {
        frames += 1;
        let info = frame.info();
        raw_bytes += info.raw_bytes;
        compressed_bytes += info.compressed_bytes;
        if info.raw_bytes > max_raw_frame {
            max_raw_frame = info.raw_bytes;
            max_raw_range = Some((info.first_entity, info.last_entity));
        }
        if info.compressed_bytes > max_compressed_frame {
            max_compressed_frame = info.compressed_bytes;
            max_compressed_range = Some((info.first_entity, info.last_entity));
        }
        while let Some(record) = frame.next_record().map_err(|e| e.to_string())? {
            records += 1;
            match record {
                crate::archive::Record::Revision { .. } => revisions += 1,
                crate::archive::Record::PageAction { .. } => page_actions += 1,
                crate::archive::Record::UserAction { .. } => user_actions += 1,
                crate::archive::Record::UserState { .. } => user_states += 1,
                crate::archive::Record::Unknown { .. } => unknown += 1,
                crate::archive::Record::PageState { .. }
                | crate::archive::Record::Manifest { .. }
                | crate::archive::Record::SiteInfo { .. } => {}
            }
        }
    }
    println!(
        "archive frames {frames}  records {records}  revisions {revisions}  \
         page actions {page_actions}  user actions/states {user_actions}/{user_states}  \
         unknown {unknown}  complete {}",
        reader.is_complete()
    );
    println!(
        "raw/compressed bytes {raw_bytes}/{compressed_bytes}  \
         max raw/compressed frame {max_raw_frame}/{max_compressed_frame}\n\
         max raw range {max_raw_range:?}  max compressed range {max_compressed_range:?}"
    );
    Ok(())
}

#[derive(Default)]
struct ArchiveHistogram {
    revision_buckets: [u64; 16],
    text_buckets: [u64; 16],
}

fn cmd_archive_histogram(path: &str, workers: usize) -> Result<(), String> {
    if workers == 0 {
        return Err("workers must be positive".into());
    }
    let (_, frames, complete) = crate::archive::index_file(path).map_err(|e| e.to_string())?;
    if !complete {
        return Err("archive has no clean completion marker".into());
    }
    if frames.is_empty() {
        return Err("archive contains no frames".into());
    }
    let workers = workers.min(frames.len().max(1));
    let next_frame = std::sync::atomic::AtomicUsize::new(0);
    let partials = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..workers {
            let next_frame = &next_frame;
            let frames = &frames;
            handles.push(scope.spawn(move || {
                let mut stats = ArchiveHistogram::default();
                loop {
                    let index = next_frame.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(location) = frames.get(index) else {
                        break;
                    };
                    let mut current_page = None;
                    let mut current_revisions = 0_u64;
                    crate::archive::visit_frame(path, location, |record| {
                        if record.entity().kind != crate::archive::EntityKind::Page {
                            return Ok(());
                        }
                        let page_id = record.entity().id;
                        if current_page != Some(page_id) {
                            if current_revisions != 0 {
                                stats.revision_buckets
                                    [archive_revision_count_bucket(current_revisions)] += 1;
                            }
                            current_page = Some(page_id);
                            current_revisions = 0;
                        }
                        if let crate::archive::Record::Revision { revision, .. } = record {
                            current_revisions += 1;
                            stats.text_buckets
                                [archive_text_length_bucket(revision.text.len())] += 1;
                        }
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                    if current_revisions != 0 {
                        stats.revision_buckets
                            [archive_revision_count_bucket(current_revisions)] += 1;
                    }
                }
                Ok::<_, String>(stats)
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| "archive histogram worker panicked".to_string())?
            })
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut total = ArchiveHistogram::default();
    for partial in partials {
        for (total, partial) in total
            .revision_buckets
            .iter_mut()
            .zip(partial.revision_buckets)
        {
            *total += partial;
        }
        for (total, partial) in total.text_buckets.iter_mut().zip(partial.text_buckets) {
            *total += partial;
        }
    }
    println!("frames {}  workers {}", frames.len(), workers);
    println!(
        "revisions/page (1,2,3,4,5-7,8-15,...,4096-8191,8192+): {:?}",
        total.revision_buckets
    );
    println!(
        "revision text bytes (0,1-31,32-63,...,131072-262143,262144+): {:?}",
        total.text_buckets
    );
    Ok(())
}

fn archive_revision_count_bucket(revisions: u64) -> usize {
    match revisions {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        5..=7 => 4,
        8..=15 => 5,
        16..=31 => 6,
        32..=63 => 7,
        64..=127 => 8,
        128..=255 => 9,
        256..=511 => 10,
        512..=1023 => 11,
        1024..=2047 => 12,
        2048..=4095 => 13,
        4096..=8191 => 14,
        _ => 15,
    }
}

fn archive_text_length_bucket(len: usize) -> usize {
    const UPPER_BOUNDS: [usize; 16] = [
        0, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535, 131071,
        262143, usize::MAX,
    ];
    UPPER_BOUNDS
        .iter()
        .position(|upper| len <= *upper)
        .expect("last bucket is unbounded")
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
        ["experiment-split-revisions", root] => cmd_experiment_split_revisions(root, 4, 4096),
        ["experiment-split-revisions", root, workers] => workers
            .parse::<usize>()
            .map_err(|e| format!("workers: {e}"))
            .and_then(|workers| cmd_experiment_split_revisions(root, workers, 4096)),
        ["experiment-split-revisions", root, workers, packed_shards] => workers
            .parse::<usize>()
            .map_err(|e| format!("workers: {e}"))
            .and_then(|workers| {
                packed_shards
                    .parse::<usize>()
                    .map_err(|e| format!("packed shards: {e}"))
                    .and_then(|packed_shards| {
                        cmd_experiment_split_revisions(root, workers, packed_shards)
                    })
            }),
        ["experiment-packed-f0", root] => cmd_experiment_packed_f0(root, 4, 256),
        ["experiment-packed-f0", root, workers] => workers
            .parse::<usize>()
            .map_err(|e| format!("workers: {e}"))
            .and_then(|workers| cmd_experiment_packed_f0(root, workers, 256)),
        ["experiment-packed-f0", root, workers, packed_shards] => workers
            .parse::<usize>()
            .map_err(|e| format!("workers: {e}"))
            .and_then(|workers| {
                packed_shards
                    .parse::<usize>()
                    .map_err(|e| format!("packed shards: {e}"))
                    .and_then(|packed_shards| {
                        cmd_experiment_packed_f0(root, workers, packed_shards)
                    })
            }),
        ["metadata-example", root, page] => page
            .parse::<u64>()
            .map_err(|e| format!("page: {e}"))
            .and_then(|page| cmd_metadata_example(root, page)),
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
        #[cfg(feature = "serve")]
        ["archive-serve", path] => cmd_archive_serve(path, "127.0.0.1:8642"),
        #[cfg(feature = "serve")]
        ["archive-serve", path, addr] => cmd_archive_serve(path, addr),
        ["head", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_head(root, p)),
        ["text", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_text(root, p, None)),
        ["text", root, page, asof] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| Ok((p, asof.parse::<i64>().map_err(|e| format!("asof: {e}"))?)))
            .and_then(|(p, ts)| cmd_text(root, p, Some(ts))),
        ["history", root, page] => page.parse().map_err(|e| format!("{e}"))
            .and_then(|p| cmd_history(root, p)),
        ["archive-export", root, output] => cmd_archive_export(root, output),
        ["archive-import", archive, root] => cmd_archive_import(archive, root, max_page_id),
        ["archive-build-direct", dbname, output, scratch] =>
            cmd_archive_build_direct(dbname, output, scratch),
        ["archive-build-update", dbname, base, output, scratch] =>
            cmd_archive_build_update(dbname, base, output, scratch),
        ["archive-fetch-siteinfo", api_url, output] =>
            cmd_archive_fetch_siteinfo(api_url, output),
        ["archive-title-index", archive, output] =>
            cmd_archive_title_index(archive, output),
        ["archive-repack", args @ ..] => cmd_archive_repack(args),
        ["archive-merge", args @ ..] => cmd_archive_merge(args),
        ["archive-inspect", path] => cmd_archive_inspect(path),
        ["archive-histogram", path] => cmd_archive_histogram(
            path,
            std::thread::available_parallelism().map_or(1, usize::from),
        ),
        ["archive-histogram", path, workers] => workers
            .parse::<usize>()
            .map_err(|e| format!("workers: {e}"))
            .and_then(|workers| cmd_archive_histogram(path, workers)),
        _ => Err("usage: wikimak discover <dbname>\n\
                  \x20      wikimak pages <root> [filter]\n\
                  \x20      wikimak fetch <dbname> <root> [--max-page-id N]\n\
                  \x20      wikimak refresh-full <dbname> <root> [--max-page-id N]\n\
                  \x20      wikimak repack-f0 <root> [--max-page-id N]\n\
                  \x20      wikimak experiment-split-revisions <root> [workers] [packed-shards]\n\
                  \x20      wikimak experiment-packed-f0 <root> [workers] [packed-shards]\n\
                  \x20      wikimak reconcile-history <dbname> <root> [--max-page-id N]\n\
                  \x20      wikimak import <dump.xml[.bz2]> <root> [--max-page-id N]\n\
                  \x20      wikimak serve <root> [addr]        (default 127.0.0.1:8642)\n\
                  \x20      wikimak archive-serve <file> [addr] (default 127.0.0.1:8642)\n\
                  \x20      wikimak head|history <root> <page_id>\n\
                  \x20      wikimak text <root> <page_id> [asof-unix-micros]\n\
                  \x20      wikimak archive-export <root> <file>\n\
                  \x20      wikimak archive-import <file> <root>\n\
                  \x20      wikimak archive-build-direct <dbname> <file> <scratch-dir>\n\
                  \x20      wikimak archive-build-update <dbname> <base-dump> <file> <scratch-dir>\n\
                  \x20      wikimak archive-fetch-siteinfo <api-url> <file>\n\
                  \x20      wikimak archive-title-index <archive> <index>\n\
                  \x20      wikimak archive-repack <input> <output> <frame-bytes> <zstd-level> [settings]\n\
                  \x20      wikimak archive-merge <output> <frame-bytes> <zstd-level> [settings] <input>...\n\
                  \x20      wikimak archive-inspect|archive-histogram <file>".into()),
    };
    match r {
        Ok(()) => 0,
        Err(e) => { eprintln!("wikimak: {e}"); 1 }
    }
}
