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
//! A crash between 2 and 3 re-imports the part. The depot chain is the
//! revision ledger: identical ids deduplicate without a write, newer
//! prefixes prepend, and interleaved additions use an atomic replacement.

use std::collections::{HashMap, HashSet, VecDeque};
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
        let (body, status) = wikimak_mediawiki::discover::get_small(client, url)?;
        if status.is_success() {
            return String::from_utf8(body).map_err(|error| {
                crate::error::Error::Mediawiki(wikimak_mediawiki::Error::Parse(format!(
                    "history listing is not utf-8: {error}"
                )))
            });
        }
        if attempt < 3 && (status.as_u16() == 429 || status.is_server_error()) {
            std::thread::sleep(delay.min(Duration::from_secs(60)));
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
            || files.last().is_none_or(|file| {
                file.partition
                    != if width == 7 { snapshot.as_str() } else { &snapshot[..4] }
            })
            || files.windows(2).any(|pair| {
                partition_successor(&pair[0].partition).as_deref()
                    != Some(pair[1].partition.as_str())
            })
        {
            return Err(crate::error::Error::Mediawiki(
                wikimak_mediawiki::Error::Parse(format!(
                    "incomplete, invalid, or mixed MediaWiki History partition scheme for {dbname} in {snapshot}"
                )),
            ));
        }
    }
    Ok((snapshot, files))
}

fn partition_successor(partition: &str) -> Option<String> {
    match partition.len() {
        4 => partition.parse::<u32>().ok()?.checked_add(1).map(|year| format!("{year:04}")),
        7 if partition.as_bytes().get(4) == Some(&b'-') => {
            let year = partition[..4].parse::<u32>().ok()?;
            let month = partition[5..].parse::<u32>().ok()?;
            match month {
                1..=11 => Some(format!("{year:04}-{:02}", month + 1)),
                12 => year.checked_add(1).map(|year| format!("{year:04}-01")),
                _ => None,
            }
        }
        _ => None,
    }
}

fn history_parse_error(file: &HistoryFile, line: usize, message: String) -> crate::error::Error {
    crate::error::Error::Mediawiki(wikimak_mediawiki::Error::Parse(format!(
        "{}:{line} {message}",
        file.part.filename
    )))
}

