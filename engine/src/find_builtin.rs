//! In-process `find` builtin for box brush shells.
//!
//! Runs the vendored find-only fork of uutils/findutils against the shell's
//! LOGICAL stdout/stderr/stdin (via a `Dependencies` impl) on a dedicated
//! worker thread that owns its current directory. Nothing touches the engine's
//! process-global stdio or cwd, so the result is correct after `cd`/redirects
//! and safe to run concurrently with sibling builtins.
//!
//! ## Why a thread with its own cwd
//!
//! `find` resolves its start paths, every `stat`, and `-exec` child cwd against
//! the *kernel* current directory, pervasively, through `std::fs`/`walkdir` —
//! there is no single seam to redirect. brush uses a LOGICAL cwd (it never
//! `chdir`s the process on `cd`), so to make `find .` walk the right place we
//! give the worker thread its own cwd: `unshare(CLONE_FS)` splits the thread's
//! `fs_struct` (cwd/root/umask) from the rest of the process — an unprivileged
//! operation, NOT a namespace (no `CLONE_NEWNS`/`CLONE_NEWUSER`) — and then
//! `chdir` to the shell's logical working dir affects only this thread. Sibling
//! threads and the engine's own cwd are untouched, and `-exec`'s `fork`+`exec`
//! inherits the thread's logical cwd for free. If `unshare` fails (e.g. a
//! seccomp filter blocks the syscall) we skip the `chdir` and fall back to the
//! process cwd rather than mutating it process-wide.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;
use std::time::SystemTime;

use findutils::find::Dependencies;

/// `find`'s injected dependencies, bound to one shell command's logical I/O.
///
/// Built and used entirely on the worker thread, so the non-`Send` `Rc`s never
/// cross a thread boundary.
struct BrushFindDeps {
    output: Rc<RefCell<dyn Write>>,
    error_output: Rc<RefCell<dyn Write>>,
    stdin: RefCell<Box<dyn BufRead>>,
    now: SystemTime,
}

impl Dependencies for BrushFindDeps {
    fn get_output(&self) -> &RefCell<dyn Write> {
        self.output.as_ref()
    }

    fn get_error_output(&self) -> &RefCell<dyn Write> {
        self.error_output.as_ref()
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
        // captured before we leave `context`'s borrow.
        let out = context.stdout();
        let err = context.stderr();
        let input = context.stdin();
        let cwd = context.shell.working_dir().to_path_buf();

        let spawned = std::thread::Builder::new()
            .name("sarun-find".into())
            .spawn(move || {
                // Give this thread its own cwd, then point it at the logical dir.
                let owns_cwd = unsafe { libc::unshare(libc::CLONE_FS) } == 0;
                if owns_cwd {
                    if let Ok(c) = std::ffi::CString::new(cwd.as_os_str().as_bytes()) {
                        // Thread-local after the unshare above; never the process cwd.
                        unsafe { libc::chdir(c.as_ptr()) };
                    }
                }

                let deps = BrushFindDeps {
                    output: Rc::new(RefCell::new(out)),
                    error_output: Rc::new(RefCell::new(err)),
                    stdin: RefCell::new(Box::new(BufReader::new(input))),
                    now: SystemTime::now(),
                };
                let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                findutils::find::find_main(&argv_refs, &deps)
            });

        // A spawn failure (resource exhaustion) or a panic inside find_main both
        // surface as a generic failure exit; find_main itself never panics on
        // normal input (it returns an exit code).
        let code = match spawned {
            Ok(handle) => handle.join().unwrap_or(1),
            Err(_) => 1,
        };
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}
