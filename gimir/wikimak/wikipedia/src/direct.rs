//! Direct upstream-dump to portable-archive construction.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PartialStats {
    pages: u64,
    revisions: u64,
    events: u64,
    page_events: u64,
    user_events: u64,
    global_events: u64,
}

#[derive(Deserialize, Serialize)]
struct PartCheckpointReceipt {
    schema: u32,
    key: String,
    stats: PartialStats,
}

struct ContentPartResult {
    path: PathBuf,
    stats: PartialStats,
    site_info: Option<SiteInfoRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PlannedPart {
    url: String,
    filename: String,
    size_bytes: u64,
    sha256: Option<String>,
    sha1: Option<String>,
    md5: Option<String>,
}

impl From<&wikimak_mediawiki::Part> for PlannedPart {
    fn from(part: &wikimak_mediawiki::Part) -> Self {
        Self {
            url: part.url.clone(),
            filename: part.filename.clone(),
            size_bytes: part.size_bytes,
            sha256: part.sha256.clone(),
            sha1: part.sha1.clone(),
            md5: part.md5.clone(),
        }
    }
}

impl From<&PlannedPart> for wikimak_mediawiki::Part {
    fn from(part: &PlannedPart) -> Self {
        Self {
            url: part.url.clone(),
            filename: part.filename.clone(),
            size_bytes: part.size_bytes,
            sha256: part.sha256.clone(),
            sha1: part.sha1.clone(),
            md5: part.md5.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PlannedHistoryFile {
    partition: String,
    part: PlannedPart,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DirectBuildPlan {
    pub(crate) schema: u32,
    pub(crate) plan_id: String,
    pub(crate) wiki_db: String,
    pub(crate) content_snapshot: String,
    pub(crate) metadata_snapshot: String,
    pub(crate) observed_at_micros: i64,
    pub(crate) frame_target: usize,
    pub(crate) range_target: u64,
    pub(crate) compression_level: i32,
    pub(crate) ref_prefix_sample_bytes: usize,
    pub(crate) ref_prefix_bytes: usize,
    pub(crate) content_groups: Vec<Vec<PlannedPart>>,
    pub(crate) history_files: Vec<PlannedHistoryFile>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MirrorBuildProgress {
    pub phase: String,
    pub targets_total: u64,
    pub targets_completed: u64,
    pub targets_active: Vec<String>,
    pub source_bytes_total: u64,
    pub source_bytes_completed: u64,
    pub snapshot: String,
}

impl DirectBuildPlan {
    pub(crate) fn target_count(&self) -> usize {
        self.content_groups.len() + self.history_files.len()
    }

    pub(crate) fn source_bytes(&self) -> u64 {
        self.content_groups
            .iter()
            .flatten()
            .map(|part| part.size_bytes)
            .chain(
                self.history_files
                    .iter()
                    .map(|file| file.part.size_bytes),
            )
            .sum()
    }

    fn target_source_bytes(&self, kind: &str, index: usize) -> u64 {
        match kind {
            "content" => self
                .content_groups
                .get(index)
                .into_iter()
                .flatten()
                .map(|part| part.size_bytes)
                .sum(),
            "history" => self
                .history_files
                .get(index)
                .map_or(0, |file| file.part.size_bytes),
            _ => 0,
        }
    }
}

fn direct_plan_id(plan: &DirectBuildPlan) -> Result<String> {
    let mut identity = plan.clone();
    identity.plan_id.clear();
    let identity = serde_json::to_vec(&identity)
        .map_err(|_| Error::Corrupt("cannot encode direct build plan"))?;
    use sha1::Digest;
    Ok(hex::encode(sha1::Sha1::digest(identity)))
}

#[derive(Debug, Deserialize, Serialize)]
struct BuildReceipt {
    plan_id: String,
    kind: String,
    index: usize,
    data_bytes: u64,
}

fn plan_part(part: &PlannedPart) -> wikimak_mediawiki::Part {
    part.into()
}

fn planned_history(file: &PlannedHistoryFile) -> crate::sync::HistoryFile {
    crate::sync::HistoryFile {
        partition: file.partition.clone(),
        part: plan_part(&file.part),
    }
}

pub(crate) fn discover_direct_build_plan(
    client: &Client,
    config: &wikimak_mediawiki::Config,
    dbname: &str,
    progress: &(impl Fn(&str) + Sync),
) -> Result<DirectBuildPlan> {
    progress("discovering upstream content and MediaWiki History files");
    let content_run = wikimak_mediawiki::discover_with(client, config, dbname)?;
    let (metadata_snapshot, history_files) =
        crate::sync::discover_history(client, config, dbname)?;
    let content_groups = crate::sync::part_groups(content_run.parts.clone())
        .iter()
        .map(|group| group.iter().map(PlannedPart::from).collect())
        .collect::<Vec<_>>();
    if content_groups.is_empty() {
        return Err(Error::Corrupt("content dump contains no parts"));
    }
    let mut plan = DirectBuildPlan {
        schema: 1,
        plan_id: String::new(),
        wiki_db: dbname.to_owned(),
        content_snapshot: content_run.date.to_string(),
        metadata_snapshot,
        observed_at_micros: snapshot_date_micros(content_run.date)?,
        frame_target: MIRROR_FRAME_TARGET,
        range_target: crate::archive_set::DEFAULT_RANGE_TARGET,
        compression_level: 9,
        ref_prefix_sample_bytes: MIRROR_REF_PREFIX_SAMPLE_BYTES,
        ref_prefix_bytes: MIRROR_REF_PREFIX_BYTES,
        content_groups,
        history_files: history_files
            .iter()
            .map(|file| PlannedHistoryFile {
                partition: file.partition.clone(),
                part: PlannedPart::from(&file.part),
            })
            .collect(),
    };
    plan.plan_id = direct_plan_id(&plan)?;
    progress(&format!(
        "planned {} durable source targets ({} bytes)",
        plan.target_count(),
        plan.source_bytes(),
    ));
    Ok(plan)
}

pub(crate) fn read_direct_build_plan(path: &Path) -> Result<DirectBuildPlan> {
    let bytes = std::fs::read(path)?;
    let plan: DirectBuildPlan = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Corrupt("invalid direct build plan"))?;
    if plan.schema != 1
        || plan.frame_target == 0
        || plan.range_target == 0
        || plan.ref_prefix_sample_bytes == 0
        || plan.ref_prefix_bytes == 0
        || plan.plan_id != direct_plan_id(&plan)?
    {
        return Err(Error::Corrupt("unsupported direct build plan"));
    }
    Ok(plan)
}

fn node_path(root: &Path, kind: &str, index: usize) -> PathBuf {
    root.join("nodes")
        .join(format!("{kind}-{index:06}.done"))
}

fn validate_node(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: &str,
    index: usize,
) -> Result<bool> {
    let node = node_path(root, kind, index);
    let data = node.join("data.swdump");
    let receipt: BuildReceipt = match std::fs::read(node.join("receipt.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(receipt) => receipt,
        None => return Ok(false),
    };
    if receipt.plan_id != plan.plan_id
        || receipt.kind != kind
        || receipt.index != index
        || std::fs::metadata(&data)?.len() != receipt.data_bytes
    {
        return Ok(false);
    }
    let (_, _, complete) = crate::archive::index_file(&data).map_err(map_archive)?;
    if !complete {
        return Ok(false);
    }
    if kind == "content" && index == 0 {
        let (_, _, complete) =
            crate::archive::index_file(node.join("siteinfo.swdump")).map_err(map_archive)?;
        if !complete {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn prune_invalid_build_nodes(root: &Path, plan: &DirectBuildPlan) -> Result<usize> {
    std::fs::create_dir_all(root.join("nodes"))?;
    let mut reusable = 0;
    for (kind, count) in [
        ("content", plan.content_groups.len()),
        ("history", plan.history_files.len()),
    ] {
        for index in 0..count {
            let path = node_path(root, kind, index);
            if validate_node(root, plan, kind, index).unwrap_or(false) {
                reusable += 1;
            } else if path.exists() {
                std::fs::remove_dir_all(path)?;
            }
        }
    }
    for entry in std::fs::read_dir(root.join("nodes"))? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
        }
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.file_name().to_string_lossy().starts_with(".tmp")
        {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(reusable)
}

fn persist_completion_marker(root: &Path, plan: &DirectBuildPlan) -> Result<()> {
    let marker = root.join("archive.complete");
    {
        let mut file = std::fs::File::create(&marker)?;
        file.write_all(plan.plan_id.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    sync_directory(root)
}

pub(crate) fn recover_direct_build_completion(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<bool> {
    let marker = root.join("archive.complete");
    let output = root.join("archive.swdump");
    let marker_matches = std::fs::read_to_string(&marker)
        .is_ok_and(|stored| stored.trim_end() == plan.plan_id);
    if output.exists()
        && archive_file_complete(&output)
        && (marker_matches || archive_records_are_readable(&output))
    {
        if !marker_matches {
            persist_completion_marker(root, plan)?;
        }
        return Ok(true);
    }
    if output.exists() {
        if output.is_dir() {
            std::fs::remove_dir_all(&output)?;
        } else {
            std::fs::remove_file(&output)?;
        }
    }
    if marker.exists() {
        std::fs::remove_file(marker)?;
    }
    sync_directory(root)?;
    Ok(false)
}

fn archive_records_are_readable(path: &Path) -> bool {
    let Ok(mut reader) = ArchiveRecordReader::open(path) else {
        return false;
    };
    loop {
        match reader.next_record() {
            Ok(Some(_)) => {}
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

pub fn mirror_build_progress(archive: impl AsRef<Path>) -> Option<MirrorBuildProgress> {
    let root = crate::cli::mirror_scratch_path(archive.as_ref());
    let plan = read_direct_build_plan(&root.join("plan.json")).ok()?;
    let total = plan.target_count() as u64;
    if root.join("archive.complete").exists() {
        return Some(MirrorBuildProgress {
            phase: "indexing".into(),
            targets_total: total,
            targets_completed: total,
            source_bytes_total: plan.source_bytes(),
            source_bytes_completed: plan.source_bytes(),
            snapshot: plan.content_snapshot,
            ..Default::default()
        });
    }
    let mut completed = 0_u64;
    let mut completed_bytes = 0_u64;
    for (kind, count) in [
        ("content", plan.content_groups.len()),
        ("history", plan.history_files.len()),
    ] {
        for index in 0..count {
            if node_path(&root, kind, index).join("receipt.json").is_file() {
                completed += 1;
                completed_bytes =
                    completed_bytes.saturating_add(plan.target_source_bytes(kind, index));
            }
        }
    }
    let mut active = std::fs::read_dir(root.join("nodes"))
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with('.')
                .then(|| {
                    name.trim_start_matches('.')
                        .split('.')
                        .next()
                        .unwrap_or(&name)
                        .to_owned()
                })
        })
        .collect::<Vec<_>>();
    active.sort();
    active.dedup();
    Some(MirrorBuildProgress {
        phase: if !active.is_empty() || completed < total {
            "fetching and parsing".into()
        } else if root.join("stage2.mk").exists() {
            "assembling".into()
        } else {
            "preparing assembly".into()
        },
        targets_total: total,
        targets_completed: completed,
        targets_active: active,
        source_bytes_total: plan.source_bytes(),
        source_bytes_completed: completed_bytes,
        snapshot: plan.content_snapshot,
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn publish_node(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: &str,
    index: usize,
    temporary: &Path,
) -> Result<()> {
    let data = temporary.join("data.swdump");
    let data_bytes = std::fs::metadata(&data)?.len();
    std::fs::File::open(&data)?.sync_all()?;
    if temporary.join("siteinfo.swdump").exists() {
        std::fs::File::open(temporary.join("siteinfo.swdump"))?.sync_all()?;
    }
    let receipt = BuildReceipt {
        plan_id: plan.plan_id.clone(),
        kind: kind.to_owned(),
        index,
        data_bytes,
    };
    let receipt_path = temporary.join("receipt.json");
    {
        let mut output = std::fs::File::create(&receipt_path)?;
        serde_json::to_writer(&mut output, &receipt)
            .map_err(|_| Error::Corrupt("cannot encode build receipt"))?;
        output.write_all(b"\n")?;
        output.sync_all()?;
    }
    sync_directory(temporary)?;
    let destination = node_path(root, kind, index);
    std::fs::rename(temporary, &destination)?;
    sync_directory(&root.join("nodes"))?;
    Ok(())
}

pub(crate) fn materialize_direct_build_node(
    client: &Client,
    root: &Path,
    plan: &DirectBuildPlan,
    kind: &str,
    index: usize,
    bz2_workers: usize,
    progress: &(impl Fn(&str) + Sync),
) -> Result<()> {
    if validate_node(root, plan, kind, index)? {
        progress(&format!("reusing {kind} target {}/{}", index + 1, plan.target_count()));
        return Ok(());
    }
    let temporary = root.join("nodes").join(format!(
        ".{kind}-{index:06}.{}.partial",
        std::process::id()
    ));
    if temporary.exists() {
        std::fs::remove_dir_all(&temporary)?;
    }
    std::fs::create_dir(&temporary)?;
    let result = match kind {
        "content" => {
            let parts = plan
                .content_groups
                .get(index)
                .ok_or(Error::Corrupt("content target is outside build plan"))?
                .iter()
                .map(plan_part)
                .collect::<Vec<_>>();
            let built = build_content_group(
                client,
                &parts,
                0,
                &temporary,
                bz2_workers,
                plan.observed_at_micros,
                progress,
            )?;
            std::fs::rename(built.path, temporary.join("data.swdump"))?;
            if index == 0 {
                let site_info = built
                    .site_info
                    .ok_or(Error::Corrupt("first content target has no siteinfo"))?;
                let siteinfo_path = temporary.join("siteinfo.swdump");
                let mut writer = ArchiveWriter::new(
                    std::fs::File::create(&siteinfo_path)?,
                    DEFAULT_FRAME_TARGET,
                )
                .map_err(map_archive)?;
                writer
                    .write(&Record::SiteInfo {
                        timestamp_micros: plan.observed_at_micros,
                        site_info,
                    })
                    .map_err(map_archive)?;
                writer.finish().map_err(map_archive)?;
            }
            Ok(())
        }
        "history" => {
            let file = planned_history(
                plan.history_files
                    .get(index)
                    .ok_or(Error::Corrupt("history target is outside build plan"))?,
            );
            let cancelled = Arc::new(AtomicBool::new(false));
            let (path, _) = build_history_part(
                client,
                &plan.wiki_db,
                &file,
                index,
                &temporary,
                bz2_workers,
                cancelled,
            )?;
            std::fs::rename(path, temporary.join("data.swdump"))?;
            Ok(())
        }
        _ => Err(Error::Corrupt("unknown direct build target kind")),
    };
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&temporary);
        return Err(error);
    }
    publish_node(root, plan, kind, index, &temporary)?;
    progress(&format!("finished {kind} target {}", index + 1));
    Ok(())
}

pub(crate) fn assemble_direct_build(
    root: &Path,
    plan: &DirectBuildPlan,
    progress: &(impl Fn(&str) + Sync),
) -> Result<PathBuf> {
    let output = root.join("archive.swdump");
    if recover_direct_build_completion(root, plan)? {
        return Ok(output);
    }
    for (kind, count) in [
        ("content", plan.content_groups.len()),
        ("history", plan.history_files.len()),
    ] {
        for index in 0..count {
            if !validate_node(root, plan, kind, index)? {
                return Err(Error::Corrupt("direct build input target is incomplete"));
            }
        }
    }
    let manifest_archive = root.join("manifest.swdump");
    let mut manifest_writer = ArchiveWriter::new(
        std::fs::File::create(&manifest_archive)?,
        DEFAULT_FRAME_TARGET,
    )
    .map_err(map_archive)?;
    let mut source_files = plan
        .content_groups
        .iter()
        .flatten()
        .map(|part| part.filename.clone())
        .chain(
            plan.history_files
                .iter()
                .map(|file| file.part.filename.clone()),
        )
        .collect::<Vec<_>>();
    source_files.sort();
    manifest_writer
        .write(&Record::Manifest {
            timestamp_micros: plan.observed_at_micros,
            manifest: ManifestRecord {
                wiki_db: plan.wiki_db.clone(),
                content_snapshot: plan.content_snapshot.clone(),
                metadata_snapshot: plan.metadata_snapshot.clone(),
                source_files,
            },
        })
        .map_err(map_archive)?;
    manifest_writer.finish().map_err(map_archive)?;

    let mut inputs = (0..plan.content_groups.len())
        .map(|index| node_path(root, "content", index).join("data.swdump"))
        .chain(
            (0..plan.history_files.len())
                .map(|index| node_path(root, "history", index).join("data.swdump")),
        )
        .collect::<Vec<_>>();
    inputs.push(node_path(root, "content", 0).join("siteinfo.swdump"));
    inputs.push(manifest_archive.clone());
    progress("assembling durable page-ID range files");
    let temporary = crate::archive_set::ArchiveSetOutput::new_in(
        root,
        plan.range_target,
    )
    .map_err(map_archive)?;
    let bootstrap = tempfile::tempfile_in(root)?;
    let (file, _, _, _) = crate::archive::merge_many_archives_bootstrapping_ref_prefix(
        &inputs,
        temporary,
        bootstrap,
        plan.frame_target,
        CompressionSettings {
            level: plan.compression_level,
            ..CompressionSettings::default()
        },
        plan.ref_prefix_sample_bytes,
        plan.ref_prefix_bytes,
    )
    .map_err(map_archive)?;
    let completed = file.finish().map_err(map_archive)?;
    completed.persist(&output).map_err(map_archive)?;
    sync_directory(&output)?;
    persist_completion_marker(root, plan)?;

    for kind in ["content", "history"] {
        let count = if kind == "content" {
            plan.content_groups.len()
        } else {
            plan.history_files.len()
        };
        for index in 0..count {
            std::fs::remove_dir_all(node_path(root, kind, index))?;
        }
    }
    std::fs::remove_file(manifest_archive)?;
    sync_directory(&root.join("nodes"))?;
    progress("final range files are durable; consumed source targets removed");
    Ok(output)
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

struct CancelReader<R> {
    inner: R,
    cancelled: Arc<AtomicBool>,
}

impl<R: Read> Read for CancelReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "history import cancelled after another file failed",
            ));
        }
        self.inner.read(output)
    }
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
    let scratch = scratch_parent.as_ref();
    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let path = scratch.to_path_buf();
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
        scratch,
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

pub(crate) fn update_checkpoint_key(
    path: &Path,
    dbname: &str,
    overlap_days: u64,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<String> {
    let frontier = archive_frontier(path, dbname)?;
    let identity = (
        dbname,
        frontier.content.to_string(),
        frontier.metadata,
        overlap_days,
        frame_target,
        compression.level,
        compression.checksum,
        compression.long_distance_matching,
        compression.window_log,
        compression.target_block_size,
    );
    let bytes = serde_json::to_vec(&identity)
        .map_err(|_| Error::Corrupt("cannot encode update checkpoint identity"))?;
    use sha1::Digest;
    Ok(hex::encode(sha1::Sha1::digest(bytes)))
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
        std::fs::create_dir_all(&run_scratch)?;
        content_results.extend(build_content_parts(
            client,
            &run.parts,
            &run_scratch,
            cores,
            snapshot_date_micros(run.date)?,
            progress,
        )?);
    }

    progress("discovering MediaWiki History partitions");
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
    sync_directory(output_parent)?;
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
    progress("discovering upstream content and MediaWiki History files");
    let content_run = wikimak_mediawiki::discover_with(client, config, dbname)?;
    let (history_snapshot, history_files) =
        crate::sync::discover_history(client, config, dbname)?;
    let content_bytes = content_run
        .parts
        .iter()
        .map(|part| part.size_bytes)
        .sum::<u64>();
    let history_bytes = history_files
        .iter()
        .map(|file| file.part.size_bytes)
        .sum::<u64>();
    let unknown_history_sizes = history_files
        .iter()
        .filter(|file| file.part.size_bytes == 0)
        .count();
    progress(&format!(
        "discovered {} content files ({} bytes) and {} history files ({})",
        content_run.parts.len(),
        content_bytes,
        history_files.len(),
        byte_size_summary(history_bytes, unknown_history_sizes),
    ));
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
    let temporary = crate::archive_set::ArchiveSetOutput::new_in(
        output_parent,
        crate::archive_set::DEFAULT_RANGE_TARGET,
    )
    .map_err(map_archive)?;
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
    let completed_content_bytes = Arc::new(AtomicU64::new(0));
    let completed_content_groups = Arc::new(AtomicU64::new(0));
    let (sender, receiver) = std::sync::mpsc::sync_channel(workers);
    let content_stats = Arc::new(Mutex::new(ContentStreamStats::default()));
    let observed_at_micros = snapshot_date_micros(content_run.date)?;
    let (file, output_frames) = std::thread::scope(|scope| -> Result<_> {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let failed = Arc::clone(&failed);
            let completed_content_bytes = Arc::clone(&completed_content_bytes);
            let completed_content_groups = Arc::clone(&completed_content_groups);
            let sender = sender.clone();
            scope.spawn(move || loop {
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                let Some((index, group)) = queue.lock().expect("queue mutex").pop_front() else {
                    return;
                };
                let group_bytes = group.iter().map(|part| part.size_bytes).sum::<u64>();
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
                } else {
                    let completed = completed_content_bytes
                        .fetch_add(group_bytes, Ordering::Relaxed)
                        .saturating_add(group_bytes);
                    let completed_groups = completed_content_groups
                        .fetch_add(1, Ordering::Relaxed)
                        .saturating_add(1);
                    progress(&format!(
                        "finished content group {}/{}; {}",
                        completed_groups,
                        group_count,
                        byte_progress(completed, content_bytes)
                    ));
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
    let completed = file.finish().map_err(map_archive)?;
    let output_bytes = completed.virtual_bytes;
    completed.persist(output).map_err(map_archive)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755))?;
    }

    let content_stats = content_stats.lock().expect("content stats mutex");
    let mut stats = DirectArchiveStats {
        content_parts: content_run.parts.len() as u64,
        history_parts: history_files.len() as u64,
        content_archive_bytes: content_stats.bytes,
        history_archive_bytes,
        output_bytes,
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
    let total_bytes = files.iter().map(|file| file.part.size_bytes).sum::<u64>();
    let mut pending = Vec::new();
    let mut reused = Vec::new();
    let mut reused_bytes = 0_u64;
    for (index, file) in files.iter().cloned().enumerate() {
        let path = scratch.join(format!("history-{index:06}.swdump"));
        let key = checkpoint_key("history", 0, [PlannedPart::from(&file.part)])?;
        if let Some(stats) = checkpoint_stats(&path, &key) {
            reused_bytes = reused_bytes.saturating_add(file.part.size_bytes);
            reused.push((index, (path, stats)));
        } else {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            let receipt = checkpoint_receipt_path(&path);
            if receipt.exists() {
                std::fs::remove_file(receipt)?;
            }
            pending.push((index, file, key));
        }
    }
    let workers = pending.len().min(cores).min(3).max(1);
    let bz2_workers = (cores / workers).max(1);
    let queue = Arc::new(Mutex::new(VecDeque::from(pending)));
    let results = Arc::new(Mutex::new(reused));
    let failure = Arc::new(Mutex::new(None));
    let cancelled = Arc::new(AtomicBool::new(false));
    let completed_bytes = Arc::new(AtomicU64::new(reused_bytes));
    let completed_files = Arc::new(AtomicU64::new(
        results.lock().expect("results mutex").len() as u64,
    ));
    let unknown_sizes = files
        .iter()
        .filter(|file| file.part.size_bytes == 0)
        .count();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let failure = Arc::clone(&failure);
            let cancelled = Arc::clone(&cancelled);
            let completed_bytes = Arc::clone(&completed_bytes);
            let completed_files = Arc::clone(&completed_files);
            scope.spawn(move || loop {
                if cancelled.load(Ordering::Relaxed) {
                    return;
                }
                let Some((index, file, key)) = queue.lock().expect("queue mutex").pop_front() else {
                    return;
                };
                progress(&format!("history {}", file.part.filename));
                match build_history_part(
                    client,
                    dbname,
                    &file,
                    index,
                    scratch,
                    bz2_workers,
                    Arc::clone(&cancelled),
                ) {
                    Ok(result) => {
                        if let Err(error) =
                            write_checkpoint_receipt(&result.0, &key, &result.1)
                        {
                            if !cancelled.swap(true, Ordering::Relaxed) {
                                *failure.lock().expect("failure mutex") = Some(error);
                            }
                            return;
                        }
                        let completed = completed_bytes
                            .fetch_add(file.part.size_bytes, Ordering::Relaxed)
                            .saturating_add(file.part.size_bytes);
                        let files_done = completed_files
                            .fetch_add(1, Ordering::Relaxed)
                            .saturating_add(1);
                        progress(&format!(
                            "finished history {}; {}",
                            file.part.filename,
                            history_progress(
                                files_done,
                                files.len() as u64,
                                completed,
                                total_bytes,
                                unknown_sizes,
                            )
                        ));
                        results.lock().expect("results mutex").push((index, result));
                    }
                    Err(error) => {
                        if !cancelled.swap(true, Ordering::Relaxed) {
                            *failure.lock().expect("failure mutex") = Some(error);
                        }
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
    cancelled: Arc<AtomicBool>,
) -> Result<(PathBuf, PartialStats)> {
    let source = wikimak_mediawiki::fetch(client, &file.part)?;
    let decoder = wikimak_mediawiki::new_bz2_reader(
        CancelReader {
            inner: source,
            cancelled,
        },
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
    let total_groups = groups.len();
    let total_bytes = parts.iter().map(|part| part.size_bytes).sum::<u64>();
    let mut pending = Vec::new();
    let mut reused = Vec::new();
    let mut reused_bytes = 0_u64;
    for (index, group) in groups.into_iter().enumerate() {
        let path = scratch.join(format!("content-{index:06}.swdump"));
        let key = checkpoint_key(
            "content",
            observed_at_micros,
            group.iter().map(PlannedPart::from),
        )?;
        let reusable = checkpoint_stats(&path, &key)
            .map(|stats| read_site_info_checkpoint(&path).map(|site_info| (stats, site_info)))
            .transpose();
        if let Ok(Some((stats, site_info))) = reusable {
            reused_bytes = reused_bytes
                .saturating_add(group.iter().map(|part| part.size_bytes).sum::<u64>());
            reused.push((
                index,
                ContentPartResult {
                    path,
                    stats,
                    site_info,
                },
            ));
        } else {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            let receipt = checkpoint_receipt_path(&path);
            if receipt.exists() {
                std::fs::remove_file(receipt)?;
            }
            let site_info = site_info_checkpoint_path(&path);
            if site_info.exists() {
                std::fs::remove_file(site_info)?;
            }
            pending.push((index, group, key));
        }
    }
    let workers = pending.len().min(cores).min(3).max(1);
    let bz2_workers = (cores / workers).max(1);
    let queue = Arc::new(Mutex::new(VecDeque::from(pending)));
    let results = Arc::new(Mutex::new(reused));
    let failure = Arc::new(Mutex::new(None));
    let completed_bytes = Arc::new(AtomicU64::new(reused_bytes));
    let completed_groups = Arc::new(AtomicU64::new(
        results.lock().expect("results mutex").len() as u64,
    ));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let failure = Arc::clone(&failure);
            let completed_bytes = Arc::clone(&completed_bytes);
            let completed_groups = Arc::clone(&completed_groups);
            scope.spawn(move || loop {
                if failure.lock().expect("failure mutex").is_some() {
                    return;
                }
                let Some((index, group, key)) = queue.lock().expect("queue mutex").pop_front() else {
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
                    Ok(result) => {
                        if let Err(error) = write_site_info_checkpoint(
                            &result.path,
                            observed_at_micros,
                            result.site_info.as_ref(),
                        ) {
                            *failure.lock().expect("failure mutex") = Some(error);
                            return;
                        }
                        if let Err(error) =
                            write_checkpoint_receipt(&result.path, &key, &result.stats)
                        {
                            *failure.lock().expect("failure mutex") = Some(error);
                            return;
                        }
                        let bytes = group.iter().map(|part| part.size_bytes).sum::<u64>();
                        let completed = completed_bytes
                            .fetch_add(bytes, Ordering::Relaxed)
                            .saturating_add(bytes);
                        let completed_groups = completed_groups
                            .fetch_add(1, Ordering::Relaxed)
                            .saturating_add(1);
                        progress(&format!(
                            "finished content group {}/{}; {}",
                            completed_groups,
                            total_groups,
                            byte_progress(completed, total_bytes)
                        ));
                        results.lock().expect("results mutex").push((index, result));
                    }
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

fn byte_progress(completed: u64, total: u64) -> String {
    if total == 0 {
        return "byte size unknown".into();
    }
    let percent = completed.saturating_mul(100).checked_div(total).unwrap_or(0);
    format!("{completed}/{total} bytes ({percent}%)")
}

fn byte_size_summary(known_bytes: u64, unknown_sizes: usize) -> String {
    match (known_bytes, unknown_sizes) {
        (bytes, 0) => format!("{bytes} bytes"),
        (0, unknown) => format!("byte sizes unknown for {unknown}"),
        (bytes, unknown) => format!("{bytes} known bytes; {unknown} sizes unknown"),
    }
}

fn history_progress(
    files_done: u64,
    total_files: u64,
    completed_bytes: u64,
    total_bytes: u64,
    unknown_sizes: usize,
) -> String {
    if unknown_sizes == 0 {
        format!(
            "{files_done}/{total_files} files; {}",
            byte_progress(completed_bytes, total_bytes)
        )
    } else if total_bytes == 0 {
        format!("{files_done}/{total_files} files; byte sizes unknown")
    } else {
        format!(
            "{files_done}/{total_files} files; {completed_bytes}/{total_bytes} known bytes \
             ({unknown_sizes} sizes unknown)"
        )
    }
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

fn archive_file_complete(path: &Path) -> bool {
    crate::archive::index_file(path)
        .is_ok_and(|(_, _, complete)| complete)
}

fn checkpoint_receipt_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".receipt");
    PathBuf::from(name)
}

fn site_info_checkpoint_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".siteinfo");
    PathBuf::from(name)
}

fn read_site_info_checkpoint(path: &Path) -> Result<Option<SiteInfoRecord>> {
    let checkpoint = site_info_checkpoint_path(path);
    if !checkpoint.exists() {
        return Ok(None);
    }
    let mut reader = ArchiveRecordReader::open(checkpoint).map_err(map_archive)?;
    let site_info = match reader.next_record().map_err(map_archive)? {
        Some(Record::SiteInfo { site_info, .. }) => site_info,
        _ => return Err(Error::Corrupt("invalid siteinfo checkpoint")),
    };
    if reader.next_record().map_err(map_archive)?.is_some() {
        return Err(Error::Corrupt("siteinfo checkpoint has extra records"));
    }
    Ok(Some(site_info))
}

fn write_site_info_checkpoint(
    path: &Path,
    timestamp_micros: i64,
    site_info: Option<&SiteInfoRecord>,
) -> Result<()> {
    let checkpoint = site_info_checkpoint_path(path);
    if let Some(site_info) = site_info {
        let mut temporary = tempfile::NamedTempFile::new_in(
            checkpoint.parent().unwrap_or_else(|| Path::new(".")),
        )?;
        let mut writer = ArchiveWriter::new(temporary.as_file_mut(), DEFAULT_FRAME_TARGET)
            .map_err(map_archive)?;
        writer
            .write(&Record::SiteInfo {
                timestamp_micros,
                site_info: site_info.clone(),
            })
            .map_err(map_archive)?;
        writer.finish().map_err(map_archive)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&checkpoint)
            .map_err(|error| Error::Io(error.error))?;
        sync_directory(checkpoint.parent().unwrap_or_else(|| Path::new(".")))?;
    } else if checkpoint.exists() {
        std::fs::remove_file(checkpoint)?;
    }
    Ok(())
}

fn checkpoint_key(
    kind: &str,
    observed_at_micros: i64,
    parts: impl IntoIterator<Item = PlannedPart>,
) -> Result<String> {
    let value = (kind, observed_at_micros, parts.into_iter().collect::<Vec<_>>());
    let bytes =
        serde_json::to_vec(&value).map_err(|_| Error::Corrupt("cannot encode checkpoint key"))?;
    use sha1::Digest;
    Ok(hex::encode(sha1::Sha1::digest(bytes)))
}

fn checkpoint_stats(path: &Path, key: &str) -> Option<PartialStats> {
    if !archive_file_complete(path) {
        return None;
    }
    let receipt = std::fs::read(checkpoint_receipt_path(path)).ok()?;
    let receipt: PartCheckpointReceipt = serde_json::from_slice(&receipt).ok()?;
    (receipt.schema == 1 && receipt.key == key).then_some(receipt.stats)
}

fn write_checkpoint_receipt(path: &Path, key: &str, stats: &PartialStats) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    let receipt = checkpoint_receipt_path(path);
    let parent = receipt
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
    serde_json::to_writer(
        &mut temporary,
        &PartCheckpointReceipt {
            schema: 1,
            key: key.to_owned(),
            stats: stats.clone(),
        },
    )
    .map_err(|_| Error::Corrupt("cannot encode checkpoint receipt"))?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(receipt)
        .map_err(|error| Error::Io(error.error))?;
    sync_directory(&parent)
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

#[cfg(test)]
mod build_graph_tests {
    use httpmock::Method::GET;
    use httpmock::MockServer;

    use super::*;

    #[test]
    fn durable_target_is_reused_and_removed_only_after_final_generation() {
        let server = MockServer::start();
        let content = include_bytes!("../tests/data/export_three_pages.xml");
        let source = server.mock(|when, then| {
            when.method(GET).path("/content.xml");
            then.status(200).body(content);
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nodes")).unwrap();
        let mut plan = DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
            wiki_db: "testwiki".into(),
            content_snapshot: "2024-06-01".into(),
            metadata_snapshot: "2024-06".into(),
            observed_at_micros: snapshot_date_micros(
                chrono::NaiveDate::from_ymd_opt(2024, 6, 1).unwrap(),
            )
            .unwrap(),
            frame_target: MIRROR_FRAME_TARGET,
            range_target: crate::archive_set::DEFAULT_RANGE_TARGET,
            compression_level: 9,
            ref_prefix_sample_bytes: MIRROR_REF_PREFIX_SAMPLE_BYTES,
            ref_prefix_bytes: MIRROR_REF_PREFIX_BYTES,
            content_groups: vec![vec![PlannedPart {
                url: server.url("/content.xml"),
                filename: "content.xml".into(),
                size_bytes: content.len() as u64,
                sha256: None,
                sha1: None,
                md5: None,
            }]],
            history_files: vec![],
        };
        plan.plan_id = direct_plan_id(&plan).unwrap();
        let client = Client::new();

        materialize_direct_build_node(
            &client,
            root.path(),
            &plan,
            "content",
            0,
            1,
            &|_| {},
        )
        .unwrap();
        assert_eq!(source.hits(), 1);
        assert!(validate_node(root.path(), &plan, "content", 0).unwrap());

        materialize_direct_build_node(
            &client,
            root.path(),
            &plan,
            "content",
            0,
            1,
            &|_| {},
        )
        .unwrap();
        assert_eq!(source.hits(), 1, "durable target was fetched again");

        let archive = assemble_direct_build(root.path(), &plan, &|_| {}).unwrap();
        crate::archive_set::ArchiveSetReader::open(archive).unwrap();
        assert!(root.path().join("archive.complete").exists());
        assert!(
            !node_path(root.path(), "content", 0).exists(),
            "consumed target survived its durable replacement"
        );
        std::fs::remove_file(root.path().join("archive.complete")).unwrap();
        assert!(recover_direct_build_completion(root.path(), &plan).unwrap());
        assert_eq!(
            std::fs::read_to_string(root.path().join("archive.complete"))
                .unwrap()
                .trim_end(),
            plan.plan_id
        );
    }
}
