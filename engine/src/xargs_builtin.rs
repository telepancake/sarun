//! In-process `xargs` builtin for box brush shells.
//!
//! Runs the vendored findutils fork against the shell's LOGICAL
//! stdin/stdout/stderr, routing each command through brush (`Shell::run_argv`)
//! so builtins/functions/scripts are honored, exactly like `env`/`nice`.
//!
//! ## Why logical stdin matters
//!
//! xargs reads NUL/whitespace-separated items from stdin. In-process,
//! `std::io::stdin()` is the engine's real fd 0 — reading it steals bytes from
//! whatever owns it (control channel, parent pipe). The vendored patch routes
//! reads through `XargsIo::take_input`, which yields the shell's logical stdin.
//!
//! ## sync→async bridge
//!
//! findutils is synchronous; `Shell::run_argv` is async on a non-`Send` shell.
//! findutils runs on a worker thread, submitting each built argv over a channel;
//! the async executor runs each via `run_argv` on a subshell clone and replies
//! with the exit code (see `builtin_exec`). `-P` parallelism = executor
//! concurrency. Nothing touches process-global stdio or cwd.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};

use brush_core::openfiles::OpenFile;
use clap::Parser;
use findutils::xargs::{CommandExecutionError, ExecChild, XargsIo};

use crate::builtin_exec;

/// xargs's injected logical I/O and execution submitter.
/// `input` is `Option` so `take_input` can move it exactly once into xargs's
/// `ArgumentReader`. `output`/`error_output` are xargs's own sinks (`-t`, warnings).
struct BrushXargsIo {
    input: RefCell<Option<Box<dyn Read>>>,
    output: RefCell<OpenFile>,
    error_output: RefCell<OpenFile>,
    /// The shell's LOGICAL exported env, snapshotted before the worker thread
    /// starts. xargs hands this to every dispatched child (matching `export
    /// FOO=bar | xargs cmd`) and uses it to size command batches with headroom
    /// for the child's real env at execve time.
    env: HashMap<OsString, OsString>,
    submitter: builtin_exec::ExecSubmitter,
}

/// Pending command: blocks on its reply channel for the exit code.
struct BridgeChild(builtin_exec::ExecTicket);

impl ExecChild for BridgeChild {
    fn wait(self: Box<Self>) -> Result<i32, CommandExecutionError> {
        match self.0.wait() {
            builtin_exec::Outcome::Code(c) => Ok(c),
            // run_argv couldn't dispatch it → GNU xargs exit 127, and stop.
            builtin_exec::Outcome::CouldNotRun => Err(CommandExecutionError::NotFound),
        }
    }
}

impl XargsIo for BrushXargsIo {
    fn take_input(&self) -> Box<dyn Read> {
        // Logical stdin, moved once; a second call sees EOF (never the engine fd).
        self.input
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Box::new(std::io::empty()))
    }

    fn output(&self) -> &RefCell<dyn Write> {
        &self.output
    }

    fn error_output(&self) -> &RefCell<dyn Write> {
        &self.error_output
    }

    fn env(&self) -> Option<HashMap<OsString, OsString>> {
        Some(self.env.clone())
    }

    fn submit(&self, argv: &[std::ffi::OsString]) -> Option<Box<dyn ExecChild>> {
        // Run through brush (builtin/function/external) in the shell's logical
        // cwd (xargs has no per-command dir like -execdir; pass None).
        Some(Box::new(BridgeChild(self.submitter.submit(argv, None))))
    }
}

/// `xargs` — build and run command lines from stdin items through brush.
#[derive(Parser)]
pub(crate) struct XargsBuiltin {
    /// Raw argv; the findutils fork parses xargs's grammar.
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl brush_core::builtins::Command for XargsBuiltin {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::results::ExecutionResult, Self::Error> {
        // xargs_main_with_io treats argv[0] as the program name; prepend it.
        let mut argv: Vec<String> = vec![context.command_name.clone()];
        argv.extend(self.args.iter().cloned());

        // Capture logical I/O as owned Send values for the worker thread.
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let input = context.stdin();

        // Snapshot the shell's LOGICAL exported env for the child commands (the
        // worker thread hands it to each dispatched argv, and sizes batches by
        // it). iter_exported is the same idiom `env`/`printenv` use.
        let env: HashMap<OsString, OsString> = context
            .shell
            .env()
            .iter_exported()
            .map(|(k, v)| {
                (
                    k.clone().into(),
                    v.value().to_cow_str(context.shell).to_string().into(),
                )
            })
            .collect();

        // Execution bridge: findutils (thread) submits argvs; executor runs each
        // through run_argv on a subshell clone.
        let (submitter, rx) = builtin_exec::channel();

        let worker = std::thread::Builder::new()
            .name("sarun-xargs".into())
            .spawn(move || {
                let io = BrushXargsIo {
                    input: RefCell::new(Some(Box::new(input))),
                    output: RefCell::new(out),
                    error_output: RefCell::new(err),
                    env,
                    submitter,
                };
                let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                findutils::xargs::xargs_main_with_io(&argv_refs, &io)
                // io (and submitter) drop here → channel closes → run_executor returns.
            });

        // Drive the executor until the worker finishes and the submitter drops.
        builtin_exec::run_executor(rx, context.shell, &context.params).await;

        // Worker has finished (submitter dropped); join for exit code.
        // A panic inside xargs surfaces as a generic failure (isolation intact).
        let code = match worker {
            Ok(handle) => handle.join().unwrap_or(1),
            Err(_) => 1,
        };
        Ok(brush_core::results::ExecutionResult::new(exit_code_to_u8(code)))
    }
}

/// Narrow xargs's `i32` exit code to `u8`. The vendored findutils follows the GNU
/// signal convention (killed by signal N → `128 + N`; xargs's own codes
/// 123/124/125/126/127 all fit). `(code & 0xff) as u8` was wrong for out-of-range
/// values (negative sentinel, `256+N` → wraps to an unrelated small number).
/// Clamp: out-of-range → 255 (generic failure).
fn exit_code_to_u8(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(255)
}
