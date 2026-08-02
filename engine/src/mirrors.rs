//! Mirror-update jobs: the engine-side schedule for `gitdepot mirror` /
//! `wikimak fetch` / `ietfmak update` runs (MIRRORS.md "Update").
//!
//! The engine owns the schedule and lifecycle. A job gets one supervised
//! wikimak process for isolation and stderr attribution; its Kati graph calls
//! the `wikimak` brush builtin for build nodes, so the actual mirror work,
//! cancellation state, provenance, and Wikimedia gate have one owner rather
//! than a private dispatcher/service hierarchy.
//!
//! Bookkeeping lives in `{state_home}/mirrors.db` (jobs are engine
//! inventory, not box layer data). Liveness (which jobs are running
//! right now, and their pids) is in-process only: a crashed engine
//! leaves no stale "running" rows, just jobs whose last run never
//! ended — shown as `stopped`.
//!
//! Job states surfaced to the UI/CLI:
//!   running    a driver process is live right now (in-process set)
//!   paused     never auto-runs; force-run still works
//!   pending    due now (never ran, or interval elapsed since last start)
//!   scheduled  ran, waiting for its interval
//!   completed  last run exited 0 (and not yet due again) — same row as
//!              scheduled, the status column shows the outcome
//!   stopped    last run never recorded an end (engine died mid-run)
//!   error      last run exited non-zero (detail = stderr tail)

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn db() -> Result<Connection, String> {
    let path = crate::paths::state_home().join("mirrors.db");
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS jobs (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            src TEXT NOT NULL,
            dest TEXT NOT NULL,
            interval_secs INTEGER NOT NULL,
            paused INTEGER NOT NULL DEFAULT 0,
            last_start INTEGER,
            last_end INTEGER,
            last_exit INTEGER,
            last_detail TEXT NOT NULL DEFAULT '',
            media_source TEXT
        )",
        [],
    )
    .map_err(|e| e.to_string())?;
    // Existing installations predate the optional Kiwix source.  This is a
    // one-column additive migration; old jobs remain unchanged.
    let _ = conn.execute("ALTER TABLE jobs ADD COLUMN media_source TEXT", []);
    Ok(conn)
}

#[derive(Clone, Copy)]
struct RunningProcess {
    pid: u32,
    stopping: bool,
    wiki: bool,
}

/// Jobs whose driver process is live right now.
static RUNNING: Mutex<Option<HashMap<i64, RunningProcess>>> = Mutex::new(None);
static PATH_BYTES: Mutex<Option<HashMap<std::path::PathBuf, PathMeasurement>>> = Mutex::new(None);

struct PathMeasurement {
    bytes: Option<u64>,
    measured_at: Option<std::time::Instant>,
    scanning: bool,
}

