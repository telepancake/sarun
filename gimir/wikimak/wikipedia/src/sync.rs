//! Discover → fetch → import loop (MIRRORS.md phase 1).
//!
//! `sync` pulls the newest complete dump run for a dbname and imports
//! every part not already recorded in `parts_seen`. Advertised checksums
//! are verified while materializing a compressed part under the instance;
//! only then does bz2 decoder → page stream → depot begin.
//!
//! Ordering of the durability handshake per part:
//!   1. import the whole part (per-page atomic inside);
//!   2. `Instance::flush` — pages durable;
//!   3. `mark_part_seen` — only now is the part skippable.
//!
//! A crash between 2 and 3 re-imports the part; `revisions_seen` dedup
//! makes that a cheap no-op, never a correctness problem.

use std::collections::HashSet;
use std::io::{BufRead, Read};
use std::time::Duration;

use reqwest::blocking::Client;
use regex::Regex;
use rusqlite::params;
use wikimak_mediawiki::{
    discover_incremental_with, discover_with, fetch, new_page_stream, Config, Run,
};

use crate::error::Result;
use crate::instance::{ImportStats, Instance};

fn safe_cache_name(part: &wikimak_mediawiki::Part) -> Result<String> {
    let path = std::path::Path::new(&part.filename);
    if path.file_name().and_then(|name| name.to_str()) != Some(part.filename.as_str()) {
        return Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse("dump part filename contains a path".into()),
        ));
    }
    let digest = part
        .sha256
        .as_deref()
        .or(part.sha1.as_deref())
        .or(part.md5.as_deref())
        .unwrap_or("unchecked");
    Ok(format!("{}.{}", part.filename, digest))
}

/// Materialize and verify the compressed part before the importer sees a
/// byte. The verified file doubles as a crash/retry cache: a process that
/// dies after download does not ask Wikimedia for the same multi-GB object
/// again.
fn verified_part(
    inst: &Instance,
    client: &Client,
    part: &wikimak_mediawiki::Part,
) -> Result<std::path::PathBuf> {
    let dir = inst.root().join(".downloads");
    std::fs::create_dir_all(&dir)?;
    let final_path = dir.join(safe_cache_name(part)?);
    if final_path.exists() {
        return Ok(final_path);
    }
    let partial = final_path.with_extension("partial");
    let mut source = fetch(client, part)?;
    let mut output = std::fs::File::create(&partial)?;
    std::io::copy(&mut source, &mut output)?;
    output.sync_all()?;
    std::fs::rename(&partial, &final_path)?;
    Ok(final_path)
}

/// Counters from one [`sync`] pass.
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    pub parts_total: u64,
    pub parts_fetched: u64,
    pub parts_skipped: u64,
    pub history_parts_fetched: u64,
    pub page_actions: u64,
    pub import: ImportStats,
}

#[derive(Debug, Clone)]
struct HistoryFile {
    partition: String,
    part: wikimak_mediawiki::Part,
}

fn history_listing(client: &Client, url: &str) -> Result<String> {
    let mut delay = Duration::from_secs(1);
    for attempt in 0..4 {
        let response = client.get(url).send().map_err(|error| {
            crate::error::Error::Mediawiki(wikimak_mediawiki::Error::Http(error))
        })?;
        let status = response.status();
        if status.is_success() {
            return response.text().map_err(|error| {
                crate::error::Error::Mediawiki(wikimak_mediawiki::Error::Http(error))
            });
        }
        if attempt < 3 && (status.as_u16() == 429 || status.is_server_error()) {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(delay)
                .min(Duration::from_secs(60));
            std::thread::sleep(retry_after);
            delay = delay.saturating_mul(2);
            continue;
        }
        return Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::HttpStatus {
                status: status.as_u16(),
                url: url.to_string(),
            },
        ));
    }
    unreachable!("history listing retry loop returns")
}

