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
//! inventory, not box layer data). Runs and their identities are durable;
//! the in-process owner map contains only process handles and telemetry.
//!
//! Job states surfaced to the UI/CLI:
//!   starting   a driver launch is reserved but has not yielded its process ID
//!   running    a driver process was spawned for the durable active RunId
//!   deleting   destination ownership is retained while cleanup is resumable
//! `paused` is a separate schedule flag: it never hides the run outcome, and
//! a force-run still works.
//!   pending    never ran, or a successful run is due again
//!   completed  last run exited 0; next due is measured from its completion
//!   cancelled  the user stopped the last run
//!   interrupted the engine disappeared while owning the last run
//!   error      last run exited non-zero (detail = stderr tail)
//! Interrupted, failed, and cancelled attempts require an explicit run; they
//! are not scheduler requests merely because time passed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn db() -> Result<Connection, String> {
    let path = crate::paths::state_home().join("mirrors.db");
    let mut conn = Connection::open(&path).map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| e.to_string())?;
    migrate_schema(&mut conn)?;
    Ok(conn)
}

const MIRROR_SCHEMA_VERSION: i64 = 1;

fn create_schema(transaction: &Transaction<'_>) -> Result<(), String> {
    transaction
        .execute_batch(
            "CREATE TABLE jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                src TEXT NOT NULL,
                dest TEXT NOT NULL UNIQUE,
                interval_secs INTEGER NOT NULL,
                paused INTEGER NOT NULL DEFAULT 0 CHECK(paused IN (0,1)),
                media_source TEXT,
                delete_mode TEXT CHECK(delete_mode IS NULL OR
                    delete_mode IN ('registration','data'))
            );
            CREATE TABLE runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                request TEXT NOT NULL CHECK(request IN ('explicit','scheduled','full')),
                state TEXT NOT NULL CHECK(state IN
                    ('starting','running','stopping','succeeded','failed',
                     'cancelled','interrupted')),
                started_at INTEGER NOT NULL,
                spawned_at INTEGER,
                ended_at INTEGER,
                process_group INTEGER,
                exit_code INTEGER,
                stop_reason TEXT CHECK(stop_reason IS NULL OR
                    stop_reason IN ('user','shutdown')),
                detail TEXT NOT NULL DEFAULT '',
                CHECK(
                    (state='starting' AND spawned_at IS NULL AND ended_at IS NULL
                                      AND process_group IS NULL AND stop_reason IS NULL)
                 OR (state='running' AND spawned_at IS NOT NULL AND ended_at IS NULL
                                     AND process_group IS NOT NULL AND stop_reason IS NULL)
                 OR (state='stopping' AND ended_at IS NULL AND stop_reason IS NOT NULL)
                 OR (state='succeeded' AND ended_at IS NOT NULL AND exit_code=0
                                       AND stop_reason IS NULL)
                 OR (state='failed' AND ended_at IS NOT NULL AND exit_code IS NOT NULL
                                    AND exit_code<>0 AND stop_reason IS NULL)
                 OR (state='cancelled' AND ended_at IS NOT NULL
                                       AND stop_reason='user')
                 OR (state='interrupted' AND ended_at IS NOT NULL
                                         AND exit_code IS NULL
                                         AND (stop_reason IS NULL OR stop_reason='shutdown'))
                )
            );
            CREATE UNIQUE INDEX one_active_run_per_job ON runs(job_id)
                WHERE state IN ('starting','running','stopping');
            CREATE INDEX runs_by_job ON runs(job_id,id DESC);",
        )
        .map_err(|error| error.to_string())
}

fn migrate_schema(conn: &mut Connection) -> Result<(), String> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if version == MIRROR_SCHEMA_VERSION {
        return Ok(());
    }
    if version != 0 {
        return Err(format!(
            "unsupported mirror database schema {version}; expected {MIRROR_SCHEMA_VERSION}"
        ));
    }
    let transaction = conn.transaction().map_err(|error| error.to_string())?;
    let version: i64 = transaction
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if version == MIRROR_SCHEMA_VERSION {
        return transaction.commit().map_err(|error| error.to_string());
    }
    let legacy_exists: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
                           WHERE type='table' AND name='jobs')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !legacy_exists {
        create_schema(&transaction)?;
    } else {
        let has_media_source = {
            let mut statement = transaction
                .prepare("PRAGMA table_info(jobs)")
                .map_err(|error| error.to_string())?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|error| error.to_string())?;
            let mut found = false;
            for column in columns {
                if column.map_err(|error| error.to_string())? == "media_source" {
                    found = true;
                }
            }
            found
        };
        transaction
            .execute("ALTER TABLE jobs RENAME TO legacy_jobs", [])
            .map_err(|error| error.to_string())?;
        create_schema(&transaction)?;
        let media = if has_media_source {
            "media_source"
        } else {
            "NULL"
        };
        transaction
            .execute_batch(&format!(
                "INSERT INTO jobs(id,kind,src,dest,interval_secs,paused,media_source)
                 SELECT id,kind,src,dest,interval_secs,paused,{media}
                 FROM legacy_jobs;
                 INSERT INTO runs(job_id,request,state,started_at,ended_at,exit_code,detail)
                 SELECT id,'explicit',
                        CASE
                          WHEN last_end IS NULL THEN 'interrupted'
                          WHEN last_exit = 0 THEN 'succeeded'
                          ELSE 'failed'
                        END,
                        last_start,
                        COALESCE(last_end,last_start),
                        CASE
                          WHEN last_end IS NULL THEN NULL
                          WHEN last_exit = 0 THEN 0
                          ELSE COALESCE(last_exit,-1)
                        END,
                        COALESCE(last_detail,'')
                 FROM legacy_jobs WHERE last_start IS NOT NULL;
                 DROP TABLE legacy_jobs;"
            ))
            .map_err(|error| error.to_string())?;
    }
    transaction
        .pragma_update(None, "user_version", MIRROR_SCHEMA_VERSION)
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RunId(i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    User,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeleteMode {
    Registration,
    Data,
}

impl DeleteMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Registration => "registration",
            Self::Data => "data",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationState {
    Active,
    Deleting(DeleteMode),
    Removed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationEvent {
    BeginDelete(DeleteMode),
    FinishDelete(DeleteMode),
}

fn transition_registration(
    state: RegistrationState,
    event: RegistrationEvent,
) -> Result<RegistrationState, &'static str> {
    match (state, event) {
        (RegistrationState::Active, RegistrationEvent::BeginDelete(mode)) => {
            Ok(RegistrationState::Deleting(mode))
        }
        (
            RegistrationState::Deleting(current),
            RegistrationEvent::BeginDelete(requested),
        ) if current == requested => Ok(state),
        (
            RegistrationState::Deleting(current),
            RegistrationEvent::FinishDelete(completed),
        ) if current == completed => Ok(RegistrationState::Removed),
        _ => Err("event is invalid for the current registration state"),
    }
}

impl StopReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Clone, Copy)]
struct RunningProcess {
    run_id: RunId,
    process_group: Option<u32>,
    stop_reason: Option<StopReason>,
}

