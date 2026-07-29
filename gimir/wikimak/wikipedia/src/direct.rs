//! Direct upstream-dump to portable-archive construction.

use std::collections::VecDeque;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;

use crate::archive::{
    ArchiveError, ArchiveRecordReader, ArchiveWriter, CompressionSettings, ManifestRecord,
    Record, RecordSorter, RevisionRecord, SiteInfoRecord, SiteInterwikiRecord,
    SiteNamespaceRecord, DEFAULT_FRAME_TARGET,
};
use crate::instance::{ContributorMeta, RevisionMeta};
use crate::{Error, Result};

#[derive(Clone, Debug, Default)]
pub struct DirectArchiveStats {
    pub content_parts: u64,
    pub history_parts: u64,
    pub pages: u64,
    pub revisions: u64,
    pub history_events: u64,
    pub page_history_events: u64,
    pub user_history_events: u64,
    pub global_history_events: u64,
    pub content_archive_bytes: u64,
    pub history_archive_bytes: u64,
    pub output_bytes: u64,
    pub scratch_peak_bytes: u64,
    pub content_frames: u64,
    pub history_frames: u64,
    pub output_frames: u64,
    pub elapsed_millis: u64,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateArchiveStats {
    pub content_from: String,
    pub content_through: String,
    pub metadata_snapshot: String,
    pub incremental_runs: u64,
    pub content_parts: u64,
    pub history_parts: u64,
    pub pages: u64,
    pub revisions: u64,
    pub output_frames: u64,
    pub output_records: u64,
    pub output_bytes: u64,
    pub scratch_peak_bytes: u64,
    pub elapsed_millis: u64,
}

#[derive(Default)]
struct PartialStats {
    pages: u64,
    revisions: u64,
    events: u64,
    page_events: u64,
    user_events: u64,
    global_events: u64,
}

struct ContentPartResult {
    path: PathBuf,
    stats: PartialStats,
    site_info: Option<SiteInfoRecord>,
}

pub fn build_direct_archive(
    client: &Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    output: impl AsRef<Path>,
    scratch_parent: impl AsRef<Path>,
    progress: impl Fn(&str) + Sync,
) -> Result<DirectArchiveStats> {
    let started = Instant::now();
    std::fs::create_dir_all(scratch_parent.as_ref())?;
    let scratch = tempfile::TempDir::new_in(scratch_parent)?;
    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let path = scratch.path().to_path_buf();
        let peak = Arc::clone(&peak);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(bytes) = directory_bytes(&path) {
                    peak.fetch_max(bytes, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if let Ok(bytes) = directory_bytes(&path) {
                peak.fetch_max(bytes, Ordering::Relaxed);
            }
        })
    };
    let result = build_direct_inner(
        client,
        config,
        dbname,
        output.as_ref(),
        scratch.path(),
        &progress,
    );
    stop.store(true, Ordering::Relaxed);
    let _ = monitor.join();
    let mut stats = result?;
    stats.scratch_peak_bytes = peak.load(Ordering::Relaxed);
    stats.elapsed_millis = started.elapsed().as_millis() as u64;
    Ok(stats)
}

pub fn build_update_archive(
    client: &Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    base_archive: impl AsRef<Path>,
    output: impl AsRef<Path>,
    scratch_parent: impl AsRef<Path>,
    overlap_days: u64,
    frame_target: usize,
    compression: CompressionSettings,
    progress: impl Fn(&str) + Sync,
) -> Result<UpdateArchiveStats> {
    let started = Instant::now();
    let content_from = archive_content_date(base_archive.as_ref(), dbname)?;
    std::fs::create_dir_all(scratch_parent.as_ref())?;
    let scratch = tempfile::TempDir::new_in(scratch_parent)?;
    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let path = scratch.path().to_path_buf();
        let peak = Arc::clone(&peak);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(bytes) = directory_bytes(&path) {
                    peak.fetch_max(bytes, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if let Ok(bytes) = directory_bytes(&path) {
                peak.fetch_max(bytes, Ordering::Relaxed);
            }
        })
    };
    let result = build_update_inner(
        client,
        config,
        dbname,
        content_from,
        output.as_ref(),
        scratch.path(),
        overlap_days,
        frame_target,
        compression,
        &progress,
    );
    stop.store(true, Ordering::Relaxed);
    let _ = monitor.join();
    let mut stats = result?;
    stats.scratch_peak_bytes = peak.load(Ordering::Relaxed);
    stats.elapsed_millis = started.elapsed().as_millis() as u64;
    Ok(stats)
}

