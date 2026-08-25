//! Direct upstream-dump to portable-archive construction.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::archive::{
    ArchiveError, ArchiveRecordReader, ArchiveWriter, CompressionSettings, EntityKey, EntityKind,
    FrameInfo, ManifestRecord, ParallelArchiveWriter, Record, RecordSorter, RecordSource,
    RevisionRecord, SiteInfoRecord, SiteInterwikiRecord, SiteNamespaceRecord,
    StreamingFrameOutput,
    DEFAULT_FRAME_TARGET, MIRROR_FRAME_TARGET, MIRROR_REF_PREFIX_BYTES,
    MIRROR_REF_PREFIX_SAMPLE_BYTES,
};
use crate::instance::{ContributorMeta, RevisionMeta};
use crate::{Error, Result};

const HISTORY_SORT_RUN_TARGET: usize = 8 << 30;

/// Destination-local exclusion held by a full/import build or by engine
/// deletion while it is claiming scratch candidates. The public cleanup
/// acquisition is deliberately nonblocking: `Ok(None)` means an importer
/// currently owns the lock, and callers must fail before touching the tree.
pub struct MirrorBuildWriterCleanupLease {
    file: Option<std::fs::File>,
}

struct LocalHistoryMaterializationLocks {
    held: BTreeSet<PathBuf>,
}

struct LocalHistoryMaterializationLease {
    key: PathBuf,
}

static LOCAL_HISTORY_MATERIALIZATION_LOCKS:
    OnceLock<(Mutex<LocalHistoryMaterializationLocks>, Condvar)> = OnceLock::new();

impl Drop for LocalHistoryMaterializationLease {
    fn drop(&mut self) {
        let (state, wake) = LOCAL_HISTORY_MATERIALIZATION_LOCKS.get_or_init(|| {
            (
                Mutex::new(LocalHistoryMaterializationLocks {
                    held: BTreeSet::new(),
                }),
                Condvar::new(),
            )
        });
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.held.remove(&self.key);
        wake.notify_all();
    }
}

struct HistoryMaterializationLease {
    _file: std::fs::File,
    _local: LocalHistoryMaterializationLease,
}

fn acquire_history_materialization_lease(root: &Path) -> Result<HistoryMaterializationLease> {
    let lock_directory = root.join("target-logs");
    match std::fs::symlink_metadata(&lock_directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(Error::Corrupt("history materialization lock directory is not real")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(&lock_directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(Error::Io(error)),
            }
            if !std::fs::symlink_metadata(&lock_directory)?.file_type().is_dir() {
                return Err(Error::Corrupt(
                    "history materialization lock directory is not real",
                ));
            }
        }
        Err(error) => return Err(Error::Io(error)),
    }
    let lock_path = lock_directory.join("history-materialization.lock");
    let key = std::fs::canonicalize(&lock_directory)
        .unwrap_or_else(|_| lock_directory.clone())
        .join("history-materialization.lock");
    let (state, wake) = LOCAL_HISTORY_MATERIALIZATION_LOCKS.get_or_init(|| {
        (
            Mutex::new(LocalHistoryMaterializationLocks {
                held: BTreeSet::new(),
            }),
            Condvar::new(),
        )
    });
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !state.held.insert(key.clone()) {
        state = wake
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    drop(state);
    let local = LocalHistoryMaterializationLease { key };

    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&lock_path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(Error::Corrupt("history materialization lock is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
    }
    #[cfg(not(unix))]
    {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "history materialization lock has no supported platform backend",
        )));
    }
    Ok(HistoryMaterializationLease {
        _file: file,
        _local: local,
    })
}

impl Drop for MirrorBuildWriterCleanupLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(file) = &self.file {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

/// Acquire the importer/deletion exclusion without creating any destination
/// state. A missing scratch namespace means no importer can own its lock and
/// returns a no-op guard. An existing namespace without a regular build.lock
/// fails closed because exclusion cannot be established.
pub fn try_acquire_mirror_build_writer_cleanup_lease(
    scratch: &Path,
) -> std::io::Result<Option<MirrorBuildWriterCleanupLease>> {
    let scratch_metadata = match std::fs::symlink_metadata(scratch) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(MirrorBuildWriterCleanupLease { file: None }));
        }
        Err(error) => return Err(error),
    };
    if !scratch_metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("mirror scratch is not a real directory: {}", scratch.display()),
        ));
    }
    #[cfg(not(unix))]
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "mirror writer exclusion has no supported platform lock backend",
        ));
    }
    let lock_path = scratch.join("build.lock");
    let lock_metadata = std::fs::symlink_metadata(&lock_path)?;
    if !lock_metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("mirror build lock is not a regular file: {}", lock_path.display()),
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(lock_path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            ) {
                return Ok(None);
            }
            return Err(error);
        }
    }
    Ok(Some(MirrorBuildWriterCleanupLease { file: Some(file) }))
}

fn acquire_mirror_build_writer_lease(
    scratch: &Path,
) -> std::io::Result<MirrorBuildWriterCleanupLease> {
    #[cfg(not(unix))]
    {
        let _ = scratch;
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "mirror writer exclusion has no supported platform lock backend",
        ));
    }
    match std::fs::symlink_metadata(scratch) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("mirror scratch is not a real directory: {}", scratch.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(scratch)?;
            if !std::fs::symlink_metadata(scratch)?.file_type().is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("mirror scratch is not a real directory: {}", scratch.display()),
                ));
            }
        }
        Err(error) => return Err(error),
    }
    let lock_path = scratch.join("build.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&lock_path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("mirror build lock is not a regular file: {}", lock_path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(MirrorBuildWriterCleanupLease { file: Some(file) })
}

pub(crate) fn acquire_mirror_build_writer_for_cli(
    scratch: &Path,
) -> std::io::Result<MirrorBuildWriterCleanupLease> {
    acquire_mirror_build_writer_lease(scratch)
}

/// Select the nominal physical range threshold for a newly discovered plan.
///
/// `source_bytes` is the size of upstream compressed inputs, not the size of
/// the resulting archive-set stream. It therefore must not be used to inflate
/// the physical range threshold: a large Enwiki input would otherwise produce
/// unusably large HDD update pieces. The archive-set writer seals only between
/// complete entity frames, so a resulting piece may overshoot this threshold
/// by the frame that crosses it (or by more when one frame is larger than the
/// threshold). Existing persisted plans retain their own `range_target` and
/// identity when read; [`assembly_range_target`] applies the same operational
/// ceiling when their final archive has not yet been published.
fn planned_range_layout(_source_bytes: u64) -> u64 {
    crate::archive_set::DEFAULT_RANGE_TARGET
}

/// Operational range threshold used by the current assembly implementation.
///
/// Early schema-1 plans derived this field from upstream compressed bytes and
/// could record a threshold hundreds of GiB wide. Rewriting such a plan would
/// change its identity and invalidate every expensive completed source target.
/// Preserve that durable evidence, but treat the recorded threshold as a
/// request subject to the current 1-GiB updateability ceiling. This changes no
/// target materialization and lets an unassembled legacy build retain all of
/// its completed nodes while producing pieces the phased HDD updater can
/// preload. A resumed assembly can contain an older sealed prefix; its actual
/// segment sizes remain explicit in the assembly checkpoint and the updater's
/// hard base-plus-tail admission check still fails before mutation if any pair
/// exceeds its configured RAM bound.
fn assembly_range_target(plan: &DirectBuildPlan) -> u64 {
    plan.range_target
        .min(crate::archive_set::DEFAULT_RANGE_TARGET)
}

/// Bound direct-build reference-prefix selection without dictionary search.
///
/// A fixed 16 MiB FastCover output target made dictionary search consume
/// hundreds of MiB even for a 6.7 MiB public source, and the resulting stored
/// prefix was nevertheless valuable: reducing it to 105 KiB made the public
/// votewiki page segment grow from about 480 KiB to 6.3 MiB. New plans
/// therefore retain at most 16 MiB of newest-revision text and use all of it
/// directly as the reference prefix. Because sample bytes never exceed prefix
/// capacity, `distill_plan_ref_prefix` takes its deterministic concatenation
/// path and does not invoke FastCover. The exact values remain persisted in
/// the durable plan; an older plan with the previous 150/16 MiB geometry keeps
/// that geometry when resumed.
fn planned_ref_prefix_layout() -> (usize, usize) {
    (MIRROR_REF_PREFIX_BYTES, MIRROR_REF_PREFIX_BYTES)
}

pub(crate) fn processing_parallelism() -> usize {
    std::env::var("SARUN_WIKIMAK_CPU_BUDGET")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from))
}

/// Return the source counts consumed at each materialized merge level.
///
/// Every entry is one merge batch.  The final entry describes the source
/// count passed to the last in-memory merge.  This is deliberately shared by
/// the implementation and its integration test so the tested decomposition
/// is not a second copy of the batching algorithm.
#[doc(hidden)]
pub fn bounded_merge_batch_sizes(mut source_count: usize) -> Vec<Vec<usize>> {
    if source_count == 0 {
        return vec![Vec::new()];
    }
    let mut levels = Vec::new();
    while source_count > crate::archive::MAX_SORTED_MERGE_FAN_IN {
        let mut batches = Vec::new();
        let mut remaining = source_count;
        while remaining != 0 {
            let batch = remaining.min(crate::archive::MAX_SORTED_MERGE_FAN_IN);
            batches.push(batch);
            remaining -= batch;
        }
        source_count = batches.len();
        levels.push(batches);
    }
    levels.push(vec![source_count]);
    levels
}

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

/// Checkpoint receipts can outlive the fields collected by the current build.
/// Keep serde's default unknown-field behavior: old page-revision telemetry is
/// ignored on read and is not emitted by new receipts.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PartialStats {
    pages: u64,
    revisions: u64,
    events: u64,
    page_events: u64,
    user_events: u64,
    global_events: u64,
    #[serde(default)]
    fetch_attempts: u64,
    #[serde(default)]
    fetch_bytes_received: u64,
    #[serde(default)]
    fetch_rate_limit_responses: u64,
    #[serde(default)]
    fetch_client_error_responses: u64,
    #[serde(default)]
    fetch_server_error_responses: u64,
    #[serde(default)]
    fetch_transport_errors: u64,
    #[serde(default)]
    fetch_server_timed_retries: u64,
    #[serde(default)]
    fetch_robots_timed_retries: u64,
    #[serde(default)]
    fetch_fallback_timed_retries: u64,
    #[serde(default)]
    fetch_local_spacing_timed_retries: u64,
}

impl PartialStats {
    fn merge_from(&mut self, other: &Self) {
        self.pages = self.pages.saturating_add(other.pages);
        self.revisions = self.revisions.saturating_add(other.revisions);
        self.events = self.events.saturating_add(other.events);
        self.page_events = self.page_events.saturating_add(other.page_events);
        self.user_events = self.user_events.saturating_add(other.user_events);
        self.global_events = self.global_events.saturating_add(other.global_events);
        self.fetch_attempts = self.fetch_attempts.saturating_add(other.fetch_attempts);
        self.fetch_bytes_received = self
            .fetch_bytes_received
            .saturating_add(other.fetch_bytes_received);
        self.fetch_rate_limit_responses = self
            .fetch_rate_limit_responses
            .saturating_add(other.fetch_rate_limit_responses);
        self.fetch_client_error_responses = self
            .fetch_client_error_responses
            .saturating_add(other.fetch_client_error_responses);
        self.fetch_server_error_responses = self
            .fetch_server_error_responses
            .saturating_add(other.fetch_server_error_responses);
        self.fetch_transport_errors = self
            .fetch_transport_errors
            .saturating_add(other.fetch_transport_errors);
        self.fetch_server_timed_retries = self
            .fetch_server_timed_retries
            .saturating_add(other.fetch_server_timed_retries);
        self.fetch_robots_timed_retries = self
            .fetch_robots_timed_retries
            .saturating_add(other.fetch_robots_timed_retries);
        self.fetch_fallback_timed_retries = self
            .fetch_fallback_timed_retries
            .saturating_add(other.fetch_fallback_timed_retries);
        self.fetch_local_spacing_timed_retries = self
            .fetch_local_spacing_timed_retries
            .saturating_add(other.fetch_local_spacing_timed_retries);
    }

    fn record_fetch(&mut self, handle: &wikimak_mediawiki::FetchStatsHandle) {
        if let Ok(fetch) = handle.lock() {
            self.fetch_attempts = fetch.attempts;
            self.fetch_bytes_received = fetch.bytes_received;
            self.fetch_rate_limit_responses = fetch.rate_limit_responses;
            self.fetch_client_error_responses = fetch.client_error_responses;
            self.fetch_server_error_responses = fetch.server_error_responses;
            self.fetch_transport_errors = fetch.transport_errors;
            self.fetch_server_timed_retries = fetch.server_timed_retries;
            self.fetch_robots_timed_retries = fetch.robots_timed_retries;
            self.fetch_fallback_timed_retries = fetch.fallback_timed_retries;
            self.fetch_local_spacing_timed_retries = fetch.local_spacing_timed_retries;
        }
    }
}

#[derive(Deserialize, Serialize)]
struct PartCheckpointReceipt {
    schema: u32,
    key: String,
    stats: PartialStats,
}

struct ContentPartResult {
    path: PathBuf,
    title_path: PathBuf,
    stats: PartialStats,
    site_info: Option<SiteInfoRecord>,
    samples: Option<PathBuf>,
}

const SAMPLE_MAGIC: [u8; 8] = *b"SWSAMPLE";
const SAMPLE_VERSION: u32 = 1;

struct NewestTextSampleWriter {
    output: BufWriter<std::fs::File>,
    remaining: usize,
}

impl NewestTextSampleWriter {
    fn create(path: &Path, capacity: usize) -> Result<Self> {
        let mut output = BufWriter::new(std::fs::File::create(path)?);
        output.write_all(&SAMPLE_MAGIC)?;
        output.write_all(&SAMPLE_VERSION.to_le_bytes())?;
        Ok(Self {
            output,
            remaining: capacity,
        })
    }

    fn push(&mut self, text: &[u8]) -> Result<()> {
        if text.is_empty() || text.len() > self.remaining || text.len() > u32::MAX as usize {
            return Ok(());
        }
        self.output.write_all(&(text.len() as u32).to_le_bytes())?;
        self.output.write_all(text)?;
        self.remaining -= text.len();
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.output.write_all(&u32::MAX.to_le_bytes())?;
        self.output.flush()?;
        self.output.get_ref().sync_all()?;
        Ok(())
    }
}

