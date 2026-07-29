//! Direct upstream-dump to portable-archive construction.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;

use crate::archive::{
    ArchiveError, ArchiveRecordReader, ArchiveWriter, CompressionSettings, ManifestRecord,
    Record, RecordSorter, RecordSource, RevisionRecord, SiteInfoRecord, SiteInterwikiRecord,
    SiteNamespaceRecord, DEFAULT_FRAME_TARGET, MIRROR_FRAME_TARGET,
    MIRROR_REF_PREFIX_BYTES, MIRROR_REF_PREFIX_SAMPLE_BYTES,
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

#[derive(Default)]
struct ContentStreamStats {
    pages: u64,
    revisions: u64,
    frames: u64,
    bytes: u64,
}

struct ContentPartEnvelope {
    result: Result<ContentPartResult>,
    consumed: std::sync::mpsc::SyncSender<()>,
}

struct ContentArchiveSequence {
    receiver: std::sync::mpsc::Receiver<(usize, ContentPartEnvelope)>,
    pending: BTreeMap<usize, ContentPartEnvelope>,
    next_index: usize,
    total: usize,
    current: Option<(
        PathBuf,
        ArchiveRecordReader,
        std::sync::mpsc::SyncSender<()>,
    )>,
    stats: Arc<Mutex<ContentStreamStats>>,
}

impl ContentArchiveSequence {
    fn next_result(&mut self) -> crate::archive::Result<Option<ContentPartResult>> {
        if self.next_index == self.total {
            return Ok(None);
        }
        let envelope = loop {
            if let Some(envelope) = self.pending.remove(&self.next_index) {
                break envelope;
            }
            let (index, envelope) = self.receiver.recv().map_err(|_| {
                ArchiveError::Invalid("content workers stopped before completing the stream")
            })?;
            if index == self.next_index {
                break envelope;
            }
            self.pending.insert(index, envelope);
        };
        self.next_index += 1;
        let result = envelope.result.map_err(ArchiveError::Mirror)?;
        self.current = Some((
            result.path.clone(),
            ArchiveRecordReader::open(&result.path)?,
            envelope.consumed,
        ));
        Ok(Some(result))
    }

    fn open_next(&mut self) -> crate::archive::Result<Option<SiteInfoRecord>> {
        let Some(result) = self.next_result()? else {
            return Ok(None);
        };
        let (_, frames, complete) = crate::archive::index_file(&result.path)?;
        if !complete {
            return Err(ArchiveError::Invalid(
                "typed content segment is incomplete",
            ));
        }
        let bytes = std::fs::metadata(&result.path)?.len();
        {
            let mut stats = self.stats.lock().expect("content stats mutex");
            stats.pages += result.stats.pages;
            stats.revisions += result.stats.revisions;
            stats.frames += frames.len() as u64;
            stats.bytes += bytes;
        }
        let site_info = result.site_info;
        Ok(site_info)
    }

    fn prefetch(&mut self) -> crate::archive::Result<Option<SiteInfoRecord>> {
        if self.current.is_none() {
            self.open_next()
        } else {
            Ok(None)
        }
    }
}

impl RecordSource for ContentArchiveSequence {
    fn next_record(&mut self) -> crate::archive::Result<Option<Record>> {
        loop {
            if let Some((_, reader, _)) = self.current.as_mut() {
                if let Some(record) = reader.next_record()? {
                    return Ok(Some(record));
                }
                let (path, _, consumed) = self.current.take().expect("current content part");
                std::fs::remove_file(path)?;
                let _ = consumed.send(());
            }
            if self.open_next()?.is_none() && self.current.is_none() {
                return Ok(None);
            }
        }
    }
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
    let frontier = archive_frontier(base_archive.as_ref(), dbname)?;
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
        frontier,
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

struct ArchiveFrontier {
    content: chrono::NaiveDate,
    metadata: String,
}

fn archive_frontier(path: &Path, dbname: &str) -> Result<ArchiveFrontier> {
    let mut reader = ArchiveRecordReader::open(path).map_err(map_archive)?;
    let mut content = None;
    let mut metadata = None;
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
            content =
                Some(content.map_or(parsed, |current: chrono::NaiveDate| current.max(parsed)));
            metadata = Some(metadata.map_or(manifest.metadata_snapshot.clone(), |current: String| {
                current.max(manifest.metadata_snapshot)
            }));
        }
    }
    Ok(ArchiveFrontier {
        content: content.ok_or(Error::Corrupt("base archive has no manifest"))?,
        metadata: metadata.ok_or(Error::Corrupt("base archive has no metadata frontier"))?,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_update_inner(
    client: &Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    frontier: ArchiveFrontier,
    output: &Path,
    scratch: &Path,
    overlap_days: u64,
    frame_target: usize,
    compression: CompressionSettings,
    progress: &(impl Fn(&str) + Sync),
) -> Result<UpdateArchiveStats> {
    let content_from = frontier.content;
    progress(&format!(
        "discovering daily updates after {content_from} with {overlap_days} days of overlap"
    ));
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
    progress(&format!(
        "{} daily update runs cover through {content_through}",
        runs.len()
    ));
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
            snapshot_date_micros(run.date)?,
            progress,
        )?);
    }

    let (metadata_snapshot, mut history_files) =
        crate::sync::discover_history(client, config, dbname)?;
    if metadata_snapshot < frontier.metadata {
        return Err(Error::Mediawiki(wikimak_mediawiki::Error::Parse(
            format!(
                "MediaWiki History snapshot regressed from {} to {metadata_snapshot}",
                frontier.metadata
            ),
        )));
    }
    if metadata_snapshot == frontier.metadata {
        history_files.clear();
        progress(&format!(
            "MediaWiki History {metadata_snapshot} is already present"
        ));
    } else if history_files.len() > 2 {
        history_files = history_files.split_off(history_files.len() - 2);
    }
    if !history_files.is_empty() {
        progress(&format!(
            "ingesting {} partitions from MediaWiki History {metadata_snapshot}",
            history_files.len()
        ));
    }
    let history_results =
        build_history_parts(client, dbname, &history_files, scratch, cores, progress)?;

    let manifest_archive = scratch.join("update-manifest.swdump");
    let mut manifest_writer =
        ArchiveWriter::new(std::fs::File::create(&manifest_archive)?, DEFAULT_FRAME_TARGET)
            .map_err(map_archive)?;
    manifest_writer
        .write(&Record::Manifest {
            timestamp_micros: snapshot_date_micros(content_through)?,
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
                timestamp_micros: snapshot_date_micros(content_through)?,
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
    progress("assembling the sorted update record stream");
    let output_parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(output_parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(output_parent)?;
    let (_, output_frames, output_records) =
        crate::archive::merge_many_archives_with_compression(
            &inputs,
            temporary.as_file_mut(),
            frame_target,
            compression,
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
    let mut history_frames = 0_u64;
    let mut history_archive_bytes = 0_u64;
    for path in &history_paths {
        let (_, frames, complete) = crate::archive::index_file(path).map_err(map_archive)?;
        if !complete {
            return Err(Error::Corrupt("typed history segment is incomplete"));
        }
        history_frames += frames.len() as u64;
        history_archive_bytes += std::fs::metadata(path)?.len();
    }

    let output_parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(output_parent)?;
    let temporary = tempfile::NamedTempFile::new_in(output_parent)?;
    let mut source_files = content_run
        .parts
        .iter()
        .map(|part| part.filename.clone())
        .chain(history_files.iter().map(|file| file.part.filename.clone()))
        .collect::<Vec<_>>();
    source_files.sort();
    let groups = crate::sync::part_groups(content_run.parts.clone());
    if groups.is_empty() {
        return Err(Error::Corrupt("content dump contains no parts"));
    }
    let group_count = groups.len();
    let workers = groups.len().min(cores).min(3).max(1);
    let bz2_workers = (cores / workers).max(1);
    let queue = Arc::new(Mutex::new(VecDeque::from(
        groups.into_iter().enumerate().collect::<Vec<_>>(),
    )));
    let failed = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = std::sync::mpsc::sync_channel(workers);
    let content_stats = Arc::new(Mutex::new(ContentStreamStats::default()));
    let observed_at_micros = snapshot_date_micros(content_run.date)?;
    let (file, output_frames) = std::thread::scope(|scope| -> Result<_> {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let failed = Arc::clone(&failed);
            let sender = sender.clone();
            scope.spawn(move || loop {
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                let Some((index, group)) = queue.lock().expect("queue mutex").pop_front() else {
                    return;
                };
                let result = build_content_group(
                    client,
                    &group,
                    index,
                    scratch,
                    bz2_workers,
                    observed_at_micros,
                    progress,
                );
                if result.is_err() {
                    failed.store(true, Ordering::Relaxed);
                }
                let (consumed, wait_consumed) = std::sync::mpsc::sync_channel(0);
                if sender
                    .send((
                        index,
                        ContentPartEnvelope {
                            result,
                            consumed,
                        },
                    ))
                    .is_err()
                {
                    return;
                }
                if wait_consumed.recv().is_err() {
                    return;
                }
            });
        }
        drop(sender);
        let mut content = ContentArchiveSequence {
            receiver,
            pending: BTreeMap::new(),
            next_index: 0,
            total: group_count,
            current: None,
            stats: Arc::clone(&content_stats),
        };
        let site_info = content
            .prefetch()
            .map_err(map_archive)?
            .ok_or(Error::Corrupt("content dumps contain no siteinfo"))?;

        let manifest_archive = scratch.join("manifest.swdump");
        let mut manifest_writer = ArchiveWriter::new(
            std::fs::File::create(&manifest_archive)?,
            DEFAULT_FRAME_TARGET,
        )
        .map_err(map_archive)?;
        manifest_writer
            .write(&Record::Manifest {
                timestamp_micros: observed_at_micros,
                manifest: ManifestRecord {
                    wiki_db: dbname.to_owned(),
                    content_snapshot: content_run.date.to_string(),
                    metadata_snapshot: history_snapshot,
                    source_files,
                },
            })
            .map_err(map_archive)?;
        manifest_writer
            .write(&Record::SiteInfo {
                timestamp_micros: observed_at_micros,
                site_info,
            })
            .map_err(map_archive)?;
        manifest_writer.finish().map_err(map_archive)?;

        let mut inputs: Vec<Box<dyn RecordSource + '_>> = vec![Box::new(content)];
        for path in &history_paths {
            inputs.push(Box::new(ArchiveRecordReader::open(path).map_err(map_archive)?));
        }
        inputs.push(Box::new(
            ArchiveRecordReader::open(manifest_archive).map_err(map_archive)?,
        ));
        progress("assembling final archive and sampling newest page revisions");
        let bootstrap = tempfile::tempfile_in(scratch)?;
        let (file, output_frames, _, _) =
            crate::archive::merge_record_sources_bootstrapping_ref_prefix(
            inputs,
            temporary,
            bootstrap,
            MIRROR_FRAME_TARGET,
            CompressionSettings {
                level: 9,
                ..CompressionSettings::default()
            },
            MIRROR_REF_PREFIX_SAMPLE_BYTES,
            MIRROR_REF_PREFIX_BYTES,
        )
        .map_err(map_archive)?;
        Ok((file, output_frames))
    })?;
    file.as_file().sync_all()?;
    file.persist(output)
        .map_err(|error| Error::Io(error.error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o644))?;
    }

    let content_stats = content_stats.lock().expect("content stats mutex");
    let mut stats = DirectArchiveStats {
        content_parts: content_run.parts.len() as u64,
        history_parts: history_files.len() as u64,
        content_archive_bytes: content_stats.bytes,
        history_archive_bytes,
        output_bytes: std::fs::metadata(output)?.len(),
        content_frames: content_stats.frames,
        history_frames,
        output_frames,
        pages: content_stats.pages,
        revisions: content_stats.revisions,
        ..Default::default()
    };
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
    observed_at_micros: i64,
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
                match build_content_group(
                    client,
                    &group,
                    index,
                    scratch,
                    bz2_workers,
                    observed_at_micros,
                    progress,
                ) {
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
    observed_at_micros: i64,
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
            let count = crate::archive::write_content_page(
                &mut writer,
                page_id,
                observed_at_micros,
                header.title,
                records,
            )
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
        language: String::new(),
        rtl: false,
        server: String::new(),
        script_path: String::new(),
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
        magic_words: Vec::new(),
    }
}

fn snapshot_date_micros(date: chrono::NaiveDate) -> Result<i64> {
    date.and_hms_micro_opt(23, 59, 59, 999_999)
        .map(|value| value.and_utc().timestamp_micros())
        .ok_or(Error::Corrupt("invalid content snapshot date"))
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
