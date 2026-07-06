//! Mirror-update jobs: the engine-side schedule for `gitdepot mirror` /
//! `wikimak fetch` / `ietfmak update` runs (MIRRORS.md "Update").
//!
//! The engine SCHEDULES; the drivers FETCH. The drivers are compiled into
//! the sarun binary itself (multi-call dispatch in main.rs), so a job run
//! spawns the engine's OWN binary with the driver name as the subcommand.
//! The child is a separate host process: the engine-never-dials-out
//! property is a PROCESS property — the HTTP stack only ever runs in the
//! spawned child, never in the engine's address space. Moving those
//! spawns into tap boxes later is mechanical.
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

use rusqlite::{params, Connection};

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn db() -> Result<Connection, String> {
    let path = crate::paths::state_home().join("mirrors.db");
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
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
            last_detail TEXT NOT NULL DEFAULT ''
        )",
        [],
    ).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Jobs whose driver process is live right now: id → pid.
static RUNNING: Mutex<Option<HashMap<i64, u32>>> = Mutex::new(None);

fn running_map<R>(f: impl FnOnce(&mut HashMap<i64, u32>) -> R) -> R {
    let mut g = RUNNING.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

#[derive(Debug, Clone, serde::Serialize)]
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
    /// Derived: running | paused | pending | stopped | error | completed
    /// | scheduled (never-ran pending shows as pending too).
    pub state: String,
    /// Unix seconds of the next auto run (None while paused/running).
    pub next_due: Option<i64>,
}

fn derive(mut j: Job) -> Job {
    let live = running_map(|m| m.contains_key(&j.id));
    let due_at = j.last_start.map(|s| s + j.interval_secs);
    j.state = if live {
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
    j.next_due = if j.paused || live { None } else { due_at.or(Some(now())) };
    j
}

pub fn jobs_list() -> Result<Vec<Job>, String> {
    let conn = db()?;
    let mut st = conn
        .prepare("SELECT id,kind,src,dest,interval_secs,paused,last_start,last_end,last_exit,last_detail FROM jobs ORDER BY id")
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
                state: String::new(),
                next_due: None,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().map(derive).collect())
}

