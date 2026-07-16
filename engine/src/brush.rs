// The embedded brush shell (D9). With `-b`, the box runs brush-core/brush-parser
// IN-PROCESS in the --inner shim, not /bin/sh. Explicit toggle: a construct brush
// cannot run is a VISIBLE error and non-zero exit — never a silent downgrade.
//
// What this buys (D9): the sh-storm a build's per-recipe `sh -c` would otherwise
// fork+exec runs in-process, AND brush emits SEMANTIC PROVENANCE — the exact
// command string plus parsed pipeline/redirect structure — that raw FUSE cannot
// recover.
//
// Capture: we dup2 the box's FUSE sink files onto fd 1/2 BEFORE building the
// shell, so brush and every binary it forks/execs write through the overlay,
// exactly like ordinary capture mode.
//
// BUILTIN STDOUT IS CAPTURED: brush-core's OpenFile::Stdout write() hits the real
// fd 1 (openfiles.rs). Since fd 1 == the FUSE sink, a builtin writing stdout is
// captured exactly like a forked binary. BashMode builtins (echo/printf/test/[/let/
// source/declare…) all run in-process. The prior "ShMode preserves capture" claim
// was FALSE. (A redirected builtin stdout — pipe or `> file` — gets a different
// fd-backed OpenFile, also captured.)
//
// IN-PROCESS COREUTILS: `brush_coreutils_builtins::bundled_commands()` installs
// uutils/coreutils uumain adapters as brush builtins — `cat ls cp mv rm mkdir wc
// sort cut tr …` run IN-PROCESS. Shell builtins WIN for overlapping names
// (coreutils installed first, BashMode set overwrites overlaps). Each coreutil runs
// on a fresh thread via run_coreutil_localized — no process-fd dup2, pipeline-safe.
// All three shells (top-level box, nested sh, nested bash) get their builtins from
// box_builtins(), which is the single policy point.
//
// brush↔PROCESS LINKAGE (D9, DONE — see capture.rs):
//   brush emits one FRAME_PROV per pipeline IN EXECUTION ORDER, immediately before
//   running it, carrying a 0-based `seq`. Each FRAME_PROV carries the pipeline's
//   literal WRITE-redirect TARGET paths (`> file`, `>>`, `&>`). At teardown the
//   engine resolves each target's LAST writer process and stamps it with the
//   brushprov row id (guarded by forest ancestry). A pipeline's output file is
//   written by exactly that pipeline, requiring NO clock comparison — which matters
//   because process rows are materialized at file CLOSE (async FUSE release), out
//   of order with their pipeline. Pipelines with no write-redirect target are
//   recognizable via forest ancestry but not stamped to a specific pipeline
//   (per-pipeline column stays NULL). Two linkage gaps:
//   (a) only LITERAL output targets link — `> $OUT`/`> a/*.o`/`> $(cmd)` are
//       skipped, unresolvable offline;
//   (b) RELATIVE redirects (`> out.txt`) do not link in this cut (box-absolute
//       path matching required).
//   Both are documented gaps, not silent mislinks.
//   Readers: discover::{proc_pipeline, pipeline_procs, brushprov(.processes)}.
//
// NESTED-SHELL EXECUTION (D9 follow-on — see brush_sh below):
// The runner shadows /bin/sh, /usr/bin/sh, /bin/bash, /usr/bin/bash with the
// engine binary. When a tool inside the box execs `sh -c RECIPE` it lands in
// `brush_sh`, which runs the recipe through embedded brush-core. No real-shell
// fallback: a construct brush cannot run is a VISIBLE error (stderr + non-zero
// exit). Each nested invocation emits one `brush_prov_nested` record per pipeline
// via the FD broker (SARUN_BROKER — see runner::send_nested_prov), then runs
// pipeline-by-pipeline on a fresh shell with the invocation's cwd, $0, and $1..$N.
// The shim inherits fd 1/2 from its caller (typically make, which itself inherited
// the --inner brush's sinks) — no re-redirection; inner_brush owns capture once.
// Nested-pipeline processes are descendants of the top-level --inner, so
// finalize_brush_links (capture.rs) accepts them too.
//
// PARSER MODE BY INVOCATION NAME (B): reached as `bash` → BASH mode (bashisms:
// `[[ ]]`, `<(…)`, arrays, `${x//a/b}`, …); `sh`/`dash` → sh_mode(true), faithful
// POSIX. The top-level box brush stays sh_mode(true). Mode is the ONLY difference
// between the three; builtins come from box_builtins().
//
// Brush-core coverage (VERIFIED): POSIX builtins (cd, export, set, [, test,
// printf, echo, shift, …), variable assignment+expansion, arithmetic,
// if/case/for/while/until, functions, simple traps, here-docs/here-strings,
// one-char flags (-e/-u/-x/-o, set/unset). Bash-only constructs work in BASH mode;
// in sh-mode they surface as visible parse/exec errors — the /bin/sh-vs-/bin/bash
// contract.

use std::os::fd::AsRawFd;

use serde_json::json;
use serde_json::Value;

// ── shared shell builder ─────────────────────────────────────────────────────
// All three brush shells install the SAME builtin policy via box_builtins().
// Only the parser mode differs (see module header).

use std::ffi::OsString;

/// Map a finished child's `ExitStatus` to a shell exit code, honouring the
/// POSIX/GNU signal-death convention (T1): `128 + signo` (e.g. SIGSEGV → 139,
/// SIGINT → 130). `ExitStatus::code()` returns `None` on signal death; using
/// `.unwrap_or(…)` there would mask the crash with a bogus exit code.
fn child_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        // Signal death: GNU/bash convention is 128 + signal number.
        128 + sig
    } else {
        // Neither a normal exit nor a signal (should not happen on Unix); fall
        // back to a generic failure rather than inventing a success.
        127
    }
}

/// Run a coreutil on a fresh thread so it gets its own thread-local uucore Fluent bundle.
/// uucore's `LOCALIZER` is "set once per thread": a second distinct util on the same thread
/// would emit raw Fluent keys (e.g. `head-error-cannot-open`). Returns the exit code
/// (1 on spawn/join failure).
fn run_coreutil_localized(
    util: &'static str,
    env: Vec<(OsString, OsString)>,
    body: impl FnOnce() -> i32 + Send + 'static,
) -> i32 {
    match std::thread::Builder::new()
        .name(format!("sarun-{util}"))
        .spawn(move || {
            // Install the shell's LOGICAL environment for this worker thread
            // FIRST — before localization and the util body — so every runtime
            // env read uucore routes through `uucore::logical_env::get`
            // (LANG/LC_* for the Fluent bundle, VERSION_CONTROL/
            // SIMPLE_BACKUP_SUFFIX, _POSIX2_VERSION, NO_COLOR/TERM, …) sees the
            // vars the box shell `export`ed, NOT the engine process's own
            // environ. This is the principled seam that supersedes the old
            // CLONE_FS-style env hacks: the thread-local carries the env, so the
            // per-crate `env` entry params that some utils still take are a
            // harmless belt-and-suspenders, not the source of truth.
            uucore::logical_env::set_logical_env(env);
            brush_coreutils_builtins::init_localization(util);
            let code = body();
            uucore::logical_env::clear();
            code
        }) {
        Ok(h) => h.join().unwrap_or(1),
        Err(_) => 1,
    }
}

/// Snapshot the shell's LOGICAL environment — its exported variables — as
/// `(name, value)` `OsString` pairs, the shape the coreutil builtins take for
/// their `POSIXLY_CORRECT`-class knob reads. Only exported vars are included
/// (a child process/`printenv` would see exactly these), so a builtin sees the
/// same environment a forked coreutil would, NOT the engine process's own env.
fn exported_env_snapshot<SE: brush_core::extensions::ShellExtensions>(
    context: &brush_core::commands::ExecutionContext<'_, SE>,
) -> Vec<(OsString, OsString)> {
    context
        .shell
        .env()
        .iter_exported()
        .map(|(k, v)| {
            (
                k.clone().into(),
                v.value().to_cow_str(context.shell).to_string().into(),
            )
        })
        .collect()
}

/// `cat` — STREAM template: injected logical stdin/stdout, `splice(2)` fast path intact.
/// See [`run_coreutil_localized`] for thread-per-call localization isolation.
struct CatBuiltin;

impl brush_core::builtins::SimpleCommand for CatBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O cat builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() { argv.push(OsString::from(&name)); }

        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let cwd = context.shell.working_dir().to_path_buf();
        let code = run_coreutil_localized("uu_cat", exported_env_snapshot(&context), move || {
            use std::io::Write;
            use std::os::fd::{AsRawFd, BorrowedFd};
            let mut out = out;
            let mut err = err;
            let mut inp = inp;
            let out_raw = out.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            let in_raw = inp.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            // SAFETY: fd is owned by an OpenFile that outlives this call.
            let out_fd = out_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let in_fd = in_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let r = match uu_cat::cat(argv.into_iter(), &cwd, &mut out, out_fd, &mut inp, in_fd) {
                Ok(()) => 0,
                Err(e) => { let _ = writeln!(err, "{name}: {e}"); 1 }
            };
            let _ = out.flush();
            let _ = err.flush();
            r
        });
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// `head` — STREAM template: injected logical stdin/stdout/stderr. See [`run_coreutil_localized`].
struct HeadBuiltin;

impl brush_core::builtins::SimpleCommand for HeadBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O head builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() { argv.push(OsString::from(&name)); }

        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let cwd = context.shell.working_dir().to_path_buf();
        let code = run_coreutil_localized("uu_head", exported_env_snapshot(&context), move || {
            use std::io::Write;
            use std::os::fd::{AsRawFd, BorrowedFd};
            let mut out = out;
            let mut err = err;
            let mut inp = inp;
            let out_raw = out.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            let in_raw = inp.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            // SAFETY: fd is owned by an OpenFile that outlives this call.
            let out_fd = out_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let in_fd = in_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let r = match uu_head::head(argv.into_iter(), &cwd, &mut out, out_fd, &mut inp, in_fd) {
                Ok(()) => 0,
                Err(e) => { let _ = writeln!(err, "{name}: {e}"); 1 }
            };
            let _ = out.flush();
            let _ = err.flush();
            r
        });
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// `tail` — STREAM template like [`HeadBuiltin`]. See [`run_coreutil_localized`].
struct TailBuiltin;

impl brush_core::builtins::SimpleCommand for TailBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O tail builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() { argv.push(OsString::from(&name)); }

        let cwd = context.shell.working_dir().to_path_buf();
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let code = run_coreutil_localized("uu_tail", exported_env_snapshot(&context), move || {
            use std::io::Write;
            use std::os::fd::{AsRawFd, BorrowedFd};
            let mut out = out;
            let mut err = err;
            let mut inp = inp;
            let out_raw = out.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            let in_raw = inp.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            // SAFETY: fd is owned by an OpenFile that outlives this call.
            let out_fd = out_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let in_fd = in_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let r = match uu_tail::tail(argv.into_iter(), &cwd, &mut out, out_fd, &mut err, &mut inp, in_fd) {
                Ok(()) => 0,
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                    e.code()
                }
            };
            let _ = out.flush();
            let _ = err.flush();
            r
        });
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// `wc` — STREAM template like [`HeadBuiltin`]. See [`run_coreutil_localized`].
struct WcBuiltin;

impl brush_core::builtins::SimpleCommand for WcBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O wc builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() { argv.push(OsString::from(&name)); }

        let cwd = context.shell.working_dir().to_path_buf();
        // Shell's LOGICAL exported env: wc reads POSIXLY_CORRECT from this,
        // not the engine process's environment.
        let envv = exported_env_snapshot(&context);
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let code = run_coreutil_localized("uu_wc", envv.clone(), move || {
            use std::io::Write;
            use std::os::fd::{AsRawFd, BorrowedFd};
            let mut out = out;
            let mut err = err;
            let mut inp = inp;
            let out_raw = out.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            let in_raw = inp.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            // SAFETY: fd is owned by an OpenFile that outlives this call.
            let out_fd = out_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let in_fd = in_raw.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
            let r = match uu_wc::wc(argv.into_iter(), &cwd, &envv, &mut out, out_fd, &mut err, &mut inp, in_fd) {
                Ok(()) => 0,
                Err(e) => e.code(),
            };
            let _ = out.flush();
            let _ = err.flush();
            r
        });
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

// ── coreutil builtin templates ───────────────────────────────────────────────
// Each box coreutil builtin below is ONE macro invocation: the struct name, the
// util text (for `get_content`), the vendored entry path, and the thread label
// passed to `run_coreutil_localized` (which runs the util on a fresh thread so it
// gets its own thread-local uucore Fluent bundle and never `chdir`s the process).
// The macros differ only by the entry's argument SHAPE — whether it also takes
// the shell's logical cwd, its exported-env snapshot, and/or logical stdin:
//
//   info_builtin!        (args, out, err)                 — uname/whoami/basename/…
//   info_env_builtin!    (args, env, out, err)            — id/nproc
//   fs_builtin!          (args, cwd, out, err)            — mkdir/rmdir/chmod/…
//   fs_env_builtin!      (args, cwd, env, out, err)       — cp/readlink/mktemp
//   fs_builtin_stdin!    (args, cwd, out, err, stdin)     — rm/mv/ln (-i prompt)
//   stream_builtin!      (args, cwd, out, err, in)        — nl/tac/cut/tee
//   stream_env_builtin!  (args, cwd, env, out, err, in)   — uniq/sort
//
// cat/head/tail/wc stay hand-written above: their entries take a raw `BorrowedFd`
// for the splice(2) fast path, a shape shared by no other util. touch and tr are
// the only members of their shapes and stay hand-written below.
//
// Every macro renders a failed entry's error as `NAME: <msg>` on the logical
// stderr and returns the util's own `e.code()`; a fresh thread per call keeps the
// process cwd and localization untouched.

/// INFO shape: `(args, out, err)` — no cwd, no env, no stdin.
macro_rules! info_builtin {
    ($builtin:ident, $util:literal, $entry:path, $thread:literal) => {
        struct $builtin;

        impl brush_core::builtins::SimpleCommand for $builtin {
            fn get_content(
                name: &str,
                _content_type: brush_core::builtins::ContentType,
                _options: &brush_core::builtins::ContentOptions,
            ) -> Result<String, brush_core::error::Error> {
                Ok(format!("{name}: native injected-I/O {} builtin\n", $util))
            }

            fn execute<SE: brush_core::extensions::ShellExtensions,
                       I: Iterator<Item = S>, S: AsRef<str>>(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() { argv.push(OsString::from(&name)); }

                let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
                let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_localized($thread, exported_env_snapshot(&context), move || {
                    use std::io::Write;
                    let mut out = out;
                    let mut err = err;
                    let r = match $entry(argv.into_iter(), &mut out, &mut err) {
                        Ok(()) => 0,
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                            e.code()
                        }
                    };
                    let _ = out.flush();
                    let _ = err.flush();
                    r
                });
                Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
            }
        }
    };
}

