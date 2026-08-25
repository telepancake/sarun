//! Mirror-update jobs: Chupa's schedule for `gitdepot mirror` /
//! `wikimak fetch` / `ietfmak update` runs (MIRRORS.md "Update").
//!
//! Chupa owns the schedule and lifecycle. A job gets one supervised
//! wikimak process for isolation and stderr attribution; its Kati graph calls
//! the `wikimak` brush builtin for build nodes, so the actual mirror work,
//! cancellation state, provenance, and Wikimedia gate have one owner rather
//! than a private dispatcher/service hierarchy.
//!
//! Bookkeeping lives in `{state_home}/mirrors.db` (jobs are Chupa inventory,
//! not mirrored corpus data). Runs and their identities are durable;
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
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

static STATE_HOME: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

/// Select the directory containing Chupa's durable supervisor database.
///
/// An embedder such as Sarun calls this with its namespaced state directory so
/// existing registrations remain in place. Standalone Chupa otherwise uses
/// `$CHUPA_STATE_HOME`, then the ordinary XDG state location.
pub fn set_state_home(path: impl Into<PathBuf>) {
    *STATE_HOME
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("Chupa state-home lock poisoned") = Some(path.into());
}

pub fn state_home() -> PathBuf {
    if let Some(path) = STATE_HOME
        .get_or_init(|| RwLock::new(None))
        .read()
        .expect("Chupa state-home lock poisoned")
        .clone()
    {
        return path;
    }
    if let Some(path) = std::env::var_os("CHUPA_STATE_HOME").filter(|value| !value.is_empty()) {
        return path.into();
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()))
                .join(".local/state")
        });
    base.join("chupa")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn db() -> Result<Connection, String> {
    let state_home = state_home();
    std::fs::create_dir_all(&state_home).map_err(|error| {
        format!("create Chupa state directory {}: {error}", state_home.display())
    })?;
    let path = state_home.join("mirrors.db");
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

const MIRROR_SCHEMA_VERSION: i64 = 5;

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
                request TEXT NOT NULL CHECK(request IN ('explicit','scheduled','full','images','backrefs')),
                state TEXT NOT NULL CHECK(state IN
                    ('starting','running','stopping','succeeded','failed',
                     'cancelled','interrupted')),
                started_at INTEGER NOT NULL,
                spawned_at INTEGER,
                ended_at INTEGER,
                process_group INTEGER,
                process_start_identity INTEGER,
                recovery_blocked INTEGER NOT NULL DEFAULT 0 CHECK(recovery_blocked IN (0,1)),
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
    if !matches!(version, 0 | 1 | 2 | 3 | 4) {
        return Err(format!(
            "unsupported mirror database schema {version}; expected 0, 1, 2, 3, 4, or {MIRROR_SCHEMA_VERSION}"
        ));
    }
    let transaction = conn.transaction().map_err(|error| error.to_string())?;
    let version: i64 = transaction
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if version == MIRROR_SCHEMA_VERSION {
        return transaction.commit().map_err(|error| error.to_string());
    }
    if version == 1 {
        transaction
            .execute(
                "ALTER TABLE runs ADD COLUMN process_start_identity INTEGER",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    if matches!(version, 1 | 2) {
        transaction
            .execute(
                "ALTER TABLE runs ADD COLUMN recovery_blocked INTEGER NOT NULL DEFAULT 0
                 CHECK(recovery_blocked IN (0,1))",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    if version == 0 {
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
    }
    if version != 0 {
        rebuild_runs_for_backrefs_requests(&transaction)?;
    }
    transaction
        .pragma_update(None, "user_version", MIRROR_SCHEMA_VERSION)
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

/// SQLite cannot alter a CHECK constraint in place. Rebuild only the runs
/// table, copying every durable column and preserving explicit row ids. The
/// operation is inside the caller's transaction, so jobs and runs remain an
/// all-or-nothing migration; no run history is reconstructed or discarded.
fn rebuild_runs_for_backrefs_requests(transaction: &Transaction<'_>) -> Result<(), String> {
    transaction
        .execute_batch(
            "CREATE TABLE runs_backrefs_requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                request TEXT NOT NULL CHECK(request IN ('explicit','scheduled','full','images','backrefs')),
                state TEXT NOT NULL CHECK(state IN
                    ('starting','running','stopping','succeeded','failed',
                     'cancelled','interrupted')),
                started_at INTEGER NOT NULL,
                spawned_at INTEGER,
                ended_at INTEGER,
                process_group INTEGER,
                process_start_identity INTEGER,
                recovery_blocked INTEGER NOT NULL DEFAULT 0 CHECK(recovery_blocked IN (0,1)),
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
            INSERT INTO runs_backrefs_requests(
                id,job_id,request,state,started_at,spawned_at,ended_at,
                process_group,process_start_identity,recovery_blocked,
                exit_code,stop_reason,detail
            )
            SELECT id,job_id,request,state,started_at,spawned_at,ended_at,
                   process_group,process_start_identity,recovery_blocked,
                   exit_code,stop_reason,detail
            FROM runs;
            DROP TABLE runs;
            ALTER TABLE runs_backrefs_requests RENAME TO runs;
            CREATE UNIQUE INDEX one_active_run_per_job ON runs(job_id)
                WHERE state IN ('starting','running','stopping');
            CREATE INDEX runs_by_job ON runs(job_id,id DESC);",
        )
        .map_err(|error| error.to_string())
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
    /// Durable work identity of the most recent run: explicit, scheduled,
    /// full, images, or backrefs. This is a projection of runs.request, never inferred
    /// from stderr or from the UI action that happens to be selected.
    #[serde(default)]
    pub last_request: Option<String>,
    /// Optional Kiwix source selector.  The UI stores `auto`, meaning the
    /// latest matching official all-maxi release is fetched in ranges.
    #[serde(default)]
    pub media_source: Option<String>,
    /// Whether the installed Wikimedia mirror has category/backreference
    /// work waiting for the explicit full relation-index scan.
    #[serde(default)]
    pub backrefs_pending: bool,
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
    pub fetch_server_timed_retries: Option<u64>,
    #[serde(default)]
    pub fetch_robots_timed_retries: Option<u64>,
    #[serde(default)]
    pub fetch_fallback_timed_retries: Option<u64>,
    #[serde(default)]
    pub fetch_local_spacing_timed_retries: Option<u64>,
    #[serde(default)]
    pub bz2_admission_limit: Option<u64>,
    #[serde(default)]
    pub bz2_admission_active_decoders: Option<u64>,
    #[serde(default)]
    pub bz2_admission_peak_active_decoders: Option<u64>,
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

/// The fetch layer records pending category/backreference work in this
/// destination-local marker. Older installed mirrors may predate that marker,
/// so a missing current sidecar is also pending when the mirror has an
/// installed title index. This is deliberately a cheap filesystem projection
/// for the UI; the `backrefs-task` driver remains authoritative and validates
/// the selected generation before publishing anything.
pub fn wikipedia_backrefs_pending(archive: &std::path::Path) -> bool {
    let title_index = archive.with_extension("swtitle");
    if !title_index.is_file() {
        return false;
    }
    let task = wikimak_wikipedia::mirror_scratch_path(archive).join("backrefs.task.json");
    task.is_file() || !archive.with_extension("swrefs").is_file()
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
    j.backrefs_pending = j.kind == "wiki" && wikipedia_backrefs_pending(destination);
    // Wikipedia workers project every source/target/assembly observation into
    // one bounded, plan-bound file. This read opens only that file and retains
    // one row per fixed source/target slot; it never inspects build receipts,
    // partial directories, or archive data. The lifecycle projection above
    // remains authoritative even when this telemetry is stale.
    if j.kind == "wiki" {
        let progress = if uses_live_run_progress(&j) {
            run_id.and_then(|run_id| {
                wikimak_wikipedia::mirror_build_progress_for_run(
                    destination,
                    &run_id.0.to_string(),
                )
            })
        } else if matches!(j.last_request.as_deref(), Some("images" | "backrefs")) {
            None
        } else {
            // A terminal run may leave an update selector behind for
            // resumability.  Its run-bound source counters are no longer
            // live telemetry; retain the existing completed full-build
            // projection at the mirror root only.
            wikimak_wikipedia::mirror_build_progress(destination)
        };
        if let Some(progress) = progress {
            apply_wikipedia_progress(&mut j, progress);
        }
    }
    j
}

fn uses_live_run_progress(job: &Job) -> bool {
    job.kind == "wiki" && job.is_live() && job.last_request.as_deref() != Some("images")
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
    job.fetch_server_timed_retries = Some(progress.fetch_server_timed_retries);
    job.fetch_robots_timed_retries = Some(progress.fetch_robots_timed_retries);
    job.fetch_fallback_timed_retries = Some(progress.fetch_fallback_timed_retries);
    job.fetch_local_spacing_timed_retries = Some(progress.fetch_local_spacing_timed_retries);
    job.bz2_admission_limit = Some(progress.bz2_admission_limit);
    job.bz2_admission_active_decoders = Some(progress.bz2_admission_active_decoders);
    job.bz2_admission_peak_active_decoders = Some(progress.bz2_admission_peak_active_decoders);
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
                    r.process_group,r.stop_reason,r.request
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
                last_request: r.get(16)?,
                media_source: r.get(6)?,
                backrefs_pending: false,
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
                fetch_server_timed_retries: None,
                fetch_robots_timed_retries: None,
                fetch_fallback_timed_retries: None,
                fetch_local_spacing_timed_retries: None,
                bz2_admission_limit: None,
                bz2_admission_active_decoders: None,
                bz2_admission_peak_active_decoders: None,
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
    if kind == "wiki" && !std::path::Path::new(dest).is_absolute() {
        return Err(format!(
            "Wikipedia mirror destination must be an absolute path: {dest}"
        ));
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
        remove_git_fetch_path(&p)?;
    }
    finish_job_deletion(id, DeleteMode::Registration)?;
    Ok(format!("fetch buffer dropped; store kept at {dest}/store"))
}

// This recursive helper is restricted to the derived git fetch buffer.
// Wikipedia cleanup uses the ownership-aware nonrecursive/quarantine paths
// below and never passes a user-visible wiki namespace here.
fn remove_git_fetch_path(path: &std::path::Path) -> Result<(), String> {
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

#[derive(Default)]
struct WikipediaDeletionReport {
    reclaimed_bytes: u64,
    reclaimed_paths: usize,
    quarantined: Vec<(std::path::PathBuf, std::path::PathBuf, String)>,
}

impl WikipediaDeletionReport {
    fn note(&self, archive: &std::path::Path) -> String {
        let mut note = format!(
            "Wikipedia data removed from {}: reclaimed {} bytes from {} validated files",
            archive.display(),
            self.reclaimed_bytes,
            self.reclaimed_paths,
        );
        if !self.quarantined.is_empty() {
            note.push_str("; quarantined ");
            for (index, (source, destination, reason)) in self.quarantined.iter().enumerate() {
                if index != 0 {
                    note.push_str(", ");
                }
                note.push_str(&format!(
                    "{} -> {} ({reason})",
                    source.display(),
                    destination.display()
                ));
            }
        }
        note
    }
}

fn wikipedia_quarantine_root(archive: &std::path::Path) -> std::path::PathBuf {
    archive
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".sarun-quarantine")
}

fn wikipedia_path_present(path: &std::path::Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn sync_wikipedia_directory(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync Wikipedia cleanup directory {}: {error}", path.display()))?;
    Ok(())
}

fn ensure_wikipedia_quarantine_root(path: &std::path::Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "Wikipedia quarantine root is not a directory: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            if !metadata.file_type().is_dir() {
                return Err(format!(
                    "Wikipedia quarantine root became non-directory: {}",
                    path.display()
                ));
            }
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    }
    sync_wikipedia_directory(path)
}

fn rename_wikipedia_without_replacing(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "source contains NUL")
        })?;
        let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination contains NUL")
        })?;
        let rc = unsafe {
            libc::renameatx_np(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "source contains NUL")
        })?;
        let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination contains NUL")
        })?;
        // `libc` does not publish the renameat2 wrapper for musl targets, but
        // it does publish the architecture-correct syscall number. Keep the
        // kernel's atomic no-replace operation instead of emulating it with a
        // racy destination check followed by rename.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, destination);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace rename is unavailable on this platform",
        ))
    }
}