pub fn job_add(kind: &str, src: &str, dest: &str, interval_secs: i64) -> Result<i64, String> {
    if !matches!(kind, "git" | "wiki" | "ietf" | "cmd") {
        return Err(format!("unknown mirror kind {kind:?} (git|wiki|ietf|cmd)"));
    }
    let conn = db()?;
    conn.execute(
        "INSERT INTO jobs(kind, src, dest, interval_secs) VALUES(?1,?2,?3,?4)",
        params![kind, src, dest, interval_secs.max(60)],
    ).map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub fn job_remove(id: i64) -> Result<(), String> {
    if running_map(|m| m.contains_key(&id)) {
        return Err("job is running".into());
    }
    let n = db()?.execute("DELETE FROM jobs WHERE id = ?1", [id]).map_err(|e| e.to_string())?;
    if n == 0 { Err("no such job".into()) } else { Ok(()) }
}

pub fn job_set_paused(id: i64, paused: bool) -> Result<(), String> {
    let n = db()?
        .execute("UPDATE jobs SET paused = ?2 WHERE id = ?1", params![id, paused as i64])
        .map_err(|e| e.to_string())?;
    if n == 0 { Err("no such job".into()) } else { Ok(()) }
}

/// Force-run one job NOW (also works on paused jobs — force is force).
/// Returns immediately; the run is a background thread + child process.
pub fn job_run(id: i64) -> Result<(), String> {
    let jobs = jobs_list()?;
    let job = jobs.into_iter().find(|j| j.id == id).ok_or("no such job")?;
    if running_map(|m| m.contains_key(&id)) {
        return Err("job is already running".into());
    }
    spawn_run(job);
    Ok(())
}

/// Start every due, unpaused, not-running job. Returns the started ids.
pub fn run_pending() -> Result<Vec<i64>, String> {
    let mut started = Vec::new();
    for j in jobs_list()? {
        if j.state == "pending" || j.state == "stopped" {
            let id = j.id;
            spawn_run(j);
            started.push(id);
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

fn spawn_run(job: Job) {
    let driver = |name: &str| driver_argv(name, std::env::current_exe().ok());
    let argv: Vec<String> = match job.kind.as_str() {
        "git" => [driver("gitdepot"),
                  vec!["mirror".into(), job.src.clone(), job.dest.clone()]].concat(),
        "wiki" => [driver("wikimak"),
                   vec!["fetch".into(), job.src.clone(), job.dest.clone()]].concat(),
        // The plugin seam (gimir/PLUGINS.md): src IS the command line,
        // dest arrives as $1 (and $SARUN_MIRROR_DEST) — any script gets
        // the full job state machine without touching the engine.
        "cmd" => vec!["/bin/sh".into(), "-c".into(), job.src.clone(),
                      "mirror-job".into(), job.dest.clone()],
        _ => [driver("ietfmak"), vec!["update".into(), job.dest.clone()]].concat(),
    };
    let id = job.id;
    // Reserve the running slot BEFORE anything else — the scheduler
    // tick and a force-run can race to here; exactly one proceeds.
    if !running_map(|m| {
        if m.contains_key(&id) { false } else { m.insert(id, 0); true }
    }) {
        return;
    }
    if let Ok(conn) = db() {
        let _ = conn.execute(
            "UPDATE jobs SET last_start = ?2, last_end = NULL, last_exit = NULL WHERE id = ?1",
            params![id, now()],
        );
    }
    std::thread::spawn(move || {
        let child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("SARUN_MIRROR_DEST", &job.dest)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let (exit, detail) = match child {
            Ok(c) => {
                running_map(|m| { m.insert(id, c.id()); });
                let out = c.wait_with_output();
                match out {
                    Ok(o) => {
                        let tail = String::from_utf8_lossy(&o.stderr);
                        let tail = &tail[tail.len().saturating_sub(2048)..];
                        (o.status.code().unwrap_or(-1) as i64, tail.to_string())
                    }
                    Err(e) => (-1, e.to_string()),
                }
            }
            // Name the resolution that failed: an absolute argv[0] is the
            // self-exec path; a bare name means the PATH fallback.
            Err(e) => (-1, format!(
                "spawn {} ({}): {e}", argv[0],
                if argv[0].starts_with('/') { "self-exec" } else { "via PATH" })),
        };
        running_map(|m| { m.remove(&id); });
        if let Ok(conn) = db() {
            let _ = conn.execute(
                "UPDATE jobs SET last_end = ?2, last_exit = ?3, last_detail = ?4 WHERE id = ?1",
                params![id, now(), exit, detail],
            );
        }
    });
}

/// The scheduler tick loop: every minute, start whatever is due. Runs
/// for the life of the engine; jobs only exist if the user added them.
pub fn scheduler_thread() {
    std::thread::spawn(|| loop {
        let _ = run_pending();
        std::thread::sleep(std::time::Duration::from_secs(60));
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
        assert_eq!(driver_argv("gitdepot", Some(exe)),
                   vec!["/opt/sarun/bin/sarun".to_string(), "gitdepot".to_string()]);
        assert_eq!(driver_argv("ietfmak", None), vec!["ietfmak".to_string()]);
    }

    /// The argv prefix must compose with a subcommand tail exactly the way
    /// spawn_run builds it: [exe, driver, verb, args...].
    #[test]
    fn driver_argv_composes_with_the_subcommand_tail() {
        let argv = [driver_argv("wikimak", Some("/x/sarun".into())),
                    vec!["fetch".into(), "enwiki".into(), "/depot/w".into()]].concat();
        assert_eq!(argv, ["/x/sarun", "wikimak", "fetch", "enwiki", "/depot/w"]
                   .map(String::from).to_vec());
    }
}