/// INFO+ENV shape: `(args, env, out, err)`. `id` suppresses its SELinux context
/// suffix under `POSIXLY_CORRECT`; `nproc` scales by `OMP_NUM_THREADS`/
/// `OMP_THREAD_LIMIT` — both read from the shell's exported env, not the engine's.
macro_rules! info_env_builtin {
    ($builtin:ident, $util:literal, $entry:path, $thread:literal) => {
        struct $builtin;

        impl brush_core::builtins::SimpleCommand for $builtin {
            fn get_content(
                name: &str,
                _content_type: brush_core::builtins::ContentType,
                _options: &brush_core::builtins::ContentOptions,
            ) -> Result<String, brush_core::error::Error> {
                Ok(format!("{name}: native injected-I/O {} builtin\n", $util))
            }

            fn execute<SE: brush_core::extensions::ShellExtensions,
                       I: Iterator<Item = S>, S: AsRef<str>>(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() { argv.push(OsString::from(&name)); }

                let envv = exported_env_snapshot(&context);
                let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
                let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_localized($thread, envv.clone(), move || {
                    use std::io::Write;
                    let mut out = out;
                    let mut err = err;
                    let r = match $entry(argv.into_iter(), &envv, &mut out, &mut err) {
                        Ok(()) => 0,
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                            e.code()
                        }
                    };
                    let _ = out.flush();
                    let _ = err.flush();
                    r
                });
                Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
            }
        }
    };
}

/// FILESYSTEM shape: `(args, cwd, out, err)` — relative operands resolve against
/// the shell's logical cwd (captured before the worker runs; the process is never
/// `chdir`'d). No stdin.
macro_rules! fs_builtin {
    ($builtin:ident, $util:literal, $entry:path, $thread:literal) => {
        struct $builtin;

        impl brush_core::builtins::SimpleCommand for $builtin {
            fn get_content(
                name: &str,
                _content_type: brush_core::builtins::ContentType,
                _options: &brush_core::builtins::ContentOptions,
            ) -> Result<String, brush_core::error::Error> {
                Ok(format!("{name}: native injected-I/O {} builtin\n", $util))
            }

            fn execute<SE: brush_core::extensions::ShellExtensions,
                       I: Iterator<Item = S>, S: AsRef<str>>(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() { argv.push(OsString::from(&name)); }

                let cwd = context.shell.working_dir().to_path_buf();
                let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
                let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_localized($thread, exported_env_snapshot(&context), move || {
                    use std::io::Write;
                    let mut out = out;
                    let mut err = err;
                    let r = match $entry(argv.into_iter(), &cwd, &mut out, &mut err) {
                        Ok(()) => 0,
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                            e.code()
                        }
                    };
                    let _ = out.flush();
                    let _ = err.flush();
                    r
                });
                Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
            }
        }
    };
}

/// FILESYSTEM+ENV shape: `(args, cwd, env, out, err)`. `cp`/`readlink` read
/// `POSIXLY_CORRECT`, `mktemp` reads `$TMPDIR` (a relative one rooted at the
/// logical cwd) — all from the shell's exported env, not the engine's.
macro_rules! fs_env_builtin {
    ($builtin:ident, $util:literal, $entry:path, $thread:literal) => {
        struct $builtin;

        impl brush_core::builtins::SimpleCommand for $builtin {
            fn get_content(
                name: &str,
                _content_type: brush_core::builtins::ContentType,
                _options: &brush_core::builtins::ContentOptions,
            ) -> Result<String, brush_core::error::Error> {
                Ok(format!("{name}: native injected-I/O {} builtin\n", $util))
            }

            fn execute<SE: brush_core::extensions::ShellExtensions,
                       I: Iterator<Item = S>, S: AsRef<str>>(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() { argv.push(OsString::from(&name)); }

                let cwd = context.shell.working_dir().to_path_buf();
                let envv = exported_env_snapshot(&context);
                let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
                let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_localized($thread, envv.clone(), move || {
                    use std::io::Write;
                    let mut out = out;
                    let mut err = err;
                    let r = match $entry(argv.into_iter(), &cwd, &envv, &mut out, &mut err) {
                        Ok(()) => 0,
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                            e.code()
                        }
                    };
                    let _ = out.flush();
                    let _ = err.flush();
                    r
                });
                Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
            }
        }
    };
}

/// FILESYSTEM+STDIN shape: `(args, cwd, out, err, stdin)` — `rm -i`/`mv -i`/`ln -i`
/// read the y/N prompt from logical stdin, never the engine's fd 0.
macro_rules! fs_builtin_stdin {
    ($builtin:ident, $util:literal, $entry:path, $thread:literal) => {
        struct $builtin;

        impl brush_core::builtins::SimpleCommand for $builtin {
            fn get_content(
                name: &str,
                _content_type: brush_core::builtins::ContentType,
                _options: &brush_core::builtins::ContentOptions,
            ) -> Result<String, brush_core::error::Error> {
                Ok(format!("{name}: native injected-I/O {} builtin\n", $util))
            }

            fn execute<SE: brush_core::extensions::ShellExtensions,
                       I: Iterator<Item = S>, S: AsRef<str>>(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() { argv.push(OsString::from(&name)); }

                let cwd = context.shell.working_dir().to_path_buf();
                let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
                let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
                let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

                let code = run_coreutil_localized($thread, exported_env_snapshot(&context), move || {
                    use std::io::Write;
                    let mut out = out;
                    let mut err = err;
                    let stdin_src: Box<dyn std::io::BufRead> =
                        Box::new(std::io::BufReader::new(inp));
                    let r = match $entry(argv.into_iter(), &cwd, &mut out, &mut err, stdin_src) {
                        Ok(()) => 0,
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                            e.code()
                        }
                    };
                    let _ = out.flush();
                    let _ = err.flush();
                    r
                });
                Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
            }
        }
    };
}

/// STREAM shape: `(args, cwd, out, err, in)` — reads logical stdin, writes logical
/// stdout/stderr; relative file operands resolve against the shell's logical cwd.
macro_rules! stream_builtin {
    ($builtin:ident, $util:literal, $entry:path, $thread:literal) => {
        struct $builtin;

        impl brush_core::builtins::SimpleCommand for $builtin {
            fn get_content(
                name: &str,
                _content_type: brush_core::builtins::ContentType,
                _options: &brush_core::builtins::ContentOptions,
            ) -> Result<String, brush_core::error::Error> {
                Ok(format!("{name}: native injected-I/O {} builtin\n", $util))
            }

            fn execute<SE: brush_core::extensions::ShellExtensions,
                       I: Iterator<Item = S>, S: AsRef<str>>(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() { argv.push(OsString::from(&name)); }

                let cwd = context.shell.working_dir().to_path_buf();
                let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
                let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
                let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

                let code = run_coreutil_localized($thread, exported_env_snapshot(&context), move || {
                    use std::io::Write;
                    let mut out = out;
                    let mut err = err;
                    let mut inp = inp;
                    let r = match $entry(argv.into_iter(), &cwd, &mut out, &mut err, &mut inp) {
                        Ok(()) => 0,
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                            e.code()
                        }
                    };
                    let _ = out.flush();
                    let _ = err.flush();
                    r
                });
                Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
            }
        }
    };
}

/// STREAM+ENV shape: `(args, cwd, env, out, err, in)`. `sort` reads `$TMPDIR` (its
/// external-sort spill dir) and both read the locale knobs (`LC_ALL`/`LC_CTYPE`/
/// `LANG`) from the shell's exported env, not the engine's.
macro_rules! stream_env_builtin {
    ($builtin:ident, $util:literal, $entry:path, $thread:literal) => {
        struct $builtin;

        impl brush_core::builtins::SimpleCommand for $builtin {
            fn get_content(
                name: &str,
                _content_type: brush_core::builtins::ContentType,
                _options: &brush_core::builtins::ContentOptions,
            ) -> Result<String, brush_core::error::Error> {
                Ok(format!("{name}: native injected-I/O {} builtin\n", $util))
            }

            fn execute<SE: brush_core::extensions::ShellExtensions,
                       I: Iterator<Item = S>, S: AsRef<str>>(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() { argv.push(OsString::from(&name)); }

                let cwd = context.shell.working_dir().to_path_buf();
                let envv = exported_env_snapshot(&context);
                let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
                let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
                let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

                let code = run_coreutil_localized($thread, envv.clone(), move || {
                    use std::io::Write;
                    let mut out = out;
                    let mut err = err;
                    let mut inp = inp;
                    let r = match $entry(argv.into_iter(), &cwd, &envv, &mut out, &mut err, &mut inp) {
                        Ok(()) => 0,
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                            e.code()
                        }
                    };
                    let _ = out.flush();
                    let _ = err.flush();
                    r
                });
                Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
            }
        }
    };
}

// STREAM builtins.
stream_builtin!(NlBuiltin, "nl", uu_nl::nl, "uu_nl");
stream_builtin!(TacBuiltin, "tac", uu_tac::tac, "uu_tac");
stream_builtin!(CutBuiltin, "cut", uu_cut::cut, "uu_cut");
stream_builtin!(TeeBuiltin, "tee", uu_tee::tee_main, "uu_tee");
stream_env_builtin!(UniqBuiltin, "uniq", uu_uniq::uniq, "uu_uniq");
stream_env_builtin!(SortBuiltin, "sort", uu_sort::sort, "uu_sort");

/// `tr` — STREAM shape but its entry is `(args, out, err, in)` with NO cwd (tr has
/// no file operands), the sole member of that shape; hand-written rather than a
/// single-use macro. See [`run_coreutil_localized`].
struct TrBuiltin;

impl brush_core::builtins::SimpleCommand for TrBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O tr builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() { argv.push(OsString::from(&name)); }

        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let code = run_coreutil_localized("uu_tr", exported_env_snapshot(&context), move || {
            use std::io::Write;
            let mut out = out;
            let mut err = err;
            let mut inp = inp;
            let r = match uu_tr::tr(argv.into_iter(), &mut out, &mut err, &mut inp) {
                Ok(()) => 0,
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                    e.code()
                }
            };
            let _ = out.flush();
            let _ = err.flush();
            r
        });
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

// FILESYSTEM builtins.
fs_builtin!(MkdirBuiltin, "mkdir", uu_mkdir::mkdir_main, "uu_mkdir");
fs_builtin!(RmdirBuiltin, "rmdir", uu_rmdir::rmdir_main, "uu_rmdir");
fs_builtin!(RealpathBuiltin, "realpath", uu_realpath::realpath, "uu_realpath");
fs_builtin!(ChmodBuiltin, "chmod", uu_chmod::chmod_main, "uu_chmod");
fs_builtin!(ChownBuiltin, "chown", uu_chown::chown_main, "uu_chown");
fs_builtin!(InstallBuiltin, "install", uu_install::install_main, "uu_install");
fs_env_builtin!(CpBuiltin, "cp", uu_cp::cp, "uu_cp");
fs_env_builtin!(ReadlinkBuiltin, "readlink", uu_readlink::readlink, "uu_readlink");
fs_env_builtin!(MktempBuiltin, "mktemp", uu_mktemp::mktemp_main, "uu_mktemp");
fs_builtin_stdin!(RmBuiltin, "rm", uu_rm::rm_main, "uu_rm");
fs_builtin_stdin!(MvBuiltin, "mv", uu_mv::mv_main, "uu_mv");
fs_builtin_stdin!(LnBuiltin, "ln", uu_ln::ln_main, "uu_ln");

/// `touch` — FILESYSTEM shape but hand-written: the `-` operand passes the logical
/// fd 1 as a raw fd so `touch -` updates the logical stdout's referent, and its
/// entry also takes the shell's exported env for the obsolete `_POSIX2_VERSION`
/// knob. The sole member of that shape. See [`run_coreutil_localized`].
struct TouchBuiltin;

impl brush_core::builtins::SimpleCommand for TouchBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O touch builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() { argv.push(OsString::from(&name)); }

        let cwd = context.shell.working_dir().to_path_buf();
        let envv = exported_env_snapshot(&context);
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());

        let code = run_coreutil_localized("uu_touch", envv.clone(), move || {
            use std::io::Write;
            use std::os::fd::AsRawFd;
            let out = out;
            let mut err = err;
            // Raw fd for the logical stdout, for the `-` operand only;
            // borrowed for the call's duration (the OpenFile outlives it).
            let out_fd = out.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
            let r = match uu_touch::touch_main(argv.into_iter(), &cwd, &envv, out_fd, &mut err) {
                Ok(()) => 0,
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.is_empty() { let _ = writeln!(err, "{name}: {msg}"); }
                    e.code()
                }
            };
            let _ = err.flush();
            r
        });
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

// INFO builtins.
info_builtin!(BasenameBuiltin, "basename", uu_basename::basename, "uu_basename");
info_builtin!(DirnameBuiltin, "dirname", uu_dirname::dirname, "uu_dirname");
info_builtin!(SeqBuiltin, "seq", uu_seq::seq, "uu_seq");
info_builtin!(ExprBuiltin, "expr", uu_expr::expr, "uu_expr");
info_builtin!(UnameBuiltin, "uname", uu_uname::uname_main, "uu_uname");
info_builtin!(WhoamiBuiltin, "whoami", uu_whoami::whoami_main, "uu_whoami");
info_env_builtin!(IdBuiltin, "id", uu_id::id_main, "uu_id");
info_env_builtin!(NprocBuiltin, "nproc", uu_nproc::nproc_main, "uu_nproc");

/// `sarun` / `oaita` in-box builtins: re-exec the engine at /proc/self/exe so
/// `sarun …`/`oaita …` work with nothing on PATH inside the box. `oaita` maps
/// to the engine's `oaita` subcommand. Child fds are wired from the brush
/// context's OpenFiles (no process-wide dup2 — pipeline-safe).
struct EngineSelfCommand;

impl EngineSelfCommand {
    /// Dup the raw fd backing `of` into a `Stdio`; `None` → inherit this
    /// process's fd. The dup is owned by the returned `Stdio`.
    fn stdio_from(of: &Option<brush_core::openfiles::OpenFile>) -> Option<std::process::Stdio> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let of = of.as_ref()?;
        let bf = of.try_borrow_as_fd().ok()?;
        let dup = unsafe { libc::dup(bf.as_raw_fd()) };
        if dup < 0 { return None; }
        Some(unsafe { std::process::Stdio::from(OwnedFd::from_raw_fd(dup)) })
    }
}

impl brush_core::builtins::SimpleCommand for EngineSelfCommand {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: sarun in-box engine builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        // args includes argv[0]; drop it, prepend "oaita" for the oaita subcommand.
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        let rest = if argv.is_empty() { vec![] } else { argv.split_off(1) };
        let mut eargs: Vec<OsString> = Vec::new();
        if name == "oaita" { eargs.push(OsString::from("oaita")); }
        eargs.extend(rest);