fn unescape_tsv(file: &HistoryFile, line: usize, field: &str, value: &str) -> Result<String> {
    let input = value.as_bytes();
    let mut out = Vec::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        let byte = input[cursor];
        cursor += 1;
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let Some(&escape) = input.get(cursor) else {
            // A terminal backslash is legal title content. MWH demonstrably
            // does not escape literal backslashes consistently, so absence
            // of an escape code means the byte is literal.
            out.push(b'\\');
            break;
        };
        cursor += 1;
        match escape {
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'\\' => out.push(b'\\'),
            b'x' => {
                let Some(hex) = input.get(cursor..cursor + 2) else {
                    return Err(history_parse_error(
                        file,
                        line,
                        format!("has incomplete \\\\xHH escape in {field}"),
                    ));
                };
                let Some(high) = hex_nibble(hex[0]) else {
                    return Err(history_parse_error(
                        file,
                        line,
                        format!("has invalid \\\\xHH escape in {field}"),
                    ));
                };
                let Some(low) = hex_nibble(hex[1]) else {
                    return Err(history_parse_error(
                        file,
                        line,
                        format!("has invalid \\\\xHH escape in {field}"),
                    ));
                };
                out.push((high << 4) | low);
                cursor += 2;
            }
            other => {
                // Backslash is legal title content. MWH uses it as an
                // introducer only for the structural escapes above; an
                // unknown pair is therefore literal, not malformed.
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    String::from_utf8(out).map_err(|_| {
        history_parse_error(
            file,
            line,
            format!("has invalid UTF-8 after unescaping {field}"),
        )
    })
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn optional_i64(
    file: &HistoryFile,
    line: usize,
    field: &str,
    value: &str,
) -> Result<Option<i64>> {
    if value.is_empty() {
        return Ok(None);
    }
    value.parse::<i64>().map(Some).map_err(|_| {
        history_parse_error(file, line, format!("has invalid {field} integer {value:?}"))
    })
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

fn history_unix_seconds(file: &HistoryFile, line: usize, value: &str) -> Result<u32> {
    let seconds = parse_history_timestamp(value)
        .ok_or_else(|| history_parse_error(file, line, format!("has invalid timestamp {value:?}")))?;
    seconds.try_into().map_err(|_| {
        history_parse_error(
            file,
            line,
            format!("has timestamp outside the title-history range {value:?}"),
        )
    })
}

fn parse_history_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .map(|timestamp| timestamp.and_utc().timestamp())
        })
        .ok()
}

fn import_history_file<R: Read + Send + 'static>(
    tx: &rusqlite::Transaction<'_>,
    file: &HistoryFile,
    expected_dbname: &str,
    input: R,
    source_ordinal: &mut u64,
) -> Result<u64> {
    let decoder = wikimak_mediawiki::bz2::new_bz2_reader(
        input,
        wikimak_mediawiki::bz2::Bz2Options { workers: 0 },
    );
    let reader = std::io::BufReader::new(decoder);
    let mut insert = tx.prepare(
        "INSERT INTO page_actions(
            source_key,source_partition,event_log_id,source_ordinal,event_type,event_timestamp,
            event_comment,actor_id,actor_name,page_id,title_historical,title_current,
            namespace_historical,namespace_current,page_deleted
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
    )?;
    let mut insert_visibility = tx.prepare(
        "INSERT INTO revision_visibility(
            revision_id,page_id,source_partition,deleted_parts,
            parts_are_suppressed,deleted_by_page_deletion,page_deletion_timestamp
         ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
    )?;
    let mut imported = 0_u64;
    for (line_number, line) in reader.lines().enumerate() {
        let ordinal = *source_ordinal;
        *source_ordinal = source_ordinal.checked_add(1).ok_or(
            crate::error::Error::Corrupt("MediaWiki History source ordinal overflow"),
        )?;
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
        match fields[2] {
            "page" | "revision" => {}
            "user" => continue,
            other => {
                return Err(history_parse_error(
                    file,
                    line_number + 1,
                    format!("has unsupported event entity {other:?}"),
                ));
            }
        }
        if fields[3].is_empty() || fields[4].is_empty() {
            return Err(history_parse_error(
                file,
                line_number + 1,
                "has an empty event type or timestamp".into(),
            ));
        }
        let event_at = history_unix_seconds(file, line_number + 1, fields[4])?;
        let page_id = if fields[page].is_empty() || fields[page] == "0" {
            None
        } else {
            let value = fields[page]
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    crate::error::Error::Mediawiki(wikimak_mediawiki::Error::Parse(format!(
                        "{}:{} has invalid page id {:?}",
                        file.part.filename,
                        line_number + 1,
                        fields[page]
                    )))
                })?;
            u32::try_from(value).map_err(|_| history_parse_error(
                file,
                line_number + 1,
                format!("has page id outside the flat-title range {value}"),
            ))?;
            Some(value)
        };
        if fields[2] == "revision" {
            if fields[3] != "create" {
                return Err(history_parse_error(
                    file,
                    line_number + 1,
                    format!("has unsupported revision event type {:?}", fields[3]),
                ));
            }
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
            if let (Some(page_id), Some(namespace)) = (
                page_id,
                optional_i64(
                    file,
                    line_number + 1,
                    "page_namespace_historical",
                    fields[page + 3],
                )?,
            ) {
                let title = unescape_tsv(
                    file,
                    line_number + 1,
                    "page_title_historical",
                    fields[page + 1],
                )?;
                if !title.is_empty() {
                    tx.execute(
                        "INSERT INTO temp.title_history_observations(
                            page_id,title,namespace_historical,event_s,source_ordinal
                         ) VALUES(?1,?2,?3,?4,?5)
                         ON CONFLICT(page_id,title,namespace_historical) DO UPDATE SET
                           event_s=excluded.event_s,
                           source_ordinal=excluded.source_ordinal
                         WHERE (excluded.event_s,excluded.source_ordinal)
                             < (event_s,source_ordinal)",
                        params![page_id, title, namespace, event_at, ordinal],
                    )?;
                }
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
        let event_log_id = optional_i64(
            file,
            line_number + 1,
            "event_log_id",
            fields[1],
        )?;
        let actor_id = optional_i64(file, line_number + 1, "event_user_id", fields[6])?;
        let namespace_historical = optional_i64(
            file,
            line_number + 1,
            "page_namespace_historical",
            fields[page + 3],
        )?;
        let namespace_current = optional_i64(
            file,
            line_number + 1,
            "page_namespace_current",
            fields[page + 5],
        )?;
        insert.execute(params![
            source_key,
            file.partition,
            event_log_id,
            ordinal,
            fields[3],
            fields[4],
            unescape_tsv(file, line_number + 1, "event_comment", fields[5])?,
            actor_id,
            unescape_tsv(
                file,
                line_number + 1,
                "event_user_text",
                if fields[9].is_empty() { fields[8] } else { fields[9] },
            )?,
            page_id,
            unescape_tsv(file, line_number + 1, "page_title_historical", fields[page + 1])?,
            unescape_tsv(file, line_number + 1, "page_title_current", fields[page + 2])?,
            namespace_historical,
            namespace_current,
            page_deleted as i64,
        ])?;
        imported += 1;
    }
    drop(insert_visibility);
    drop(insert);
    Ok(imported)
}

