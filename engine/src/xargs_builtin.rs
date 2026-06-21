//! In-process `xargs` builtin for box brush shells.
//!
//! Runs the vendored find-and-xargs fork of uutils/findutils against the shell's
//! LOGICAL stdin/stdout/stderr, and — crucially — runs each command it builds
//! THROUGH BRUSH rather than `exec`ing a binary. So `… | xargs cmd` honors shell
//! builtins, functions, and scripts, with the box's snooping, exactly like
//! `env`/`nice` do (see `crate::exec_wrappers` and `Shell::run_argv`).
//!
//! ## Why logical stdin matters most here
//!
//! xargs's primary input is stdin: it reads NUL/whitespace-separated items and
//! builds command lines from them. In-process, `std::io::stdin()` is the
//! ENGINE's real fd 0 — a control channel, a parent pipe, another box's stream —
//! so reading it would steal bytes from whatever owns it. The vendored patch
//! routes the item read through `XargsIo::take_input`, which here yields the
//! shell's logical stdin.
//!
//! ## Running commands through brush (the sync→async bridge)
//!
//! findutils is synchronous; `Shell::run_argv` is async and must run on the
//! builtin's task (the shell isn't `Send`). So findutils runs on a worker
//! THREAD, submitting each built argv over a channel; an async executor on this
//! task runs it via `run_argv` on a subshell clone and replies with the exit
//! code (see `crate::builtin_exec`). `-P` parallelism is just the executor
//! running several submissions at once — `(cmd)& … & wait`. Nothing touches the
//! engine's process-global stdio or cwd; each command runs in the shell's
//! logical cwd because its subshell carries it.

use std::cell::RefCell;
use std::io::{Read, Write};

use brush_core::openfiles::OpenFile;
use clap::Parser;
use findutils::xargs::{CommandExecutionError, ExecChild, XargsIo};

use crate::builtin_exec;

/// xargs's injected logical I/O plus the execution submitter.
///
/// `input` is held in an `Option` so `take_input` can move it into xargs's
/// `ArgumentReader` exactly once. `output`/`error_output` are xargs's OWN sinks
/// (the no-command `echo`, `-t` traces, warnings). `submitter` hands each built
/// command to the async executor that runs it through brush.
struct BrushXargsIo {
    input: RefCell<Option<Box<dyn Read>>>,
    output: RefCell<OpenFile>,
    error_output: RefCell<OpenFile>,
    submitter: builtin_exec::ExecSubmitter,
}

/// A command dispatched through brush, awaited by blocking on its reply channel.
struct BridgeChild(builtin_exec::ExecTicket);

impl ExecChild for BridgeChild {
    fn wait(self: Box<Self>) -> Result<i32, CommandExecutionError> {
        Ok(self.0.wait())
    }
}

impl XargsIo for BrushXargsIo {
    fn take_input(&self) -> Box<dyn Read> {
        // Hand xargs the shell's logical stdin (consulted once, only when no
        // `-a/--arg-file` is given). A second call sees EOF, never the engine fd.
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

    fn submit(&self, argv: &[std::ffi::OsString]) -> Option<Box<dyn ExecChild>> {
        // Run the command through brush (builtin / function / external, snooped)
        // instead of spawning a process. The child's cwd, env, and stdio come
        // from the subshell + params the executor runs `run_argv` with — so we
        // do NOT need `cwd`/`child_stdout`/`child_stderr` here.
        // xargs commands always run in the shell's logical cwd (no per-command
        // dir like find -execdir), so pass `None`.
        Some(Box::new(BridgeChild(self.submitter.submit(argv, None))))
    }
}

/// `xargs` — build and run command lines from stdin items, through brush.
#[derive(Parser)]
pub(crate) struct XargsBuiltin {
    /// All arguments collected raw; the findutils fork parses xargs's grammar.
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl brush_core::builtins::Command for XargsBuiltin {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::results::ExecutionResult, Self::Error> {
        // xargs_main_with_io treats argv[0] as the program name and parses the
        // rest; rebuild the full argv from the command name + collected args.
        let mut argv: Vec<String> = vec![context.command_name.clone()];
        argv.extend(self.args.iter().cloned());

        // Logical I/O captured as owned, Send values for the worker thread.
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let input = context.stdin();

        // The execution bridge: findutils (on the thread) submits argvs; the
        // executor (awaited below) runs each through `run_argv` on a subshell.
        let (submitter, rx) = builtin_exec::channel();

        let worker = std::thread::Builder::new()
            .name("sarun-xargs".into())
            .spawn(move || {
                let io = BrushXargsIo {
                    input: RefCell::new(Some(Box::new(input))),
                    output: RefCell::new(out),
                    error_output: RefCell::new(err),
                    submitter,
                };
                let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                findutils::xargs::xargs_main_with_io(&argv_refs, &io)
                // `io` (and its `submitter`) drops here → the executor's channel
                // closes → `run_executor` returns.
            });

        // Drive the executor until the worker finishes and its submitter drops.
        builtin_exec::run_executor(rx, context.shell, &context.params).await;

        // The worker has effectively finished (it dropped the submitter); join to
        // collect its exit code. A panic inside xargs surfaces as a generic
        // failure, the same isolation the old worker thread gave.
        let code = match worker {
            Ok(handle) => handle.join().unwrap_or(1),
            Err(_) => 1,
        };
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}