fn archive_content_date(path: &Path, dbname: &str) -> Result<chrono::NaiveDate> {
    let mut reader = ArchiveRecordReader::open(path).map_err(map_archive)?;
    let mut date = None;
    while let Some(record) = reader.next_record().map_err(map_archive)? {
        if let Record::Manifest { manifest, .. } = record {
            if manifest.wiki_db != dbname {
                return Err(Error::Corrupt("base archive belongs to another wiki"));
            }
            let parsed = chrono::NaiveDate::parse_from_str(
                &manifest.content_snapshot,
                "%Y-%m-%d",
            )
            .map_err(|_| Error::Corrupt("invalid archive content snapshot date"))?;
            date = Some(date.map_or(parsed, |current: chrono::NaiveDate| current.max(parsed)));
        }
    }
    date.ok_or(Error::Corrupt("base archive has no manifest"))
}

#[allow(clippy::too_many_arguments)]
fn build_update_inner(
    client: &Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    content_from: chrono::NaiveDate,
    output: &Path,
    scratch: &Path,
    overlap_days: u64,
    frame_target: usize,
    compression: CompressionSettings,
    progress: &(impl Fn(&str) + Sync),
) -> Result<UpdateArchiveStats> {
    let discovery_after = content_from
        .checked_sub_signed(chrono::Duration::days(
            i64::try_from(overlap_days.saturating_add(1))
                .map_err(|_| Error::Corrupt("update overlap is too large"))?,
        ))
        .ok_or(Error::Corrupt("update overlap precedes the calendar"))?;
    let runs = wikimak_mediawiki::discover_incremental_with(
        client,
        config,
        dbname,
        Some(discovery_after),
    )?;
    if let Some(first_after_base) = runs.iter().find(|run| run.date > content_from) {
        if first_after_base.date > content_from.succ_opt().unwrap_or(content_from) {
            return Err(Error::Mediawiki(wikimak_mediawiki::Error::Parse(
                format!(
                    "daily dump gap after {content_from}; explicit full refresh required"
                ),
            )));
        }
    }
    let content_through = runs
        .last()
        .map(|run| run.date)
        .unwrap_or(content_from);
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let mut content_results = Vec::new();
    for run in &runs {
        let run_scratch = scratch.join(format!("incremental-{}", run.date));
        std::fs::create_dir(&run_scratch)?;
        content_results.extend(build_content_parts(
            client,
            &run.parts,
            &run_scratch,
            cores,
            progress,
        )?);
    }

    let (metadata_snapshot, mut history_files) =
        crate::sync::discover_history(client, config, dbname)?;
    if history_files.len() > 2 {
        history_files = history_files.split_off(history_files.len() - 2);
    }
    let history_results =
        build_history_parts(client, dbname, &history_files, scratch, cores, progress)?;

    let manifest_archive = scratch.join("update-manifest.swdump");
    let mut manifest_writer =
        ArchiveWriter::new(std::fs::File::create(&manifest_archive)?, DEFAULT_FRAME_TARGET)
            .map_err(map_archive)?;
    manifest_writer
        .write(&Record::Manifest {
            timestamp_micros: i64::MAX,
            manifest: ManifestRecord {
                wiki_db: dbname.to_owned(),
                content_snapshot: content_through.to_string(),
                metadata_snapshot: metadata_snapshot.clone(),
                source_files: Vec::new(),
            },
        })
        .map_err(map_archive)?;
    if let Some(site_info) = content_results
        .iter()
        .find_map(|result| result.site_info.clone())
    {
        manifest_writer
            .write(&Record::SiteInfo {
                timestamp_micros: i64::MAX,
                site_info,
            })
            .map_err(map_archive)?;
    }
    manifest_writer.finish().map_err(map_archive)?;

    let mut inputs = content_results
        .iter()
        .map(|result| result.path.clone())
        .chain(history_results.iter().map(|(path, _)| path.clone()))
        .collect::<Vec<_>>();
    inputs.push(manifest_archive);
    let output_parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(output_parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(output_parent)?;
    let (_, output_frames, output_records) =
        crate::archive::merge_many_archives_with_compression_in(
            &inputs,
            temporary.as_file_mut(),
            frame_target,
            compression,
            scratch,
        )
        .map_err(map_archive)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(output)
        .map_err(|error| Error::Io(error.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o644))?;
    }

    let mut stats = UpdateArchiveStats {
        content_from: content_from.to_string(),
        content_through: content_through.to_string(),
        metadata_snapshot,
        incremental_runs: runs.len() as u64,
        content_parts: runs.iter().map(|run| run.parts.len() as u64).sum(),
        history_parts: history_files.len() as u64,
        output_frames,
        output_records,
        output_bytes: std::fs::metadata(output)?.len(),
        ..Default::default()
    };
    for result in content_results {
        stats.pages += result.stats.pages;
        stats.revisions += result.stats.revisions;
    }
    Ok(stats)
}

