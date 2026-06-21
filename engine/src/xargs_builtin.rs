//! In-process `xargs` builtin for box brush shells.
//!
//! Runs the vendored find-and-xargs fork of uutils/findutils against the
//! shell's LOGICAL stdin/stdout/stderr (via an `XargsIo` impl) on a dedicated
//! worker thread that owns its current directory. Nothing touches the engine's
//! process-global stdio or cwd, so xargs reads the right pipe, prints to the
//! box's streams, and spawns its children in the shell's logical cwd — and it
//! is safe to run concurrently with sibling builtins.
//!
//! ## Why logical stdin matters most here
//!
//! xargs's primary input is stdin: it reads NUL/whitespace-separated items and
//! builds command lines from them. In-process, `std::io::stdin()` is the
//! ENGINE's real fd 0 — a control channel, a parent pipe, another box's stream
//! — so reading it would steal bytes from whatever owns it. The vendored patch
//! routes the item read through `XargsIo::take_input`, which here yields the
//! shell's logical stdin. (`-a/--arg-file` still reads the named file, same as
//! upstream.)
//!
//! ## Why a thread with its own cwd
//!
//! The commands xargs spawns run via `std::process::Command`, which resolves
//! relative program paths and the child's cwd against the *kernel* current
//! directory. brush uses a LOGICAL cwd (it never `chdir`s the process on `cd`),
//! so — exactly as `find_builtin` does for `-exec` — we run xargs on a worker
//! thread that `unshare(CLONE_FS)`s (an unprivileged split of this thread's
//! cwd/root/umask from the rest of the process, NOT a namespace) and then
//! `chdir`s to the shell's logical dir. The spawned children inherit that cwd
//! for free; sibling threads and the engine's own cwd are untouched. If
//! `unshare` fails (e.g. a seccomp filter) we skip the `chdir` and fall back to
//! the process cwd rather than mutating it process-wide.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::process::Stdio;

use brush_core::openfiles::OpenFile;
use findutils::xargs::XargsIo;

/// xargs's injected logical I/O, bound to one shell command's streams.
///
/// Built and used entirely on the worker thread, so the non-`Send` trait
/// objects never cross a thread boundary. `input` is held in an `Option` so
/// `take_input` can move it into xargs's `ArgumentReader` exactly once.
struct BrushXargsIo {
    input: RefCell<Option<Box<dyn Read>>>,
    // The shell's logical stdout/stderr as `OpenFile`s: used both as the sink
    // for xargs's OWN output/diagnostics (`OpenFile: Write`) and as the source
    // we dup into spawned children's stdio, so the children honor the box's
    // redirects and pipes.
    output: RefCell<OpenFile>,
    error_output: RefCell<OpenFile>,
}

impl XargsIo for BrushXargsIo {
    fn take_input(&self) -> Box<dyn Read> {
        // Hand xargs the shell's logical stdin. Consulted once, only when no
        // `-a/--arg-file` is given. If it were ever called twice, later reads
        // see EOF (empty) rather than the engine's real fd 0.
        self.input
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Box::new(std::io::empty()))
    }

    fn output(&self) -> &RefCell<dyn Write> {
        // RefCell<OpenFile> unsizes to RefCell<dyn Write> (OpenFile: Write).
        &self.output
    }

    fn error_output(&self) -> &RefCell<dyn Write> {
        &self.error_output
    }

    fn child_stdout(&self) -> Option<Stdio> {
        // Dup the logical stdout for the child: a piped/redirected sink
        // (PipeWriter/File) is dup'd so `xargs cmd | next` / `> file` work; the
        // inherited stdout handle yields `inherit`; an fd-less in-memory stream
        // yields `null` (its bytes can't be handed to a child process).
        Stdio::try_from(self.output.borrow().clone()).ok()
    }

    fn child_stderr(&self) -> Option<Stdio> {
        Stdio::try_from(self.error_output.borrow().clone()).ok()
    }
}

/// brush `SimpleCommand` that runs `xargs` in-process.
pub(crate) struct XargsBuiltin;

impl brush_core::builtins::SimpleCommand for XargsBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: in-process uutils/findutils xargs builtin\n"))
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
        // CoreutilWrapper / FindBuiltin); xargs_main treats argv[0] as the
        // program name and parses argv[1..], so we pass it through unchanged.
        let mut argv: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        if argv.is_empty() {
            argv.push(context.command_name.clone());
        }

        // Logical I/O (owned, Send `OpenFile`s) and the logical working dir,
        // captured before we leave `context`'s borrow. stdout/stderr are taken
        // as `OpenFile`s (not `impl Write`) so they can be both written by xargs
        // and dup'd into its children; fall back to the process streams if a
        // descriptor isn't mapped (shouldn't happen for a builtin).
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let input = context.stdin();
        let cwd = context.shell.working_dir().to_path_buf();

        let spawned = std::thread::Builder::new()
            .name("sarun-xargs".into())
            .spawn(move || {
                // Give this thread its own cwd pointed at the box's logical dir
                // so the commands xargs spawns run there. If it can't be
                // established, refuse loudly rather than spawn children in the
                // engine's cwd (audit H2).
                if let Err(msg) = crate::find_builtin::establish_thread_cwd(&cwd) {
                    let mut e = err;
                    let _ = writeln!(e, "xargs: {msg}");
                    return 1;
                }

                let io = BrushXargsIo {
                    input: RefCell::new(Some(Box::new(input))),
                    output: RefCell::new(out),
                    error_output: RefCell::new(err),
                };
                let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                findutils::xargs::xargs_main_with_io(&argv_refs, &io)
            });

        // A spawn failure (resource exhaustion) or a panic inside xargs both
        // surface as a generic failure exit; xargs_main_with_io itself returns
        // an exit code rather than exiting the process.
        let code = match spawned {
            Ok(handle) => handle.join().unwrap_or(1),
            Err(_) => 1,
        };
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}