        // Re-exec the engine via the ferried fd (SARUN_EXE) so `sarun`/`oaita`
        // work inside a closed rootfs where `/proc/self/exe`'s path is absent.
        let mut cmd = std::process::Command::new(crate::runner::in_box_self_exe());
        cmd.args(&eargs);
        if let Some(s) = Self::stdio_from(&context.try_fd(0)) { cmd.stdin(s); }
        if let Some(s) = Self::stdio_from(&context.try_fd(1)) { cmd.stdout(s); }
        if let Some(s) = Self::stdio_from(&context.try_fd(2)) { cmd.stderr(s); }
        let code = match cmd.status() {
            // T1: a signal death of the re-exec'd engine reports 128 + signo,
            // not a bogus "exited 1".
            Ok(s) => child_exit_code(s),
            Err(e) => { eprintln!("{name}: cannot exec engine: {e}"); 127 }
        };
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// `make`/`gmake` — embedded GNU make (in-process kati). Dispatched whenever
/// brush itself runs make (a recipe's recursive `$(MAKE)`, or `make` invoked by
/// a configure/cmake shell step), so it stays in THIS process at the brush
/// context's logical cwd instead of re-exec'ing the engine via the FUSE shadow.
/// The top-level `make` (spawned by the box runner) still goes through the
/// shadow/`main()` path — both share kati; this is just the in-process door.
struct MakeBuiltin;

impl brush_core::builtins::SimpleCommand for MakeBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: embedded GNU make (in-process kati)\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        // `args` already includes argv[0] (the command name), like the coreutil
        // builtins — use it directly; only synthesize a name if somehow empty.
        let name = context.command_name.clone();
        let mut argv: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        if argv.is_empty() {
            argv.push(name);
        }
        // Logical cwd from the brush context — NOT the process cwd.
        let cwd = context.shell.working_dir().to_path_buf();
        // Seed the make from THIS subshell's exported env, not the process env.
        // The parent make applied its exports to this subshell via the recipe's
        // export prefix, so iter_exported() here = the base box env + the
        // parent's exports — exactly what a forked child make would inherit, and
        // without any make ever mutating the shared process env.
        let shell_ref = &*context.shell;
        let seed_env: Vec<(std::ffi::OsString, std::ffi::OsString)> = shell_ref
            .env()
            .iter_exported()
            .map(|(k, v)| {
                (
                    std::ffi::OsString::from(k.clone()),
                    std::ffi::OsString::from(v.value().to_cow_str(shell_ref).into_owned()),
                )
            })
            .collect();
        // fd 1/2 twice each: one handle for make's own messages, one as the
        // recipe/diagnostic sink kati writes through (set_recipe_out/err).
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let recipe_out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let recipe_err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let stdin: Option<Box<dyn std::io::Read>> =
            context.try_fd(0).map(|f| Box::new(f) as Box<dyn std::io::Read>);
        let code = crate::katirun::make_builtin(
            &argv, &cwd, &seed_env, out, err, Box::new(recipe_out), Box::new(recipe_err),
            stdin,
        );
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// `ninja` — embedded n2 (in-process). Dispatched when brush runs `ninja` (a
/// cmake/configure step, or a recipe), so a top-level box ninja stays in this
/// process. See katirun::ninja_builtin for the current logical-cwd limitation.
struct NinjaBuiltin;

impl brush_core::builtins::SimpleCommand for NinjaBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: embedded ninja (in-process n2)\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        if argv.is_empty() {
            argv.push(name);
        }
        let cwd = context.shell.working_dir().to_path_buf();
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let code = crate::n2run::ninja_builtin(&argv, &cwd, out, err);
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// `edit PATH` — foreground, in-process Ratatui editor. The editor owns the
/// controlling terminal directly, so command redirections do not become its
/// display transport. Pipeline and non-interactive execution are rejected
/// before terminal state changes.
struct EditBuiltin;

impl brush_core::builtins::SimpleCommand for EditBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!(
            "{name} PATH: open PATH in sarun's in-process relation-aware editor\n"
        ))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        use brush_core::openfiles::OpenFile;
        use std::io::Write;

        let mut argv = args
            .map(|argument| argument.as_ref().to_string())
            .collect::<Vec<_>>();
        if argv.first() == Some(&context.command_name) {
            argv.remove(0);
        }
        if argv.first().map(String::as_str) == Some("--") {
            argv.remove(0);
        }
        let fail = |context: &brush_core::commands::ExecutionContext<'_, SE>, message: &str, code| {
            let mut stderr = context.stderr();
            let _ = writeln!(stderr, "edit: {message}");
            brush_core::results::ExecutionResult::new(code)
        };
        if argv.len() != 1 {
            return Ok(fail(&context, "usage: edit PATH", 2));
        }
        if !context.shell.options().interactive {
            return Ok(fail(
                &context,
                "requires a foreground interactive Brush session",
                1,
            ));
        }
        let pipeline_io = |file: Option<OpenFile>| {
            matches!(file, Some(OpenFile::PipeReader(_) | OpenFile::PipeWriter(_)))
        };
        if context.spawned_pipeline_stage
            || pipeline_io(context.try_fd(0))
            || pipeline_io(context.try_fd(1))
        {
            return Ok(fail(&context, "cannot run in a pipeline", 1));
        }

        let operand = std::path::PathBuf::from(&argv[0]);
        let path = if operand.is_absolute() {
            operand
        } else {
            context.shell.working_dir().join(operand)
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::editor::run_standalone(path)
        }));
        match result {
            Ok(Ok(())) => Ok(brush_core::results::ExecutionResult::new(0)),
            Ok(Err(error)) => Ok(fail(&context, &error.to_string(), 1)),
            Err(_) => Ok(fail(
                &context,
                "editor panicked; terminal state was restored",
                1,
            )),
        }
    }
}

/// All box brush builtins — the single builtin-policy point for every box brush
/// shell (top-level box, nested `sh -c`, make/n2 recipes). The stream/filter
/// coreutils are registered UNCONDITIONALLY: each runs on its own fresh thread
/// (`run_coreutil_localized`) with a thread-local uucore Fluent bundle, so N
/// utils in one process — even concurrent under `make -j` — never poison each
/// other's localization. (Vendored uucore made `LOCALIZER` and its resource
/// caches thread-local; the old process-global `OnceLock` that once forced a
/// fork+exec gate for make recipes is gone.)
fn box_builtins<SE: brush_core::extensions::ShellExtensions>(
) -> std::collections::HashMap<String, brush_core::builtins::Registration<SE>> {
    use brush_core::builtins::{builtin, simple_builtin};
    let mut m: std::collections::HashMap<String, brush_core::builtins::Registration<SE>>
        = std::collections::HashMap::new();
    // Stream/filter + info coreutils (see the macro block above for their shapes).
    m.insert("cat".to_string(), simple_builtin::<CatBuiltin, SE>());
    m.insert("head".to_string(), simple_builtin::<HeadBuiltin, SE>());
    m.insert("tail".to_string(), simple_builtin::<TailBuiltin, SE>());
    m.insert("wc".to_string(), simple_builtin::<WcBuiltin, SE>());
    m.insert("nl".to_string(), simple_builtin::<NlBuiltin, SE>());
    m.insert("tac".to_string(), simple_builtin::<TacBuiltin, SE>());
    m.insert("basename".to_string(), simple_builtin::<BasenameBuiltin, SE>());
    m.insert("dirname".to_string(), simple_builtin::<DirnameBuiltin, SE>());
    m.insert("seq".to_string(), simple_builtin::<SeqBuiltin, SE>());
    m.insert("expr".to_string(), simple_builtin::<ExprBuiltin, SE>());
    m.insert("tr".to_string(), simple_builtin::<TrBuiltin, SE>());
    m.insert("cut".to_string(), simple_builtin::<CutBuiltin, SE>());
    m.insert("uniq".to_string(), simple_builtin::<UniqBuiltin, SE>());
    m.insert("sort".to_string(), simple_builtin::<SortBuiltin, SE>());
    m.insert("uname".to_string(), simple_builtin::<UnameBuiltin, SE>());
    m.insert("nproc".to_string(), simple_builtin::<NprocBuiltin, SE>());
    m.insert("id".to_string(), simple_builtin::<IdBuiltin, SE>());
    m.insert("whoami".to_string(), simple_builtin::<WhoamiBuiltin, SE>());
    // Filesystem-op builtins: each runs on a fresh thread and honors logical cwd.
    m.insert("cp".to_string(), simple_builtin::<CpBuiltin, SE>());
    m.insert("mkdir".to_string(), simple_builtin::<MkdirBuiltin, SE>());
    m.insert("rmdir".to_string(), simple_builtin::<RmdirBuiltin, SE>());
    m.insert("rm".to_string(), simple_builtin::<RmBuiltin, SE>());
    m.insert("mv".to_string(), simple_builtin::<MvBuiltin, SE>());
    m.insert("ln".to_string(), simple_builtin::<LnBuiltin, SE>());
    m.insert("touch".to_string(), simple_builtin::<TouchBuiltin, SE>());
    m.insert("readlink".to_string(), simple_builtin::<ReadlinkBuiltin, SE>());
    m.insert("realpath".to_string(), simple_builtin::<RealpathBuiltin, SE>());
    m.insert("mktemp".to_string(), simple_builtin::<MktempBuiltin, SE>());
    m.insert("tee".to_string(), simple_builtin::<TeeBuiltin, SE>());
    m.insert("chmod".to_string(), simple_builtin::<ChmodBuiltin, SE>());
    m.insert("chown".to_string(), simple_builtin::<ChownBuiltin, SE>());
    m.insert("install".to_string(), simple_builtin::<InstallBuiltin, SE>());
    // Embedded GNU make: keeps recursive `$(MAKE)` and configure/cmake-invoked
    // make IN-PROCESS (see MakeBuiltin) instead of re-exec'ing the engine.
    m.insert("make".to_string(), simple_builtin::<MakeBuiltin, SE>());
    m.insert("gmake".to_string(), simple_builtin::<MakeBuiltin, SE>());
    m.insert("ninja".to_string(), simple_builtin::<NinjaBuiltin, SE>());
    // BashMode shell builtins overwrite any overlapping coreutil names (highest priority).
    m.extend(brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode));
    m.insert("edit".to_string(), simple_builtin::<EditBuiltin, SE>());
    // In-box engine entry points via /proc/self/exe (no PATH shadow needed).
    m.insert("sarun".to_string(), simple_builtin::<EngineSelfCommand, SE>());
    m.insert("oaita".to_string(), simple_builtin::<EngineSelfCommand, SE>());
    // find/xargs: vendored findutils fork; always present (see find_builtin/xargs_builtin).
    m.insert(
        "find".to_string(),
        builtin::<crate::find_builtin::FindBuiltin, SE>(),
    );
    m.insert(
        "xargs".to_string(),
        builtin::<crate::xargs_builtin::XargsBuiltin, SE>(),
    );
    // env/printenv: clone-and-dispatch launchers (see exec_wrappers).
    m.insert(
        "env".to_string(),
        builtin::<crate::exec_wrappers::EnvCommand, SE>(),
    );
    m.insert(
        "printenv".to_string(),
        builtin::<crate::exec_wrappers::PrintenvCommand, SE>(),
    );
    // nice/setsid/nohup: clone-and-dispatch with a LaunchState disposition
    // (priority/session/SIGHUP) that materializes only in a forked child.
    m.insert(
        "nice".to_string(),
        builtin::<crate::exec_wrappers::NiceCommand, SE>(),
    );
    m.insert(
        "setsid".to_string(),
        builtin::<crate::exec_wrappers::SetsidCommand, SE>(),
    );
    m.insert(
        "nohup".to_string(),
        builtin::<crate::exec_wrappers::NohupCommand, SE>(),
    );
    m
}

/// Project finite argument domains from the builtin parser definitions into
/// grammar data. This is the sole Brush-builtin/grammar adapter: it is driven
/// entirely by each registration's declarative Clap definition and contains no
/// command-name cases. The parser, editor, and interactive UI only see the
/// resulting typed relation values.
pub(crate) fn builtin_command_signatures(
) -> &'static [crate::prolog::DocumentCommandSignature] {
    static SIGNATURES: std::sync::OnceLock<Vec<crate::prolog::DocumentCommandSignature>> =
        std::sync::OnceLock::new();
    SIGNATURES.get_or_init(|| {
        let mut registrations = box_builtins::<
            brush_core::extensions::DefaultShellExtensions,
        >()
        .into_iter()
        .collect::<Vec<_>>();
        registrations.sort_by(|left, right| left.0.cmp(&right.0));

        registrations
            .into_iter()
            .filter_map(|(command_name, registration)| {
                let definition = registration.definition_func?;
                let command = definition();
                let following_arguments = command
                    .get_arguments()
                    .filter(|argument| {
                        !argument.is_hide_set() && argument.get_action().takes_values()
                    })
                    .flat_map(|argument| {
                        let values = argument
                            .get_value_parser()
                            .possible_values()
                            .into_iter()
                            .flatten()
                            .filter(|value| !value.is_hide_set())
                            .map(|value| {
                                let text = value.get_name().to_string();
                                let description = value
                                    .get_help()
                                    .or_else(|| argument.get_help())
                                    .map_or_else(
                                        || format!("value for {}", argument.get_id()),
                                        ToString::to_string,
                                    );
                                crate::prolog::CommandArgumentValue {
                                    semantic: crate::prolog::RelationValue::Compound(
                                        "builtin_argument".into(),
                                        vec![
                                            crate::prolog::RelationValue::String(
                                                command_name.clone(),
                                            ),
                                            crate::prolog::RelationValue::String(
                                                argument.get_id().to_string(),
                                            ),
                                            crate::prolog::RelationValue::String(text.clone()),
                                        ],
                                    ),
                                    text,
                                    description: crate::prolog::RelationValue::String(description),
                                    preference: 30,
                                }
                            })
                            .collect::<Vec<_>>();

                        let mut flags = Vec::new();
                        if let Some(short) = argument.get_short() {
                            flags.push(format!("-{short}"));
                        }
                        if let Some(long) = argument.get_long() {
                            flags.push(format!("--{long}"));
                        }
                        flags.into_iter().filter_map(move |flag| {
                            (!values.is_empty()).then(|| {
                                crate::prolog::CommandFollowingArgument {
                                    flag,
                                    values: values.clone(),
                                    syntax: "builtin_argument".into(),
                                }
                            })
                        })
                    })
                    .collect::<Vec<_>>();
                (!following_arguments.is_empty()).then_some(
                    crate::prolog::DocumentCommandSignature {
                        command: command_name,
                        following_arguments,
                    },
                )
            })
            .collect()
    })
}

/// Build a box brush shell. `sh_mode=true` → POSIX; `false` → BASH mode.
/// `shell_name`/`positional`/`cwd` are $0/$1../$PWD; `None` → brush-core defaults.
async fn build_box_shell(
    sh_mode: bool,
    shell_name: Option<String>,
    positional: Option<Vec<String>>,
    cwd: Option<std::path::PathBuf>,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    build_box_shell_opt(sh_mode, shell_name, positional, cwd, false).await
}

