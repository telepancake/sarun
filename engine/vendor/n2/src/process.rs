//! Exposes process::run_command, a wrapper around platform-native process execution.

#[cfg(unix)]
pub use crate::process_posix::run_command;
#[cfg(windows)]
pub use crate::process_win::run_command;

#[cfg(target_arch = "wasm32")]
fn run_command(
    cmdline: &str,
    mut output_cb: impl FnMut(&[u8]),
) -> anyhow::Result<(Termination, Vec<u8>)> {
    anyhow::bail!("wasm cannot run commands");
}

#[derive(Debug, PartialEq)]
pub enum Termination {
    Success,
    Interrupted,
    Failure,
}

// ── sarun: in-process executor hook ──────────────────────────────────────────
// sarun embeds n2 and runs each recipe through its in-process brush shell
// instead of posix_spawning `/bin/sh -c`. The host installs an executor here
// BEFORE calling run::run(); when set, run_command (process_posix.rs) routes
// every cmdline to it and posix_spawn is never reached. The contract is
// IDENTICAL to upstream run_command: feed merged stdout+stderr bytes to
// output_cb, return a Termination. The scheduler (task.rs/work.rs) is untouched.
//
// The executor is a bare fn pointer (no captured state) so it needs no
// allocation/locking and is trivially Send+Sync; sarun's executor reaches its
// shared tokio runtime via its own statics.
pub type Executor = fn(cmdline: &str, output_cb: &mut dyn FnMut(&[u8])) -> Termination;

static EXECUTOR: std::sync::OnceLock<Executor> = std::sync::OnceLock::new();

/// sarun: install the in-process recipe executor. Call once before run::run().
/// Idempotent: a second call is ignored (OnceLock).
pub fn set_executor(exec: Executor) {
    let _ = EXECUTOR.set(exec);
}

/// sarun: the installed executor, if any.
pub fn executor() -> Option<Executor> {
    EXECUTOR.get().copied()
}
