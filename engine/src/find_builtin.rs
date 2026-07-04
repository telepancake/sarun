//! In-process `find` builtin for box brush shells.
//!
//! Runs the vendored findutils fork against the shell's LOGICAL I/O and LOGICAL
//! cwd. `-exec`/`-execdir` commands run THROUGH BRUSH (builtin/function/script,
//! snooped), exactly like `xargs` and `env`/`nice`.
//!
//! ## Logical cwd
//!
//! brush never `chdir`s the process. A relative start path is rooted at the
//! logical `cwd` (absolute walk, process cwd untouched); the display path is
//! relative to the start. No `unshare(CLONE_FS)` hack. See `Dependencies::cwd`.
//!
//! ## sync→async bridge for `-exec`
//!
//! findutils is synchronous; `Shell::run_argv` is async on a non-`Send` shell.
//! find runs on a worker thread, submitting each `-exec` argv over a channel;
//! the async executor runs each via `run_argv` on a subshell clone (-execdir
//! uses the entry's parent dir; otherwise the logical cwd). See `builtin_exec`.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::SystemTime;

use brush_core::openfiles::OpenFile;
use findutils::find::Dependencies;

use crate::builtin_exec;

/// `find`'s injected dependencies: logical I/O, logical cwd, and the
/// `-exec` submitter that routes commands through brush.
struct BrushFindDeps {
    output: RefCell<OpenFile>,
    error_output: RefCell<OpenFile>,
    stdin: RefCell<Box<dyn BufRead>>,
    now: SystemTime,
    cwd: std::path::PathBuf,
    /// The box shell's LOGICAL `TZ` (its `export TZ=…`), or `None` when unset.
    /// Steers `-printf %T/%A/%C` and `-daystart` to the box's zone without the
    /// engine process mutating its own `TZ`. See `Dependencies::tz`.
    tz: Option<String>,
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
        // Logical stdin; `-files0-from -` reads here, not the engine's fd 0.
        &self.stdin
    }

    fn now(&self) -> SystemTime {
        self.now
    }

    fn confirm(&self, prompt: &str) -> bool {
        // POSIX `-ok`: prompt on logical stderr, answer from logical stdin.
        // EOF/error → empty string → declined.
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
        // Logical cwd: find roots relative start paths here, no process chdir.
        Some(&self.cwd)
    }

    fn tz(&self) -> Option<String> {
        // Logical TZ: the box shell's `export TZ=…`, snapshotted at spawn.
        self.tz.clone()
    }

    fn exec_via_shell(&self) -> bool {
        true
    }

    fn run(&self, argv: &[std::ffi::OsString], cwd: Option<&std::path::Path>) -> i32 {
        // Serial: find dispatches one -exec at a time; block for the exit code.
        self.submitter.run(argv, cwd.map(std::path::Path::to_path_buf))
    }
}

/// `find` — directory tree walker with tests and `-exec` actions.
#[derive(clap::Parser)]
pub(crate) struct FindBuiltin {
    /// Raw argv; the findutils fork parses find's grammar.
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl brush_core::builtins::Command for FindBuiltin {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::results::ExecutionResult, Self::Error> {
        // find_main treats argv[0] as the program name; prepend it.
        let mut argv: Vec<String> = vec![context.command_name.clone()];
        argv.extend(self.args.iter().cloned());

        // Capture logical I/O as owned Send values for the worker thread.
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let input = context.stdin();
        let cwd = context.shell.working_dir().to_path_buf();

        // Snapshot the box shell's LOGICAL exported `TZ` (only exported vars
        // reach a child), so `-printf %T` / `-daystart` render in the box's zone.
        let tz = context
            .shell
            .env()
            .iter_exported()
            .find(|(k, _)| k.as_str() == "TZ")
            .map(|(_, v)| v.value().to_cow_str(context.shell).to_string());

        // Execution bridge: find (thread) submits -exec argvs; executor runs each
        // through run_argv on a subshell clone.
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
                    tz,
                    submitter,
                };
                let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                findutils::find::find_main(&argv_refs, &deps)
                // deps (and submitter) drop here → channel closes → run_executor returns.
            });

        builtin_exec::run_executor(rx, context.shell, &context.params).await;

        let code = match worker {
            Ok(handle) => handle.join().unwrap_or(1),
            Err(_) => 1,
        };
        Ok(brush_core::results::ExecutionResult::new(exit_code_to_u8(code)))
    }
}

/// Narrow find's `i32` exit code to `u8`. The vendored findutils follows the GNU
/// signal convention internally (killed by signal N → `128 + N`; fits u8).
/// `(code & 0xff) as u8` was wrong for out-of-range values (negative sentinel or
/// `256+N` would wrap to an unrelated small number). Clamp: anything outside
/// 0..=255 → 255 (generic failure), never masquerading as a valid status.
fn exit_code_to_u8(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(255)
}