fn read_text_samples(path: &Path, mut visit: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut input = BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0_u8; 8];
    input.read_exact(&mut magic)?;
    let mut version = [0_u8; 4];
    input.read_exact(&mut version)?;
    if magic != SAMPLE_MAGIC || u32::from_le_bytes(version) != SAMPLE_VERSION {
        return Err(Error::Corrupt("unknown newest-revision sample format"));
    }
    loop {
        let mut length = [0_u8; 4];
        input.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length);
        if length == u32::MAX {
            let mut trailing = [0_u8; 1];
            if input.read(&mut trailing)? != 0 {
                return Err(Error::Corrupt("newest-revision sample has trailing bytes"));
            }
            return Ok(());
        }
        let mut sample = vec![0; length as usize];
        input.read_exact(&mut sample)?;
        visit(&sample)?;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PlannedPart {
    pub(crate) url: String,
    pub(crate) filename: String,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: Option<String>,
    pub(crate) sha1: Option<String>,
    pub(crate) md5: Option<String>,
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
    pub(crate) partition: String,
    pub(crate) part: PlannedPart,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PlannedIncrementalRun {
    pub(crate) date: String,
    pub(crate) parts: Vec<PlannedPart>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) struct PlannedCompression {
    pub(crate) level: i32,
    pub(crate) checksum: bool,
    pub(crate) long_distance_matching: bool,
    pub(crate) window_log: Option<u32>,
    pub(crate) target_block_size: Option<u32>,
    pub(crate) workers: u32,
}

impl From<CompressionSettings> for PlannedCompression {
    fn from(settings: CompressionSettings) -> Self {
        Self {
            level: settings.level,
            checksum: settings.checksum,
            long_distance_matching: settings.long_distance_matching,
            window_log: settings.window_log,
            target_block_size: settings.target_block_size,
            workers: settings.workers,
        }
    }
}

impl From<PlannedCompression> for CompressionSettings {
    fn from(settings: PlannedCompression) -> Self {
        Self {
            level: settings.level,
            checksum: settings.checksum,
            long_distance_matching: settings.long_distance_matching,
            window_log: settings.window_log,
            target_block_size: settings.target_block_size,
            workers: settings.workers,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct UpdateSourcePlan {
    pub(crate) schema: u32,
    pub(crate) source_plan_id: String,
    pub(crate) generation_id: crate::generation::GenerationId,
    pub(crate) base_generation_id: crate::generation::GenerationId,
    pub(crate) wiki_db: String,
    pub(crate) base_content_frontier: String,
    pub(crate) base_metadata_frontier: String,
    pub(crate) overlap_days: u64,
    pub(crate) frame_target: usize,
    pub(crate) compression: PlannedCompression,
    pub(crate) content_runs: Vec<PlannedIncrementalRun>,
    pub(crate) history_snapshot: String,
    pub(crate) history_files: Vec<PlannedHistoryFile>,
    pub(crate) resulting_content_frontier: String,
    pub(crate) resulting_metadata_frontier: String,
}

/// Adapt the immutable incremental source plan to the existing bounded live
/// progress projection.  This is a progress-plan view only: it does not
/// change the durable update plan or its identity.  The materializers use the
/// same `content-{index}`/`history-{index}` target names as the direct builder;
/// a repeated content index across daily runs remains unambiguous because the
/// source filename is part of the slot lookup.
pub(crate) fn incremental_progress_plan(plan: &UpdateSourcePlan) -> DirectBuildPlan {
    let mut content_groups = Vec::new();
    for run in &plan.content_runs {
        let parts = run
            .parts
            .iter()
            .map(wikimak_mediawiki::Part::from)
            .collect::<Vec<_>>();
        for group in crate::sync::part_groups(parts) {
            content_groups.push(group.iter().map(PlannedPart::from).collect());
        }
    }
    DirectBuildPlan {
        schema: 1,
        plan_id: plan.source_plan_id.clone(),
        wiki_db: plan.wiki_db.clone(),
        content_snapshot: plan.resulting_content_frontier.clone(),
        metadata_snapshot: plan.resulting_metadata_frontier.clone(),
        observed_at_micros: 0,
        frame_target: plan.frame_target,
        range_target: crate::archive_set::DEFAULT_RANGE_TARGET,
        compression_level: plan.compression.level,
        ref_prefix_sample_bytes: 0,
        ref_prefix_bytes: 0,
        content_groups,
        history_files: plan.history_files.clone(),
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MirrorBuildProgress {
    pub phase: String,
    pub targets_total: u64,
    pub targets_completed: u64,
    /// Structured rows for the live worker table.  `targets_active` remains
    /// as a compact compatibility projection for the CLI, but consumers that
    /// need attribution must use these rows instead of parsing prose.
    #[serde(default)]
    pub target_progress: Vec<MirrorTargetProgress>,
    pub targets_active: Vec<String>,
    pub source_bytes_total: u64,
    pub source_bytes_completed: u64,
    /// Rate for currently active source readers.  This answers whether the
    /// importer is receiving bytes now, rather than hiding a stalled reader
    /// behind a whole-job average.
    pub active_source_bytes_per_second: Option<u64>,
    /// Age of the most recent observable update across active targets.  The
    /// whole run is quiet only when every active target is quiet.
    pub active_quiet_seconds: Option<u64>,
    /// Network counters reported by the fetcher.  `fetch_bytes_received` is
    /// wire bytes delivered, so a resumed range may count bytes that were
    /// already present in the logical source stream.
    pub fetch_attempts: u64,
    pub fetch_bytes_received: u64,
    pub fetch_rate_limit_responses: u64,
    pub fetch_client_error_responses: u64,
    pub fetch_server_error_responses: u64,
    pub fetch_transport_errors: u64,
    #[serde(default)]
    pub fetch_server_timed_retries: u64,
    #[serde(default)]
    pub fetch_robots_timed_retries: u64,
    #[serde(default)]
    pub fetch_fallback_timed_retries: u64,
    #[serde(default)]
    pub fetch_local_spacing_timed_retries: u64,
    #[serde(default)]
    pub bz2_admission_limit: u64,
    #[serde(default)]
    pub bz2_admission_active_decoders: u64,
    #[serde(default)]
    pub bz2_admission_peak_active_decoders: u64,
    pub snapshot: String,
}

/// One currently materialised source target.  This is deliberately a data
/// record rather than a preformatted status string: the UI can keep the
/// worker identity, phase, counters, and long source/title text separate.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MirrorTargetProgress {
    pub target: String,
    pub kind: String,
    pub phase: String,
    pub source: String,
    pub source_bytes_read: u64,
    pub source_bytes_total: u64,
    #[serde(default)]
    pub decoded_bytes: u64,
    pub bytes_per_second: u64,
    pub pages: u64,
    pub records: u64,
    pub text_bytes: u64,
    pub current_page: u64,
    pub current_title: String,
    pub quiet_seconds: u64,
    pub heartbeat_seconds: u64,
    #[serde(default)]
    pub phase_seconds: u64,
    #[serde(default)]
    pub fetch_attempts: u64,
    #[serde(default)]
    pub fetch_bytes_received: u64,
    #[serde(default)]
    pub fetch_rate_limit_responses: u64,
    #[serde(default)]
    pub fetch_client_error_responses: u64,
    #[serde(default)]
    pub fetch_server_error_responses: u64,
    #[serde(default)]
    pub fetch_transport_errors: u64,
    #[serde(default)]
    pub fetch_server_timed_retries: u64,
    #[serde(default)]
    pub fetch_robots_timed_retries: u64,
    #[serde(default)]
    pub fetch_fallback_timed_retries: u64,
    #[serde(default)]
    pub fetch_local_spacing_timed_retries: u64,
    #[serde(default)]
    pub bz2_admission_limit: u64,
    #[serde(default)]
    pub bz2_admission_active_decoders: u64,
    #[serde(default)]
    pub bz2_admission_peak_active_decoders: u64,
    #[serde(default)]
    pub cpu_user_micros: u64,
    #[serde(default)]
    pub cpu_system_micros: u64,
    #[serde(default)]
    pub peak_rss_bytes: u64,
}

/// Short-lived progress written by an active source-fragment worker. Each
/// source fragment has its own durable target and retry boundary; this
/// sidecar exposes byte and revision progress until its receipt is published.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct LiveTargetProgress {
    pub(crate) target: String,
    pub(crate) part: String,
    pub(crate) phase: String,
    pub(crate) source_bytes_read: u64,
    pub(crate) source_bytes_total: u64,
    #[serde(default)]
    pub(crate) decoded_bytes: u64,
    pub(crate) pages: u64,
    pub(crate) revisions: u64,
    pub(crate) text_bytes: u64,
    pub(crate) current_page: u64,
    pub(crate) current_title: String,
    pub(crate) started_at_micros: u64,
    pub(crate) updated_at_micros: u64,
    /// Last liveness write, kept separate from `updated_at_micros` so a
    /// blocked parser cannot masquerade as making data progress.
    #[serde(default)]
    pub(crate) heartbeat_at_micros: u64,
    #[serde(default)]
    pub(crate) phase_started_at_micros: u64,
    #[serde(default)]
    pub(crate) fetch_attempts: u64,
    #[serde(default)]
    pub(crate) fetch_bytes_received: u64,
    #[serde(default)]
    pub(crate) fetch_rate_limit_responses: u64,
    #[serde(default)]
    pub(crate) fetch_client_error_responses: u64,
    #[serde(default)]
    pub(crate) fetch_server_error_responses: u64,
    #[serde(default)]
    pub(crate) fetch_transport_errors: u64,
    #[serde(default)]
    pub(crate) fetch_server_timed_retries: u64,
    #[serde(default)]
    pub(crate) fetch_robots_timed_retries: u64,
    #[serde(default)]
    pub(crate) fetch_fallback_timed_retries: u64,
    #[serde(default)]
    pub(crate) fetch_local_spacing_timed_retries: u64,
    #[serde(default)]
    pub(crate) bz2_admission_limit: u64,
    #[serde(default)]
    pub(crate) bz2_admission_active_decoders: u64,
    #[serde(default)]
    pub(crate) bz2_admission_peak_active_decoders: u64,
    #[serde(default)]
    pub(crate) cpu_user_micros: u64,
    #[serde(default)]
    pub(crate) cpu_system_micros: u64,
    #[serde(default)]
    pub(crate) peak_rss_bytes: u64,
}

struct LiveProgressState {
    projection: Option<crate::progress_projection::SourceWriter>,
    value: LiveTargetProgress,
    last_write: Instant,
    last_phase: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AssemblyProgressSnapshot {
    plan_id: String,
    run_id: Option<String>,
    pid: u32,
    phase: String,
    input_bytes: u64,
    input_bytes_total: u64,
    output_bytes: u64,
    records: u64,
    current_entity_kind: u64,
    current_entity_id: u64,
    bytes_per_second: u64,
    phase_current: u64,
    phase_total: u64,
    started_at_micros: u64,
    updated_at_micros: u64,
    cpu_user_micros: u64,
    cpu_system_micros: u64,
    peak_rss_bytes: u64,
}

fn write_assembly_progress(root: &Path, value: &AssemblyProgressSnapshot) {
    crate::progress_projection::write_assembly(
        root,
        crate::progress_projection::AssemblyValue {
            plan_id: value.plan_id.clone(),
            run_id: value.run_id.clone(),
            phase: value.phase.clone(),
            input_bytes: value.input_bytes,
            input_bytes_total: value.input_bytes_total,
            output_bytes: value.output_bytes,
            records: value.records,
            current_entity_id: value.current_entity_id,
            bytes_per_second: value.bytes_per_second,
            started_at_micros: value.started_at_micros,
            updated_at_micros: value.updated_at_micros,
            cpu_user_micros: value.cpu_user_micros,
            cpu_system_micros: value.cpu_system_micros,
            peak_rss_bytes: value.peak_rss_bytes,
        },
    );
}

/// Keep a live sidecar fresh while a parser is inside one long blocking read
/// or decompression call. This is deliberately a liveness signal, not fake
/// byte/revision progress; the UI can distinguish the two timestamps.
struct LiveProgressHeartbeat {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Any error after a source reader has been created used to leave its last
/// transient phase behind, making a dead worker look as though it were still
/// downloading or parsing. Keep the sidecar truthful even when `?` returns
/// through a deeply nested decoder/parser call.
struct LiveProgressFailureGuard {
    state: Arc<Mutex<LiveProgressState>>,
}

impl Drop for LiveProgressFailureGuard {
    fn drop(&mut self) {
        let should_mark = self
            .state
            .lock()
            .map(|state| {
                state.value.phase != "finished"
                    && !state.value.phase.to_ascii_lowercase().contains("failed")
            })
            .unwrap_or(false);
        if should_mark {
            set_live_phase(
                &self.state,
                "stopped before completion; inspect attributed target error",
            );
            persist_live_progress(&self.state, true);
        }
    }
}

impl LiveProgressHeartbeat {
    fn start(state: &Arc<Mutex<LiveProgressState>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let state_thread = Arc::clone(state);
        let join = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                for _ in 0..20 {
                    if stop_thread.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                persist_live_heartbeat(&state_thread);
            }
        });
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for LiveProgressHeartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Return the nearest ancestor that actually owns the build plan and shared
/// progress projection. Partial-node names are deliberately irrelevant: the
/// lifecycle has changed that representation before, while these two files
/// are the evidence `SourceWriter` validates and uses.
fn progress_scratch_root(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        let owns_regular_file = |name: &str| {
            std::fs::symlink_metadata(ancestor.join(name))
                .is_ok_and(|metadata| metadata.file_type().is_file())
        };
        if owns_regular_file("plan.json") && owns_regular_file("progress.bin") {
            return ancestor.to_path_buf();
        }
    }
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn persist_live_progress(state: &Arc<Mutex<LiveProgressState>>, force: bool) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    if !force && state.last_write.elapsed() < Duration::from_secs(2) {
        return;
    }
    let now = now_micros();
    if state.last_phase != state.value.phase {
        state.last_phase = state.value.phase.clone();
        state.value.phase_started_at_micros = now;
    } else if state.value.phase_started_at_micros == 0 {
        state.value.phase_started_at_micros = now;
    }
    sample_live_resource_telemetry(&mut state.value);
    state.value.updated_at_micros = now;
    write_live_progress_locked(&mut state);
}

fn sample_live_resource_telemetry(value: &mut LiveTargetProgress) {
    let (user, system, rss) = process_resource_usage();
    value.cpu_user_micros = user;
    value.cpu_system_micros = system;
    value.peak_rss_bytes = rss;
    let admission = wikimak_mediawiki::bz2_admission_stats();
    value.bz2_admission_limit = u64::try_from(admission.limit).unwrap_or(u64::MAX);
    value.bz2_admission_active_decoders =
        u64::try_from(admission.active_decoders).unwrap_or(u64::MAX);
    value.bz2_admission_peak_active_decoders =
        u64::try_from(admission.peak_active_decoders).unwrap_or(u64::MAX);
}

/// Per-target resource counters are sampled in the build-node process itself.
/// `ru_maxrss` is bytes on macOS and KiB on Linux; keeping the conversion here
/// makes the sidecar format platform-independent.
fn process_resource_usage() -> (u64, u64, u64) {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
            let usage = unsafe { usage.assume_init() };
            let micros = |value: libc::timeval| {
                let seconds = u64::try_from(value.tv_sec).unwrap_or(0);
                let micros = u64::try_from(value.tv_usec).unwrap_or(0);
                seconds.saturating_mul(1_000_000).saturating_add(micros)
            };
            let rss = u64::try_from(usage.ru_maxrss).unwrap_or(0);
            let rss = if cfg!(target_os = "macos") {
                rss
            } else {
                rss.saturating_mul(1024)
            };
            return (micros(usage.ru_utime), micros(usage.ru_stime), rss);
        }
    }
    (0, 0, 0)
}

fn persist_live_heartbeat(state: &Arc<Mutex<LiveProgressState>>) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    let now = now_micros();
    state.value.heartbeat_at_micros = now;
    if state.last_phase != state.value.phase {
        state.last_phase = state.value.phase.clone();
        state.value.phase_started_at_micros = now;
    } else if state.value.phase_started_at_micros == 0 {
        state.value.phase_started_at_micros = now;
    }
    sample_live_resource_telemetry(&mut state.value);
    write_live_progress_locked(&mut state);
}

fn set_live_phase(state: &Arc<Mutex<LiveProgressState>>, phase: &str) {
    if let Ok(mut state) = state.lock() {
        if state.value.phase != phase {
            state.value.phase = phase.to_owned();
        }
    }
}

fn write_live_progress_locked(state: &mut LiveProgressState) {
    if let Some(projection) = &state.projection {
        projection.write(&state.value);
    }
    state.last_write = Instant::now();
}

struct CountingReader<R> {
    inner: R,
    read_bytes: u64,
    last_sync: Instant,
    state: Arc<Mutex<LiveProgressState>>,
    stats: wikimak_mediawiki::FetchStatsHandle,
}

struct DecodedCountingReader {
    inner: Box<dyn Read + Send>,
    read_bytes: u64,
    last_sync: Instant,
    state: Arc<Mutex<LiveProgressState>>,
}

impl Read for DecodedCountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        if count != 0 {
            self.read_bytes = self.read_bytes.saturating_add(count as u64);
            if self.last_sync.elapsed() >= Duration::from_secs(1) {
                if let Ok(mut state) = self.state.lock() {
                    state.value.decoded_bytes = self.read_bytes;
                }
                self.last_sync = Instant::now();
            }
        }
        Ok(count)
    }
}

impl<R> CountingReader<R> {
    fn sync_stats(&mut self, force: bool) {
        if !force && self.last_sync.elapsed() < Duration::from_secs(1) {
            return;
        }
        if let Ok(stats) = self.stats.lock() {
            if let Ok(mut state) = self.state.lock() {
                state.value.source_bytes_read = self.read_bytes;
                state.value.fetch_attempts = stats.attempts;
                state.value.fetch_bytes_received = stats.bytes_received;
                state.value.fetch_rate_limit_responses = stats.rate_limit_responses;
                state.value.fetch_client_error_responses = stats.client_error_responses;
                state.value.fetch_server_error_responses = stats.server_error_responses;
                state.value.fetch_transport_errors = stats.transport_errors;
                state.value.fetch_server_timed_retries = stats.server_timed_retries;
                state.value.fetch_robots_timed_retries = stats.robots_timed_retries;
                state.value.fetch_fallback_timed_retries = stats.fallback_timed_retries;
                state.value.fetch_local_spacing_timed_retries =
                    stats.local_spacing_timed_retries;
            }
        }
        self.last_sync = Instant::now();
        if force {
            persist_live_progress(&self.state, true);
        }
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = match self.inner.read(buffer) {
            Ok(count) => count,
            Err(error) => {
                self.sync_stats(true);
                return Err(error);
            }
        };
        if count != 0 {
            self.read_bytes = self.read_bytes.saturating_add(count as u64);
        }
        self.sync_stats(false);
        Ok(count)
    }
}

impl<R> Drop for CountingReader<R> {
    fn drop(&mut self) {
        // The final successful read can occur inside the one-second telemetry
        // coalescing interval. Publish the exact consumer and fetch counters
        // when the decompressor releases its source instead of leaving a
        // completed target with the preceding sample.
        self.sync_stats(true);
    }
}

impl DirectBuildPlan {
    pub(crate) fn target_count(&self) -> usize {
        self.content_target_count() + self.history_files.len()
    }

    pub(crate) fn content_target_count(&self) -> usize {
        self.content_groups.len()
    }

    fn content_target(&self, index: usize) -> Option<&[PlannedPart]> {
        self.content_groups.get(index).map(Vec::as_slice)
    }

    pub(crate) fn target_name(&self, kind: &str, index: usize) -> Option<String> {
        match kind {
            "content" => self
                .content_target(index)
                .map(|_| format!("content-{index:06}")),
            "history" => self
                .history_files
                .get(index)
                .map(|_| format!("history-{index:06}")),
            _ => None,
        }
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

    pub(crate) fn first_source_url(&self) -> Option<&str> {
        self.content_groups
            .iter()
            .flatten()
            .map(|part| part.url.as_str())
            .chain(
                self.history_files
                    .iter()
                    .map(|file| file.part.url.as_str()),
            )
            .next()
    }

    pub(crate) fn target_source_bytes(&self, kind: &str, index: usize) -> u64 {
        match kind {
            "content" => self
                .content_target(index)
                .map_or(0, |group| group.iter().map(|part| part.size_bytes).sum()),
            "history" => self
                .history_files
                .get(index)
                .map_or(0, |file| file.part.size_bytes),
            _ => 0,
        }
    }

    pub(crate) fn target_source_identity(&self, kind: &str, index: usize) -> Result<String> {
        let identity = match kind {
            "content" => self
                .content_target(index)
                .map(|group| serde_json::to_vec(&("content", group)))
                .transpose()
                .map_err(|_| Error::Corrupt("cannot encode content target identity"))?,
            "history" => self
                .history_files
                .get(index)
                .map(|file| serde_json::to_vec(&("history", file)))
                .transpose()
                .map_err(|_| Error::Corrupt("cannot encode history target identity"))?,
            _ => None,
        }
        .ok_or(Error::Corrupt("target is outside build plan"))?;
        use sha2::Digest;
        Ok(hex::encode(sha2::Sha256::digest(identity)))
    }
}

fn content_sample_quotas(plan: &DirectBuildPlan) -> Vec<usize> {
    let weights = plan
        .content_groups
        .iter()
        .map(|group| group.iter().map(|part| part.size_bytes).sum::<u64>())
        .collect::<Vec<_>>();
    if weights.is_empty() {
        return Vec::new();
    }
    let total_weight = weights.iter().map(|weight| u128::from(*weight)).sum::<u128>();
    if total_weight == 0 {
        let mut quotas = vec![plan.ref_prefix_sample_bytes / weights.len(); weights.len()];
        for quota in quotas
            .iter_mut()
            .take(plan.ref_prefix_sample_bytes % weights.len())
        {
            *quota += 1;
        }
        return quotas;
    }
    let capacity = plan.ref_prefix_sample_bytes as u128;
    let mut quotas = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    let mut assigned = 0_usize;
    for (index, weight) in weights.into_iter().enumerate() {
        let product = capacity * u128::from(weight);
        let quota = usize::try_from(product / total_weight)
            .expect("proportional sample quota does not exceed total capacity");
        assigned = assigned.saturating_add(quota);
        quotas.push(quota);
        remainders.push((product % total_weight, index));
    }
    remainders.sort_unstable_by(|left, right| {
        right.0.cmp(&left.0).then(left.1.cmp(&right.1))
    });
    for (_, index) in remainders
        .into_iter()
        .take(plan.ref_prefix_sample_bytes.saturating_sub(assigned))
    {
        quotas[index] += 1;
    }
    quotas
}

pub(crate) fn canonical_direct_plan_id(plan: &DirectBuildPlan) -> Result<String> {
    let mut identity = plan.clone();
    identity.plan_id.clear();
    let identity = serde_json::to_vec(&identity)
        .map_err(|_| Error::Corrupt("cannot encode direct build plan"))?;
    use sha2::Digest;
    Ok(hex::encode(sha2::Sha256::digest(identity)))
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

fn planned_content_groups(
    parts: Vec<wikimak_mediawiki::Part>,
) -> Vec<Vec<PlannedPart>> {
    crate::sync::part_groups(parts)
        .into_iter()
        .map(|group| group.iter().map(PlannedPart::from).collect())
        .collect()
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
    let content_groups = planned_content_groups(content_run.parts.clone());
    if content_groups.is_empty() {
        return Err(Error::Corrupt("content dump contains no parts"));
    }
    let source_bytes = content_run
        .parts
        .iter()
        .map(|part| part.size_bytes)
        .chain(history_files.iter().map(|file| file.part.size_bytes))
        .sum();
    let (ref_prefix_sample_bytes, ref_prefix_bytes) = planned_ref_prefix_layout();
    let mut plan = DirectBuildPlan {
        schema: 1,
        plan_id: String::new(),
        wiki_db: dbname.to_owned(),
        content_snapshot: content_run.date.to_string(),
        metadata_snapshot,
        observed_at_micros: snapshot_date_micros(content_run.date)?,
        frame_target: MIRROR_FRAME_TARGET,
        range_target: planned_range_layout(source_bytes),
        compression_level: 9,
        ref_prefix_sample_bytes,
        ref_prefix_bytes,
        content_groups,
        history_files: history_files
            .iter()
            .map(|file| PlannedHistoryFile {
                partition: file.partition.clone(),
                part: PlannedPart::from(&file.part),
            })
            .collect(),
    };
    plan.plan_id = canonical_direct_plan_id(&plan)?;
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
        || plan.plan_id != canonical_direct_plan_id(&plan)?
    {
        return Err(Error::Corrupt("unsupported direct build plan"));
    }
    Ok(plan)
}

fn node_path(root: &Path, plan: &DirectBuildPlan, kind: &str, index: usize) -> PathBuf {
    let kind = crate::build_lifecycle::TargetKind::parse(kind)
        .expect("direct build target kind is fixed by the plan");
    crate::build_lifecycle::target_path(root, plan, kind, index)
        .expect("direct build target index came from the validated plan")
}

fn human_progress_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
    stats: &PartialStats,
) -> Result<()> {
    let data = temporary.join("data.swdump");
    let data_bytes = std::fs::metadata(&data)?.len();
    std::fs::File::open(&data)?.sync_all()?;
    if temporary.join("siteinfo.swdump").exists() {
        std::fs::File::open(temporary.join("siteinfo.swdump"))?.sync_all()?;
    }
    if temporary.join("newest.samples").exists() {
        std::fs::File::open(temporary.join("newest.samples"))?.sync_all()?;
    }
    std::fs::File::open(temporary.join("title-records.swdump"))?.sync_all()?;
    std::fs::File::open(temporary.join("data.swframe"))?.sync_all()?;
    let kind = crate::build_lifecycle::TargetKind::parse(kind)
        .ok_or(Error::Corrupt("unknown direct build target kind"))?;
    let siteinfo_bytes = temporary
        .join("siteinfo.swdump")
        .exists()
        .then(|| std::fs::metadata(temporary.join("siteinfo.swdump")).map(|value| value.len()))
        .transpose()?;
    let sample_bytes = temporary
        .join("newest.samples")
        .exists()
        .then(|| std::fs::metadata(temporary.join("newest.samples")).map(|value| value.len()))
        .transpose()?;
    let checkpoint = temporary.join("checkpoint.json");
    if checkpoint.exists() {
        std::fs::remove_file(&checkpoint)?;
    }
    let receipt = crate::build_lifecycle::make_target_receipt(
        plan,
        kind,
        index,
        data_bytes,
        siteinfo_bytes,
        sample_bytes,
        crate::build_lifecycle::target_file_inventory(temporary, kind, index)
            .map_err(map_invalid_build)?,
        stats.clone(),
    )
    .map_err(map_invalid_build)?;
    let receipt_path = temporary.join("receipt.json");
    {
        let mut output = std::fs::File::create(&receipt_path)?;
        serde_json::to_writer(&mut output, &receipt)
            .map_err(|_| Error::Corrupt("cannot encode build receipt"))?;
        output.write_all(b"\n")?;
        output.sync_all()?;
    }
    sync_directory(temporary)?;
    let destination = node_path(root, plan, kind.as_str(), index);
    std::fs::rename(temporary, &destination)?;
    sync_directory(&destination)?;
    sync_directory(&root.join("nodes"))?;
    crate::progress_projection::mark_target_completed(root, plan, kind.as_str(), index);
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
    // Stage one exposes one make recipe per source target.  Content recipes
    // may run concurrently, but history recipes all share the same remote
    // stream, sort workspace, and destination device; their target-level
    // materialization contract is therefore one owner at a time.  Acquire
    // this before inspecting or beginning the target so waiting make recipes
    // do not publish themselves as active history work.
    let _history_lease = (kind == "history")
        .then(|| acquire_history_materialization_lease(root))
        .transpose()?;
    let target_kind = crate::build_lifecycle::TargetKind::parse(kind)
        .ok_or(Error::Corrupt("unknown direct build target kind"))?;
    let target = crate::build_lifecycle::inspect_target_for_materialization(
        root,
        plan,
        target_kind,
        index,
    )
    .map_err(map_invalid_build)?;
    match crate::build_lifecycle::transition_target(
        &target.state,
        crate::build_lifecycle::TargetEvent::Begin,
    )
    .map_err(|_| Error::Corrupt("invalid target begin transition"))?
    {
        crate::build_lifecycle::TargetTransition::Reuse => {
            progress(&format!(
                "reusing {kind} target {}/{}",
                index + 1,
                plan.target_count()
            ));
            return Ok(());
        }
        crate::build_lifecycle::TargetTransition::Start
        | crate::build_lifecycle::TargetTransition::Resume => {}
        _ => return Err(Error::Corrupt("target begin produced a non-begin decision")),
    }
    let target_name = plan
        .target_name(kind, index)
        .ok_or(Error::Corrupt("target is outside build plan"))?;
    let temporary = crate::build_lifecycle::target_partial_path(
        root,
        plan,
        target_kind,
        index,
    )
    .map_err(map_invalid_build)?;
    std::fs::create_dir_all(&temporary)?;
    // A target can spend many minutes inside one blocking read/write call:
    // durable receipts only appear after the whole target is complete, and
    // the parent scheduler otherwise sees no stderr at all during that time.
    // Keep emitting the last known activity so the live mirror job has a
    // heartbeat even while the parser or output file is blocked on the
    // destination volume.
    let activity = Arc::new(Mutex::new(format!(
        "starting {kind} target {}/{}",
        index + 1,
        plan.target_count()
    )));
    let heartbeat_stop = Arc::new(AtomicBool::new(false));
    let heartbeat_activity = Arc::clone(&activity);
    let heartbeat_stop_thread = Arc::clone(&heartbeat_stop);
    let heartbeat_name = target_name.clone();
    let heartbeat = std::thread::spawn(move || {
        while !heartbeat_stop_thread.load(Ordering::Relaxed) {
            for _ in 0..50 {
                if heartbeat_stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let message = heartbeat_activity
                .lock()
                .map(|message| message.clone())
                .unwrap_or_else(|_| "activity state unavailable".into());
            eprintln!("heartbeat {heartbeat_name}: {message}");
        }
    });
    let report = |message: &str| {
        if let Ok(mut activity) = activity.lock() {
            *activity = message.to_owned();
        }
        progress(message);
    };
    let result: Result<PartialStats> = (|| match kind {
        "content" => {
            let parts = plan
                .content_target(index)
                .ok_or(Error::Corrupt("content target is outside build plan"))?
                .iter()
                .map(plan_part)
                .collect::<Vec<_>>();
            let built = build_content_group(
                client,
                &parts,
                index,
                &temporary,
                bz2_workers,
                plan.observed_at_micros,
                true,
                Some(content_sample_quotas(plan)[index]),
                &report,
            )?;
            let built_stats = built.stats.clone();
            std::fs::rename(built.path, temporary.join("data.swdump"))?;
            std::fs::rename(
                built.title_path,
                temporary.join("title-records.swdump"),
            )?;
            let samples = built
                .samples
                .ok_or(Error::Corrupt("content target produced no newest-revision samples"))?;
            std::fs::rename(samples, temporary.join("newest.samples"))?;
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
            Ok(built_stats)
        }
        "history" => {
            let file = planned_history(
                plan.history_files
                    .get(index)
                    .ok_or(Error::Corrupt("history target is outside build plan"))?,
            );
            let cancelled = Arc::new(AtomicBool::new(false));
            let history_workers = history_decoder_workers(processing_parallelism());
            let (path, stats) = build_history_part(
                client,
                &plan.wiki_db,
                &file,
                index,
                &temporary,
                history_workers,
                cancelled,
                &report,
            )?;
            let title_path = path.with_extension("titles.swdump");
            std::fs::rename(path, temporary.join("data.swdump"))?;
            std::fs::rename(title_path, temporary.join("title-records.swdump"))?;
            Ok(stats)
        }
        _ => Err(Error::Corrupt("unknown direct build target kind")),
    })();
    heartbeat_stop.store(true, Ordering::Relaxed);
    let _ = heartbeat.join();
    let stats = match result {
        Ok(stats) => stats,
        Err(error) => {
            let message = format!("failed {kind} target {}: {error}", index + 1);
            eprintln!("{message}");
            progress(&message);
            // Keep the failed node for diagnosis. A resumed build removes this
            // dot-prefixed attempt before retrying; plan-bound telemetry stays
            // in the shared projection.
            return Err(error);
        }
    };
    crate::frame_directory::write_from_archive(
        temporary.join("data.swdump"),
        temporary.join("data.swframe"),
        crate::build_lifecycle::target_frame_directory_identity(
            plan,
            target_kind,
            index,
        )
        .map_err(map_invalid_build)?,
    )
    .map_err(map_archive)?;
    if crate::build_lifecycle::transition_target(
        &target.state,
        crate::build_lifecycle::TargetEvent::Checkpoint,
    )
    .map_err(|_| Error::Corrupt("invalid target checkpoint transition"))?
        != crate::build_lifecycle::TargetTransition::PersistCheckpoint
    {
        return Err(Error::Corrupt(
            "target checkpoint produced a non-checkpoint decision",
        ));
    }
    let checkpoint = crate::build_lifecycle::make_target_checkpoint(
        root,
        plan,
        target_kind,
        index,
    )
    .map_err(map_invalid_build)?;
    crate::build_lifecycle::persist_receipt(
        &temporary.join("checkpoint.json"),
        &checkpoint,
    )
    .map_err(map_invalid_build)?;
    let checkpointed = crate::build_lifecycle::inspect_target_for_materialization(
        root,
        plan,
        target_kind,
        index,
    )
    .map_err(map_invalid_build)?;
    if crate::build_lifecycle::transition_target(
        &checkpointed.state,
        crate::build_lifecycle::TargetEvent::Publish,
    )
    .map_err(|_| Error::Corrupt("invalid target publish transition"))?
        != crate::build_lifecycle::TargetTransition::Publish
    {
        return Err(Error::Corrupt(
            "target publish produced a non-publish decision",
        ));
    }
    publish_node(root, plan, kind, index, &temporary, &stats)?;
    progress(&format!("finished {kind} target {}", index + 1));
    Ok(())
}

struct AssemblyTelemetry {
    phase: AtomicU64,
    phase_current: AtomicU64,
    phase_total: AtomicU64,
    input_bytes: Arc<AtomicU64>,
    output_bytes: AtomicU64,
    records: AtomicU64,
    entity_kind: AtomicU64,
    entity_id: AtomicU64,
}

struct SequentialTargetReaders {
    targets: std::vec::IntoIter<(PathBuf, PathBuf, [u8; 32])>,
    current: Option<ArchiveRecordReader>,
    last_entity: Option<EntityKey>,
    at_target_start: bool,
    resume_after: Option<EntityKey>,
    completed_compressed_bytes: Arc<AtomicU64>,
}

impl SequentialTargetReaders {
    fn new(
        targets: Vec<(PathBuf, PathBuf, [u8; 32])>,
        resume_after: Option<EntityKey>,
        completed_compressed_bytes: Arc<AtomicU64>,
    ) -> Self {
        Self {
            targets: targets.into_iter(),
            current: None,
            last_entity: None,
            at_target_start: false,
            resume_after,
            completed_compressed_bytes,
        }
    }
}

impl RecordSource for SequentialTargetReaders {
    fn next_record(&mut self) -> crate::archive::Result<Option<Record>> {
        loop {
            if let Some(current) = self.current.as_mut() {
                if let Some(record) = current.next_record()? {
                    let entity = record.entity();
                    if self.at_target_start
                        && self.last_entity.is_some_and(|last| entity <= last)
                    {
                        return Err(ArchiveError::Invalid(
                            "sequential content targets overlap or are out of order",
                        ));
                    }
                    self.at_target_start = false;
                    self.last_entity = Some(entity);
                    return Ok(Some(record));
                }
                self.current = None;
            }
            let Some((archive, directory_path, identity)) = self.targets.next() else {
                return Ok(None);
            };
            let directory = Arc::new(
                crate::frame_directory::FrameDirectory::open_bound(
                    directory_path,
                    identity,
                )?,
            );
            let position = self
                .resume_after
                .map_or(0, |boundary| directory.first_after_entity(boundary));
            if position == directory.len() {
                continue;
            }
            self.current = Some(
                ArchiveRecordReader::open_frame_directory_accounted(
                    archive,
                    directory,
                    position,
                    Arc::clone(&self.completed_compressed_bytes),
                )?,
            );
            self.at_target_start = true;
        }
    }
}

impl AssemblyTelemetry {
    fn new() -> Self {
        Self {
            phase: AtomicU64::new(0),
            phase_current: AtomicU64::new(0),
            phase_total: AtomicU64::new(0),
            input_bytes: Arc::new(AtomicU64::new(0)),
            output_bytes: AtomicU64::new(0),
            records: AtomicU64::new(0),
            entity_kind: AtomicU64::new(0),
            entity_id: AtomicU64::new(0),
        }
    }
}

struct AssemblyProgressWriter<'a> {
    inner: crate::archive_set::ArchiveSetOutput,
    output_bytes: Arc<AssemblyTelemetry>,
    root: &'a Path,
    plan: &'a DirectBuildPlan,
    sealed_segments: usize,
    checkpoint: crate::build_lifecycle::AssemblyCheckpointTracker,
}

const ARCHIVE_FRAME_HEADER_BYTES: u64 = 64;

impl AssemblyProgressWriter<'_> {
    fn checkpoint(&mut self) -> std::io::Result<()> {
        self.checkpoint
            .checkpoint(self.root, self.plan, self.inner.segments())
            .map(|_| ())
            .map_err(std::io::Error::other)
    }