/// Like `build_box_shell` but marks the shell interactive. The builder wires up
/// history + job-control during `build()`; setting the flag afterwards leaves
/// `shell.history = None`, causing brush-interactive's reedline hinter to panic
/// on first keystroke.
async fn build_box_shell_opt(
    sh_mode: bool,
    shell_name: Option<String>,
    positional: Option<Vec<String>>,
    cwd: Option<std::path::PathBuf>,
    interactive: bool,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    build_box_shell_full(sh_mode, shell_name, positional, cwd, interactive).await
}

async fn build_box_shell_full(
    sh_mode: bool,
    shell_name: Option<String>,
    positional: Option<Vec<String>>,
    cwd: Option<std::path::PathBuf>,
    interactive: bool,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    install_shell_var_recorder();
    install_pipeline_observer();
    // Shell work reports into the same activity feed / stall watchdog as
    // the embedded makes — a pure-shell hang (configure!) must be visible.
    crate::katirun::start_activity_reporting();
    // bon's builder is typestate-typed; we can't conditionally chain setters.
    // Passing the inner value (unwrapped from Option with a sensible default)
    // reproduces brush-core's own unset default for each field.
    let cwd = cwd.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    });
    let mut shell = brush_core::Shell::builder()
        .sh_mode(sh_mode)
        .interactive(interactive)
        .builtins(box_builtins())
        .shell_name(shell_name.unwrap_or_default())
        .shell_args(positional.unwrap_or_default())
        .working_dir(cwd)
        .build().await?;
    // sarun: snoop self-shadowed /bin/sh scripts in-process instead of forking
    // ourselves. Inherited by every subshell clone (recipes, nested, -exec).
    shell.set_exec_interposer(std::sync::Arc::new(SnoopInterposer));
    Ok(shell)
}

/// dup2 the box's FUSE stdout/stderr sinks onto fd 1/2 so brush and every forked
/// binary write to the overlay. Returns false if the sinks can't be opened.
fn redirect_stdio_to_sinks() -> bool {
    let out = std::fs::OpenOptions::new().write(true).open("/.slopbox-stdout");
    let err = std::fs::OpenOptions::new().write(true).open("/.slopbox-stderr");
    let (out, err) = match (out, err) {
        (Ok(o), Ok(e)) => (o, e),
        _ => {
            eprintln!("sarun-engine inner: -b capture sinks unavailable");
            return false;
        }
    };
    unsafe {
        if libc::dup2(out.as_raw_fd(), 1) < 0 { return false; }
        if libc::dup2(err.as_raw_fd(), 2) < 0 { return false; }
    }
    // out/err drop here; dup'd fd 1/2 keep the sinks open.
    true
}

/// Build the brush script from the box argv. `sh/bash/dash -c SCRIPT` extracts
/// SCRIPT directly; anything else is reconstructed into a quoted command string.
/// The shell IDENTITY of a box command: "bash" when the user's argv[0]
/// basename is bash (BASH-mode parse: `[[ ]]`, `<(…)`, arrays), else
/// "sh" (faithful POSIX). The top-level brush must parse in the mode
/// the user actually invoked — `sarun run -b -- bash -c 'cat <(x)'`
/// used to die with a POSIX parse error because the `bash -c` wrapper
/// was unwrapped by script_from_argv and the bash-ness dropped.
pub(crate) fn shell_name_from_argv(cmd: &[String]) -> &'static str {
    let base = std::path::Path::new(&cmd[0])
        .file_name().and_then(|s| s.to_str()).unwrap_or(&cmd[0]);
    if base == "bash" { "bash" } else { "sh" }
}

pub(crate) fn script_from_argv(cmd: &[String]) -> String {
    let base = std::path::Path::new(&cmd[0])
        .file_name().and_then(|s| s.to_str()).unwrap_or(&cmd[0]);
    if matches!(base, "sh" | "bash" | "dash" | "brush") {
        if let Some(pos) = cmd.iter().position(|a| a == "-c") {
            if let Some(script) = cmd.get(pos + 1) {
                return script.clone();
            }
        }
    }
    // Not a `sh -c` form: reconstruct with quoting so brush sees the same words.
    cmd.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ")
}

/// POSIX single-quote escaping: wrap in `'…'`, escaping `'` as `'\''`.
/// Safe alnum/punct words pass through unquoted.
fn shell_quote(w: &str) -> String {
    let safe = !w.is_empty() && w.chars().all(|c|
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '=' | ':' | '+'));
    if safe { return w.to_string(); }
    let mut s = String::from("'");
    for c in w.chars() {
        if c == '\'' { s.push_str("'\\''"); } else { s.push(c); }
    }
    s.push('\'');
    s
}

/// Per-pipeline provenance records for one complete-command (CompoundList):
/// command string, pipeline structure, and literal write-redirect targets.
/// Emitted as FRAME_PROV BEFORE brush runs the complete-command (D9).
pub(crate) fn complete_command_records(complete: &brush_parser::ast::CompoundList) -> Vec<Value> {
    use brush_parser::ast;
    let mut out = vec![];
    // CompleteCommand = CompoundList = Vec<CompoundListItem(AndOrList, sep)>.
    for item in &complete.0 {
        let andor = &item.0;
        // The first pipeline plus any && / || continuations.
        let mut pipelines: Vec<&ast::Pipeline> = vec![&andor.first];
        for cont in &andor.additional {
            match cont {
                ast::AndOr::And(p) | ast::AndOr::Or(p) => pipelines.push(p),
            }
        }
        for pl in pipelines {
            let stages: Vec<Value> = pl.seq.iter().map(stage_record).collect();
            out.push(json!({
                "cmd": pl.to_string(),
                "bang": pl.bang,
                "stages": pl.seq.len(),
                "stage_detail": stages,
                // Literal write-redirect targets (`>`, `>>`, `>|`, `&>`): the
                // engine uses these as the exact, race-free brush↔process link.
                // Words requiring expansion are skipped (unresolvable offline).
                "out_targets": pipeline_out_targets(pl),
            }));
        }
    }
    out
}

/// Literal write-redirect target paths across all stages of a pipeline.
/// Targets needing expansion (`$`, `` ` ``, `*`, `?`, `[`, `~`) are skipped.
fn pipeline_out_targets(pl: &brush_parser::ast::Pipeline) -> Vec<String> {
    use brush_parser::ast::Command;
    let mut out = vec![];
    for cmd in &pl.seq {
        if let Command::Simple(s) = cmd {
            if let Some(p) = &s.prefix { collect_out_targets(&p.0, &mut out); }
            if let Some(suf) = &s.suffix { collect_out_targets(&suf.0, &mut out); }
        }
    }
    out
}

fn collect_out_targets(items: &[brush_parser::ast::CommandPrefixOrSuffixItem],
                       out: &mut Vec<String>) {
    use brush_parser::ast::CommandPrefixOrSuffixItem as It;
    use brush_parser::ast::{IoRedirect, IoFileRedirectKind as K, IoFileRedirectTarget as T};
    for it in items {
        let It::IoRedirect(io) = it else { continue };
        match io {
            IoRedirect::File(_, kind, T::Filename(w)) => {
                if matches!(kind, K::Write | K::Append | K::Clobber | K::ReadAndWrite) {
                    if let Some(p) = literal_word(w) { out.push(p); }
                }
            }
            IoRedirect::OutputAndError(w, _) => {
                if let Some(p) = literal_word(w) { out.push(p); }
            }
            _ => {}
        }
    }
}

/// Return the word as a literal path string if it needs no expansion; else `None`.
fn literal_word(w: &brush_parser::ast::Word) -> Option<String> {
    let s = w.to_string();
    if s.is_empty() || s.chars().any(|c| matches!(c, '$' | '`' | '*' | '?' | '[' | '~')) {
        return None;
    }
    Some(s)
}

fn scan_items(items: &[brush_parser::ast::CommandPrefixOrSuffixItem],
              words: &mut Vec<String>, redirects: &mut usize) {
    use brush_parser::ast::CommandPrefixOrSuffixItem as It;
    for it in items {
        match it {
            It::IoRedirect(_) => *redirects += 1,
            It::Word(w) => words.push(w.to_string()),
            It::AssignmentWord(_, w) => words.push(w.to_string()),
            _ => {}
        }
    }
}

/// Per-stage provenance detail: command words (simple command) and redirect count.
fn stage_record(cmd: &brush_parser::ast::Command) -> Value {
    use brush_parser::ast;
    match cmd {
        ast::Command::Simple(s) => {
            let mut words: Vec<String> = vec![];
            let mut redirects = 0usize;
            if let Some(p) = &s.prefix { scan_items(&p.0, &mut words, &mut redirects); }
            if let Some(w) = &s.word_or_name { words.push(w.to_string()); }
            if let Some(suf) = &s.suffix { scan_items(&suf.0, &mut words, &mut redirects); }
            json!({"kind": "simple", "words": words, "redirects": redirects})
        }
        ast::Command::Compound(_, redirs) => json!({
            "kind": "compound",
            "redirects": redirs.as_ref().map(|r| r.0.len()).unwrap_or(0),
            "text": cmd.to_string(),
        }),
        ast::Command::Function(_) => json!({"kind": "function", "text": cmd.to_string()}),
        ast::Command::ExtendedTest(..) => json!({"kind": "extended_test",
                                                 "text": cmd.to_string()}),
    }
}

/// Send one FRAME_PROV over the box channel. Best-effort: drop on failure rather
/// than emit a malformed frame (a missing row is recoverable; a corrupt one is not).
fn send_prov(conn_fd: i32, rec: &Value) {
    // serialize failure: drop (not emit empty/malformed). `Value` should always
    // serialize; a failure is exceptional — log and skip rather than corrupt.
    let payload = match serde_json::to_vec(rec) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sarun-engine brush: dropping provenance frame, \
                       JSON serialize failed: {e}");
            return;
        }
    };
    let frame = crate::frames::encode(crate::frames::FRAME_PROV, &payload);
    unsafe { libc::write(conn_fd, frame.as_ptr().cast(), frame.len()); }
}

