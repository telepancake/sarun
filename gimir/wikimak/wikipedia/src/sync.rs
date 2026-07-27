//! Discover → fetch → import loop (MIRRORS.md phase 1).
//!
//! `sync` pulls the newest complete dump run for a dbname and imports
//! every part not already recorded in `parts_seen`, streaming HTTP →
//! checksum reader → bz2 decoder → page stream → depot. A checksum or
//! tail failure leaves complete pages recovered but the part unwatermarked.
//!
//! Ordering of the durability handshake per part:
//!   1. import complete pages atomically as the part streams;
//!   2. reach clean EOF with the advertised checksum;
//!   3. `Instance::flush` — pages durable;
//!   4. `mark_part_seen` — only now is the part skippable.
//!
//! A crash between 2 and 3 re-imports the part; `revisions_seen` dedup
//! makes that a cheap no-op, never a correctness problem.

use std::collections::VecDeque;
use std::io::{BufRead, Read};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::blocking::Client;
use regex::Regex;
use rusqlite::params;
use wikimak_mediawiki::{
    discover_incremental_with, discover_with, fetch, new_page_stream, Config, Run,
};

use crate::error::Result;
use crate::instance::{ImportStats, Instance};

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
        let response = match client.get(url).send() {
            Ok(response) => response,
            Err(error) if attempt < 3 && (error.is_connect() || error.is_timeout()) => {
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
                continue;
            }
            Err(error) => {
                return Err(crate::error::Error::Mediawiki(
                    wikimak_mediawiki::Error::Http(error),
                ));
            }
        };
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
    let first_partition = files[0].partition.as_str();
    if first_partition == "all-time" {
        if files.len() != 1 {
            return Err(crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(format!(
                    "mixed all-time and partitioned MediaWiki History files for {dbname} in {snapshot}"
                )),
            ));
        }
    } else {
        let width = first_partition.len();
        if !matches!(width, 4 | 7)
            || files.iter().any(|file| file.partition.len() != width)
            || files.last().is_some_and(|file| {
                if width == 7 {
                    file.partition.as_str() > snapshot.as_str()
                } else {
                    file.partition.as_str() > &snapshot[..4]
                }
            })
        {
            return Err(crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(format!(
                    "invalid or mixed MediaWiki History partition scheme for {dbname} in {snapshot}"
                )),
            ));
        }
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

fn history_bool(file: &HistoryFile, line: usize, field: &str, value: &str) -> Result<bool> {
    match value {
        "" | "false" | "0" => Ok(false),
        "true" | "1" => Ok(true),
        _ => Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse(format!(
                "{}:{line} has invalid {field} boolean {value:?}",
                file.part.filename
            )),
        )),
    }
}