/// The independent axes from which a mirror row is projected.
///
/// Keep these closed. Durable run rows are decoded once and every consumer
/// goes through `classify_job`; SQL-column precedence is not a lifecycle
/// model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScheduleClass {
    Enabled,
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationClass {
    Active,
    Deleting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptClass {
    NeverRun,
    Active,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeClass {
    Idle,
    Starting,
    Running,
    Stopping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DisplayClass {
    Starting,
    Running,
    Stopping,
    Deleting,
    Pending,
    Cancelled,
    Interrupted,
    Error,
    Completed,
}

impl DisplayClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Deleting => "deleting",
            Self::Pending => "pending",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Error => "error",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JobProjection {
    registration: RegistrationClass,
    schedule: ScheduleClass,
    attempt: AttemptClass,
    runtime: RuntimeClass,
    display: DisplayClass,
    next_due: Option<i64>,
    automatic_start: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PersistedRun {
    NeverRun,
    Starting {
        id: RunId,
        started_at: i64,
    },
    Running {
        id: RunId,
        started_at: i64,
        process_group: u32,
    },
    Stopping {
        id: RunId,
        started_at: i64,
        process_group: Option<u32>,
        reason: StopReason,
    },
    Idle {
        id: RunId,
        started_at: i64,
        ended_at: i64,
        outcome: AttemptClass,
        exit_code: Option<i64>,
    },
    Invalid,
}

impl Default for PersistedRun {
    fn default() -> Self {
        Self::NeverRun
    }
}

fn persisted_run(
    run_id: Option<i64>,
    state: Option<&str>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    exit_code: Option<i64>,
    process_group: Option<u32>,
    stop_reason: Option<&str>,
) -> PersistedRun {
    let Some(run_id) = run_id.map(RunId) else {
        return if state.is_none()
            && started_at.is_none()
            && ended_at.is_none()
            && exit_code.is_none()
            && process_group.is_none()
            && stop_reason.is_none()
        {
            PersistedRun::NeverRun
        } else {
            PersistedRun::Invalid
        };
    };
    let Some(started_at) = started_at else {
        return PersistedRun::Invalid;
    };
    match state {
        Some("starting")
            if ended_at.is_none()
                && exit_code.is_none()
                && process_group.is_none()
                && stop_reason.is_none() =>
        {
            PersistedRun::Starting {
                id: run_id,
                started_at,
            }
        }
        Some("running")
            if ended_at.is_none() && exit_code.is_none() && stop_reason.is_none() =>
        {
            match process_group {
                Some(process_group) => PersistedRun::Running {
                    id: run_id,
                    started_at,
                    process_group,
                },
                None => PersistedRun::Invalid,
            }
        }
        Some("stopping") if ended_at.is_none() && exit_code.is_none() => {
            let reason = match stop_reason {
                Some("user") => Some(StopReason::User),
                Some("shutdown") => Some(StopReason::Shutdown),
                _ => None,
            };
            match reason {
                Some(reason) => PersistedRun::Stopping {
                    id: run_id,
                    started_at,
                    process_group,
                    reason,
                },
                None => PersistedRun::Invalid,
            }
        }
        Some("starting") | Some("running") | Some("stopping") => PersistedRun::Invalid,
        Some(state) => {
            let Some(ended_at) = ended_at else {
                return PersistedRun::Invalid;
            };
            let outcome = match (state, exit_code, stop_reason) {
                ("succeeded", Some(0), None) => Some(AttemptClass::Succeeded),
                ("failed", Some(exit), None) if exit != 0 => Some(AttemptClass::Failed),
                ("cancelled", _, Some("user")) => Some(AttemptClass::Cancelled),
                ("interrupted", None, None | Some("shutdown")) => {
                    Some(AttemptClass::Interrupted)
                }
                _ => None,
            };
            match outcome {
                Some(outcome) => PersistedRun::Idle {
                    id: run_id,
                    started_at,
                    ended_at,
                    outcome,
                    exit_code,
                },
                None => PersistedRun::Invalid,
            }
        }
        None => PersistedRun::Invalid,
    }
}

fn attempt_class(run: &PersistedRun) -> AttemptClass {
    match run {
        PersistedRun::NeverRun => AttemptClass::NeverRun,
        PersistedRun::Starting { .. }
        | PersistedRun::Running { .. }
        | PersistedRun::Stopping { .. } => AttemptClass::Active,
        PersistedRun::Idle { outcome, .. } => *outcome,
        PersistedRun::Invalid => AttemptClass::Invalid,
    }
}

fn runtime_class(run: &PersistedRun) -> RuntimeClass {
    match run {
        PersistedRun::Starting { .. } => RuntimeClass::Starting,
        PersistedRun::Running { .. } => RuntimeClass::Running,
        PersistedRun::Stopping { .. } => RuntimeClass::Stopping,
        _ => RuntimeClass::Idle,
    }
}

fn run_state_kind(run: &PersistedRun) -> Option<RunStateKind> {
    match run {
        PersistedRun::NeverRun => Some(RunStateKind::NeverRun),
        PersistedRun::Starting { .. } => Some(RunStateKind::Starting),
        PersistedRun::Running { .. } => Some(RunStateKind::Running),
        PersistedRun::Stopping {
            reason: StopReason::User,
            ..
        } => Some(RunStateKind::StoppingUser),
        PersistedRun::Stopping {
            reason: StopReason::Shutdown,
            ..
        } => Some(RunStateKind::StoppingShutdown),
        PersistedRun::Idle {
            outcome: AttemptClass::Succeeded,
            ..
        } => Some(RunStateKind::Succeeded),
        PersistedRun::Idle {
            outcome: AttemptClass::Failed,
            ..
        } => Some(RunStateKind::Failed),
        PersistedRun::Idle {
            outcome: AttemptClass::Cancelled,
            ..
        } => Some(RunStateKind::Cancelled),
        PersistedRun::Idle {
            outcome: AttemptClass::Interrupted,
            ..
        } => Some(RunStateKind::Interrupted),
        PersistedRun::Idle { .. } | PersistedRun::Invalid => None,
    }
}

fn classify_job(
    deleting: bool,
    paused: bool,
    run: &PersistedRun,
    interval_secs: i64,
    observed_at: i64,
) -> JobProjection {
    let registration = if deleting {
        RegistrationClass::Deleting
    } else {
        RegistrationClass::Active
    };
    let schedule = if paused {
        ScheduleClass::Paused
    } else {
        ScheduleClass::Enabled
    };
    let attempt = attempt_class(run);
    let runtime = runtime_class(run);

    if registration == RegistrationClass::Deleting {
        return JobProjection {
            registration,
            schedule,
            attempt,
            runtime,
            display: DisplayClass::Deleting,
            next_due: None,
            automatic_start: false,
        };
    }

    if runtime != RuntimeClass::Idle {
        return JobProjection {
            registration,
            schedule,
            attempt,
            runtime,
            display: match runtime {
                RuntimeClass::Starting => DisplayClass::Starting,
                RuntimeClass::Running => DisplayClass::Running,
                RuntimeClass::Stopping => DisplayClass::Stopping,
                RuntimeClass::Idle => unreachable!("non-idle branch projected idle runtime"),
            },
            next_due: None,
            automatic_start: false,
        };
    }

    let (display, mut next_due, mut automatic_start) = match attempt {
        AttemptClass::NeverRun => (DisplayClass::Pending, Some(observed_at), true),
        AttemptClass::Active | AttemptClass::Failed | AttemptClass::Invalid => {
            (DisplayClass::Error, None, false)
        }
        AttemptClass::Cancelled => (DisplayClass::Cancelled, None, false),
        AttemptClass::Interrupted => (DisplayClass::Interrupted, None, false),
        AttemptClass::Succeeded => {
            let PersistedRun::Idle { ended_at, .. } = run else {
                unreachable!("successful attempt is always an idle durable run")
            };
            let due = ended_at.saturating_add(interval_secs.max(0));
            if due <= observed_at {
                (DisplayClass::Pending, Some(due), true)
            } else {
                (DisplayClass::Completed, Some(due), false)
            }
        }
    };
    if schedule == ScheduleClass::Paused {
        next_due = None;
        automatic_start = false;
    }
    JobProjection {
        registration,
        schedule,
        attempt,
        runtime,
        display,
        next_due,
        automatic_start,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunStateKind {
    NeverRun,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    Starting,
    Running,
    StoppingUser,
    StoppingShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunEvent {
    ExplicitStart,
    ScheduledStart,
    SpawnSucceeded,
    SpawnFailed,
    Cancel,
    ExitZero,
    ExitFailure,
    Shutdown,
    Restart,
    PauseChanged,
}

fn transition_run(state: RunStateKind, event: RunEvent) -> Result<RunStateKind, &'static str> {
    use RunEvent as E;
    use RunStateKind as S;
    match (state, event) {
        (state, E::PauseChanged) => Ok(state),
        (S::NeverRun | S::Succeeded | S::Failed | S::Cancelled | S::Interrupted, E::ExplicitStart) => {
            Ok(S::Starting)
        }
        (S::NeverRun | S::Succeeded, E::ScheduledStart) => Ok(S::Starting),
        (S::Failed | S::Cancelled | S::Interrupted, E::ScheduledStart) => Ok(state),
        (S::Starting, E::SpawnSucceeded) => Ok(S::Running),
        (S::Starting, E::SpawnFailed) => Ok(S::Failed),
        (S::Starting | S::Running, E::Cancel) => Ok(S::StoppingUser),
        (S::Starting | S::Running, E::Shutdown) => Ok(S::StoppingShutdown),
        (S::Running, E::ExitZero) => Ok(S::Succeeded),
        (S::Running, E::ExitFailure) => Ok(S::Failed),
        (S::StoppingUser, E::ExitZero | E::ExitFailure | E::Restart) => Ok(S::Cancelled),
        (S::StoppingShutdown, E::ExitZero | E::ExitFailure | E::Restart) => Ok(S::Interrupted),
        (S::Starting | S::Running, E::Restart) => Ok(S::Interrupted),
        (
            S::NeverRun | S::Succeeded | S::Failed | S::Cancelled | S::Interrupted,
            E::Restart | E::Shutdown,
        ) => Ok(state),
        _ => Err("event is invalid for the current run state"),
    }
}

/// Jobs whose driver process is live right now.
static RUNNING: Mutex<Option<HashMap<i64, RunningProcess>>> = Mutex::new(None);
static SUPERVISOR_GATE: Mutex<()> = Mutex::new(());
static DELETE_GATE: Mutex<()> = Mutex::new(());
static PROCESS_SNAPSHOT: Mutex<Option<ProcessSnapshot>> = Mutex::new(None);
static SCHEDULER_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static SHUTTING_DOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn running_map<R>(f: impl FnOnce(&mut HashMap<i64, RunningProcess>) -> R) -> R {
    let mut g = RUNNING.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

struct ProcessSnapshot {
    captured: std::time::Instant,
    children: HashMap<u32, Vec<u32>>,
    usage: HashMap<u32, (f64, u64)>,
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
    /// Derived: starting | running | stopping | deleting | pending |
    /// cancelled | interrupted | error | completed.
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
    /// Engine-only scheduler decision from the same closed projection that
    /// produced `state` and `next_due`; deliberately omitted from the wire.
    #[serde(skip)]
    automatic_start: bool,
    #[serde(skip)]
    delete_mode: Option<String>,
    #[serde(skip)]
    persisted_run: PersistedRun,
}

impl Job {
    pub fn is_live(&self) -> bool {
        matches!(self.state.as_str(), "starting" | "running" | "stopping")
    }
}

#[derive(Debug, Clone)]
pub struct LibraryJob {
    pub kind: String,
    pub src: String,
    pub dest: String,
}

/// The archive gateway's deliberately small inventory read. Serving a page
/// must not trigger even the bounded UI projection or its optional live
/// process telemetry.
pub fn library_jobs() -> Result<Vec<LibraryJob>, String> {
    let conn = db()?;
    let mut statement = conn
        .prepare("SELECT kind,src,dest FROM jobs WHERE delete_mode IS NULL ORDER BY id")
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
    // Lifecycle projection is pure and constant-time per row. Everything
    // below it is optional UI telemetry: no corpus scan, build inspection, or
    // process-tree sample is allowed to influence the projected state.
    let projection = classify_job(
        j.delete_mode.is_some(),
        j.paused,
        &j.persisted_run,
        j.interval_secs,
        now(),
    );
    j.state = projection.display.as_str().into();
    j.next_due = projection.next_due;
    j.automatic_start = projection.automatic_start;
    let run_id = match &j.persisted_run {
        PersistedRun::Starting { id, .. }
        | PersistedRun::Running { id, .. }
        | PersistedRun::Stopping { id, .. }
        | PersistedRun::Idle { id, .. } => Some(*id),
        PersistedRun::NeverRun | PersistedRun::Invalid => None,
    };
    j.pid = running_map(|owners| {
        owners.get(&j.id).and_then(|owner| {
            (Some(owner.run_id) == run_id)
                .then_some(owner.process_group)
                .flatten()
        })
    });
    if let Some(pid) = j.pid {
        if let Some((cpu, rss)) = process_tree_metrics(pid) {
            j.process_cpu_percent = Some(cpu);
            j.process_rss_bytes = Some(rss);
        }
    }
    let destination = std::path::Path::new(&j.dest);
    // Mirror and scratch sizes require typed generation/build accounting.
    // Recursive directory walks are intentionally forbidden here: this
    // listing is refreshed interactively and must remain O(job count).
    j.mirror_bytes = None;
    j.scratch_bytes = None;
    j.available_bytes = destination.parent().and_then(available_bytes);
    // Wikipedia workers project every source/target/assembly observation into
    // one bounded, plan-bound file. This read opens only that file and retains
    // one row per fixed source/target slot; it never inspects build receipts,
    // partial directories, or archive data. The lifecycle projection above
    // remains authoritative even when this telemetry is stale.
    if j.kind == "wiki" {
        let progress = run_id
            .map(|run_id| {
                wikimak_wikipedia::mirror_build_progress_for_run(
                    destination,
                    &run_id.0.to_string(),
                )
            })
            .unwrap_or_else(|| wikimak_wikipedia::mirror_build_progress(destination));
        if let Some(progress) = progress {
            apply_wikipedia_progress(&mut j, progress);
        }
    }
    j
}

fn apply_wikipedia_progress(
    job: &mut Job,
    progress: wikimak_wikipedia::MirrorBuildProgress,
) {
    job.build_phase = Some(progress.phase);
    job.build_snapshot = Some(progress.snapshot);
    job.targets_total = Some(progress.targets_total);
    job.targets_completed = Some(progress.targets_completed);
    job.targets_active = progress.targets_active;
    job.target_progress = progress.target_progress;
    job.source_bytes_total = Some(progress.source_bytes_total);
    job.source_bytes_completed = Some(progress.source_bytes_completed);
    job.fetch_attempts = Some(progress.fetch_attempts);
    job.fetch_bytes_received = Some(progress.fetch_bytes_received);
    job.fetch_rate_limit_responses = Some(progress.fetch_rate_limit_responses);
    job.fetch_client_error_responses = Some(progress.fetch_client_error_responses);
    job.fetch_server_error_responses = Some(progress.fetch_server_error_responses);
    job.fetch_transport_errors = Some(progress.fetch_transport_errors);
    if job.is_live() {
        job.active_source_bytes_per_second = progress.active_source_bytes_per_second;
        job.active_quiet_seconds = progress.active_quiet_seconds;
    } else {
        job.targets_active.clear();
    }
}

/// Sum the importer and its descendants. A wiki worker is a small process tree
/// (driver → Kati/brush build nodes → curl), so reporting only the driver would
/// make an apparently idle job hide the actual transfer/decompression cost.
/// `ps` is used here because it is available on macOS and Linux and avoids
/// making the UI depend on a platform-specific procfs layout.
fn process_tree_metrics(root: u32) -> Option<(f64, u64)> {
    refresh_process_snapshot()?;
    let snapshot = PROCESS_SNAPSHOT.lock().unwrap();
    let snapshot = snapshot.as_ref()?;
    Some(aggregate_process_tree(root, snapshot))
}

fn aggregate_process_tree(root: u32, snapshot: &ProcessSnapshot) -> (f64, u64) {
    let mut members = std::collections::HashSet::from([root]);
    let mut pending = std::collections::VecDeque::from([root]);
    while let Some(parent) = pending.pop_front() {
        for child in snapshot.children.get(&parent).into_iter().flatten() {
            if members.insert(*child) {
                pending.push_back(*child);
            }
        }
    }
    let mut cpu = 0.0;
    let mut rss = 0_u64;
    for member in members {
        if let Some((value, rss_kib)) = snapshot.usage.get(&member) {
            cpu += *value;
            rss = rss.saturating_add(rss_kib.saturating_mul(1024));
        }
    }
    (cpu, rss)
}

fn refresh_process_snapshot() -> Option<()> {
    // Hold the gate while sampling. Concurrent UI/socket readers must share
    // one system-wide `ps` invocation rather than each starting its own.
    let mut snapshot = PROCESS_SNAPSHOT.lock().unwrap();
    if let Some(snapshot) = snapshot.as_ref()
        && snapshot.captured.elapsed() < std::time::Duration::from_secs(1)
    {
        return Some(());
    }
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,%cpu=,rss="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut children = HashMap::<u32, Vec<u32>>::new();
    let mut usage = HashMap::<u32, (f64, u64)>::new();
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
        children.entry(ppid).or_default().push(pid);
        usage.insert(pid, (cpu, rss_kib));
    }
    *snapshot = Some(ProcessSnapshot {
        captured: std::time::Instant::now(),
        children,
        usage,
    });
    Some(())
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
        .prepare(
            "SELECT j.id,j.kind,j.src,j.dest,j.interval_secs,j.paused,j.media_source,
                    j.delete_mode,
                    r.id,r.state,r.started_at,r.ended_at,r.exit_code,
                    COALESCE(r.detail,''),
                    r.process_group,r.stop_reason
             FROM jobs j
             LEFT JOIN runs r ON r.id = (
                 SELECT id FROM runs WHERE job_id = j.id ORDER BY id DESC LIMIT 1
             )
             ORDER BY j.id"
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            let run_id = r.get::<_, Option<i64>>(8)?;
            let run_state = r.get::<_, Option<String>>(9)?;
            let started_at = r.get::<_, Option<i64>>(10)?;
            let ended_at = r.get::<_, Option<i64>>(11)?;
            let exit_code = r.get::<_, Option<i64>>(12)?;
            let process_group = r.get::<_, Option<u32>>(14)?;
            let stop_reason = r.get::<_, Option<String>>(15)?;
            let persisted_run = persisted_run(
                run_id,
                run_state.as_deref(),
                started_at,
                ended_at,
                exit_code,
                process_group,
                stop_reason.as_deref(),
            );
            Ok(Job {
                id: r.get(0)?,
                kind: r.get(1)?,
                src: r.get(2)?,
                dest: r.get(3)?,
                interval_secs: r.get(4)?,
                paused: r.get::<_, i64>(5)? != 0,
                last_start: started_at,
                last_end: ended_at,
                last_exit: exit_code,
                last_detail: r.get(13)?,
                media_source: r.get(6)?,
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
                automatic_start: false,
                delete_mode: r.get(7)?,
                persisted_run,
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
                "starting" | "running" | "stopping" => MirrorState::Running,
                "deleting" => MirrorState::Stopped,
                "pending" => MirrorState::Pending,
                "cancelled" | "interrupted" => MirrorState::Stopped,
                "error" => MirrorState::Error,
                "completed" => MirrorState::Completed,
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
        .optional()
        .map_err(|error| error.to_string())?;
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
            "UPDATE jobs SET media_source = ?2
             WHERE id = ?1 AND kind = 'wiki' AND delete_mode IS NULL",
            params![id, source],
        )
        .map_err(|error| error.to_string())?;
    if changed == 0 {
        return Err(format!("Wikipedia mirror job #{id} does not exist"));
    }
    Ok(())
}

fn prepare_job_deletion(
    id: i64,
    required_kind: Option<&str>,
    mode: DeleteMode,
) -> Result<(String, String), String> {
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let row = transaction
        .query_row(
            "SELECT kind,dest,delete_mode FROM jobs WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no such job".to_string())?;
    if required_kind.is_some_and(|required| row.0 != required) {
        return Err(format!(
            "deleting mirrored files is currently restricted to {} jobs",
            required_kind.unwrap()
        ));
    }
    if required_kind == Some("wiki")
        && std::path::Path::new(&row.1)
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("swdump")
    {
        return Err(format!(
            "refusing to delete unexpected Wikipedia destination {}",
            row.1
        ));
    }
    let active: bool = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM runs WHERE job_id = ?1
                 AND state IN ('starting','running','stopping')
             )",
            [id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if active {
        return Err("job is running; stop it first".into());
    }
    let current = match row.2.as_deref() {
        None => RegistrationState::Active,
        Some("registration") => RegistrationState::Deleting(DeleteMode::Registration),
        Some("data") => RegistrationState::Deleting(DeleteMode::Data),
        Some(_) => return Err("invalid durable job deletion mode".into()),
    };
    let next = transition_registration(current, RegistrationEvent::BeginDelete(mode))
        .map_err(str::to_string)?;
    match (current, next) {
        (RegistrationState::Active, RegistrationState::Deleting(_)) => {
            let changed = transaction
                .execute(
                    "UPDATE jobs SET delete_mode=?2 WHERE id=?1 AND delete_mode IS NULL",
                    params![id, mode.as_str()],
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("delete preparation lost destination ownership".into());
            }
        }
        (RegistrationState::Deleting(_), RegistrationState::Deleting(_)) => {}
        _ => return Err("delete preparation produced an invalid state".into()),
    }
    transaction.commit().map_err(|error| error.to_string())?;
    running_map(|owners| {
        owners.remove(&id);
    });
    Ok((row.0, row.1))
}

fn finish_job_deletion(id: i64, mode: DeleteMode) -> Result<(), String> {
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current = transaction
        .query_row(
            "SELECT delete_mode FROM jobs WHERE id=?1",
            [id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "job deletion ownership was lost".to_string())?;
    let current = match current.as_deref() {
        Some("registration") => RegistrationState::Deleting(DeleteMode::Registration),
        Some("data") => RegistrationState::Deleting(DeleteMode::Data),
        Some(_) => return Err("invalid durable job deletion mode".into()),
        None => RegistrationState::Active,
    };
    if transition_registration(current, RegistrationEvent::FinishDelete(mode))
        .map_err(str::to_string)?
        != RegistrationState::Removed
    {
        return Err("delete completion produced an invalid state".into());
    }
    let changed = transaction
        .execute(
            "DELETE FROM jobs WHERE id=?1 AND delete_mode=?2",
            params![id, mode.as_str()],
        )
        .map_err(|error| error.to_string())?;
    if changed == 1 {
        transaction.commit().map_err(|error| error.to_string())
    } else {
        Err("job deletion ownership was lost".into())
    }
}

/// Remove a job. Returns a human note describing what happened to the
/// job's on-disk state. For git jobs the `<dest>/repo.git` fetch buffer
/// (plus any `repo.git.new` scratch) is dropped: it is DERIVED — the
/// mirror loop reconstructs it from the store via SHA-exact export — and
/// with no schedule left it is ownerless cache. `<dest>/store` is the
/// authoritative corpus (live box attachments may reference it) and is
/// NEVER touched here; deleting it stays an explicit manual act.
/// The destination remains durably claimed while cleanup runs. A cleanup
/// error leaves the job in `deleting`, so retrying this same operation resumes
/// cleanup and another job cannot claim paths still owned by it.
pub fn job_remove(id: i64) -> Result<String, String> {
    let _gate = DELETE_GATE.lock().unwrap();
    let (kind, dest) = prepare_job_deletion(id, None, DeleteMode::Registration)?;
    if kind != "git" {
        // wiki/ietf/cmd keep no separate fetch buffer.
        finish_job_deletion(id, DeleteMode::Registration)?;
        return Ok(String::new());
    }
    for name in ["repo.git", "repo.git.new"] {
        let p = std::path::Path::new(&dest).join(name);
        remove_mirror_path(&p)?;
    }
    finish_job_deletion(id, DeleteMode::Registration)?;
    Ok(format!("fetch buffer dropped; store kept at {dest}/store"))
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

/// Durably claim deletion of an idle Wikipedia job, remove its archive,
/// index, media, destination scratch, and sidecars, then release the
/// destination registration. A failed or interrupted cleanup remains
/// resumable and cannot race a new owner of the same destination.
pub fn job_remove_with_data(id: i64) -> Result<String, String> {
    let _gate = DELETE_GATE.lock().unwrap();
    let (_, destination) = prepare_job_deletion(id, Some("wiki"), DeleteMode::Data)?;
    let archive = std::path::PathBuf::from(&destination);
    let titles = archive.with_extension("swtitle");
    let media = archive.with_extension("media");
    remove_mirror_path(&archive)?;
    remove_mirror_path(&titles)?;
    remove_mirror_path(&media)?;
    for path in wikimak_wikipedia::mirror_auxiliary_paths(&archive)? {
        remove_mirror_path(&path)?;
    }
    finish_job_deletion(id, DeleteMode::Data)?;
    Ok(format!(
        "archive, title index, media cache, and scratch removed from {}",
        archive.display()
    ))
}

pub fn job_set_paused(id: i64, paused: bool) -> Result<(), String> {
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current = transaction
        .query_row(
            "SELECT state,stop_reason FROM runs
             WHERE job_id=?1 ORDER BY id DESC LIMIT 1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?
        .map(|(state, reason)| {
            sql_state_kind(&state, reason.as_deref())
                .ok_or_else(|| format!("invalid durable mirror run state {state:?}"))
        })
        .transpose()?
        .unwrap_or(RunStateKind::NeverRun);
    if transition_run(current, RunEvent::PauseChanged).map_err(str::to_string)? != current {
        return Err("pause unexpectedly changed the run state".into());
    }
    let n = transaction
        .execute(
            "UPDATE jobs SET paused = ?2 WHERE id = ?1 AND delete_mode IS NULL",
            params![id, paused as i64],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        Err("no such job".into())
    } else {
        transaction.commit().map_err(|error| error.to_string())
    }
}

/// Force-run one job NOW (also works on paused jobs — force is force).
/// Returns immediately; the run is a background thread + child process.
pub fn job_run(id: i64) -> Result<(), String> {
    start_job(id, StartRequest::Explicit(WikiRun::Maintain))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Stop a live mirror driver and every transfer/decompressor process it
/// spawned. The driver is a process-group leader, so one signal covers curl
/// and any other descendants without relying on process-tree polling.
pub fn job_cancel(id: i64) -> Result<(), String> {
    let (run_id, process_group) = request_stop(id, StopReason::User)?;
    if let Some(process_group) = process_group {
        if let Err(error) = signal_owned(id, run_id, process_group, libc::SIGTERM)
            && error != "run no longer owns that process group"
        {
            return Err(error);
        }
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let _ = signal_owned(id, run_id, process_group, libc::SIGKILL);
        });
    }
    Ok(())
}

/// Stop all mirror drivers before the engine exits. This is synchronous
/// because delayed escalation threads disappear with the engine process.
pub fn stop_all() {
    SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::Release);
    let _gate = SUPERVISOR_GATE.lock().unwrap();
    let ids = running_map(|owners| owners.keys().copied().collect::<Vec<_>>());
    let mut stopping = Vec::new();
    for id in ids {
        match request_stop(id, StopReason::Shutdown) {
            Ok((run_id, process_group)) => {
                if let Some(process_group) = process_group
                    && let Err(error) =
                        signal_owned(id, run_id, process_group, libc::SIGTERM)
                    && error != "run no longer owns that process group"
                {
                    eprintln!(
                        "mirror job #{id} run #{} shutdown signal failed: {error}",
                        run_id.0
                    );
                }
                stopping.push((id, run_id, process_group));
            }
            Err(error) => {
                eprintln!("mirror job #{id} shutdown transition failed: {error}");
            }
        }
    }
    if !stopping.is_empty() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        for (id, run_id, process_group) in &stopping {
            if let Some(process_group) = process_group
                && let Err(error) =
                    signal_owned(*id, *run_id, *process_group, libc::SIGKILL)
                && error != "run no longer owns that process group"
            {
                eprintln!(
                    "mirror job #{id} run #{} shutdown escalation failed: {error}",
                    run_id.0
                );
            }
        }
        for (id, run_id, _) in stopping {
            if let Err(error) = finish_stopped_run(run_id, "engine shutdown")
                && error != "stale stop completion"
            {
                eprintln!(
                    "mirror job #{id} run #{} shutdown completion failed: {error}",
                    run_id.0
                );
            }
            remove_owner(id, run_id);
        }
    }
    // Covers the narrow durable-start-before-owner-registration window and
    // any owner whose local bookkeeping failed. Explicit user stops become
    // Cancelled; every other active run becomes Interrupted.
    if let Err(error) = recover_unowned_runs() {
        eprintln!("mirror supervisor shutdown recovery failed: {error}");
    }
}

/// Explicitly re-ingest the newest full Wikipedia snapshot. This is never
/// scheduled: routine wiki jobs consume daily adds/changes through `fetch`.
pub fn job_run_full(id: i64) -> Result<(), String> {
    start_job(id, StartRequest::Explicit(WikiRun::RefreshContent))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Start every genuinely pending, unpaused, idle job. Interrupted, failed, and
/// cancelled attempts remain visible until an explicit run supersedes them.
/// Returns the started ids.
pub fn run_pending() -> Result<Vec<i64>, String> {
    let mut started = Vec::new();
    let jobs = scheduler_jobs()?;
    // Full-history Wikimedia parts are large. Keep at most one automatic
    // wiki transfer active; other mirror kinds remain independent. A user
    // can still force-run a particular job explicitly.
    let mut wiki_running = jobs
        .iter()
        .any(|job| job.kind == "wiki" && job.live);
    for j in jobs {
        if j.automatic_start {
            if j.kind == "wiki" && wiki_running {
                continue;
            }
            match start_job(j.id, StartRequest::Scheduled) {
                Ok(_) => {
                    if j.kind == "wiki" {
                        wiki_running = true;
                    }
                    started.push(j.id);
                }
                Err(StartError::Unavailable(_)) => {}
                Err(StartError::Fatal(error)) => return Err(error),
            }
        }
    }
    Ok(started)
}

struct SchedulerJob {
    id: i64,
    kind: String,
    live: bool,
    automatic_start: bool,
}

/// Scheduler projection is one indexed row lookup per job. It deliberately
/// excludes path measurement, build-tree inspection, process-tree polling,
/// and every other UI telemetry source.
fn scheduler_jobs() -> Result<Vec<SchedulerJob>, String> {
    let conn = db()?;
    let observed_at = now();
    let mut statement = conn
        .prepare(
            "SELECT j.id,j.kind,j.interval_secs,j.paused,j.delete_mode,
                    r.id,r.state,r.started_at,r.ended_at,r.exit_code,
                    r.process_group,r.stop_reason
             FROM jobs j
             LEFT JOIN runs r ON r.id = (
                 SELECT id FROM runs WHERE job_id=j.id ORDER BY id DESC LIMIT 1
             )
             ORDER BY j.id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let id = row.get(0)?;
            let kind = row.get(1)?;
            let interval_secs = row.get::<_, i64>(2)?;
            let paused = row.get::<_, i64>(3)? != 0;
            let deleting = row.get::<_, Option<String>>(4)?.is_some();
            let run_id = row.get::<_, Option<i64>>(5)?;
            let state = row.get::<_, Option<String>>(6)?;
            let started_at = row.get::<_, Option<i64>>(7)?;
            let ended_at = row.get::<_, Option<i64>>(8)?;
            let exit_code = row.get::<_, Option<i64>>(9)?;
            let process_group = row.get::<_, Option<u32>>(10)?;
            let stop_reason = row.get::<_, Option<String>>(11)?;
            let run = persisted_run(
                run_id,
                state.as_deref(),
                started_at,
                ended_at,
                exit_code,
                process_group,
                stop_reason.as_deref(),
            );
            let projection =
                classify_job(deleting, paused, &run, interval_secs, observed_at);
            Ok(SchedulerJob {
                id,
                kind,
                live: projection.runtime != RuntimeClass::Idle,
                automatic_start: projection.automatic_start,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
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

#[derive(Clone, Copy)]
enum StartRequest {
    Explicit(WikiRun),
    Scheduled,
}

#[derive(Debug)]
enum StartError {
    Unavailable(String),
    Fatal(String),
}

impl From<String> for StartError {
    fn from(error: String) -> Self {
        Self::Fatal(error)
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) | Self::Fatal(error) => formatter.write_str(error),
        }
    }
}

impl StartRequest {
    fn request_name(self) -> &'static str {
        match self {
            Self::Explicit(WikiRun::Maintain) => "explicit",
            Self::Explicit(WikiRun::RefreshContent) => "full",
            Self::Scheduled => "scheduled",
        }
    }

    fn event(self) -> RunEvent {
        match self {
            Self::Explicit(_) => RunEvent::ExplicitStart,
            Self::Scheduled => RunEvent::ScheduledStart,
        }
    }

    fn wiki_run(self) -> WikiRun {
        match self {
            Self::Explicit(run) => run,
            Self::Scheduled => WikiRun::Maintain,
        }
    }
}

#[derive(Clone)]
struct JobConfig {
    id: i64,
    kind: String,
    src: String,
    dest: String,
    interval_secs: i64,
    paused: bool,
    media_source: Option<String>,
    delete_mode: Option<String>,
}

fn sql_state_kind(state: &str, stop_reason: Option<&str>) -> Option<RunStateKind> {
    match (state, stop_reason) {
        ("succeeded", _) => Some(RunStateKind::Succeeded),
        ("failed", _) => Some(RunStateKind::Failed),
        ("cancelled", _) => Some(RunStateKind::Cancelled),
        ("interrupted", _) => Some(RunStateKind::Interrupted),
        ("starting", _) => Some(RunStateKind::Starting),
        ("running", _) => Some(RunStateKind::Running),
        ("stopping", Some("user")) => Some(RunStateKind::StoppingUser),
        ("stopping", Some("shutdown")) => Some(RunStateKind::StoppingShutdown),
        _ => None,
    }
}

fn start_job(id: i64, request: StartRequest) -> Result<RunId, StartError> {
    let _gate = SUPERVISOR_GATE.lock().unwrap();
    if SHUTTING_DOWN.load(std::sync::atomic::Ordering::Acquire) {
        return Err(StartError::Unavailable(
            "mirror supervisor is shutting down".into(),
        ));
    }
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let job = transaction
        .query_row(
            "SELECT id,kind,src,dest,interval_secs,paused,media_source,delete_mode
             FROM jobs WHERE id = ?1",
            [id],
            |row| {
                Ok(JobConfig {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    src: row.get(2)?,
                    dest: row.get(3)?,
                    interval_secs: row.get(4)?,
                    paused: row.get::<_, i64>(5)? != 0,
                    media_source: row.get(6)?,
                    delete_mode: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| StartError::Unavailable("no such job".into()))?;
    if job.delete_mode.is_some() {
        return Err(StartError::Unavailable("job deletion is in progress".into()));
    }
    if matches!(
        request,
        StartRequest::Explicit(WikiRun::RefreshContent)
    ) && job.kind != "wiki"
    {
        return Err(StartError::Unavailable(
            "full snapshot re-ingest is only available for wiki mirrors".into(),
        ));
    }
    let latest = transaction
        .query_row(
            "SELECT id,state,started_at,ended_at,exit_code,process_group,stop_reason FROM runs
             WHERE job_id = ?1 ORDER BY id DESC LIMIT 1",
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<u32>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let current_run = match latest {
        Some((run_id, state, started_at, ended_at, exit_code, process_group, reason)) => {
            persisted_run(
                Some(run_id),
                Some(&state),
                started_at,
                ended_at,
                exit_code,
                process_group,
                reason.as_deref(),
            )
        }
        None => PersistedRun::NeverRun,
    };
    let current = run_state_kind(&current_run)
        .ok_or_else(|| format!("invalid durable mirror run state {current_run:?}"))?;
    let next = transition_run(current, request.event())
        .map_err(|error| StartError::Unavailable(error.into()))?;
    if next != RunStateKind::Starting {
        return Err(StartError::Unavailable(
            "job is not eligible for an automatic run".into(),
        ));
    }
    if matches!(request, StartRequest::Scheduled) {
        if job.paused {
            return Err(StartError::Unavailable("job is paused".into()));
        }
        if current == RunStateKind::Succeeded {
            let PersistedRun::Idle { ended_at, .. } = current_run else {
                return Err(StartError::Fatal(
                    "successful run is not durably idle".into(),
                ));
            };
            let due = ended_at.saturating_add(job.interval_secs.max(0));
            if due > now() {
                return Err(StartError::Unavailable("job is not due".into()));
            }
        }
    }
    transaction
        .execute(
            "INSERT INTO runs(job_id,request,state,started_at)
             VALUES(?1,?2,'starting',?3)",
            params![id, request.request_name(), now()],
        )
        .map_err(|error| {
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
                StartError::Unavailable("job is already running".into())
            } else {
                StartError::Fatal(error.to_string())
            }
        })?;
    let run_id = RunId(transaction.last_insert_rowid());
    transaction.commit().map_err(|error| error.to_string())?;
    running_map(|owners| {
        owners.insert(
            id,
            RunningProcess {
                run_id,
                process_group: None,
                stop_reason: None,
            },
        );
    });
    spawn_run(job, request.wiki_run(), run_id);
    Ok(run_id)
}

fn request_stop(id: i64, requested: StopReason) -> Result<(RunId, Option<u32>), String> {
    // One immediate transaction selects and advances exactly one RunId.
    // Cancellation never enumerates descendants: at most one owned process
    // group is signalled after the durable transition commits.
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let (run_id, state, current_reason, process_group) = transaction
        .query_row(
            "SELECT id,state,stop_reason,process_group FROM runs
             WHERE job_id = ?1 AND state IN ('starting','running','stopping')
             ORDER BY id DESC LIMIT 1",
            [id],
            |row| {
                Ok((
                    RunId(row.get(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "job is not running".to_string())?;
    let current = sql_state_kind(&state, current_reason.as_deref())
        .ok_or_else(|| format!("invalid durable mirror run state {state:?}"))?;
    if current == RunStateKind::Running && process_group.is_none() {
        return Err("durable running mirror has no process group".into());
    }
    let effective_reason = match current {
        RunStateKind::StoppingUser => StopReason::User,
        RunStateKind::StoppingShutdown => StopReason::Shutdown,
        _ => {
            let expected = match requested {
                StopReason::User => RunStateKind::StoppingUser,
                StopReason::Shutdown => RunStateKind::StoppingShutdown,
            };
            let event = match requested {
                StopReason::User => RunEvent::Cancel,
                StopReason::Shutdown => RunEvent::Shutdown,
            };
            let next = transition_run(current, event).map_err(str::to_string)?;
            if next != expected {
                return Err("stop transition produced an invalid state".into());
            }
            let changed = transaction
                .execute(
                    "UPDATE runs SET state='stopping',stop_reason=?2
                     WHERE id=?1 AND state IN ('starting','running')",
                    params![run_id.0, requested.as_str()],
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("stop transition lost its run ownership".into());
            }
            requested
        }
    };
    transaction.commit().map_err(|error| error.to_string())?;
    running_map(|owners| {
        if let Some(owner) = owners.get_mut(&id)
            && owner.run_id == run_id
        {
            owner.stop_reason = Some(effective_reason);
        }
    });
    Ok((run_id, process_group))
}

fn signal_owned(
    id: i64,
    run_id: RunId,
    process_group: u32,
    signal: i32,
) -> Result<(), String> {
    let owned = running_map(|owners| {
        owners.get(&id).and_then(|owner| {
            (owner.run_id == run_id && owner.stop_reason.is_some())
                .then_some(owner.process_group)
        })
    });
    match owned {
        Some(Some(owned_group)) if owned_group == process_group => {}
        // Spawn committed the group durably just before attaching it to the
        // local owner. The spawn thread observes stop_reason and signals it.
        Some(None) => return Ok(()),
        _ => return Err("run no longer owns that process group".into()),
    }
    signal_process_group(process_group, signal)
}

/// Signal a group whose ownership was just committed durably by
/// `record_spawned`.  This path intentionally does not consult RUNNING: a
/// concurrent shutdown may already have removed the in-memory owner while the
/// spawn thread is still attaching the newly created group.
fn signal_process_group(process_group: u32, signal: i32) -> Result<(), String> {
    let group =
        i32::try_from(process_group).map_err(|_| "mirror process group exceeds i32")?;
    if unsafe { libc::kill(-group, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("signal mirror process group {process_group}: {error}"));
        }
    }
    Ok(())
}

fn durable_stop_reason(run_id: RunId) -> Result<Option<StopReason>, String> {
    let conn = db()?;
    let (state, reason) = conn
        .query_row(
            "SELECT state,stop_reason FROM runs WHERE id=?1",
            [run_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .map_err(|error| error.to_string())?;
    match (state.as_str(), reason.as_deref()) {
        ("starting" | "running", None) => Ok(None),
        ("stopping", Some("user")) => Ok(Some(StopReason::User)),
        ("stopping", Some("shutdown")) => Ok(Some(StopReason::Shutdown)),
        _ => Err("run is no longer spawnable".into()),
    }
}

/// End a run whose driver has not been spawned when the pre-spawn ownership
/// check itself fails.  This deliberately uses one conditional UPDATE rather
/// than first reading the row: the check that failed is the database read, so
/// another read cannot be a prerequisite for clearing the durable `starting`
/// state.  A stopping run keeps its requested outcome; an un-stopped run is a
/// failed attempt with a synthetic non-zero exit code.
fn finish_pre_spawn_failure(run_id: RunId, detail: &str) -> Result<(), String> {
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let changed = transaction
        .execute(
            "UPDATE runs SET
                 state = CASE
                     WHEN state='stopping' AND stop_reason='user' THEN 'cancelled'
                     WHEN state='stopping' AND stop_reason='shutdown' THEN 'interrupted'
                     ELSE 'failed'
                 END,
                 ended_at = ?2,
                 exit_code = CASE
                     WHEN state='stopping' AND stop_reason IN ('user','shutdown')
                         THEN NULL
                     ELSE -1
                 END,
                 stop_reason = CASE
                     WHEN state='stopping' AND stop_reason IN ('user','shutdown')
                         THEN stop_reason
                     ELSE NULL
                 END,
                 detail = ?3
             WHERE id=?1
               AND state IN ('starting','running','stopping')
               AND NOT EXISTS(
                   SELECT 1 FROM runs newer
                   WHERE newer.job_id=runs.job_id AND newer.id>runs.id
               )",
            params![run_id.0, now(), detail],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("stale pre-spawn failure lost its run ownership".into());
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn record_spawned(run_id: RunId, process_group: u32) -> Result<Option<StopReason>, String> {
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let (state, reason) = transaction
        .query_row(
            "SELECT state,stop_reason FROM runs WHERE id=?1",
            [run_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .map_err(|error| error.to_string())?;
    let stop_reason = match (state.as_str(), reason.as_deref()) {
        ("starting", None) => {
            if transition_run(RunStateKind::Starting, RunEvent::SpawnSucceeded)
                .map_err(str::to_string)?
                != RunStateKind::Running
            {
                return Err("spawn transition produced an invalid state".into());
            }
            let changed = transaction
                .execute(
                    "UPDATE runs SET state='running',spawned_at=?2,process_group=?3
                     WHERE id=?1 AND state='starting'",
                    params![run_id.0, now(), process_group],
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("spawn transition lost its run ownership".into());
            }
            None
        }
        ("stopping", Some("user")) => {
            let changed = transaction
                .execute(
                    "UPDATE runs SET spawned_at=?2,process_group=?3
                     WHERE id=?1 AND state='stopping' AND stop_reason='user'",
                    params![run_id.0, now(), process_group],
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("stopped spawn lost its run ownership".into());
            }
            Some(StopReason::User)
        }
        ("stopping", Some("shutdown")) => {
            let changed = transaction
                .execute(
                    "UPDATE runs SET spawned_at=?2,process_group=?3
                     WHERE id=?1 AND state='stopping' AND stop_reason='shutdown'",
                    params![run_id.0, now(), process_group],
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("stopped spawn lost its run ownership".into());
            }
            Some(StopReason::Shutdown)
        }
        _ => return Err("stale spawn completion for inactive run".into()),
    };
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(stop_reason)
}

fn finish_child_run(run_id: RunId, exit: i64, detail: &str) -> Result<(), String> {
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let (state, reason) = transaction
        .query_row(
            "SELECT state,stop_reason FROM runs
             WHERE id=?1 AND NOT EXISTS(
                 SELECT 1 FROM runs newer
                 WHERE newer.job_id=runs.job_id AND newer.id>runs.id
             )",
            [run_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "stale child completion".to_string())?;
    let current = sql_state_kind(&state, reason.as_deref())
        .ok_or_else(|| format!("invalid durable mirror run state {state:?}"))?;
    let (event, next) = match current {
        RunStateKind::Starting => (
            RunEvent::SpawnFailed,
            transition_run(current, RunEvent::SpawnFailed),
        ),
        RunStateKind::Running => {
            let event = if exit == 0 {
                RunEvent::ExitZero
            } else {
                RunEvent::ExitFailure
            };
            (event, transition_run(current, event))
        }
        RunStateKind::StoppingUser | RunStateKind::StoppingShutdown => {
            let event = if exit == 0 {
                RunEvent::ExitZero
            } else {
                RunEvent::ExitFailure
            };
            (event, transition_run(current, event))
        }
        _ => return Err("stale child completion for terminal run".into()),
    };
    let next = next.map_err(str::to_string)?;
    let state_name = match next {
        RunStateKind::Succeeded => "succeeded",
        RunStateKind::Failed => "failed",
        RunStateKind::Cancelled => "cancelled",
        RunStateKind::Interrupted => "interrupted",
        _ => return Err(format!("invalid terminal transition for {event:?}")),
    };
    let terminal_exit = (next != RunStateKind::Interrupted).then_some(exit);
    let changed = transaction
        .execute(
            "UPDATE runs SET state=?2,ended_at=?3,exit_code=?4,detail=?5
             WHERE id=?1 AND state IN ('starting','running','stopping')",
            params![run_id.0, state_name, now(), terminal_exit, detail],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("stale child completion lost its run ownership".into());
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn finish_stopped_run(run_id: RunId, detail: &str) -> Result<(), String> {
    let mut conn = db()?;
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let reason = transaction
        .query_row(
            "SELECT stop_reason FROM runs WHERE id=?1 AND state='stopping'",
            [run_id.0],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "stale stop completion".to_string())?;
    let (current, state_name) = match reason.as_str() {
        "user" => (RunStateKind::StoppingUser, "cancelled"),
        "shutdown" => (RunStateKind::StoppingShutdown, "interrupted"),
        _ => return Err("invalid stop reason".into()),
    };
    let next = transition_run(current, RunEvent::ExitFailure).map_err(str::to_string)?;
    if !matches!(
        next,
        RunStateKind::Cancelled | RunStateKind::Interrupted
    ) {
        return Err("stop completion produced a non-terminal state".into());
    }
    let changed = transaction
        .execute(
            "UPDATE runs SET state=?2,ended_at=?3,exit_code=NULL,detail=?4
             WHERE id=?1 AND state='stopping'",
            params![run_id.0, state_name, now(), detail],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("stop completion lost its run ownership".into());
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn remove_owner(id: i64, run_id: RunId) {
    running_map(|owners| {
        if owners.get(&id).is_some_and(|owner| owner.run_id == run_id) {
            owners.remove(&id);
        }
    });
}

fn spawn_run(job: JobConfig, wiki_run: WikiRun, run_id: RunId) {
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
                "--run-id".into(),
                run_id.0.to_string(),
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
    let wiki_background = if wiki {
        match wikimak_wikipedia::mirror_has_installed_generation(
            std::path::Path::new(&job.dest),
        ) {
            Ok(installed) => installed,
            Err(error) => {
                eprintln!(
                    "mirror job #{} cannot inspect installed Wikipedia generation: {error}",
                    job.id
                );
                false
            }
        }
    } else {
        false
    };
    let wiki_cpu_budget = wiki_background.then(|| {
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .saturating_sub(2)
            .max(1)
    });
    std::thread::spawn(move || {
        match durable_stop_reason(run_id) {
            Ok(Some(_)) => {
                if let Err(error) = finish_stopped_run(run_id, "cancelled before spawn") {
                    eprintln!(
                        "mirror job #{id} run #{} pre-spawn stop completion failed: {error}",
                        run_id.0
                    );
                }
                remove_owner(id, run_id);
                return;
            }
            Ok(None) => {}
            Err(error) => {
                let detail = format!("pre-spawn ownership check failed: {error}");
                if let Err(completion_error) = finish_pre_spawn_failure(run_id, &detail) {
                    eprintln!(
                        "mirror job #{id} run #{} pre-spawn ownership check failed: {error}; \
                         terminalization failed: {completion_error}",
                        run_id.0
                    );
                }
                remove_owner(id, run_id);
                return;
            }
        }
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
                let process_group = c.id();
                let stop_reason = match record_spawned(run_id, process_group) {
                    Ok(reason) => reason,
                    Err(error) => {
                        unsafe {
                            libc::kill(-(process_group as i32), libc::SIGKILL);
                        }
                        let _ = c.wait();
                        let detail = format!("spawn registration failed: {error}");
                        if let Err(completion_error) =
                            finish_child_run(run_id, -1, &detail)
                        {
                            eprintln!(
                                "mirror job #{id} run #{} spawn registration and completion failed: \
                                 {error}; {completion_error}",
                                run_id.0
                            );
                        }
                        remove_owner(id, run_id);
                        return;
                    }
                };
                let effective_stop = running_map(|owners| {
                    if let Some(owner) = owners.get_mut(&id)
                        && owner.run_id == run_id
                    {
                        owner.process_group = Some(process_group);
                        if stop_reason.is_some() {
                            owner.stop_reason = stop_reason;
                        }
                        owner.stop_reason
                    } else {
                        stop_reason
                    }
                });
                if effective_stop.is_some() {
                    if let Err(error) = signal_process_group(process_group, libc::SIGTERM) {
                        eprintln!(
                            "mirror job #{id} run #{} could not signal the stopping group {}: \
                             {error}",
                            run_id.0, process_group
                        );
                    }
                }
                let stderr = c.stderr.take().expect("piped stderr");
                let (exit, tail) = stream_stderr(run_id, stderr, &mut c);
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
        if let Some(process_group) = running_map(|owners| {
            owners.get(&id).and_then(|owner| {
                (owner.run_id == run_id && owner.stop_reason.is_some())
                    .then_some(owner.process_group)
                    .flatten()
            })
        }) {
            let _ = signal_owned(id, run_id, process_group, libc::SIGKILL);
        }
        if let Err(error) = finish_child_run(run_id, exit, &detail) {
            eprintln!(
                "mirror job #{id} run #{} child completion rejected: {error}",
                run_id.0
            );
        }
        remove_owner(id, run_id);
    });
}

/// Read child stderr line-by-line, updating this RunId's detail in the DB every
/// ~2s so the UI's mirror detail pane shows live progress. Returns the
/// collected stderr tail (last 2KB) and the child's exit status.
fn stream_stderr(
    run_id: RunId,
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
                    "UPDATE runs SET detail = ?2
                     WHERE id = ?1 AND state IN ('starting','running','stopping')",
                    params![run_id.0, tail_2k(&tail)],
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
    if SCHEDULER_STARTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    if let Err(error) = recover_unowned_runs() {
        // Recovery is per-run: diagnostics from one malformed or foreign row
        // must not prevent scheduling the other jobs after all rows were
        // terminalized.
        eprintln!("mirror supervisor recovery completed with diagnostics: {error}");
    }
    std::thread::spawn(|| {
        loop {
            if SHUTTING_DOWN.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            if let Err(error) = run_pending() {
                eprintln!("mirror scheduler tick failed: {error}");
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
}

fn process_group_is_live(process_group: u32) -> bool {
    if process_group <= 1 {
        return false;
    }
    let Ok(group) = i32::try_from(process_group) else {
        return false;
    };
    (unsafe { libc::kill(-group, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn reap_owned_children(process_group: i32) {
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(-process_group, &mut status, libc::WNOHANG) };
        if result > 0 {
            continue;
        }
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break;
    }
}

/// Check that a persisted group still looks like the process group created by
/// this run before sending a signal after engine restart.  On Linux the
/// process start time in /proc catches PID/PGID reuse.  Other Unix hosts do
/// not expose an equivalent portable token, so we require the group leader to
/// still be its own group and rely on the driver's parent-death watchdog while
/// the engine is alive; a reused group is rejected when it is obviously
/// unsafe (PID 1 or the engine's own group).
fn recorded_group_is_safe(process_group: u32, spawned_at: Option<i64>) -> Result<bool, String> {
    if process_group <= 1 {
        return Ok(false);
    }
    let group = i32::try_from(process_group)
        .map_err(|_| format!("process group {process_group} exceeds i32"))?;
    let own_group = unsafe { libc::getpgrp() };
    if group == own_group {
        return Ok(false);
    }
    if unsafe { libc::getpgid(group) } != group {
        return Ok(false);
    }

    #[cfg(target_os = "linux")]
    {
        let Some(spawned_at) = spawned_at else {
            return Ok(false);
        };
        let stat_path = format!("/proc/{process_group}/stat");
        let stat = std::fs::read_to_string(&stat_path)
            .map_err(|error| format!("read {stat_path}: {error}"))?;
        let fields = stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.split_whitespace().collect::<Vec<_>>())
            .ok_or_else(|| format!("malformed {stat_path}"))?;
        // After the command name, field 3 is index 0 and field 22
        // (starttime) is index 19. Field 5 (pgrp) is index 2.
        let observed_group = fields
            .get(2)
            .ok_or_else(|| format!("{stat_path} has no process group"))?
            .parse::<i32>()
            .map_err(|_| format!("invalid process group in {stat_path}"))?;
        if observed_group != group {
            return Ok(false);
        }
        let start_ticks = fields
            .get(19)
            .ok_or_else(|| format!("{stat_path} has no start time"))?
            .parse::<u64>()
            .map_err(|_| format!("invalid start time in {stat_path}"))?;
        let boot = std::fs::read_to_string("/proc/stat")
            .map_err(|error| format!("read /proc/stat: {error}"))?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|value| value.trim().parse::<u64>().ok())
            .ok_or_else(|| "Linux /proc/stat has no boot time".to_owned())?;
        let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks_per_second <= 0 {
            return Err("cannot determine Linux clock tick rate".into());
        }
        let start_epoch = boot.saturating_add(start_ticks / ticks_per_second as u64);
        // record_spawned stores seconds after spawn. A group leader can be
        // observed a few seconds before/after that write under scheduler
        // pressure, but a PID reused much later must not be signalled.
        return Ok(start_epoch.saturating_add(5) >= spawned_at as u64
            && (spawned_at.saturating_add(5) as u64) >= start_epoch);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = spawned_at;
        Ok(true)
    }
}

fn terminate_recorded_group(
    process_group: u32,
    spawned_at: Option<i64>,
) -> Result<(), String> {
    // A child that already exited is exactly the state we want to recover;
    // there is no process left to signal or reap.
    if !process_group_is_live(process_group) {
        return Ok(());
    }
    if !recorded_group_is_safe(process_group, spawned_at)? {
        return Err(format!(
            "refusing to signal process group {process_group}: group incarnation is not owned"
        ));
    }
    let group = i32::try_from(process_group)
        .map_err(|_| format!("process group {process_group} exceeds i32"))?;
    let mut term_failed = false;
    if unsafe { libc::kill(-group, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        // A group containing only an already-exiting/zombie member can
        // reject SIGTERM with EPERM even though SIGKILL is still meaningful.
        // Escalate once before declaring recovery incomplete.
        term_failed = true;
    }
    if !term_failed {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // A direct child can remain as a zombie until the supervisor reaps it;
    // kill(-pgid, 0) reports that group as present and SIGKILL then returns
    // EPERM. Reap before deciding that escalation is still necessary.
    reap_owned_children(group);
    if process_group_is_live(process_group) {
        if unsafe { libc::kill(-group, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("kill process group {process_group}: {error}"));
            }
        }
    }
    // If recovery is running in the original parent (as stop_all can call
    // this path), reap direct children before checking group disappearance.
    // After an engine restart waitpid returns ECHILD; the old parent or init
    // owns those processes and the bounded liveness check below still applies.
    reap_owned_children(group);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while process_group_is_live(process_group) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
        reap_owned_children(group);
    }
    if process_group_is_live(process_group) {
        return Err(format!(
            "process group {process_group} remains live after SIGKILL"
        ));
    }
    Ok(())
}

fn recover_unowned_runs() -> Result<(), String> {
    let conn = db()?;
    let mut diagnostics = Vec::new();
    let mut active = {
        let mut statement = conn
            .prepare(
                "SELECT id,state,stop_reason,process_group,spawned_at FROM runs
                 WHERE state IN ('starting','running','stopping')",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    RunId(row.get(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut active = Vec::new();
        for row in rows {
            match row {
                Ok(row) => active.push(row),
                Err(error) => diagnostics.push(format!(
                    "cannot decode an active mirror run during restart recovery: {error}"
                )),
            }
        }
        active
    };
    active.sort_by_key(|(run_id, _, _, _, _)| run_id.0);

    // Signal/reap every recorded group before changing any durable run state.
    // This ordering prevents a restarted engine from publishing an apparently
    // idle job while its old driver is still mutating the destination.
    for (run_id, _state, _reason, process_group, spawned_at) in &active {
        if let Some(process_group) = process_group
            && let Err(error) = terminate_recorded_group(*process_group, *spawned_at)
        {
            diagnostics.push(format!(
                "run #{} process group {}: {error}",
                run_id.0, process_group
            ));
        }
    }
    drop(conn);

    // A malformed row must not prevent later rows from being recovered.  The
    // conditional UPDATE below also handles the narrow case where a valid
    // state has an invalid stop reason: it records a failed outcome rather
    // than leaving an active row that the scheduler can never start.
    for (run_id, state, reason, _process_group, _spawned_at) in active {
        let (state_name, stop_reason, exit_code) = match (state.as_str(), reason.as_deref()) {
            ("stopping", Some("user")) => ("cancelled", Some("user"), None),
            ("stopping", Some("shutdown")) => ("interrupted", Some("shutdown"), None),
            ("starting" | "running", None) => ("interrupted", None, None),
            _ => {
                diagnostics.push(format!(
                    "run #{} has malformed active state {:?}/{:?}",
                    run_id.0, state, reason
                ));
                ("failed", None, Some(-1_i64))
            }
        };
        let detail = if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.starts_with(&format!("run #{} process group", run_id.0)))
        {
            "engine restarted; process-group cleanup reported an error"
        } else {
            "engine restarted before run completion"
        };
        let mut conn = match db() {
            Ok(conn) => conn,
            Err(error) => {
                diagnostics.push(format!("run #{} terminalization: {error}", run_id.0));
                continue;
            }
        };
        let transaction = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(transaction) => transaction,
            Err(error) => {
                diagnostics.push(format!(
                    "run #{} terminalization transaction: {error}",
                    run_id.0
                ));
                continue;
            }
        };
        let changed = match transaction.execute(
            "UPDATE runs SET state=?2,ended_at=?3,exit_code=?4,stop_reason=?5,detail=?6
             WHERE id=?1 AND state=?7
               AND NOT EXISTS(
                   SELECT 1 FROM runs newer
                   WHERE newer.job_id=runs.job_id AND newer.id>runs.id
               )",
            params![
                run_id.0,
                state_name,
                now(),
                exit_code,
                stop_reason,
                detail,
                state,
            ],
        ) {
            Ok(changed) => changed,
            Err(error) => {
                diagnostics.push(format!("run #{} terminalization: {error}", run_id.0));
                continue;
            }
        };
        if changed != 1 {
            diagnostics.push(format!(
                "restart recovery lost ownership of run #{}",
                run_id.0
            ));
            continue;
        }
        if let Err(error) = transaction.commit() {
            diagnostics.push(format!("run #{} terminalization commit: {error}", run_id.0));
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "restart recovery completed with diagnostics: {}",
            diagnostics.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_lifecycle_never_projects_stale_telemetry_as_active() {
        let mut job: Job = serde_json::from_value(serde_json::json!({
            "id": 1,
            "kind": "wiki",
            "src": "enwiki",
            "dest": "/tmp/enwiki.swdump",
            "interval_secs": 86400,
            "paused": true,
            "last_start": 1,
            "last_end": 2,
            "last_exit": 1,
            "last_detail": "failed",
            "state": "error",
            "next_due": null
        }))
        .unwrap();
        apply_wikipedia_progress(
            &mut job,
            wikimak_wikipedia::MirrorBuildProgress {
                phase: "materializing source targets".into(),
                targets_active: vec!["content-000001 · parsing".into()],
                active_source_bytes_per_second: Some(123),
                active_quiet_seconds: Some(4),
                ..Default::default()
            },
        );
        assert_eq!(job.state, "error");
        assert!(job.targets_active.is_empty());
        assert_eq!(job.active_source_bytes_per_second, None);
        assert_eq!(job.active_quiet_seconds, None);
    }

    #[test]
    fn one_process_snapshot_aggregates_each_tree_without_quadratic_membership_scans() {
        let snapshot = ProcessSnapshot {
            captured: std::time::Instant::now(),
            children: HashMap::from([
                (1, vec![2, 5]),
                (2, vec![3, 4]),
                (9, vec![10]),
            ]),
            usage: HashMap::from([
                (1, (1.0, 10)),
                (2, (2.0, 20)),
                (3, (3.0, 30)),
                (4, (4.0, 40)),
                (5, (5.0, 50)),
                (9, (9.0, 90)),
                (10, (10.0, 100)),
            ]),
        };
        assert_eq!(aggregate_process_tree(1, &snapshot), (15.0, 150 * 1024));
        assert_eq!(aggregate_process_tree(9, &snapshot), (19.0, 190 * 1024));
        assert_eq!(aggregate_process_tree(99, &snapshot), (0.0, 0));
    }

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
    fn lifecycle_projection_matrix() {
        struct Case {
            name: &'static str,
            paused: bool,
            run: PersistedRun,
            interval: i64,
            now: i64,
            schedule: ScheduleClass,
            attempt_class: AttemptClass,
            runtime_class: RuntimeClass,
            display: DisplayClass,
            next_due: Option<i64>,
            automatic_start: bool,
        }

        let idle = |outcome, exit_code| PersistedRun::Idle {
            id: RunId(1),
            started_at: 100,
            ended_at: 200,
            outcome,
            exit_code,
        };
        let cases = vec![
            Case {
                name: "never-run enabled job is genuinely pending",
                paused: false,
                run: PersistedRun::NeverRun,
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::NeverRun,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Pending,
                next_due: Some(1_000),
                automatic_start: true,
            },
            Case {
                name: "successful job waits from completion",
                paused: false,
                run: idle(AttemptClass::Succeeded, Some(0)),
                interval: 100,
                now: 299,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Succeeded,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Completed,
                next_due: Some(300),
                automatic_start: false,
            },
            Case {
                name: "successful job becomes due",
                paused: false,
                run: idle(AttemptClass::Succeeded, Some(0)),
                interval: 100,
                now: 300,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Succeeded,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Pending,
                next_due: Some(300),
                automatic_start: true,
            },
            Case {
                name: "long run does not become due based on its start",
                paused: false,
                run: PersistedRun::Idle {
                    id: RunId(1),
                    started_at: 0,
                    ended_at: 1_000,
                    outcome: AttemptClass::Succeeded,
                    exit_code: Some(0),
                },
                interval: 100,
                now: 1_050,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Succeeded,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Completed,
                next_due: Some(1_100),
                automatic_start: false,
            },
            Case {
                name: "restart exposes interrupted attempt without retrying",
                paused: false,
                run: idle(AttemptClass::Interrupted, None),
                interval: 100,
                now: 10_000,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Interrupted,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Interrupted,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "old failed attempt remains an error when time passes",
                paused: false,
                run: idle(AttemptClass::Failed, Some(7)),
                interval: 100,
                now: 10_000,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Failed,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Error,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "cancelled attempt is distinct and is not retried",
                paused: false,
                run: idle(AttemptClass::Cancelled, Some(-1)),
                interval: 100,
                now: 10_000,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Cancelled,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Cancelled,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "pause prevents scheduling without hiding a pending attempt",
                paused: true,
                run: PersistedRun::NeverRun,
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Paused,
                attempt_class: AttemptClass::NeverRun,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Pending,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "pause prevents scheduling without hiding a due attempt",
                paused: true,
                run: idle(AttemptClass::Succeeded, Some(0)),
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Paused,
                attempt_class: AttemptClass::Succeeded,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Pending,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "pause does not hide a failed attempt that needs attention",
                paused: true,
                run: idle(AttemptClass::Failed, Some(7)),
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Paused,
                attempt_class: AttemptClass::Failed,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Error,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "durable starting is visible without process telemetry",
                paused: false,
                run: PersistedRun::Starting {
                    id: RunId(2),
                    started_at: 900,
                },
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Active,
                runtime_class: RuntimeClass::Starting,
                display: DisplayClass::Starting,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "durable running remains live when future runs are paused",
                paused: true,
                run: PersistedRun::Running {
                    id: RunId(2),
                    started_at: 900,
                    process_group: 42,
                },
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Paused,
                attempt_class: AttemptClass::Active,
                runtime_class: RuntimeClass::Running,
                display: DisplayClass::Running,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "stop request is typed as stopping",
                paused: false,
                run: PersistedRun::Stopping {
                    id: RunId(2),
                    started_at: 900,
                    process_group: Some(42),
                    reason: StopReason::User,
                },
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Active,
                runtime_class: RuntimeClass::Stopping,
                display: DisplayClass::Stopping,
                next_due: None,
                automatic_start: false,
            },
            Case {
                name: "invalid durable row fails closed",
                paused: false,
                run: PersistedRun::Invalid,
                interval: 100,
                now: 1_000,
                schedule: ScheduleClass::Enabled,
                attempt_class: AttemptClass::Invalid,
                runtime_class: RuntimeClass::Idle,
                display: DisplayClass::Error,
                next_due: None,
                automatic_start: false,
            },
        ];

        for case in cases {
            let projection =
                classify_job(false, case.paused, &case.run, case.interval, case.now);
            assert_eq!(
                projection.registration,
                RegistrationClass::Active,
                "{}: registration",
                case.name
            );
            assert_eq!(projection.schedule, case.schedule, "{}: schedule", case.name);
            assert_eq!(
                projection.attempt, case.attempt_class,
                "{}: attempt",
                case.name
            );
            assert_eq!(
                projection.runtime, case.runtime_class,
                "{}: runtime",
                case.name
            );
            assert_eq!(projection.display, case.display, "{}: display", case.name);
            assert_eq!(projection.next_due, case.next_due, "{}: next due", case.name);
            assert_eq!(
                projection.automatic_start, case.automatic_start,
                "{}: scheduler eligibility",
                case.name
            );
        }
        let deleting =
            classify_job(true, false, &PersistedRun::NeverRun, 100, 1_000);
        assert_eq!(deleting.registration, RegistrationClass::Deleting);
        assert_eq!(deleting.display, DisplayClass::Deleting);
        assert!(!deleting.automatic_start);
    }

    #[test]
    fn run_transition_matrix_is_exhaustive() {
        use RunEvent as E;
        use RunStateKind as S;
        let events = [
            E::ExplicitStart,
            E::ScheduledStart,
            E::SpawnSucceeded,
            E::SpawnFailed,
            E::Cancel,
            E::ExitZero,
            E::ExitFailure,
            E::Shutdown,
            E::Restart,
            E::PauseChanged,
        ];
        let cases = [
            (
                S::NeverRun,
                [
                    Some(S::Starting),
                    Some(S::Starting),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::NeverRun),
                    Some(S::NeverRun),
                    Some(S::NeverRun),
                ],
            ),
            (
                S::Succeeded,
                [
                    Some(S::Starting),
                    Some(S::Starting),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Succeeded),
                    Some(S::Succeeded),
                    Some(S::Succeeded),
                ],
            ),
            (
                S::Failed,
                [
                    Some(S::Starting),
                    Some(S::Failed),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Failed),
                    Some(S::Failed),
                    Some(S::Failed),
                ],
            ),
            (
                S::Cancelled,
                [
                    Some(S::Starting),
                    Some(S::Cancelled),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Cancelled),
                    Some(S::Cancelled),
                    Some(S::Cancelled),
                ],
            ),
            (
                S::Interrupted,
                [
                    Some(S::Starting),
                    Some(S::Interrupted),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Interrupted),
                    Some(S::Interrupted),
                    Some(S::Interrupted),
                ],
            ),
            (
                S::Starting,
                [
                    None,
                    None,
                    Some(S::Running),
                    Some(S::Failed),
                    Some(S::StoppingUser),
                    None,
                    None,
                    Some(S::StoppingShutdown),
                    Some(S::Interrupted),
                    Some(S::Starting),
                ],
            ),
            (
                S::Running,
                [
                    None,
                    None,
                    None,
                    None,
                    Some(S::StoppingUser),
                    Some(S::Succeeded),
                    Some(S::Failed),
                    Some(S::StoppingShutdown),
                    Some(S::Interrupted),
                    Some(S::Running),
                ],
            ),
            (
                S::StoppingUser,
                [
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Cancelled),
                    Some(S::Cancelled),
                    None,
                    Some(S::Cancelled),
                    Some(S::StoppingUser),
                ],
            ),
            (
                S::StoppingShutdown,
                [
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Interrupted),
                    Some(S::Interrupted),
                    None,
                    Some(S::Interrupted),
                    Some(S::StoppingShutdown),
                ],
            ),
        ];
        for (state, expected) in cases {
            for (event, expected) in events.into_iter().zip(expected) {
                assert_eq!(
                    transition_run(state, event).ok(),
                    expected,
                    "{state:?} + {event:?}"
                );
            }
        }
    }

    #[test]
    fn registration_transition_matrix_is_exhaustive() {
        use DeleteMode as D;
        use RegistrationEvent as E;
        use RegistrationState as S;
        let events = [
            E::BeginDelete(D::Registration),
            E::BeginDelete(D::Data),
            E::FinishDelete(D::Registration),
            E::FinishDelete(D::Data),
        ];
        let cases = [
            (
                S::Active,
                [
                    Some(S::Deleting(D::Registration)),
                    Some(S::Deleting(D::Data)),
                    None,
                    None,
                ],
            ),
            (
                S::Deleting(D::Registration),
                [
                    Some(S::Deleting(D::Registration)),
                    None,
                    Some(S::Removed),
                    None,
                ],
            ),
            (
                S::Deleting(D::Data),
                [
                    None,
                    Some(S::Deleting(D::Data)),
                    None,
                    Some(S::Removed),
                ],
            ),
            (S::Removed, [None, None, None, None]),
        ];
        for (state, expected) in cases {
            for (event, expected) in events.into_iter().zip(expected) {
                assert_eq!(
                    transition_registration(state, event).ok(),
                    expected,
                    "{state:?} + {event:?}"
                );
            }
        }
    }

    #[test]
    fn cancel_stops_the_driver_process_group() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sleep");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut child = command.arg("60").spawn().unwrap();
        assert!(child.try_wait().unwrap().is_none(), "test child exited before recovery");
        let id = job_add("cmd", "sleep 60", "/tmp/sarun-cancel-test", 3600).unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(job_id,request,state,started_at,spawned_at,process_group)
             VALUES(?1,'explicit','running',?2,?2,?3)",
            params![id, now(), child.id()],
        )
        .unwrap();
        let run_id = RunId(conn.last_insert_rowid());
        running_map(|running| {
            running.insert(
                id,
                RunningProcess {
                    run_id,
                    process_group: Some(child.id()),
                    stop_reason: None,
                },
            );
        });
        job_cancel(id).unwrap();
        let status = child.wait().unwrap();
        finish_child_run(run_id, -1, "test cancellation").unwrap();
        remove_owner(id, run_id);
        assert!(!status.success());
        assert_eq!(
            jobs_list().unwrap()[0].state,
            "cancelled",
            "explicit cancellation is a durable outcome"
        );
    }

    #[test]
    fn cancel_during_starting_is_durable_and_does_not_spawn_again() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let id = job_add(
            "cmd",
            "true",
            temporary.path().join("dest").to_str().unwrap(),
            3600,
        )
        .unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(job_id,request,state,started_at)
             VALUES(?1,'explicit','starting',?2)",
            params![id, now()],
        )
        .unwrap();
        let run_id = RunId(conn.last_insert_rowid());
        running_map(|owners| {
            owners.insert(
                id,
                RunningProcess {
                    run_id,
                    process_group: None,
                    stop_reason: None,
                },
            );
        });

        job_cancel(id).unwrap();
        assert_eq!(jobs_list().unwrap()[0].state, "stopping");
        finish_stopped_run(run_id, "cancelled before spawn").unwrap();
        remove_owner(id, run_id);
        let job = jobs_list().unwrap().remove(0);
        assert_eq!(job.state, "cancelled");
        assert!(!job.automatic_start);
    }

    #[test]
    fn pre_spawn_failure_cannot_leave_a_starting_run() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let id = job_add(
            "cmd",
            "true",
            temporary.path().join("dest").to_str().unwrap(),
            3600,
        )
        .unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(job_id,request,state,started_at)
             VALUES(?1,'explicit','starting',?2)",
            params![id, now()],
        )
        .unwrap();
        let run_id = RunId(conn.last_insert_rowid());

        finish_pre_spawn_failure(run_id, "ownership check failed").unwrap();
        let state: String = conn
            .query_row("SELECT state FROM runs WHERE id=?1", [run_id.0], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[test]
    fn restart_recovery_terminates_recorded_process_group_before_publishing_state() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sleep");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut child = command.arg("60").spawn().unwrap();
        let id = job_add(
            "cmd",
            "sleep 60",
            temporary.path().join("dest").to_str().unwrap(),
            3600,
        )
        .unwrap();
        let spawned_at = now();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(
                job_id,request,state,started_at,spawned_at,process_group
             ) VALUES(?1,'explicit','running',?2,?2,?3)",
            params![id, spawned_at, child.id()],
        )
        .unwrap();

        let recovery = recover_unowned_runs();
        let _ = child.wait();
        assert!(recovery.is_ok(), "{recovery:?}");
        assert_eq!(jobs_list().unwrap()[0].state, "interrupted");
    }

    #[test]
    fn stale_child_exit_cannot_finish_a_newer_run() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let id = job_add(
            "cmd",
            "true",
            temporary.path().join("dest").to_str().unwrap(),
            3600,
        )
        .unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(job_id,request,state,started_at,spawned_at,process_group)
             VALUES(?1,'explicit','running',1,1,42)",
            [id],
        )
        .unwrap();
        let old = RunId(conn.last_insert_rowid());
        recover_unowned_runs().unwrap();
        conn.execute(
            "INSERT INTO runs(job_id,request,state,started_at)
             VALUES(?1,'explicit','starting',2)",
            [id],
        )
        .unwrap();
        let current = RunId(conn.last_insert_rowid());

        assert_eq!(
            finish_child_run(old, 0, "late success").unwrap_err(),
            "stale child completion"
        );
        let state: String = conn
            .query_row("SELECT state FROM runs WHERE id=?1", [current.0], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "starting");
    }

    #[test]
    fn pause_while_running_changes_only_schedule() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let id = job_add(
            "cmd",
            "true",
            temporary.path().join("dest").to_str().unwrap(),
            3600,
        )
        .unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(job_id,request,state,started_at,spawned_at,process_group)
             VALUES(?1,'explicit','running',1,1,42)",
            [id],
        )
        .unwrap();
        let run_id = RunId(conn.last_insert_rowid());

        job_set_paused(id, true).unwrap();
        let job = jobs_list().unwrap().remove(0);
        assert!(job.paused);
        assert_eq!(job.state, "running");
        let state: String = conn
            .query_row("SELECT state FROM runs WHERE id=?1", [run_id.0], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "running");
    }

    #[test]
    fn restart_preserves_explicit_cancel_and_interrupts_other_active_runs() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let conn = db().unwrap();
        for (index, state, reason) in [
            (0, "starting", None),
            (1, "running", None),
            (2, "stopping", Some("user")),
        ] {
            let id = job_add(
                "cmd",
                "true",
                temporary
                    .path()
                    .join(format!("dest-{index}"))
                    .to_str()
                    .unwrap(),
                3600,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runs(
                    job_id,request,state,started_at,spawned_at,process_group,stop_reason
                 )
                 VALUES(?1,'explicit',?2,1,
                        CASE WHEN ?2='running' THEN 1 ELSE NULL END,
                        CASE WHEN ?2='starting' THEN NULL ELSE 42 END,?3)",
                params![id, state, reason],
            )
            .unwrap();
        }

        recover_unowned_runs().unwrap();
        let states = jobs_list()
            .unwrap()
            .into_iter()
            .map(|job| (job.state, job.automatic_start))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                ("interrupted".into(), false),
                ("interrupted".into(), false),
                ("cancelled".into(), false),
            ]
        );
    }

    #[test]
    fn active_run_blocks_delete_and_a_second_start() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let destination = temporary.path().join("dest");
        let id = job_add("cmd", "sleep 10", destination.to_str().unwrap(), 3600).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let contenders = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    start_job(id, StartRequest::Explicit(WikiRun::Maintain))
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let starts = contenders
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(starts.iter().filter(|result| result.is_ok()).count(), 1);
        let first = starts.into_iter().find_map(Result::ok).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let cancel = {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                job_cancel(id)
            })
        };
        let remove = {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                job_remove(id)
            })
        };
        barrier.wait();
        let cancel_result = cancel.join().unwrap();
        let remove_result = remove.join().unwrap();
        assert!(cancel_result.is_ok(), "{cancel_result:?}");
        match remove_result {
            Ok(_) => {
                assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
            }
            Err(error) => {
                assert!(
                    error.to_string().contains("stop it first"),
                    "unexpected remove error: {error:#}"
                );
                for _ in 0..200 {
                    if jobs_list()
                        .unwrap()
                        .first()
                        .is_some_and(|job| job.state == "cancelled")
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                assert_eq!(jobs_list().unwrap()[0].state, "cancelled");
            }
        }
        assert!(running_map(|owners| {
            owners.get(&id).is_none_or(|owner| owner.run_id != first)
        }));
    }

    #[test]
    fn deleting_destination_cannot_be_registered_by_a_new_owner() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let destination = temporary.path().join("testwiki.swdump");
        let destination_text = destination.to_str().unwrap().to_owned();
        let id = job_add("wiki", "testwiki", &destination_text, 3600).unwrap();
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let deletion = std::thread::spawn(move || {
            prepare_job_deletion(id, Some("wiki"), DeleteMode::Data).unwrap();
            claimed_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
            finish_job_deletion(id, DeleteMode::Data).unwrap();
        });

        claimed_rx.recv().unwrap();
        let job = jobs_list().unwrap().remove(0);
        assert_eq!(job.state, "deleting");
        assert!(
            job_add("wiki", "new-owner", &destination_text, 3600)
                .unwrap_err()
                .contains("already owned"),
            "the deleting row must retain destination ownership"
        );
        finish_tx.send(()).unwrap();
        deletion.join().unwrap();
        assert!(jobs_list().unwrap().is_empty());
        job_add("wiki", "new-owner", &destination_text, 3600).unwrap();
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
        std::fs::write(&auxiliary[1], b"selector").unwrap();
        std::fs::create_dir(&auxiliary[2]).unwrap();
        std::fs::write(auxiliary[2].join("generation"), b"archive").unwrap();
        std::fs::write(&auxiliary[3], b"install receipt").unwrap();
        std::fs::write(&auxiliary[4], b"back references").unwrap();
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
        assert_eq!(job.state, "pending");
        assert!(job.next_due.is_none());
        assert!(job.last_start.is_none());
    }

    #[test]
    fn legacy_nullable_outcome_schema_is_replaced_once() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let legacy = Connection::open(crate::paths::state_home().join("mirrors.db")).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE jobs(
                    id INTEGER PRIMARY KEY,kind TEXT NOT NULL,src TEXT NOT NULL,
                    dest TEXT NOT NULL,interval_secs INTEGER NOT NULL,
                    paused INTEGER NOT NULL,last_start INTEGER,last_end INTEGER,
                    last_exit INTEGER,last_detail TEXT NOT NULL,media_source TEXT
                 );
                 INSERT INTO jobs VALUES
                    (1,'cmd','true','/legacy',3600,0,10,NULL,NULL,'partial',NULL);",
            )
            .unwrap();
        drop(legacy);

        let migrated = db().unwrap();
        let columns = migrated
            .prepare("PRAGMA table_info(jobs)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!columns.iter().any(|column| column.starts_with("last_")));
        let state: String = migrated
            .query_row("SELECT state FROM runs WHERE job_id=1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "interrupted");
        let version: i64 = migrated
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIRROR_SCHEMA_VERSION);
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