    fn into_inner(self) -> crate::archive_set::ArchiveSetOutput {
        self.inner
    }

    fn observe_sealed_segments(&mut self) -> std::io::Result<()> {
        let sealed = self.inner.segments().len();
        if sealed != self.sealed_segments {
            self.checkpoint()?;
            self.sealed_segments = sealed;
        }
        Ok(())
    }
}

impl Write for AssemblyProgressWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.output_bytes
            .output_bytes
            .fetch_add(written as u64, Ordering::Relaxed);
        self.observe_sealed_segments()?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl StreamingFrameOutput for AssemblyProgressWriter<'_> {
    type StreamingFrameToken =
        <crate::archive_set::ArchiveSetOutput as StreamingFrameOutput>::StreamingFrameToken;

    fn begin_streaming_frame(
        &mut self,
        provisional: FrameInfo,
    ) -> crate::archive::Result<Self::StreamingFrameToken> {
        let token = self.inner.begin_streaming_frame(provisional)?;
        self.output_bytes
            .output_bytes
            .fetch_add(ARCHIVE_FRAME_HEADER_BYTES, Ordering::Relaxed);
        self.observe_sealed_segments()
            .map_err(ArchiveError::Io)?;
        Ok(token)
    }

    fn finish_streaming_frame(
        &mut self,
        token: Self::StreamingFrameToken,
        completed: FrameInfo,
    ) -> crate::archive::Result<()> {
        self.inner.finish_streaming_frame(token, completed)?;
        self.observe_sealed_segments()
            .map_err(ArchiveError::Io)?;
        Ok(())
    }
}

fn entity_kind_name(kind: u64) -> &'static str {
    match kind {
        value if value == EntityKind::Page as u64 => "page",
        value if value == EntityKind::User as u64 => "user",
        value if value == EntityKind::Global as u64 => "global",
        _ => "initializing",
    }
}

fn duration_summary(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn process_cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0.0;
    }
    let usage = unsafe { usage.assume_init() };
    let timeval = |value: libc::timeval| {
        value.tv_sec as f64 + value.tv_usec as f64 / 1_000_000.0
    };
    timeval(usage.ru_utime) + timeval(usage.ru_stime)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn process_resident_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::zeroed();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    (result == 0).then(|| unsafe { info.assume_init().resident_size })
}

#[cfg(not(target_os = "macos"))]
fn process_resident_bytes() -> Option<u64> {
    None
}

fn read_site_info_target(path: &Path) -> Result<crate::archive::SiteInfoRecord> {
    let mut reader = ArchiveRecordReader::open(path).map_err(map_archive)?;
    let mut latest = None::<(i64, crate::archive::SiteInfoRecord)>;
    while let Some(record) = reader.next_record().map_err(map_archive)? {
        if let Record::SiteInfo {
            timestamp_micros,
            site_info,
        } = record
        {
            if latest
                .as_ref()
                .is_none_or(|(timestamp, _)| timestamp_micros > *timestamp)
            {
                latest = Some((timestamp_micros, site_info));
            }
        }
    }
    latest
        .map(|(_, site_info)| site_info)
        .ok_or(Error::Corrupt("siteinfo target contains no siteinfo record"))
}

fn write_generation_index_from_projection(
    archive: &Path,
    output: &Path,
    generation_id: &crate::generation::GenerationId,
    title_entries: &crate::title_projection::ExternalTitleEntries,
) -> Result<()> {
    let identity = generation_id.to_bytes().map_err(|_| {
        Error::Corrupt("generation ID cannot bind the assembly frame directory")
    })?;
    let frame_directory_path = archive.with_extension("swframe");
    if !frame_directory_path.exists() {
        crate::frame_directory::write_from_archive_set(
            archive,
            &frame_directory_path,
            identity,
        )
        .map_err(map_archive)?;
    }
    let frames =
        crate::frame_directory::FrameDirectory::open_bound(&frame_directory_path, identity)
            .map_err(map_archive)?;
    let archive_set =
        crate::archive_set::ArchiveSetReader::open(archive).map_err(map_archive)?;
    let frame_entries = frames.iter().map(|frame| {
        frame.map(|frame| crate::title_index::FrameIndexEntry {
            info: frame.frame_info(),
            compressed_offset: frame.compressed_offset,
        })
    });
    let segment_entries = archive_set.segments().iter().map(|segment| {
        let role = match segment.kind {
            Some(crate::archive::EntityKind::Page) => 1,
            Some(crate::archive::EntityKind::User) => 2,
            Some(crate::archive::EntityKind::Global) => 3,
            None if segment.name.starts_with("0000-") => 0,
            None if segment.name.starts_with("9999-") => 4,
            None => u8::MAX,
        };
        Ok(crate::title_index::SegmentIndexEntry {
            role,
            first_id: segment.first_id,
            last_id: segment.last_id,
            virtual_start: segment.virtual_start,
            bytes: segment.bytes,
        })
    });
    crate::title_index::write_generation_index(
        output,
        generation_id,
        title_entries.iter(),
        frame_entries,
        segment_entries,
    )
    .map_err(map_archive)
}

fn distill_plan_ref_prefix(root: &Path, plan: &DirectBuildPlan) -> Result<Vec<u8>> {
    let mut samples = Vec::new();
    let mut sample_bytes = 0_usize;
    for index in 0..plan.content_target_count() {
        let path = node_path(root, plan, "content", index).join("newest.samples");
        read_text_samples(&path, |sample| {
            sample_bytes = sample_bytes
                .checked_add(sample.len())
                .ok_or(Error::Corrupt("newest-revision sample volume overflow"))?;
            if sample_bytes > plan.ref_prefix_sample_bytes {
                return Err(Error::Corrupt(
                    "newest-revision samples exceed the plan-wide bound",
                ));
            }
            samples.push(sample.to_vec());
            Ok(())
        })?;
    }
    if sample_bytes == 0 {
        return Err(Error::Corrupt(
            "newest-revision sampling produced no text",
        ));
    }
    if sample_bytes <= plan.ref_prefix_bytes {
        let mut prefix = Vec::with_capacity(sample_bytes);
        for sample in samples {
            prefix.extend_from_slice(&sample);
        }
        return Ok(prefix);
    }
    crate::archive::distill_ref_prefix(
        &samples,
        plan.ref_prefix_bytes,
        plan.compression_level,
    )
    .map_err(map_archive)
}