fn import_history_file<R: Read + Send + 'static>(
    tx: &rusqlite::Transaction<'_>,
    file: &HistoryFile,
    expected_dbname: &str,
    input: R,
) -> Result<u64> {
    let decoder = wikimak_mediawiki::bz2::new_bz2_reader(
        input,
        wikimak_mediawiki::bz2::Bz2Options { workers: 0 },
    );
    let reader = std::io::BufReader::new(decoder);
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
        if fields[0] != expected_dbname {
            return Err(crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(format!(
                    "{}:{} contains wiki {:?}, expected {:?}",
                    file.part.filename,
                    line_number + 1,
                    fields[0],
                    expected_dbname
                )),
            ));
        }
        if fields[2] != "page" && fields[2] != "revision" {
            continue;
        }
        let page_id = fields[page].parse::<i64>().ok().filter(|value| *value > 0)
            .ok_or_else(|| crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(format!(
                    "{}:{} has invalid page id {:?}",
                    file.part.filename,
                    line_number + 1,
                    fields[page]
                )),
            ))?;
        if fields[2] == "revision" {
            let revision_id = fields[revision].parse::<i64>().ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| crate::error::Error::Mediawiki(
                    wikimak_mediawiki::Error::Parse(format!(
                        "{}:{} has invalid revision id {:?}",
                        file.part.filename,
                        line_number + 1,
                        fields[revision]
                    )),
                ))?;
            let suppressed = history_bool(
                file,
                line_number + 1,
                "revision_parts_are_suppressed",
                fields[revision + 4],
            )?;
            let page_deleted = history_bool(
                file,
                line_number + 1,
                "revision_deleted_by_page_deletion",
                fields[revision + 10],
            )?;
            if !fields[revision + 3].is_empty()
                || suppressed
                || page_deleted
                || !fields[revision + 11].is_empty()
            {
                insert_visibility.execute(params![
                    revision_id,
                    page_id,
                    file.partition,
                    fields[revision + 3],
                    suppressed as i64,
                    page_deleted as i64,
                    fields[revision + 11],
                ])?;
            }
            continue;
        }
        let page_deleted = history_bool(
            file,
            line_number + 1,
            "page_is_deleted",
            fields[page + 8],
        )?;
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
            page_deleted as i64,
        ])?;
        imported += 1;
    }
    drop(insert_visibility);
    drop(insert);
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

    // Every MediaWiki History release is a reconstructed snapshot, not an
    // append-only continuation of the preceding month. Old partitions can
    // change after later moves, renames, and reverts, so a new release must
    // replace the complete derived metadata set.
    let mut g = inst.inner.lock().expect("instance mutex poisoned");
    let tx = g.conn.transaction()?;
    tx.execute("DELETE FROM page_actions", [])?;
    tx.execute("DELETE FROM revision_visibility", [])?;
    let mut actions = 0;
    for file in &files {
        progress(&file.part.filename, true);
        let source = fetch(client, &file.part)?;
        actions += import_history_file(&tx, file, dbname, source)?;
    }
    let frontier = files
        .last()
        .expect("history discovery rejects empty file lists")
        .partition
        .as_str();
    for (key, value) in [
        ("history_frontier_snapshot", snapshot.as_str()),
        ("history_frontier_partition", frontier),
        ("history_reconciled_snapshot", snapshot.as_str()),
    ] {
        tx.execute(
            "INSERT OR REPLACE INTO sync_state(key, value) VALUES(?1, ?2)",
            [key, value],
        )?;
    }
    tx.commit()?;
    Ok((files.len() as u64, actions))
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
    let mut pending = Vec::new();
    for part in &run.parts {
        let digest = part.sha256.as_deref().or(part.sha1.as_deref()).or(part.md5.as_deref());
        if inst.part_seen_with_digest(&part.filename, digest)? {
            stats.parts_skipped += 1;
            progress(&part.filename, false);
            continue;
        }
        progress(&part.filename, true);
        pending.push(part.clone());
    }
    if pending.is_empty() {
        return Ok(stats);
    }

    // A page may be split into revision slices (`-pXrArB`). Such slices form
    // one sequential group; groups with disjoint page-id ranges are safe to
    // fetch, decode, and parse concurrently. The Instance's per-page mutex is
    // the narrow final writer boundary.
    let groups = part_groups(pending.clone());
    let cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let outer_workers = groups.len().min(cores).min(4).max(1);
    let bz2_workers = (cores / outer_workers).max(1);
    let queue = Arc::new(Mutex::new(VecDeque::from(groups)));
    let imported = Arc::new(Mutex::new(ImportStats::default()));
    let failure = Arc::new(Mutex::new(None));

    std::thread::scope(|scope| {
        for _ in 0..outer_workers {
            let queue = Arc::clone(&queue);
            let imported = Arc::clone(&imported);
            let failure = Arc::clone(&failure);
            scope.spawn(move || {
                loop {
                    if failure.lock().expect("failure mutex poisoned").is_some() {
                        return;
                    }
                    let Some(group) = queue.lock().expect("part queue poisoned").pop_front() else {
                        return;
                    };
                    let mut local = ImportStats::default();
                    for part in group {
                        if failure.lock().expect("failure mutex poisoned").is_some() {
                            return;
                        }
                        match import_part(inst, client, &part, bz2_workers) {
                            Ok(part_stats) => add_import(&mut local, &part_stats),
                            Err(error) => {
                                *failure.lock().expect("failure mutex poisoned") = Some(error);
                                return;
                            }
                        }
                    }
                    add_import(
                        &mut imported.lock().expect("import stats mutex poisoned"),
                        &local,
                    );
                }
            });
        }
    });

    if let Some(error) = failure.lock().expect("failure mutex poisoned").take() {
        // Complete pages from every active pipeline are independent archival
        // records. Make all successfully parsed prefixes durable, but leave
        // every pending part unwatermarked so a retry repairs/deduplicates.
        inst.flush_salvage()?;
        return Err(error);
    }

    // One durability fence covers the concurrent batch. Only after it lands
    // do any part watermarks become skippable.
    inst.flush()?;
    for part in &pending {
        let digest = part
            .sha256
            .as_deref()
            .or(part.sha1.as_deref())
            .or(part.md5.as_deref());
        inst.mark_part_seen(&part.filename, digest)?;
    }
    stats.parts_fetched = pending.len() as u64;
    stats.import = std::mem::take(&mut *imported.lock().expect("import stats mutex poisoned"));
    inst.collect()?;
    Ok(stats)
}