fn build_direct_inner(
    client: &Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    output: &Path,
    scratch: &Path,
    progress: &(impl Fn(&str) + Sync),
) -> Result<DirectArchiveStats> {
    let content_run = wikimak_mediawiki::discover_with(client, config, dbname)?;
    let (history_snapshot, history_files) =
        crate::sync::discover_history(client, config, dbname)?;
    let cores = std::thread::available_parallelism().map_or(1, usize::from);

    let history_results =
        build_history_parts(client, dbname, &history_files, scratch, cores, progress)?;
    let history_paths: Vec<PathBuf> = history_results
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    let history_archive = scratch.join("history-all.swdump");
    let history_frames = if history_paths.len() == 1 {
        let (_, frames, complete) =
            crate::archive::index_file(&history_paths[0]).map_err(map_archive)?;
        if !complete {
            return Err(Error::Corrupt("typed history segment is incomplete"));
        }
        std::fs::rename(&history_paths[0], &history_archive)?;
        frames.len() as u64
    } else {
        let (_, frames, _) = crate::archive::merge_many_archives_with_compression_in(
            &history_paths,
            std::fs::File::create(&history_archive)?,
            DEFAULT_FRAME_TARGET,
            CompressionSettings::default(),
            scratch,
        )
        .map_err(map_archive)?;
        for path in &history_paths {
            std::fs::remove_file(path)?;
        }
        frames
    };

    let content_results =
        build_content_parts(client, &content_run.parts, scratch, cores, progress)?;
    let content_paths: Vec<PathBuf> = content_results
        .iter()
        .map(|result| result.path.clone())
        .collect();
    let content_archive = scratch.join("content-all.swdump");
    let (_, content_frames) = crate::archive::concatenate_archives(
        &content_paths,
        std::fs::File::create(&content_archive)?,
        DEFAULT_FRAME_TARGET,
    )
    .map_err(map_archive)?;
    for path in &content_paths {
        std::fs::remove_file(path)?;
    }

    let output_parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(output_parent)?;
    let temporary = tempfile::NamedTempFile::new_in(output_parent)?;
    let manifest_archive = scratch.join("manifest.swdump");
    let mut manifest_writer = ArchiveWriter::new(
        std::fs::File::create(&manifest_archive)?,
        DEFAULT_FRAME_TARGET,
    )
    .map_err(map_archive)?;
    let mut source_files = content_run
        .parts
        .iter()
        .map(|part| part.filename.clone())
        .chain(history_files.iter().map(|file| file.part.filename.clone()))
        .collect::<Vec<_>>();
    source_files.sort();
    manifest_writer
        .write(&Record::Manifest {
            timestamp_micros: i64::MAX,
            manifest: ManifestRecord {
                wiki_db: dbname.to_owned(),
                content_snapshot: content_run.date.to_string(),
                metadata_snapshot: history_snapshot,
                source_files,
            },
        })
        .map_err(map_archive)?;
    let site_info = content_results
        .iter()
        .find_map(|result| result.site_info.clone())
        .ok_or(Error::Corrupt("content dumps contain no siteinfo"))?;
    manifest_writer
        .write(&Record::SiteInfo {
            timestamp_micros: i64::MAX,
            site_info,
        })
        .map_err(map_archive)?;
    manifest_writer.finish().map_err(map_archive)?;
    let (file, output_frames, _) = crate::archive::merge_many_archives_with_compression_in(
        &[content_archive.clone(), history_archive.clone(), manifest_archive],
        temporary,
        DEFAULT_FRAME_TARGET,
        CompressionSettings::default(),
        scratch,
    )
    .map_err(map_archive)?;
    file.as_file().sync_all()?;
    file.persist(output)
        .map_err(|error| Error::Io(error.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o644))?;
    }

    let mut stats = DirectArchiveStats {
        content_parts: content_run.parts.len() as u64,
        history_parts: history_files.len() as u64,
        content_archive_bytes: std::fs::metadata(&content_archive)?.len(),
        history_archive_bytes: std::fs::metadata(&history_archive)?.len(),
        output_bytes: std::fs::metadata(output)?.len(),
        content_frames,
        history_frames,
        output_frames,
        ..Default::default()
    };
    for result in content_results {
        stats.pages += result.stats.pages;
        stats.revisions += result.stats.revisions;
    }
    for (_, partial) in history_results {
        stats.history_events += partial.events;
        stats.page_history_events += partial.page_events;
        stats.user_history_events += partial.user_events;
        stats.global_history_events += partial.global_events;
    }
    Ok(stats)
}