fn discover_history(
    client: &Client,
    cfg: &Config,
    dbname: &str,
) -> Result<(String, Vec<HistoryFile>)> {
    let root = format!("{}/other/mediawiki_history/", cfg.base_url.trim_end_matches('/'));
    let root_html = history_listing(client, &root)?;
    let date_re = Regex::new(r#"href="([0-9]{4}-[0-9]{2})/""#)
        .map_err(|_| crate::error::Error::Corrupt("mediawiki history date regex"))?;
    let snapshot = date_re
        .captures_iter(&root_html)
        .map(|capture| capture[1].to_string())
        .max()
        .ok_or_else(|| crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse("no MediaWiki History snapshot found".into()),
        ))?;
    let dir = format!("{root}{snapshot}/{dbname}/");
    let html = history_listing(client, &dir)?;
    let escaped_dbname = regex::escape(dbname);
    let escaped_snapshot = regex::escape(&snapshot);
    let file_re = Regex::new(&format!(
        r#"href="({escaped_snapshot}\.{escaped_dbname}\.(all-time|[0-9]{{4}}(?:-[0-9]{{2}})?)\.tsv\.bz2)""#
    ))
    .map_err(|_| crate::error::Error::Corrupt("mediawiki history filename regex"))?;
    let mut files: Vec<HistoryFile> = file_re
        .captures_iter(&html)
        .map(|capture| {
            let filename = capture[1].to_string();
            HistoryFile {
                partition: capture[2].to_string(),
                part: wikimak_mediawiki::Part {
                    url: format!("{dir}{filename}"),
                    filename,
                    size_bytes: 0,
                    sha256: None,
                    sha1: None,
                    md5: None,
                },
            }
        })
        .collect();
    files.sort_by(|a, b| a.partition.cmp(&b.partition));
    files.dedup_by(|a, b| a.partition == b.partition);
    if files.is_empty() {
        return Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse(format!(
                "no MediaWiki History files found for {dbname} in {snapshot}"
            )),
        ));
    }
    Ok((snapshot, files))
}

fn unescape_tsv(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn import_history_file(inst: &Instance, file: &HistoryFile, path: &std::path::Path) -> Result<u64> {
    let input = std::fs::File::open(path)?;
    let decoder = wikimak_mediawiki::bz2::new_bz2_reader(
        input,
        wikimak_mediawiki::bz2::Bz2Options { workers: 0 },
    );
    let reader = std::io::BufReader::new(decoder);
    let mut g = inst.inner.lock().expect("instance mutex poisoned");
    let tx = g.conn.transaction()?;
    tx.execute(
        "DELETE FROM page_actions WHERE source_partition = ?1",
        [&file.partition],
    )?;
    tx.execute(
        "DELETE FROM revision_visibility WHERE source_partition = ?1",
        [&file.partition],
    )?;
    let mut insert = tx.prepare(
        "INSERT INTO page_actions(
            source_key,source_partition,event_log_id,event_type,event_timestamp,
            event_comment,actor_id,actor_name,page_id,title_historical,title_current,
            namespace_historical,namespace_current,page_deleted
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
    )?;
    let mut insert_visibility = tx.prepare(
        "INSERT OR REPLACE INTO revision_visibility(
            revision_id,page_id,source_partition,deleted_parts,
            parts_are_suppressed,deleted_by_page_deletion,page_deletion_timestamp
         ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
    )?;
    let mut imported = 0_u64;
    for (line_number, line) in reader.lines().enumerate() {
        let line = line?;
        let fields: Vec<&str> = line.split('\t').collect();
        let (page, revision) = match fields.len() {
            // Two temporary-account fields were added ahead of the page
            // columns in the current dump schema.
            78 => (28, 60),
            76 => (26, 58),
            count => {
                return Err(crate::error::Error::Mediawiki(
                    wikimak_mediawiki::Error::Parse(format!(
                        "{}:{} has unsupported MediaWiki History schema ({count} fields)",
                        file.part.filename,
                        line_number + 1
                    )),
                ));
            }
        };
        if fields[0] != inst.dbname {
            return Err(crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(format!(
                    "{}:{} contains wiki {:?}, expected {:?}",
                    file.part.filename,
                    line_number + 1,
                    fields[0],
                    inst.dbname
                )),
            ));
        }
        if fields[2] != "page" && fields[2] != "revision" {
            continue;
        }
        let page_id = match fields[page].parse::<i64>() {
            Ok(value) if value >= 0 => value,
            _ => continue,
        };
        if fields[2] == "revision" {
            let Ok(revision_id) = fields[revision].parse::<i64>() else {
                continue;
            };
            insert_visibility.execute(params![
                revision_id,
                page_id,
                file.partition,
                fields[revision + 3],
                matches!(fields[revision + 4], "true" | "1") as i64,
                matches!(fields[revision + 10], "true" | "1") as i64,
                fields[revision + 11],
            ])?;
            continue;
        }
        let source_key = format!("{}:{}", file.partition, line_number + 1);
        insert.execute(params![
            source_key,
            file.partition,
            fields[1].parse::<i64>().ok(),
            fields[3],
            fields[4],
            unescape_tsv(fields[5]),
            fields[6].parse::<i64>().ok(),
            unescape_tsv(if fields[9].is_empty() { fields[8] } else { fields[9] }),
            page_id,
            unescape_tsv(fields[page + 1]),
            unescape_tsv(fields[page + 2]),
            fields[page + 3].parse::<i64>().ok(),
            fields[page + 5].parse::<i64>().ok(),
            matches!(fields[page + 8], "true" | "1") as i64,
        ])?;
        imported += 1;
    }
    drop(insert_visibility);
    drop(insert);
    tx.commit()?;
    Ok(imported)
}

