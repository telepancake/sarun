//! In-process `find` builtin for box brush shells.
//!
//! Runs the vendored find-only fork of uutils/findutils against the shell's
//! LOGICAL stdout/stderr/stdin and LOGICAL cwd (via a `Dependencies` impl),
//! in-process on the calling thread. Nothing touches the engine's process-global
//! stdio or cwd, so the result is correct after `cd`/redirects.
//!
//! ## Logical cwd without `unshare` (no per-thread kernel cwd)
//!
//! `find` resolves its start paths and `-exec` child cwd against the current
//! directory. brush keeps a LOGICAL cwd (it never `chdir`s the process on
//! `cd`). Rather than give the run a private kernel cwd via
//! `unshare(CLONE_FS)`+`chdir` (a non-generalizing hack), the builtin passes the
//! logical cwd through `Dependencies::cwd`: the vendored fork roots a relative
//! start path at that absolute dir (an absolute walk that never reads or mutates
//! the process cwd) while presenting paths relative to the start, and runs
//! `-exec` children with that dir as their cwd. Pure logical state, exactly like
//! the `env`/`nice`/… exec-wrapper builtins — and no cross-thread cwd race.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;
use std::time::SystemTime;

use brush_core::openfiles::OpenFile;
use findutils::find::Dependencies;

/// `find`'s injected dependencies, bound to one shell command's logical I/O and
/// logical cwd. stdout/stderr are held as `OpenFile`s so they serve both as
/// find's own write sinks and as the source we dup into `-exec` children's
/// stdio. `cwd` is the shell's LOGICAL working dir: find roots a relative start
/// path there (absolute walk, paths presented relative) and runs `-exec`
/// children there — no process/thread cwd is ever touched.
struct BrushFindDeps {
    output: RefCell<OpenFile>,
    error_output: RefCell<OpenFile>,
    stdin: RefCell<Box<dyn BufRead>>,
    now: SystemTime,
    cwd: std::path::PathBuf,
}

impl Dependencies for BrushFindDeps {
    fn get_output(&self) -> &RefCell<dyn Write> {
        // RefCell<OpenFile> unsizes to RefCell<dyn Write> (OpenFile: Write).
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

    fn child_stdout(&self) -> Option<Stdio> {
        // Dup the logical stdout for an `-exec` child: a piped/redirected sink
        // is dup'd, an inherited handle stays inherit, an fd-less stream → null.
        Stdio::try_from(self.output.borrow().clone()).ok()
    }

    fn child_stderr(&self) -> Option<Stdio> {
        Stdio::try_from(self.error_output.borrow().clone()).ok()
    }

    fn cwd(&self) -> Option<&std::path::Path> {
        // The shell's logical cwd: find resolves relative start paths against it
        // and runs -exec children there, without any process/thread chdir.
        Some(&self.cwd)
    }
}

/// brush `SimpleCommand` that runs `find` in-process.
pub(crate) struct FindBuiltin;

impl brush_core::builtins::SimpleCommand for FindBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: in-process uutils/findutils find builtin\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        // brush hands `args` INCLUDING the command name as argv[0] (same as
        // CoreutilWrapper); find_main treats argv[0] as the program name and
        // parses argv[1..], so we pass it through unchanged.
        let mut argv: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        if argv.is_empty() {
            argv.push(context.command_name.clone());
        }

        // Logical I/O (owned, Send `OpenFile`s) and the logical working dir,
        // captured before we leave `context`'s borrow. stdout/stderr are taken
        // as `OpenFile`s (not `impl Write`) so they can be both written by find
        // and dup'd into `-exec` children; fall back to the process streams if a
        // descriptor isn't mapped (shouldn't happen for a builtin).
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let input = context.stdin();
        let cwd = context.shell.working_dir().to_path_buf();

        // Run find in-process on THIS thread — no `unshare`/`chdir`. find roots
        // a relative start path at the logical `cwd` (an absolute walk that
        // never reads or mutates the engine's process cwd) and runs -exec
        // children there, via `Dependencies::cwd`. `catch_unwind` keeps a panic
        // in find_main from unwinding into brush; find_main otherwise returns an
        // exit code.
        let deps = BrushFindDeps {
            output: RefCell::new(out),
            error_output: RefCell::new(err),
            stdin: RefCell::new(Box::new(BufReader::new(input))),
            now: SystemTime::now(),
            cwd,
        };
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            findutils::find::find_main(&argv_refs, &deps)
        }))
        .unwrap_or(1);
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}