/// Box body for `-b` brush shells. Returns the exit code. Errors are visible on
/// stderr and yield non-zero — no silent /bin/sh fallback.
pub fn inner_brush(conn_fd: i32, cmd: Vec<String>) -> i32 {
    // 1. Capture wiring: save real fd 1/2, dup sinks onto fd 1/2 (brush + forks
    //    write captured), MUTE our pid so echo readback isn't re-recorded, then
    //    spawn the ECHO reader that replays captured bytes to the saved fd 1/2.
    let real_out = unsafe { libc::dup(1) };
    let real_err = unsafe { libc::dup(2) };
    if !redirect_stdio_to_sinks() {
        return 127;
    }
    // MUTE our pid: writes are echoed (live) but not re-recorded.
    let pidfd = crate::runner::pidfd_open_pub(std::process::id() as i32);
    if pidfd >= 0 {
        crate::runner::send_frame_pub(
            conn_fd, &crate::frames::encode(crate::frames::FRAME_MUTE, &[]), Some(pidfd));
        unsafe { libc::close(pidfd); }
    }
    // ECHO reader: replays captured bytes to saved real fd 1/2 (live output).
    // Stops on ECHO_DONE or channel close.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    let rfd = conn_fd;
    let reader = std::thread::spawn(move || {
        let mut buf: Vec<u8> = vec![];
        let mut tmp = [0u8; 65536];
        loop {
            // recvmsg (not read) so SCM_RIGHTS fds from FRAME_CONN reach the broker;
            // one fd per recvmsg, associated with the first FRAME_CONN in the batch.
            let mut got_fd: Option<i32> = None;
            let n = crate::runner::recv_box_frame_bytes_pub(
                rfd, &mut tmp, &mut got_fd);
            if n <= 0 { break; }
            buf.extend_from_slice(&tmp[..n as usize]);
            let (frames, used) = crate::frames::decode(&buf);
            buf.drain(..used);
            for (ft, payload) in frames {
                if ft == crate::frames::FRAME_CONN {
                    if let Some(fd) = got_fd.take() {
                        crate::runner::runner_broker_handoff_pub(fd);
                    }
                    continue;
                }
                if ft == crate::frames::FRAME_ECHO && !payload.is_empty() {
                    let realfd = if payload[0] == 1 { real_err } else { real_out };
                    unsafe { libc::write(realfd, payload[1..].as_ptr().cast(),
                                         payload.len() - 1); }
                } else if ft == crate::frames::FRAME_ECHO_DONE {
                    done2.store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
            if let Some(fd) = got_fd { unsafe { libc::close(fd); } }
        }
    });

    // 2. Run through embedded brush (async; tokio multi-thread runtime).
    //    Parse errors and execution errors surface visibly.
    let script = script_from_argv(&cmd);
    let sh_mode = shell_name_from_argv(&cmd) != "bash";
    // 64 MiB worker stacks: the box's top command may be a `make` whose kati
    // parse/eval recurses deeply on big Makefiles (busybox/kernel), overflowing
    // the default 2 MiB tokio worker stack.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(64 * 1024 * 1024)
        .enable_all().build();
    let code = match rt {
        Ok(rt) => rt.block_on(run_brush(conn_fd, script, sh_mode)),
        Err(e) => { eprintln!("sarun-engine inner: -b runtime: {e}"); 127 }
    };

    // 3. Teardown: restore fd 1/2 to the saved terminal so late eprints surface;
    //    wait for the ECHO reader to drain (triggers ECHO_DONE), then UNMUTE.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    unsafe { libc::dup2(real_out, 1); libc::dup2(real_err, 2); }
    while !done.load(std::sync::atomic::Ordering::SeqCst)
        && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    crate::runner::send_frame_pub(
        conn_fd, &crate::frames::encode(crate::frames::FRAME_UNMUTE, &[]), None);
    let _ = reader;
    code
}

// ── brush-sh shim (D9 follow-on: nested shells are brush) ────────────────────
// runner::run shadows /bin/sh, /usr/bin/sh, /bin/bash, /usr/bin/bash with the
// engine binary and sets SARUN_BRUSH_SH=1. When a nested tool execs
// `/bin/sh -c RECIPE` it lands here; brush-core runs the recipe directly.

/// True when SARUN_BRUSH_SH=1 and argv[0] basename is a shell name.
/// Checked by main() before normal subcommand dispatch.
pub fn is_brush_sh_invocation() -> bool {
    if std::env::var("SARUN_BRUSH_SH").as_deref() != Ok("1") {
        return false;
    }
    let arg0 = std::env::args().next().unwrap_or_default();
    let base = std::path::Path::new(&arg0)
        .file_name().and_then(|s| s.to_str()).unwrap_or("");
    matches!(base, "sh" | "bash" | "dash")
}

/// Public, standalone Brush CLI. Unlike `sarun run -b`, this executes in the
/// invoking process on the host and needs neither an engine nor a box. Argv is
/// first related to the central action grammar without joining its elements;
/// the resulting strings are then ordinary shell argv after `sarun brush`.
#[cfg(test)]
pub fn cli(arguments: &[String]) -> i32 {
    let mut source = Vec::with_capacity(arguments.len() + 1);
    source.push("brush".to_string());
    source.extend_from_slice(arguments);
    let invocation = match crate::parser::parse_argv(&source, &crate::parser::EmptyContext) {
        crate::parser::ParseResult::Invocation(invocation)
            if invocation.action == "brush"
                && invocation.target == crate::parser::ActionTarget::CliLocal => invocation,
        crate::parser::ParseResult::BackendError(error) => {
            eprintln!("sarun brush: parser: {error}");
            return 2;
        }
        _ => {
            eprintln!(
                "usage: sarun brush [SHELL_ARGS...]\n       sarun brush -c SCRIPT [NAME [ARG...]]\n       sarun brush SCRIPT [ARG...]"
            );
            return 2;
        }
    };
    execute_cli(invocation)
}

pub fn execute_cli(invocation: crate::parser::Invocation) -> i32 {
    let mut shell_argv = vec!["bash".to_string()];
    for value in invocation.args {
        match value {
            crate::parser::ArgValue::String(value) => shell_argv.push(value),
            _ => {
                eprintln!("sarun brush: grammar returned a non-string shell argument");
                return 2;
            }
        }
    }
    if shell_argv.len() == 1 {
        shell_argv.push("-i".into());
    }
    brush_sh(&shell_argv)
}

/// Nested-shell shim. `argv` is the full process argv (argv[0] = shell name).
/// Parses `-c SCRIPT` or a script-file form, emits nested provenance, then runs
/// through brush-core. No real-shell fallback: errors are visible + non-zero.
pub fn brush_sh(argv: &[String]) -> i32 {
    if argv.is_empty() {
        eprintln!("sarun-engine brush-sh: empty argv");
        return 2;
    }
    // In-process self-unwind sink for `stuck` (no-op unless the runner set
    // SARUN_STUCK_FD): lets a spinning box thread dump its own symbolized
    // stack, which external gdb cannot under the sud loader.
    crate::selfbt::install();
    let arg0 = &argv[0];
    let base = std::path::Path::new(arg0)
        .file_name().and_then(|s| s.to_str()).unwrap_or("sh").to_string();
    // Do NOT touch fd 1/2: the shim inherits them from its caller (typically
    // make/system()), which already point at the top-level inner_brush's FUSE
    // sinks. Re-redirecting would double-record and stamp writes to the wrong
    // process row. inner_brush owns capture; we don't.

    // Parse leading flags. brush-core honors -e/-u/-x/-o NAME (applied via `set`).
    // -l/--login is not supported inside a box — error visibly.
    let mut idx = 1;
    let mut set_flags: Vec<String> = vec![];   // e.g. ["-e","-u","-x"]
    let mut set_o: Vec<String> = vec![];       // names from `-o NAME`
    let mut unset_o: Vec<String> = vec![];     // names from `+o NAME`
    let mut have_c = false;
    let mut interactive = false;
    while idx < argv.len() {
        let a = &argv[idx];
        if a == "--" { idx += 1; break; }
        if a == "-c" { have_c = true; idx += 1; break; }
        if a == "-o" || a == "+o" {
            let Some(name) = argv.get(idx + 1) else {
                eprintln!("sarun-engine brush-sh: {a} requires an option name");
                return 2;
            };
            if a == "-o" { set_o.push(name.clone()); }
            else { unset_o.push(name.clone()); }
            idx += 2; continue;
        }
        // -i/--interactive: enter the brush-interactive reedline REPL.
        if a == "-i" || a == "--interactive" {
            interactive = true;
            idx += 1; continue;
        }
        if a == "-l" || a == "--login" {
            eprintln!("sarun-engine brush-sh: {a} not supported inside a brush box");
            return 2;
        }
        // Grouped flags like `-eux`; `-` alone is the stdin marker, ends flags.
        if a == "-" { break; }
        if let Some(rest) = a.strip_prefix('-') {
            // Each char must be a known POSIX-ish flag.
            for c in rest.chars() {
                match c {
                    'e' | 'u' | 'x' | 'v' | 'f' | 'n' | 'h' | 'm' | 'b' | 'C' | 'a' =>
                        set_flags.push(format!("-{c}")),
                    'c' => { have_c = true; }
                    _ => {
                        eprintln!("sarun-engine brush-sh: unsupported flag -{c}");
                        return 2;
                    }
                }
            }
            idx += 1;
            if have_c { break; }  // -c terminates flag parse
            continue;
        }
        if let Some(rest) = a.strip_prefix('+') {
            for c in rest.chars() {
                match c {
                    'e' | 'u' | 'x' | 'v' | 'f' | 'n' | 'h' | 'm' | 'b' | 'C' | 'a' =>
                        set_flags.push(format!("+{c}")),
                    _ => {
                        eprintln!("sarun-engine brush-sh: unsupported flag +{c}");
                        return 2;
                    }
                }
            }
            idx += 1; continue;
        }
        // First non-flag operand: stop flag parsing here.
        break;
    }

    // Interactive REPL form: `sh -i` (no -c, no script path).
    if interactive && have_c == false
        && idx >= argv.len()  // no script path follows the flags
    {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("sarun-engine brush-sh: runtime: {e}");
                return 127;
            }
        };
        let bash_mode = base == "bash";
        return rt.block_on(run_brush_interactive(
            base.clone(), set_flags, set_o, unset_o, bash_mode));
    }

    // Discriminate `-c SCRIPT` vs. script-file forms.
    let (script_src, dollar0, positional): (String, String, Vec<String>);
    if have_c {
        // `sh [-flags] -c SCRIPT [name [args...]]`
        // glibc's popen (and POSIX `sh -c -- cmd`) inserts a `--` option
        // terminator between `-c` and the script so a command beginning with
        // `-` can't be mistaken for a flag. The flag loop above breaks out the
        // instant it sees `-c`, so that `--` lands here unconsumed — skip a
        // single leading one, else SCRIPT becomes the literal `--` and brush
        // runs it as a command ("command not found: --").
        if argv.get(idx).map(String::as_str) == Some("--") {
            idx += 1;
        }
        let Some(s) = argv.get(idx).cloned() else {
            eprintln!("sarun-engine brush-sh: -c requires a SCRIPT argument");
            return 2;
        };
        idx += 1;
        let name = argv.get(idx).cloned().unwrap_or(base.clone());
        let args = if idx < argv.len() { argv[idx + 1..].to_vec() } else { vec![] };
        script_src = s;
        dollar0 = name;
        positional = args;
    } else if let Some(path) = argv.get(idx).cloned() {
        // `sh [-flags] SCRIPT [args...]` — read SCRIPT from disk.
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sarun-engine brush-sh: cannot read script {path}: {e}");
                return 127;
            }
        };
        script_src = s;
        dollar0 = path.clone();
        positional = argv[idx + 1..].to_vec();
    } else {
        // No -c and no script-file. We refuse to enter an interactive REPL
        // inside a box (out of scope here).
        eprintln!("sarun-engine brush-sh: requires -c SCRIPT or a script path \
                   (interactive nested shell is out of scope inside a brush box)");
        eprintln!("hint: for an interactive shell in a fresh box, start it from \
                   the HOST instead: `sarun run -b --` (in the UI: Pty+ → \
                   \"Shell in a new box\")");
        return 2;
    }

    // Run through brush-core; errors are visible. Per-pipeline provenance is
    // emitted BEFORE each pipeline runs (matching run_brush's contract).
    //
    // MULTI-thread runtime, same as run_brush: a recipe/script mixes
    // concurrent constructs (process substitutions, background jobs, in-
    // process builtins) whose blocking IO parks a current-thread runtime's
    // only worker — a procsub-heavy script wedged reliably as a NESTED
    // shell while the identical script passed at top level, and that
    // difference was exactly this runtime. Worker stacks stay big for the
    // same reason as run_brush (kati recursion on huge Makefiles).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(64 * 1024 * 1024)
        .enable_all().build();
    let rt = match rt {
        Ok(rt) => rt,
        Err(e) => { eprintln!("sarun-engine brush-sh: runtime: {e}"); return 127; }
    };
    // Invoked as `bash` → BASH mode; `sh`/`dash` → POSIX sh_mode (B).
    let bash_mode = base == "bash";
    rt.block_on(run_brush_script(script_src, dollar0, positional,
                                  set_flags, set_o, unset_o, bash_mode))
}

/// Send one `brush_prov_nested` message per pipeline (with `nested:true`);
/// called per-pipeline by run_brush_script so the engine sees provenance in
/// execution order even for multi-command recipes.
fn send_nested_pipeline_records(records: Vec<Value>) {
    if records.is_empty() { return; }
    let msg = json!({"type": "brush_prov_nested", "records": records});
    crate::runner::send_nested_prov(format!("{msg}\n").as_bytes());
}

/// Send one `brush_prov_done` after a complete-command finishes: the engine
/// stamps done_ts + exit_code on the pipelines with these uids, so a reader can
/// show per-pipeline wall time and tell running from finished.
fn send_pipeline_done(uids: &[u64], code: i32, done_ts: f64) {
    if uids.is_empty() { return; }
    let msg = json!({"type": "brush_prov_done", "uids": uids, "code": code, "done_ts": done_ts});
    crate::runner::send_nested_prov(format!("{msg}\n").as_bytes());
}

/// After a $(shell) recipe finishes, fix the attribution on output rows the
/// FUSE handler captured with the wrong (racy) brush_pipeline_id. The stderr
/// flowed through fd 2 → FUSE normally for live backread; this message tells
/// the engine to UPDATE those rows' brush_pipeline_id to the correct value.
fn send_recipe_fixup(uids: &[u64], start_ts: f64) {
    if uids.is_empty() { return; }
    let msg = json!({
        "type": "recipe_fixup",
        "uids": uids,
        "start_ts": start_ts
    });
    crate::runner::send_nested_prov(format!("{msg}\n").as_bytes());
}

/// Report a build edge's run-state transition to the engine, so the targets
/// pane can show only the targets CURRENTLY building (started but not ended)
/// and each target's wall time. The engine matches the box's `build_edges` row
/// by `out` (kati: the node's primary output == outs[0]) or `cmd` (n2: the
/// exact recipe cmdline == the stored cmd). `phase` is "start" or "done";
/// `code` is only meaningful for "done". Best-effort (same broker path as the
/// per-pipeline provenance); a failure leaves the recipe running unchanged.
pub fn send_build_edge_state(out: Option<&str>, cmd: Option<&str>, phase: &str, code: i32,
                             excerpt: Option<&str>) {
    let mut m = json!({"type": "build_edge_state", "state": phase, "ts": now_secs()});
    if let Some(o) = out { m["out"] = json!(o); }
    if let Some(c) = cmd { m["cmd"] = json!(c); }
    if phase == "done" { m["code"] = json!(code); }
    if let Some(x) = excerpt { m["excerpt"] = json!(x); }
    crate::runner::send_nested_prov(format!("{m}\n").as_bytes());
}

/// Build the brush shell, apply set-flags, parse, and execute the script.
/// Mirrors run_brush: same parse/execute discipline and visible-failure rule.
async fn run_brush_script(script: String, shell_name: String,
                          positional: Vec<String>,
                          set_flags: Vec<String>, set_o: Vec<String>,
                          unset_o: Vec<String>, bash_mode: bool) -> i32 {
    // Shim inherits cwd from execve; pass it explicitly in case the builder
    // default ever changes.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    // sh_mode=!bash_mode: sh/dash → POSIX; bash → bashisms (B).
    let shell_res = build_box_shell(!bash_mode, Some(shell_name.clone()),
                                    Some(positional.clone()), Some(cwd)).await;
    let mut shell = match shell_res {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: brush init failed: {e}");
            return 127;
        }
    };
    // Apply flags via explicit `set`; failures are visible (never silently dropped).
    if !set_flags.is_empty() || !set_o.is_empty() || !unset_o.is_empty() {
        let mut set_cmd = String::from("set");
        for f in &set_flags { set_cmd.push(' '); set_cmd.push_str(f); }
        for n in &set_o    { set_cmd.push_str(" -o "); set_cmd.push_str(n); }
        for n in &unset_o  { set_cmd.push_str(" +o "); set_cmd.push_str(n); }
        let src = brush_core::SourceInfo {
            source: "<brush-sh flags>".into(),
            start: None,
        };
        let params0 = shell.default_exec_params();
        if let Err(e) = shell.run_string(set_cmd.clone(), &src, &params0).await {
            eprintln!("sarun-engine brush-sh: applying flags ({set_cmd}) failed: {e}");
            return 2;
        }
    }

    let prog = match shell.parse_string(script.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: cannot parse this script \
                       (NO /bin/sh fallback): {e}");
            return 2;
        }
    };
    let params = shell.default_exec_params();
    // A base call frame so $LINENO (frame-tracked execution position) works
    // at script top level — brush pushes one in its own source_file path,
    // but we parse/run the script ourselves.
    shell.call_stack_mut().push_command_string();
    let (code, _uids) = run_nested_pipelines(&mut shell, prog, &params).await;
    shell.call_stack_mut().pop();
    // bash fires the EXIT trap when the (non-interactive) shell finishes —
    // including after an explicit `exit`. brush-core has the hook; the
    // engine has to call it.
    let _ = shell.on_exit().await;
    code
}