fn sync_page_actions(
    inst: &Instance,
    client: &Client,
    cfg: &Config,
    dbname: &str,
    reconcile_all: bool,
    progress: &mut impl FnMut(&str, bool),
) -> Result<(u64, u64)> {
    let (snapshot, files) = discover_history(client, cfg, dbname)?;
    let previous_snapshot = inst.sync_state("history_frontier_snapshot")?;
    if !reconcile_all && previous_snapshot.as_deref() == Some(&snapshot) {
        return Ok((0, 0));
    }
    let previous_frontier = inst.sync_state("history_frontier_partition")?;
    let available: HashSet<&str> = files.iter().map(|file| file.partition.as_str()).collect();
    if !reconcile_all {
        if let Some(frontier) = previous_frontier.as_deref() {
            if !available.contains(frontier) {
                return Err(crate::error::Error::Mediawiki(
                    wikimak_mediawiki::Error::Parse(format!(
                        "MediaWiki History partition scheme changed or frontier {frontier} disappeared; run explicit full refresh"
                    )),
                ));
            }
            if frontier.len() == 7
                && files
                    .last()
                    .is_some_and(|file| file.partition.as_str() <= frontier)
            {
                return Err(crate::error::Error::Mediawiki(
                    wikimak_mediawiki::Error::Parse(format!(
                        "new MediaWiki History snapshot {snapshot} has no complete monthly frontier after {frontier}; retry later"
                    )),
                ));
            }
        }
    }
    let selected: Vec<&HistoryFile> = files
        .iter()
        .filter(|file| {
            reconcile_all
                || previous_snapshot.is_none()
                || previous_frontier
                    .as_deref()
                    .is_some_and(|frontier| file.partition.as_str() >= frontier)
        })
        .collect();
    let mut actions = 0;
    for file in &selected {
        progress(&file.part.filename, true);
        let cached = verified_part(inst, client, &file.part)?;
        match import_history_file(inst, file, &cached) {
            Ok(count) => {
                actions += count;
                let _ = std::fs::remove_file(cached);
            }
            Err(error) => {
                let _ = std::fs::remove_file(cached);
                return Err(error);
            }
        }
    }
    let frontier = files
        .last()
        .expect("history discovery rejects empty file lists")
        .partition
        .as_str();
    inst.set_sync_state("history_frontier_snapshot", &snapshot)?;
    inst.set_sync_state("history_frontier_partition", frontier)?;
    if reconcile_all || previous_snapshot.is_none() {
        inst.set_sync_state("history_reconciled_snapshot", &snapshot)?;
    }
    Ok((selected.len() as u64, actions))
}

fn add_import(into: &mut ImportStats, s: &ImportStats) {
    into.pages += s.pages;
    into.revisions_new += s.revisions_new;
    into.revisions_deduped += s.revisions_deduped;
    into.sha1_ok += s.sha1_ok;
    into.sha1_fudged += s.sha1_fudged;
    into.sha1_mismatch += s.sha1_mismatch;
}

fn ensure_dbname(inst: &Instance, dbname: &str) -> Result<()> {
    match inst.sync_state("wiki_dbname")? {
        Some(existing) if existing == dbname => Ok(()),
        Some(existing) => Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse(format!(
                "mirror root belongs to {existing}, not {dbname}"
            )),
        )),
        None => inst.set_sync_state("wiki_dbname", dbname),
    }
}

/// Discover the newest complete run for `dbname` and import its unseen
/// parts into `inst`. Returns the run picked and the counters.
///
/// `progress` is called before each part with `(filename, fetched|skipped)`;
/// pass `|_, _| ()` for silence.
pub fn sync(
    inst: &Instance,
    client: &Client,
    cfg: &Config,
    dbname: &str,
    mut progress: impl FnMut(&str, bool),
) -> Result<(Run, SyncStats)> {
    ensure_dbname(inst, dbname)?;
    let run = discover_with(client, cfg, dbname)?;
    let stats = import_run(inst, client, &run, &mut progress)?;
    inst.set_sync_state("full_snapshot_date", &run.date.to_string())?;
    let run_date = run.date.to_string();
    let incremental = inst
        .sync_state("incremental_date")?
        .filter(|date| date.as_str() > run_date.as_str())
        .unwrap_or(run_date);
    inst.set_sync_state("incremental_date", &incremental)?;
    Ok((run, stats))
}

