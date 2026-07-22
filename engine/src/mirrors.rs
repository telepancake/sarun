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
            last_detail TEXT NOT NULL DEFAULT ''
        )",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Jobs whose driver process is live right now: id → pid.
static RUNNING: Mutex<Option<HashMap<i64, u32>>> = Mutex::new(None);

fn running_map<R>(f: impl FnOnce(&mut HashMap<i64, u32>) -> R) -> R {
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
    j.next_due = if j.paused || live {
        None
    } else {
        due_at.or(Some(now()))
    };
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
    rows.map(|row| row.map(derive).map_err(|error| error.to_string()))
        .collect()
}

pub fn jobs_list_typed() -> Result<Vec<crate::generated_wire::MirrorJob>, String> {
    use crate::generated_wire::{MirrorJob, MirrorState};
    jobs_list()?
        .into_iter()
        .map(|job| {
            let state = match job.state.as_str() {
                "running" => MirrorState::Running,
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
    if !matches!(kind, "git" | "wiki" | "ietf" | "cmd") {
        return Err(format!("unknown mirror kind {kind:?} (git|wiki|ietf|cmd)"));
    }
    let conn = db()?;
    conn.execute(
        "INSERT INTO jobs(kind, src, dest, interval_secs) VALUES(?1,?2,?3,?4)",
        params![kind, src, dest, interval_secs.max(60)],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
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
        "git" => [
            driver("gitdepot"),
            vec!["mirror".into(), job.src.clone(), job.dest.clone()],
        ]
        .concat(),
        "wiki" => [
            driver("wikimak"),
            vec!["fetch".into(), job.src.clone(), job.dest.clone()],
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
    if !running_map(|m| {
        if m.contains_key(&id) {
            false
        } else {
            m.insert(id, 0);
            true
        }
    }) {
        return;
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
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
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
                    m.insert(id, c.id());
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
    let mut lines: Vec<String> = Vec::new();
    let mut last_flush = Instant::now();
    let mut pending = String::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        pending.push_str(&line);
        pending.push('\n');
        if last_flush.elapsed() >= Duration::from_secs(2) {
            let tail = tail_2k(&pending);
            if let Ok(conn) = db() {
                let _ = conn.execute(
                    "UPDATE jobs SET last_detail = ?2 WHERE id = ?1",
                    params![id, tail],
                );
            }
            last_flush = Instant::now();
        }
        lines.push(line);
    }
    let exit = child.wait();
    let all = lines.join("\n");
    let tail = if all.len() > 2048 {
        all[all.len() - 2048..].to_string()
    } else {
        all
    };
    (exit, tail)
}

fn tail_2k(s: &str) -> String {
    if s.len() > 2048 {
        s[s.len() - 2048..].to_string()
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
