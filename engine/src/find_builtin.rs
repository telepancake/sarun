//! In-process `find` builtin for box brush shells.
//!
//! Runs the vendored find-only fork of uutils/findutils against the shell's
//! LOGICAL stdout/stderr/stdin and LOGICAL cwd, and runs each `-exec`/`-execdir`
//! command THROUGH BRUSH (so it may be a builtin / function / script, all
//! snooped), exactly like the `xargs` builtin and `env`/`nice`.
//!
//! ## Logical cwd without `unshare`
//!
//! `find` resolves its start paths against the current directory; brush keeps a
//! LOGICAL cwd (it never `chdir`s the process). The fork roots a relative start
//! path at the logical `cwd` (an absolute walk that never reads or mutates the
//! process cwd) and presents paths relative to the start — see
//! `Dependencies::cwd`. No per-thread `unshare(CLONE_FS)` hack.
//!
//! ## Running `-exec` through brush (the sync→async bridge)
//!
//! findutils is synchronous; `Shell::run_argv` is async on a non-`Send` shell.
//! So find runs on a worker THREAD and submits each built `-exec` argv over a
//! channel; an async executor on the builtin's task runs it via `run_argv` on a
//! subshell clone (in the entry's parent dir for `-execdir`, else the logical
//! cwd) and replies with the exit code (see `crate::builtin_exec`). `-exec` is
//! serial — find walks and dispatches one command at a time — so the builtin
//! blocks on each reply, which the executor is free to service concurrently.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::SystemTime;

use brush_core::openfiles::OpenFile;
use findutils::find::Dependencies;

use crate::builtin_exec;

/// `find`'s injected dependencies: its logical I/O, logical cwd, and the
/// execution submitter that routes `-exec` commands through brush.
struct BrushFindDeps {
    output: RefCell<OpenFile>,
    error_output: RefCell<OpenFile>,
    stdin: RefCell<Box<dyn BufRead>>,
    now: SystemTime,
    cwd: std::path::PathBuf,
    submitter: builtin_exec::ExecSubmitter,
}

impl Dependencies for BrushFindDeps {
    fn get_output(&self) -> &RefCell<dyn Write> {
        &self.output
    }

    fn get_error_output(&self) -> &RefCell<dyn Write> {
        &self.error_output
    }

    fn get_input(&self) -> &RefCell<dyn Read> {
        // The shell's logical stdin (also used by `confirm`). `-files0-from -`
        // reads it instead of the host process's real fd 0.
        &self.stdin
    }

    fn now(&self) -> SystemTime {
        self.now
    }

    fn confirm(&self, prompt: &str) -> bool {
        // POSIX `-ok`: prompt on stderr, read the response from stdin. Both are
        // the shell's logical streams here. EOF / read error → empty → declined.
        {
            let mut err = self.error_output.borrow_mut();
            let _ = write!(err, "{prompt}");
            let _ = err.flush();
        }
        let mut line = String::new();
        let read = self.stdin.borrow_mut().read_line(&mut line).unwrap_or(0);
        read > 0 && line.trim_start().starts_with(['y', 'Y'])
    }

    fn cwd(&self) -> Option<&std::path::Path> {
        // The shell's logical cwd: find resolves relative start paths against it,
        // without any process/thread chdir.
        Some(&self.cwd)
    }

    fn exec_via_shell(&self) -> bool {
        true
    }

    fn run(&self, argv: &[std::ffi::OsString], cwd: Option<&std::path::Path>) -> i32 {
        // Serial: find dispatches one `-exec` at a time. Block for the code; the
        // async executor runs the command through brush meanwhile.
        self.submitter.run(argv, cwd.map(std::path::Path::to_path_buf))
    }
}

/// `find` — walk a directory tree, running tests and `-exec` actions per entry.
#[derive(clap::Parser)]
pub(crate) struct FindBuiltin {
    /// All arguments collected raw; the findutils fork parses find's grammar.
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl brush_core::builtins::Command for FindBuiltin {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::results::ExecutionResult, Self::Error> {
        // find_main treats argv[0] as the program name; rebuild the full argv.
        let mut argv: Vec<String> = vec![context.command_name.clone()];
        argv.extend(self.args.iter().cloned());

        // Logical I/O captured as owned, Send values for the worker thread.
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let input = context.stdin();
        let cwd = context.shell.working_dir().to_path_buf();

        // The execution bridge: find (on the thread) submits each -exec argv; the
        // executor (awaited below) runs each through run_argv on a subshell.
        let (submitter, rx) = builtin_exec::channel();

        let worker = std::thread::Builder::new()
            .name("sarun-find".into())
            .spawn(move || {
                let deps = BrushFindDeps {
                    output: RefCell::new(out),
                    error_output: RefCell::new(err),
                    stdin: RefCell::new(Box::new(BufReader::new(input))),
                    now: SystemTime::now(),
                    cwd,
                    submitter,
                };
                let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                findutils::find::find_main(&argv_refs, &deps)
                // `deps` (and its `submitter`) drops here → the executor's
                // channel closes → `run_executor` returns.
            });

        builtin_exec::run_executor(rx, context.shell, &context.params).await;

        let code = match worker {
            Ok(handle) => handle.join().unwrap_or(1),
            Err(_) => 1,
        };
        Ok(brush_core::results::ExecutionResult::new(exit_code_to_u8(code)))
    }
}

/// Narrow find's `i32` exit code to the `u8` an `ExecutionResult` carries
/// WITHOUT fabricating a bogus value for a signal death.
///
/// The vendored findutils already follows the GNU convention internally: a
/// child killed by signal N is reported as `128 + N` (see `find/mod.rs`'s `run`
/// doc and xargs's `map_code`). Those land in 129..=255, which fit a `u8`
/// verbatim. The old `(code & 0xff) as u8` was wrong for any code outside
/// 0..=255 — a negative sentinel or an over-wide value would wrap to an
/// unrelated small number (e.g. a hypothetical `256+N` collapsing to `N`, a
/// "success"-looking code). Clamp instead: anything that doesn't fit a `u8`
/// becomes 255 (a generic failure), so a real exit/signal code is preserved and
/// an out-of-range one can never masquerade as a different, plausible status.
fn exit_code_to_u8(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(255)
}