fn build_history_parts(
    client: &Client,
    dbname: &str,
    files: &[crate::sync::HistoryFile],
    scratch: &Path,
    cores: usize,
    progress: &(impl Fn(&str) + Sync),
) -> Result<Vec<(PathBuf, PartialStats)>> {
    let workers = files.len().min(cores).min(3).max(1);
    let bz2_workers = (cores / workers).max(1);
    let queue = Arc::new(Mutex::new(VecDeque::from(
        files.iter().cloned().enumerate().collect::<Vec<_>>(),
    )));
    let results = Arc::new(Mutex::new(Vec::new()));
    let failure = Arc::new(Mutex::new(None));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let failure = Arc::clone(&failure);
            scope.spawn(move || loop {
                if failure.lock().expect("failure mutex").is_some() {
                    return;
                }
                let Some((index, file)) = queue.lock().expect("queue mutex").pop_front() else {
                    return;
                };
                progress(&format!("history {}", file.part.filename));
                match build_history_part(client, dbname, &file, index, scratch, bz2_workers) {
                    Ok(result) => results.lock().expect("results mutex").push((index, result)),
                    Err(error) => {
                        *failure.lock().expect("failure mutex") = Some(error);
                        return;
                    }
                }
            });
        }
    });
    if let Some(error) = failure.lock().expect("failure mutex").take() {
        return Err(error);
    }
    let mut results = std::mem::take(&mut *results.lock().expect("results mutex"));
    results.sort_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