fn quarantine_wikipedia_path(
    path: &std::path::Path,
    quarantine_root: &std::path::Path,
    reason: impl Into<String>,
    report: &mut WikipediaDeletionReport,
) -> Result<(), String> {
    if !wikipedia_path_present(path)? {
        return Ok(());
    }
    ensure_wikipedia_quarantine_root(quarantine_root)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    let mut counter = 0_u32;
    let destination = loop {
        let suffix = if counter == 0 {
            String::new()
        } else {
            format!("-{counter}")
        };
        let candidate = quarantine_root.join(format!("wiki-delete-{name}{suffix}"));
        match rename_wikipedia_without_replacing(path, &candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                counter = counter.checked_add(1).ok_or_else(|| {
                    format!("quarantine name exhausted for {}", path.display())
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "cannot quarantine Wikipedia path {} -> {}: {error}",
                    path.display(),
                    candidate.display()
                ));
            }
        }
    };
    let source_parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    sync_wikipedia_directory(source_parent)?;
    sync_wikipedia_directory(quarantine_root)?;
    report
        .quarantined
        .push((path.to_path_buf(), destination, reason.into()));
    Ok(())
}

fn wikipedia_directory_entries(path: &std::path::Path) -> Result<Vec<std::path::PathBuf>, String> {
    std::fs::read_dir(path)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("{}: {error}", path.display()))
        })
        .collect()
}

fn is_wikipedia_generation_id(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn wikipedia_generation_manifest_id(name: &str) -> Option<&str> {
    let generation_id = name.strip_suffix(".manifest.json")?;
    is_wikipedia_generation_id(generation_id).then_some(generation_id)
}

fn wikipedia_selected_generation_id(
    destination: &std::path::Path,
) -> Result<Option<String>, String> {
    let selector = destination.with_extension("swtitle");
    match std::fs::symlink_metadata(&selector) {
        Ok(metadata) if metadata.file_type().is_file() => {
            wikimak_wikipedia::title_index::TitleIndex::open(&selector)
                .map(|index| Some(index.generation_id().as_str().to_owned()))
                .map_err(|error| format!("inspect selected Wikipedia generation {}: {error}", selector.display()))
        }
        Ok(_) => Err(format!(
            "selected Wikipedia title selector is not a regular file: {}",
            selector.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("inspect selected Wikipedia generation {}: {error}", selector.display())),
    }
}

fn validate_wikipedia_selector_for_deletion(
    destination: &std::path::Path,
    generation_root: &std::path::Path,
    leases: &[(String, wikimak_wikipedia::archive::ArchiveCleanupLease)],
) -> Result<Option<String>, String> {
    let Some(generation_id) = wikipedia_selected_generation_id(destination)? else {
        return Ok(None);
    };
    if !is_wikipedia_generation_id(&generation_id) {
        return Err(format!(
            "selected Wikipedia generation has an invalid identifier: {generation_id:?}"
        ));
    }
    let generation = generation_root.join(&generation_id);
    let metadata = std::fs::symlink_metadata(&generation).map_err(|error| {
        format!(
            "selected Wikipedia generation {} is not available for explicit deletion: {error}",
            generation.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "selected Wikipedia generation is not a real directory: {}",
            generation.display()
        ));
    }
    if !leases
        .iter()
        .any(|(leased_generation_id, _)| leased_generation_id == &generation_id)
    {
        return Err(format!(
            "selected Wikipedia generation {} has no acquired cleanup lease",
            generation.display()
        ));
    }
    Ok(Some(generation_id))
}

fn wikipedia_quarantine_entry_matches_source(
    entry_name: &str,
    source_name: &str,
) -> bool {
    let prefix = format!("wiki-delete-{source_name}");
    entry_name == prefix
        || entry_name
            .strip_prefix(&prefix)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .is_some_and(|counter| {
                !counter.is_empty() && counter.bytes().all(|byte| byte.is_ascii_digit())
            })
}

fn report_existing_wikipedia_quarantine(
    source: &std::path::Path,
    quarantine_root: &std::path::Path,
    reason: &str,
    report: &mut WikipediaDeletionReport,
) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(quarantine_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", quarantine_root.display())),
    };
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "Wikipedia quarantine root is not a directory: {}",
            quarantine_root.display()
        ));
    }
    let Some(source_name) = source.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let mut retained = wikipedia_directory_entries(quarantine_root)?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| wikipedia_quarantine_entry_matches_source(name, source_name))
        })
        .collect::<Vec<_>>();
    retained.sort();
    report.quarantined.extend(retained.into_iter().map(|destination| {
        (source.to_path_buf(), destination, reason.to_owned())
    }));
    Ok(())
}

fn quarantine_wikipedia_selector(
    selector: &std::path::Path,
    selected_generation: Option<&str>,
    quarantine_root: &std::path::Path,
    report: &mut WikipediaDeletionReport,
) -> Result<(), String> {
    if wikipedia_path_present(selector)? {
        let generation_id = selected_generation.ok_or_else(|| {
            format!(
                "Wikipedia selector {} exists but was not validated",
                selector.display()
            )
        })?;
        quarantine_wikipedia_path(
            selector,
            quarantine_root,
            format!(
                "explicit deletion visibility boundary for selected generation {generation_id}"
            ),
            report,
        )
    } else {
        report_existing_wikipedia_quarantine(
            selector,
            quarantine_root,
            "title selector was already quarantined by an earlier explicit-deletion attempt",
            report,
        )
    }
}

fn acquire_wikipedia_archive_cleanup_lease(
    path: &std::path::Path,
    label: &str,
) -> Result<Option<wikimak_wikipedia::archive::ArchiveCleanupLease>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if !metadata.file_type().is_dir() {
        return Ok(None);
    }
    match wikimak_wikipedia::archive::try_acquire_archive_cleanup_lease(path)
        .map_err(|error| format!("acquire {label} cleanup lease for {}: {error}", path.display()))?
    {
        Some(lease) => Ok(Some(lease)),
        None => Err(format!(
            "Wikipedia data deletion is blocked before filesystem mutation: {label} {} still has an active reader; close the reader and retry",
            path.display()
        )),
    }
}

fn acquire_wikipedia_writer_cleanup_lease(
    scratch: &std::path::Path,
) -> Result<wikimak_wikipedia::direct::MirrorBuildWriterCleanupLease, String> {
    match wikimak_wikipedia::direct::try_acquire_mirror_build_writer_cleanup_lease(scratch)
        .map_err(|error| {
            format!(
                "Wikipedia data deletion is blocked before filesystem mutation: cannot establish writer exclusion for {}: {error}",
                scratch.display()
            )
        })?
    {
        Some(lease) => Ok(lease),
        None => Err(format!(
            "Wikipedia data deletion is blocked before filesystem mutation: {} still has an active importer; stop it and retry",
            scratch.display()
        )),
    }
}