/// Run `prog`'s complete-commands one at a time on `shell`, emitting one nested
/// provenance record-set per pipeline BEFORE running it (execution order, the
/// same contract as run_brush). Shared by the forked brush-sh shim (fresh
/// shell, default params) and the in-process snoop (cloned shell, the caller's
/// params — so its logical fds/redirects carry through). On a brush execution
/// error there is NO /bin/sh fallback: the error is visible and the run stops.
// ── pipeline provenance nesting (parent links for the UI tree) ───────────────
// Every in-process pipeline brush logs gets a process-global unique `uid`, and
// records the `parent_uid` of the pipeline that ENCLOSED it (0 = a root). The
// "current pipeline" is a per-thread value pushed around each pipeline's run, so
// a nested shell (snooped `sh -c`), a recipe's sub-pipelines, and `-exec`/xargs
// children chain under their parent — letting the UI render the otherwise-flat
// pipeline log as a tree (make → recipe → sh -c → …). Crossing a worker-thread
// boundary (recipes/$(shell) run on a spawned brush thread) is handled by
// capturing the parent uid before the spawn and re-establishing it on the new
// thread via set_current_pipeline_uid.
static NEXT_PIPELINE_UID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
thread_local! {
    static CURRENT_PIPELINE_UID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A fresh process-global pipeline uid.
fn next_pipeline_uid() -> u64 {
    NEXT_PIPELINE_UID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The pipeline uid currently executing on THIS thread (0 = none/root).
pub(crate) fn current_pipeline_uid() -> u64 {
    CURRENT_PIPELINE_UID.with(std::cell::Cell::get)
}

/// Set this thread's current-pipeline uid, returning the previous value. Callers
/// save/restore around a pipeline's run so nesting chains correctly; the cross-
/// thread recipe/$(shell) path uses it to seed the worker thread's parent.
pub fn set_current_pipeline_uid(uid: u64) -> u64 {
    CURRENT_PIPELINE_UID.with(|c| c.replace(uid))
}

/// Stamp uid/parent_uid/seq/spawn_ts (+ `nested` when set) onto one complete-
/// command's pipeline records; returns (records, frame_uid). The frame_uid is
/// the first pipeline's uid — used as the parent for anything this complete-
/// command spawns in-process, so e.g. a recipe's `sh -c` nests under the recipe.
fn stamp_pipeline_records(
    complete: &brush_parser::ast::CompoundList,
    seq: &mut i64,
    spawn_ts: f64,
    nested: bool,
) -> (Vec<Value>, u64, Vec<u64>) {
    let parent_uid = current_pipeline_uid();
    let mut recs = vec![];
    let mut uids = vec![];
    let mut frame_uid = 0u64;
    for mut rec in complete_command_records(complete) {
        let uid = next_pipeline_uid();
        if frame_uid == 0 {
            frame_uid = uid;
        }
        uids.push(uid);
        if let Value::Object(ref mut m) = rec {
            m.insert("uid".to_string(), json!(uid));
            m.insert("parent_uid".to_string(), json!(parent_uid));
            m.insert("seq".to_string(), json!(*seq));
            m.insert("spawn_ts".to_string(), json!(spawn_ts));
            if nested {
                m.insert("nested".to_string(), json!(true));
            }
            if let Some(edge) = current_recipe_edge() {
                m.insert("edge_out".to_string(), json!(edge));
            }
        }
        recs.push(rec);
        *seq += 1;
    }
    (recs, frame_uid, uids)
}

/// Wall-clock seconds since the epoch (for pipeline spawn/done timestamps).
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// First pipeline command text of a complete-command, truncated — the
/// activity-feed label for shell work (the counterpart of kati's
/// "recipe of 'x'" entries; a pure-shell hang was invisible before).
fn activity_desc(complete: &brush_parser::ast::CompoundList) -> String {
    let mut s = format!("{complete}");
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 120 {
        let mut cut = 120;
        while !s.is_char_boundary(cut) { cut -= 1; }
        s.truncate(cut);
        s.push('…');
    }
    format!("sh: {s}")
}

async fn run_nested_pipelines(
    shell: &mut brush_core::Shell,
    prog: brush_parser::ast::Program,
    params: &brush_core::ExecutionParameters,
) -> (i32, Vec<u64>) {
    let mut last_code = 0i32;
    let mut seq = 0i64;
    let mut all_uids: Vec<u64> = Vec::new();
    for complete in prog.complete_commands {
        let spawn_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let (recs, frame_uid, uids) = stamp_pipeline_records(&complete, &mut seq, spawn_ts, true);
        all_uids.extend_from_slice(&uids);
        send_nested_pipeline_records(recs);
        let _act = kati::fileutil::ActivityGuard::new(activity_desc(&complete));
        let one = brush_parser::ast::Program { complete_commands: vec![complete] };
        let prev_uid = set_current_pipeline_uid(frame_uid);
        let r = shell.run_program(one, params).await;
        set_current_pipeline_uid(prev_uid);
        match r {
            Ok(result) => {
                last_code = u8::from(result.exit_code) as i32;
                send_pipeline_done(&uids, last_code, now_secs());
                if !result.is_normal_flow() {
                    break;
                }
            }
            Err(e) => {
                eprintln!("sarun-engine brush-sh: execution error \
                           (NO /bin/sh fallback): {e}");
                return (1, all_uids);
            }
        }
    }
    (last_code, all_uids)
}

// ── in-process script snooping (sarun exec interposer) ───────────────────────
// When a box brush shell is about to fork an external command that resolves to
// our OWN bind-shadowed binary (/bin/sh, /usr/bin/sh, … served as the engine;
// see runner shadowing + overlay.rs), forking would just re-exec sarun and
// re-enter brush. Instead we interpret the script IN-PROCESS on a clone of the
// calling shell, preserving its logical fds (carried in `params`) and emitting
// the same nested provenance the fork path would (run_nested_pipelines). This is
// installed on EVERY box shell by build_box_shell_full; brush-core consults it
// at SimpleCommand::execute's external-exec point. find/xargs `-exec` are
// covered too — they dispatch through run_argv → execute.
//
// SELF-DETECTION is box-side and needs no host config: the shadow serves the
// engine binary's CONTENT and SIZE but keeps the FUSE path's own inode, so an
// inode comparison is useless. We compare a candidate's size + leading bytes
// against our own /proc/self/exe. The binary is musl-static + stripped (no
// build-id), so size+content is the robust signal available — distinct programs
// differ in size (cheap reject) and a real shell never collides.
//
// MODE BY NAME: `bash` → extended dialect, `sh`/`dash` → POSIX. The cloned
// subshell is built in the box's sh_mode; for a `bash` invocation the snoop
// flips the single `sh_mode` option (the only thing build_box_shell varies by
// mode), which parser_options()/is_keyword() re-read at parse time.
//
// CONSERVATIVE: anything we can't confidently parse (interactive `-i` shells,
// `-l`/`--login`, any `--long` option, an unknown short flag, an unreadable
// script) returns None and falls through to the normal fork path — so snooping
// can only ever skip a redundant fork, never change behavior. Those fork cases
// re-enter brush_sh, which has the full flag parser and a real interactive REPL.
// Launch flags we DO model: `-e/-u/-x/-v/-f/-n/-h/-m/-b/-C/-a` (and their `+`
// forms) and `-o NAME`/`+o NAME`, all replayed via `set` on the subshell.

const SELF_HEAD: usize = 4096;

/// `(size, leading bytes)` of our own executable, read once from /proc/self/exe.
/// `None` (→ snooping disabled, we fork) if it can't be read.
fn self_exe_image() -> Option<&'static (u64, Vec<u8>)> {
    use std::io::Read;
    use std::sync::OnceLock;
    static IMG: OnceLock<Option<(u64, Vec<u8>)>> = OnceLock::new();
    IMG.get_or_init(|| {
        let md = std::fs::metadata("/proc/self/exe").ok()?;
        let size = md.len();
        let mut f = std::fs::File::open("/proc/self/exe").ok()?;
        let want = std::cmp::min(size as usize, SELF_HEAD);
        let mut head = vec![0u8; want];
        f.read_exact(&mut head).ok()?;
        Some((size, head))
    })
    .as_ref()
}

/// True if `path` IS our running binary — same size AND same leading bytes.
/// The size check is a cheap stat (fast reject for the gcc/cc1/sed storm); the
/// head read happens only when the size already matches (i.e. it's plausibly
/// the shadowed engine binary).
fn is_self_exe(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Some((size, head)) = self_exe_image() else {
        return false;
    };
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    if md.len() != *size {
        return false;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; head.len()];
    if f.read_exact(&mut buf).is_err() {
        return false;
    }
    buf == *head
}

/// The interpreter path from a `#!` shebang line (first whitespace-delimited
/// token), or None if `path` has no shebang / can't be read.
fn shebang_interp(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 256];
    let n = f.read(&mut head).ok()?;
    let rest = head[..n].strip_prefix(b"#!")?;
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    let token: Vec<u8> = rest[..end]
        .iter()
        .copied()
        .skip_while(|&b| b == b' ' || b == b'\t')
        .take_while(|&b| b != b' ' && b != b'\t')
        .collect();
    if token.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&token)))
}

/// A snoopable shell invocation extracted from a self-exe shell argv.
struct Snoop {
    script: String,
    dollar0: String,
    positional: Vec<String>,
    /// Simple set-flags (`-e`/`-u`/`-x`/…) to apply via `set` before running.
    set_flags: Vec<String>,
    /// Invoked as `bash` (→ extended dialect) vs `sh`/`dash` (→ POSIX). The only
    /// option `build_box_shell` varies by mode is `sh_mode`, so the snoop just
    /// flips that one field on the cloned subshell — see run().
    bash_mode: bool,
}

/// Single-letter set-flag characters we replay via `set` (both `-X` to enable
/// and `+X` to disable). `-o NAME`/`+o NAME` are handled separately. Flags
/// outside all of these (e.g. `-i`, `-l`, `--long`) make us decline → fork.
const SNOOP_SET_FLAGS: &str = "euxvfnhmbCa";

/// Decide whether `resolved` + `argv` (argv[0] = the shell name as invoked) is
/// an in-process-snoopable shell invocation. Returns None (→ fork) for odd flags
/// or anything we can't parse confidently. Mirrors brush_sh's `-c`/script/`--`
/// handling (including the popen `sh -c -- CMD` terminator) and its mode-by-name
/// rule (`bash` → extended; `sh`/`dash` → POSIX).
fn parse_snoop(resolved: &std::path::Path, argv: &[String]) -> Option<Snoop> {
    let arg0 = argv.first()?;
    let direct = is_self_exe(resolved);
    if direct {
        let base = std::path::Path::new(arg0)
            .file_name().and_then(|s| s.to_str()).unwrap_or("");
        let bash_mode = base == "bash";
        let mut i = 1usize;
        let mut set_flags = vec![];
        let mut have_c = false;
        while i < argv.len() {
            let a = &argv[i];
            if a == "--" { i += 1; break; }
            if a == "-" { break; } // stdin marker: ends flags, operand follows
            if a == "-c" { have_c = true; i += 1; break; }
            // `-o NAME` / `+o NAME`: a named option (pipefail, errexit, …). Take
            // the name and replay it verbatim via `set` (the two tokens become
            // `set … -o pipefail`). Missing name → decline (fork; brush_sh errors).
            if a == "-o" || a == "+o" {
                let name = argv.get(i + 1)?;
                set_flags.push(a.clone());
                set_flags.push(name.clone());
                i += 2;
                continue;
            }
            if let Some(rest) = a.strip_prefix('-') {
                if rest.is_empty()
                    || !rest.chars().all(|c| c == 'c' || SNOOP_SET_FLAGS.contains(c))
                {
                    return None; // an option we don't model precisely → fork
                }
                for c in rest.chars() {
                    if c == 'c' { have_c = true; } else { set_flags.push(format!("-{c}")); }
                }
                i += 1;
                if have_c { break; }
                continue;
            }
            // `+`-prefixed bundle (`+e`, `+x`, …): turn options OFF via `set +X`.
            if let Some(rest) = a.strip_prefix('+') {
                if rest.is_empty() || !rest.chars().all(|c| SNOOP_SET_FLAGS.contains(c)) {
                    return None;
                }
                for c in rest.chars() { set_flags.push(format!("+{c}")); }
                i += 1;
                continue;
            }
            break; // first non-flag operand
        }
        if have_c {
            // popen / `sh -c -- CMD` insert a `--` terminator before SCRIPT.
            if argv.get(i).map(String::as_str) == Some("--") { i += 1; }
            let script = argv.get(i)?.clone();
            i += 1;
            let dollar0 = argv.get(i).cloned().unwrap_or_else(|| arg0.clone());
            let positional = if i < argv.len() { argv[i + 1..].to_vec() } else { vec![] };
            return Some(Snoop { script, dollar0, positional, set_flags, bash_mode });
        }
        // `sh [-flags] SCRIPT [args]` — read SCRIPT from disk.
        let path = argv.get(i)?;
        let script = std::fs::read_to_string(path).ok()?;
        return Some(Snoop {
            script,
            dollar0: path.clone(),
            positional: argv.get(i + 1..).unwrap_or(&[]).to_vec(),
            set_flags,
            bash_mode,
        });
    }
    // Shebang form: `./configure` etc. whose `#!` interpreter is our binary.
    let interp = shebang_interp(resolved)?;
    if !is_self_exe(&interp) {
        return None;
    }
    let ibase = interp.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let script = std::fs::read_to_string(resolved).ok()?;
    Some(Snoop {
        script,
        dollar0: resolved.to_string_lossy().into_owned(),
        positional: argv.get(1..).unwrap_or(&[]).to_vec(),
        set_flags: vec![],
        bash_mode: ibase == "bash",
    })
}

/// sarun's exec interposer: interpret self-shadowed `/bin/sh` scripts in-process
/// (see the section comment above). A ZST — registered on every box shell.
pub(crate) struct SnoopInterposer;

impl brush_core::commands::ExecInterposer<brush_core::extensions::DefaultShellExtensions>
    for SnoopInterposer
{
    fn wants(&self, resolved: &std::path::Path) -> bool {
        if is_self_exe(resolved) {
            return true;
        }
        // A `#!`-script whose interpreter is our binary (e.g. ./configure).
        shebang_interp(resolved).is_some_and(|i| is_self_exe(&i))
    }

    fn run<'a>(
        &'a self,
        mut sub: brush_core::Shell,
        resolved: std::path::PathBuf,
        argv: Vec<String>,
        mut params: brush_core::ExecutionParameters,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<brush_core::ExecutionResult>> + Send + 'a>,
    > {
        Box::pin(async move {
            use std::sync::atomic::{AtomicU32, Ordering};
            let snoop = parse_snoop(&resolved, &argv)?; // None → decline (fork)

            // A snooped script is a fresh execution context, like a forked shell:
            // its OWN `set -e` governs its commands. The caller's params carry
            // `suppress_errexit=true` when this `sh -c …` sits in a conditional
            // (LHS of `&&`/`||`, an `if`, a `!`-pipeline) — that suppresses the
            // PARENT shell's errexit and must NOT leak in and disable the child's.
            // The fds carried in params are kept; only this flag is reset.
            params.suppress_errexit = false;

            // Same principle for the `set` flags themselves: the cloned subshell
            // carries the CALLER's option state, but a real `sh script.sh` is a
            // fresh process with default options — a recipe prefix's `set -e`
            // must not govern the script's commands (kbuild's cmd macro runs
            // every recipe under `set -e`, and headers_install.sh relies on
            // inspecting unifdef's exit 1 rather than dying on it). Reset the
            // per-invocation `set` flags to sh defaults; the script's own argv
            // flags (snoop.set_flags) are replayed below.
            // A real `sh script.sh` is a FRESH PROCESS: it inherits NO trap
            // handlers. The clone carries the caller's traps — firing e.g.
            // configure's EXIT trap (`rm -f -r conftest* …`) at every
            // snooped child's exit deleted files the caller had just
            // generated.
            sub.traps_mut().clear_all_handlers();
            {
                let o = sub.options_mut();
                o.exit_on_nonzero_command_exit = false;
                o.treat_unset_variables_as_error = false;
                o.print_commands_and_arguments = false;
                o.print_shell_input_lines = false;
                o.disable_filename_globbing = false;
                o.do_not_execute_commands = false;
            }

            // A unique synthetic $$/BASHPID per snooped script, so concurrent
            // in-process scripts get distinct `conftest$$`-style temp files even
            // though they share one OS process.
            static SNOOP_PID: AtomicU32 = AtomicU32::new(0x4000_0000);
            let synth = SNOOP_PID.fetch_add(1, Ordering::Relaxed);
            sub.set_snoop_identity(snoop.dollar0, snoop.positional, synth);

            // Mode by invocation name. The cloned subshell was built in the box's
            // POSIX sh_mode; `bash …` wants the extended dialect (`[[ ]]`, arrays,
            // bash keywords). `build_box_shell` varies ONLY `sh_mode` between its
            // sh and bash shells (it never sets `.posix(...)`, and nothing else is
            // derived from the mode), and `parser_options()`/`is_keyword()` re-read
            // `sh_mode` at parse time — so flipping this one field is exactly
            // equivalent to having built a bash-mode shell. `set` flags and the
            // script below are then parsed in the right dialect.
            if snoop.bash_mode {
                sub.options_mut().sh_mode = false;
            }

            // Replay simple set-flags (-e/-u/-x…) via `set`, like run_brush_script.
            if !snoop.set_flags.is_empty() {
                let mut cmd = String::from("set");
                for f in &snoop.set_flags {
                    cmd.push(' ');
                    cmd.push_str(f);
                }
                let src = brush_core::SourceInfo { source: "<snoop flags>".into(), start: None };
                if sub.run_string(cmd, &src, &params).await.is_err() {
                    return Some(brush_core::ExecutionResult::new(2));
                }
            }

            let prog = match sub.parse_string(snoop.script) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "sarun-engine brush-sh (snooped): cannot parse this script \
                         (NO /bin/sh fallback): {e}"
                    );
                    return Some(brush_core::ExecutionResult::new(2));
                }
            };
            sub.call_stack_mut().push_command_string();
            let (code, _uids) = run_nested_pipelines(&mut sub, prog, &params).await;
            sub.call_stack_mut().pop();
            // The nested shell terminates here — fire its EXIT trap, as
            // bash does for a script run via `sh file.sh`. With the CALLER's
            // params: a redirected `bash s.sh > f` must trap into f.
            let _ = sub.on_exit_with_params(&params).await;
            Some(brush_core::ExecutionResult::new(code as u8))
        })
    }
}