fn build_history_part(
    client: &Client,
    dbname: &str,
    file: &crate::sync::HistoryFile,
    file_index: usize,
    scratch: &Path,
    bz2_workers: usize,
) -> Result<(PathBuf, PartialStats)> {
    let source = wikimak_mediawiki::fetch(client, &file.part)?;
    let decoder = wikimak_mediawiki::new_bz2_reader(
        source,
        wikimak_mediawiki::Bz2Options {
            workers: bz2_workers,
        },
    );
    let mut sorter = RecordSorter::new_in(scratch).map_err(map_archive)?;
    let mut stats = PartialStats::default();
    for (line_number, line) in std::io::BufReader::new(decoder).lines().enumerate() {
        let line = line?;
        let fields: Vec<&str> = line.split('\t').collect();
        let page = match fields.len() {
            78 => 28,
            76 => 26,
            _ => {
                return Err(parse_error(format!(
                    "{}:{} unsupported history schema with {} fields",
                    file.part.filename,
                    line_number + 1,
                    fields.len()
                )))
            }
        };
        if fields[0] != dbname {
            return Err(parse_error(format!(
                "{}:{} contains {}, expected {dbname}",
                file.part.filename,
                line_number + 1,
                fields[0]
            )));
        }
        let ordinal = ((file_index as u64) << 48) | line_number as u64;
        for record in crate::sync::typed_history_records(
            file,
            line_number + 1,
            &fields,
            page,
            if fields.len() == 78 { 60 } else { 58 },
            ordinal,
        )? {
            sorter.push(record).map_err(map_archive)?;
        }
        stats.events += 1;
        match fields[2] {
            "page" | "revision" if !fields[page].is_empty() && fields[page] != "0" => {
                stats.page_events += 1
            }
            "user" if !fields[page + 13].is_empty() && fields[page + 13] != "0" => {
                stats.user_events += 1
            }
            _ => stats.global_events += 1,
        }
    }
    let path = scratch.join(format!("history-{file_index:06}.swdump"));
    let (_, _, _) = sorter
        .finish(std::fs::File::create(&path)?, DEFAULT_FRAME_TARGET)
        .map_err(map_archive)?;
    Ok((path, stats))
}

fn build_content_parts(
    client: &Client,
    parts: &[wikimak_mediawiki::Part],
    scratch: &Path,
    cores: usize,
    progress: &(impl Fn(&str) + Sync),
) -> Result<Vec<ContentPartResult>> {
    let groups = crate::sync::part_groups(parts.to_vec());
    let workers = groups.len().min(cores).min(3).max(1);
    let bz2_workers = (cores / workers).max(1);
    let queue = Arc::new(Mutex::new(VecDeque::from(
        groups.into_iter().enumerate().collect::<Vec<_>>(),
    )));
    let results = Arc::new(Mutex::new(Vec::new()));
    let failure = Arc::new(Mutex::new(None));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let failure = Arc::clone(&failure);
            scope.spawn(move || loop {
                if failure.lock().expect("failure mutex").is_some() {
                    return;
                }
                let Some((index, group)) = queue.lock().expect("queue mutex").pop_front() else {
                    return;
                };
                match build_content_group(client, &group, index, scratch, bz2_workers, progress) {
                    Ok(result) => results.lock().expect("results mutex").push((index, result)),
                    Err(error) => {
                        *failure.lock().expect("failure mutex") = Some(error);
                        return;
                    }
                }
            });
        }
    });
    if let Some(error) = failure.lock().expect("failure mutex").take() {
        return Err(error);
    }
    let mut results = std::mem::take(&mut *results.lock().expect("results mutex"));
    results.sort_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

fn build_content_group(
    client: &Client,
    parts: &[wikimak_mediawiki::Part],
    index: usize,
    scratch: &Path,
    bz2_workers: usize,
    progress: &(impl Fn(&str) + Sync),
) -> Result<ContentPartResult> {
    let path = scratch.join(format!("content-{index:06}.swdump"));
    let mut writer = ArchiveWriter::new(std::fs::File::create(&path)?, DEFAULT_FRAME_TARGET)
        .map_err(map_archive)?;
    let mut stats = PartialStats::default();
    let mut site_info = None;
    for part in parts {
        progress(&format!("content {}", part.filename));
        let source = wikimak_mediawiki::fetch(client, part)?;
        let input: Box<dyn Read + Send> = if part.filename.ends_with(".bz2") {
            Box::new(wikimak_mediawiki::new_bz2_reader(
                source,
                wikimak_mediawiki::Bz2Options {
                    workers: bz2_workers,
                },
            ))
        } else {
            Box::new(source)
        };
        let mut page_stream = wikimak_mediawiki::new_page_stream(input);
        let revisions = page_stream.revisions_mut();
        while let Some(header) = revisions.next_page() {
            let header = header?;
            if site_info.is_none() {
                site_info = revisions.site_info().map(convert_site_info);
            }
            let page_id = u64::try_from(header.id)
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| parse_error(format!("invalid page id {}", header.id)))?;
            let records = std::iter::from_fn(|| {
                revisions.next_revision().map(|result| {
                    result
                        .map_err(Error::Mediawiki)
                        .and_then(convert_revision)
                        .map_err(ArchiveError::Mirror)
                })
            });
            let count =
                crate::archive::write_content_page(&mut writer, page_id, header.title, records)
                    .map_err(map_archive)?;
            stats.pages += 1;
            stats.revisions += count;
        }
    }
    writer.finish().map_err(map_archive)?;
    Ok(ContentPartResult {
        path,
        stats,
        site_info,
    })
}