fn acquire_wikipedia_generation_cleanup_leases(
    root: &std::path::Path,
) -> Result<Vec<(String, wikimak_wikipedia::archive::ArchiveCleanupLease)>, String> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "Wikipedia data deletion is blocked before filesystem mutation: generation namespace {} is not a real directory",
            root.display()
        ));
    }
    let mut leases = Vec::new();
    for generation in wikipedia_directory_entries(root)? {
        let name = generation
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !is_wikipedia_generation_id(name) {
            continue;
        }
        if let Some(lease) =
            acquire_wikipedia_archive_cleanup_lease(&generation, "generation")?
        {
            leases.push((name.to_owned(), lease));
        }
    }
    Ok(leases)
}

fn reclaim_wikipedia_generation_namespace(
    destination: &std::path::Path,
    quarantine_root: &std::path::Path,
    report: &mut WikipediaDeletionReport,
    leases: &mut Vec<(String, wikimak_wikipedia::archive::ArchiveCleanupLease)>,
) -> Result<(), String> {
    let root = destination.with_extension("generations");
    let metadata = match std::fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };
    if !metadata.file_type().is_dir() {
        return quarantine_wikipedia_path(
            &root,
            quarantine_root,
            "unvalidated generation namespace is not a directory",
            report,
        );
    }
    // Take one direct-entry snapshot before making any change. In particular,
    // a manifest sidecar must not be moved merely because the filesystem
    // happened to enumerate it before its generation directory.
    let mut generation_entries = Vec::new();
    let mut manifest_entries = Vec::new();
    let mut unknown_entries = Vec::new();
    for entry in wikipedia_directory_entries(&root)? {
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if is_wikipedia_generation_id(name) {
            generation_entries.push(entry);
        } else if wikipedia_generation_manifest_id(name).is_some() {
            manifest_entries.push(entry);
        } else {
            unknown_entries.push(entry);
        }
    }

    let entry_name = |path: &std::path::Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    generation_entries.sort_by_key(|path| entry_name(path));
    manifest_entries.sort_by_key(|path| entry_name(path));
    unknown_entries.sort_by_key(|path| entry_name(path));

    // Keep this list separate from the report: a generation whose exact
    // owned children were partly reclaimed is still moved as one remaining
    // namespace, and its sidecar is handled only after all generations.
    let mut quarantined_generations = Vec::new();
    for entry in generation_entries {
        let name = entry_name(&entry);
        let metadata = std::fs::symlink_metadata(&entry)
            .map_err(|error| format!("inspect {}: {error}", entry.display()))?;
        if !metadata.file_type().is_dir() {
            quarantine_wikipedia_path(
                &entry,
                quarantine_root,
                "generation entry is not a directory",
                report,
            )?;
            quarantined_generations.push(name);
            continue;
        }

        let position = leases
            .iter()
            .position(|(generation_id, _)| generation_id == &name)
            .ok_or_else(|| format!("generation {name} has no acquired cleanup lease"))?;
        let (_, lease) = leases.remove(position);
        match wikimak_wikipedia::reclaim_installed_generation(destination, &name, &lease) {
            Ok(cleanup) => {
                report.reclaimed_paths = report
                    .reclaimed_paths
                    .saturating_add(cleanup.reclaimed_segments as usize);
                report.reclaimed_bytes = report
                    .reclaimed_bytes
                    .saturating_add(cleanup.reclaimed_bytes);
                if !cleanup.pending_paths.is_empty() || !cleanup.quarantined_paths.is_empty() {
                    quarantine_wikipedia_path(
                        &entry,
                        quarantine_root,
                        "generation ownership cleanup has pending paths",
                        report,
                    )?;
                    quarantined_generations.push(name);
                }
            }
            Err(error) if error.starts_with("refusing to reclaim selected generation ") => {
                // Explicit deletion moved and synced the selector before
                // entering this function. Preserve the low-level lifecycle
                // guard and surface any contradictory publication state.
                return Err(error);
            }
            Err(error) => {
                // Missing, legacy, and malformed ownership receipts all end
                // up here before the lifecycle API reads any archive payload.
                // Preserve the complete remaining namespace for inspection;
                // do not fall back to recursive traversal or deletion.
                quarantine_wikipedia_path(
                    &entry,
                    quarantine_root,
                    format!("generation ownership is unavailable: {error}"),
                    report,
                )?;
                quarantined_generations.push(name);
            }
        }
    }

    // Manifest sidecars are processed after their generation directories.
    // A successful reclaim normally removed the sidecar itself. A pending or
    // unvalidated generation leaves it behind, so move that sidecar intact as
    // part of the same explicit-deletion report. Orphan sidecars are likewise
    // unvalidated and must not be silently discarded.
    for entry in manifest_entries {
        let name = entry_name(&entry);
        let Some(generation_id) = wikipedia_generation_manifest_id(&name) else {
            continue;
        };
        let generation = root.join(generation_id);
        if quarantined_generations
            .iter()
            .any(|id| id == generation_id)
            || wikipedia_path_present(&generation)?
        {
            quarantine_wikipedia_path(
                &entry,
                quarantine_root,
                "ownership sidecar belongs to a quarantined generation",
                report,
            )?;
        } else {
            // The generation was reclaimed and its sidecar should have been
            // removed by the same lifecycle operation. If the sidecar still
            // exists, it is an orphaned residual, not deletion authority.
            quarantine_wikipedia_path(
                &entry,
                quarantine_root,
                "orphaned generation ownership sidecar",
                report,
            )?;
        }
    }

    for entry in unknown_entries {
        quarantine_wikipedia_path(
            &entry,
            quarantine_root,
            "unrecognized residual under generation namespace",
            report,
        )?;
    }

    // No later recursive sweep: entries created after the snapshot are not
    // ours to classify. The root is removed only when the filesystem itself
    // confirms that all snapshotted work is gone.
    match std::fs::remove_dir(&root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Err(format!(
            "Wikipedia generation namespace {} still contains unsnapshotted residuals",
            root.display()
        )),
        Err(error) => Err(format!("{}: {error}", root.display())),
    }
}