pub(crate) fn assemble_direct_build(
    root: &Path,
    plan: &DirectBuildPlan,
    progress: &(impl Fn(&str) + Sync),
) -> Result<PathBuf> {
    let output = root.join("archive.swdump");
    let assembly_run_id =
        crate::progress_projection::current_run_id(root, &plan.plan_id);
    if crate::build_lifecycle::recover_archive_commit(root, plan)
        .map_err(map_invalid_build)?
        .is_some()
    {
        progress("recovered the plan-bound complete archive; projecting its index");
    }
    let existing_title_projection =
        match crate::build_lifecycle::inspect_build(root, Some(&plan.plan_id))
        .map_err(map_invalid_build)?
    {
        crate::build_lifecycle::BuildState::Ready { .. } => {
            if crate::build_lifecycle::transition_assembly(
                crate::build_lifecycle::AssemblyState::Ready,
                crate::build_lifecycle::AssemblyEvent::Begin,
            )
            .map_err(|_| Error::Corrupt("invalid ready-generation reuse transition"))?
                != crate::build_lifecycle::AssemblyTransition::Reuse
            {
                return Err(Error::Corrupt("ready generation is not reusable"));
            }
            return Ok(output);
        }
        crate::build_lifecycle::BuildState::Projecting {
            title_projection,
            ..
        } => {
            if crate::build_lifecycle::transition_assembly(
                crate::build_lifecycle::AssemblyState::Projecting,
                crate::build_lifecycle::AssemblyEvent::Begin,
            )
            .map_err(|_| Error::Corrupt("invalid title-projection resume transition"))?
                != crate::build_lifecycle::AssemblyTransition::ResumeProjection
            {
                return Err(Error::Corrupt("title projection is not resumable"));
            }
            let titles = output.with_extension("swtitle");
            if !titles.exists() {
                progress("recovery is finishing the index from its durable title projection");
                let title_entries = crate::title_projection::ExternalTitleEntries::open_bound(
                    root.join(&title_projection.file_name),
                    &title_projection.sha256,
                    title_projection.entries,
                )
                .map_err(map_archive)?;
                if title_entries.entry_count() != title_projection.entries {
                    return Err(Error::Corrupt(
                        "durable title projection disagrees with its receipt",
                    ));
                }
                write_generation_index_from_projection(
                    &output,
                    &titles,
                    &crate::generation::GenerationId::from_plan_id(&plan.plan_id),
                    &title_entries,
                )?;
            }
            crate::build_lifecycle::commit_generation(root, plan)
                .map_err(map_invalid_build)?;
            for path in [
                root.join(&title_projection.file_name),
                root.join("title-projection.receipt.json"),
            ] {
                std::fs::remove_file(path)?;
            }
            sync_directory(root)?;
            remove_consumed_build_inputs(root, plan)?;
            return Ok(output);
        }
        crate::build_lifecycle::BuildState::ReadyForAssembly { .. } => None,
        crate::build_lifecycle::BuildState::Assembling {
            title_projection,
            ..
        } => title_projection,
        crate::build_lifecycle::BuildState::Planned { .. } => {
            return Err(Error::Corrupt("direct build input target is incomplete"))
        }
        crate::build_lifecycle::BuildState::Unplanned => {
            return Err(Error::Corrupt("direct assembly has no plan"))
        }
    };
    crate::build_lifecycle::prepare_assembly(root, plan).map_err(map_invalid_build)?;
    let site_info_path = node_path(root, plan, "content", 0).join("siteinfo.swdump");
    let site_info = read_site_info_target(&site_info_path)?;
    progress("distilling the refPrefix from bounded content-target samples");
    let planned_ref_prefix = distill_plan_ref_prefix(root, plan)?;
    let title_entries = if let Some(receipt) = existing_title_projection {
        crate::title_projection::ExternalTitleEntries::open_bound(
            root.join(&receipt.file_name),
            &receipt.sha256,
            receipt.entries,
        )
        .map_err(map_archive)?
    } else {
        // Use a newly-created, owned workspace.  A stale interrupted
        // projection is evidence to preserve, not a directory this build may
        // recursively delete.  The TempDir owns only the files created by
        // this invocation and cleans them on ordinary error return.
        let title_projection_work = tempfile::Builder::new()
            .prefix("title-projection-work-")
            .tempdir_in(root)?;
        let mut projection = crate::title_projection::ExternalTitleProjectionBuilder::new_in(
            title_projection_work.path(),
            site_info,
            crate::title_projection::ProjectionLimits::default(),
        )
        .map_err(map_archive)?;
        let content_title_groups = (0..plan.content_target_count())
            .map(|index| {
                vec![node_path(root, plan, "content", index)
                    .join("title-records.swdump")]
            })
            .collect::<Vec<_>>();
        let title_progress = Arc::new(AtomicU64::new(0));
        let mut title_sources: Vec<Box<dyn RecordSource>> =
            Vec::with_capacity(plan.history_files.len() + 1);
        title_sources.push(Box::new(
            crate::archive::SequentialRecordGroups::open_paths(
                content_title_groups,
                Arc::clone(&title_progress),
            ),
        ));
        for index in 0..plan.history_files.len() {
            title_sources.push(Box::new(
                ArchiveRecordReader::open_accounted(
                    node_path(root, plan, "history", index)
                        .join("title-records.swdump"),
                    Arc::clone(&title_progress),
                )
                .map_err(map_archive)?,
            ));
        }
        let title_merge_workspace = if title_sources.len() > crate::archive::MAX_SORTED_MERGE_FAN_IN {
            Some(
                tempfile::Builder::new()
                    .prefix("merge-")
                    .tempdir_in(title_projection_work.path())?,
            )
        } else {
            None
        };
        if let Some(workspace) = title_merge_workspace.as_ref() {
            (title_sources, _) = materialize_bounded_merge_sources(
                title_sources,
                workspace.path(),
                Arc::clone(&title_progress),
                DEFAULT_FRAME_TARGET,
                CompressionSettings::default(),
            )?;
        }
        progress("projecting title history from durable metadata-only target sidecars");
        let mut projection_error = None;
        crate::archive::visit_merged_record_sources(title_sources, |record| {
            if projection_error.is_none() {
                projection_error = projection.observe(record).err();
            }
        })
        .map_err(map_archive)?;
        if let Some(error) = projection_error {
            return Err(map_archive(error));
        }
        let entries = projection
            .finish()
            .map_err(map_archive)?
            .persist_content_addressed(root)
            .map_err(map_archive)?;
        let file_name = entries
            .file_name()
            .to_str()
            .ok_or(Error::Corrupt("title projection filename is not UTF-8"))?
            .to_owned();
        let identity = entries.identity_hex();
        crate::build_lifecycle::commit_title_projection(
            root,
            plan,
            &file_name,
            entries.entry_count(),
            &identity,
        )
        .map_err(map_invalid_build)?;
        entries
    };
    let assembly_name = "assembly.partial";
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

    let content_inputs = (0..plan.content_target_count())
        .map(|index| node_path(root, plan, "content", index).join("data.swdump"))
        .collect::<Vec<_>>();
    let history_inputs = (0..plan.history_files.len())
        .map(|index| node_path(root, plan, "history", index).join("data.swdump"))
        .collect::<Vec<_>>();
    let mut inputs = content_inputs
        .iter()
        .cloned()
        .chain(history_inputs.iter().cloned())
        .collect::<Vec<_>>();
    inputs.push(site_info_path.clone());
    inputs.push(manifest_archive.clone());
    let assembly_started_micros = now_micros();
    let inventory_total_bytes = inputs.iter().fold(0_u64, |total, input| {
        total.saturating_add(std::fs::metadata(input).map_or(0, |metadata| metadata.len()))
    });
    progress(&format!(
        "inventorying {} durable inputs before assembly",
        inputs.len()
    ));
    let (cpu_user_micros, cpu_system_micros, peak_rss_bytes) = process_resource_usage();
    write_assembly_progress(
        root,
        &AssemblyProgressSnapshot {
            plan_id: plan.plan_id.clone(),
            run_id: assembly_run_id.clone(),
            pid: std::process::id(),
            phase: format!("inventorying 0/{} inputs", inputs.len()),
            input_bytes: 0,
            input_bytes_total: inventory_total_bytes,
            output_bytes: 0,
            records: 0,
            current_entity_kind: 0,
            current_entity_id: 0,
            bytes_per_second: 0,
            phase_current: 0,
            phase_total: inputs.len() as u64,
            started_at_micros: assembly_started_micros,
            updated_at_micros: now_micros(),
            cpu_user_micros,
            cpu_system_micros,
            peak_rss_bytes,
        },
    );
    let mut input_compressed_bytes = inventory_total_bytes;
    let telemetry = Arc::new(AssemblyTelemetry::new());
    progress(&format!(
        "assembling {} compressed from {} inputs into durable page-ID ranges",
        human_progress_bytes(input_compressed_bytes),
        inputs.len()
    ));
    let range_target = assembly_range_target(plan);
    let generation_id = crate::generation::GenerationId::from_plan_id(&plan.plan_id);
    let frame_directory_identity = generation_id.to_bytes().map_err(|_| {
        Error::Corrupt("generation ID cannot bind the assembly frame directory")
    })?;
    let temporary = crate::archive_set::ArchiveSetOutput::resumable_in(
        root,
        &assembly_name,
        range_target,
    )
    .and_then(|output_set| {
        output_set.write_frame_directory_to(
            output.with_extension("swframe"),
            frame_directory_identity,
        )
    })
    .map_err(map_archive)?;
    let resume_after = temporary.resume_after();
    let preserved_ref_prefix = temporary.preserved_ref_prefix().map_err(map_archive)?;
    let resumed_output_bytes = temporary.virtual_bytes();
    if let Some(prefix) = preserved_ref_prefix.as_ref() {
        if let Some(boundary) = resume_after {
            progress(&format!(
                "resuming final assembly with preserved {} refPrefix after durable {} {} \
                 ({} already sealed)",
                human_progress_bytes(prefix.len() as u64),
                entity_kind_name(boundary.kind as u64),
                boundary.id,
                human_progress_bytes(resumed_output_bytes),
            ));
        } else {
            progress(&format!(
                "resuming final assembly with preserved {} refPrefix",
                human_progress_bytes(prefix.len() as u64),
            ));
        }
    } else if let Some(boundary) = resume_after {
        progress(&format!(
            "resuming final assembly after durable {} {} ({} already sealed)",
            entity_kind_name(boundary.kind as u64),
            boundary.id,
            human_progress_bytes(resumed_output_bytes),
        ));
    }
    if preserved_ref_prefix
        .as_deref()
        .is_some_and(|prefix| prefix != planned_ref_prefix.as_slice())
    {
        return Err(Error::Corrupt(
            "resumed assembly refPrefix differs from its plan-bound samples",
        ));
    }
    let ref_prefix = preserved_ref_prefix
        .as_deref()
        .unwrap_or(&planned_ref_prefix);
    let content_targets = (0..plan.content_target_count())
        .map(|index| {
            let node = node_path(root, plan, "content", index);
            Ok((
                node.join("data.swdump"),
                node.join("data.swframe"),
                crate::build_lifecycle::target_frame_directory_identity(
                    plan,
                    crate::build_lifecycle::TargetKind::Content,
                    index,
                )
                .map_err(map_invalid_build)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut record_sources: Vec<Box<dyn RecordSource>> =
        Vec::with_capacity(plan.history_files.len() + 3);
    record_sources.push(Box::new(SequentialTargetReaders::new(
        content_targets,
        resume_after,
        Arc::clone(&telemetry.input_bytes),
    )));
    for (index, path) in history_inputs.iter().enumerate() {
        let node = node_path(root, plan, "history", index);
        let directory = Arc::new(
            crate::frame_directory::FrameDirectory::open_bound(
                node.join("data.swframe"),
                crate::build_lifecycle::target_frame_directory_identity(
                    plan,
                    crate::build_lifecycle::TargetKind::History,
                    index,
                )
                .map_err(map_invalid_build)?,
            )
            .map_err(map_archive)?,
        );
        let position = resume_after
            .map_or(0, |boundary| directory.first_after_entity(boundary));
        if position != directory.len() {
            record_sources.push(Box::new(
                ArchiveRecordReader::open_frame_directory_accounted(
                    path,
                    directory,
                    position,
                    Arc::clone(&telemetry.input_bytes),
                )
                .map_err(map_archive)?,
            ));
        }
    }
    for path in [site_info_path, manifest_archive.clone()] {
        record_sources.push(Box::new(
            ArchiveRecordReader::open_accounted(
                path,
                Arc::clone(&telemetry.input_bytes),
            )
            .map_err(map_archive)?,
        ));
    }
    // The refPrefix-aware final merge still has the archive reader's fixed
    // source limit. Materialize deterministic merge levels at that same
    // archive-module bound in a fresh destination-local workspace before
    // invoking that final merge.
    // The workspace is owned by this invocation: ordinary errors remove only
    // its generated intermediates, while a process interruption leaves the
    // uniquely named directory for inspection rather than deleting it on the
    // next attempt.  Per-level crash resume is intentionally not claimed.
    let assembly_merge_workspace = if record_sources.len() > crate::archive::MAX_SORTED_MERGE_FAN_IN {
        Some(
            tempfile::Builder::new()
                .prefix("assembly-merge-")
                .tempdir_in(root)?,
        )
    } else {
        None
    };
    if let Some(workspace) = assembly_merge_workspace.as_ref() {
        let (bounded_sources, intermediate_bytes) = materialize_bounded_merge_sources(
            record_sources,
            workspace.path(),
            Arc::clone(&telemetry.input_bytes),
            plan.frame_target,
            CompressionSettings {
                level: plan.compression_level,
                ..CompressionSettings::default()
            },
        )?;
        record_sources = bounded_sources;
        // Every generated archive is consumed exactly once by a later merge
        // level or by the final refPrefix-aware merge. Include those reads in
        // the denominator so byte progress and throughput remain truthful
        // instead of exceeding 100% as soon as fan-in materialization occurs.
        input_compressed_bytes = input_compressed_bytes.saturating_add(intermediate_bytes);
    }
    telemetry
        .output_bytes
        .store(resumed_output_bytes, Ordering::Relaxed);
    let stop_reporter = Arc::new(AtomicBool::new(false));
    let assembly_started = Instant::now();
    let cpu_started = process_cpu_seconds();
    let telemetry_output = AssemblyProgressWriter {
        sealed_segments: temporary.segments().len(),
        checkpoint: crate::build_lifecycle::AssemblyCheckpointTracker::new(
            temporary.segments(),
        ),
        inner: temporary,
        output_bytes: Arc::clone(&telemetry),
        root,
        plan,
    };
    let (mut file, _, records, _) = std::thread::scope(|scope| {
        let reporter_telemetry = Arc::clone(&telemetry);
        let reporter_stop = Arc::clone(&stop_reporter);
        let reporter = scope.spawn(move || {
            let mut previous_phase = u64::MAX;
            let mut phase_started = Instant::now();
            while !reporter_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                if reporter_stop.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = assembly_started.elapsed().as_secs_f64().max(0.001);
                let input = reporter_telemetry.input_bytes.load(Ordering::Relaxed);
                let output = reporter_telemetry.output_bytes.load(Ordering::Relaxed);
                let records = reporter_telemetry.records.load(Ordering::Relaxed);
                let phase = reporter_telemetry.phase.load(Ordering::Relaxed);
                if phase != previous_phase {
                    previous_phase = phase;
                    phase_started = Instant::now();
                }
                let phase_elapsed = phase_started.elapsed().as_secs_f64().max(0.001);
                let phase_current = reporter_telemetry
                    .phase_current
                    .load(Ordering::Relaxed);
                let phase_total = reporter_telemetry.phase_total.load(Ordering::Relaxed);
                let kind = reporter_telemetry.entity_kind.load(Ordering::Relaxed);
                let entity_id = reporter_telemetry.entity_id.load(Ordering::Relaxed);
                let rate = input as f64 / elapsed;
                let percent = if input_compressed_bytes == 0 {
                    0.0
                } else {
                    input as f64 * 100.0 / input_compressed_bytes as f64
                };
                let eta = if input > 0 && input < input_compressed_bytes {
                    duration_summary(
                        ((input_compressed_bytes - input) as f64 / rate.max(1.0)) as u64,
                    )
                } else {
                    "estimating".to_owned()
                };
                let cpu = (process_cpu_seconds() - cpu_started) / elapsed;
                let memory = process_resident_bytes()
                    .map(human_progress_bytes)
                    .unwrap_or_else(|| "unknown".to_owned());
                let range = output / range_target + 1;
                let status = match phase {
                    0 => format!(
                        "sampling newest revisions · {} {} · input {}/{} \
                         ({percent:.1}%, {}/s, ETA {eta})",
                        entity_kind_name(kind),
                        entity_id,
                        human_progress_bytes(input),
                        human_progress_bytes(input_compressed_bytes),
                        human_progress_bytes(rate as u64),
                    ),
                    1 => format!(
                        "distilling {} refPrefix from {} samples",
                        human_progress_bytes(phase_total),
                        human_progress_bytes(phase_current),
                    ),
                    2 => {
                        let replay_percent = if phase_total == 0 {
                            0.0
                        } else {
                            phase_current as f64 * 100.0 / phase_total as f64
                        };
                        let replay_rate = phase_current as f64 / phase_elapsed;
                        let replay_eta = if phase_current > 0 && phase_current < phase_total {
                            duration_summary(
                                ((phase_total - phase_current) as f64
                                    / replay_rate.max(1.0)) as u64,
                            )
                        } else {
                            "estimating".to_owned()
                        };
                        format!(
                            "replaying bootstrap {phase_current}/{phase_total} records \
                            ({replay_percent:.1}%, ETA {replay_eta})"
                        )
                    }
                    _ => format!(
                        "merging · {} {} · input {}/{} \
                         ({percent:.1}%, {}/s, ETA {eta})",
                        entity_kind_name(kind),
                        entity_id,
                        human_progress_bytes(input),
                        human_progress_bytes(input_compressed_bytes),
                        human_progress_bytes(rate as u64),
                    ),
                };
                let persisted_phase = match phase {
                    0 => format!(
                        "sampling newest revisions · page {entity_id} · {percent:.1}% input · \
                         ETA {eta}"
                    ),
                    1 => format!(
                        "distilling {} refPrefix from {} samples",
                        human_progress_bytes(phase_total),
                        human_progress_bytes(phase_current),
                    ),
                    2 => {
                        let replay_percent = if phase_total == 0 {
                            0.0
                        } else {
                            phase_current as f64 * 100.0 / phase_total as f64
                        };
                        let replay_rate = phase_current as f64 / phase_elapsed;
                        let replay_eta = if phase_current > 0 && phase_current < phase_total {
                            duration_summary(
                                ((phase_total - phase_current) as f64
                                    / replay_rate.max(1.0)) as u64,
                            )
                        } else {
                            "estimating".to_owned()
                        };
                        format!(
                            "replaying bootstrap {phase_current}/{phase_total} records \
                             ({replay_percent:.1}%) · ETA {replay_eta}"
                        )
                    }
                    _ => format!(
                        "merging · {} {entity_id} · {percent:.1}% input · ETA {eta}",
                        entity_kind_name(kind),
                    ),
                };
                let persisted_rate = if matches!(phase, 0 | 3) {
                    rate as u64
                } else {
                    0
                };
                let (cpu_user_micros, cpu_system_micros, peak_rss_bytes) =
                    process_resource_usage();
                write_assembly_progress(
                    root,
                    &AssemblyProgressSnapshot {
                        plan_id: plan.plan_id.clone(),
                        run_id: assembly_run_id.clone(),
                        pid: std::process::id(),
                        phase: persisted_phase,
                        input_bytes: input,
                        input_bytes_total: input_compressed_bytes,
                        output_bytes: output,
                        records,
                        current_entity_kind: kind,
                        current_entity_id: entity_id,
                        bytes_per_second: persisted_rate,
                        phase_current,
                        phase_total,
                        started_at_micros: assembly_started_micros,
                        updated_at_micros: now_micros(),
                        cpu_user_micros,
                        cpu_system_micros,
                        peak_rss_bytes,
                    },
                );
                progress(&format!(
                    "assembly · {status} · output {} (range ~{range}) · {records} records · \
                     CPU {cpu:.1} cores · RSS {memory}",
                    human_progress_bytes(output),
                ));
            }
        });
        let mut observe = |record: &Record| {
            let entity = record.entity();
            telemetry.records.fetch_add(1, Ordering::Relaxed);
            telemetry
                .entity_kind
                .store(entity.kind as u64, Ordering::Relaxed);
            telemetry.entity_id.store(entity.id, Ordering::Relaxed);
        };
        let compression = CompressionSettings {
            level: plan.compression_level,
            ..CompressionSettings::default()
        };
        telemetry.phase.store(3, Ordering::Relaxed);
        let result = crate::archive::merge_record_sources_reusing_ref_prefix_observing_after(
            record_sources,
            telemetry_output,
            plan.frame_target,
            compression,
            ref_prefix,
            resume_after,
            &mut observe,
        );
        stop_reporter.store(true, Ordering::Relaxed);
        let _ = reporter.join();
        result
    })
    .map_err(map_archive)?;
    telemetry
        .input_bytes
        .store(input_compressed_bytes, Ordering::Relaxed);
    let output_bytes = telemetry.output_bytes.load(Ordering::Relaxed);
    progress(&format!(
        "assembly merge complete · {records} records · input {} · output {} · elapsed {}",
        human_progress_bytes(input_compressed_bytes),
        human_progress_bytes(output_bytes),
        duration_summary(assembly_started.elapsed().as_secs()),
    ));
    file.checkpoint()?;
    let completed = file.into_inner().finish().map_err(map_archive)?;
    if crate::build_lifecycle::transition_assembly(
        crate::build_lifecycle::AssemblyState::Partial,
        crate::build_lifecycle::AssemblyEvent::FinishAndRename,
    )
    .map_err(|_| Error::Corrupt("invalid assembly finish transition"))?
        != crate::build_lifecycle::AssemblyTransition::RenameArchive
    {
        return Err(Error::Corrupt("assembly is not ready for atomic rename"));
    }
    completed.persist(&output).map_err(map_archive)?;
    crate::build_lifecycle::commit_archive(root, plan).map_err(map_invalid_build)?;
    progress("writing title and virtual-frame index from the merged record projection");
    write_generation_index_from_projection(
        &output,
        &output.with_extension("swtitle"),
        &generation_id,
        &title_entries,
    )?;
    let title_projection_name = title_entries
        .file_name()
        .to_str()
        .ok_or(Error::Corrupt("title projection filename is not UTF-8"))?
        .to_owned();
    drop(title_entries);
    crate::build_lifecycle::commit_generation(root, plan).map_err(map_invalid_build)?;
    for path in [
        root.join(title_projection_name),
        root.join("title-projection.receipt.json"),
    ] {
        std::fs::remove_file(path)?;
    }
    sync_directory(root)?;

    if let Err(error) = remove_consumed_build_inputs(root, plan) {
        progress(&format!(
            "generation is ready; optional source cleanup remains pending: {error}"
        ));
    } else {
        progress("generation is ready; consumed source targets removed");
    }
    Ok(output)
}

#[cfg(test)]
const COMMITTED_TARGET_FILES: &[&str] = &[
    "data.swdump",
    "data.swframe",
    "title-records.swdump",
    "receipt.json",
    "newest.samples",
    "siteinfo.swdump",
];

const CLEANUP_MANIFEST_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum CleanupEntryState {
    Planned,
    Claimed,
    Removed,
    Foreign,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CleanupInventoryEntry {
    name: String,
    bytes: u64,
    identity: Option<String>,
    state: CleanupEntryState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CleanupInventory {
    schema: u32,
    operation: String,
    entries: Vec<CleanupInventoryEntry>,
}

fn cleanup_file_identity(path: &Path) -> std::io::Result<(u64, Option<String>)> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((
            metadata.len(),
            Some(format!(
                "unix:{}:{}:{}:{}",
                metadata.dev(),
                metadata.ino(),
                metadata.mtime(),
                metadata.mtime_nsec()
            )),
        ))
    }
    #[cfg(not(unix))]
    {
        Ok((metadata.len(), None))
    }
}

fn update_tail_cleanup_quarantine_root(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "updates"))
        .and_then(Path::parent)
        .map(|scratch| scratch.join(".sarun-quarantine"))
        .unwrap_or_else(|| {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(".sarun-quarantine")
        })
}

fn ensure_cleanup_quarantine_root(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(Error::Corrupt("cleanup quarantine is not a real directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(Error::Io(error)),
    }
}

fn persist_cleanup_inventory(path: &Path, inventory: &CleanupInventory) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(Error::Corrupt("cleanup inventory has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, inventory)
        .map_err(|_| Error::Corrupt("cannot encode cleanup inventory"))?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    sync_directory(parent)
}

fn cleanup_leaf_name_is_safe(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(component)), None)
            if component.to_str() == Some(name)
    )
}

fn validate_cleanup_inventory(
    label: &str,
    expected: &[CleanupInventoryEntry],
    inventory: &CleanupInventory,
) -> Result<()> {
    if !cleanup_leaf_name_is_safe(label) {
        return Err(Error::Corrupt("cleanup operation name is not a safe leaf"));
    }
    if inventory.schema != CLEANUP_MANIFEST_SCHEMA || inventory.operation != label {
        return Err(Error::Corrupt(
            "cleanup inventory belongs to another operation",
        ));
    }
    if inventory.entries.len() < expected.len() {
        return Err(Error::Corrupt("cleanup inventory lost an owned entry"));
    }

    let mut names = BTreeSet::new();
    for (index, entry) in inventory.entries.iter().enumerate() {
        if !cleanup_leaf_name_is_safe(&entry.name) || entry.name == "cleanup.json" {
            return Err(Error::Corrupt(
                "cleanup inventory contains an unsafe entry name",
            ));
        }
        if !names.insert(entry.name.as_str()) {
            return Err(Error::Corrupt(
                "cleanup inventory contains duplicate entry names",
            ));
        }
        if let Some(expected_entry) = expected.get(index) {
            if entry.name != expected_entry.name
                || entry.bytes != expected_entry.bytes
                || entry.identity != expected_entry.identity
            {
                return Err(Error::Corrupt(
                    "cleanup inventory does not match the validated ownership receipt",
                ));
            }
        } else if entry.state != CleanupEntryState::Foreign
            || entry.bytes != 0
            || entry.identity.is_some()
            || !entry.name.starts_with("foreign-")
        {
            return Err(Error::Corrupt(
                "cleanup inventory contains an invalid foreign entry",
            ));
        }
    }
    Ok(())
}

fn create_or_resume_cleanup_inventory(
    quarantine: &Path,
    label: &str,
    entries: Vec<CleanupInventoryEntry>,
) -> Result<(PathBuf, CleanupInventory)> {
    if !cleanup_leaf_name_is_safe(label) {
        return Err(Error::Corrupt("cleanup operation name is not a safe leaf"));
    }
    let mut expected_names = BTreeSet::new();
    for entry in &entries {
        if entry.state != CleanupEntryState::Planned {
            return Err(Error::Corrupt(
                "new cleanup ownership entries must be planned",
            ));
        }
        if !cleanup_leaf_name_is_safe(&entry.name) || entry.name == "cleanup.json" {
            return Err(Error::Corrupt(
                "cleanup ownership receipt contains an unsafe entry name",
            ));
        }
        if !expected_names.insert(entry.name.as_str()) {
            return Err(Error::Corrupt(
                "cleanup ownership receipt contains duplicate entry names",
            ));
        }
    }
    ensure_cleanup_quarantine_root(quarantine)?;
    let operation = quarantine.join(label);
    match std::fs::symlink_metadata(&operation) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&operation)?;
            let inventory = CleanupInventory {
                schema: CLEANUP_MANIFEST_SCHEMA,
                operation: label.to_owned(),
                entries,
            };
            validate_cleanup_inventory(label, &inventory.entries, &inventory)?;
            persist_cleanup_inventory(&operation.join("cleanup.json"), &inventory)?;
            Ok((operation, inventory))
        }
        Err(error) => Err(Error::Io(error)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            Err(Error::Corrupt("cleanup operation is not a directory"))
        }
        Ok(_) => {
            let manifest = operation.join("cleanup.json");
            let metadata = std::fs::symlink_metadata(&manifest)?;
            if !metadata.file_type().is_file() {
                return Err(Error::Corrupt("cleanup inventory is not a regular file"));
            }
            let bytes = std::fs::read(&manifest)?;
            let inventory: CleanupInventory = serde_json::from_slice(&bytes)
                .map_err(|_| Error::Corrupt("invalid resumable cleanup inventory"))?;
            validate_cleanup_inventory(label, &entries, &inventory)?;
            Ok((operation, inventory))
        }
    }
}

fn claim_cleanup_entry(
    source: &Path,
    operation: &Path,
    entry_index: usize,
    inventory_path: &Path,
    inventory: &mut CleanupInventory,
) -> Result<()> {
    let entry = inventory
        .entries
        .get(entry_index)
        .ok_or(Error::Corrupt("cleanup inventory entry disappeared"))?
        .clone();
    let destination = operation.join(&entry.name);
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match crate::instance::rename_without_replacing(source, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if matches!(
                        entry.state,
                        CleanupEntryState::Claimed | CleanupEntryState::Removed
                    ) || (entry.state == CleanupEntryState::Planned
                        && entry.bytes == 0
                        && entry.identity.is_none())
                    {
                        inventory.entries[entry_index].state = CleanupEntryState::Removed;
                        persist_cleanup_inventory(inventory_path, inventory)?;
                        return Ok(());
                    }
                    if entry.state == CleanupEntryState::Foreign {
                        return Ok(());
                    }
                    return Err(Error::Io(error));
                }
                Err(error) => return Err(Error::Io(error)),
            }
        }
        Err(error) => return Err(Error::Io(error)),
    }
    let actual = cleanup_file_identity(&destination);
    let owned = actual.as_ref().is_ok_and(|(bytes, identity)| {
        *bytes == entry.bytes && entry.identity.is_some() && identity == &entry.identity
    });
    inventory.entries[entry_index].state = if owned {
        CleanupEntryState::Claimed
    } else {
        CleanupEntryState::Foreign
    };
    persist_cleanup_inventory(inventory_path, inventory)?;
    if owned {
        std::fs::remove_file(&destination)?;
        inventory.entries[entry_index].state = CleanupEntryState::Removed;
        persist_cleanup_inventory(inventory_path, inventory)?;
    }
    Ok(())
}

fn claim_unexpected_cleanup_entry(
    source: &Path,
    operation: &Path,
    inventory_path: &Path,
    inventory: &mut CleanupInventory,
) -> Result<()> {
    let name = source
        .file_name()
        .ok_or(Error::Corrupt("cleanup candidate has no filename"))?
        .to_string_lossy();
    for counter in 0_u32..1024 {
        let candidate_name = format!("foreign-{name}-{counter}");
        let destination = operation.join(&candidate_name);
        match crate::instance::rename_without_replacing(source, &destination) {
            Ok(()) => {
                inventory.entries.push(CleanupInventoryEntry {
                    name: candidate_name,
                    bytes: 0,
                    identity: None,
                    state: CleanupEntryState::Foreign,
                });
                persist_cleanup_inventory(inventory_path, inventory)?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Err(Error::Corrupt("cleanup quarantine namespace exhausted"))
}

fn quarantine_target_remainder(root: &Path, node: &Path) -> Result<()> {
    let quarantine = root.join(".sarun-quarantine");
    ensure_cleanup_quarantine_root(&quarantine)?;
    let name = node
        .file_name()
        .ok_or(Error::Corrupt("cannot name residual target directory"))?
        .to_string_lossy();
    let mut destination = None;
    for counter in 0_u32..1024 {
        let candidate = quarantine.join(format!(
            "target-residue-{name}-{}-{counter}",
            std::process::id()
        ));
        match crate::instance::rename_without_replacing(node, &candidate) {
            Ok(()) => {
                destination = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::Io(error)),
        }
    }
    let _destination =
        destination.ok_or(Error::Corrupt("target quarantine namespace exhausted"))?;
    sync_directory(root)?;
    sync_directory(&root.join("nodes"))?;
    sync_directory(&quarantine)
}

pub(crate) fn retire_validated_target_directory(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: crate::build_lifecycle::TargetKind,
    index: usize,
) -> Result<()> {
    let path = node_path(root, plan, kind.as_str(), index);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::Io(error)),
    };
    if !metadata.file_type().is_dir() {
        return quarantine_target_remainder(root, &path);
    }
    let receipt_path = path.join("receipt.json");
    match std::fs::symlink_metadata(&receipt_path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return quarantine_target_remainder(root, &path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return quarantine_target_remainder(root, &path);
        }
        Err(error) => return Err(Error::Io(error)),
    }
    crate::build_lifecycle::validate_ready_target_for_cleanup(root, plan, kind, index)
        .map_err(map_invalid_build)?;
    let receipt_bytes = std::fs::read(&receipt_path)?;
    let receipt: crate::build_lifecycle::TargetReceipt = serde_json::from_slice(&receipt_bytes)
        .map_err(|_| Error::Corrupt("target cleanup receipt is invalid"))?;
    if receipt.files.is_empty() {
        return quarantine_target_remainder(root, &path);
    }
    let receipt_identity = cleanup_file_identity(&receipt_path)?;
    let mut entries = receipt
        .files
        .iter()
        .map(|file| CleanupInventoryEntry {
            name: file.name.clone(),
            bytes: file.bytes,
            identity: file.identity.clone(),
            state: CleanupEntryState::Planned,
        })
        .collect::<Vec<_>>();
    entries.push(CleanupInventoryEntry {
        name: "receipt.json".into(),
        bytes: receipt_identity.0,
        identity: receipt_identity.1,
        state: CleanupEntryState::Planned,
    });
    let label = format!("target-cleanup-{}", receipt.target_id);
    let quarantine = root.join(".sarun-quarantine");
    let (operation, mut inventory) =
        create_or_resume_cleanup_inventory(&quarantine, &label, entries)?;
    let inventory_path = operation.join("cleanup.json");
    for entry_index in 0..inventory.entries.len() {
        if inventory.entries[entry_index].state == CleanupEntryState::Removed {
            continue;
        }
        let name = inventory.entries[entry_index].name.clone();
        let source = path.join(name);
        claim_cleanup_entry(
            &source,
            &operation,
            entry_index,
            &inventory_path,
            &mut inventory,
        )?;
    }
    let residuals = std::fs::read_dir(&path)
        .map_err(Error::Io)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for residual in residuals {
        claim_unexpected_cleanup_entry(
            &residual.path(),
            &operation,
            &inventory_path,
            &mut inventory,
        )?;
    }
    match std::fs::remove_dir(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            Err(Error::Corrupt("target cleanup left a residual source entry"))
        }
        Err(error) => Err(Error::Io(error)),
    }
}