/// Interactive REPL for `sh -i`/`bash -i` inside a `-b` box: reedline backend
/// with history, completion, and highlighting. Same brush-core shell as `-c`.
/// Set-flags are applied before the loop starts.
async fn run_brush_interactive(shell_name: String,
                               set_flags: Vec<String>, set_o: Vec<String>,
                               unset_o: Vec<String>, bash_mode: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    // Must be built as interactive from the start — builder wires up
    // Option<History> and job-control in build(); setting it afterwards
    // leaves history=None, causing reedline DefaultHinter to panic.
    let shell_res = build_box_shell_opt(!bash_mode, Some(shell_name.clone()),
                                        Some(vec![]), Some(cwd),
                                        /*interactive=*/ true).await;
    let mut shell = match shell_res {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: brush init failed: {e}");
            return 127;
        }
    };
    // Apply set-flags as in run_brush_script.
    if !set_flags.is_empty() || !set_o.is_empty() || !unset_o.is_empty() {
        let mut set_cmd = String::from("set");
        for f in &set_flags { set_cmd.push(' '); set_cmd.push_str(f); }
        for n in &set_o    { set_cmd.push_str(" -o "); set_cmd.push_str(n); }
        for n in &unset_o  { set_cmd.push_str(" +o "); set_cmd.push_str(n); }
        let src = brush_core::SourceInfo {
            source: "<brush-sh flags>".into(), start: None,
        };
        let params0 = shell.default_exec_params();
        if let Err(e) = shell.run_string(set_cmd.clone(), &src, &params0).await {
            eprintln!("sarun-engine brush-sh: applying flags ({set_cmd}) failed: {e}");
            return 2;
        }
    }
    // brush-interactive requires Arc<tokio::Mutex<>> (ShellRef) so reedline
    // helpers (completer/validator/highlighter) can clone and query the shell.
    let shell_ref: brush_interactive::ShellRef = std::sync::Arc::new(
        tokio::sync::Mutex::new(shell));
    let ui_opts = brush_interactive::UIOptions::builder().build();
    let mut backend = match brush_interactive::ReedlineInputBackend::new(
        &ui_opts, &shell_ref) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: reedline init failed: {e}");
            return 127;
        }
    };
    let opts: brush_interactive::InteractiveOptions = (&ui_opts).into();
    let mut iash = match brush_interactive::InteractiveShell::new(
        &shell_ref, &mut backend, &opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: interactive shell init failed: {e}");
            return 127;
        }
    };
    if let Err(e) = iash.run_interactively().await {
        eprintln!("sarun-engine brush-sh: interactive: {e}");
        return 1;
    }
    // Last exit code is on the shell after the loop.
    let s = shell_ref.lock().await;
    i32::from(u8::from(s.last_exit_status()))
}

/// Build the brush shell, parse, emit FRAME_PROV per pipeline (in execution
/// order), then execute. No /bin/sh fallback: parse errors → exit 2; exec
/// errors → visible message + non-zero.
async fn run_brush(conn_fd: i32, script: String, sh_mode: bool) -> i32 {
    // Mode follows the user's argv[0] (shell_name_from_argv): bash → BASH
    // mode, anything else → POSIX. Without the builtin table brush-core
    // ships empty, so even POSIX builtins fail.
    let shell_res = build_box_shell(sh_mode, None, None, None).await;
    let mut shell = match shell_res {
        Ok(s) => s,
        Err(e) => { eprintln!("sarun-engine inner: -b brush init failed: {e}"); return 127; }
    };

    // Parse first: enables provenance emission and surfaces parse errors visibly.
    let prog = match shell.parse_string(script.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sarun-engine inner: -b brush cannot parse this command \
                       (NO /bin/sh fallback): {e}");
            return 2;
        }
    };
    // Execute one complete-command at a time on the SAME persistent shell so
    // shell state carries across. Emit each pipeline's FRAME_PROV (structure +
    // literal output-redirect targets + seq/spawn_ts) BEFORE running it.
    // Own error handling: no run_string auto-display, no /bin/sh fallback.
    let params = shell.default_exec_params();
    let mut last_code = 0i32;
    let mut seq = 0i64;
    for complete in prog.complete_commands {
        let spawn_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let (recs, frame_uid, uids) = stamp_pipeline_records(&complete, &mut seq, spawn_ts, false);
        for rec in &recs {
            send_prov(conn_fd, rec);
        }
        let one = brush_parser::ast::Program { complete_commands: vec![complete] };
        let prev_uid = set_current_pipeline_uid(frame_uid);
        let r = shell.run_program(one, &params).await;
        set_current_pipeline_uid(prev_uid);
        match r {
            Ok(result) => {
                last_code = u8::from(result.exit_code) as i32;
                send_pipeline_done(&uids, last_code, now_secs());
            }
            Err(e) => {
                eprintln!("sarun-engine inner: -b brush execution error \
                           (NO /bin/sh fallback): {e}");
                return 1;
            }
        }
    }
    last_code
}

// ── n2/ninja in-process recipe executor ──────────────────────────────────────
// `ninja` is shadow-bound to the engine (see runner.rs / crate::n2run). Each
// recipe is routed through this executor instead of n2's posix_spawn(/bin/sh).
// n2's Termination contract: stdin=/dev/null; stdout+stderr merged into one pipe
// fed to output_cb; exit 0 → Success, non-zero → Failure. Recipes run through
// the same embedded brush as every other -b recipe (no /bin/sh fork). File writes
// go through the FUSE overlay as usual.

/// Shared multi-thread tokio runtime for all embedded-n2 recipes. n2's scheduler
/// is sync; we block_on each async brush future against this runtime (OnceLock).
static N2_RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn n2_runtime() -> Option<&'static tokio::runtime::Runtime> {
    N2_RT.get_or_init(|| {
        // 64 MiB worker stacks: recipes run here may be recursive sub-makes whose
        // kati parse recurses deeply; the default 2 MiB tokio stack overflows.
        tokio::runtime::Builder::new_multi_thread()
            .thread_stack_size(64 * 1024 * 1024)
            .enable_all().build()
            .expect("sarun-engine n2: tokio runtime")
    });
    N2_RT.get()
}

/// Strip a `/bin/sh -c '<recipe>'` wrapper (sh/bash/dash by basename) and return
/// the inner recipe; otherwise return the cmdline unchanged. Ninja `command =`
/// lines are often `sh -c '<recipe>'`; unwrapping avoids nesting a shell.
/// Uses a small POSIX word-splitter to recognize the `<shell> -c <script>` shape;
/// unrecognized cmdlines pass through byte-identical.
///
/// Also handles a bash leniency: if the recipe ends with an ODD number of
/// trailing backslashes (e.g. from `$(call func, \)` in GNU make), append one
/// more so the final `\` self-quotes. Even-length runs are already valid pairs.
fn double_trailing_backslash(mut recipe: String) -> String {
    let trailing = recipe.bytes().rev().take_while(|&b| b == b'\\').count();
    if trailing.is_multiple_of(2) {
        return recipe;
    }
    recipe.push('\\');
    recipe
}

fn unwrap_sh_c(cmdline: &str) -> String {
    let words = split_words(cmdline);
    if words.len() >= 3 {
        let base = std::path::Path::new(&words[0])
            .file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(base, "sh" | "bash" | "dash" | "brush") && words[1] == "-c" {
            return words[2].clone();
        }
    }
    cmdline.to_string()
}

/// Minimal POSIX word splitter: unquoted whitespace, `'…'` literal,
/// `"…"` with `\"\\$\`` escapes, bare backslash. Used only to recover
/// the inner script of `sh -c '…'`.
fn split_words(s: &str) -> Vec<String> {
    let mut words = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' if !in_word => {}
            ' ' | '\t' | '\n' => { words.push(std::mem::take(&mut cur)); in_word = false; }
            '\'' => {
                in_word = true;
                for q in chars.by_ref() { if q == '\'' { break; } cur.push(q); }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    if q == '"' { break; }
                    if q == '\\' {
                        if let Some(&n) = chars.peek() {
                            if matches!(n, '"' | '\\' | '$' | '`') { cur.push(chars.next().unwrap()); continue; }
                        }
                    }
                    cur.push(q);
                }
            }
            '\\' => { in_word = true; if let Some(n) = chars.next() { cur.push(n); } }
            _ => { in_word = true; cur.push(c); }
        }
    }
    if in_word { words.push(cur); }
    words
}

thread_local! {
    /// sarun: working directory for in-process recipes. An in-process
    /// `make`/`ninja` builtin sets this to its logical working_dir so recipes
    /// run THERE without mutating the process cwd (which would race other
    /// in-process instances). None → process cwd, i.e. the shadow/top-level
    /// path where the box already chdir'd appropriately. Same thread as the
    /// build that set it (recipes are dispatched synchronously per build).
    static BOX_RECIPE_CWD: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// sarun: set (or clear) the thread-local in-process-recipe working dir,
/// returning the previous value so a nested build can save/restore it.
pub fn set_box_recipe_cwd(cwd: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    BOX_RECIPE_CWD.with(|c| std::mem::replace(&mut *c.borrow_mut(), cwd))
}

thread_local! {
    /// The build edge (primary output name) whose recipe THIS thread is
    /// currently running — set by katirun's edge reporter around each recipe,
    /// re-seeded onto the spawned brush thread like the pipeline uid. Every
    /// pipeline record stamped while it's set carries `edge_out`, giving the
    /// UI an EXACT edge → pipelines → processes/outputs causal chain (the
    /// previous linkage guessed by execution-time windows).
    static BOX_RECIPE_EDGE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_box_recipe_edge(edge: Option<String>) -> Option<String> {
    BOX_RECIPE_EDGE.with(|c| std::mem::replace(&mut *c.borrow_mut(), edge))
}

// Install (once) the brush-core assignment observer: every shell variable
// assignment a box shell applies is queued as a variable-provenance row,
// tagged `sh` plus the recipe's build edge (when inside one) or the current
// pipeline uid — the same makevar table the make hook feeds, so
// make↔shell↔sub-make value flows are one searchable history.
thread_local! {
    /// True while this thread runs engine plumbing (the make export prefix)
    /// whose assignments must NOT be recorded as variable provenance.
    static SUPPRESS_VAR_RECORD: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Record pipeline provenance for complete-commands the engine did NOT
/// drive itself: sourced files (`. file`) and other nested run_program
/// invocations. Depth 1 is the outermost program — run_nested_pipelines
/// already records those; deeper programs were previously INVISIBLE in
/// Pipes (a configure sourcing its .lineno copy ran entirely off-camera).
pub(crate) fn install_pipeline_observer() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        brush_core::interp::install_complete_command_observer(std::sync::Arc::new(
            |complete, depth| {
                if depth <= 1 {
                    return None;
                }
                let spawn_ts = now_secs();
                let mut seq = 0i64;
                let (recs, frame_uid, uids) =
                    stamp_pipeline_records(complete, &mut seq, spawn_ts, true);
                if uids.is_empty() {
                    return None;
                }
                send_nested_pipeline_records(recs);
                let act = kati::fileutil::ActivityGuard::new(activity_desc(complete));
                let prev_uid = set_current_pipeline_uid(frame_uid);
                Some(Box::new(move |code: i32| {
                    set_current_pipeline_uid(prev_uid);
                    send_pipeline_done(&uids, code, now_secs());
                    drop(act);
                }))
            },
        ));
    });
}

pub(crate) fn install_shell_var_recorder() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        brush_core::interp::install_assign_observer(std::sync::Arc::new(
            |name, value, exported, rhs| {
                if !crate::katirun::vartrace_enabled()
                    || SUPPRESS_VAR_RECORD.with(|c| c.get())
                {
                    return;
                }
                let uid = current_pipeline_uid();
                let edge = current_recipe_edge();
                // Only record assignments inside a recorded execution context
                // (a pipeline or a recipe) — shell-internal churn outside one
                // is engine plumbing, not build provenance.
                if uid == 0 && edge.is_none() {
                    return;
                }
                let loc = match &edge {
                    Some(e) => format!("recipe of {e}"),
                    None => format!("pipeline #{uid}"),
                };
                // ShellValueLiteral's Display shell-quotes scalars with
                // spaces; strip a matching outer quote pair for readability.
                let mut v = value.to_string();
                if v.len() >= 2
                    && ((v.starts_with('\'') && v.ends_with('\''))
                        || (v.starts_with('"') && v.ends_with('"')))
                {
                    v = v[1..v.len() - 1].to_string();
                }
                let v = crate::katirun::cap_text(v, 4096);
                let rhs_s = crate::katirun::cap_text(rhs.to_string(), 1024);
                let refs = crate::katirun::extract_var_refs(&rhs_s);
                crate::katirun::push_makevar(serde_json::json!({
                    "name": name,
                    "loc": loc,
                    "value": v,
                    "make": if exported { "sh export" } else { "sh" },
                    "rhs": rhs_s,
                    "refs": refs,
                    "edge": edge,
                    "uid": uid,
                    "flags": if exported { "sh x" } else { "sh" },
                }));
            },
        ));
    });
}

pub(crate) fn current_recipe_edge() -> Option<String> {
    BOX_RECIPE_EDGE.with(|c| c.borrow().clone())
}

