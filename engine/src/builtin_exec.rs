//! Sync→async execution bridge for the in-process `find` / `xargs` builtins.
//!
//! findutils (find, xargs) runs synchronously and historically spawned each
//! command via `std::process::Command`. To run those commands through brush
//! instead — so a command can be a shell **builtin**, a **function**, or a
//! **script**, all with the box's snooping, exactly like `env`/`nice` do — each
//! argv is routed through [`brush_core::Shell::run_argv`], which is async.
//!
//! findutils can't `.await`, so it runs on a worker THREAD and submits each argv
//! over a channel to an async executor on the builtin's own task. The executor
//! runs `run_argv` on a **subshell clone** (per-command env/cwd mutations don't
//! leak — matching the one-process-per-command model) and replies with the exit
//! code. `xargs -P` parallelism falls out for free: the executor drives
//! submitted commands CONCURRENTLY (a `FuturesUnordered` on the single task — no
//! `Send` needed, since a `Shell` clone isn't `Send`), bounded only by findutils
//! issuing at most N submissions before it reaps one.
//!
//! The standalone findutils binaries keep their `std::process::Command` path
//! (the trait hooks default to "not handled"); only the in-process embedder
//! supplies a submitter.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender};

use brush_core::{ExecutionParameters, Shell};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// One command for the async executor to run, plus where to send its outcome.
pub struct ExecRequest {
    argv: Vec<String>,
    /// Working directory for this command (`find -execdir` runs in the entry's
    /// parent). `None` runs it in the shell's own logical cwd.
    cwd: Option<PathBuf>,
    reply: SyncSender<Outcome>,
}

/// The result of running one command through the bridge: its exit code, or
/// `CouldNotRun` when `run_argv` couldn't dispatch the command at all (not
/// found / not executable). The two are distinct so xargs can report GNU's exit
/// 127 (vs 123 for a command that merely *exited* non-zero).
pub enum Outcome {
    Code(i32),
    CouldNotRun,
}

/// Sync-side handle held by the findutils Io/Deps and its worker thread.
/// Submitting is non-blocking; the returned [`ExecTicket`] blocks for the code.
#[derive(Clone)]
pub struct ExecSubmitter {
    tx: UnboundedSender<ExecRequest>,
}

impl ExecSubmitter {
    /// Submit an argv (optionally with a working directory) for execution;
    /// returns a ticket to await its exit code. Non-blocking. argv is lossily
    /// decoded to `String` (brush command words are `String`s; this matches how
    /// brush itself models argv).
    pub fn submit(&self, argv: &[OsString], cwd: Option<PathBuf>) -> ExecTicket {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        let argv = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        // If the executor is gone the send fails; the ticket then yields 127.
        let _ = self.tx.send(ExecRequest { argv, cwd, reply });
        ExecTicket { rx }
    }

    /// Submit and block for the exit code — serial execution. A command that
    /// could not be run is reported as 127 (find has no stop-on-not-found
    /// semantics; it just treats the -exec as failed).
    pub fn run(&self, argv: &[OsString], cwd: Option<PathBuf>) -> i32 {
        match self.submit(argv, cwd).wait() {
            Outcome::Code(c) => c,
            Outcome::CouldNotRun => 127,
        }
    }
}

/// A pending command's result, awaited by blocking the findutils worker thread.
pub struct ExecTicket {
    rx: Receiver<Outcome>,
}

impl ExecTicket {
    /// Block until the command finishes; `CouldNotRun` if the executor dropped
    /// the reply (it is gone) — same as a command that couldn't be dispatched.
    pub fn wait(self) -> Outcome {
        self.rx.recv().unwrap_or(Outcome::CouldNotRun)
    }
}

/// Create a bridge: the sync submitter (cloned onto the findutils thread) and
/// the receiver half the async [`run_executor`] consumes.
pub fn channel() -> (ExecSubmitter, UnboundedReceiver<ExecRequest>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (ExecSubmitter { tx }, rx)
}

/// Drive submitted commands through `run_argv` on subshell clones, concurrently.
/// Returns when every submitter has been dropped (the findutils thread finished)
/// AND all in-flight commands have completed.
pub async fn run_executor<SE: brush_core::extensions::ShellExtensions>(
    mut rx: UnboundedReceiver<ExecRequest>,
    shell: &Shell<SE>,
    params: &ExecutionParameters,
) {
    let mut running = FuturesUnordered::new();
    loop {
        if running.is_empty() {
            match rx.recv().await {
                Some(req) => running.push(run_one(shell.clone(), params.clone(), req)),
                None => break,
            }
        } else {
            tokio::select! {
                maybe = rx.recv() => match maybe {
                    Some(req) => running.push(run_one(shell.clone(), params.clone(), req)),
                    None => {
                        // No more submissions; finish the in-flight commands.
                        while running.next().await.is_some() {}
                        break;
                    }
                },
                Some(()) = running.next() => {}
            }
        }
    }
}

/// Run ONE submitted command on its own subshell clone and reply with the code.
async fn run_one<SE: brush_core::extensions::ShellExtensions>(
    mut subshell: Shell<SE>,
    params: ExecutionParameters,
    req: ExecRequest,
) {
    // `find -execdir` runs the command in the entry's parent directory; set it on
    // the subshell (best-effort) so run_argv resolves relative {} there.
    if let Some(dir) = &req.cwd {
        let _ = subshell.set_working_dir(dir);
    }
    let outcome = match subshell.run_argv(&req.argv, &params).await {
        Ok(result) => Outcome::Code(i32::from(u8::from(result.exit_code))),
        // run_argv could not dispatch the command (not found / not executable):
        // distinct from a command that ran and exited non-zero, so xargs can
        // report GNU's 127 rather than 123.
        Err(_) => Outcome::CouldNotRun,
    };
    let _ = req.reply.send(outcome);
}