fn history_namespace_names(
    tx: &rusqlite::Transaction<'_>,
) -> Result<HashMap<i64, (String, String)>> {
    let payload: Option<Vec<u8>> = tx
        .query_row(
            "SELECT json FROM siteinfo_snapshots ORDER BY captured_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some(payload) = payload else {
        return Ok(HashMap::new());
    };
    let value: serde_json::Value = serde_json::from_slice(&payload).map_err(|_| {
        crate::error::Error::Mediawiki(wikimak_mediawiki::Error::Parse(
            "stored siteinfo snapshot is invalid JSON".into(),
        ))
    })?;
    let mut names = HashMap::new();
    for namespace in value
        .get("namespaces")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = namespace.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let localized = namespace
            .get("localized")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let canonical = namespace
            .get("canonical")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        names.insert(id, (localized, canonical));
    }
    Ok(names)
}

fn qualify_history_title(
    namespace: i64,
    title: &[u8],
    names: &HashMap<i64, (String, String)>,
) -> Result<Vec<u8>> {
    let normalized = crate::titles::normalize_title(title);
    if namespace == 0 {
        return Ok(normalized);
    }
    let (localized, canonical) = names.get(&namespace).ok_or_else(|| {
        crate::error::Error::Mediawiki(wikimak_mediawiki::Error::Parse(format!(
            "MediaWiki History names unknown namespace {namespace}"
        )))
    })?;
    let raw = String::from_utf8_lossy(&normalized);
    if let Some((prefix, _)) = raw.split_once(':') {
        if prefix.eq_ignore_ascii_case(localized) || prefix.eq_ignore_ascii_case(canonical) {
            return Ok(normalized);
        }
    }
    if localized.is_empty() {
        return Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse(format!(
                "siteinfo has no localized name for namespace {namespace}"
            )),
        ));
    }
    Ok(crate::titles::normalize_title(
        format!("{localized}:{raw}").as_bytes(),
    ))
}

fn history_event_kind(value: &str) -> Result<crate::title_history::EventKind> {
    use crate::title_history::EventKind;
    match value {
        "create" => Ok(EventKind::Create),
        "create-page" => Ok(EventKind::CreatePage),
        "move" => Ok(EventKind::Move),
        "delete" => Ok(EventKind::Delete),
        "restore" => Ok(EventKind::Restore),
        "merge" => Ok(EventKind::Merge),
        other => Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse(format!(
                "unsupported MediaWiki History page event type {other:?}"
            )),
        )),
    }
}

fn next_title_slot_generation(root: &std::path::Path, selected: u32) -> Result<u32> {
    let mut generation = selected.checked_add(1).ok_or(
        crate::error::Error::Corrupt("title-slot generation overflow"),
    )?;
    loop {
        if !root.join(format!("title-slots.{generation}")).exists()
            && !root.join(format!("page-titles.{generation}")).exists()
        {
            return Ok(generation);
        }
        generation = generation.checked_add(1).ok_or(
            crate::error::Error::Corrupt("title-slot generation overflow"),
        )?;
    }
}