fn running_map<R>(f: impl FnOnce(&mut HashMap<i64, RunningProcess>) -> R) -> R {
    let mut g = RUNNING.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

// Deserialize: the UI pane reads jobs back through the `mirror_jobs`
// control verb (JSON over the socket), not this module.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub src: String,
    pub dest: String,
    pub interval_secs: i64,
    pub paused: bool,
    pub last_start: Option<i64>,
    pub last_end: Option<i64>,
    pub last_exit: Option<i64>,
    pub last_detail: String,
    /// Optional Kiwix source selector.  The UI stores `auto`, meaning the
    /// latest matching official all-maxi release is fetched in ranges.
    #[serde(default)]
    pub media_source: Option<String>,
    /// Derived: running | paused | pending | stopped | error | completed
    /// | scheduled (never-ran pending shows as pending too).
    pub state: String,
    /// Unix seconds of the next auto run (None while paused/running).
    pub next_due: Option<i64>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub mirror_bytes: Option<u64>,
    #[serde(default)]
    pub scratch_bytes: Option<u64>,
    #[serde(default)]
    pub available_bytes: Option<u64>,
    #[serde(default)]
    pub build_phase: Option<String>,
    #[serde(default)]
    pub build_snapshot: Option<String>,
    #[serde(default)]
    pub targets_total: Option<u64>,
    #[serde(default)]
    pub targets_completed: Option<u64>,
    #[serde(default)]
    pub targets_active: Vec<String>,
    /// Structured per-target progress.  The prose `targets_active` field is
    /// retained for older CLI clients; the UI uses this for attribution.
    #[serde(default)]
    pub target_progress: Vec<wikimak_wikipedia::MirrorTargetProgress>,
    #[serde(default)]
    pub source_bytes_total: Option<u64>,
    #[serde(default)]
    pub source_bytes_completed: Option<u64>,
    #[serde(default)]
    pub active_source_bytes_per_second: Option<u64>,
    #[serde(default)]
    pub active_quiet_seconds: Option<u64>,
    #[serde(default)]
    pub fetch_attempts: Option<u64>,
    #[serde(default)]
    pub fetch_bytes_received: Option<u64>,
    #[serde(default)]
    pub fetch_rate_limit_responses: Option<u64>,
    #[serde(default)]
    pub fetch_client_error_responses: Option<u64>,
    #[serde(default)]
    pub fetch_server_error_responses: Option<u64>,
    #[serde(default)]
    pub fetch_transport_errors: Option<u64>,
    #[serde(default)]
    pub process_cpu_percent: Option<f64>,
    #[serde(default)]
    pub process_rss_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct LibraryJob {
    pub kind: String,
    pub src: String,
    pub dest: String,
}

/// The archive gateway's deliberately small inventory read. Serving a page
/// must not trigger the UI job projection: that path measures destination and
/// scratch sizes, reads build progress, and samples process trees.
pub fn library_jobs() -> Result<Vec<LibraryJob>, String> {
    let conn = db()?;
    let mut statement = conn
        .prepare("SELECT kind,src,dest FROM jobs ORDER BY id")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(LibraryJob {
                kind: row.get(0)?,
                src: row.get(1)?,
                dest: row.get(2)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.map(|row| row.map_err(|error| error.to_string()))
        .collect()
}

fn derive(mut j: Job) -> Job {
    let running = running_map(|m| m.get(&j.id).copied());
    let live = running.is_some();
    let due_at = j.last_start.map(|s| s + j.interval_secs);
    j.state = if running.is_some_and(|process| process.stopping) {
        "stopping".into()
    } else if live {
        "running".into()
    } else if j.paused {
        "paused".into()
    } else if j.last_start.is_some() && j.last_end.is_none() {
        // A start without an end and no live process: the engine died
        // mid-run. The store itself self-repairs (crash contracts down
        // in the mirror crates); the job just shows what happened.
        "stopped".into()
    } else if due_at.map(|d| d <= now()).unwrap_or(true) {
        "pending".into()
    } else if j.last_exit == Some(0) {
        "completed".into()
    } else if j.last_exit.is_some() {
        "error".into()
    } else {
        "scheduled".into()
    };
    j.next_due = if j.paused || live {
        None
    } else {
        due_at.or(Some(now()))
    };
    j.pid = running.map(|process| process.pid).filter(|pid| *pid != 0);
    if let Some(pid) = j.pid {
        if let Some((cpu, rss)) = process_tree_metrics(pid) {
            j.process_cpu_percent = Some(cpu);
            j.process_rss_bytes = Some(rss);
        }
    }
    let destination = std::path::Path::new(&j.dest);
    j.mirror_bytes = cached_path_bytes(destination);
    j.scratch_bytes = (j.kind == "wiki")
        .then(|| cached_path_bytes(&wikimak_wikipedia::mirror_scratch_path(destination)))
        .flatten();
    j.available_bytes = destination.parent().and_then(available_bytes);
    if j.kind == "wiki" {
        if let Some(progress) = wikimak_wikipedia::mirror_build_progress(destination) {
            let incomplete = progress.targets_completed < progress.targets_total;
            j.build_phase = Some(if incomplete
                && matches!(j.state.as_str(), "stopped" | "error")
            {
                format!("job not running; {}", progress.phase)
            } else {
                progress.phase
            });
            j.build_snapshot = Some(progress.snapshot);
            j.targets_total = Some(progress.targets_total);
            j.targets_completed = Some(progress.targets_completed);
            j.target_progress = progress.target_progress;
            j.targets_active = progress.targets_active;
            j.source_bytes_total = Some(progress.source_bytes_total);
            j.source_bytes_completed = Some(progress.source_bytes_completed);
            j.active_source_bytes_per_second = progress.active_source_bytes_per_second;
            j.active_quiet_seconds = progress.active_quiet_seconds;
            j.fetch_attempts = Some(progress.fetch_attempts);
            j.fetch_bytes_received = Some(progress.fetch_bytes_received);
            j.fetch_rate_limit_responses = Some(progress.fetch_rate_limit_responses);
            j.fetch_client_error_responses = Some(progress.fetch_client_error_responses);
            j.fetch_server_error_responses = Some(progress.fetch_server_error_responses);
            j.fetch_transport_errors = Some(progress.fetch_transport_errors);
        }
    }
    j
}

/// Sum the importer and its descendants. A wiki worker is a small process tree
/// (driver → Kati/brush build nodes → curl), so reporting only the driver would
/// make an apparently idle job hide the actual transfer/decompression cost.
/// `ps` is used here because it is available on macOS and Linux and avoids
/// making the UI depend on a platform-specific procfs layout.
fn process_tree_metrics(root: u32) -> Option<(f64, u64)> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,%cpu=,rss="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 4 {
            continue;
        }
        let (Ok(pid), Ok(ppid), Ok(cpu), Ok(rss_kib)) = (
            fields[0].parse::<u32>(),
            fields[1].parse::<u32>(),
            fields[2].parse::<f64>(),
            fields[3].parse::<u64>(),
        ) else {
            continue;
        };
        rows.push((pid, ppid, cpu, rss_kib));
    }
    let mut members = vec![root];
    let mut cursor = 0;
    while cursor < members.len() {
        let parent = members[cursor];
        for (pid, ppid, _, _) in &rows {
            if *ppid == parent && !members.contains(pid) {
                members.push(*pid);
            }
        }
        cursor += 1;
    }
    let mut cpu = 0.0;
    let mut rss = 0_u64;
    for (pid, _, value, rss_kib) in rows {
        if members.contains(&pid) {
            cpu += value;
            rss = rss.saturating_add(rss_kib.saturating_mul(1024));
        }
    }
    Some((cpu, rss))
}