/// Run one n2 recipe through embedded brush, merging stdout+stderr into
/// `output_cb` (n2's contract). Returns the recipe's exit code.
/// How a run-through-brush command's stderr (fd 2) is handled.
pub enum RecipeStderr {
    /// Merge fd 2 into fd 1 → the captured output (recipe / n2 contract).
    Merge,
    /// Leave fd 2 on the shell's default (the box's real fd 2 = FUSE stderr
    /// sink / terminal); only fd 1 is captured. Matches `$(shell …)`.
    Inherit,
    /// Discard fd 2 (`2>/dev/null`).
    Null,
}

pub fn run_recipe_in_process(cmdline: &str, output_cb: &mut dyn FnMut(&[u8])) -> i32 {
    run_recipe_in_process_opt(cmdline, output_cb, RecipeStderr::Merge)
}

/// Like [`run_recipe_in_process`] but selects how the recipe's fd 2 is handled.
/// The full box coreutil set runs in-process for every recipe (make and n2
/// alike): `box_builtins` registers them unconditionally — see its note on the
/// thread-local localization that makes this safe.
pub fn run_recipe_in_process_opt(
    cmdline: &str,
    output_cb: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
) -> i32 {
    run_recipe_in_process_prefixed("", cmdline, output_cb, stderr)
}

/// Like run_recipe_in_process_opt, with a make export PREFIX that runs in the
/// SAME shell before the recipe but is NOT recorded as pipeline provenance —
/// it's sarun plumbing (the make's `export NAME='…'` lines), and recording it
/// buried every recipe's real pipelines under hundreds of export rows.
pub fn run_recipe_in_process_prefixed(
    prefix: &str,
    cmdline: &str,
    output_cb: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
) -> i32 {
    let prefix_owned = prefix.to_string();
    let recipe = double_trailing_backslash(unwrap_sh_c(cmdline));
    let Some(rt) = n2_runtime() else {
        output_cb(b"sarun-engine n2: no tokio runtime\n");
        return 127;
    };
    // Merged stdout+stderr pipe (n2's posix contract: one pipe, both fds on it).
    let (mut reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => { output_cb(format!("sarun-engine n2: pipe: {e}\n").as_bytes()); return 127; }
    };
    let recipe_start_ts = now_secs();
    let is_inherit = matches!(stderr, RecipeStderr::Inherit);

    let cwd = BOX_RECIPE_CWD
        .with(|c| c.borrow().clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let recipe_owned = recipe.clone();
    // Capture the enclosing pipeline uid on THIS thread so the recipe/$(shell)
    // pipelines (which run on the spawned brush thread below) nest under it.
    let parent_uid = current_pipeline_uid();
    // Same for the running edge tag (set by katirun's edge reporter).
    let recipe_edge = current_recipe_edge();
    // Run brush on a worker thread so this (n2 scheduler) thread can drain the
    // pipe concurrently — a finite pipe buffer would otherwise deadlock. Large
    // stack: a recipe can be a recursive in-process make whose kati parse/eval
    // recurses deeply on big Makefiles, overflowing the default 2 MiB stack.
    let exec = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
        rt.block_on(async move {
            // Seed this worker thread's current pipeline from the captured parent.
            set_current_pipeline_uid(parent_uid);
            set_box_recipe_edge(recipe_edge);
            let mut shell = match build_box_shell_full(
                true, None, None, Some(cwd), false,
            ).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("sarun-engine n2: brush init failed: {e}");
                    return (127, vec![]);
                }
            };
            match stderr {
                RecipeStderr::Merge => {
                    let w2 = match writer.try_clone() {
                        Ok(w) => w,
                        Err(e) => { eprintln!("sarun-engine n2: pipe clone: {e}"); return (127, vec![]); }
                    };
                    shell.open_files_mut().set_fd(2, brush_core::openfiles::OpenFile::from(w2));
                }
                RecipeStderr::Inherit => {
                    // Leave fd 2 on the shell default (box's real fd 2 = FUSE
                    // stderr sink). Stderr flows through the existing capture
                    // path for live backread. Attribution is fixed retroactively
                    // after the recipe finishes (send_recipe_fixup).
                }
                RecipeStderr::Null => {
                    if let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
                        shell.open_files_mut()
                            .set_fd(2, brush_core::openfiles::OpenFile::from(devnull));
                    }
                }
            }
            shell.open_files_mut().set_fd(1, brush_core::openfiles::OpenFile::from(writer));
            // Apply the make's export prefix to THIS shell, provenance-free:
            // run_string sets the vars/exports without emitting pipeline
            // records (only run_nested_pipelines below records).
            if !prefix_owned.is_empty() {
                let src = brush_core::SourceInfo {
                    source: "<make exports>".into(), start: None };
                let params = shell.default_exec_params();
                SUPPRESS_VAR_RECORD.with(|c| c.set(true));
                let r = shell.run_string(prefix_owned.clone(), &src, &params).await;
                SUPPRESS_VAR_RECORD.with(|c| c.set(false));
                if r.is_err() {
                    eprintln!("sarun-engine make: export prefix failed to apply");
                }
            }
            let prog = match shell.parse_string(recipe_owned.clone()) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("sarun-engine n2: cannot parse recipe \
                               (NO /bin/sh fallback): {e}");
                    return (2, vec![]);
                }
            };
            let params = shell.default_exec_params();
            // Route through run_nested_pipelines so the recipe's own pipelines are
            // logged (as nested prov rows) and become tree nodes — the recipe's
            // `sh -c …` then nests under it — and so `set -e`/`exit` between the
            // recipe's statements is honored. Each pipeline records parent_uid =
            // the enclosing pipeline (the make/recipe that spawned this one).
            let r = run_nested_pipelines(&mut shell, prog, &params).await;
            // A recipe line is its own shell: fire its EXIT trap (into the
            // recipe's captured output), as bash-per-recipe-line would.
            let _ = shell.on_exit_with_params(&params).await;
            r
            // shell and PipeWriter clones drop here → write end closed → drain sees EOF.
        })
    }).expect("spawn brush recipe thread");

    // Drain the merged pipe into n2's output_cb. n2 writes to the terminal;
    // we previously also teed to the FUSE sink, causing double output — removed.
    //
    // Bash-compat shim: brush emits "error: command not found: NAME"; bash emits
    // "/bin/bash: line N: NAME: command not found". kati_norms strips the prefix
    // only if it matches the bash shape. Rewrite line-by-line on the fly;
    // unterminated tail bytes are buffered until a newline or pipe close.
    let mut buf = [0u8; 4 << 10];
    let mut line_buf: Vec<u8> = Vec::new();
    loop {
        let n = match std::io::Read::read(&mut reader, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            line_buf.push(b);
            if b == b'\n' {
                emit_bash_compat(&line_buf, output_cb);
                line_buf.clear();
            }
        }
    }
    if !line_buf.is_empty() {
        emit_bash_compat(&line_buf, output_cb);
    }
    let (code, uids) = exec.join().unwrap_or((127, vec![]));
    if is_inherit && !uids.is_empty() {
        send_recipe_fixup(&uids, recipe_start_ts);
    }
    code
}

/// Rewrite `error: command not found: NAME` → `/bin/sh: line 1: NAME: command
/// not found` (bash shape, normalized by kati_norms). Other lines pass through.
fn emit_bash_compat(line: &[u8], output_cb: &mut dyn FnMut(&[u8])) {
    const NF: &[u8] = b"error: command not found: ";
    if let Some(rest) = line.strip_prefix(NF) {
        let name = rest.strip_suffix(b"\n").unwrap_or(rest);
        let mut out = Vec::with_capacity(line.len() + 24);
        out.extend_from_slice(b"/bin/sh: line 1: ");
        out.extend_from_slice(name);
        out.extend_from_slice(b": command not found\n");
        output_cb(&out);
        return;
    }
    output_cb(line);
}

/// Executor installed into vendored n2 (process::set_executor).
/// Maps exit 0 → Success, non-zero → Failure. Interrupted is not produced
/// (no signal path for a brush recipe; n2's SIGINT handling is suppressed in
/// embedded mode).
pub fn n2_executor(cmdline: &str, output_cb: &mut dyn FnMut(&[u8])) -> n2::process::Termination {
    // n2 runs each recipe on a fresh worker thread that carries n2's logical
    // build dir (graph::set_cwd). Mirror it into the recipe cwd so the brush
    // shell runs the command from the build dir (this thread's BOX_RECIPE_CWD
    // is otherwise unset). Restore on the way out.
    let prev = set_box_recipe_cwd(n2::graph::get_cwd());
    // Mark this edge running for the targets pane (matched by its exact cmdline,
    // which n2 also stored as the build_edges row's `cmd`). Phony / up-to-date
    // edges never reach the executor, so they're correctly left un-started.
    send_build_edge_state(None, Some(cmdline), "start", 0, None);
    let code = run_recipe_in_process(cmdline, output_cb);
    send_build_edge_state(None, Some(cmdline), "done", code, None);
    set_box_recipe_cwd(prev);
    if code == 0 {
        n2::process::Termination::Success
    } else {
        n2::process::Termination::Failure
    }
}

// ── builtin-boundary unit tests ──────────────────────────────────────────────
// These exercise the coreutil builtins directly through the box brush shell —
// no box, no bwrap, no FUSE, no keystrokes. They pin the two seams the macro
// block above exists to preserve: relative operands resolve against the shell's
// LOGICAL cwd (the process is never `chdir`'d), and an EXPORTED shell var reaches
// the vendored entry (via `exported_env_snapshot`), for one representative of
// each argument shape (stream, stream+env, fs+env, info+env).
#[cfg(test)]
mod builtin_boundary_tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn standalone_cli_runs_relation_parsed_argv_without_an_engine() {
        let arguments = [
            "-c",
            "[ \"$#\" -eq 2 ] && [ \"$1\" = 'argument with spaces' ] && [ -z \"$2\" ]",
            "brush",
            "argument with spaces",
            "",
        ]
        .map(str::to_string);
        assert_eq!(super::cli(&arguments), 0);
    }

    /// A fresh empty scratch dir under the system tempdir (never the process cwd).
    fn scratch_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sarun_brush_boundary_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Build a box shell with logical cwd = `cwd`, run `script` with fd 1 wired to
    /// a capture file, and return everything the shell wrote to stdout.
    async fn run_capture(script: &str, cwd: &Path) -> String {
        run_capture_mode(script, cwd, false).await
    }

    async fn run_capture_mode(script: &str, cwd: &Path, interactive: bool) -> String {
        let mut shell = super::build_box_shell(true, None, None, Some(cwd.to_path_buf()))
            .await
            .expect("build box shell");
        shell.options_mut().interactive = interactive;
        let out_path = cwd.join(".capture_stdout");
        let err_path = cwd.join(".capture_stderr");
        let file = std::fs::File::create(&out_path).expect("create capture file");
        let err = std::fs::File::create(&err_path).expect("create stderr capture file");
        shell
            .open_files_mut()
            .set_fd(1, brush_core::openfiles::OpenFile::from(file));
        shell
            .open_files_mut()
            .set_fd(2, brush_core::openfiles::OpenFile::from(err));
        let src = brush_core::SourceInfo { source: "<boundary-test>".into(), start: None };
        let params = shell.default_exec_params();
        shell
            .run_string(script.to_string(), &src, &params)
            .await
            .expect("run script");
        drop(shell); // close the capture fd before reading it back
        std::fs::read_to_string(&out_path).expect("read capture file")
    }

    #[test]
    fn edit_builtin_is_in_the_single_shared_builtin_catalog() {
        let builtins = super::box_builtins::<brush_core::extensions::DefaultShellExtensions>();
        assert!(builtins.contains_key("edit"));
    }

    #[test]
    fn builtin_command_signatures_are_derived_from_canonical_parser_values() {
        let bind = super::builtin_command_signatures()
            .iter()
            .find(|signature| signature.command == "bind")
            .expect("bind's finite argument definition was not projected");
        let keymap = bind
            .following_arguments
            .iter()
            .find(|argument| argument.flag == "-m")
            .expect("bind -m definition was not projected");
        assert_eq!(
            keymap
                .values
                .iter()
                .map(|value| value.text.as_str())
                .collect::<Vec<_>>(),
            [
                "emacs-standard",
                "emacs-meta",
                "emacs-ctlx",
                "vi-command",
                "vi-insert",
            ]
        );
        assert!(keymap.values.iter().all(|value| {
            !matches!(value.text.as_str(), "emacs" | "vi" | "vi-move")
        }));
    }

    #[tokio::test]
    async fn edit_builtin_refuses_noninteractive_and_pipeline_execution() {
        let dir = scratch_dir();
        let noninteractive = run_capture("edit file.sh; printf '%s' \"$?\"", &dir).await;
        assert_eq!(noninteractive, "1");

        let pipeline = run_capture_mode(
            "printf x | edit file.sh; printf '%s' \"$?\"",
            &dir,
            true,
        )
        .await;
        assert_eq!(pipeline, "1");
    }

    /// STREAM shape (`cat`): a relative operand resolves against the shell's
    /// logical cwd, not the engine process's cwd.
    #[tokio::test]
    async fn stream_relative_operand_uses_logical_cwd() {
        let dir = scratch_dir();
        std::fs::File::create(dir.join("rel.txt"))
            .unwrap()
            .write_all(b"logical-cwd-hit\n")
            .unwrap();
        assert_ne!(std::env::current_dir().unwrap(), dir);
        let out = run_capture("cat rel.txt", &dir).await;
        assert_eq!(out, "logical-cwd-hit\n");
    }

    /// STREAM+ENV shape (`sort`): reads piped logical stdin and sorts it; also
    /// confirms the stream+env entry is wired (LC_ALL from the exported env).
    #[tokio::test]
    async fn stream_env_sort_orders_stdin() {
        let dir = scratch_dir();
        let out = run_capture("export LC_ALL=C; printf 'b\\na\\nc\\n' | sort", &dir).await;
        assert_eq!(out, "a\nb\nc\n");
    }

    /// FS+ENV shape (`cp`): relative source AND dest resolve against the logical
    /// cwd (the copy lands in `dir`, proving no process chdir).
    #[tokio::test]
    async fn fs_env_cp_relative_uses_logical_cwd() {
        let dir = scratch_dir();
        std::fs::File::create(dir.join("src.txt"))
            .unwrap()
            .write_all(b"payload\n")
            .unwrap();
        run_capture("cp src.txt dst.txt", &dir).await;
        let dst = std::fs::read_to_string(dir.join("dst.txt")).expect("dst.txt in logical cwd");
        assert_eq!(dst, "payload\n");
    }

    /// INFO+ENV shape (`nproc`): an EXPORTED var reaches the vendored entry —
    /// `OMP_NUM_THREADS=1` clamps the reported count to 1.
    #[tokio::test]
    async fn info_env_nproc_reads_exported_var() {
        let dir = scratch_dir();
        let out = run_capture("export OMP_NUM_THREADS=1; nproc", &dir).await;
        assert_eq!(out.trim(), "1");
    }
}