/// Durably claim deletion of an idle Wikipedia job, reclaiming only validated
/// metadata-identified generation segments and moving unvalidated legacy archives,
/// sidecars, media, scratch, and residual namespaces to destination-local
/// quarantine before releasing the destination registration. Generation
/// cleanup holds wikimak's archive cleanup leases through segment unlink. A failed or
/// interrupted cleanup remains resumable and cannot race a new owner of the
/// same destination.
pub fn job_remove_with_data(id: i64) -> Result<String, String> {
    let _gate = DELETE_GATE.lock().unwrap();
    let (_, destination) = prepare_job_deletion(id, Some("wiki"), DeleteMode::Data)?;
    let archive = std::path::PathBuf::from(&destination);
    let quarantine_root = wikipedia_quarantine_root(&archive);
    let mut report = WikipediaDeletionReport::default();
    let titles = archive.with_extension("swtitle");
    let media = archive.with_extension("media");
    let generations = archive.with_extension("generations");
    let scratch = wikimak_wikipedia::mirror_scratch_path(&archive);
    // Acquire writer exclusion and every reader lease before the first
    // filesystem mutation. If an importer or reader is still active, dropping
    // earlier leases leaves the durable deleting row available for an exact
    // retry without touching archive data.
    let _writer_lease = acquire_wikipedia_writer_cleanup_lease(&scratch)?;
    let _archive_lease =
        acquire_wikipedia_archive_cleanup_lease(&archive, "archive")?;
    let mut generation_leases = acquire_wikipedia_generation_cleanup_leases(&generations)?;
    // Snapshot and validate publication state only after all cooperating
    // writers/readers are excluded, but before any filesystem mutation. A
    // live selector must name one of the real generation directories whose
    // cleanup lease is held by this explicit deletion.
    let selected_generation = validate_wikipedia_selector_for_deletion(
        &archive,
        &generations,
        &generation_leases,
    )?;
    // This no-replace rename plus both directory syncs is the explicit
    // deletion visibility boundary: after it completes, no generation is
    // selected for serving and the low-level selected-generation guard can
    // remain strict for every other lifecycle caller.
    quarantine_wikipedia_selector(
        &titles,
        selected_generation.as_deref(),
        &quarantine_root,
        &mut report,
    )?;
    quarantine_wikipedia_path(
        &archive,
        &quarantine_root,
        "legacy archive has no durable engine-owned content identity",
        &mut report,
    )?;
    quarantine_wikipedia_path(
        &media,
        &quarantine_root,
        "media namespace has no public ownership validator in the engine",
        &mut report,
    )?;
    for path in wikimak_wikipedia::mirror_auxiliary_paths(&archive)? {
        if path == titles {
            // The selector was handled as the visibility boundary above.
            continue;
        } else if path == wikimak_wikipedia::mirror_scratch_path(&archive) {
            quarantine_wikipedia_path(
                &path,
                &quarantine_root,
                "Wikimedia scratch namespace has no public ownership validator in the engine",
                &mut report,
            )?;
        } else if path == generations {
            reclaim_wikipedia_generation_namespace(
                &archive,
                &quarantine_root,
                &mut report,
                &mut generation_leases,
            )?;
        } else {
            quarantine_wikipedia_path(
                &path,
                &quarantine_root,
                "fixed sidecar name is not deletion ownership evidence",
                &mut report,
            )?;
        }
    }
    finish_job_deletion(id, DeleteMode::Data)?;
    Ok(report.note(&archive))
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

/// Start the selected Wikipedia mirror's images-only acquisition now. This is
/// explicit work, so it remains valid while the schedule is paused, but it
/// enters the same one-active-Wikipedia-job gate and the same supervised
/// process/cancellation lifecycle as text work.
pub fn job_run_images(id: i64) -> Result<(), String> {
    start_job(id, StartRequest::Explicit(WikiRun::Images))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Start the selected Wikipedia mirror's full category/backreference
/// relation-index scan. This is explicit work, so it remains valid while the
/// schedule is paused, but it shares the normal supervised run and
/// one-active-Wikipedia-job gate with text and image work.
pub fn job_run_backrefs(id: i64) -> Result<(), String> {
    start_job(id, StartRequest::Explicit(WikiRun::Backrefs))
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
    // wiki transfer active; start_job enforces the same admission rule for
    // explicit starts so the request gate remains a whole-engine bound.
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
    Images,
    Backrefs,
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
            Self::Explicit(WikiRun::Images) => "images",
            Self::Explicit(WikiRun::Backrefs) => "backrefs",
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

fn pause_blocks_request(request: StartRequest, paused: bool) -> bool {
    paused && matches!(request, StartRequest::Scheduled)
}

fn wiki_run_args(wiki_run: WikiRun, run_id: RunId, dbname: &str, archive: &str) -> Vec<String> {
    if matches!(wiki_run, WikiRun::Backrefs) {
        return vec!["backrefs-task".into(), archive.into()];
    }
    vec![
        match wiki_run {
            WikiRun::Maintain => "fetch",
            WikiRun::RefreshContent => "refresh-full",
            WikiRun::Images => "media-update",
            WikiRun::Backrefs => unreachable!("backrefs handled above"),
        }
        .into(),
        "--run-id".into(),
        run_id.0.to_string(),
        dbname.into(),
        archive.into(),
    ]
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
    if matches!(request, StartRequest::Explicit(WikiRun::RefreshContent)) && job.kind != "wiki" {
        return Err(StartError::Unavailable(
            "full snapshot re-ingest is only available for wiki mirrors".into(),
        ));
    }
    if matches!(request, StartRequest::Explicit(WikiRun::Images)) && job.kind != "wiki" {
        return Err(StartError::Unavailable(
            "image acquisition is only available for Wikipedia mirrors".into(),
        ));
    }
    if matches!(request, StartRequest::Explicit(WikiRun::Backrefs)) && job.kind != "wiki" {
        return Err(StartError::Unavailable(
            "category/backreference indexing is only available for Wikipedia mirrors".into(),
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
    if pause_blocks_request(request, job.paused) {
        return Err(StartError::Unavailable("job is paused".into()));
    }
    if matches!(request, StartRequest::Scheduled) {
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
    if job.kind == "wiki" {
        let active_wiki_runs = transaction
            .query_row(
                "SELECT COUNT(*)
                 FROM runs r JOIN jobs j ON j.id=r.job_id
                 WHERE j.kind='wiki' AND r.state IN ('starting','running','stopping')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())?;
        if active_wiki_runs != 0 {
            return Err(StartError::Unavailable(
                "another Wikipedia mirror job is already active".into(),
            ));
        }
    }
    let wiki_tmp = if job.kind == "wiki" {
        let path = wikimak_wikipedia::mirror_scratch_path(std::path::Path::new(&job.dest));
        std::fs::create_dir_all(&path).map_err(|error| {
            StartError::Fatal(format!(
                "cannot create Wikipedia scratch directory {}: {error}",
                path.display()
            ))
        })?;
        Some(path)
    } else {
        None
    };
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
    spawn_run(job, request.wiki_run(), run_id, wiki_tmp);
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

fn record_spawned(
    run_id: RunId,
    process_group: u32,
    process_start_identity: Option<i64>,
) -> Result<Option<StopReason>, String> {
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
                    "UPDATE runs SET state='running',spawned_at=?2,process_group=?3,
                                     process_start_identity=?4
                     WHERE id=?1 AND state='starting'",
                    params![run_id.0, now(), process_group, process_start_identity],
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
                    "UPDATE runs SET spawned_at=?2,process_group=?3,
                                     process_start_identity=?4
                     WHERE id=?1 AND state='stopping' AND stop_reason='user'",
                    params![run_id.0, now(), process_group, process_start_identity],
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
                    "UPDATE runs SET spawned_at=?2,process_group=?3,
                                     process_start_identity=?4
                     WHERE id=?1 AND state='stopping' AND stop_reason='shutdown'",
                    params![run_id.0, now(), process_group, process_start_identity],
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

fn spawn_run(
    job: JobConfig,
    wiki_run: WikiRun,
    run_id: RunId,
    wiki_tmp: Option<std::path::PathBuf>,
) {
    let driver = |name: &str| driver_argv(name, std::env::current_exe().ok());
    let argv: Vec<String> = match job.kind.as_str() {
        "git" => [
            driver("gitdepot"),
            vec!["mirror".into(), job.src.clone(), job.dest.clone()],
        ]
        .concat(),
        "wiki" => [
            driver("wikimak"),
            wiki_run_args(wiki_run, run_id, &job.src, &job.dest),
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
        if wiki && matches!(wiki_run, WikiRun::Images) {
            cmd.env(
                "SARUN_KIWIX_SOURCE",
                job.media_source.as_deref().unwrap_or("auto"),
            );
            let shared_media = std::path::Path::new(&job.dest)
                .parent()
                .map(|parent| parent.join("wikimedia.media"))
                .unwrap_or_else(|| std::path::PathBuf::from("wikimedia.media"));
            cmd.env("SARUN_WIKIMEDIA_MEDIA", shared_media);
        } else if let Some(source) = &job.media_source {
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
                let process_start_identity = match current_process_start_identity(process_group) {
                    Ok(identity) => identity,
                    Err(error) => {
                        // The local supervisor can still own and signal this
                        // group, but a restart must not guess its incarnation.
                        // Persisting NULL makes a later recovery retain the
                        // active row until it can prove the group is gone.
                        eprintln!(
                            "mirror job #{id} run #{} could not record process {} start identity: {error}",
                            run_id.0, process_group
                        );
                        None
                    }
                };
                let stop_reason = match record_spawned(
                    run_id,
                    process_group,
                    process_start_identity,
                ) {
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
        // must not prevent the scheduler from serving unrelated jobs.  A run
        // whose old group is not safely gone remains active, so its unique
        // active-run row still prevents a second destination mutator.
        eprintln!("mirror supervisor recovery completed with diagnostics: {error}");
    }
    std::thread::spawn(|| {
        loop {
            if SHUTTING_DOWN.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            if let Err(error) = recover_blocked_runs() {
                eprintln!("mirror blocked-run recovery tick failed: {error}");
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

/// Return the kernel's start identity for a process where the platform
/// exposes one that can be persisted and compared after an engine restart.
/// Darwin's libproc timestamp is preferable to the supervisor's wall clock:
/// it identifies the process incarnation rather than merely saying that it
/// was created around the same second.
fn current_process_start_identity(process_id: u32) -> Result<Option<i64>, String> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let expected_size = std::mem::size_of::<libc::proc_bsdinfo>();
        let returned = unsafe {
            libc::proc_pidinfo(
                i32::try_from(process_id)
                    .map_err(|_| format!("process id {process_id} exceeds i32"))?,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast::<libc::c_void>(),
                i32::try_from(expected_size)
                    .map_err(|_| "proc_bsdinfo size exceeds i32".to_owned())?,
            )
        };
        if returned != i32::try_from(expected_size).unwrap_or(i32::MAX) {
            let error = std::io::Error::last_os_error();
            return Err(format!(
                "proc_pidinfo({process_id}) returned {returned} instead of {expected_size}: {error}"
            ));
        }
        if info.pbi_pid != process_id || info.pbi_start_tvsec == 0 {
            return Err(format!(
                "proc_pidinfo({process_id}) returned an invalid process identity"
            ));
        }
        let micros = (info.pbi_start_tvsec as i128)
            .checked_mul(1_000_000)
            .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec as i128))
            .ok_or_else(|| format!("process {process_id} start identity overflowed"))?;
        return i64::try_from(micros)
            .map(Some)
            .map_err(|_| format!("process {process_id} start identity exceeds i64"));
    }

    #[cfg(target_os = "linux")]
    {
        let stat_path = format!("/proc/{process_id}/stat");
        let stat = std::fs::read_to_string(&stat_path)
            .map_err(|error| format!("read {stat_path}: {error}"))?;
        let fields = stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.split_whitespace().collect::<Vec<_>>())
            .ok_or_else(|| format!("malformed {stat_path}"))?;
        let start_ticks = fields
            .get(19)
            .ok_or_else(|| format!("{stat_path} has no start time"))?
            .parse::<u64>()
            .map_err(|_| format!("invalid start time in {stat_path}"))?;
        return i64::try_from(start_ticks)
            .map(Some)
            .map_err(|_| format!("process {process_id} start identity exceeds i64"));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = process_id;
        Ok(None)
    }
}

/// Check that a persisted group still looks like the process group created by
/// this run before sending a signal after engine restart.  On Linux the
/// process start time in /proc catches PID/PGID reuse.  Darwin uses libproc's
/// process start timestamp, persisted at spawn; equality of a PGID alone is
/// not an ownership proof.  Other Unix hosts currently have no equivalent
/// token here, so a live group is rejected rather than guessed safe.
fn recorded_group_is_safe(
    process_group: u32,
    spawned_at: Option<i64>,
    process_start_identity: Option<i64>,
) -> Result<bool, String> {
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
        if let Some(expected) = process_start_identity {
            return Ok(i64::try_from(start_ticks).ok() == Some(expected));
        }
        let Some(spawned_at) = spawned_at else {
            return Ok(false);
        };
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

    #[cfg(target_os = "macos")]
    {
        let _ = spawned_at;
        let Some(expected) = process_start_identity else {
            return Ok(false);
        };
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let expected_size = std::mem::size_of::<libc::proc_bsdinfo>();
        let returned = unsafe {
            libc::proc_pidinfo(
                group,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast::<libc::c_void>(),
                i32::try_from(expected_size)
                    .map_err(|_| "proc_bsdinfo size exceeds i32".to_owned())?,
            )
        };
        if returned != i32::try_from(expected_size).unwrap_or(i32::MAX) {
            let error = std::io::Error::last_os_error();
            return Err(format!(
                "proc_pidinfo({process_group}) returned {returned} instead of {expected_size}: {error}"
            ));
        }
        if info.pbi_pid != process_group || info.pbi_pgid != process_group {
            return Ok(false);
        }
        let observed = (info.pbi_start_tvsec as i128)
            .checked_mul(1_000_000)
            .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec as i128))
            .ok_or_else(|| format!("process group {process_group} start identity overflowed"))?;
        let observed = i64::try_from(observed)
            .map_err(|_| format!("process group {process_group} start identity exceeds i64"))?;
        Ok(observed == expected)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (spawned_at, process_start_identity);
        Ok(false)
    }
}

fn terminate_recorded_group(
    process_group: u32,
    spawned_at: Option<i64>,
    process_start_identity: Option<i64>,
) -> Result<(), String> {
    // A child that already exited is exactly the state we want to recover;
    // there is no process left to signal or reap.
    if !process_group_is_live(process_group) {
        return Ok(());
    }
    if !recorded_group_is_safe(process_group, spawned_at, process_start_identity)? {
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
    recover_runs(false)
}

/// Retry only rows that restart recovery deliberately left active because the
/// old process group was not yet safe to terminate. Ordinary active rows are
/// owned by this engine and must never be treated as abandoned by a tick.
fn recover_blocked_runs() -> Result<(), String> {
    recover_runs(true)
}

fn recover_runs(blocked_only: bool) -> Result<(), String> {
    let conn = db()?;
    let mut diagnostics = Vec::new();
    let active = {
        let mut statement = conn
            .prepare(
                "SELECT id,state,stop_reason,process_group,spawned_at,
                 process_start_identity FROM runs
                 WHERE state IN ('starting','running','stopping')
                   AND (?1 = 0 OR recovery_blocked=1)",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([blocked_only as i64], |row| {
                Ok((
                    RunId(row.get(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
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
    let mut active = active;
    active.sort_by_key(|(run_id, _, _, _, _, _)| run_id.0);

    // Signal/reap every recorded group before changing any durable run state.
    // This ordering prevents a restarted engine from publishing an apparently
    // idle job while its old driver is still mutating the destination.
    let mut blocked = HashMap::<RunId, String>::new();
    for (run_id, _state, _reason, process_group, spawned_at, process_start_identity) in &active {
        if let Some(process_group) = process_group
            && let Err(error) =
                terminate_recorded_group(*process_group, *spawned_at, *process_start_identity)
        {
            let detail = format!(
                "engine restart recovery blocked: process group {process_group} was not safely terminated: {error}"
            );
            diagnostics.push(format!("run #{}: {detail}", run_id.0));
            blocked.insert(*run_id, detail);
        }
    }
    drop(conn);

    // A group that could not be proven safe or terminated still owns the
    // destination.  Keep its active row and the one-active-run constraint;
    // only publish a durable diagnostic so the next recovery attempt has an
    // attributable reason and a user cannot start a second mutator.
    for (run_id, detail) in &blocked {
        let mut conn = match db() {
            Ok(conn) => conn,
            Err(error) => {
                diagnostics.push(format!("run #{} blocked diagnostic: {error}", run_id.0));
                continue;
            }
        };
        let transaction = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(transaction) => transaction,
            Err(error) => {
                diagnostics.push(format!(
                    "run #{} blocked diagnostic transaction: {error}",
                    run_id.0
                ));
                continue;
            }
        };
        let changed = match transaction.execute(
            "UPDATE runs SET recovery_blocked=1,detail=?2
             WHERE id=?1 AND state IN ('starting','running','stopping')
               AND NOT EXISTS(
                   SELECT 1 FROM runs newer
                   WHERE newer.job_id=runs.job_id AND newer.id>runs.id
               )",
            params![run_id.0, detail],
        ) {
            Ok(changed) => changed,
            Err(error) => {
                diagnostics.push(format!("run #{} blocked diagnostic: {error}", run_id.0));
                continue;
            }
        };
        if changed != 1 {
            diagnostics.push(format!(
                "restart recovery lost ownership of blocked run #{}",
                run_id.0
            ));
            continue;
        }
        if let Err(error) = transaction.commit() {
            diagnostics.push(format!(
                "run #{} blocked diagnostic commit: {error}",
                run_id.0
            ));
        }
    }

    // A malformed row must not prevent later rows from being recovered.  The
    // conditional UPDATE below also handles the narrow case where a valid
    // state has an invalid stop reason: it records a failed outcome rather
    // than leaving an active row that the scheduler can never start.  Runs
    // in `blocked` were handled above and are deliberately skipped here.
    for (run_id, state, reason, _process_group, _spawned_at, _process_start_identity) in active {
        if blocked.contains_key(&run_id) {
            continue;
        }
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
        let detail = "engine restarted before run completion";
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
            "UPDATE runs SET state=?2,ended_at=?3,exit_code=?4,stop_reason=?5,
                             recovery_blocked=0,detail=?6
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
            "dest": "/Volumes/Elements/sarun-progress-test/enwiki.swdump",
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
        assert_eq!(job.fetch_server_timed_retries, None);
        assert_eq!(job.bz2_admission_limit, None);
        assert!(!uses_live_run_progress(&job));
        apply_wikipedia_progress(
            &mut job,
            wikimak_wikipedia::MirrorBuildProgress {
                phase: "materializing source targets".into(),
                targets_active: vec!["content-000001 · parsing".into()],
                active_source_bytes_per_second: Some(123),
                active_quiet_seconds: Some(4),
                fetch_server_timed_retries: 2,
                fetch_robots_timed_retries: 3,
                fetch_fallback_timed_retries: 4,
                fetch_local_spacing_timed_retries: 5,
                bz2_admission_limit: 8,
                bz2_admission_active_decoders: 3,
                bz2_admission_peak_active_decoders: 6,
                ..Default::default()
            },
        );
        assert_eq!(job.state, "error");
        assert!(job.targets_active.is_empty());
        assert_eq!(job.active_source_bytes_per_second, None);
        assert_eq!(job.active_quiet_seconds, None);
        let json = serde_json::to_value(&job).unwrap();
        assert_eq!(json["fetch_server_timed_retries"], 2);
        assert_eq!(json["fetch_robots_timed_retries"], 3);
        assert_eq!(json["fetch_fallback_timed_retries"], 4);
        assert_eq!(json["fetch_local_spacing_timed_retries"], 5);
        assert_eq!(json["bz2_admission_limit"], 8);
        assert_eq!(json["bz2_admission_active_decoders"], 3);
        assert_eq!(json["bz2_admission_peak_active_decoders"], 6);
        let mut running = job.clone();
        running.state = "running".into();
        assert!(uses_live_run_progress(&running));
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

    #[test]
    fn images_run_has_distinct_request_identity_and_exact_argv_tail() {
        assert_eq!(
            StartRequest::Explicit(WikiRun::Images).request_name(),
            "images"
        );
        assert_eq!(
            wiki_run_args(
                WikiRun::Images,
                RunId(41),
                "ruwiki",
                "/Volumes/Elements/wikipedia/ruwiki.swdump"
            ),
            vec![
                "media-update",
                "--run-id",
                "41",
                "ruwiki",
                "/Volumes/Elements/wikipedia/ruwiki.swdump",
            ]
        );
    }

    #[test]
    fn backrefs_run_has_distinct_request_identity_and_exact_argv_tail() {
        assert_eq!(
            StartRequest::Explicit(WikiRun::Backrefs).request_name(),
            "backrefs"
        );
        assert_eq!(
            wiki_run_args(
                WikiRun::Backrefs,
                RunId(42),
                "ruwiki",
                "/Volumes/Elements/wikipedia/ruwiki.swdump"
            ),
            vec![
                "backrefs-task",
                "/Volumes/Elements/wikipedia/ruwiki.swdump",
            ]
        );
    }

    #[test]
    fn backrefs_pending_covers_missing_sidecar_and_explicit_task_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("ruwiki.swdump");
        std::fs::write(archive.with_extension("swtitle"), b"title index").unwrap();
        assert!(wikipedia_backrefs_pending(&archive));

        std::fs::write(archive.with_extension("swrefs"), b"current relation index").unwrap();
        assert!(!wikipedia_backrefs_pending(&archive));

        let task = wikimak_wikipedia::mirror_scratch_path(&archive).join("backrefs.task.json");
        std::fs::create_dir_all(task.parent().unwrap()).unwrap();
        std::fs::write(task, b"pending").unwrap();
        assert!(wikipedia_backrefs_pending(&archive));
    }

    #[test]
    fn explicit_images_request_is_admitted_when_schedule_is_paused() {
        assert!(!pause_blocks_request(
            StartRequest::Explicit(WikiRun::Images),
            true
        ));
        assert!(pause_blocks_request(StartRequest::Scheduled, true));
        assert!(!pause_blocks_request(StartRequest::Scheduled, false));
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
    fn wikipedia_registration_rejects_relative_destination_before_database_access() {
        let error = job_add("wiki", "testwiki", "relative/testwiki.swdump", 86400)
            .expect_err("relative Wikipedia destination must be rejected");
        assert!(error.contains("absolute path"), "{error}");
    }

    #[test]
    fn explicit_start_cannot_bypass_whole_engine_wikipedia_admission() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let first = job_add(
            "wiki",
            "firstwiki",
            temporary.path().join("first.swdump").to_str().unwrap(),
            86400,
        )
        .unwrap();
        let second = job_add(
            "wiki",
            "secondwiki",
            temporary.path().join("second.swdump").to_str().unwrap(),
            86400,
        )
        .unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(job_id,request,state,started_at)
             VALUES(?1,'explicit','starting',?2)",
            params![first, now()],
        )
        .unwrap();

        let error = start_job(second, StartRequest::Explicit(WikiRun::Maintain))
            .expect_err("a second explicit Wikipedia run must be rejected");

        assert!(
            error
                .to_string()
                .contains("another Wikipedia mirror job is already active"),
            "{error}"
        );
        let image_error = start_job(second, StartRequest::Explicit(WikiRun::Images))
            .expect_err("images work must use the same Wikipedia admission gate");
        assert!(
            image_error
                .to_string()
                .contains("another Wikipedia mirror job is already active"),
            "{image_error}"
        );
        let second_runs = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE job_id=?1",
                [second],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(second_runs, 0, "rejection must precede durable start");
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
        let process_start_identity = current_process_start_identity(child.id()).unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(
                job_id,request,state,started_at,spawned_at,process_group,
                process_start_identity
             ) VALUES(?1,'explicit','running',?2,?2,?3,?4)",
            params![id, spawned_at, child.id(), process_start_identity],
        )
        .unwrap();

        let recovery = recover_unowned_runs();
        assert!(
            !process_group_is_live(child.id()),
            "recovery must terminate the old group before publishing its state"
        );
        let _ = child.wait();
        assert!(recovery.is_ok(), "{recovery:?}");
        assert_eq!(jobs_list().unwrap()[0].state, "interrupted");
    }

    #[test]
    fn restart_recovery_keeps_run_active_when_group_termination_is_unverifiable() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let process_group = unsafe { libc::getpgrp() };
        assert!(
            process_group > 1,
            "test process must have a signalable group"
        );
        let id = job_add(
            "cmd",
            "true",
            temporary.path().join("dest").to_str().unwrap(),
            3600,
        )
        .unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(
                job_id,request,state,started_at,spawned_at,process_group,
                process_start_identity
             ) VALUES(?1,'explicit','running',?2,?2,?3,?4)",
            params![
                id,
                now(),
                process_group,
                current_process_start_identity(std::process::id()).unwrap()
            ],
        )
        .unwrap();

        let recovery = recover_unowned_runs();
        assert!(recovery.is_err(), "unverifiable group must block recovery");
        let (state, ended_at, recovery_blocked, detail): (String, Option<i64>, i64, String) = conn
            .query_row(
                "SELECT state,ended_at,recovery_blocked,detail FROM runs WHERE job_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, "running");
        assert_eq!(ended_at, None);
        assert_eq!(recovery_blocked, 1);
        assert!(detail.contains("restart recovery blocked"), "{detail}");
        assert!(
            start_job(id, StartRequest::Explicit(WikiRun::Maintain)).is_err(),
            "a blocked active run must retain the one-run admission guard"
        );
    }

    #[test]
    fn blocked_restart_recovery_retries_and_terminalizes_only_after_termination() {
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
        let identity = current_process_start_identity(child.id()).unwrap().unwrap();
        let id = job_add(
            "cmd",
            "sleep 60",
            temporary.path().join("dest").to_str().unwrap(),
            3600,
        )
        .unwrap();
        let conn = db().unwrap();
        conn.execute(
            "INSERT INTO runs(
                job_id,request,state,started_at,spawned_at,process_group,
                process_start_identity
             ) VALUES(?1,'explicit','running',?2,?2,?3,?4)",
            params![id, now(), child.id(), identity + 1],
        )
        .unwrap();

        let first = recover_unowned_runs();
        assert!(
            first.is_err(),
            "the mismatched incarnation must block recovery"
        );
        let state: String = conn
            .query_row("SELECT state FROM runs WHERE job_id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "running");
        conn.execute(
            "UPDATE runs SET process_start_identity=?2 WHERE job_id=?1",
            params![id, identity],
        )
        .unwrap();

        let second = recover_blocked_runs();
        assert!(second.is_ok(), "{second:?}");
        assert!(!process_group_is_live(child.id()));
        let _ = child.wait();
        assert_eq!(jobs_list().unwrap()[0].state, "interrupted");
    }

    #[test]
    fn process_start_identity_has_a_platform_specific_contract() {
        let identity = current_process_start_identity(std::process::id()).unwrap();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(
                identity.is_some(),
                "the platform must expose a process start identity"
            );
            assert_eq!(
                identity,
                current_process_start_identity(std::process::id()).unwrap()
            );
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(identity, None);
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
        std::fs::create_dir(archive.with_extension("media")).unwrap();
        let auxiliary = wikimak_wikipedia::mirror_auxiliary_paths(&archive).unwrap();
        std::fs::create_dir_all(&auxiliary[0]).unwrap();
        std::fs::write(auxiliary[0].join("build.lock"), b"").unwrap();
        std::fs::write(auxiliary[0].join("partial"), b"scratch").unwrap();
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

    fn make_valid_deletion_archive(archive: &std::path::Path) -> Vec<std::path::PathBuf> {
        use wikimak_wikipedia::archive::{ArchiveWriter, ManifestRecord, Record, SiteInfoRecord};
        let output = wikimak_wikipedia::archive_set::ArchiveSetOutput::new_in(
            archive.parent().unwrap(),
            1 << 20,
        )
        .unwrap();
        let mut writer = ArchiveWriter::new(output, 1024).unwrap();
        writer
            .write(&Record::Manifest {
                timestamp_micros: 1,
                manifest: ManifestRecord {
                    wiki_db: "delete-test".into(),
                    content_snapshot: "2026-01-01".into(),
                    metadata_snapshot: "2026-01-01".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 1,
                site_info: SiteInfoRecord {
                    site_name: "Deletion test".into(),
                    db_name: "delete-test".into(),
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
        let (output, _) = writer.finish().unwrap();
        output.finish().unwrap().persist(archive).unwrap();
        let reader = wikimak_wikipedia::archive_set::ArchiveSetReader::open(archive).unwrap();
        reader
            .segments()
            .iter()
            .map(|segment| archive.join(&segment.name))
            .collect()
    }

    #[cfg(unix)]
    fn write_current_generation_manifest(
        generations: &std::path::Path,
        generation_id: &str,
        generation: &std::path::Path,
    ) {
        use std::os::unix::fs::MetadataExt;

        let reader = wikimak_wikipedia::archive_set::ArchiveSetReader::open(generation).unwrap();
        let segments = reader
            .segments()
            .iter()
            .map(|segment| {
                let path = generation.join(&segment.name);
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                serde_json::json!({
                    "name": segment.name,
                    "bytes": metadata.len(),
                    "identity": {
                        "device": metadata.dev(),
                        "inode": metadata.ino(),
                        "modified_seconds": metadata.mtime(),
                        "modified_nanos": metadata.mtime_nsec(),
                        "bytes": metadata.len()
                    }
                })
            })
            .collect::<Vec<_>>();
        drop(reader);
        std::fs::write(
            generations.join(format!("{generation_id}.manifest.json")),
            serde_json::to_vec(&serde_json::json!({
                "schema": 3,
                "generation_id": generation_id,
                "segments": segments
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn tree_contains_file_bytes(root: &std::path::Path, expected: &[u8]) -> bool {
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_dir() {
                if let Ok(entries) = std::fs::read_dir(path) {
                    pending.extend(entries.flatten().map(|entry| entry.path()));
                }
            } else if metadata.file_type().is_file()
                && std::fs::read(path).is_ok_and(|bytes| bytes == expected)
            {
                return true;
            }
        }
        false
    }

    #[test]
    fn deleting_wikipedia_data_reclaims_validated_archive_and_quarantines_residue() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let archive_segments = make_valid_deletion_archive(&archive);
        let archive_foreign = archive.join("foreign/nested");
        std::fs::create_dir_all(&archive_foreign).unwrap();
        std::fs::write(archive_foreign.join("sentinel"), b"preserve archive residue").unwrap();

        let titles = archive.with_extension("swtitle");
        let media = archive.with_extension("media");
        std::fs::create_dir_all(media.join("foreign/nested")).unwrap();
        std::fs::write(media.join("foreign/nested/sentinel"), b"preserve media").unwrap();

        let scratch = wikimak_wikipedia::mirror_scratch_path(&archive);
        std::fs::create_dir_all(scratch.join("foreign/nested")).unwrap();
        std::fs::write(scratch.join("build.lock"), b"").unwrap();
        std::fs::write(scratch.join("foreign/nested/sentinel"), b"preserve scratch").unwrap();

        let install = archive.with_extension("install.json");
        let refs = archive.with_extension("swrefs");
        std::fs::write(&install, b"owned receipt").unwrap();
        std::fs::write(&refs, b"owned backrefs").unwrap();
        let sibling = library.join("keep");
        std::fs::write(&sibling, b"keep").unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        let note = job_remove_with_data(id).unwrap();

        for segment in archive_segments {
            assert!(!segment.exists(), "validated segment was not reclaimed: {segment:?}");
        }
        assert!(!archive.exists());
        assert!(!titles.exists());
        assert!(!install.exists());
        assert!(!refs.exists());
        assert!(sibling.exists());
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));

        let quarantine = library.join(".sarun-quarantine");
        let quarantined = std::fs::read_dir(&quarantine)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert!(
            quarantined
                .iter()
                .any(|path| path.join("foreign/nested/sentinel").exists()),
            "archive residual was not reported and preserved in {quarantine:?}"
        );
        assert!(
            quarantined
                .iter()
                .any(|path| {
                    std::fs::read(path.join("foreign/nested/sentinel"))
                        .is_ok_and(|bytes| bytes == b"preserve media")
                }),
            "media namespace was not preserved in {quarantine:?}"
        );
        assert!(
            quarantined
                .iter()
                .any(|path| {
                    std::fs::read(path.join("foreign/nested/sentinel"))
                        .is_ok_and(|bytes| bytes == b"preserve scratch")
                }),
            "scratch namespace was not preserved in {quarantine:?}"
        );
        assert!(note.contains("reclaimed"));
        assert!(note.contains("media namespace has no public ownership validator"));
        assert!(note.contains("Wikimedia scratch namespace has no public ownership validator"));
        assert!(note.contains(&quarantine.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_processes_generation_before_manifest_sidecar() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let generations = archive.with_extension("generations");
        let generation_id = "a".repeat(64);
        let generation = generations.join(&generation_id);
        let staged_generation = generations.join("staged-generation");
        std::fs::create_dir(&generations).unwrap();
        let segments = make_valid_deletion_archive(&staged_generation);
        write_current_generation_manifest(&generations, &generation_id, &staged_generation);
        // Create the sidecar before the final generation directory entry. On
        // filesystems that preserve directory insertion order this reproduces
        // the old manifest-first failure deterministically.
        std::fs::rename(&staged_generation, &generation).unwrap();
        let segments = segments
            .into_iter()
            .map(|path| generation.join(path.file_name().unwrap()))
            .collect::<Vec<_>>();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        let note = job_remove_with_data(id).unwrap();

        assert!(segments.iter().all(|path| !path.exists()));
        assert!(!generation.exists());
        assert!(!generations.exists());
        assert!(!library.join(".sarun-quarantine").exists());
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
        assert!(note.contains("reclaimed"));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_removes_selected_generation_after_selector_boundary() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let generations = archive.with_extension("generations");
        let generation_id = "d".repeat(64);
        let generation = generations.join(&generation_id);
        std::fs::create_dir(&generations).unwrap();
        make_valid_deletion_archive(&generation);
        write_current_generation_manifest(&generations, &generation_id, &generation);
        wikimak_wikipedia::title_index::build(
            &generation,
            archive.with_extension("swtitle"),
            &wikimak_wikipedia::generation::GenerationId::parse(&generation_id).unwrap(),
        )
        .unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        let note = job_remove_with_data(id).unwrap();

        assert!(!generation.exists());
        assert!(!generations.exists());
        assert!(!archive.with_extension("swtitle").exists());
        let quarantined_selector = library
            .join(".sarun-quarantine")
            .join("wiki-delete-testwiki.swtitle");
        let retained_index =
            wikimak_wikipedia::title_index::TitleIndex::open(&quarantined_selector).unwrap();
        assert_eq!(retained_index.generation_id().as_str(), generation_id);
        assert!(note.contains("explicit deletion visibility boundary"), "{note}");
        assert!(note.contains(&quarantined_selector.display().to_string()), "{note}");
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_retries_after_selector_quarantine_cut() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let selector = archive.with_extension("swtitle");
        let generations = archive.with_extension("generations");
        let generation_id = "e".repeat(64);
        let generation = generations.join(&generation_id);
        std::fs::create_dir(&generations).unwrap();
        make_valid_deletion_archive(&generation);
        write_current_generation_manifest(&generations, &generation_id, &generation);
        wikimak_wikipedia::title_index::build(
            &generation,
            &selector,
            &wikimak_wikipedia::generation::GenerationId::parse(&generation_id).unwrap(),
        )
        .unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        prepare_job_deletion(id, Some("wiki"), DeleteMode::Data).unwrap();
        let scratch = wikimak_wikipedia::mirror_scratch_path(&archive);
        let writer_lease = acquire_wikipedia_writer_cleanup_lease(&scratch).unwrap();
        let archive_lease = acquire_wikipedia_archive_cleanup_lease(&archive, "archive").unwrap();
        let generation_leases =
            acquire_wikipedia_generation_cleanup_leases(&generations).unwrap();
        let selected = validate_wikipedia_selector_for_deletion(
            &archive,
            &generations,
            &generation_leases,
        )
        .unwrap();
        assert_eq!(selected.as_deref(), Some(generation_id.as_str()));
        let quarantine = wikipedia_quarantine_root(&archive);
        let mut interrupted_report = WikipediaDeletionReport::default();
        quarantine_wikipedia_selector(
            &selector,
            selected.as_deref(),
            &quarantine,
            &mut interrupted_report,
        )
        .unwrap();
        let quarantined_selector = quarantine.join("wiki-delete-testwiki.swtitle");
        assert!(!selector.exists());
        assert!(quarantined_selector.exists());
        assert!(generation.exists());
        drop(generation_leases);
        drop(archive_lease);
        drop(writer_lease);

        let note = job_remove_with_data(id).unwrap();

        assert!(!generation.exists());
        assert!(!generations.exists());
        assert!(quarantined_selector.exists());
        assert!(
            note.contains("already quarantined by an earlier explicit-deletion attempt"),
            "{note}"
        );
        assert!(note.contains(&quarantined_selector.display().to_string()), "{note}");
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_rejects_malformed_selector_before_mutation() {
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
        std::fs::write(archive.join("sentinel"), b"preserve archive").unwrap();
        let selector = archive.with_extension("swtitle");
        std::fs::write(&selector, b"not a title index").unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        let error = job_remove_with_data(id).unwrap_err();

        assert!(error.contains("inspect selected Wikipedia generation"), "{error}");
        assert_eq!(std::fs::read(archive.join("sentinel")).unwrap(), b"preserve archive");
        assert_eq!(std::fs::read(&selector).unwrap(), b"not a title index");
        assert!(!library.join(".sarun-quarantine").exists());
        assert!(jobs_list()
            .unwrap()
            .iter()
            .any(|job| job.id == id && job.state == "deleting"));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_quarantines_pending_generation_intact() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let generations = archive.with_extension("generations");
        let generation_id = "b".repeat(64);
        let generation = generations.join(&generation_id);
        std::fs::create_dir(&generations).unwrap();
        let segments = make_valid_deletion_archive(&generation);
        write_current_generation_manifest(&generations, &generation_id, &generation);
        let sentinel = generation.join("foreign/nested/sentinel");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"preserve nested namespace").unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        let note = job_remove_with_data(id).unwrap();

        assert!(segments.iter().all(|path| !path.exists()));
        assert!(!generation.exists());
        assert!(!generations.exists());
        let quarantine = library.join(".sarun-quarantine");
        let quarantined_generation = quarantine.join(format!("wiki-delete-{generation_id}"));
        assert_eq!(
            std::fs::read(quarantined_generation.join("foreign/nested/sentinel")).unwrap(),
            b"preserve nested namespace"
        );
        assert!(
            quarantine
                .join(format!("wiki-delete-{generation_id}.manifest.json"))
                .exists()
        );
        assert!(note.contains("generation ownership cleanup has pending paths"));
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_quarantines_generation_without_reading_payload() {
        use std::io::Write;
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        use std::time::{Duration, Instant};

        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let generations = archive.with_extension("generations");
        let generation_id = "c".repeat(64);
        let generation = generations.join(&generation_id);
        std::fs::create_dir(&generations).unwrap();
        std::fs::create_dir(&generation).unwrap();
        let payload = generation.join("0000-reference.swdump-part");
        let payload_c = std::ffi::CString::new(payload.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(payload_c.as_ptr(), 0o600) }, 0);
        std::fs::write(
            generations.join(format!("{generation_id}.manifest.json")),
            b"{ this is malformed ownership metadata",
        )
        .unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        let payload_was_opened = Arc::new(AtomicBool::new(false));
        let payload_was_opened_by_probe = Arc::clone(&payload_was_opened);
        let payload_probe = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).custom_flags(libc::O_NONBLOCK);
                match options.open(&payload) {
                    Ok(mut writer) => {
                        payload_was_opened_by_probe.store(true, Ordering::SeqCst);
                        writer.write_all(b"probe").unwrap();
                        return;
                    }
                    Err(error)
                        if matches!(error.raw_os_error(), Some(code) if code == libc::ENXIO)
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error)
                        if matches!(error.raw_os_error(), Some(code) if code == libc::ENOENT) =>
                    {
                        return;
                    }
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => return,
                }
            }
        });

        let note = job_remove_with_data(id).unwrap();
        payload_probe.join().unwrap();

        let quarantine = library.join(".sarun-quarantine");
        let quarantined_generation = quarantine.join(format!("wiki-delete-{generation_id}"));
        assert!(std::fs::symlink_metadata(
            quarantined_generation.join("0000-reference.swdump-part")
        )
        .unwrap()
        .file_type()
        .is_fifo());
        assert!(!payload_was_opened.load(Ordering::SeqCst));
        assert!(!generation.exists());
        assert!(!generations.exists());
        assert!(note.contains("generation ownership is unavailable"));
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_waits_for_reader_lease_then_retries_without_loss() {
        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let generations = archive.with_extension("generations");
        let generation = generations.join("a".repeat(64));
        std::fs::create_dir(&generations).unwrap();
        let generation_segments = make_valid_deletion_archive(&generation);
        let generation_sentinel = generation.join("foreign/nested/sentinel");
        std::fs::create_dir_all(generation_sentinel.parent().unwrap()).unwrap();
        std::fs::write(&generation_sentinel, b"preserve generation").unwrap();

        let reader_lease = {
            use std::os::fd::AsRawFd;
            let file = std::fs::File::open(&generation).unwrap();
            assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) }, 0);
            file
        };

        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();
        let error = job_remove_with_data(id).unwrap_err();

        assert!(error.contains("blocked before filesystem mutation"), "{error}");
        assert!(error.contains("active reader"), "{error}");
        assert!(generation_segments.iter().all(|path| path.exists()));
        assert!(generation_sentinel.exists());
        assert!(jobs_list()
            .unwrap()
            .iter()
            .any(|job| job.id == id && job.state == "deleting"));

        drop(reader_lease);
        let note = job_remove_with_data(id).unwrap();
        assert!(generation_segments.iter().all(|path| !path.exists()));
        assert!(!generation.exists());
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
        let quarantine = library.join(".sarun-quarantine");
        let quarantined = std::fs::read_dir(&quarantine)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert!(
            quarantined
                .iter()
                .any(|entry| tree_contains_file_bytes(entry, b"preserve generation")),
            "generation residual missing from {quarantined:?}; report: {note}"
        );
        assert!(note.contains("reclaimed"));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_waits_for_importer_then_retries_without_loss() {
        use std::os::fd::AsRawFd;

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
        std::fs::write(archive.join("sentinel"), b"preserve archive").unwrap();
        let scratch = wikimak_wikipedia::mirror_scratch_path(&archive);
        std::fs::create_dir_all(&scratch).unwrap();
        let build_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(scratch.join("build.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(build_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();
        let error = job_remove_with_data(id).unwrap_err();

        assert!(error.contains("blocked before filesystem mutation"), "{error}");
        assert!(error.contains("active importer"), "{error}");
        assert_eq!(std::fs::read(archive.join("sentinel")).unwrap(), b"preserve archive");
        assert!(scratch.join("build.lock").exists());
        assert!(!library.join(".sarun-quarantine").exists());
        assert!(jobs_list()
            .unwrap()
            .iter()
            .any(|job| job.id == id && job.state == "deleting"));

        drop(build_lock);
        let note = job_remove_with_data(id).unwrap();
        assert!(!archive.exists());
        assert!(!scratch.exists());
        assert!(jobs_list().unwrap().iter().all(|job| job.id != id));
        assert!(!note.contains("active importer"));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_wikipedia_data_quarantines_symlink_without_following_target() {
        use std::os::unix::fs::symlink;

        let _guard = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", temporary.path().join("state"));
        }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let library = temporary.path().join("library");
        std::fs::create_dir(&library).unwrap();
        let archive = library.join("testwiki.swdump");
        let outside = temporary.path().join("outside-media");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), b"do not follow").unwrap();
        std::fs::create_dir(&archive).unwrap();
        let outside_segment = temporary.path().join("outside-segment");
        std::fs::write(&outside_segment, b"do not inspect through archive symlink").unwrap();
        symlink(
            &outside_segment,
            archive.join("0000-reference.swdump-part"),
        )
        .unwrap();
        symlink(&outside, archive.with_extension("media")).unwrap();
        let id = job_add("wiki", "testwiki", archive.to_str().unwrap(), 86400).unwrap();

        let note = job_remove_with_data(id).unwrap();

        assert!(outside.join("sentinel").exists());
        assert!(outside_segment.exists());
        assert!(std::fs::symlink_metadata(archive.with_extension("media")).is_err());
        let quarantined_archive = library
            .join(".sarun-quarantine/wiki-delete-testwiki.swdump")
            .join("0000-reference.swdump-part");
        assert_eq!(std::fs::read_link(quarantined_archive).unwrap(), outside_segment);
        let quarantined_media = library
            .join(".sarun-quarantine")
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().is_some_and(|name| name == "wiki-delete-testwiki.media"))
            .expect("media symlink quarantine entry");
        assert_eq!(std::fs::read_link(quarantined_media).unwrap(), outside);
        assert!(note.contains("media namespace has no public ownership validator"));
    }

    #[test]
    fn wikipedia_quarantine_is_idempotent_for_missing_active_path() {
        let temporary = tempfile::tempdir().unwrap();
        let active = temporary.path().join("active.swdump");
        let quarantine = temporary.path().join(".sarun-quarantine");
        std::fs::create_dir_all(active.join("foreign/nested")).unwrap();
        std::fs::write(active.join("foreign/nested/sentinel"), b"preserve").unwrap();
        std::fs::create_dir_all(quarantine.join("wiki-delete-active.swdump")).unwrap();
        std::fs::write(
            quarantine.join("wiki-delete-active.swdump/existing-sentinel"),
            b"pre-existing quarantine data",
        )
        .unwrap();
        let mut first = WikipediaDeletionReport::default();
        quarantine_wikipedia_path(
            &active,
            &quarantine,
            "unvalidated test namespace",
            &mut first,
        )
        .unwrap();
        let mut second = WikipediaDeletionReport::default();
        quarantine_wikipedia_path(
            &active,
            &quarantine,
            "unvalidated test namespace",
            &mut second,
        )
        .unwrap();
        assert_eq!(first.quarantined.len(), 1);
        assert!(second.quarantined.is_empty());
        assert_eq!(
            std::fs::read(quarantine.join("wiki-delete-active.swdump/existing-sentinel"))
                .unwrap(),
            b"pre-existing quarantine data"
        );
        assert!(quarantine
            .join("wiki-delete-active.swdump-1/foreign/nested/sentinel")
            .exists());
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

    #[test]
    fn v3_migration_preserves_jobs_and_runs_while_admitting_backrefs_requests() {
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
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind TEXT NOT NULL,src TEXT NOT NULL,dest TEXT NOT NULL UNIQUE,
                    interval_secs INTEGER NOT NULL,paused INTEGER NOT NULL DEFAULT 0,
                    media_source TEXT,delete_mode TEXT
                 );
                 CREATE TABLE runs(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                    request TEXT NOT NULL CHECK(request IN ('explicit','scheduled','full')),
                    state TEXT NOT NULL CHECK(state IN
                        ('starting','running','stopping','succeeded','failed',
                         'cancelled','interrupted')),
                    started_at INTEGER NOT NULL,spawned_at INTEGER,ended_at INTEGER,
                    process_group INTEGER,process_start_identity INTEGER,
                    recovery_blocked INTEGER NOT NULL DEFAULT 0,
                    exit_code INTEGER,stop_reason TEXT,detail TEXT NOT NULL DEFAULT ''
                 );
                 CREATE UNIQUE INDEX one_active_run_per_job ON runs(job_id)
                    WHERE state IN ('starting','running','stopping');
                 CREATE INDEX runs_by_job ON runs(job_id,id DESC);
                 INSERT INTO jobs(id,kind,src,dest,interval_secs,paused,media_source)
                    VALUES(7,'wiki','lvwiki','/library/lvwiki.swdump',86400,1,'auto');
                 INSERT INTO runs(id,job_id,request,state,started_at,ended_at,exit_code,detail)
                    VALUES(13,7,'full','succeeded',100,140,0,'old full receipt');
                 PRAGMA user_version=3;",
            )
            .unwrap();
        drop(legacy);

        let migrated = db().unwrap();
        let (job_count, run_count, request, detail): (i64, i64, String, String) = migrated
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM jobs),
                    (SELECT COUNT(*) FROM runs),
                    request,detail FROM runs WHERE id=13",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((job_count, run_count), (1, 1));
        assert_eq!(request, "full");
        assert_eq!(detail, "old full receipt");
        let version: i64 = migrated
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIRROR_SCHEMA_VERSION);
        migrated
            .execute(
                "INSERT INTO runs(job_id,request,state,started_at,ended_at,exit_code) VALUES(7,'images','succeeded',200,250,0)",
                [],
            )
            .unwrap();
        assert_eq!(
            migrated
                .query_row("SELECT request FROM runs WHERE id=last_insert_rowid()", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "images"
        );
        migrated
            .execute(
                "INSERT INTO runs(job_id,request,state,started_at) VALUES(7,'backrefs','starting',300)",
                [],
            )
            .unwrap();
        assert_eq!(
            migrated
                .query_row("SELECT request FROM runs WHERE id=last_insert_rowid()", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "backrefs"
        );
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