fn import_part(
    inst: &Instance,
    client: &Client,
    part: &wikimak_mediawiki::Part,
    bz2_workers: usize,
) -> Result<ImportStats> {
    let reader = fetch(client, part)?;
    let boxed: Box<dyn Read + Send> = if part.filename.ends_with(".bz2") {
        Box::new(wikimak_mediawiki::bz2::new_bz2_reader(
            reader,
            wikimak_mediawiki::bz2::Bz2Options {
                workers: bz2_workers,
            },
        ))
    } else {
        Box::new(reader)
    };
    let mut stream = new_page_stream(boxed);
    inst.import(&mut stream)
}

fn part_groups(parts: Vec<wikimak_mediawiki::Part>) -> Vec<Vec<wikimak_mediawiki::Part>> {
    let spans: Option<Vec<_>> = parts
        .iter()
        .map(|part| part_page_span(&part.filename))
        .collect();
    let Some(spans) = spans else {
        // An unparseable filename might overlap any other part. Preserve the
        // source order rather than guessing that parallel import is safe.
        return vec![parts];
    };
    let mut groups: Vec<Vec<wikimak_mediawiki::Part>> = Vec::new();
    let mut group_end = 0u64;
    for (part, (start, end)) in parts.into_iter().zip(spans) {
        if groups.is_empty() || start > group_end {
            groups.push(vec![part]);
            group_end = end;
        } else {
            groups.last_mut().expect("group exists").push(part);
            group_end = group_end.max(end);
        }
    }
    groups
}

fn part_page_span(filename: &str) -> Option<(u64, u64)> {
    let marker = filename.rfind("-p")?;
    let tail = &filename[marker + 2..];
    let start_len = tail.bytes().take_while(u8::is_ascii_digit).count();
    if start_len == 0 {
        return None;
    }
    let start = tail[..start_len].parse().ok()?;
    match tail.as_bytes().get(start_len).copied()? {
        b'r' => Some((start, start)),
        b'p' => {
            let rest = &tail[start_len + 1..];
            let end_len = rest.bytes().take_while(u8::is_ascii_digit).count();
            let end = rest.get(..end_len)?.parse().ok()?;
            (start <= end).then_some((start, end))
        }
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn part(filename: &str) -> wikimak_mediawiki::Part {
        wikimak_mediawiki::Part {
            url: String::new(),
            filename: filename.into(),
            size_bytes: 0,
            sha256: None,
            sha1: None,
            md5: None,
        }
    }

    #[test]
    fn disjoint_page_ranges_form_parallel_groups() {
        let groups = part_groups(vec![
            part("lvwiki-2026-07-01-p1p35441.xml.bz2"),
            part("lvwiki-2026-07-01-p35442p146417.xml.bz2"),
            part("lvwiki-2026-07-01-p146418p415563.xml.bz2"),
        ]);
        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|group| group.len() == 1));
    }

    #[test]
    fn same_page_revision_slices_remain_sequential() {
        let groups = part_groups(vec![
            part("enwiki-2026-07-01-p1p99.xml.bz2"),
            part("enwiki-2026-07-01-p100r1r999.xml.bz2"),
            part("enwiki-2026-07-01-p100r1000r1999.xml.bz2"),
            part("enwiki-2026-07-01-p101p200.xml.bz2"),
        ]);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[1].len(), 2);
    }

    #[test]
    fn overlapping_or_unknown_ranges_disable_unsafe_parallelism() {
        let overlapping = part_groups(vec![
            part("x-p1p100.xml.bz2"),
            part("x-p50p150.xml.bz2"),
            part("x-p151p200.xml.bz2"),
        ]);
        assert_eq!(overlapping.len(), 2);
        assert_eq!(overlapping[0].len(), 2);

        let unknown = part_groups(vec![part("x-p1p100.xml.bz2"), part("mystery.xml.bz2")]);
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].len(), 2);
    }
}