fn remove_consumed_build_inputs(root: &Path, plan: &DirectBuildPlan) -> Result<()> {
    if !matches!(
        crate::build_lifecycle::inspect_build(root, Some(&plan.plan_id))
            .map_err(map_invalid_build)?,
        crate::build_lifecycle::BuildState::Ready { .. }
    ) {
        return Err(Error::Corrupt(
            "source cleanup is allowed only after generation commit",
        ));
    }
    if crate::build_lifecycle::transition_assembly(
        crate::build_lifecycle::AssemblyState::Ready,
        crate::build_lifecycle::AssemblyEvent::CleanupRequested,
    )
    .map_err(|_| Error::Corrupt("invalid committed-source cleanup transition"))?
        != crate::build_lifecycle::AssemblyTransition::Cleanup
    {
        return Err(Error::Corrupt("committed source cleanup is not authorized"));
    }
    let targets = [
        (
            crate::build_lifecycle::TargetKind::Content,
            plan.content_target_count(),
        ),
        (
            crate::build_lifecycle::TargetKind::History,
            plan.history_files.len(),
        ),
    ];
    // Validate the complete set before the first mutation.  A committed
    // archive proves assembly completed, but cleanup authority still comes
    // from each source target's own receipt and concrete representation.
    for (kind, count) in targets {
        for index in 0..count {
            crate::build_lifecycle::validate_ready_target_for_cleanup(root, plan, kind, index)
                .map_err(map_invalid_build)?;
        }
    }
    for (kind, count) in targets {
        for index in 0..count {
            retire_validated_target_directory(root, plan, kind, index)?;
        }
    }
    let manifest = root.join("manifest.swdump");
    if std::fs::symlink_metadata(&manifest).is_ok() {
        quarantine_target_remainder(root, &manifest)?;
    }
    sync_directory(&root.join("nodes"))
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
    std::env::set_var(
        "SARUN_WIKIMEDIA_ROBOTS_CACHE",
        scratch.path().join("robots-cache"),
    );
    let result = build_direct_inner(
        client,
        config,
        dbname,
        output.as_ref(),
        scratch.path(),
        &progress,
    );
    let mut stats = result?;
    stats.elapsed_millis = started.elapsed().as_millis() as u64;
    Ok(stats)
}

fn update_source_plan_id(plan: &UpdateSourcePlan) -> Result<String> {
    let mut canonical = plan.clone();
    canonical.source_plan_id.clear();
    canonical.generation_id = crate::generation::GenerationId::from_plan_bytes(&[]);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|_| Error::Corrupt("cannot encode update source plan"))?;
    use sha2::Digest;
    Ok(hex::encode(sha2::Sha256::digest(bytes)))
}

fn update_generation_id(
    base_generation_id: &crate::generation::GenerationId,
    source_plan_id: &str,
) -> crate::generation::GenerationId {
    let mut identity = b"wikipedia-update-generation\0".to_vec();
    identity.extend_from_slice(base_generation_id.as_str().as_bytes());
    identity.push(0);
    identity.extend_from_slice(source_plan_id.as_bytes());
    crate::generation::GenerationId::from_plan_bytes(&identity)
}

pub(crate) fn validate_update_source_plan(plan: &UpdateSourcePlan) -> Result<()> {
    if plan.schema != 1
        || plan.frame_target == 0
        || plan.wiki_db.is_empty()
        || plan.source_plan_id != update_source_plan_id(plan)?
        || plan.generation_id
            != update_generation_id(&plan.base_generation_id, &plan.source_plan_id)
        || chrono::NaiveDate::parse_from_str(&plan.base_content_frontier, "%Y-%m-%d").is_err()
        || chrono::NaiveDate::parse_from_str(&plan.resulting_content_frontier, "%Y-%m-%d")
            .is_err()
    {
        return Err(Error::Corrupt("invalid update source plan"));
    }
    Ok(())
}

pub(crate) fn discover_update_source_plan(
    client: &Client,
    config: &wikimak_mediawiki::Config,
    base: &crate::generation::GenerationIdentity,
    overlap_days: u64,
    frame_target: usize,
    compression: CompressionSettings,
    progress: &(impl Fn(&str) + Sync),
) -> Result<UpdateSourcePlan> {
    let content_from = chrono::NaiveDate::parse_from_str(
        &base.content_frontier,
        "%Y-%m-%d",
    )
    .map_err(|_| Error::Corrupt("invalid base generation content frontier"))?;
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
        &base.wiki_db,
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

    progress("discovering MediaWiki History partitions");
    let (metadata_snapshot, mut history_files) =
        crate::sync::discover_history(client, config, &base.wiki_db)?;
    if metadata_snapshot < base.metadata_frontier {
        return Err(Error::Mediawiki(wikimak_mediawiki::Error::Parse(
            format!(
                "MediaWiki History snapshot regressed from {} to {metadata_snapshot}",
                base.metadata_frontier
            ),
        )));
    }
    if metadata_snapshot == base.metadata_frontier {
        history_files.clear();
        progress(&format!(
            "MediaWiki History {metadata_snapshot} is already present"
        ));
    } else if history_files.len() > 2 {
        history_files = history_files.split_off(history_files.len() - 2);
    }
    let mut plan = UpdateSourcePlan {
        schema: 1,
        source_plan_id: String::new(),
        generation_id: crate::generation::GenerationId::from_plan_bytes(&[]),
        base_generation_id: base.generation_id.clone(),
        wiki_db: base.wiki_db.clone(),
        base_content_frontier: base.content_frontier.clone(),
        base_metadata_frontier: base.metadata_frontier.clone(),
        overlap_days,
        frame_target,
        compression: compression.into(),
        content_runs: runs
            .iter()
            .map(|run| PlannedIncrementalRun {
                date: run.date.to_string(),
                parts: run.parts.iter().map(PlannedPart::from).collect(),
            })
            .collect(),
        history_snapshot: metadata_snapshot.clone(),
        history_files: history_files
            .iter()
            .map(|file| PlannedHistoryFile {
                partition: file.partition.clone(),
                part: PlannedPart::from(&file.part),
            })
            .collect(),
        resulting_content_frontier: content_through.to_string(),
        resulting_metadata_frontier: metadata_snapshot,
    };
    plan.source_plan_id = update_source_plan_id(&plan)?;
    plan.generation_id =
        update_generation_id(&plan.base_generation_id, &plan.source_plan_id);
    validate_update_source_plan(&plan)?;
    Ok(plan)
}

pub(crate) fn build_update_archive_from_plan(
    client: &Client,
    plan: &UpdateSourcePlan,
    output: impl AsRef<Path>,
    scratch_parent: impl AsRef<Path>,
    progress: impl Fn(&str) + Sync,
) -> Result<UpdateArchiveStats> {
    let scratch_parent = scratch_parent.as_ref();
    build_update_archive_from_plan_inner(
        client,
        plan,
        output.as_ref(),
        scratch_parent,
        scratch_parent,
        None,
        progress,
    )
}

pub(crate) fn build_update_archive_from_plan_for_run(
    client: &Client,
    plan: &UpdateSourcePlan,
    output: impl AsRef<Path>,
    scratch_parent: impl AsRef<Path>,
    progress_root: impl AsRef<Path>,
    run_id: Option<&str>,
    progress: impl Fn(&str) + Sync,
) -> Result<UpdateArchiveStats> {
    build_update_archive_from_plan_inner(
        client,
        plan,
        output.as_ref(),
        scratch_parent.as_ref(),
        progress_root.as_ref(),
        run_id,
        progress,
    )
}

fn build_update_archive_from_plan_inner(
    client: &Client,
    plan: &UpdateSourcePlan,
    output: &Path,
    scratch_parent: &Path,
    progress_root: &Path,
    run_id: Option<&str>,
    progress: impl Fn(&str) + Sync,
) -> Result<UpdateArchiveStats> {
    validate_update_source_plan(plan)?;
    let started = Instant::now();
    std::fs::create_dir_all(scratch_parent)?;
    std::fs::create_dir_all(progress_root)?;
    let scratch = scratch_parent;
    std::env::set_var(
        "SARUN_WIKIMEDIA_ROBOTS_CACHE",
        scratch.join("robots-cache"),
    );
    let result = build_update_inner(
        client,
        plan,
        output,
        scratch,
        progress_root,
        run_id,
        &progress,
    );
    let mut stats = result?;
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
    let base_archive = base_archive.as_ref();
    let base = crate::generation::generation_identity(
        base_archive,
        base_archive.with_extension("swtitle"),
    )
    .map_err(|error| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        ))
    })?;
    if base.wiki_db != dbname {
        return Err(Error::Corrupt("base archive belongs to another wiki"));
    }
    let plan = discover_update_source_plan(
        client,
        config,
        &base,
        overlap_days,
        frame_target,
        compression,
        &progress,
    )?;
    build_update_archive_from_plan(
        client,
        &plan,
        output,
        scratch_parent,
        progress,
    )
}

fn build_update_inner(
    client: &Client,
    plan: &UpdateSourcePlan,
    output: &Path,
    scratch: &Path,
    progress_root: &Path,
    run_id: Option<&str>,
    progress: &(impl Fn(&str) + Sync),
) -> Result<UpdateArchiveStats> {
    validate_update_source_plan(plan)?;
    let progress_plan = incremental_progress_plan(plan);
    crate::progress_projection::initialize(progress_root, &progress_plan)
        .map_err(|error| Error::Io(std::io::Error::new(std::io::ErrorKind::Other, error)))?;
    match run_id {
        Some(run_id) => crate::progress_projection::begin_run(progress_root, &progress_plan, run_id)
            .map_err(|error| Error::Io(std::io::Error::new(std::io::ErrorKind::Other, error)))?,
        None => crate::progress_projection::clear_run(progress_root, &progress_plan)
            .map_err(|error| Error::Io(std::io::Error::new(std::io::ErrorKind::Other, error)))?,
    }
    let content_from = chrono::NaiveDate::parse_from_str(
        &plan.base_content_frontier,
        "%Y-%m-%d",
    )
    .map_err(|_| Error::Corrupt("invalid update base frontier"))?;
    let content_through = chrono::NaiveDate::parse_from_str(
        &plan.resulting_content_frontier,
        "%Y-%m-%d",
    )
    .map_err(|_| Error::Corrupt("invalid update result frontier"))?;
    let cores = processing_parallelism();
    let mut content_results = Vec::new();
    let mut content_input_groups = Vec::new();
    for run in &plan.content_runs {
        let date = chrono::NaiveDate::parse_from_str(&run.date, "%Y-%m-%d")
            .map_err(|_| Error::Corrupt("invalid planned incremental date"))?;
        let parts = run.parts.iter().map(wikimak_mediawiki::Part::from).collect::<Vec<_>>();
        let run_scratch = scratch.join(format!("incremental-{date}"));
        std::fs::create_dir_all(&run_scratch)?;
        let run_results = build_content_parts(
            client,
            &parts,
            &run_scratch,
            cores,
            snapshot_date_micros(date)?,
            progress,
        )?;
        content_input_groups.push(
            run_results
                .iter()
                .map(|result| result.path.clone())
                .collect::<Vec<_>>(),
        );
        content_results.extend(run_results);
    }

    let metadata_snapshot = plan.resulting_metadata_frontier.clone();
    let history_files = plan
        .history_files
        .iter()
        .map(planned_history)
        .collect::<Vec<_>>();
    if !history_files.is_empty() {
        progress(&format!(
            "ingesting {} partitions from MediaWiki History {metadata_snapshot}",
            history_files.len()
        ));
    }
    let history_results =
        build_history_parts(client, &plan.wiki_db, &history_files, scratch, cores, progress)?;

    let manifest_archive = scratch.join("update-manifest.swdump");
    let mut manifest_writer =
        ArchiveWriter::new(std::fs::File::create(&manifest_archive)?, DEFAULT_FRAME_TARGET)
            .map_err(map_archive)?;
    manifest_writer
        .write(&Record::Manifest {
            timestamp_micros: snapshot_date_micros(content_through)?,
            manifest: ManifestRecord {
                wiki_db: plan.wiki_db.clone(),
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

    progress("assembling the sorted update record stream");
    let output_parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(output_parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(output_parent)?;
    let completed_bytes = Arc::new(AtomicU64::new(0));
    let mut inputs: Vec<Box<dyn RecordSource>> =
        Vec::with_capacity(content_input_groups.len() + history_results.len() + 1);
    for group in content_input_groups {
        inputs.push(Box::new(
            crate::archive::SequentialRecordGroups::open_paths(
                vec![group],
                Arc::clone(&completed_bytes),
            ),
        ));
    }
    for (path, _) in &history_results {
        inputs.push(Box::new(
            ArchiveRecordReader::open_accounted(
                path,
                Arc::clone(&completed_bytes),
            )
            .map_err(map_archive)?,
        ));
    }
    inputs.push(Box::new(
        ArchiveRecordReader::open_accounted(
            &manifest_archive,
            Arc::clone(&completed_bytes),
        )
        .map_err(map_archive)?,
    ));
    let merge_work = scratch.join("update-tail-merge-work");
    let (output_frames, output_records) = merge_record_sources_bounded(
        inputs,
        temporary.as_file_mut(),
        &merge_work,
        plan.frame_target,
        plan.compression.into(),
    )?;
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
        incremental_runs: plan.content_runs.len() as u64,
        content_parts: plan
            .content_runs
            .iter()
            .map(|run| run.parts.len() as u64)
            .sum(),
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

fn ensure_empty_merge_workspace(work: &Path) -> Result<()> {
    if !work.is_dir() {
        return Err(Error::Corrupt("bounded merge workspace is not a directory"));
    }
    if std::fs::read_dir(work)?.next().transpose()?.is_some() {
        return Err(Error::Corrupt(
            "bounded merge workspace is not an owned empty directory",
        ));
    }
    Ok(())
}

fn materialize_bounded_merge_sources(
    mut sources: Vec<Box<dyn RecordSource>>,
    work: &Path,
    completed_bytes: Arc<AtomicU64>,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(Vec<Box<dyn RecordSource>>, u64)> {
    if sources.len() <= crate::archive::MAX_SORTED_MERGE_FAN_IN {
        return Ok((sources, 0));
    }
    ensure_empty_merge_workspace(work)?;
    let mut level = 0_usize;
    let mut intermediate_bytes = 0_u64;
    while sources.len() > crate::archive::MAX_SORTED_MERGE_FAN_IN {
        let batches = bounded_merge_batch_sizes(sources.len())
            .into_iter()
            .next()
            .ok_or(Error::Corrupt("bounded merge produced no batches"))?;
        let mut input = sources.into_iter();
        let mut next = Vec::<Box<dyn RecordSource>>::with_capacity(batches.len());
        for (batch, batch_size) in batches.into_iter().enumerate() {
            let path = work.join(format!("level-{level:03}-{batch:06}.swdump"));
            let group = input.by_ref().take(batch_size).collect::<Vec<_>>();
            if group.len() != batch_size {
                return Err(Error::Corrupt("bounded merge batch lost a source"));
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            let (file, _, _) = crate::archive::merge_record_sources_with_compression(
                group,
                file,
                frame_target,
                compression,
            )
            .map_err(map_archive)?;
            file.sync_all()?;
            intermediate_bytes = intermediate_bytes.saturating_add(std::fs::metadata(&path)?.len());
            next.push(Box::new(
                ArchiveRecordReader::open_accounted(&path, Arc::clone(&completed_bytes))
                    .map_err(map_archive)?,
            ));
        }
        if input.next().is_some() {
            return Err(Error::Corrupt("bounded merge left an unassigned source"));
        }
        sources = next;
        level = level
            .checked_add(1)
            .ok_or(Error::Corrupt("bounded merge level overflow"))?;
    }
    Ok((sources, intermediate_bytes))
}

const UPDATE_TAIL_MERGE_WORK_SCHEMA: u32 = 1;
const UPDATE_TAIL_MERGE_WORK_MANIFEST: &str = ".sarun-merge-ownership.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct UpdateTailMergeWorkManifest {
    schema: u32,
    operation: String,
    source_count: usize,
    artifacts: Vec<String>,
    /// Metadata captured immediately after each existing intermediate is
    /// durably written. Missing identity is intentionally not upgraded at
    /// cleanup time: legacy/unverifiable files are retained in quarantine.
    #[serde(default)]
    artifact_inventory: Vec<CleanupInventoryEntry>,
}

fn update_tail_merge_artifacts(source_count: usize) -> Vec<String> {
    let mut artifacts = Vec::new();
    let mut sources = source_count;
    let mut level = 0_usize;
    while sources > crate::archive::MAX_SORTED_MERGE_FAN_IN {
        let batches = sources.div_ceil(crate::archive::MAX_SORTED_MERGE_FAN_IN);
        for batch in 0..batches {
            artifacts.push(format!("level-{level:03}-{batch:06}.swdump"));
        }
        sources = batches;
        level = level.saturating_add(1);
    }
    artifacts
}

fn write_update_tail_merge_manifest(
    work: &Path,
    source_count: usize,
) -> Result<UpdateTailMergeWorkManifest> {
    let manifest = UpdateTailMergeWorkManifest {
        schema: UPDATE_TAIL_MERGE_WORK_SCHEMA,
        operation: "wikimak-update-tail-merge".into(),
        source_count,
        artifacts: update_tail_merge_artifacts(source_count),
        artifact_inventory: update_tail_merge_artifacts(source_count)
            .into_iter()
            .map(|name| CleanupInventoryEntry {
                name,
                bytes: 0,
                identity: None,
                state: CleanupEntryState::Planned,
            })
            .collect(),
    };
    let path = work.join(UPDATE_TAIL_MERGE_WORK_MANIFEST);
    let mut temporary = tempfile::NamedTempFile::new_in(work)?;
    serde_json::to_writer(&mut temporary, &manifest)
        .map_err(|_| Error::Corrupt("cannot encode update-tail merge ownership"))?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| Error::Io(error.error))?;
    sync_directory(work)?;
    Ok(manifest)
}

fn persist_update_tail_merge_manifest(
    work: &Path,
    manifest: &UpdateTailMergeWorkManifest,
) -> Result<()> {
    let path = work.join(UPDATE_TAIL_MERGE_WORK_MANIFEST);
    let mut temporary = tempfile::NamedTempFile::new_in(work)?;
    serde_json::to_writer(&mut temporary, manifest)
        .map_err(|_| Error::Corrupt("cannot encode update-tail merge ownership"))?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| Error::Io(error.error))?;
    sync_directory(work)
}

fn record_update_tail_merge_artifact(work: &Path, name: &str) -> Result<()> {
    let mut manifest = read_update_tail_merge_manifest(work)?;
    let path = work.join(name);
    let (bytes, identity) = cleanup_file_identity(&path)?;
    let Some(entry) = manifest
        .artifact_inventory
        .iter_mut()
        .find(|entry| entry.name == name)
    else {
        return Err(Error::Corrupt("merge output is absent from ownership plan"));
    };
    entry.bytes = bytes;
    entry.identity = identity;
    entry.state = CleanupEntryState::Planned;
    persist_update_tail_merge_manifest(work, &manifest)
}

fn read_update_tail_merge_manifest(work: &Path) -> Result<UpdateTailMergeWorkManifest> {
    let path = work.join(UPDATE_TAIL_MERGE_WORK_MANIFEST);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() {
        return Err(Error::Corrupt(
            "update-tail merge ownership is not a regular file",
        ));
    }
    let bytes = std::fs::read(&path)?;
    let manifest: UpdateTailMergeWorkManifest = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Corrupt("invalid update-tail merge ownership"))?;
    if manifest.schema != UPDATE_TAIL_MERGE_WORK_SCHEMA
        || manifest.operation != "wikimak-update-tail-merge"
        || manifest.artifacts != update_tail_merge_artifacts(manifest.source_count)
    {
        return Err(Error::Corrupt("foreign update-tail merge ownership"));
    }
    Ok(manifest)
}

fn inspect_update_tail_merge_entries(
    work: &Path,
    manifest: &UpdateTailMergeWorkManifest,
) -> Result<()> {
    for entry in std::fs::read_dir(work)? {
        let entry = entry?;
        let name = entry.file_name();
        let owned = name == UPDATE_TAIL_MERGE_WORK_MANIFEST
            || manifest
                .artifacts
                .iter()
                .any(|artifact| name == artifact.as_str());
        if !owned {
            return Err(Error::Corrupt(
                "update-tail merge workspace contains unowned entries",
            ));
        }
        if name != UPDATE_TAIL_MERGE_WORK_MANIFEST && !entry.file_type()?.is_file() {
            return Err(Error::Corrupt(
                "update-tail merge workspace artifact is not a file",
            ));
        }
    }
    Ok(())
}

fn prepare_update_tail_merge_workspace(
    work: &Path,
    source_count: usize,
) -> Result<UpdateTailMergeWorkManifest> {
    match std::fs::symlink_metadata(work) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(work)?;
            return write_update_tail_merge_manifest(work, source_count);
        }
        Err(error) => return Err(Error::Io(error)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(Error::Corrupt(
                "update-tail merge workspace is not a directory",
            ));
        }
        Ok(_) => {}
    }
    let manifest_path = work.join(UPDATE_TAIL_MERGE_WORK_MANIFEST);
    let manifest = match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if metadata.file_type().is_file() => read_update_tail_merge_manifest(work)?,
        Ok(_) => {
            return Err(Error::Corrupt(
                "update-tail merge ownership is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::read_dir(work)?.next().is_none() {
                write_update_tail_merge_manifest(work, source_count)?
            } else {
                return Err(Error::Corrupt(
                    "non-empty update-tail merge workspace has no ownership manifest",
                ));
            }
        }
        Err(error) => return Err(Error::Io(error)),
    };
    if manifest.source_count != source_count {
        return Err(Error::Corrupt(
            "update-tail merge retry has a different source set",
        ));
    }
    inspect_update_tail_merge_entries(work, &manifest)?;
    clear_update_tail_merge_workspace(work)?;
    std::fs::create_dir(work)?;
    write_update_tail_merge_manifest(work, source_count)
}

/// Remove only the exact staging names recorded by an update-tail merge.
/// An unmanifested non-empty directory is left untouched and rejected; this
/// is the recovery boundary for interrupted or foreign work.
pub(crate) fn clear_update_tail_merge_workspace(work: &Path) -> Result<()> {
    match std::fs::symlink_metadata(work) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::Io(error)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(Error::Corrupt(
                "update-tail merge workspace is not a directory",
            ));
        }
        Ok(_) => {}
    }
    let manifest_path = work.join(UPDATE_TAIL_MERGE_WORK_MANIFEST);
    match std::fs::symlink_metadata(&manifest_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::read_dir(work)?.next().is_none() {
                std::fs::remove_dir(work)?;
                return Ok(());
            }
            return Err(Error::Corrupt(
                "non-empty update-tail merge workspace has no ownership manifest",
            ));
        }
        Err(error) => return Err(Error::Io(error)),
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(Error::Corrupt(
                "update-tail merge ownership is not a regular file",
            ));
        }
        Ok(_) => {}
    }
    let manifest = read_update_tail_merge_manifest(work)?;
    inspect_update_tail_merge_entries(work, &manifest)?;
    let mut entries = manifest.artifact_inventory.clone();
    if entries.len() != manifest.artifacts.len() {
        entries = manifest
            .artifacts
            .iter()
            .map(|name| CleanupInventoryEntry {
                name: name.clone(),
                bytes: 0,
                identity: None,
                state: CleanupEntryState::Planned,
            })
            .collect();
    }
    let (manifest_bytes, manifest_identity) = cleanup_file_identity(&manifest_path)?;
    let manifest_label_identity = manifest_identity
        .clone()
        .unwrap_or_else(|| "unidentified".into());
    let label = format!(
        "update-tail-cleanup-{}-{}",
        work.file_name().unwrap_or_default().to_string_lossy(),
        manifest_label_identity
    );
    entries.push(CleanupInventoryEntry {
        name: UPDATE_TAIL_MERGE_WORK_MANIFEST.into(),
        bytes: manifest_bytes,
        identity: manifest_identity,
        state: CleanupEntryState::Planned,
    });
    let quarantine = update_tail_cleanup_quarantine_root(work);
    let (operation, mut inventory) =
        create_or_resume_cleanup_inventory(&quarantine, &label, entries)?;
    let inventory_path = operation.join("cleanup.json");
    for index in 0..inventory.entries.len() {
        if inventory.entries[index].state == CleanupEntryState::Removed {
            continue;
        }
        let source = work.join(inventory.entries[index].name.clone());
        claim_cleanup_entry(
            &source,
            &operation,
            index,
            &inventory_path,
            &mut inventory,
        )?;
    }
    let residuals = std::fs::read_dir(work)?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    for residual in residuals {
        claim_unexpected_cleanup_entry(
            &residual.path(),
            &operation,
            &inventory_path,
            &mut inventory,
        )?;
    }
    match std::fs::remove_dir(work) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            return Err(Error::Corrupt(
                "update-tail cleanup left a residual workspace entry",
            ));
        }
        Err(error) => return Err(Error::Io(error)),
    }
    if let Some(parent) = work.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

