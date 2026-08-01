//! Direct upstream-dump to portable-archive construction.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

const HISTORY_SORT_RUN_TARGET: usize = 8 << 30;

pub(crate) fn processing_parallelism() -> usize {
    std::env::var("SARUN_WIKIMAK_CPU_BUDGET")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from))
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PartialStats {
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
    }

    fn record_fetch(&mut self, handle: &wikimak_mediawiki::FetchStatsHandle) {
        if let Ok(fetch) = handle.lock() {
            self.fetch_attempts = fetch.attempts;
            self.fetch_bytes_received = fetch.bytes_received;
            self.fetch_rate_limit_responses = fetch.rate_limit_responses;
            self.fetch_client_error_responses = fetch.client_error_responses;
            self.fetch_server_error_responses = fetch.server_error_responses;
            self.fetch_transport_errors = fetch.transport_errors;
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
    /// Age of the quietest active target's last observable update.
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
struct LiveTargetProgress {
    target: String,
    part: String,
    phase: String,
    source_bytes_read: u64,
    source_bytes_total: u64,
    #[serde(default)]
    decoded_bytes: u64,
    pages: u64,
    revisions: u64,
    text_bytes: u64,
    current_page: u64,
    current_title: String,
    started_at_micros: u64,
    updated_at_micros: u64,
    /// Last liveness write, kept separate from `updated_at_micros` so a
    /// blocked parser cannot masquerade as making data progress.
    #[serde(default)]
    heartbeat_at_micros: u64,
    #[serde(default)]
    phase_started_at_micros: u64,
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
    cpu_user_micros: u64,
    #[serde(default)]
    cpu_system_micros: u64,
    #[serde(default)]
    peak_rss_bytes: u64,
}

struct LiveProgressState {
    path: PathBuf,
    value: LiveTargetProgress,
    last_write: Instant,
    last_phase: String,
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

/// Return the scratch root that owns a live progress sidecar.  Direct build
/// nodes live below `nodes/.<target>.<pid>.partial`; the older checkpoint
/// path puts sidecars directly below the scratch directory.  Keeping this
/// small bit of path knowledge here lets every attempt write an immutable
/// accounting snapshot without threading the build root through all parser
/// helpers.
fn progress_scratch_root(path: &Path) -> PathBuf {
    let Some(parent) = path.parent() else {
        return PathBuf::from(".");
    };
    let hidden_node = parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'));
    let direct_nodes = parent
        .parent()
        .and_then(|nodes| nodes.file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "nodes");
    if hidden_node && direct_nodes {
        parent
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.to_path_buf())
    } else {
        parent.to_path_buf()
    }
}

fn progress_history_path(path: &Path, value: &LiveTargetProgress) -> PathBuf {
    #[derive(Deserialize)]
    struct PlanMarker {
        plan_id: String,
    }

    let root = progress_scratch_root(path);
    // A new plan gets a new directory.  This is important: scratch is reused
    // for resumable builds, but counters from an older mirror snapshot must
    // never leak into the new build's totals.
    let plan_id = std::fs::read(root.join("plan.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PlanMarker>(&bytes).ok())
        .map(|marker| marker.plan_id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "legacy".into());
    let target = value
        .target
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
        .collect::<String>();
    root.join("network-history")
        .join(plan_id)
        .join(format!("{target}-{}.json", value.started_at_micros))
}

#[derive(Default)]
struct NetworkHistoryTotals {
    targets: HashSet<String>,
    fetch_attempts: u64,
    fetch_bytes_received: u64,
    fetch_rate_limit_responses: u64,
    fetch_client_error_responses: u64,
    fetch_server_error_responses: u64,
    fetch_transport_errors: u64,
}

fn progress_record_belongs_to_plan(
    plan: &DirectBuildPlan,
    value: &LiveTargetProgress,
) -> bool {
    let Some((kind, index)) = plan.target_index(&value.target) else {
        return false;
    };
    match kind {
        "content" => plan
            .content_target(index)
            .is_some_and(|(_, _, part)| part.filename == value.part),
        "history" => plan
            .history_files
            .get(index)
            .is_some_and(|file| file.part.filename == value.part),
        _ => false,
    }
}

fn read_network_history(root: &Path, plan: &DirectBuildPlan) -> Option<NetworkHistoryTotals> {
    let directory = root.join("network-history").join(&plan.plan_id);
    let entries = std::fs::read_dir(directory).ok()?;
    let mut totals = NetworkHistoryTotals::default();
    let mut found = false;
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<LiveTargetProgress>(&bytes) else {
            continue;
        };
        if !progress_record_belongs_to_plan(plan, &value) {
            continue;
        }
        found = true;
        totals.targets.insert(value.target);
        totals.fetch_attempts = totals.fetch_attempts.saturating_add(value.fetch_attempts);
        totals.fetch_bytes_received = totals
            .fetch_bytes_received
            .saturating_add(value.fetch_bytes_received);
        totals.fetch_rate_limit_responses = totals
            .fetch_rate_limit_responses
            .saturating_add(value.fetch_rate_limit_responses);
        totals.fetch_client_error_responses = totals
            .fetch_client_error_responses
            .saturating_add(value.fetch_client_error_responses);
        totals.fetch_server_error_responses = totals
            .fetch_server_error_responses
            .saturating_add(value.fetch_server_error_responses);
        totals.fetch_transport_errors = totals
            .fetch_transport_errors
            .saturating_add(value.fetch_transport_errors);
    }
    found.then_some(totals)
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
    let (user, system, rss) = process_resource_usage();
    state.value.cpu_user_micros = user;
    state.value.cpu_system_micros = system;
    state.value.peak_rss_bytes = rss;
    state.value.updated_at_micros = now;
    write_live_progress_locked(&mut state);
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
    let (user, system, rss) = process_resource_usage();
    state.value.cpu_user_micros = user;
    state.value.cpu_system_micros = system;
    state.value.peak_rss_bytes = rss;
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
    let temporary = state.path.with_extension("progress.json.tmp");
    if let Ok(bytes) = serde_json::to_vec(&state.value) {
        if std::fs::write(&temporary, &bytes).is_ok() {
            let _ = std::fs::rename(&temporary, &state.path);
            // Keep the latest snapshot outside the disposable partial target.
            // A failed target is deliberately removed before the next retry,
            // so the sidecar alone cannot provide monotonic build totals.
            // One file per attempt avoids a contended shared ledger and makes
            // concurrent workers naturally atomic: each worker only replaces
            // its own file.
            let history = progress_history_path(&state.path, &state.value);
            if let Some(parent) = history.parent() {
                if std::fs::create_dir_all(parent).is_ok() {
                    let history_tmp = history.with_extension("json.tmp");
                    if std::fs::write(&history_tmp, &bytes).is_ok() {
                        let _ = std::fs::rename(history_tmp, history);
                    }
                }
            }
            state.last_write = Instant::now();
        }
    }
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

impl DirectBuildPlan {
    pub(crate) fn target_count(&self) -> usize {
        self.content_target_count() + self.history_files.len()
    }

    pub(crate) fn content_target_count(&self) -> usize {
        self.content_groups.iter().map(Vec::len).sum()
    }

    fn content_target(
        &self,
        index: usize,
    ) -> Option<(usize, usize, &PlannedPart)> {
        let mut remaining = index;
        for (group_index, group) in self.content_groups.iter().enumerate() {
            if remaining < group.len() {
                return Some((group_index, remaining, &group[remaining]));
            }
            remaining -= group.len();
        }
        None
    }

    fn content_target_index(&self, group_index: usize, source_index: usize) -> Option<usize> {
        let group = self.content_groups.get(group_index)?;
        (source_index < group.len()).then(|| {
            self.content_groups[..group_index]
                .iter()
                .map(Vec::len)
                .sum::<usize>()
                + source_index
        })
    }

    pub(crate) fn target_name(&self, kind: &str, index: usize) -> Option<String> {
        match kind {
            "content" => self.content_target(index).map(|(group, source, _)| {
                if self.content_groups[group].len() == 1 {
                    format!("content-{group:06}")
                } else {
                    format!("content-{group:06}-source-{source:06}")
                }
            }),
            "history" => self
                .history_files
                .get(index)
                .map(|_| format!("history-{index:06}")),
            _ => None,
        }
    }

    fn target_index(&self, target: &str) -> Option<(&'static str, usize)> {
        for index in 0..self.content_target_count() {
            if self.target_name("content", index).as_deref() == Some(target) {
                return Some(("content", index));
            }
        }
        let index = target.strip_prefix("history-")?.parse::<usize>().ok()?;
        self.history_files
            .get(index)
            .map(|_| ("history", index))
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

    fn target_source_bytes(&self, kind: &str, index: usize) -> u64 {
        match kind {
            "content" => self
                .content_target(index)
                .map_or(0, |(_, _, part)| part.size_bytes),
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
    #[serde(default)]
    stats: PartialStats,
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
    let content_groups = content_run
        .parts
        .iter()
        .map(|part| vec![PlannedPart::from(part)])
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

fn node_path(root: &Path, plan: &DirectBuildPlan, kind: &str, index: usize) -> PathBuf {
    root.join("nodes")
        .join(format!(
            "{}.done",
            plan.target_name(kind, index)
                .unwrap_or_else(|| format!("{kind}-{index:06}"))
        ))
}

fn validate_node(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: &str,
    index: usize,
) -> Result<bool> {
    let node = node_path(root, plan, kind, index);
    let data = node.join("data.swdump");
    let receipt: BuildReceipt = match std::fs::read(node.join("receipt.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(receipt) => receipt,
        None => return Ok(false),
    };
    let receipt_index_matches = match kind {
        "content" => plan.content_target(index).is_some_and(|(group, _, _)| {
            receipt.index == index
                || (plan.content_groups[group].len() == 1 && receipt.index == group)
        }),
        _ => receipt.index == index,
    };
    if receipt.plan_id != plan.plan_id
        || receipt.kind != kind
        || !receipt_index_matches
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

fn copy_or_link_file(source: &Path, destination: &Path) -> Result<()> {
    match std::fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(source, destination)?;
            Ok(())
        }
    }
}

fn grouped_partial_index(name: &str, plan: &DirectBuildPlan) -> Option<usize> {
    let target = name
        .strip_prefix('.')?
        .strip_suffix(".partial")?
        .split('.')
        .next()?;
    let group_index = target.strip_prefix("content-")?.parse::<usize>().ok()?;
    (plan.content_groups.get(group_index)?.len() > 1).then_some(group_index)
}

fn owned_partial_target(name: &str) -> Option<(&str, u32)> {
    let attempt = name
        .strip_prefix('.')?
        .strip_suffix(".partial")?;
    let (target, pid) = attempt.rsplit_once('.')?;
    Some((target, pid.parse::<u32>().ok()?))
}

fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if unsafe { libc::kill(pid, 0) } == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn grouped_source_index(name: &str) -> Option<usize> {
    let stem = name.strip_suffix(".swdump")?;
    stem.rsplit_once("-source-")?.1.parse::<usize>().ok()
}

fn saved_source_stats(
    directory: &Path,
    part: &PlannedPart,
    archive: &Path,
    observed_at_micros: i64,
) -> PartialStats {
    let key = checkpoint_key(
        "content-source",
        observed_at_micros,
        [part.clone()],
    )
    .ok();
    if let Some(stats) = key
        .as_deref()
        .and_then(|key| checkpoint_stats(archive, key))
    {
        return stats;
    }
    std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".progress.json")
        })
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<LiveTargetProgress>(&bytes).ok())
        .find(|value| value.part == part.filename && value.phase == "finished")
        .map(|value| PartialStats {
            pages: value.pages,
            revisions: value.revisions,
            fetch_attempts: value.fetch_attempts,
            fetch_bytes_received: value.fetch_bytes_received,
            fetch_rate_limit_responses: value.fetch_rate_limit_responses,
            fetch_client_error_responses: value.fetch_client_error_responses,
            fetch_server_error_responses: value.fetch_server_error_responses,
            fetch_transport_errors: value.fetch_transport_errors,
            ..Default::default()
        })
        .unwrap_or_default()
}

fn grouped_node_receipt(
    root: &Path,
    plan: &DirectBuildPlan,
    group_index: usize,
) -> Option<(PathBuf, BuildReceipt)> {
    let node = root
        .join("nodes")
        .join(format!("content-{group_index:06}.done"));
    let data = node.join("data.swdump");
    let receipt = std::fs::read(node.join("receipt.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BuildReceipt>(&bytes).ok())?;
    if receipt.plan_id != plan.plan_id
        || receipt.kind != "content"
        || receipt.index != group_index
        || std::fs::metadata(&data).ok()?.len() != receipt.data_bytes
        || !archive_file_complete(&data)
        || (group_index == 0 && !archive_file_complete(&node.join("siteinfo.swdump")))
    {
        return None;
    }
    Some((node, receipt))
}

fn write_empty_archive(path: &Path) -> Result<()> {
    ArchiveWriter::new(std::fs::File::create(path)?, DEFAULT_FRAME_TARGET)
        .map_err(map_archive)?
        .finish()
        .map_err(map_archive)?;
    Ok(())
}

fn adopt_grouped_completed_nodes(root: &Path, plan: &DirectBuildPlan) -> Result<()> {
    let nodes = root.join("nodes");
    for group_index in 0..plan.content_groups.len() {
        let group_len = plan.content_groups[group_index].len();
        if group_len <= 1 {
            continue;
        }
        let Some((old_node, receipt)) = grouped_node_receipt(root, plan, group_index) else {
            continue;
        };
        for source_index in 0..group_len {
            let index = plan
                .content_target_index(group_index, source_index)
                .ok_or(Error::Corrupt("content source is outside build plan"))?;
            if validate_node(root, plan, "content", index).unwrap_or(false) {
                continue;
            }
            let destination = node_path(root, plan, "content", index);
            if destination.exists() {
                if destination.is_dir() {
                    std::fs::remove_dir_all(&destination)?;
                } else {
                    std::fs::remove_file(&destination)?;
                }
            }
            let target = plan
                .target_name("content", index)
                .ok_or(Error::Corrupt("content target is outside build plan"))?;
            let temporary = nodes.join(format!(".{target}.adopting"));
            if temporary.exists() {
                std::fs::remove_dir_all(&temporary)?;
            }
            std::fs::create_dir(&temporary)?;
            if source_index == 0 {
                copy_or_link_file(
                    &old_node.join("data.swdump"),
                    &temporary.join("data.swdump"),
                )?;
                if group_index == 0 {
                    copy_or_link_file(
                        &old_node.join("siteinfo.swdump"),
                        &temporary.join("siteinfo.swdump"),
                    )?;
                }
            } else {
                write_empty_archive(&temporary.join("data.swdump"))?;
            }
            let stats = if source_index == 0 {
                receipt.stats.clone()
            } else {
                PartialStats::default()
            };
            publish_node(root, plan, "content", index, &temporary, &stats)?;
        }
        if (0..group_len).all(|source_index| {
            plan.content_target_index(group_index, source_index)
                .is_some_and(|index| {
                    validate_node(root, plan, "content", index).unwrap_or(false)
                })
        }) {
            std::fs::remove_dir_all(old_node)?;
        }
    }
    Ok(())
}

fn adopt_grouped_partial_sources(root: &Path, plan: &DirectBuildPlan) -> Result<()> {
    let nodes = root.join("nodes");
    let partials = std::fs::read_dir(&nodes)?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            grouped_partial_index(&name, plan)
                .map(|group_index| (group_index, entry.path()))
        })
        .collect::<Vec<_>>();
    for (group_index, directory) in partials {
        let archives = std::fs::read_dir(&directory)?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                grouped_source_index(&name).map(|source_index| (source_index, entry.path()))
            })
            .collect::<Vec<_>>();
        for (source_index, archive) in archives {
            let Some(index) = plan.content_target_index(group_index, source_index) else {
                continue;
            };
            if validate_node(root, plan, "content", index).unwrap_or(false)
                || !archive_file_complete(&archive)
            {
                continue;
            }
            let destination = node_path(root, plan, "content", index);
            if destination.exists() {
                if destination.is_dir() {
                    std::fs::remove_dir_all(&destination)?;
                } else {
                    std::fs::remove_file(&destination)?;
                }
            }
            let siteinfo = site_info_checkpoint_path(&archive);
            if index == 0 && !archive_file_complete(&siteinfo) {
                continue;
            }
            let target = plan
                .target_name("content", index)
                .ok_or(Error::Corrupt("content target is outside build plan"))?;
            let temporary = nodes.join(format!(".{target}.adopting"));
            if temporary.exists() {
                std::fs::remove_dir_all(&temporary)?;
            }
            std::fs::create_dir(&temporary)?;
            copy_or_link_file(&archive, &temporary.join("data.swdump"))?;
            if index == 0 {
                copy_or_link_file(&siteinfo, &temporary.join("siteinfo.swdump"))?;
            }
            let part = plan
                .content_groups
                .get(group_index)
                .and_then(|group| group.get(source_index))
                .ok_or(Error::Corrupt("content source is outside build plan"))?;
            let stats = saved_source_stats(
                &directory,
                part,
                &archive,
                plan.observed_at_micros,
            );
            publish_node(root, plan, "content", index, &temporary, &stats)?;
        }
    }
    Ok(())
}

pub(crate) fn prune_invalid_build_nodes(root: &Path, plan: &DirectBuildPlan) -> Result<usize> {
    std::fs::create_dir_all(root.join("nodes"))?;
    adopt_grouped_completed_nodes(root, plan)?;
    adopt_grouped_partial_sources(root, plan)?;
    let mut reusable = 0;
    for (kind, count) in [
        ("content", plan.content_target_count()),
        ("history", plan.history_files.len()),
    ] {
        for index in 0..count {
            let path = node_path(root, plan, kind, index);
            if validate_node(root, plan, kind, index).unwrap_or(false) {
                reusable += 1;
            } else if path.exists() {
                std::fs::remove_dir_all(path)?;
            }
        }
    }
    for entry in std::fs::read_dir(root.join("nodes"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
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
    let titles = output.with_extension("swtitle");
    let marker_matches = std::fs::read_to_string(&marker)
        .is_ok_and(|stored| stored.trim_end() == plan.plan_id);
    if output.exists()
        && archive_file_complete(&output)
        && (marker_matches || archive_records_are_readable(&output))
    {
        let title_matches = crate::title_index::TitleIndex::open(&titles)
            .and_then(|index| {
                crate::archive::IndexedArchiveSet::open(&output, &index)
                    .map(|_| index)
            })
            .is_ok();
        if !title_matches {
            crate::title_index::build(&output, &titles).map_err(map_archive)?;
        }
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
    let network_history = read_network_history(&root, &plan);
    if root.join("archive.complete").exists() {
        return Some(MirrorBuildProgress {
            phase: "indexing".into(),
            targets_total: total,
            targets_completed: total,
            target_progress: Vec::new(),
            source_bytes_total: plan.source_bytes(),
            source_bytes_completed: plan.source_bytes(),
            active_source_bytes_per_second: None,
            active_quiet_seconds: None,
            fetch_attempts: network_history
                .as_ref()
                .map_or(0, |history| history.fetch_attempts),
            fetch_bytes_received: network_history
                .as_ref()
                .map_or(0, |history| history.fetch_bytes_received),
            fetch_rate_limit_responses: network_history
                .as_ref()
                .map_or(0, |history| history.fetch_rate_limit_responses),
            fetch_client_error_responses: network_history
                .as_ref()
                .map_or(0, |history| history.fetch_client_error_responses),
            fetch_server_error_responses: network_history
                .as_ref()
                .map_or(0, |history| history.fetch_server_error_responses),
            fetch_transport_errors: network_history
                .as_ref()
                .map_or(0, |history| history.fetch_transport_errors),
            snapshot: plan.content_snapshot,
            ..Default::default()
        });
    }
    let historical_targets = network_history
        .as_ref()
        .map(|history| &history.targets);
    let mut completed = 0_u64;
    let mut completed_bytes = 0_u64;
    let mut fetch_attempts = network_history
        .as_ref()
        .map_or(0, |history| history.fetch_attempts);
    let mut fetch_bytes_received = network_history
        .as_ref()
        .map_or(0, |history| history.fetch_bytes_received);
    let mut fetch_rate_limit_responses = network_history
        .as_ref()
        .map_or(0, |history| history.fetch_rate_limit_responses);
    let mut fetch_client_error_responses = network_history
        .as_ref()
        .map_or(0, |history| history.fetch_client_error_responses);
    let mut fetch_server_error_responses = network_history
        .as_ref()
        .map_or(0, |history| history.fetch_server_error_responses);
    let mut fetch_transport_errors = network_history
        .as_ref()
        .map_or(0, |history| history.fetch_transport_errors);
    for (kind, count) in [
        ("content", plan.content_target_count()),
        ("history", plan.history_files.len()),
    ] {
        for index in 0..count {
            let receipt_path = node_path(&root, &plan, kind, index).join("receipt.json");
            if receipt_path.is_file() {
                completed += 1;
                completed_bytes =
                    completed_bytes.saturating_add(plan.target_source_bytes(kind, index));
                let target_name = plan
                    .target_name(kind, index)
                    .unwrap_or_else(|| format!("{kind}-{index:06}"));
                let receipt_is_historical = historical_targets
                    .is_some_and(|targets| targets.contains(&target_name));
                if !receipt_is_historical {
                    if let Ok(bytes) = std::fs::read(receipt_path) {
                        if let Ok(receipt) = serde_json::from_slice::<BuildReceipt>(&bytes) {
                            fetch_attempts = fetch_attempts
                                .saturating_add(receipt.stats.fetch_attempts);
                            fetch_bytes_received = fetch_bytes_received
                                .saturating_add(receipt.stats.fetch_bytes_received);
                            fetch_rate_limit_responses = fetch_rate_limit_responses
                                .saturating_add(receipt.stats.fetch_rate_limit_responses);
                            fetch_client_error_responses = fetch_client_error_responses
                                .saturating_add(receipt.stats.fetch_client_error_responses);
                            fetch_server_error_responses = fetch_server_error_responses
                                .saturating_add(receipt.stats.fetch_server_error_responses);
                            fetch_transport_errors = fetch_transport_errors
                                .saturating_add(receipt.stats.fetch_transport_errors);
                        }
                    }
                }
            }
        }
    }
    let active_dirs = std::fs::read_dir(root.join("nodes"))
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (target, pid) = owned_partial_target(&name)?;
            process_is_alive(pid).then(|| (target.to_owned(), entry.path()))
        })
        .collect::<Vec<_>>();
    let mut active = Vec::new();
    let mut target_progress = Vec::new();
    let mut active_rate = 0_u64;
    let mut active_quiet = 0_u64;
    let mut failed_target = false;
    let mut live_target = false;
    for (target, path) in active_dirs {
        if active.iter().any(|item: &String| item.starts_with(&target)) {
            continue;
        }
        let Some((kind, index)) = plan.target_index(&target) else {
            active.push(target);
            continue;
        };
        let mut live = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&path) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                if !file_name.to_string_lossy().ends_with(".progress.json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(entry.path()) {
                    if let Ok(value) = serde_json::from_slice::<LiveTargetProgress>(&bytes) {
                        live.push(value);
                    }
                }
            }
        }
        if live.is_empty() {
            // A dot-prefixed node is not proof that a worker is alive. It
            // survives a killed worker and is retained for resumability, so
            // expose it as stale state instead of inventing a dependency on
            // a coordination service that is not part of the mirror job.
            failed_target = true;
            target_progress.push(MirrorTargetProgress {
                target: target.clone(),
                kind: kind.to_owned(),
                phase: "stale build state (no progress record)".into(),
                source_bytes_total: plan.target_source_bytes(kind, index),
                ..Default::default()
            });
            active.push(format!("{target} · stale build state (no progress record)"));
            continue;
        }
        let downloaded = live
            .iter()
            .map(|value| value.source_bytes_read)
            .sum::<u64>();
        let total = live
            .iter()
            .map(|value| value.source_bytes_total)
            .sum::<u64>();
        let revisions = live.iter().map(|value| value.revisions).sum::<u64>();
        let text_bytes = live.iter().map(|value| value.text_bytes).sum::<u64>();
        if historical_targets.is_none_or(|targets| !targets.contains(&target)) {
            fetch_attempts = fetch_attempts.saturating_add(
                live.iter()
                    .map(|value| value.fetch_attempts)
                    .sum::<u64>(),
            );
            fetch_bytes_received = fetch_bytes_received.saturating_add(
                live.iter()
                    .map(|value| value.fetch_bytes_received)
                    .sum::<u64>(),
            );
            fetch_rate_limit_responses = fetch_rate_limit_responses.saturating_add(
                live.iter()
                    .map(|value| value.fetch_rate_limit_responses)
                    .sum::<u64>(),
            );
            fetch_client_error_responses = fetch_client_error_responses.saturating_add(
                live.iter()
                    .map(|value| value.fetch_client_error_responses)
                    .sum::<u64>(),
            );
            fetch_server_error_responses = fetch_server_error_responses.saturating_add(
                live.iter()
                    .map(|value| value.fetch_server_error_responses)
                    .sum::<u64>(),
            );
            fetch_transport_errors = fetch_transport_errors.saturating_add(
                live.iter()
                    .map(|value| value.fetch_transport_errors)
                    .sum::<u64>(),
            );
        }
        let phase_is_failed = |value: &LiveTargetProgress| {
            let phase = value.phase.to_ascii_lowercase();
            phase.contains("failed") || phase.contains("error") || phase.contains("stopped")
        };
        // A sibling source can continue briefly after another one fails. Its
        // newer heartbeat must not hide the failed source that will determine
        // the target result.
        let current = live
            .iter()
            .filter(|value| phase_is_failed(value))
            .max_by_key(|value| value.updated_at_micros)
            .or_else(|| {
                live.iter()
                    .filter(|value| value.phase != "finished")
                    .max_by_key(|value| value.updated_at_micros)
            })
            .or_else(|| live.iter().max_by_key(|value| value.updated_at_micros))
            .expect("live progress is non-empty");
        let observed_now = now_micros();
        let source_rate = |value: &LiveTargetProgress| {
            let elapsed = observed_now.saturating_sub(value.started_at_micros);
            if elapsed == 0 {
                0
            } else {
                value.source_bytes_read.saturating_mul(1_000_000) / elapsed
            }
        };
        let source_quiet = |value: &LiveTargetProgress| {
            observed_now.saturating_sub(value.updated_at_micros) / 1_000_000
        };
        let rate = live
            .iter()
            .filter(|value| value.phase != "finished" && !phase_is_failed(value))
            .map(source_rate)
            .sum::<u64>();
        let quiet_seconds = source_quiet(current);
        let target_total = plan.target_source_bytes(kind, index);
        let total = total.max(target_total);
        let percent = (total > 0)
            .then(|| downloaded.saturating_mul(100) / total)
            .unwrap_or(0);
        let title = if current.current_title.is_empty() {
            String::new()
        } else {
            format!(" · {}", current.current_title.chars().take(80).collect::<String>())
        };
        let source_file = if current.part.is_empty() {
            String::new()
        } else {
            format!(
                " · source {}",
                current.part.chars().take(96).collect::<String>()
            )
        };
        let all_sources_finished = live.iter().all(|value| value.phase == "finished");
        let phase = if all_sources_finished {
            "merging completed source fragments"
        } else if current.phase.is_empty() {
            "working"
        } else {
            current.phase.as_str()
        };
        let is_failed = live.iter().any(phase_is_failed);
        failed_target |= is_failed;
        if !is_failed {
            live_target = true;
            active_rate = active_rate.saturating_add(rate);
            let quietest_source = live
                .iter()
                .filter(|value| value.phase != "finished")
                .map(source_quiet)
                .max()
                .unwrap_or(quiet_seconds);
            active_quiet = active_quiet.max(quietest_source);
        }
        let record_label = if kind == "history" { "events" } else { "revisions" };
        let waiting_for_source = current.source_bytes_read == 0
            && current.revisions == 0
            && (phase.contains("network") || phase.contains("source bytes"));
        let heartbeat_seconds = (current.heartbeat_at_micros > 0)
            .then(|| {
                now_micros()
                    .saturating_sub(current.heartbeat_at_micros)
                    / 1_000_000
            })
            .unwrap_or(u64::MAX);
        let phase_seconds = (current.phase_started_at_micros > 0)
            .then(|| {
                now_micros()
                    .saturating_sub(current.phase_started_at_micros)
                    / 1_000_000
            })
            .unwrap_or(0);
        let quiet = if quiet_seconds >= 10 {
            format!(
                " · no data progress for {quiet_seconds}s ({})",
                if heartbeat_seconds <= 5 {
                    format!("worker heartbeat {heartbeat_seconds}s ago")
                } else if waiting_for_source {
                    "waiting for source bytes and heartbeat is quiet".into()
                } else {
                    "parser/encoder heartbeat quiet".into()
                }
            )
        } else {
            String::new()
        };
        if all_sources_finished {
            target_progress.push(MirrorTargetProgress {
                target: target.clone(),
                kind: kind.to_owned(),
                phase: phase.to_owned(),
                source_bytes_read: downloaded,
                source_bytes_total: total,
                decoded_bytes: live.iter().map(|value| value.decoded_bytes).sum(),
                pages: live.iter().map(|value| value.pages).sum(),
                records: revisions,
                text_bytes,
                quiet_seconds,
                heartbeat_seconds,
                phase_seconds,
                cpu_user_micros: current.cpu_user_micros,
                cpu_system_micros: current.cpu_system_micros,
                peak_rss_bytes: current.peak_rss_bytes,
                fetch_attempts: live.iter().map(|value| value.fetch_attempts).sum(),
                fetch_bytes_received: live
                    .iter()
                    .map(|value| value.fetch_bytes_received)
                    .sum(),
                fetch_rate_limit_responses: live
                    .iter()
                    .map(|value| value.fetch_rate_limit_responses)
                    .sum(),
                fetch_client_error_responses: live
                    .iter()
                    .map(|value| value.fetch_client_error_responses)
                    .sum(),
                fetch_server_error_responses: live
                    .iter()
                    .map(|value| value.fetch_server_error_responses)
                    .sum(),
                fetch_transport_errors: live
                    .iter()
                    .map(|value| value.fetch_transport_errors)
                    .sum(),
                ..Default::default()
            });
        } else {
            let mut visible = live
                .iter()
                .filter(|value| value.phase != "finished" || phase_is_failed(value))
                .collect::<Vec<_>>();
            visible.sort_by_key(|value| value.part.clone());
            for value in visible {
                let heartbeat = (value.heartbeat_at_micros > 0)
                    .then(|| {
                        observed_now.saturating_sub(value.heartbeat_at_micros) / 1_000_000
                    })
                    .unwrap_or(u64::MAX);
                let phase_age = (value.phase_started_at_micros > 0)
                    .then(|| {
                        observed_now.saturating_sub(value.phase_started_at_micros) / 1_000_000
                    })
                    .unwrap_or(0);
                target_progress.push(MirrorTargetProgress {
                    target: target.clone(),
                    kind: kind.to_owned(),
                    phase: if value.phase.is_empty() {
                        "working".into()
                    } else {
                        value.phase.clone()
                    },
                    source: value.part.clone(),
                    source_bytes_read: value.source_bytes_read,
                    source_bytes_total: value.source_bytes_total,
                    decoded_bytes: value.decoded_bytes,
                    bytes_per_second: source_rate(value),
                    pages: value.pages,
                    records: value.revisions,
                    text_bytes: value.text_bytes,
                    current_page: value.current_page,
                    current_title: value.current_title.clone(),
                    quiet_seconds: source_quiet(value),
                    heartbeat_seconds: heartbeat,
                    phase_seconds: phase_age,
                    fetch_attempts: value.fetch_attempts,
                    fetch_bytes_received: value.fetch_bytes_received,
                    fetch_rate_limit_responses: value.fetch_rate_limit_responses,
                    fetch_client_error_responses: value.fetch_client_error_responses,
                    fetch_server_error_responses: value.fetch_server_error_responses,
                    fetch_transport_errors: value.fetch_transport_errors,
                    cpu_user_micros: value.cpu_user_micros,
                    cpu_system_micros: value.cpu_system_micros,
                    peak_rss_bytes: value.peak_rss_bytes,
                });
            }
        }
        active.push(format!(
            "{target} · {phase} · {percent}% source · {} / {} at {}/s · {} {record_label} · {} text{}{}{}",
            human_progress_bytes(downloaded),
            human_progress_bytes(total),
            human_progress_bytes(rate),
            revisions,
            human_progress_bytes(text_bytes),
            source_file,
            title,
            quiet,
        ));
        completed_bytes = completed_bytes.saturating_add(downloaded);
    }
    active.sort();
    let has_active = live_target;
    Some(MirrorBuildProgress {
        phase: if failed_target {
            "failed; inspect target details".into()
        } else if !active.is_empty() || completed < total {
            "fetching and parsing".into()
        } else if root.join("stage2.mk").exists() {
            "assembling".into()
        } else {
            "preparing assembly".into()
        },
        targets_total: total,
        targets_completed: completed,
        target_progress,
        targets_active: active,
        source_bytes_total: plan.source_bytes(),
        source_bytes_completed: completed_bytes,
        active_source_bytes_per_second: has_active.then_some(active_rate),
        active_quiet_seconds: has_active.then_some(active_quiet),
        fetch_attempts,
        fetch_bytes_received,
        fetch_rate_limit_responses,
        fetch_client_error_responses,
        fetch_server_error_responses,
        fetch_transport_errors,
        snapshot: plan.content_snapshot,
    })
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
    let receipt_index = match kind {
        "content" => plan
            .content_target(index)
            .map_or(index, |(group, _, _)| {
                if plan.content_groups[group].len() == 1 {
                    group
                } else {
                    index
                }
            }),
        _ => index,
    };
    let receipt = BuildReceipt {
        plan_id: plan.plan_id.clone(),
        kind: kind.to_owned(),
        index: receipt_index,
        data_bytes,
        stats: stats.clone(),
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
    let destination = node_path(root, plan, kind, index);
    std::fs::rename(temporary, &destination)?;
    // Finished source sidecars remain beside their intermediate archives
    // until this atomic publish so the target's live byte count cannot move
    // backwards. The durable receipt supersedes them after publication.
    for entry in std::fs::read_dir(&destination)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with(".progress.json")
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    sync_directory(&destination)?;
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
    let target_name = plan
        .target_name(kind, index)
        .ok_or(Error::Corrupt("target is outside build plan"))?;
    let temporary = root
        .join("nodes")
        .join(format!(
            ".{target_name}.{}.partial",
            std::process::id()
        ));
    std::fs::create_dir_all(&temporary)?;
    // A target can spend many minutes inside one blocking read/write call:
    // durable receipts only appear after the whole target is complete, and
    // the parent scheduler otherwise sees no stderr at all during that time.
    // Keep emitting the last known activity so the live mirror job has a
    // heartbeat even while the parser is blocked on its spool or the output
    // file is blocked on the destination volume.
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
    let result: Result<PartialStats> = match kind {
        "content" => {
            let part = plan_part(
                plan.content_target(index)
                    .ok_or(Error::Corrupt("content target is outside build plan"))?
                    .2,
            );
            let part_path = temporary.join(format!("{target_name}.swdump"));
            let built = build_content_part(
                client,
                &part,
                &part_path,
                bz2_workers,
                plan.observed_at_micros,
                true,
                &report,
            )?;
            let built_stats = built.stats.clone();
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
            Ok(built_stats)
        }
        "history" => {
            let file = planned_history(
                plan.history_files
                    .get(index)
                    .ok_or(Error::Corrupt("history target is outside build plan"))?,
            );
            let cancelled = Arc::new(AtomicBool::new(false));
            let (path, stats) = build_history_part(
                client,
                &plan.wiki_db,
                &file,
                index,
                &temporary,
                bz2_workers,
                cancelled,
                &report,
            )?;
            std::fs::rename(path, temporary.join("data.swdump"))?;
            Ok(stats)
        }
        _ => Err(Error::Corrupt("unknown direct build target kind")),
    };
    heartbeat_stop.store(true, Ordering::Relaxed);
    let _ = heartbeat.join();
    let stats = match result {
        Ok(stats) => stats,
        Err(error) => {
            let message = format!("failed {kind} target {}: {error}", index + 1);
            eprintln!("{message}");
            progress(&message);
            // Keep the failed node and its per-source sidecars for diagnosis.
            // A resumed build removes this dot-prefixed attempt before retrying.
            return Err(error);
        }
    };
    publish_node(root, plan, kind, index, &temporary, &stats)?;
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
        ("content", plan.content_target_count()),
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

    let mut inputs = (0..plan.content_target_count())
        .map(|index| node_path(root, plan, "content", index).join("data.swdump"))
        .chain(
            (0..plan.history_files.len())
                .map(|index| node_path(root, plan, "history", index).join("data.swdump")),
        )
        .collect::<Vec<_>>();
    inputs.push(node_path(root, plan, "content", 0).join("siteinfo.swdump"));
    inputs.push(manifest_archive.clone());
    progress("assembling durable page-ID range files");
    let temporary = crate::archive_set::ArchiveSetOutput::new_in(
        root,
        plan.range_target,
    )
    .map_err(map_archive)?;
    let bootstrap = tempfile::tempfile_in(root)?;
    let mut title_index = crate::title_index::TitleIndexBuilder::new();
    let (file, _, _, _) =
        crate::archive::merge_many_archives_bootstrapping_ref_prefix_observing(
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
            |record| title_index.observe(record),
        )
        .map_err(map_archive)?;
    let completed = file.finish().map_err(map_archive)?;
    completed.persist(&output).map_err(map_archive)?;
    sync_directory(&output)?;
    progress("writing title and virtual-frame index from the merged record projection");
    title_index
        .finish(&output, output.with_extension("swtitle"))
        .map_err(map_archive)?;
    persist_completion_marker(root, plan)?;

    for kind in ["content", "history"] {
        let count = if kind == "content" {
            plan.content_target_count()
        } else {
            plan.history_files.len()
        };
        for index in 0..count {
            std::fs::remove_dir_all(node_path(root, plan, kind, index))?;
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
    std::env::set_var(
        "SARUN_WIKIMEDIA_ROBOTS_CACHE",
        scratch.path().join("robots-cache"),
    );
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
    std::env::set_var(
        "SARUN_WIKIMEDIA_ROBOTS_CACHE",
        scratch.join("robots-cache"),
    );
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
    let cores = processing_parallelism();
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
                    progress,
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
    progress: &(impl Fn(&str) + Sync),
) -> Result<(PathBuf, PartialStats)> {
    let live_path = scratch.join(format!("history-{file_index:06}.progress.json"));
    let live = Arc::new(Mutex::new(LiveProgressState {
        path: live_path.clone(),
        value: LiveTargetProgress {
            target: format!("history-{file_index:06}"),
            part: file.part.filename.clone(),
            phase: "starting".into(),
            source_bytes_total: file.part.size_bytes,
            started_at_micros: now_micros(),
            updated_at_micros: now_micros(),
            ..Default::default()
        },
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
    let mut sorter =
        RecordSorter::new_with_run_target(scratch, HISTORY_SORT_RUN_TARGET)
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
    let _ = std::fs::remove_file(live_path);
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
                    false,
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
    retain_live_progress_until_publish: bool,
    progress: &(impl Fn(&str) + Sync),
) -> Result<ContentPartResult> {
    let path = scratch.join(format!("content-{index:06}.swdump"));
    if parts.len() > 1 {
        let workers = parts.len().min(bz2_workers).max(1);
        let per_part_workers = (bz2_workers / workers).max(1);
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
                                path: part_path.clone(),
                                stats,
                                site_info,
                            })
                    })
                })
                .flatten()
                .transpose()?;
            if let Some(result) = saved {
                let live_path = part_path.with_extension("progress.json");
                if !live_path.exists() {
                    let value = LiveTargetProgress {
                        target: format!("content-{index:06}"),
                        part: part.filename.clone(),
                        phase: "finished".into(),
                        source_bytes_read: part.size_bytes,
                        source_bytes_total: part.size_bytes,
                        pages: result.stats.pages,
                        revisions: result.stats.revisions,
                        started_at_micros: now_micros(),
                        updated_at_micros: now_micros(),
                        heartbeat_at_micros: now_micros(),
                        ..Default::default()
                    };
                    std::fs::write(&live_path, serde_json::to_vec(&value).map_err(|_| {
                        Error::Corrupt("cannot restore source progress checkpoint")
                    })?)?;
                }
                progress(&format!("reusing completed source {}", part.filename));
                reused.push((part_index, result));
                continue;
            }
            for stale in [
                part_path.clone(),
                checkpoint_receipt_path(&part_path),
                site_info_checkpoint_path(&part_path),
                part_path.with_extension("progress.json"),
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
            for _ in 0..workers {
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
                        per_part_workers,
                        observed_at_micros,
                        retain_live_progress_until_publish,
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
        for input in inputs {
            std::fs::remove_file(&input)?;
            for checkpoint in [
                checkpoint_receipt_path(&input),
                site_info_checkpoint_path(&input),
            ] {
                if checkpoint.exists() {
                    std::fs::remove_file(checkpoint)?;
                }
            }
        }
        return Ok(ContentPartResult {
            path,
            stats,
            site_info,
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
    progress: &(impl Fn(&str) + Sync),
) -> Result<ContentPartResult> {
    let live_path = path.with_extension("progress.json");
    let live = Arc::new(Mutex::new(LiveProgressState {
        path: live_path.clone(),
        value: LiveTargetProgress {
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
        },
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
    // Wikimedia content dumps are ordered by page ID. Buffer only the
    // revisions of the current page so they can be reversed into the
    // archive's newest-to-oldest order; retaining and sorting a whole dump
    // fragment makes memory use proportional to the fragment for no benefit.
    let output = std::fs::File::create(path)?;
    let mut writer =
        ArchiveWriter::new(output, DEFAULT_FRAME_TARGET).map_err(map_archive)?;
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
        let mut page_revisions = Vec::new();
        while let Some(revision) = revisions.next_revision() {
            let record = convert_revision(revision.map_err(Error::Mediawiki)?)?;
            revisions_seen = revisions_seen.saturating_add(1);
            text_bytes = text_bytes.saturating_add(record.text.len() as u64);
            page_revisions.push(record);
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
        page_revisions.sort_by(|left, right| {
            right
                .meta
                .ts
                .cmp(&left.meta.ts)
                .then(right.meta.rev_id.cmp(&left.meta.rev_id))
        });
        writer
            .write(&Record::PageState {
                page_id,
                timestamp_micros: observed_at_micros,
                title: header.title.clone(),
                namespace: None,
                deleted: false,
            })
            .map_err(map_archive)?;
        for revision in page_revisions {
            writer
                .write(&Record::Revision {
                    page_id,
                    revision,
                })
                .map_err(map_archive)?;
        }
        stats.pages += 1;
        stats.revisions += revisions_seen - page_revisions_before;
    }
    set_live_phase(&live, "sealing sorted archive");
    persist_live_progress(&live, true);
    let (output, _) = writer.finish().map_err(map_archive)?;
    output.sync_all()?;
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
        let _ = std::fs::remove_file(live_path);
    }
    Ok(ContentPartResult {
        path: path.to_path_buf(),
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
    fn phase_changes_are_persisted_only_by_the_periodic_sampler() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.progress.json");
        let state = Arc::new(Mutex::new(LiveProgressState {
            path: path.clone(),
            value: LiveTargetProgress {
                phase: "starting".into(),
                ..Default::default()
            },
            last_write: Instant::now(),
            last_phase: "starting".into(),
        }));

        for phase in ["waiting", "decompressing", "parsing", "decompressing"] {
            set_live_phase(&state, phase);
        }
        assert!(
            !path.exists(),
            "a hot-path phase transition performed filesystem I/O"
        );

        persist_live_heartbeat(&state);
        let persisted: LiveTargetProgress =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted.phase, "decompressing");
        assert_ne!(persisted.heartbeat_at_micros, 0);
    }

    #[test]
    fn flattened_content_targets_keep_singleton_group_names_stable() {
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
        assert_eq!(plan.content_target_count(), 4);
        assert_eq!(plan.target_name("content", 0).as_deref(), Some("content-000000"));
        assert_eq!(
            plan.target_name("content", 1).as_deref(),
            Some("content-000001-source-000000")
        );
        assert_eq!(
            plan.target_name("content", 2).as_deref(),
            Some("content-000001-source-000001")
        );
        assert_eq!(plan.target_name("content", 3).as_deref(), Some("content-000002"));
    }

    #[test]
    fn restart_adopts_complete_sources_from_an_old_grouped_partial() {
        let part = |name: &str| PlannedPart {
            url: format!("https://example.invalid/{name}"),
            filename: name.into(),
            size_bytes: 100,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let mut plan = DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
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
                vec![part("first")],
                vec![part("slice-a"), part("slice-b"), part("slice-c")],
            ],
            history_files: Vec::new(),
        };
        plan.plan_id = direct_plan_id(&plan).unwrap();
        let root = tempfile::tempdir().unwrap();
        let partial = root.path().join("nodes/.content-000001.123.partial");
        std::fs::create_dir_all(&partial).unwrap();
        for source_index in [0, 2] {
            let path = partial.join(format!(
                "content-000000-source-{source_index:06}.swdump"
            ));
            ArchiveWriter::new(std::fs::File::create(path).unwrap(), 128)
                .unwrap()
                .finish()
                .unwrap();
        }

        assert_eq!(prune_invalid_build_nodes(root.path(), &plan).unwrap(), 2);
        assert!(validate_node(root.path(), &plan, "content", 1).unwrap());
        assert!(!validate_node(root.path(), &plan, "content", 2).unwrap());
        assert!(validate_node(root.path(), &plan, "content", 3).unwrap());
        assert!(!partial.exists());
    }

    #[test]
    fn restart_adopts_an_old_completed_group_without_repeating_its_records() {
        let part = |name: &str| PlannedPart {
            url: format!("https://example.invalid/{name}"),
            filename: name.into(),
            size_bytes: 100,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let mut plan = DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
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
                vec![part("first")],
                vec![part("slice-a"), part("slice-b"), part("slice-c")],
            ],
            history_files: Vec::new(),
        };
        plan.plan_id = direct_plan_id(&plan).unwrap();
        let root = tempfile::tempdir().unwrap();
        let old_node = root.path().join("nodes/content-000001.done");
        std::fs::create_dir_all(&old_node).unwrap();
        let data = old_node.join("data.swdump");
        let mut writer =
            ArchiveWriter::new(std::fs::File::create(&data).unwrap(), 128).unwrap();
        writer
            .write(&Record::Manifest {
                timestamp_micros: 0,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: "2024-06-01".into(),
                    metadata_snapshot: "2024-06".into(),
                    source_files: vec!["old-group".into()],
                },
            })
            .unwrap();
        writer.finish().unwrap();
        std::fs::write(
            old_node.join("receipt.json"),
            serde_json::to_vec(&BuildReceipt {
                plan_id: plan.plan_id.clone(),
                kind: "content".into(),
                index: 1,
                data_bytes: std::fs::metadata(&data).unwrap().len(),
                stats: PartialStats::default(),
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(prune_invalid_build_nodes(root.path(), &plan).unwrap(), 3);
        assert!(!old_node.exists());
        for index in 1..=3 {
            assert!(validate_node(root.path(), &plan, "content", index).unwrap());
        }
        let mut aggregate = ArchiveRecordReader::open(
            node_path(root.path(), &plan, "content", 1).join("data.swdump"),
        )
        .unwrap();
        assert!(matches!(
            aggregate.next_record().unwrap(),
            Some(Record::Manifest { .. })
        ));
        let mut empty = ArchiveRecordReader::open(
            node_path(root.path(), &plan, "content", 2).join("data.swdump"),
        )
        .unwrap();
        assert!(empty.next_record().unwrap().is_none());
    }

    #[test]
    fn network_progress_survives_partial_target_replacement() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nodes")).unwrap();
        let partial = root.path().join("nodes/.content-000000.123.partial");
        std::fs::create_dir(&partial).unwrap();
        std::fs::write(
            root.path().join("plan.json"),
            br#"{"plan_id":"plan-test"}"#,
        )
        .unwrap();
        let sidecar = partial.join("content-000000.progress.json");
        let state = Arc::new(Mutex::new(LiveProgressState {
            path: sidecar.clone(),
            value: LiveTargetProgress {
                target: "content-000000".into(),
                part: "content.xml.bz2".into(),
                started_at_micros: 42,
                fetch_attempts: 3,
                fetch_rate_limit_responses: 2,
                fetch_bytes_received: 1234,
                ..Default::default()
            },
            last_write: Instant::now()
                .checked_sub(Duration::from_secs(3))
                .unwrap(),
            last_phase: String::new(),
        }));
        persist_live_progress(&state, true);
        let unrelated = Arc::new(Mutex::new(LiveProgressState {
            path: partial.join("mislabeled.progress.json"),
            value: LiveTargetProgress {
                target: "content-000000".into(),
                part: "belongs-to-another-target.xml.bz2".into(),
                started_at_micros: 43,
                fetch_attempts: 99,
                fetch_rate_limit_responses: 99,
                fetch_bytes_received: 99,
                ..Default::default()
            },
            last_write: Instant::now()
                .checked_sub(Duration::from_secs(3))
                .unwrap(),
            last_phase: String::new(),
        }));
        persist_live_progress(&unrelated, true);
        std::fs::remove_dir_all(partial).unwrap();

        let plan = DirectBuildPlan {
            schema: 1,
            plan_id: "plan-test".into(),
            wiki_db: "testwiki".into(),
            content_snapshot: "2024-06-01".into(),
            metadata_snapshot: "2024-06".into(),
            observed_at_micros: 0,
            frame_target: 1,
            range_target: 1,
            compression_level: 1,
            ref_prefix_sample_bytes: 1,
            ref_prefix_bytes: 1,
            content_groups: vec![vec![PlannedPart {
                url: "https://example.invalid/content.xml.bz2".into(),
                filename: "content.xml.bz2".into(),
                size_bytes: 0,
                sha256: None,
                sha1: None,
                md5: None,
            }]],
            history_files: Vec::new(),
        };
        let totals = read_network_history(root.path(), &plan).unwrap();
        assert_eq!(totals.targets.len(), 1);
        assert_eq!(totals.fetch_attempts, 3);
        assert_eq!(totals.fetch_rate_limit_responses, 2);
        assert_eq!(totals.fetch_bytes_received, 1234);
    }

    #[test]
    fn split_target_progress_is_monotonic_and_source_attributed() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("testwiki.swdump");
        let root = crate::cli::mirror_scratch_path(&archive);
        let part = |name: &str| PlannedPart {
            url: format!("https://example.invalid/{name}"),
            filename: name.into(),
            size_bytes: 100,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let mut plan = DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
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
                vec![
                    part("source-1.xml.bz2"),
                    part("source-2.xml.bz2"),
                    part("source-3.xml.bz2"),
                ],
                vec![part("stale.xml.bz2")],
            ],
            history_files: Vec::new(),
        };
        plan.plan_id = direct_plan_id(&plan).unwrap();
        std::fs::create_dir_all(root.join("nodes")).unwrap();
        std::fs::write(
            root.join("plan.json"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();
        let values = [
            LiveTargetProgress {
                target: "content-000000-source-000000".into(),
                part: "source-1.xml.bz2".into(),
                phase: "finished".into(),
                source_bytes_read: 100,
                source_bytes_total: 100,
                started_at_micros: 1,
                updated_at_micros: now_micros(),
                ..Default::default()
            },
            LiveTargetProgress {
                target: "content-000000-source-000001".into(),
                part: "source-2.xml.bz2".into(),
                phase: "waiting for source bytes".into(),
                source_bytes_read: 20,
                source_bytes_total: 100,
                started_at_micros: 2,
                updated_at_micros: now_micros(),
                ..Default::default()
            },
            LiveTargetProgress {
                target: "content-000000-source-000002".into(),
                part: "source-3.xml.bz2".into(),
                phase: "waiting for network slot".into(),
                source_bytes_total: 100,
                started_at_micros: 3,
                updated_at_micros: now_micros(),
                ..Default::default()
            },
        ];
        for (index, value) in values.iter().enumerate() {
            let partial = root.join(format!(
                "nodes/.content-000000-source-{index:06}.{}.partial",
                std::process::id()
            ));
            std::fs::create_dir_all(&partial).unwrap();
            std::fs::write(
                partial.join(format!("source-{index}.progress.json")),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap();
        }
        let stale = root.join("nodes/.content-000001.123.partial");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(
            stale.join("content-000000.progress.json"),
            serde_json::to_vec(&LiveTargetProgress {
                target: "content-000000.swdump".into(),
                part: "stale.xml.bz2".into(),
                phase: "downloading".into(),
                source_bytes_total: 100,
                started_at_micros: 1,
                updated_at_micros: 1,
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();

        let progress = mirror_build_progress(&archive).unwrap();
        assert_eq!(progress.source_bytes_completed, 120);
        assert_eq!(progress.target_progress.len(), 3);
        assert_eq!(
            progress
                .target_progress
                .iter()
                .find(|row| row.target == "content-000000-source-000001")
                .unwrap()
                .source_bytes_read,
            20
        );
        assert_eq!(
            progress
                .target_progress
                .iter()
                .find(|row| row.target == "content-000000-source-000002")
                .unwrap()
                .fetch_attempts,
            0
        );
        assert!(
            progress.targets_active.iter().any(|row| row.contains("100 B / 100 B")),
            "finished fragment remains visible until its receipt is published: {:?}",
            progress.targets_active,
        );
    }

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
        crate::title_index::TitleIndex::open(
            root.path().join("archive.swtitle"),
        )
        .unwrap();
        assert!(root.path().join("archive.complete").exists());
        assert!(
            !node_path(root.path(), &plan, "content", 0).exists(),
            "consumed target survived its durable replacement"
        );
        std::fs::remove_file(root.path().join("archive.complete")).unwrap();
        std::fs::remove_file(root.path().join("archive.swtitle")).unwrap();
        assert!(recover_direct_build_completion(root.path(), &plan).unwrap());
        crate::title_index::TitleIndex::open(
            root.path().join("archive.swtitle"),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("archive.complete"))
                .unwrap()
                .trim_end(),
            plan.plan_id
        );
    }

    #[test]
    fn overlapping_content_parts_are_merged_instead_of_concatenated() {
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
        plan.plan_id = direct_plan_id(&plan).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::create_dir(scratch.path().join("nodes")).unwrap();
        assert_eq!(plan.content_target_count(), 2);
        assert_eq!(
            plan.target_name("content", 0).as_deref(),
            Some("content-000000-source-000000")
        );
        assert_eq!(
            plan.target_name("content", 1).as_deref(),
            Some("content-000000-source-000001")
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
        materialize_direct_build_node(
            &Client::new(),
            scratch.path(),
            &plan,
            "content",
            1,
            2,
            &|_| {},
        )
        .unwrap();
        assert!(validate_node(scratch.path(), &plan, "content", 0).unwrap());
        assert!(validate_node(scratch.path(), &plan, "content", 1).unwrap());
        let archive = assemble_direct_build(scratch.path(), &plan, &|_| {}).unwrap();
        crate::archive_set::ArchiveSetReader::open(archive).unwrap();
        assert_eq!(first.hits(), 1);
        assert_eq!(slice.hits(), 1);
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
}