fn convert_site_info(site_info: &wikimak_mediawiki::SiteInfo) -> SiteInfoRecord {
    SiteInfoRecord {
        site_name: site_info.site_name.clone(),
        db_name: site_info.db_name.clone(),
        base: site_info.base.clone(),
        generator: site_info.generator.clone(),
        case: site_info.case.clone(),
        namespaces: site_info
            .namespaces
            .values()
            .map(|namespace| SiteNamespaceRecord {
                id: namespace.id,
                case: namespace.case.clone(),
                localized_name: namespace.name.clone(),
                aliases: namespace.aliases.clone(),
            })
            .collect(),
        interwiki: site_info
            .interwiki
            .iter()
            .map(|interwiki| SiteInterwikiRecord {
                prefix: interwiki.prefix.clone(),
                url: interwiki.url.clone(),
                is_local: interwiki.is_local,
            })
            .collect(),
    }
}

fn convert_revision(revision: wikimak_mediawiki::Revision) -> Result<RevisionRecord> {
    let mut flags = 0_u32;
    if revision.text_hidden {
        flags |= crate::FLAG_TEXT_HIDDEN;
    }
    if revision.comment_hidden {
        flags |= crate::FLAG_COMMENT_HIDDEN;
    }
    if revision.contributor_hidden {
        flags |= crate::FLAG_CONTRIBUTOR_HIDDEN;
    }
    if revision.suppressed {
        flags |= crate::FLAG_SUPPRESSED;
    }
    let contributor = match revision.contributor {
        wikimak_mediawiki::Contributor::Anonymous { ip } => ContributorMeta::Anonymous { ip },
        wikimak_mediawiki::Contributor::Named { username, user_id } => ContributorMeta::Named {
            username,
            user_id: u64::try_from(user_id)
                .map_err(|_| Error::Corrupt("negative contributor user id"))?,
        },
        wikimak_mediawiki::Contributor::Hidden => ContributorMeta::Hidden,
    };
    let text = if revision.text_hidden {
        Vec::new()
    } else {
        revision.text.into_bytes()
    };
    Ok(RevisionRecord {
        meta: RevisionMeta {
            rev_id: u64::try_from(revision.id)
                .map_err(|_| Error::Corrupt("negative revision id"))?,
            parent_id: revision
                .parent_id
                .map(u64::try_from)
                .transpose()
                .map_err(|_| Error::Corrupt("negative parent revision id"))?
                .unwrap_or(0),
            ts: revision.timestamp,
            contributor,
            comment: revision.comment,
            sha1: String::new(),
            flags,
            text_len: text.len() as u64,
        },
        has_text: !revision.text_hidden,
        text,
        visibility: None,
        history: None,
    })
}

fn directory_bytes(path: &Path) -> std::io::Result<u64> {
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            bytes = bytes.saturating_add(directory_bytes(&entry.path())?);
        } else {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

fn map_archive(error: ArchiveError) -> Error {
    match error {
        ArchiveError::Io(error) => Error::Io(error),
        ArchiveError::Mirror(error) => error,
        _ => Error::Mediawiki(wikimak_mediawiki::Error::Parse(error.to_string())),
    }
}

fn parse_error(message: String) -> Error {
    Error::Mediawiki(wikimak_mediawiki::Error::Parse(message))
}
