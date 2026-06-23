//! Sync→async execution bridge for the in-process `find` / `xargs` builtins.
//!
//! findutils is synchronous but must route commands through `Shell::run_argv`
//! (async, non-`Send` shell). findutils runs on a worker thread and submits
//! each argv over a channel; the async executor on the builtin's task runs each
//! via `run_argv` on a subshell clone (env/cwd mutations don't leak) and replies
//! with the exit code. `xargs -P` parallelism = `FuturesUnordered` concurrency
//! on the single task (no `Send` needed). Standalone binaries keep their
//! `std::process::Command` path (trait hooks default to "not handled").

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

/// Exit code from a bridge command, or `CouldNotRun` when `run_argv` couldn't
/// dispatch it (not found / not executable). Distinct from a command that ran
/// and exited non-zero, so xargs can report GNU's exit 127 (vs 123).
pub enum Outcome {
    Code(i32),
    CouldNotRun,
}

/// Sync-side handle for the findutils thread. Submit is non-blocking;
/// the returned [`ExecTicket`] blocks for the exit code.
#[derive(Clone)]
pub struct ExecSubmitter {
    tx: UnboundedSender<ExecRequest>,
}

impl ExecSubmitter {
    /// Submit an argv for execution; returns a ticket (non-blocking). argv is
    /// lossily decoded to `String` (brush command words are `String`s).
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