/// Merge sorted archive files with the archive module's bounded fan-in.
///
/// This small public seam is used by the bounded-fan-in integration test. The
/// production final assembly uses the same source materialization helper
/// before applying its refPrefix-aware final merge.
#[doc(hidden)]
pub fn merge_sorted_archives_bounded(
    inputs: &[PathBuf],
    output: &Path,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(u64, u64)> {
    if inputs.is_empty() {
        return Err(Error::Corrupt("bounded merge requires at least one input"));
    }
    let parent = output
        .parent()
        .ok_or(Error::Corrupt("bounded merge output has no parent"))?;
    let workspace = tempfile::Builder::new()
        .prefix("bounded-merge-")
        .tempdir_in(parent)?;
    let completed_bytes = Arc::new(AtomicU64::new(0));
    let sources = inputs
        .iter()
        .map(|path| {
            ArchiveRecordReader::open_accounted(path, Arc::clone(&completed_bytes))
                .map(|reader| Box::new(reader) as Box<dyn RecordSource>)
                .map_err(map_archive)
        })
        .collect::<Result<Vec<_>>>()?;
    let (sources, _) = materialize_bounded_merge_sources(
        sources,
        workspace.path(),
        completed_bytes,
        frame_target,
        compression,
    )?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let (file, frames, records) = crate::archive::merge_record_sources_with_compression(
        sources,
        file,
        frame_target,
        compression,
    )
    .map_err(map_archive)?;
    file.sync_all()?;
    Ok((frames, records))
}

fn merge_record_sources_bounded(
    mut sources: Vec<Box<dyn RecordSource>>,
    output: &mut std::fs::File,
    work: &Path,
    frame_target: usize,
    compression: CompressionSettings,
) -> Result<(u64, u64)> {
    let _ownership = prepare_update_tail_merge_workspace(work, sources.len())?;
    let completed_bytes = Arc::new(AtomicU64::new(0));
    let mut level = 0_usize;
    while sources.len() > crate::archive::MAX_SORTED_MERGE_FAN_IN {
        let mut next = Vec::<Box<dyn RecordSource>>::new();
        let mut input = sources.into_iter();
        let mut batch = 0_usize;
        loop {
            let group = input
                .by_ref()
                .take(crate::archive::MAX_SORTED_MERGE_FAN_IN)
                .collect::<Vec<_>>();
            if group.is_empty() {
                break;
            }
            let path = work.join(format!("level-{level:03}-{batch:06}.swdump"));
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            let (file, _, _) = crate::archive::merge_record_sources_with_compression(
                group,
                file,
                frame_target,
                compression,
            )
            .map_err(map_archive)?;
            file.sync_all()?;
            record_update_tail_merge_artifact(work, &format!("level-{level:03}-{batch:06}.swdump"))?;
            next.push(Box::new(
                ArchiveRecordReader::open_accounted(
                    &path,
                    Arc::clone(&completed_bytes),
                )
                .map_err(map_archive)?,
            ));
            batch += 1;
        }
        sources = next;
        level += 1;
    }
    let (_, frames, records) = crate::archive::merge_record_sources_with_compression(
        sources,
        output,
        frame_target,
        compression,
    )
    .map_err(map_archive)?;
    clear_update_tail_merge_workspace(work)?;
    Ok((frames, records))
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
    let cores = processing_parallelism();

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
    // Keep a few independent page-range pipelines available for CPU work.
    // The engine admits one Wikipedia job, while all request workers in that
    // job share the mediawiki process-wide gate.
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
                    false,
                    None,
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
    let geometry = history_parts_geometry(pending.len(), cores);
    let mut results = reused;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut completed_bytes = reused_bytes;
    let mut completed_files = results.len() as u64;
    let unknown_sizes = files
        .iter()
        .filter(|file| file.part.size_bytes == 0)
        .count();
    if geometry.target_owners == 1 {
        for (index, file, key) in pending {
            progress(&format!("history {}", file.part.filename));
            let result = match build_history_part(
                client,
                dbname,
                &file,
                index,
                scratch,
                geometry.decoder_workers,
                Arc::clone(&cancelled),
                progress,
            ) {
                Ok(result) => result,
                Err(error) => {
                    cancelled.store(true, Ordering::Relaxed);
                    return Err(error);
                }
            };
            if let Err(error) = write_checkpoint_receipt(&result.0, &key, &result.1) {
                cancelled.store(true, Ordering::Relaxed);
                return Err(error);
            }
            let completed = completed_bytes.saturating_add(file.part.size_bytes);
            completed_bytes = completed;
            completed_files = completed_files.saturating_add(1);
            progress(&format!(
                "finished history {}; {}",
                file.part.filename,
                history_progress(
                    completed_files,
                    files.len() as u64,
                    completed,
                    total_bytes,
                    unknown_sizes,
                )
            ));
            results.push((index, result));
        }
    }
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
    progress: &(impl Fn(&str) + Sync),
) -> Result<(PathBuf, PartialStats)> {
    let progress_anchor = scratch.join(format!("history-{file_index:06}.progress"));
    let value = LiveTargetProgress {
        target: format!("history-{file_index:06}"),
        part: file.part.filename.clone(),
        phase: "starting".into(),
        source_bytes_total: file.part.size_bytes,
        started_at_micros: now_micros(),
        updated_at_micros: now_micros(),
        ..Default::default()
    };
    let live = Arc::new(Mutex::new(LiveProgressState {
        projection: crate::progress_projection::source_writer(
            &progress_scratch_root(&progress_anchor),
            &value.target,
            &value.part,
        )
        .ok(),
        value,
        last_write: Instant::now()
            .checked_sub(Duration::from_secs(3))
            .unwrap_or_else(Instant::now),
        last_phase: "starting".into(),
    }));
    persist_live_progress(&live, true);
    let _heartbeat = LiveProgressHeartbeat::start(&live);
    let _failure_guard = LiveProgressFailureGuard {
        state: Arc::clone(&live),
    };
    progress(&format!("history download {}", file.part.filename));
    set_live_phase(&live, "waiting for network slot");
    let source = match wikimak_mediawiki::fetch(client, &file.part) {
        Ok(source) => source,
        Err(error) => {
            if let Ok(mut state) = live.lock() {
                state.value.phase = format!("download failed: {error}");
            }
            persist_live_progress(&live, true);
            return Err(Error::Mediawiki(error));
        }
    };
    let fetch_stats = source.stats_handle();
    let mut source = CountingReader {
        inner: source,
        read_bytes: 0,
        last_sync: Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now),
        state: Arc::clone(&live),
        stats: Arc::clone(&fetch_stats),
    };
    source.sync_stats(true);
    progress(&format!("history decompress/parse {}", file.part.filename));
    set_live_phase(&live, "streaming source pipeline");
    let decoder = wikimak_mediawiki::new_bz2_reader(
        CancelReader {
            inner: source,
            cancelled,
        },
        wikimak_mediawiki::Bz2Options {
            workers: bz2_workers,
        },
    );
    let decoder = DecodedCountingReader {
        inner: Box::new(decoder),
        read_bytes: 0,
        last_sync: Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now),
        state: Arc::clone(&live),
    };
    let mut sorter = RecordSorter::new_with_run_target(scratch, HISTORY_SORT_RUN_TARGET)
        .map_err(map_archive)?;
    let mut title_sorter = RecordSorter::new_with_run_target(scratch, HISTORY_SORT_RUN_TARGET)
        .map_err(map_archive)?;
    let mut stats = PartialStats::default();
    let mut last_activity = Instant::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(Instant::now);
    let mut history_bytes = 0_u64;
    set_live_phase(&live, "streaming and parsing TSV");
    for (line_number, line) in std::io::BufReader::new(decoder).lines().enumerate() {
        let line = line?;
        history_bytes = history_bytes.saturating_add(line.len() as u64);
        if last_activity.elapsed() >= Duration::from_secs(2) {
            if let Ok(mut state) = live.lock() {
                state.value.revisions = line_number.saturating_add(1) as u64;
                state.value.text_bytes = history_bytes;
            }
            persist_live_progress(&live, true);
            progress(&format!(
                "history parse {} line {}",
                file.part.filename,
                line_number + 1
            ));
            last_activity = Instant::now();
        }
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
            if matches!(record, Record::PageState { .. } | Record::PageAction { .. }) {
                title_sorter.push(record.clone()).map_err(map_archive)?;
            }
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
    let title_path = path.with_extension("titles.swdump");
    let (_, _, _) = title_sorter
        .finish(
            std::fs::File::create(&title_path)?,
            DEFAULT_FRAME_TARGET,
        )
        .map_err(map_archive)?;
    // Capture the final retry/HTTP counters before the last durable progress
    // snapshot.  Previously this happened after the sidecar was removed,
    // leaving the historical accounting one fetch behind.
    stats.record_fetch(&fetch_stats);
    if let Ok(mut state) = live.lock() {
        state.value.phase = "finished".into();
        state.value.revisions = stats.events;
        state.value.text_bytes = history_bytes;
    }
    persist_live_progress(&live, true);
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
            if !path.with_extension("titles.swdump").is_file() {
                pending.push((index, group, key));
                continue;
            }
            reused_bytes = reused_bytes
                .saturating_add(group.iter().map(|part| part.size_bytes).sum::<u64>());
            reused.push((
                index,
                ContentPartResult {
                    title_path: path.with_extension("titles.swdump"),
                    path,
                    stats,
                    site_info,
                    samples: None,
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
            let titles = path.with_extension("titles.swdump");
            if titles.exists() {
                std::fs::remove_file(titles)?;
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
                    false,
                    None,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HistoryPartsGeometry {
    /// A non-empty pending set has exactly one live target owner.
    target_owners: usize,
    /// The sole live target receives the complete decoder allocation.
    decoder_workers: usize,
}

/// Select history-target admission and decoder allocation.
///
/// History targets share one destination-local cache, external-sort workspace,
/// and output device.  A non-empty pending set therefore has one owner, while
/// its bzip2 decoder may use every available processing slot.  No target is
/// materialized when every target is reusable.
fn history_parts_geometry(file_count: usize, cores: usize) -> HistoryPartsGeometry {
    HistoryPartsGeometry {
        target_owners: if file_count == 0 { 0 } else { 1 },
        decoder_workers: cores.max(1),
    }
}

/// A make-level history recipe is admitted one at a time, so its decoder
/// window is the complete configured processing budget rather than the
/// per-recipe share used while several content recipes are admitted.
fn history_decoder_workers(configured: usize) -> usize {
    configured.max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GroupedSourceGeometry {
    /// Number of source materializations admitted within one content target.
    source_concurrency: usize,
    /// Decoder in-flight window assigned to each admitted source.
    decoder_workers: usize,
}

/// Choose the nested source geometry for a grouped content target.
///
/// Direct-build targets retain each completed source archive so an interrupted
/// target can reuse it.  They therefore process the sources one at a time and
/// give the sole live source the whole target decoder window.  This keeps one
/// outer target from multiplying its disk writers while the process-wide
/// decoder pool can still use every slot available to that target.
///
/// The legacy builder has a distinct caller and contract: preserve its
/// previous source-parallel / per-source decoder split there.
fn grouped_source_geometry(
    part_count: usize,
    bz2_workers: usize,
    direct_build: bool,
) -> GroupedSourceGeometry {
    let part_count = part_count.max(1);
    let bz2_workers = bz2_workers.max(1);
    if direct_build {
        return GroupedSourceGeometry {
            source_concurrency: 1,
            decoder_workers: bz2_workers,
        };
    }
    let source_concurrency = part_count.min(bz2_workers).max(1);
    GroupedSourceGeometry {
        source_concurrency,
        decoder_workers: (bz2_workers / source_concurrency).max(1),
    }
}

fn build_content_group(
    client: &Client,
    parts: &[wikimak_mediawiki::Part],
    index: usize,
    scratch: &Path,
    bz2_workers: usize,
    observed_at_micros: i64,
    retain_live_progress_until_publish: bool,
    sample_capacity: Option<usize>,
    progress: &(impl Fn(&str) + Sync),
) -> Result<ContentPartResult> {
    let path = scratch.join(format!("content-{index:06}.swdump"));
    if parts.len() > 1 {
        let geometry = grouped_source_geometry(
            parts.len(),
            bz2_workers,
            retain_live_progress_until_publish,
        );
        let mut pending = Vec::new();
        let mut reused = Vec::new();
        for (part_index, part) in parts.iter().cloned().enumerate() {
            let part_path = scratch.join(format!(
                "content-{index:06}-source-{part_index:06}.swdump"
            ));
            let key = checkpoint_key(
                "content-source",
                observed_at_micros,
                [PlannedPart::from(&part)],
            )?;
            let saved = retain_live_progress_until_publish
                .then(|| {
                    checkpoint_stats(&part_path, &key).map(|stats| {
                        read_site_info_checkpoint(&part_path)
                            .map(|site_info| ContentPartResult {
                                title_path: part_path.with_extension("titles.swdump"),
                                path: part_path.clone(),
                                stats,
                                site_info,
                                samples: part_path
                                    .with_extension("samples")
                                    .exists()
                                    .then(|| part_path.with_extension("samples")),
                            })
                    })
                })
                .flatten()
                .transpose()?;
            if let Some(result) = saved.filter(|result| {
                result.title_path.is_file()
                    && (sample_capacity.is_none() || result.samples.is_some())
            }) {
                progress(&format!("reusing completed source {}", part.filename));
                reused.push((part_index, result));
                continue;
            }
            for stale in [
                part_path.clone(),
                checkpoint_receipt_path(&part_path),
                site_info_checkpoint_path(&part_path),
                part_path.with_extension("titles.swdump"),
            ] {
                if stale.exists() {
                    std::fs::remove_file(stale)?;
                }
            }
            pending.push((part_index, part, key));
        }
        let queue = Arc::new(Mutex::new(VecDeque::from(pending)));
        let results = Arc::new(Mutex::new(reused));
        let failure = Arc::new(Mutex::new(None));
        std::thread::scope(|scope| {
            for _ in 0..geometry.source_concurrency {
                let queue = Arc::clone(&queue);
                let results = Arc::clone(&results);
                let failure = Arc::clone(&failure);
                scope.spawn(move || loop {
                    if failure.lock().expect("failure mutex").is_some() {
                        return;
                    }
                    let Some((part_index, part, key)) =
                        queue.lock().expect("queue mutex").pop_front()
                    else {
                        return;
                    };
                    let part_path = scratch.join(format!(
                        "content-{index:06}-source-{part_index:06}.swdump"
                    ));
                    match build_content_part(
                        client,
                        &part,
                        &part_path,
                        geometry.decoder_workers,
                        observed_at_micros,
                        retain_live_progress_until_publish,
                        sample_capacity
                            .map(|capacity| capacity.div_ceil(parts.len().max(1))),
                        progress,
                    ) {
                        Ok(result) => {
                            if retain_live_progress_until_publish {
                                if let Err(error) = write_site_info_checkpoint(
                                    &result.path,
                                    observed_at_micros,
                                    result.site_info.as_ref(),
                                )
                                .and_then(|_| {
                                    write_checkpoint_receipt(
                                        &result.path,
                                        &key,
                                        &result.stats,
                                    )
                                }) {
                                    *failure.lock().expect("failure mutex") = Some(error);
                                    return;
                                }
                            }
                            results
                                .lock()
                                .expect("results mutex")
                                .push((part_index, result));
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
        results.sort_by_key(|(part_index, _)| *part_index);
        let inputs = results
            .iter()
            .map(|(_, result)| result.path.clone())
            .collect::<Vec<_>>();
        let title_inputs = results
            .iter()
            .map(|(_, result)| result.title_path.clone())
            .collect::<Vec<_>>();
        let mut stats = PartialStats::default();
        let mut site_info = None;
        for (_, result) in &results {
            stats.merge_from(&result.stats);
            if site_info.is_none() {
                site_info = result.site_info.clone();
            }
        }
        crate::archive::merge_many_archives(
            &inputs,
            std::fs::File::create(&path)?,
            DEFAULT_FRAME_TARGET,
        )
        .map_err(map_archive)?;
        let title_path = path.with_extension("titles.swdump");
        crate::archive::merge_many_archives(
            &title_inputs,
            std::fs::File::create(&title_path)?,
            DEFAULT_FRAME_TARGET,
        )
        .map_err(map_archive)?;
        let samples = if let Some(capacity) = sample_capacity {
            let samples = path.with_extension("samples");
            let mut writer = NewestTextSampleWriter::create(&samples, capacity)?;
            for (_, result) in &results {
                let source = result
                    .samples
                    .as_ref()
                    .ok_or(Error::Corrupt("content source has no sample sidecar"))?;
                read_text_samples(source, |sample| writer.push(sample))?;
            }
            writer.finish()?;
            Some(samples)
        } else {
            None
        };
        for input in inputs {
            std::fs::remove_file(&input)?;
            let title_input = input.with_extension("titles.swdump");
            if title_input.exists() {
                std::fs::remove_file(title_input)?;
            }
            for checkpoint in [
                checkpoint_receipt_path(&input),
                site_info_checkpoint_path(&input),
                input.with_extension("samples"),
            ] {
                if checkpoint.exists() {
                    std::fs::remove_file(checkpoint)?;
                }
            }
        }
        return Ok(ContentPartResult {
            title_path,
            path,
            stats,
            site_info,
            samples,
        });
    }
    build_content_part(
        client,
        parts
            .first()
            .ok_or(Error::Corrupt("content group contains no parts"))?,
        &path,
        bz2_workers,
        observed_at_micros,
        retain_live_progress_until_publish,
        sample_capacity,
        progress,
    )
}

fn build_content_part(
    client: &Client,
    part: &wikimak_mediawiki::Part,
    path: &Path,
    bz2_workers: usize,
    observed_at_micros: i64,
    retain_live_progress_until_publish: bool,
    sample_capacity: Option<usize>,
    progress: &(impl Fn(&str) + Sync),
) -> Result<ContentPartResult> {
    build_content_part_with_run_target(
        client,
        part,
        path,
        bz2_workers,
        observed_at_micros,
        retain_live_progress_until_publish,
        sample_capacity,
        HISTORY_SORT_RUN_TARGET,
        progress,
    )
}

fn build_content_part_with_run_target(
    client: &Client,
    part: &wikimak_mediawiki::Part,
    path: &Path,
    bz2_workers: usize,
    observed_at_micros: i64,
    retain_live_progress_until_publish: bool,
    sample_capacity: Option<usize>,
    sort_run_target: usize,
    progress: &(impl Fn(&str) + Sync),
) -> Result<ContentPartResult> {
    let progress_anchor = path.with_extension("progress");
    let value = LiveTargetProgress {
        target: path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("content")
            .to_owned(),
        part: part.filename.clone(),
        phase: "starting".into(),
        source_bytes_total: part.size_bytes,
        started_at_micros: now_micros(),
        updated_at_micros: now_micros(),
        ..Default::default()
    };
    let live = Arc::new(Mutex::new(LiveProgressState {
        projection: crate::progress_projection::source_writer(
            &progress_scratch_root(&progress_anchor),
            &value.target,
            &value.part,
        )
        .ok(),
        value,
        last_write: Instant::now()
            .checked_sub(Duration::from_secs(3))
            .unwrap_or_else(Instant::now),
        last_phase: "starting".into(),
    }));
    persist_live_progress(&live, true);
    let _heartbeat = LiveProgressHeartbeat::start(&live);
    let _failure_guard = LiveProgressFailureGuard {
        state: Arc::clone(&live),
    };
    let output = std::fs::File::create(path)?;
    let mut writer = ParallelArchiveWriter::new_without_reference(
        output,
        DEFAULT_FRAME_TARGET,
        CompressionSettings::default(),
        bz2_workers.max(1),
    )
    .map_err(map_archive)?;
    let sort_scratch = path
        .parent()
        .ok_or(Error::Corrupt("content output has no scratch parent"))?;
    let mut sorter = RecordSorter::new_with_run_target(sort_scratch, sort_run_target)
        .map_err(map_archive)?;
    let title_path = path.with_extension("titles.swdump");
    let mut title_writer = ArchiveWriter::new(
        std::fs::File::create(&title_path)?,
        DEFAULT_FRAME_TARGET,
    )
    .map_err(map_archive)?;
    let samples = sample_capacity.map(|capacity| (path.with_extension("samples"), capacity));
    let mut sample_writer = samples
        .as_ref()
        .map(|(path, capacity)| NewestTextSampleWriter::create(path, *capacity))
        .transpose()?;
    let mut stats = PartialStats::default();
    let mut site_info = None;
    progress(&format!("content download {}", part.filename));
    set_live_phase(&live, "waiting for network slot");
    let source = match wikimak_mediawiki::fetch(client, part) {
        Ok(source) => source,
        Err(error) => {
            if let Ok(mut state) = live.lock() {
                state.value.phase = format!("download failed: {error}");
            }
            persist_live_progress(&live, true);
            return Err(Error::Mediawiki(error));
        }
    };
    let fetch_stats = source.stats_handle();
    let mut source = CountingReader {
        inner: source,
        read_bytes: 0,
        last_sync: Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now),
        state: Arc::clone(&live),
        stats: Arc::clone(&fetch_stats),
    };
    source.sync_stats(true);
    progress(&format!("content decompress/parse {}", part.filename));
    set_live_phase(&live, "streaming source pipeline");
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
    let input = DecodedCountingReader {
        inner: input,
        read_bytes: 0,
        last_sync: Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now),
        state: Arc::clone(&live),
    };
    let mut page_stream = wikimak_mediawiki::new_page_stream(input);
    let revisions = page_stream.revisions_mut();
    let mut last_activity = Instant::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(Instant::now);
    let mut pages_seen = 0_u64;
    let mut revisions_seen = 0_u64;
    let mut text_bytes = 0_u64;
    let mut last_page_id = None;
    set_live_phase(&live, "streaming XML to sorted archive");
    loop {
        let Some(header) = revisions.next_page() else {
            break;
        };
        let header = header?;
        pages_seen = pages_seen.saturating_add(1);
        if site_info.is_none() {
            site_info = revisions.site_info().map(convert_site_info);
        }
        let page_id = u64::try_from(header.id)
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| parse_error(format!("invalid page id {}", header.id)))?;
        if last_page_id.is_some_and(|previous| page_id <= previous) {
            return Err(parse_error(format!(
                "{} has page id {page_id} after {}",
                part.filename,
                last_page_id.expect("checked above")
            )));
        }
        last_page_id = Some(page_id);
        if let Ok(mut state) = live.lock() {
            state.value.pages = pages_seen;
            state.value.current_page = page_id;
            state.value.current_title = header.title.clone();
        }
        if last_activity.elapsed() >= Duration::from_secs(2) {
            progress(&format!(
                "content encode {} page {} #{} ({}); {} revisions, {} text",
                part.filename,
                pages_seen,
                page_id,
                header.title,
                revisions_seen,
                human_progress_bytes(text_bytes),
            ));
            persist_live_progress(&live, true);
            last_activity = Instant::now();
        }
        let page_revisions_before = revisions_seen;
        let page_state = Record::PageState {
            page_id,
            timestamp_micros: observed_at_micros,
            title: header.title.clone(),
            namespace: None,
            deleted: false,
        };
        writer.write(&page_state).map_err(map_archive)?;
        title_writer.write(&page_state).map_err(map_archive)?;
        loop {
            let Some(revision) = revisions.next_revision() else {
                break;
            };
            let revision = convert_revision(revision.map_err(Error::Mediawiki)?)?;
            text_bytes = text_bytes.saturating_add(revision.meta.text_len);
            sorter
                .push(Record::Revision { page_id, revision })
                .map_err(map_archive)?;
            revisions_seen = revisions_seen.saturating_add(1);
            if last_activity.elapsed() >= Duration::from_secs(2) {
                if let Ok(mut state) = live.lock() {
                    state.value.revisions = revisions_seen;
                    state.value.text_bytes = text_bytes;
                }
                progress(&format!(
                    "content stream {} page #{} ({}); {} revisions, {} text",
                    part.filename,
                    page_id,
                    header.title,
                    revisions_seen,
                    human_progress_bytes(text_bytes),
                ));
                persist_live_progress(&live, true);
                last_activity = Instant::now();
            }
        }
        let mut sampled = false;
        sorter
            .drain_batch(|record| {
                if !sampled {
                    if let Record::Revision { revision, .. } = &record {
                        if revision.has_text {
                            if let Some(samples) = sample_writer.as_mut() {
                                samples
                                    .push(&revision.text)
                                    .map_err(ArchiveError::Mirror)?;
                            }
                            sampled = true;
                        }
                    }
                }
                writer.write(&record)
            })
            .map_err(map_archive)?;
        stats.pages += 1;
        stats.revisions += revisions_seen - page_revisions_before;
    }
    set_live_phase(&live, "sealing sorted archive");
    persist_live_progress(&live, true);
    let (output, _) = writer.finish().map_err(map_archive)?;
    output.sync_all()?;
    let (title_output, _) = title_writer.finish().map_err(map_archive)?;
    title_output.sync_all()?;
    if let Some(samples) = sample_writer {
        samples.finish()?;
    }
    // The historical snapshot is the build-wide source of network totals
    // after the live sidecar is removed, so it must include the final fetch
    // counters.
    stats.record_fetch(&fetch_stats);
    if let Ok(mut state) = live.lock() {
        state.value.phase = "finished".into();
        state.value.pages = pages_seen;
        state.value.revisions = revisions_seen;
        state.value.text_bytes = text_bytes;
    }
    persist_live_progress(&live, true);
    if !retain_live_progress_until_publish {
    }
    Ok(ContentPartResult {
        path: path.to_path_buf(),
        title_path,
        stats,
        site_info,
        samples: samples.map(|(path, _)| path),
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

fn archive_file_complete(path: &Path) -> bool {
    if path.is_dir() {
        let Ok(set) = crate::archive_set::ArchiveSetReader::open(path) else {
            return false;
        };
        let Some(completion) = set.segments().last() else {
            return false;
        };
        crate::archive::has_clean_completion_marker(path.join(&completion.name))
            .unwrap_or(false)
    } else {
        crate::archive::has_clean_completion_marker(path).unwrap_or(false)
    }
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
    let value = (
        kind,
        observed_at_micros,
        parts.into_iter().collect::<Vec<_>>(),
    );
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

fn map_invalid_build(error: crate::build_lifecycle::InvalidBuildState) -> Error {
    Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn parse_error(message: String) -> Error {
    Error::Mediawiki(wikimak_mediawiki::Error::Parse(message))
}

#[cfg(test)]
mod build_graph_tests {
    use httpmock::Method::GET;
    use httpmock::MockServer;

    use super::*;

    struct CountedRecord {
        record: Option<Record>,
        calls: Arc<AtomicU64>,
    }

    impl RecordSource for CountedRecord {
        fn next_record(&mut self) -> crate::archive::Result<Option<Record>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.record.take())
        }
    }

    #[test]
    fn update_tail_merge_is_direct_below_the_fan_in_bound() {
        let root = tempfile::tempdir().unwrap();
        let output_path = root.path().join("tail.swdump");
        let mut output = std::fs::File::create(&output_path).unwrap();
        let calls = (0..7)
            .map(|_| Arc::new(AtomicU64::new(0)))
            .collect::<Vec<_>>();
        let sources = (1..=7_u64)
            .rev()
            .zip(calls.iter())
            .map(|(page_id, calls)| {
                Box::new(CountedRecord {
                    record: Some(Record::PageState {
                        page_id,
                        timestamp_micros: 1,
                        title: format!("Page {page_id}"),
                        namespace: None,
                        deleted: false,
                    }),
                    calls: Arc::clone(calls),
                }) as Box<dyn RecordSource>
            })
            .collect();
        let (frames, records) = merge_record_sources_bounded(
            sources,
            &mut output,
            &root.path().join("merge-work"),
            1024,
            CompressionSettings::default(),
        )
        .unwrap();
        assert_ne!(frames, 0);
        assert_eq!(records, 7);
        assert!(calls
            .iter()
            .all(|calls| calls.load(Ordering::Relaxed) == 2));
        assert!(!root.path().join("merge-work").exists());
    }

    #[test]
    fn update_tail_merge_bounds_fan_in_above_sixty_four_sources() {
        let root = tempfile::tempdir().unwrap();
        let output_path = root.path().join("tail.swdump");
        let mut output = std::fs::File::create(&output_path).unwrap();
        let calls = (0..130)
            .map(|_| Arc::new(AtomicU64::new(0)))
            .collect::<Vec<_>>();
        let sources = (1..=130_u64)
            .rev()
            .zip(calls.iter())
            .map(|(page_id, calls)| {
                Box::new(CountedRecord {
                    record: Some(Record::PageState {
                        page_id,
                        timestamp_micros: 1,
                        title: format!("Page {page_id}"),
                        namespace: None,
                        deleted: false,
                    }),
                    calls: Arc::clone(calls),
                }) as Box<dyn RecordSource>
            })
            .collect();
        let (frames, records) = merge_record_sources_bounded(
            sources,
            &mut output,
            &root.path().join("merge-work"),
            1024,
            CompressionSettings::default(),
        )
        .unwrap();
        drop(output);
        assert!(frames != 0);
        assert_eq!(records, 130);
        assert!(!root.path().join("merge-work").exists());
        assert!(calls
            .iter()
            .all(|calls| calls.load(Ordering::Relaxed) == 2));
        assert_eq!(crate::archive::MAX_SORTED_MERGE_FAN_IN, 64);
        let mut reader = ArchiveRecordReader::open(&output_path).unwrap();
        for expected in 1..=130_u64 {
            assert_eq!(
                reader.next_record().unwrap().unwrap().entity().id,
                expected
            );
        }
        assert!(reader.next_record().unwrap().is_none());
    }

    #[test]
    fn update_tail_merge_preserves_unowned_workspace_entries() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("merge-work");
        std::fs::create_dir_all(&work).unwrap();
        let sentinel = work.join("foreign-sentinel");
        std::fs::write(&sentinel, b"keep me").unwrap();
        let output_path = root.path().join("tail.swdump");
        let mut output = std::fs::File::create(&output_path).unwrap();
        let source = Box::new(CountedRecord {
            record: Some(Record::PageState {
                page_id: 1,
                timestamp_micros: 1,
                title: "Page 1".into(),
                namespace: None,
                deleted: false,
            }),
            calls: Arc::new(AtomicU64::new(0)),
        }) as Box<dyn RecordSource>;

        let error = merge_record_sources_bounded(
            vec![source],
            &mut output,
            &work,
            1024,
            CompressionSettings::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no ownership manifest"));
        assert!(sentinel.exists());
        assert!(!work.join(UPDATE_TAIL_MERGE_WORK_MANIFEST).exists());
    }

    #[test]
    fn target_cleanup_claims_same_name_same_size_replacement_as_foreign() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("target.swdump");
        std::fs::write(&source, b"old-data").unwrap();
        let (bytes, identity) = cleanup_file_identity(&source).unwrap();
        let entries = vec![CleanupInventoryEntry {
            name: "target.swdump".into(),
            bytes,
            identity,
            state: CleanupEntryState::Planned,
        }];
        let (operation, mut inventory) =
            create_or_resume_cleanup_inventory(
                &root.path().join(".sarun-quarantine"),
                "target-cleanup-test",
                entries,
            )
            .unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"new-data").unwrap();
        std::fs::rename(&replacement, &source).unwrap();

        let inventory_path = operation.join("cleanup.json");
        claim_cleanup_entry(
            &source,
            &operation,
            0,
            &inventory_path,
            &mut inventory,
        )
        .unwrap();

        assert!(!source.exists());
        assert_eq!(std::fs::read(operation.join("target.swdump")).unwrap(), b"new-data");
        assert_eq!(inventory.entries[0].state, CleanupEntryState::Foreign);
    }

    #[test]
    fn resumable_cleanup_rejects_manifest_path_escape_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let quarantine = root.path().join(".sarun-quarantine");
        let sentinel = root.path().join("outside-sentinel");
        std::fs::write(&sentinel, b"must survive").unwrap();
        let expected = vec![CleanupInventoryEntry {
            name: "owned.swdump".into(),
            bytes: 8,
            identity: Some("receipt-owned-identity".into()),
            state: CleanupEntryState::Planned,
        }];
        let (operation, _) = create_or_resume_cleanup_inventory(
            &quarantine,
            "target-cleanup-test",
            expected.clone(),
        )
        .unwrap();
        let tampered = CleanupInventory {
            schema: CLEANUP_MANIFEST_SCHEMA,
            operation: "target-cleanup-test".into(),
            entries: vec![CleanupInventoryEntry {
                name: sentinel.to_string_lossy().into_owned(),
                bytes: std::fs::metadata(&sentinel).unwrap().len(),
                identity: cleanup_file_identity(&sentinel).unwrap().1,
                state: CleanupEntryState::Claimed,
            }],
        };
        persist_cleanup_inventory(&operation.join("cleanup.json"), &tampered).unwrap();

        let error = create_or_resume_cleanup_inventory(
            &quarantine,
            "target-cleanup-test",
            expected,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unsafe entry name"));
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"must survive");
    }

    #[test]
    fn resumable_cleanup_rejects_changed_receipt_identity() {
        let root = tempfile::tempdir().unwrap();
        let quarantine = root.path().join(".sarun-quarantine");
        let expected = vec![CleanupInventoryEntry {
            name: "owned.swdump".into(),
            bytes: 8,
            identity: Some("receipt-owned-identity".into()),
            state: CleanupEntryState::Planned,
        }];
        let (operation, mut inventory) = create_or_resume_cleanup_inventory(
            &quarantine,
            "target-cleanup-test",
            expected.clone(),
        )
        .unwrap();
        inventory.entries[0].identity = Some("attacker-selected-identity".into());
        persist_cleanup_inventory(&operation.join("cleanup.json"), &inventory).unwrap();

        let error = create_or_resume_cleanup_inventory(
            &quarantine,
            "target-cleanup-test",
            expected,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("does not match the validated ownership receipt"));
    }

    #[test]
    fn update_tail_merge_same_name_same_size_replacement_survives_quarantine() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("merge-work");
        std::fs::create_dir_all(&work).unwrap();
        let manifest = write_update_tail_merge_manifest(&work, 65).unwrap();
        let name = manifest.artifacts[0].clone();
        let path = work.join(&name);
        std::fs::write(&path, b"old-data").unwrap();
        record_update_tail_merge_artifact(&work, &name).unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"new-data").unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        clear_update_tail_merge_workspace(&work).unwrap();

        assert!(!path.exists());
        let operation = std::fs::read_dir(root.path().join(".sarun-quarantine"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|entry| entry.join(&name).exists())
            .expect("replacement must be claimed into quarantine");
        assert_eq!(std::fs::read(operation.join(&name)).unwrap(), b"new-data");
        assert!(!work.exists());
        assert!(operation.join("cleanup.json").exists());
    }

    fn lifecycle_test_plan() -> DirectBuildPlan {
        let mut plan = DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
            wiki_db: "testwiki".into(),
            content_snapshot: "2024-06-01".into(),
            metadata_snapshot: "2024-06".into(),
            observed_at_micros: 1,
            frame_target: 1,
            range_target: 1,
            compression_level: 1,
            ref_prefix_sample_bytes: 2,
            ref_prefix_bytes: 1,
            content_groups: Vec::new(),
            history_files: Vec::new(),
        };
        plan.plan_id = canonical_direct_plan_id(&plan).unwrap();
        plan
    }

    #[test]
    fn progress_root_follows_projection_ownership_not_partial_node_spelling() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("build");
        let node = root.join("nodes/content-000000.partial");
        std::fs::create_dir_all(&node).unwrap();
        std::fs::write(root.join("plan.json"), b"plan evidence").unwrap();
        std::fs::write(root.join("progress.bin"), b"projection evidence").unwrap();
        let anchor = node.join("content-000000.progress");

        assert_eq!(progress_scratch_root(&anchor), root);

        let foreign = directory.path().join("foreign-progress");
        #[cfg(unix)]
        {
            std::fs::remove_file(root.join("progress.bin")).unwrap();
            std::os::unix::fs::symlink(&foreign, root.join("progress.bin")).unwrap();
            assert_eq!(progress_scratch_root(&anchor), node);
        }
    }

    #[test]
    fn counting_reader_drop_publishes_final_coalesced_counters() {
        let value = LiveTargetProgress {
            target: "content-000000".into(),
            part: "content.xml.bz2".into(),
            phase: "finished".into(),
            ..Default::default()
        };
        let live = Arc::new(Mutex::new(LiveProgressState {
            projection: None,
            value,
            last_write: Instant::now(),
            last_phase: "finished".into(),
        }));
        let fetch = Arc::new(Mutex::new(wikimak_mediawiki::FetchStats {
            attempts: 1,
            bytes_received: 6,
            ..Default::default()
        }));
        {
            let mut reader = CountingReader {
                inner: std::io::Cursor::new(b"source"),
                read_bytes: 0,
                last_sync: Instant::now(),
                state: Arc::clone(&live),
                stats: fetch,
            };
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"source");
            assert_eq!(live.lock().unwrap().value.source_bytes_read, 0);
        }
        let live = live.lock().unwrap();
        assert_eq!(live.value.source_bytes_read, 6);
        assert_eq!(live.value.fetch_attempts, 1);
        assert_eq!(live.value.fetch_bytes_received, 6);
        assert_eq!(live.value.fetch_server_timed_retries, 0);
        assert_eq!(live.value.fetch_robots_timed_retries, 0);
        assert_eq!(live.value.fetch_fallback_timed_retries, 0);
        assert_eq!(live.value.fetch_local_spacing_timed_retries, 0);
    }

    #[test]
    fn partial_stats_merge_retry_timing_counters_is_additive() {
        let mut first = PartialStats {
            fetch_server_timed_retries: 2,
            fetch_robots_timed_retries: 3,
            fetch_fallback_timed_retries: 4,
            fetch_local_spacing_timed_retries: 5,
            ..Default::default()
        };
        let second = PartialStats {
            fetch_server_timed_retries: 6,
            fetch_robots_timed_retries: 7,
            fetch_fallback_timed_retries: 8,
            fetch_local_spacing_timed_retries: 9,
            ..Default::default()
        };
        first.merge_from(&second);
        assert_eq!(first.fetch_server_timed_retries, 8);
        assert_eq!(first.fetch_robots_timed_retries, 10);
        assert_eq!(first.fetch_fallback_timed_retries, 12);
        assert_eq!(first.fetch_local_spacing_timed_retries, 14);
    }

    #[test]
    fn live_resource_sample_includes_process_wide_bz2_admission() {
        let admission = wikimak_mediawiki::bz2_admission_stats();
        let mut value = LiveTargetProgress::default();
        sample_live_resource_telemetry(&mut value);
        assert_eq!(
            value.bz2_admission_limit,
            u64::try_from(admission.limit).unwrap_or(u64::MAX)
        );
        assert_eq!(
            value.bz2_admission_active_decoders,
            u64::try_from(admission.active_decoders).unwrap_or(u64::MAX)
        );
        assert_eq!(
            value.bz2_admission_peak_active_decoders,
            u64::try_from(admission.peak_active_decoders).unwrap_or(u64::MAX)
        );
    }



    #[test]
    fn committed_target_retirement_preserves_nested_unowned_entries() {
        let root = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let content = include_bytes!("../tests/data/export_three_pages.xml");
        let source = server.mock(|when, then| {
            when.method(GET).path("/content.xml");
            then.status(200).body(content);
        });
        let mut plan = lifecycle_test_plan();
        plan.observed_at_micros = 2_000_000_000_000_000;
        plan.content_groups = vec![vec![PlannedPart {
            url: server.url("/content.xml"),
            filename: "content.xml".into(),
            size_bytes: content.len() as u64,
            sha256: None,
            sha1: None,
            md5: None,
        }]];
        plan.plan_id = canonical_direct_plan_id(&plan).unwrap();
        std::fs::create_dir(root.path().join("nodes")).unwrap();
        crate::build_lifecycle::commit_plan(root.path(), &plan).unwrap();
        materialize_direct_build_node(
            &Client::new(),
            root.path(),
            &plan,
            "content",
            0,
            1,
            &|_| {},
        )
        .unwrap();
        let node = node_path(root.path(), &plan, "content", 0);
        std::fs::create_dir_all(node.join("foreign/nested")).unwrap();
        std::fs::write(node.join("foreign/nested/sentinel"), b"keep me").unwrap();

        assert!(matches!(
            crate::build_lifecycle::inspect_build(root.path(), Some(&plan.plan_id)).unwrap(),
            crate::build_lifecycle::BuildState::ReadyForAssembly { .. }
        ));

        retire_validated_target_directory(
            root.path(),
            &plan,
            crate::build_lifecycle::TargetKind::Content,
            0,
        )
        .unwrap();

        assert!(!node.exists());
        let quarantined = std::fs::read_dir(root.path().join(".sarun-quarantine"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find_map(|path| {
                std::fs::read_dir(&path)
                    .ok()?
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|entry| entry.join("nested/sentinel").exists())
            })
            .expect("nested foreign entry must be quarantined intact");
        assert_eq!(
            std::fs::read(quarantined.join("nested/sentinel")).unwrap(),
            b"keep me"
        );
        for name in COMMITTED_TARGET_FILES {
            assert!(!node.join(name).exists());
        }
        assert_eq!(source.hits(), 1);
    }

    #[test]
    fn name_complete_assembly_without_clean_done_is_not_installed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir(root.join("nodes")).unwrap();
        let mut plan = lifecycle_test_plan();
        plan.content_groups = vec![vec![PlannedPart {
            url: "https://example.invalid/content.xml".into(),
            filename: "content.xml".into(),
            size_bytes: 0,
            sha256: None,
            sha1: None,
            md5: None,
        }]];
        plan.plan_id = canonical_direct_plan_id(&plan).unwrap();

        let node = root.join("nodes/.candidate");
        std::fs::create_dir(&node).unwrap();
        let data = node.join("data.swdump");
        ArchiveWriter::new(std::fs::File::create(&data).unwrap(), 128)
            .unwrap()
            .finish()
            .unwrap();
        ArchiveWriter::new(
            std::fs::File::create(node.join("title-records.swdump")).unwrap(),
            128,
        )
        .unwrap()
        .finish()
        .unwrap();
        std::fs::write(node.join("newest.samples"), b"").unwrap();
        crate::frame_directory::write_from_archive(
            &data,
            node.join("data.swframe"),
            crate::build_lifecycle::target_frame_directory_identity(
                &plan,
                crate::build_lifecycle::TargetKind::Content,
                0,
            )
            .unwrap(),
        )
        .unwrap();
        let mut siteinfo = ArchiveWriter::new(
            std::fs::File::create(node.join("siteinfo.swdump")).unwrap(),
            128,
        )
        .unwrap();
        siteinfo
            .write(&Record::SiteInfo {
                timestamp_micros: 1,
                site_info: SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        siteinfo.finish().unwrap();
        publish_node(
            root,
            &plan,
            "content",
            0,
            &node,
            &PartialStats::default(),
        )
        .unwrap();

        let assembly = root.join(format!("assembly-{}.partial", plan.plan_id));
        std::fs::create_dir(&assembly).unwrap();
        std::fs::write(
            assembly.join("0000-reference.swdump-part"),
            b"name-only reference",
        )
        .unwrap();
        std::fs::write(
            assembly.join("9999-complete.swdump-part"),
            b"not a DONE frame",
        )
        .unwrap();
        assert!(
            crate::archive_set::ArchiveSetReader::open(&assembly).is_ok(),
            "the old fast-path predicate accepted this shape"
        );
        assert!(!archive_file_complete(&assembly));

        assert!(assemble_direct_build(root, &plan, &|_| {}).is_err());
        assert!(assembly.exists(), "invalid checkpoint was renamed away");
        assert!(!root.join("archive.swdump").exists());
        assert!(node_path(root, &plan, "content", 0).exists());
    }


    #[test]
    fn content_overlap_groups_are_single_logical_targets() {
        let part = |name: &str| PlannedPart {
            url: format!("https://example.invalid/{name}"),
            filename: name.into(),
            size_bytes: 1,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let plan = DirectBuildPlan {
            schema: 1,
            plan_id: "test".into(),
            wiki_db: "testwiki".into(),
            content_snapshot: "2024-06-01".into(),
            metadata_snapshot: "2024-06".into(),
            observed_at_micros: 0,
            frame_target: 1,
            range_target: 1,
            compression_level: 1,
            ref_prefix_sample_bytes: 1,
            ref_prefix_bytes: 1,
            content_groups: vec![
                vec![part("one")],
                vec![part("slice-a"), part("slice-b")],
                vec![part("three")],
            ],
            history_files: Vec::new(),
        };
        assert_eq!(plan.content_target_count(), 3);
        assert_eq!(plan.target_name("content", 0).as_deref(), Some("content-000000"));
        assert_eq!(
            plan.target_name("content", 1).as_deref(),
            Some("content-000001")
        );
        assert_eq!(plan.target_name("content", 2).as_deref(), Some("content-000002"));
    }

    #[test]
    fn newest_revision_sample_budget_is_exact_and_source_proportional() {
        let part = |name: &str, size_bytes| PlannedPart {
            url: format!("https://example.invalid/{name}"),
            filename: name.into(),
            size_bytes,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let mut plan = lifecycle_test_plan();
        plan.ref_prefix_sample_bytes = 5;
        plan.content_groups = vec![
            vec![part("small", 1)],
            vec![part("large", 3)],
        ];
        assert_eq!(content_sample_quotas(&plan), vec![1, 4]);

        plan.content_groups = vec![
            vec![part("first", 0)],
            vec![part("second", 0)],
        ];
        assert_eq!(content_sample_quotas(&plan), vec![3, 2]);
        assert_eq!(content_sample_quotas(&plan).into_iter().sum::<usize>(), 5);
    }

    #[test]
    fn small_wiki_uses_all_samples_without_invoking_dictionary_training() {
        let root = tempfile::tempdir().unwrap();
        let mut plan = lifecycle_test_plan();
        plan.ref_prefix_sample_bytes = 1024;
        plan.ref_prefix_bytes = 512;
        plan.content_groups = vec![vec![PlannedPart {
            url: "https://example.invalid/tiny.xml".into(),
            filename: "tiny.xml".into(),
            size_bytes: 7,
            sha256: None,
            sha1: None,
            md5: None,
        }]];
        plan.plan_id = canonical_direct_plan_id(&plan).unwrap();
        let node = node_path(root.path(), &plan, "content", 0);
        std::fs::create_dir_all(&node).unwrap();
        let mut samples = NewestTextSampleWriter::create(&node.join("newest.samples"), 1024)
            .unwrap();
        samples.push(b"abc").unwrap();
        samples.push(b"defg").unwrap();
        samples.finish().unwrap();

        assert_eq!(distill_plan_ref_prefix(root.path(), &plan).unwrap(), b"abcdefg");
    }

    #[test]
    fn assembly_resume_does_not_decode_the_sealed_target_prefix() {
        use std::io::{Seek, SeekFrom};

        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("target.swdump");
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(&archive).unwrap(), 1).unwrap();
        for page_id in 1..=3 {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: 1,
                    title: format!("Page {page_id}"),
                    namespace: Some(0),
                    deleted: false,
                })
                .unwrap();
        }
        writer.finish().unwrap();
        let frame_directory = root.path().join("target.swframe");
        let identity = [19_u8; 32];
        crate::frame_directory::write_from_archive(
            &archive,
            &frame_directory,
            identity,
        )
        .unwrap();
        let directory =
            crate::frame_directory::FrameDirectory::open_bound(&frame_directory, identity)
                .unwrap();
        assert_eq!(directory.len(), 3);
        let suffix_compressed_bytes = (1..directory.len())
            .map(|index| directory.get(index).unwrap().compressed_bytes)
            .sum::<u64>();

        // Make decoding the sealed prefix fail decisively. A correctly
        // positioned resume never touches these bytes.
        let first = directory.get(0).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&archive)
            .unwrap();
        file.seek(SeekFrom::Start(first.compressed_offset)).unwrap();
        file.write_all(&vec![0; first.compressed_bytes as usize])
            .unwrap();
        file.sync_all().unwrap();
        drop(file);

        let accounted = Arc::new(AtomicU64::new(0));
        let mut source = SequentialTargetReaders::new(
            vec![(archive, frame_directory, identity)],
            Some(EntityKey {
                kind: EntityKind::Page,
                id: 1,
            }),
            Arc::clone(&accounted),
        );
        assert_eq!(source.next_record().unwrap().unwrap().entity().id, 2);
        assert_eq!(source.next_record().unwrap().unwrap().entity().id, 3);
        assert!(source.next_record().unwrap().is_none());
        assert_eq!(
            accounted.load(Ordering::Relaxed),
            suffix_compressed_bytes,
            "resume accounting includes only frames after the sealed entity"
        );
    }

    #[test]
    fn new_plan_range_target_stays_default_at_source_size_boundaries() {
        let default_target = crate::archive_set::DEFAULT_RANGE_TARGET;
        for source_bytes in [
            0,
            1,
            default_target - 1,
            default_target,
            default_target + 1,
            200 << 30,
            20 << 40,
            u64::MAX,
        ] {
            assert_eq!(
                planned_range_layout(source_bytes),
                default_target,
                "new-plan range target must not scale with {source_bytes} bytes of compressed input",
            );
        }
    }

    #[test]
    fn new_plan_range_target_is_hdd_sized_at_enwiki_scale() {
        let enwiki_compressed_source_bytes = 20 << 40;
        assert_eq!(
            planned_range_layout(enwiki_compressed_source_bytes),
            crate::archive_set::DEFAULT_RANGE_TARGET,
        );
    }

    #[test]
    fn persisted_plan_range_target_is_preserved_for_resume() {
        let root = tempfile::tempdir().unwrap();
        let mut plan = lifecycle_test_plan();
        let legacy_target = 160 << 30;
        plan.range_target = legacy_target;
        plan.plan_id = canonical_direct_plan_id(&plan).unwrap();
        let path = root.path().join("plan.json");
        std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();

        let resumed = read_direct_build_plan(&path).unwrap();
        assert_eq!(resumed.plan_id, plan.plan_id);
        assert_eq!(resumed.range_target, legacy_target);
        assert_eq!(
            assembly_range_target(&resumed),
            crate::archive_set::DEFAULT_RANGE_TARGET,
            "legacy identity is preserved while final assembly stays updateable",
        );
    }


    #[test]
    fn planned_ref_prefix_uses_bounded_samples_without_fastcover_search() {
        assert_eq!(
            planned_ref_prefix_layout(),
            (MIRROR_REF_PREFIX_BYTES, MIRROR_REF_PREFIX_BYTES),
        );
    }

    #[test]
    fn overlapping_content_parts_are_merged_instead_of_concatenated() {
        crate::frame_directory::reset_test_archive_set_directory_reconstructions();
        let server = MockServer::start();
        let content = include_bytes!("../tests/data/export_three_pages.xml");
        let first = server.mock(|when, then| {
            when.method(GET).path("/range.xml");
            then.status(200).body(content);
        });
        let slice = server.mock(|when, then| {
            when.method(GET).path("/slice.xml");
            then.status(200).body(content);
        });
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
            content_groups: vec![vec![
                PlannedPart {
                url: server.url("/range.xml"),
                filename: "testwiki-p1p3.xml".into(),
                size_bytes: content.len() as u64,
                sha256: None,
                sha1: None,
                md5: None,
            },
                PlannedPart {
                url: server.url("/slice.xml"),
                filename: "testwiki-p2r1r999.xml".into(),
                size_bytes: content.len() as u64,
                sha256: None,
                sha1: None,
                md5: None,
            },
            ]],
            history_files: Vec::new(),
        };
        plan.plan_id = canonical_direct_plan_id(&plan).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::create_dir(scratch.path().join("nodes")).unwrap();
        crate::build_lifecycle::commit_plan(scratch.path(), &plan).unwrap();
        assert_eq!(plan.content_target_count(), 1);
        assert_eq!(
            plan.target_name("content", 0).as_deref(),
            Some("content-000000")
        );
        materialize_direct_build_node(
            &Client::new(),
            scratch.path(),
            &plan,
            "content",
            0,
            2,
            &|_| {},
        )
        .unwrap();
        let inspected =
            crate::build_lifecycle::inspect_build(scratch.path(), Some(&plan.plan_id)).unwrap();
        assert!(inspected.targets().iter().all(|target| matches!(
            target.state,
            crate::build_lifecycle::TargetState::Ready(_)
        )));
        let archive = assemble_direct_build(scratch.path(), &plan, &|_| {}).unwrap();
        crate::archive_set::ArchiveSetReader::open(&archive).unwrap();
        let identity = crate::generation::GenerationId::from_plan_id(&plan.plan_id)
            .to_bytes()
            .unwrap();
        crate::frame_directory::FrameDirectory::open_bound(
            archive.with_extension("swframe"),
            identity,
        )
        .unwrap();
        assert_eq!(
            crate::frame_directory::test_archive_set_directory_reconstructions(),
            0,
            "fresh assembly must publish its frame directory from write-time metadata",
        );
        assert_eq!(first.hits(), 1);
        assert_eq!(slice.hits(), 1);
    }

    #[test]
    fn content_page_merges_multiple_swdump_runs_into_one_newest_first_frame() {
        let server = MockServer::start();
        let mut xml = String::from(
            r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
                <siteinfo>
                    <sitename>Run test</sitename><dbname>runtest</dbname><base>https://example.invalid/</base>
                    <generator>MediaWiki</generator><case>first-letter</case>
                    <namespaces><namespace key="0" case="first-letter"/></namespaces>
                </siteinfo>
                <page><title>Run test page</title><ns>0</ns><id>77</id>"#,
        );
        for revision_id in 1..=6_u64 {
            let text = format!("revision text {revision_id}");
            xml.push_str(&format!(
                "<revision><id>{revision_id}</id><parentid>{}</parentid>\
                 <timestamp>2024-01-{revision_id:02}T00:00:00Z</timestamp>\
                 <contributor><username>Editor {revision_id}</username><id>{revision_id}</id></contributor>\
                 <comment>comment {revision_id}</comment><model>wikitext</model><format>text/x-wiki</format>\
                 <text bytes=\"{}\" xml:space=\"preserve\">{text}</text></revision>",
                revision_id.saturating_sub(1),
                text.len(),
            ));
        }
        xml.push_str("</page></mediawiki>");
        let source = server.mock(|when, then| {
            when.method(GET).path("/content.xml");
            then.status(200).body(xml.as_bytes().to_vec());
        });

        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("content-000000.swdump");
        let part = wikimak_mediawiki::Part {
            url: server.url("/content.xml"),
            filename: "content.xml".into(),
            size_bytes: xml.len() as u64,
            sha256: None,
            sha1: None,
            md5: None,
        };
        build_content_part_with_run_target(
            &Client::new(),
            &part,
            &output,
            1,
            snapshot_date_micros(
                chrono::NaiveDate::from_ymd_opt(2024, 2, 1).unwrap(),
            )
            .unwrap(),
            true,
            None,
            1,
            &|_| {},
        )
        .unwrap();

        assert_eq!(source.hits(), 1);
        let (_, frames, complete) = crate::archive::index_file(&output).unwrap();
        assert!(complete);
        assert_eq!(frames.len(), 1, "one page must remain one final frame");

        let mut reader = ArchiveRecordReader::open(&output).unwrap();
        let page = reader.next_record().unwrap().unwrap();
        assert!(matches!(
            page,
            Record::PageState {
                page_id: 77,
                ref title,
                ..
            } if title == "Run test page"
        ));
        for expected_id in (1..=6_u64).rev() {
            let record = reader.next_record().unwrap().unwrap();
            let Record::Revision { page_id, revision } = record else {
                panic!("expected revision {expected_id}");
            };
            assert_eq!(page_id, 77);
            assert_eq!(revision.meta.rev_id, expected_id);
            assert_eq!(
                revision.meta.parent_id,
                expected_id.saturating_sub(1),
            );
            assert_eq!(revision.meta.comment, format!("comment {expected_id}"));
            assert_eq!(revision.text, format!("revision text {expected_id}").into_bytes());
            assert_eq!(revision.meta.text_len, revision.text.len() as u64);
            assert!(matches!(
                revision.meta.contributor,
                ContributorMeta::Named { ref username, user_id }
                    if username == &format!("Editor {expected_id}")
                        && user_id == expected_id
            ));
        }
        assert!(reader.next_record().unwrap().is_none());
    }

    #[test]
    fn split_target_retry_reuses_completed_source_archives() {
        let server = MockServer::start();
        let content = include_bytes!("../tests/data/export_three_pages.xml");
        let first = server.mock(|when, then| {
            when.method(GET).path("/first.xml");
            then.status(200).body(content);
        });
        let second = server.mock(|when, then| {
            when.method(GET).path("/second.xml");
            then.status(200).body(content);
        });
        let part = |path: &str, sha1: Option<String>| wikimak_mediawiki::Part {
            url: server.url(path),
            filename: path.trim_start_matches('/').into(),
            size_bytes: content.len() as u64,
            sha256: None,
            sha1,
            md5: None,
        };
        let mut parts = [
            part("/first.xml", None),
            part("/second.xml", Some("00".repeat(32))),
        ];
        let scratch = tempfile::tempdir().unwrap();
        let observed_at = snapshot_date_micros(
            chrono::NaiveDate::from_ymd_opt(2024, 6, 1).unwrap(),
        )
        .unwrap();
        assert!(
            build_content_group(
                &Client::new(),
                &parts,
                0,
                scratch.path(),
                2,
                observed_at,
                true,
                None,
                &|_| {},
            )
            .is_err()
        );
        assert_eq!(first.hits(), 1);
        assert_eq!(second.hits(), 1);

        use sha1::Digest;
        parts[1].sha1 = Some(hex::encode(sha1::Sha1::digest(content)));
        build_content_group(
            &Client::new(),
            &parts,
            0,
            scratch.path(),
            2,
            observed_at,
            true,
            None,
            &|_| {},
        )
        .unwrap();
        assert_eq!(
            first.hits(),
            1,
            "the successfully parsed source must not be fetched again",
        );
        assert_eq!(second.hits(), 2);
    }

    #[test]
    fn history_parts_geometry_has_one_owner_and_full_decoder_allocation() {
        for (file_count, cores, expected_owners, expected_decoder_workers) in [
            (0, 0, 0, 1),
            (1, 1, 1, 1),
            (3, 4, 1, 4),
            (64, 10, 1, 10),
        ] {
            let geometry = history_parts_geometry(file_count, cores);
            assert_eq!(geometry.target_owners, expected_owners);
            assert_eq!(geometry.decoder_workers, expected_decoder_workers);
        }
    }

    #[test]
    fn make_level_history_recipes_serialize_without_serializing_content() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        let history_active = Arc::new(AtomicU64::new(0));
        let history_peak = Arc::new(AtomicU64::new(0));
        let content_active = Arc::new(AtomicU64::new(0));
        let content_peak = Arc::new(AtomicU64::new(0));

        std::thread::scope(|scope| {
            for _ in 0..5 {
                let history_active = Arc::clone(&history_active);
                let history_peak = Arc::clone(&history_peak);
                let root_path = root_path.clone();
                scope.spawn(move || {
                    let _lease = acquire_history_materialization_lease(&root_path).unwrap();
                    let active = history_active.fetch_add(1, Ordering::SeqCst) + 1;
                    history_peak.fetch_max(active, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    history_active.fetch_sub(1, Ordering::SeqCst);
                });
            }
            for _ in 0..3 {
                let content_active = Arc::clone(&content_active);
                let content_peak = Arc::clone(&content_peak);
                scope.spawn(move || {
                    let active = content_active.fetch_add(1, Ordering::SeqCst) + 1;
                    content_peak.fetch_max(active, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    content_active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        assert_eq!(history_peak.load(Ordering::SeqCst), 1);
        assert!(
            content_peak.load(Ordering::SeqCst) >= 2,
            "content recipes must remain independently parallel"
        );
    }

    #[test]
    fn make_level_history_recipe_gets_the_full_configured_decoder_window() {
        assert_eq!(history_decoder_workers(10), 10);
        assert_eq!(history_decoder_workers(0), 1);
    }

    #[test]
    fn direct_grouped_source_geometry_is_serial_and_keeps_full_decoder_window() {
        let geometry = grouped_source_geometry(5, 5, true);
        assert_eq!(geometry.source_concurrency, 1);
        assert_eq!(
            geometry.decoder_workers, 5,
            "the sole direct-build source must receive the whole target window",
        );

        let geometry = grouped_source_geometry(2, 7, true);
        assert_eq!(geometry.source_concurrency, 1);
        assert_eq!(geometry.decoder_workers, 7);
    }

    #[test]
    fn legacy_grouped_source_geometry_preserves_parallel_split() {
        for (parts, allocated, expected_sources, expected_decoder_workers) in [
            (5, 5, 5, 1),
            (4, 8, 4, 2),
            (2, 8, 2, 4),
            (3, 2, 2, 1),
        ] {
            let geometry = grouped_source_geometry(parts, allocated, false);
            assert_eq!(geometry.source_concurrency, expected_sources);
            assert_eq!(geometry.decoder_workers, expected_decoder_workers);
        }
    }

    #[test]
    fn legacy_checkpoint_receipt_with_removed_page_revision_fields_is_reusable() {
        static SERIAL: Mutex<()> = Mutex::new(());
        let _serial = SERIAL.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("content-000000.swdump");
        let mut writer = ArchiveWriter::new(
            std::fs::File::create(&path).unwrap(),
            DEFAULT_FRAME_TARGET,
        )
        .unwrap();
        writer
            .write(&Record::PageState {
                page_id: 1,
                timestamp_micros: 1,
                title: "Main Page".into(),
                namespace: Some(0),
                deleted: false,
            })
            .unwrap();
        writer.finish().unwrap();

        let part = PlannedPart {
            url: "https://example.invalid/content.xml.bz2".into(),
            filename: "content.xml.bz2".into(),
            size_bytes: 123,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let key = checkpoint_key("content", 7, [part.clone()]).unwrap();
        let same_key = checkpoint_key("content", 7, [part]).unwrap();
        assert_eq!(key, same_key, "telemetry must not enter checkpoint identity");

        let legacy_receipt = serde_json::json!({
            "schema": 1,
            "key": key.clone(),
            "stats": {
                "pages": 1,
                "revisions": 2,
                "events": 3,
                "page_events": 4,
                "user_events": 5,
                "global_events": 6,
                "fetch_attempts": 7,
                "fetch_bytes_received": 8,
                "fetch_rate_limit_responses": 9,
                "fetch_client_error_responses": 10,
                "fetch_server_error_responses": 11,
                "fetch_transport_errors": 12,
                "page_revision_spill_count": 13,
                "page_revision_spill_uncompressed_bytes": 14,
                "page_revision_spill_physical_bytes": 15,
                "page_revision_spill_gate_wait_nanos": 16,
                "page_revision_spill_write_nanos": 17,
                "page_revision_spill_lifecycle_nanos": 18,
                "page_revision_memory_threshold_bytes": 16777216,
                "page_revision_memory_threshold_min_bytes": 16777216,
                "page_revision_memory_threshold_max_bytes": 67108864
            }
        });
        std::fs::write(
            checkpoint_receipt_path(&path),
            serde_json::to_vec(&legacy_receipt).unwrap(),
        )
        .unwrap();

        let reused = checkpoint_stats(&path, &same_key).expect("legacy receipt is reusable");
        assert_eq!(reused.pages, 1);
        assert_eq!(reused.revisions, 2);
        assert_eq!(reused.fetch_attempts, 7);
        assert_eq!(reused.fetch_bytes_received, 8);
        assert_eq!(reused.fetch_rate_limit_responses, 9);
        assert_eq!(reused.fetch_client_error_responses, 10);
        assert_eq!(reused.fetch_server_error_responses, 11);
        assert_eq!(reused.fetch_transport_errors, 12);
        assert_eq!(reused.fetch_server_timed_retries, 0);
        assert_eq!(reused.fetch_robots_timed_retries, 0);
        assert_eq!(reused.fetch_fallback_timed_retries, 0);
        assert_eq!(reused.fetch_local_spacing_timed_retries, 0);
        let current_receipt_stats = serde_json::to_value(&reused).unwrap();
        for field in [
            "page_revision_spill_count",
            "page_revision_spill_uncompressed_bytes",
            "page_revision_spill_physical_bytes",
            "page_revision_spill_gate_wait_nanos",
            "page_revision_spill_write_nanos",
            "page_revision_spill_lifecycle_nanos",
            "page_revision_memory_threshold_bytes",
            "page_revision_memory_threshold_min_bytes",
            "page_revision_memory_threshold_max_bytes",
        ] {
            assert!(
                current_receipt_stats.get(field).is_none(),
                "removed telemetry field {field} must not be emitted again"
            );
        }
    }
}