/// Explicitly rebuild all page-action and revision-visibility metadata from
/// the latest complete MediaWiki History snapshot. This is deliberately
/// separate from full XML content re-ingest.
pub fn reconcile_history(
    inst: &Instance,
    client: &Client,
    cfg: &Config,
    dbname: &str,
    mut progress: impl FnMut(&str, bool),
) -> Result<SyncStats> {
    ensure_dbname(inst, dbname)?;
    let (history_parts, page_actions) =
        sync_page_actions(inst, client, cfg, dbname, true, &mut progress)?;
    Ok(SyncStats {
        history_parts_fetched: history_parts,
        page_actions,
        ..Default::default()
    })
}

fn import_run(
    inst: &Instance,
    client: &Client,
    run: &Run,
    progress: &mut impl FnMut(&str, bool),
) -> Result<SyncStats> {
    let mut stats = SyncStats {
        parts_total: run.parts.len() as u64,
        ..Default::default()
    };
    for part in &run.parts {
        let digest = part.sha256.as_deref().or(part.sha1.as_deref()).or(part.md5.as_deref());
        if inst.part_seen_with_digest(&part.filename, digest)? {
            stats.parts_skipped += 1;
            progress(&part.filename, false);
            continue;
        }
        progress(&part.filename, true);
        let cached = verified_part(inst, client, part)?;
        let reader = std::fs::File::open(&cached)?;
        let boxed: Box<dyn Read + Send> = if part.filename.ends_with(".bz2") {
            Box::new(wikimak_mediawiki::bz2::new_bz2_reader(
                reader,
                wikimak_mediawiki::bz2::Bz2Options { workers: 0 },
            ))
        } else {
            Box::new(reader)
        };
        let mut stream = new_page_stream(boxed);
        let s = inst.import(&mut stream)?;
        add_import(&mut stats.import, &s);
        inst.flush()?;
        inst.mark_part_seen(&part.filename, digest)?;
        let _ = std::fs::remove_file(cached);
        stats.parts_fetched += 1;
    }
    // Sync session over: reclaim the churn slack (dead superseded heads)
    // parked in the depot's current write files. Once per sync, not per
    // part — mid-session the slack is what keeps prepends cheap.
    if stats.parts_fetched > 0 {
        inst.collect()?;
    }
    Ok(stats)
}

/// Bootstrap once from a full-history snapshot, then consume only Wikimedia's
/// daily adds/changes runs. An existing pre-watermark store is not silently
/// refreshed from another full snapshot: the operator must request that
/// expensive reconciliation explicitly.
pub fn maintain(
    inst: &Instance,
    client: &Client,
    cfg: &Config,
    dbname: &str,
    mut progress: impl FnMut(&str, bool),
) -> Result<SyncStats> {
    ensure_dbname(inst, dbname)?;
    let mut total = SyncStats::default();
    let baseline = match inst.sync_state("full_snapshot_date")? {
        Some(value) => value,
        None if inst.has_seen_parts()? => {
            return Err(crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(
                    "existing mirror has no daily-update watermark; run explicit full refresh"
                        .into(),
                ),
            ));
        }
        None => {
            let (run, stats) = sync(inst, client, cfg, dbname, &mut progress)?;
            if stats.parts_fetched == 0 && stats.parts_skipped == 0 {
                return Ok(stats);
            }
            total.parts_total += stats.parts_total;
            total.parts_fetched += stats.parts_fetched;
            total.parts_skipped += stats.parts_skipped;
            add_import(&mut total.import, &stats.import);
            run.date.to_string()
        }
    };
    let after_text = inst
        .sync_state("incremental_date")?
        .unwrap_or(baseline);
    let after = chrono::NaiveDate::parse_from_str(&after_text, "%Y-%m-%d")
        .map_err(|_| crate::error::Error::Corrupt("incremental_date sync state"))?;
    let runs = discover_incremental_with(client, cfg, dbname, Some(after))?;
    if let Some(first) = runs.first() {
        if first.date > after.succ_opt().unwrap_or(after) {
            return Err(crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(format!(
                    "daily dump gap after {after}; explicit full refresh required"
                )),
            ));
        }
    }
    for run in runs {
        let stats = import_run(inst, client, &run, &mut progress)?;
        total.parts_total += stats.parts_total;
        total.parts_fetched += stats.parts_fetched;
        total.parts_skipped += stats.parts_skipped;
        add_import(&mut total.import, &stats.import);
        inst.set_sync_state("incremental_date", &run.date.to_string())?;
    }
    let (history_parts, page_actions) =
        sync_page_actions(inst, client, cfg, dbname, false, &mut progress)?;
    total.history_parts_fetched = history_parts;
    total.page_actions = page_actions;
    Ok(total)
}