fn rebuild_title_history(
    root: &std::path::Path,
    titles: &strpool::Pool,
    tx: &rusqlite::Transaction<'_>,
) -> Result<u32> {
    use crate::title_history::{Event, EventKind, Reconstruction, TitleKey};

    let namespace_names = history_namespace_names(tx)?;
    let mut events = Vec::new();
    {
        let mut statement = tx.prepare(
            "SELECT page_id,event_type,event_timestamp,source_ordinal,
                    title_historical,namespace_historical
             FROM page_actions",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get_ref(4)?.as_bytes()?.to_vec(),
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;
        for row in rows {
            let (page_id, kind, timestamp, ordinal, title, namespace) = row?;
            let at = parse_history_timestamp(&timestamp)
                .ok_or_else(|| crate::error::Error::Mediawiki(
                    wikimak_mediawiki::Error::Parse(format!(
                        "stored MediaWiki History timestamp is invalid: {timestamp:?}"
                    )),
                ))?
                .try_into()
                .map_err(|_| crate::error::Error::Corrupt("title timestamp outside u32"))?;
            let historical = match (namespace, title.is_empty()) {
                (Some(ns), false) => Some(TitleKey {
                    ns: 0,
                    title: qualify_history_title(ns, &title, &namespace_names)?,
                }),
                _ => None,
            };
            events.push(Event {
                page_id: page_id
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| crate::error::Error::Corrupt(
                        "stored MediaWiki History page id outside u32",
                    ))?,
                kind: history_event_kind(&kind)?,
                at,
                source_ordinal: ordinal.try_into().map_err(|_| {
                    crate::error::Error::Corrupt("negative MediaWiki History source ordinal")
                })?,
                historical,
            });
        }
    }
    {
        let mut statement = tx.prepare(
            "SELECT page_id,title,namespace_historical,event_s,source_ordinal
             FROM temp.title_history_observations",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get_ref(1)?.as_bytes()?.to_vec(),
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (page_id, title, namespace, at, ordinal) = row?;
            events.push(Event {
                page_id: Some(page_id.try_into().map_err(|_| {
                    crate::error::Error::Corrupt("MediaWiki History page id outside u32")
                })?),
                kind: EventKind::RevisionInferred,
                at: at.try_into().map_err(|_| {
                    crate::error::Error::Corrupt("title timestamp outside u32")
                })?,
                source_ordinal: ordinal.try_into().map_err(|_| {
                    crate::error::Error::Corrupt("negative MediaWiki History source ordinal")
                })?,
                historical: Some(TitleKey {
                    ns: 0,
                    title: qualify_history_title(namespace, &title, &namespace_names)?,
                }),
            });
        }
    }

    let reconstructed = Reconstruction::from_events(events);
    let shard_count = titles.shard_count();
    let mut touched_shards = HashSet::new();
    let mut snapshots = Vec::with_capacity(reconstructed.by_title.len());
    for (title, mut intervals) in reconstructed.by_title {
        intervals.sort_by_key(|interval| interval.start);
        let mut ids = crate::titles::lookup_ids(titles, shard_count, &title.title)?;
        let title_id = if let Some(id) = ids.pop() {
            id
        } else {
            let shard = crate::titles::shard_for(&title.title, shard_count);
            touched_shards.insert(shard);
            titles.append(shard, &title.title)?
        };
        let current = intervals
            .last()
            .filter(|interval| interval.end.is_none())
            .cloned();
        let current_since = current
            .as_ref()
            .map_or_else(|| intervals.last().and_then(|i| i.end).unwrap_or(0), |i| i.start);
        let older = intervals
            .iter()
            .filter_map(|interval| {
                interval
                    .end
                    .map(|end| (interval.start as i64, end as i64, interval.page_id as u64))
            })
            .collect::<Vec<_>>();
        snapshots.push((
            title_id,
            current.map_or(0, |interval| interval.page_id as u64),
            current_since as i64,
            older,
        ));
    }
    for shard in touched_shards {
        titles.flush(shard)?;
    }

    let selected = crate::title_slots::TitleSlotGenerations::selected(tx)?;
    let generation = next_title_slot_generation(root, selected)?;
    let mut builder =
        crate::title_slots::TitleSlotGenerations::prepare_snapshot(root, generation, tx)?;
    snapshots.sort_by_key(|snapshot| snapshot.0);
    for (title_id, page_id, since, older) in snapshots {
        builder.push_title(title_id, page_id, since, &older)?;
    }
    builder.finish()?.commit()?;
    crate::title_slots::TitleSlotGenerations::select(tx, generation)?;
    Ok(generation)
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
    if previous_snapshot.as_deref().is_some_and(|previous| previous > snapshot.as_str()) {
        return Err(crate::error::Error::Mediawiki(
            wikimak_mediawiki::Error::Parse(format!(
                "MediaWiki History snapshot regressed from {} to {snapshot}",
                previous_snapshot.as_deref().unwrap_or_default()
            )),
        ));
    }
    if !reconcile_all && previous_snapshot.as_deref() == Some(&snapshot) {
        return Ok((0, 0));
    }

    // Every MediaWiki History release is a reconstructed snapshot, not an
    // append-only continuation of the preceding month. Old partitions can
    // change after later moves, renames, and reverts, so a new release must
    // replace the complete derived metadata set.
    let mut g = inst.inner.lock().expect("instance mutex poisoned");
    let crate::instance::InstanceInner { conn, titles, .. } = &mut *g;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM page_actions", [])?;
    tx.execute("DELETE FROM revision_visibility", [])?;
    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.title_history_observations;
         CREATE TEMP TABLE title_history_observations (
           page_id INTEGER NOT NULL,
           title BLOB NOT NULL,
           namespace_historical INTEGER NOT NULL,
           event_s INTEGER NOT NULL,
           source_ordinal INTEGER NOT NULL,
           PRIMARY KEY(page_id,title,namespace_historical)
         ) WITHOUT ROWID;",
    )?;
    let mut actions = 0;
    let mut source_ordinal = 0;
    for file in &files {
        progress(&file.part.filename, true);
        let source = fetch(client, &file.part)?;
        actions += import_history_file(&tx, file, dbname, source, &mut source_ordinal)?;
    }
    let title_generation = rebuild_title_history(&inst.root, titles, &tx)?;
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
    g.title_slots =
        crate::title_slots::TitleSlotGenerations::open_selected(&inst.root, &g.conn)?;
    g.page_titles =
        crate::title_slots::TitleSlotGenerations::open_selected_page_titles(&inst.root, &g.conn)?;
    crate::title_slots::TitleSlotGenerations::collect_unselected(
        &inst.root,
        title_generation,
    )?;
    debug_assert_eq!(
        crate::title_slots::TitleSlotGenerations::selected(&g.conn)?,
        title_generation
    );
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
    let initial_seed = inst.sync_state("full_snapshot_date")?.is_none()
        && !inst.has_seen_parts()?;
    let run = discover_with(client, cfg, dbname)?;
    let stats = import_run(inst, client, &run, &mut progress)?;
    if initial_seed {
        inst.finalize_seed_revision_dictionary()?;
    }
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
    // Wikimedia may rate-limit a fourth simultaneous large transfer. Three
    // still overlaps network and decompression without provoking that limit.
    let outer_workers = groups.len().min(cores).min(3).max(1);
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

    fn history_file() -> HistoryFile {
        HistoryFile {
            partition: "all-time".into(),
            part: part("testwiki.tsv.bz2"),
        }
    }

    #[test]
    fn malformed_history_scalars_are_not_coerced_to_null_or_text() {
        let file = history_file();
        let integer = optional_i64(&file, 7, "event_user_id", "not-a-number")
            .unwrap_err()
            .to_string();
        assert!(integer.contains("testwiki.tsv.bz2:7"), "{integer}");
        assert!(integer.contains("event_user_id"), "{integer}");

        assert_eq!(
            unescape_tsv(&file, 8, "event_comment", r"bad\qescape").unwrap(),
            r"bad\qescape"
        );
    }

    #[test]
    fn history_hex_escapes_decode_utf8_bytes_strictly() {
        let file = history_file();
        let decoded = unescape_tsv(
            &file,
            276_428,
            "page_title_historical",
            r"Broken/Port\xc4\x81ls\x3aZin\xc4\x81tne",
        )
        .unwrap();
        assert_eq!(decoded, "Broken/Portāls:Zinātne");

        for (value, expected) in [
            (r"bad\x", "incomplete \\\\xHH"),
            (r"bad\xG0", "invalid \\\\xHH"),
            (r"bad\xC4", "invalid UTF-8"),
        ] {
            let error = unescape_tsv(&file, 276_428, "page_title_historical", value)
                .unwrap_err()
                .to_string();
            assert!(error.contains("testwiki.tsv.bz2:276428"), "{error}");
            assert!(error.contains("page_title_historical"), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn unknown_backslash_sequence_is_literal_title_content() {
        let file = history_file();
        assert_eq!(
            unescape_tsv(&file, 1_049_781, "page_title_historical", r"M\S_Kaunas").unwrap(),
            r"M\S_Kaunas"
        );
    }

    #[test]
    fn terminal_backslash_is_literal_title_content() {
        let file = history_file();
        assert_eq!(
            unescape_tsv(&file, 1_402_387, "page_title_current", r"Broken ending\").unwrap(),
            r"Broken ending\"
        );
    }

    #[test]
    fn history_partition_successors_cross_year_boundaries() {
        assert_eq!(partition_successor("2024").as_deref(), Some("2025"));
        assert_eq!(partition_successor("2024-11").as_deref(), Some("2024-12"));
        assert_eq!(partition_successor("2024-12").as_deref(), Some("2025-01"));
        assert!(partition_successor("2024-13").is_none());
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