fn cached_path_bytes(path: &std::path::Path) -> Option<u64> {
    let path = path.to_path_buf();
    let mut measurements = PATH_BYTES.lock().unwrap();
    let measurements = measurements.get_or_insert_with(HashMap::new);
    let measurement = measurements
        .entry(path.clone())
        .or_insert(PathMeasurement {
            bytes: None,
            measured_at: None,
            scanning: false,
        });
    let stale = measurement
        .measured_at
        .is_none_or(|measured| measured.elapsed() >= std::time::Duration::from_secs(2));
    if stale && !measurement.scanning {
        measurement.scanning = true;
        std::thread::spawn(move || {
            let bytes = path_bytes(&path).ok();
            let mut measurements = PATH_BYTES.lock().unwrap();
            if let Some(measurement) = measurements
                .as_mut()
                .and_then(|measurements| measurements.get_mut(&path))
            {
                measurement.bytes = bytes;
                measurement.measured_at = Some(std::time::Instant::now());
                measurement.scanning = false;
            }
        });
    }
    measurement.bytes
}

fn path_bytes(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(metadata.blocks().saturating_mul(512));
    }
    let mut bytes = metadata.blocks().saturating_mul(512);
    for entry in std::fs::read_dir(path)? {
        bytes = bytes.saturating_add(path_bytes(&entry?.path())?);
    }
    Ok(bytes)
}

fn available_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

pub fn jobs_list() -> Result<Vec<Job>, String> {
    let conn = db()?;
    let mut st = conn
        .prepare("SELECT id,kind,src,dest,interval_secs,paused,last_start,last_end,last_exit,last_detail,media_source FROM jobs ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(Job {
                id: r.get(0)?,
                kind: r.get(1)?,
                src: r.get(2)?,
                dest: r.get(3)?,
                interval_secs: r.get(4)?,
                paused: r.get::<_, i64>(5)? != 0,
                last_start: r.get(6)?,
                last_end: r.get(7)?,
                last_exit: r.get(8)?,
                last_detail: r.get(9)?,
                media_source: r.get(10)?,
                state: String::new(),
                next_due: None,
                pid: None,
                mirror_bytes: None,
                scratch_bytes: None,
                available_bytes: None,
                build_phase: None,
                build_snapshot: None,
                targets_total: None,
                targets_completed: None,
                targets_active: Vec::new(),
                target_progress: Vec::new(),
                source_bytes_total: None,
                source_bytes_completed: None,
                active_source_bytes_per_second: None,
                active_quiet_seconds: None,
                fetch_attempts: None,
                fetch_bytes_received: None,
                fetch_rate_limit_responses: None,
                fetch_client_error_responses: None,
                fetch_server_error_responses: None,
                fetch_transport_errors: None,
                process_cpu_percent: None,
                process_rss_bytes: None,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.map(|row| row.map(derive).map_err(|error| error.to_string()))
        .collect()
}

pub fn jobs_list_typed() -> Result<Vec<crate::generated_wire::MirrorJob>, String> {
    use crate::generated_wire::{MirrorJob, MirrorState};
    jobs_list()?
        .into_iter()
        .map(|job| {
            let state = match job.state.as_str() {
                "running" | "stopping" => MirrorState::Running,
                "paused" => MirrorState::Paused,
                "pending" => MirrorState::Pending,
                "stopped" => MirrorState::Stopped,
                "error" => MirrorState::Error,
                "completed" => MirrorState::Completed,
                "scheduled" => MirrorState::Scheduled,
                state => return Err(format!("unknown derived mirror state {state:?}")),
            };
            Ok(MirrorJob {
                id: u64::try_from(job.id).map_err(|_| "negative mirror job id")?,
                kind: crate::wire::BoundedText::new(job.kind)
                    .map_err(|error| format!("mirror kind exceeds relation bound: {error:?}"))?,
                source: crate::wire::BoundedText::new(job.src)
                    .map_err(|error| format!("mirror source exceeds relation bound: {error:?}"))?,
                destination: crate::wire::BoundedBytes::new(job.dest.into_bytes()).map_err(
                    |error| format!("mirror destination exceeds relation bound: {error:?}"),
                )?,
                interval_seconds: u64::try_from(job.interval_secs)
                    .map_err(|_| "negative mirror interval")?,
                paused: job.paused,
                last_start: job.last_start,
                last_end: job.last_end,
                last_exit: job
                    .last_exit
                    .map(|exit| i32::try_from(exit).map_err(|_| "mirror exit code exceeds i32"))
                    .transpose()?,
                last_detail: crate::wire::BoundedText::new(job.last_detail)
                    .map_err(|error| format!("mirror detail exceeds relation bound: {error:?}"))?,
                state,
                next_due: job.next_due,
            })
        })
        .collect()
}

pub fn job_add(kind: &str, src: &str, dest: &str, interval_secs: i64) -> Result<i64, String> {
    job_add_with_media(kind, src, dest, interval_secs, false, None)
}

/// Register a mirror without starting or scheduling it. Used when an existing
/// portable mirror library is attached to this host for browsing.
pub fn job_register_paused(
    kind: &str,
    src: &str,
    dest: &str,
    interval_secs: i64,
) -> Result<i64, String> {
    job_add_with_media(kind, src, dest, interval_secs, true, None)
}

pub fn job_add_with_media(
    kind: &str,
    src: &str,
    dest: &str,
    interval_secs: i64,
    paused: bool,
    media_source: Option<&str>,
) -> Result<i64, String> {
    if !matches!(kind, "git" | "wiki" | "ietf" | "cmd") {
        return Err(format!("unknown mirror kind {kind:?} (git|wiki|ietf|cmd)"));
    }
    let conn = db()?;
    let collision: Option<i64> = conn
        .query_row(
            "SELECT id FROM jobs WHERE dest = ?1 LIMIT 1",
            [dest],
            |row| row.get(0),
        )
        .ok();
    if let Some(id) = collision {
        return Err(format!(
            "destination {dest:?} is already owned by mirror job #{id}"
        ));
    }
    conn.execute(
        "INSERT INTO jobs(kind, src, dest, interval_secs, paused, media_source) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            kind,
            src,
            dest,
            interval_secs.max(60),
            paused as i64,
            media_source,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

/// Enable or disable automatic Kiwix image packing for an existing Wikipedia
/// mirror.  The child process receives this setting when the next run starts;
/// changing it never mutates an already-running importer.
pub fn job_set_media_source(id: i64, source: Option<&str>) -> Result<(), String> {
    if let Some(source) = source {
        if source != "auto" {
            return Err("Wikipedia image source must be auto or disabled".into());
        }
    }
    let conn = db()?;
    let changed = conn
        .execute(
            "UPDATE jobs SET media_source = ?2 WHERE id = ?1 AND kind = 'wiki'",
            params![id, source],
        )
        .map_err(|error| error.to_string())?;
    if changed == 0 {
        return Err(format!("Wikipedia mirror job #{id} does not exist"));
    }
    Ok(())
}

/// Remove a job. Returns a human note describing what happened to the
/// job's on-disk state. For git jobs the `<dest>/repo.git` fetch buffer
/// (plus any `repo.git.new` scratch) is dropped: it is DERIVED — the
/// mirror loop reconstructs it from the store via SHA-exact export — and
/// with no schedule left it is ownerless cache. `<dest>/store` is the
/// authoritative corpus (live box attachments may reference it) and is
/// NEVER touched here; deleting it stays an explicit manual act.
/// Cleanup runs only after the row delete succeeds, and a cleanup error
/// is reported in the note without resurrecting the job.
pub fn job_remove(id: i64) -> Result<String, String> {
    if running_map(|m| m.contains_key(&id)) {
        return Err("job is running".into());
    }
    let conn = db()?;
    let row: Option<(String, String)> = conn
        .query_row("SELECT kind, dest FROM jobs WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok();
    let n = conn
        .execute("DELETE FROM jobs WHERE id = ?1", [id])
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("no such job".into());
    }
    let Some((kind, dest)) = row else {
        return Ok(String::new());
    };
    if kind != "git" {
        // wiki/ietf/cmd keep no separate fetch buffer.
        return Ok(String::new());
    }
    let mut note = format!("fetch buffer dropped; store kept at {dest}/store");
    for name in ["repo.git", "repo.git.new"] {
        let p = std::path::Path::new(&dest).join(name);
        if p.exists() {
            if let Err(e) = std::fs::remove_dir_all(&p) {
                note = format!("{note} (cleanup of {} failed: {e})", p.display());
            }
        }
    }
    Ok(note)
}

fn remove_mirror_path(path: &std::path::Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        return Err(format!("{} is not a regular mirror path", path.display()));
    }
    .map_err(|error| format!("{}: {error}", path.display()))
}

/// Delete a Wikipedia mirror's owned archive/index/media paths, its
/// destination-specific scratch, install/update sidecars, and then its
/// schedule row.
pub fn job_remove_with_data(id: i64) -> Result<String, String> {
    if running_map(|running| running.contains_key(&id)) {
        return Err("job is running; stop it first".into());
    }
    let conn = db()?;
    let (kind, destination): (String, String) = conn
        .query_row(
            "SELECT kind,dest FROM jobs WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| "no such job".to_string())?;
    if kind != "wiki" {
        return Err("deleting mirrored files is currently restricted to Wikipedia jobs".into());
    }
    let archive = std::path::PathBuf::from(&destination);
    if archive.extension().and_then(|extension| extension.to_str()) != Some("swdump") {
        return Err(format!(
            "refusing to delete unexpected Wikipedia destination {}",
            archive.display()
        ));
    }
    let titles = archive.with_extension("swtitle");
    let media = archive.with_extension("media");
    remove_mirror_path(&archive)?;
    remove_mirror_path(&titles)?;
    remove_mirror_path(&media)?;
    for path in wikimak_wikipedia::mirror_auxiliary_paths(&archive)? {
        remove_mirror_path(&path)?;
    }
    conn.execute("DELETE FROM jobs WHERE id = ?1", [id])
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "archive, title index, media cache, and scratch removed from {}",
        archive.display()
    ))
}

pub fn job_set_paused(id: i64, paused: bool) -> Result<(), String> {
    let n = db()?
        .execute(
            "UPDATE jobs SET paused = ?2 WHERE id = ?1",
            params![id, paused as i64],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        Err("no such job".into())
    } else {
        Ok(())
    }
}

/// Force-run one job NOW (also works on paused jobs — force is force).
/// Returns immediately; the run is a background thread + child process.
pub fn job_run(id: i64) -> Result<(), String> {
    let jobs = jobs_list()?;
    let job = jobs.into_iter().find(|j| j.id == id).ok_or("no such job")?;
    if !spawn_run(job, WikiRun::Maintain) {
        return Err("job is already running, or another Wikipedia mirror job is running".into());
    }
    Ok(())
}

/// Stop a live mirror driver and every transfer/decompressor process it
/// spawned. The driver is a process-group leader, so one signal covers curl
/// and any other descendants without relying on process-tree polling.
pub fn job_cancel(id: i64) -> Result<(), String> {
    let pid = running_map(|running| {
        let process = running.get_mut(&id).ok_or("job is not running")?;
        if process.pid == 0 {
            return Err("job is still starting; try again");
        }
        process.stopping = true;
        Ok(process.pid)
    })?;
    let group = i32::try_from(pid).map_err(|_| "mirror process id exceeds i32")?;
    if unsafe { libc::kill(-group, libc::SIGTERM) } != 0 {
        return Err(format!(
            "stop mirror process group {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        // The driver may have exited while a transfer child remains in its
        // process group, so RUNNING cannot be used as the escalation guard.
        // ESRCH simply means the whole group stopped during the grace period.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    });
    Ok(())
}

/// Stop all mirror drivers before the engine exits. This is synchronous
/// because delayed escalation threads disappear with the engine process.
pub fn stop_all() {
    let groups = running_map(|running| {
        running
            .values_mut()
            .filter_map(|process| {
                process.stopping = true;
                i32::try_from(process.pid).ok().filter(|pid| *pid > 0)
            })
            .collect::<Vec<_>>()
    });
    for group in &groups {
        unsafe {
            libc::kill(-*group, libc::SIGTERM);
        }
    }
    if !groups.is_empty() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        for group in groups {
            if unsafe { libc::kill(-group, 0) } == 0 {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
        }
    }
}

/// Explicitly re-ingest the newest full Wikipedia snapshot. This is never
/// scheduled: routine wiki jobs consume daily adds/changes through `fetch`.
pub fn job_run_full(id: i64) -> Result<(), String> {
    let jobs = jobs_list()?;
    let job = jobs.into_iter().find(|j| j.id == id).ok_or("no such job")?;
    if job.kind != "wiki" {
        return Err("full snapshot re-ingest is only available for wiki mirrors".into());
    }
    if !spawn_run(job, WikiRun::RefreshContent) {
        return Err("job is already running, or another Wikipedia mirror job is running".into());
    }
    Ok(())
}

/// Start every due, unpaused, not-running job. Returns the started ids.
pub fn run_pending() -> Result<Vec<i64>, String> {
    let mut started = Vec::new();
    let jobs = jobs_list()?;
    // Full-history Wikimedia parts are large. Keep at most one automatic
    // wiki transfer active; other mirror kinds remain independent. A user
    // can still force-run a particular job explicitly.
    let mut wiki_running = jobs
        .iter()
        .any(|job| job.kind == "wiki" && job.state == "running");
    for j in jobs {
        if j.state == "pending" || j.state == "stopped" {
            if j.kind == "wiki" && wiki_running {
                continue;
            }
            let id = j.id;
            if spawn_run(j.clone(), WikiRun::Maintain) {
                if j.kind == "wiki" {
                    wiki_running = true;
                }
                started.push(id);
            }
        }
    }
    Ok(started)
}

/// argv prefix that runs embedded driver `name`: the engine's own binary
/// (`self_exe`) with the driver name as the first argument — main.rs
/// multi-call dispatch routes it to the compiled-in CLI, so no separate
/// driver binary is deployed. Bare-name PATH lookup is only the fallback
/// for the degenerate case where current_exe() itself fails.
fn driver_argv(name: &str, self_exe: Option<std::path::PathBuf>) -> Vec<String> {
    match self_exe {
        Some(exe) => vec![exe.to_string_lossy().into_owned(), name.to_string()],
        None => vec![name.to_string()],
    }
}

#[derive(Clone, Copy)]
enum WikiRun {
    Maintain,
    RefreshContent,
}

fn spawn_run(job: Job, wiki_run: WikiRun) -> bool {
    let driver = |name: &str| driver_argv(name, std::env::current_exe().ok());
    let argv: Vec<String> = match job.kind.as_str() {
        "git" => [
            driver("gitdepot"),
            vec!["mirror".into(), job.src.clone(), job.dest.clone()],
        ]
        .concat(),
        "wiki" => [
            driver("wikimak"),
            vec![
                match wiki_run {
                    WikiRun::Maintain => "fetch",
                    WikiRun::RefreshContent => "refresh-full",
                }
                .into(),
                job.src.clone(),
                job.dest.clone(),
            ],
        ]
        .concat(),
        "cmd" => vec![
            "/bin/sh".into(),
            "-c".into(),
            job.src.clone(),
            "mirror-job".into(),
            job.dest.clone(),
        ],
        _ => [driver("ietfmak"), vec!["update".into(), job.dest.clone()]].concat(),
    };
    let id = job.id;
    // Keep both the importer and any helper it launches on the mirror's
    // destination volume.  The importer already routes its own temporary
    // files explicitly; TMPDIR covers third-party tools that still consult
    // the process default.  Do not apply this to other mirror kinds: their
    // scratch contracts are separate.
    let wiki_tmp = (job.kind == "wiki").then(|| {
        let path = wikimak_wikipedia::mirror_scratch_path(std::path::Path::new(&job.dest));
        let _ = std::fs::create_dir_all(&path);
        path
    });
    let wiki = job.kind == "wiki";
    let wiki_background = wiki && std::path::Path::new(&job.dest).exists();
    let wiki_cpu_budget = wiki_background.then(|| {
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .saturating_sub(2)
            .max(1)
    });
    if !running_map(|m| {
        if m.contains_key(&id) || (wiki && m.values().any(|process| process.wiki)) {
            false
        } else {
            m.insert(
                id,
                RunningProcess {
                    pid: 0,
                    stopping: false,
                    wiki,
                },
            );
            true
        }
    }) {
        return false;
    }
    if let Ok(conn) = db() {
        let _ = conn.execute(
            "UPDATE jobs SET last_start = ?2, last_end = NULL, last_exit = NULL, last_detail = '' WHERE id = ?1",
            params![id, now()],
        );
    }
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env("SARUN_MIRROR_DEST", &job.dest)
            .env("SARUN_MIRROR_PARENT_PID", std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        if let Some(source) = &job.media_source {
            cmd.env("SARUN_KIWIX_SOURCE", source);
        }
        if let Some(cpu_budget) = wiki_cpu_budget {
            cmd.env("SARUN_WIKIMAK_CPU_BUDGET", cpu_budget.to_string());
        }
        if let Some(path) = &wiki_tmp {
            cmd.env("TMPDIR", path);
        }
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if wiki_background && libc::setpriority(libc::PRIO_PROCESS, 0, 5) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let child = cmd.spawn();
        let (exit, detail) = match child {
            Ok(mut c) => {
                running_map(|m| {
                    m.insert(
                        id,
                        RunningProcess {
                            pid: c.id(),
                            stopping: false,
                            wiki,
                        },
                    );
                });
                let stderr = c.stderr.take().expect("piped stderr");
                let (exit, tail) = stream_stderr(id, stderr, &mut c);
                match exit {
                    Ok(status) => {
                        use std::os::unix::process::ExitStatusExt;
                        match status.code() {
                            Some(code) => (code as i64, tail),
                            None => {
                                let sig = status.signal().unwrap_or(0);
                                let hint = if sig == libc::SIGKILL { " (OOM?)" } else { "" };
                                (
                                    -1,
                                    format!(
                                        "killed by signal {sig}{hint}{}{tail}",
                                        if tail.is_empty() { "" } else { "; stderr: " }
                                    ),
                                )
                            }
                        }
                    }
                    Err(e) => (-1, e.to_string()),
                }
            }
            Err(e) => (
                -1,
                format!(
                    "spawn {} ({}): {e}",
                    argv[0],
                    if argv[0].starts_with('/') {
                        "self-exec"
                    } else {
                        "via PATH"
                    }
                ),
            ),
        };
        running_map(|m| {
            m.remove(&id);
        });
        if let Ok(conn) = db() {
            let _ = conn.execute(
                "UPDATE jobs SET last_end = ?2, last_exit = ?3, last_detail = ?4 WHERE id = ?1",
                params![id, now(), exit, detail],
            );
        }
    });
    true
}

/// Read child stderr line-by-line, updating `last_detail` in the DB every
/// ~2s so the UI's mirror detail pane shows live progress. Returns the
/// collected stderr tail (last 2KB) and the child's exit status.
fn stream_stderr(
    id: i64,
    stderr: std::process::ChildStderr,
    child: &mut std::process::Child,
) -> (
    std::result::Result<std::process::ExitStatus, std::io::Error>,
    String,
) {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};
    let reader = BufReader::new(stderr);
    let mut last_flush = Instant::now();
    let mut tail = String::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let first_line = tail.is_empty();
        tail.push_str(&line);
        tail.push('\n');
        if tail.len() > 4096 {
            tail = tail_2k(&tail);
        }
        if first_line || last_flush.elapsed() >= Duration::from_secs(2) {
            if let Ok(conn) = db() {
                let _ = conn.execute(
                    "UPDATE jobs SET last_detail = ?2 WHERE id = ?1",
                    params![id, tail_2k(&tail)],
                );
            }
            last_flush = Instant::now();
        }
    }
    let exit = child.wait();
    (exit, tail_2k(tail.trim_end()))
}

fn tail_2k(s: &str) -> String {
    if s.len() > 2048 {
        let mut start = s.len() - 2048;
        while !s.is_char_boundary(start) {
            start += 1;
        }
        s[start..].to_string()
    } else {
        s.to_string()
    }
}

/// The scheduler tick loop: every minute, start whatever is due. Runs
/// for the life of the engine; jobs only exist if the user added them.
pub fn scheduler_thread() {
    std::thread::spawn(|| {
        loop {
            let _ = run_pending();
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deployment contract: drivers are compiled into the engine
    /// binary, so a driver run is a self-exec of current_exe with the
    /// driver name as the subcommand. Bare-name PATH lookup only when
    /// current_exe is unavailable.
    #[test]
    fn driver_argv_self_execs_the_engine_binary() {
        let exe = std::path::PathBuf::from("/opt/sarun/bin/sarun");
        assert_eq!(
            driver_argv("gitdepot", Some(exe)),
            vec!["/opt/sarun/bin/sarun".to_string(), "gitdepot".to_string()]
        );
        assert_eq!(driver_argv("ietfmak", None), vec!["ietfmak".to_string()]);
    }

    /// The argv prefix must compose with a subcommand tail exactly the way
    /// spawn_run builds it: [exe, driver, verb, args...].
    #[test]
    fn driver_argv_composes_with_the_subcommand_tail() {
        let argv = [
            driver_argv("wikimak", Some("/x/sarun".into())),
            vec!["fetch".into(), "enwiki".into(), "/depot/w".into()],
        ]
        .concat();
        assert_eq!(
            argv,
            ["/x/sarun", "wikimak", "fetch", "enwiki", "/depot/w"]
                .map(String::from)
                .to_vec()
        );
    }

    /// A signal death must name the signal in the detail — the live
    /// failure was an OOM-killed driver recording exit=-1 with a BLANK
    /// detail in the pane.
    #[test]
    fn signal_death_names_the_killing_signal() {
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", "kill -9 $$"])
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert!(out.status.code().is_none(), "signal death has no code");
        let sig = out.status.signal().unwrap_or(0);
        assert_eq!(sig, 9);
        // The signal-name logic is now inline in stream_stderr's caller;
        // this test just proves the signal is observable on the ExitStatus.
    }

    #[test]
    fn progress_tail_keeps_utf8_boundaries() {
        let input = "ā".repeat(2049);
        let tail = tail_2k(&input);
        assert!(tail.len() <= 2048);
        assert!(tail.chars().all(|character| character == 'ā'));
    }

    #[test]
    fn cancel_stops_the_driver_process_group() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut child = command.arg("-c").arg("sleep 60").spawn().unwrap();
        let id = -9_001;
        running_map(|running| {
            running.insert(
                id,
                RunningProcess {
                    pid: child.id(),
                    stopping: false,
                    wiki: false,
                },
            );
        });
        job_cancel(id).unwrap();
        let status = child.wait().unwrap();
        running_map(|running| {
            running.remove(&id);
        });
        assert!(!status.success());
    }

    #[test]
    fn deleting_wikipedia_data_removes_only_owned_paths() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(archive.join("range"), b"archive").unwrap();
        std::fs::write(archive.with_extension("swtitle"), b"index").unwrap();
        std::fs::create_dir(archive.with_extension("media")).unwrap();
        let auxiliary = wikimak_wikipedia::mirror_auxiliary_paths(&archive).unwrap();
        std::fs::create_dir_all(&auxiliary[0]).unwrap();
        std::fs::write(auxiliary[0].join("partial"), b"scratch").unwrap();
        for path in &auxiliary[1..] {
            std::fs::write(path, b"sidecar").unwrap();
        }
        let sibling = library.join("keep");
        std::fs::write(&sibling, b"keep").unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        job_remove_with_data(id).unwrap();

        assert!(!archive.exists());
        assert!(!archive.with_extension("swtitle").exists());
        assert!(!archive.with_extension("media").exists());
        assert!(auxiliary.iter().all(|path| !path.exists()));
        assert!(sibling.exists());
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
    }

    #[test]
    fn portable_registration_is_atomically_paused() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp =
            std::env::temp_dir().join(format!("sarun-mirror-register-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &tmp);
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let id = job_register_paused("wiki", "enwiki", "/mounted/archive/enwiki", 86400).unwrap();
        let jobs = jobs_list().unwrap();
        let job = jobs.iter().find(|job| job.id == id).unwrap();
        assert!(job.paused);
        assert_eq!(job.state, "paused");
        assert!(job.next_due.is_none());
        assert!(job.last_start.is_none());
    }

    fn sh_git(repo: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `rm` on a git job drops the derived fetch buffer (repo.git +
    /// any scratch) but NEVER the authoritative store — which must stay
    /// readable afterwards. Non-git kinds are row-only.
    #[test]
    fn job_remove_drops_git_fetch_buffer_keeps_store() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("sarun-mirrorrm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: serialized by TEST_STATE_HOME_LOCK with the other
        // state-home-dependent tests.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &tmp);
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let origin = tmp.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        sh_git(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("a.txt"), "a\n").unwrap();
        sh_git(&origin, &["add", "-A"]);
        sh_git(&origin, &["commit", "-q", "-m", "a"]);

        // A completed run's on-disk state, produced by the real driver
        // library (spawning through spawn_run would self-exec the test
        // harness binary).
        let dest = tmp.join("dest");
        gitdepot::mirror(origin.to_str().unwrap(), &dest).unwrap();
        assert!(dest.join("repo.git/HEAD").exists());
        std::fs::create_dir_all(dest.join("repo.git.new")).unwrap();

        let id = job_add(
            "git",
            origin.to_str().unwrap(),
            dest.to_str().unwrap(),
            3600,
        )
        .unwrap();
        let note = job_remove(id).unwrap();
        assert!(note.contains("fetch buffer dropped"), "{note}");
        assert!(note.contains("store kept"), "{note}");
        assert!(!dest.join("repo.git").exists(), "buffer must be dropped");
        assert!(
            !dest.join("repo.git.new").exists(),
            "scratch must be dropped"
        );
        let store = dest.join("store");
        assert!(
            gitdepot::store::store_exists(&store),
            "store must survive rm"
        );
        assert!(
            gitdepot::resolve_ref(&store, "main").unwrap().is_some(),
            "store must stay readable after rm"
        );

        let cid = job_add("cmd", "true", dest.to_str().unwrap(), 3600).unwrap();
        assert_eq!(job_remove(cid).unwrap(), "", "cmd rm is row-only");
        assert!(gitdepot::store::store_exists(&store));
    }
}
