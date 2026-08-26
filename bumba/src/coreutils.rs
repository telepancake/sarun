use std::ffi::OsString;

/// Run a coreutil inline with scoped logical process state.
///
/// uucore caches immutable Fluent bundles by utility and locale; the guards
/// select the right bundle, utility name, exported environment, and umask for
/// this call, then restore any surrounding invocation. Localization therefore
/// has no effect on scheduling and consumes no per-call OS thread.
fn run_coreutil_scoped(
    util: &'static str,
    umask: u32,
    env: Vec<(OsString, OsString)>,
    body: impl FnOnce() -> i32,
) -> i32 {
    // Install the shell's LOGICAL environment FIRST: localization and all
    // runtime env reads must see the command's exported variables, not the
    // host process environment.
    let _logical = uucore::logical_env::push(env, umask);
    let _runtime = brush_coreutils_builtins::init_localization(util);
    body()
}

/// Snapshot the shell's LOGICAL environment — its exported variables — as
/// `(name, value)` `OsString` pairs, the shape the coreutil builtins take for
/// their `POSIXLY_CORRECT`-class knob reads. Only exported vars are included
/// (a child process/`printenv` would see exactly these), so a builtin sees the
/// same environment a forked coreutil would, NOT the host process's own env.
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

/// Finish a vendored uutil result without letting clap's help/version/error
/// renderer escape to the host process's fd 1/2. Informational output is
/// written to the command's logical stdout, parse errors to logical stderr,
/// and ordinary utility errors keep the usual `name: message` diagnostic.
fn finish_uutil_error(
    error: &dyn uucore::error::UError,
    name: &str,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> i32 {
    // A normal Unix utility is terminated by SIGPIPE when the next pipeline
    // stage closes early; it does not print an I/O diagnostic. In-process
    // utilities receive the equivalent `BrokenPipe` as a Rust error instead,
    // so consume that expected cancellation here and stop the producer with
    // successful builtin semantics. Uutils may attach a file operand (and,
    // for several operands, one line per operand), hence the per-line check.
    if let Some(code) = error.render_logical(out, err) {
        return code;
    }
    let message = error.to_string();
    if !message.is_empty()
        && message
            .lines()
            .all(|line| line.trim().to_ascii_lowercase().ends_with("broken pipe"))
    {
        return 0;
    }
    let code = error.code();
    if !message.is_empty() {
        let _ = writeln!(err, "{name}: {message}");
    }
    code
}

/// `cat` — STREAM template: injected logical stdin/stdout, `splice(2)` fast path intact.
/// See [`run_coreutil_scoped`] for logical execution-context isolation.
struct CatBuiltin;

impl brush_core::builtins::SimpleCommand for CatBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O cat builtin\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() {
            argv.push(OsString::from(&name));
        }

        let out = context
            .try_fd(1)
            .unwrap_or_else(|| std::io::stdout().into());
        let err = context
            .try_fd(2)
            .unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let cwd = context.shell.working_dir().to_path_buf();
        let code = run_coreutil_scoped(
            "uu_cat",
            context.shell.umask(),
            exported_env_snapshot(&context),
            move || {
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
                let r = match uu_cat::cat(argv.into_iter(), &cwd, &mut out, out_fd, &mut inp, in_fd)
                {
                    Ok(()) => 0,
                    Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                };
                let _ = out.flush();
                let _ = err.flush();
                r
            },
        );
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}

/// `head` — STREAM template: injected logical stdin/stdout/stderr. See [`run_coreutil_scoped`].
struct HeadBuiltin;

impl brush_core::builtins::SimpleCommand for HeadBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O head builtin\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() {
            argv.push(OsString::from(&name));
        }

        let out = context
            .try_fd(1)
            .unwrap_or_else(|| std::io::stdout().into());
        let err = context
            .try_fd(2)
            .unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let cwd = context.shell.working_dir().to_path_buf();
        let code = run_coreutil_scoped(
            "uu_head",
            context.shell.umask(),
            exported_env_snapshot(&context),
            move || {
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
                let r = match uu_head::head(
                    argv.into_iter(),
                    &cwd,
                    &mut out,
                    out_fd,
                    &mut inp,
                    in_fd,
                ) {
                    Ok(()) => 0,
                    Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                };
                let _ = out.flush();
                let _ = err.flush();
                r
            },
        );
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}

/// `tail` — STREAM template like [`HeadBuiltin`]. See [`run_coreutil_scoped`].
struct TailBuiltin;

impl brush_core::builtins::SimpleCommand for TailBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O tail builtin\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() {
            argv.push(OsString::from(&name));
        }

        let cwd = context.shell.working_dir().to_path_buf();
        let out = context
            .try_fd(1)
            .unwrap_or_else(|| std::io::stdout().into());
        let err = context
            .try_fd(2)
            .unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let code = run_coreutil_scoped(
            "uu_tail",
            context.shell.umask(),
            exported_env_snapshot(&context),
            move || {
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
                let r = match uu_tail::tail(
                    argv.into_iter(),
                    &cwd,
                    &mut out,
                    out_fd,
                    &mut err,
                    &mut inp,
                    in_fd,
                ) {
                    Ok(()) => 0,
                    Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                };
                let _ = out.flush();
                let _ = err.flush();
                r
            },
        );
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}

/// `wc` — STREAM template like [`HeadBuiltin`]. See [`run_coreutil_scoped`].
struct WcBuiltin;

impl brush_core::builtins::SimpleCommand for WcBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O wc builtin\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() {
            argv.push(OsString::from(&name));
        }

        let cwd = context.shell.working_dir().to_path_buf();
        // Shell's LOGICAL exported env: wc reads POSIXLY_CORRECT from this,
        // not the host process's environment.
        let envv = exported_env_snapshot(&context);
        let out = context
            .try_fd(1)
            .unwrap_or_else(|| std::io::stdout().into());
        let err = context
            .try_fd(2)
            .unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let code =
            run_coreutil_scoped("uu_wc", context.shell.umask(), envv.clone(), move || {
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
                let r = match uu_wc::wc(
                    argv.into_iter(),
                    &cwd,
                    &envv,
                    &mut out,
                    out_fd,
                    &mut err,
                    &mut inp,
                    in_fd,
                ) {
                    Ok(()) => 0,
                    Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                };
                let _ = out.flush();
                let _ = err.flush();
                r
            });
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}

// ── coreutil builtin templates ───────────────────────────────────────────────
// Each coreutil builtin below is ONE macro invocation: the struct name, the
// util text (for `get_content`), the vendored entry path, and its localization
// key passed to `run_coreutil_scoped`. No entry mutates the process cwd.
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
// stderr and returns the util's own `e.code()`; scoped runtime guards keep the
// caller's logical environment, umask, utility name, and localization intact.

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

            fn execute<
                SE: brush_core::extensions::ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            >(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() {
                    argv.push(OsString::from(&name));
                }

                let out = context
                    .try_fd(1)
                    .unwrap_or_else(|| std::io::stdout().into());
                let err = context
                    .try_fd(2)
                    .unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_scoped(
                    $thread,
                    context.shell.umask(),
                    exported_env_snapshot(&context),
                    move || {
                        use std::io::Write;
                        let mut out = out;
                        let mut err = err;
                        let r = match $entry(argv.into_iter(), &mut out, &mut err) {
                            Ok(()) => 0,
                            Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                        };
                        let _ = out.flush();
                        let _ = err.flush();
                        r
                    },
                );
                Ok(brush_core::results::ExecutionResult::new(
                    (code & 0xff) as u8,
                ))
            }
        }
    };
}

/// INFO+ENV shape: `(args, env, out, err)`. `id` suppresses its SELinux context
/// suffix under `POSIXLY_CORRECT`; `nproc` scales by `OMP_NUM_THREADS`/
/// `OMP_THREAD_LIMIT` — both read from the shell's exported env, not the host's.
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

            fn execute<
                SE: brush_core::extensions::ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            >(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() {
                    argv.push(OsString::from(&name));
                }

                let envv = exported_env_snapshot(&context);
                let out = context
                    .try_fd(1)
                    .unwrap_or_else(|| std::io::stdout().into());
                let err = context
                    .try_fd(2)
                    .unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_scoped(
                    $thread,
                    context.shell.umask(),
                    envv.clone(),
                    move || {
                        use std::io::Write;
                        let mut out = out;
                        let mut err = err;
                        let r = match $entry(argv.into_iter(), &envv, &mut out, &mut err) {
                            Ok(()) => 0,
                            Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                        };
                        let _ = out.flush();
                        let _ = err.flush();
                        r
                    },
                );
                Ok(brush_core::results::ExecutionResult::new(
                    (code & 0xff) as u8,
                ))
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

            fn execute<
                SE: brush_core::extensions::ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            >(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() {
                    argv.push(OsString::from(&name));
                }

                let cwd = context.shell.working_dir().to_path_buf();
                let out = context
                    .try_fd(1)
                    .unwrap_or_else(|| std::io::stdout().into());
                let err = context
                    .try_fd(2)
                    .unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_scoped(
                    $thread,
                    context.shell.umask(),
                    exported_env_snapshot(&context),
                    move || {
                        use std::io::Write;
                        let mut out = out;
                        let mut err = err;
                        let r = match $entry(argv.into_iter(), &cwd, &mut out, &mut err) {
                            Ok(()) => 0,
                            Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                        };
                        let _ = out.flush();
                        let _ = err.flush();
                        r
                    },
                );
                Ok(brush_core::results::ExecutionResult::new(
                    (code & 0xff) as u8,
                ))
            }
        }
    };
}

/// FILESYSTEM+ENV shape: `(args, cwd, env, out, err)`. `cp`/`readlink` read
/// `POSIXLY_CORRECT`, `mktemp` reads `$TMPDIR` (a relative one rooted at the
/// logical cwd) — all from the shell's exported env, not the host's.
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

            fn execute<
                SE: brush_core::extensions::ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            >(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() {
                    argv.push(OsString::from(&name));
                }

                let cwd = context.shell.working_dir().to_path_buf();
                let envv = exported_env_snapshot(&context);
                let out = context
                    .try_fd(1)
                    .unwrap_or_else(|| std::io::stdout().into());
                let err = context
                    .try_fd(2)
                    .unwrap_or_else(|| std::io::stderr().into());

                let code = run_coreutil_scoped(
                    $thread,
                    context.shell.umask(),
                    envv.clone(),
                    move || {
                        use std::io::Write;
                        let mut out = out;
                        let mut err = err;
                        let r = match $entry(argv.into_iter(), &cwd, &envv, &mut out, &mut err) {
                            Ok(()) => 0,
                            Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                        };
                        let _ = out.flush();
                        let _ = err.flush();
                        r
                    },
                );
                Ok(brush_core::results::ExecutionResult::new(
                    (code & 0xff) as u8,
                ))
            }
        }
    };
}

/// FILESYSTEM+STDIN shape: `(args, cwd, out, err, stdin)` — `rm -i`/`mv -i`/`ln -i`
/// read the y/N prompt from logical stdin, never the host's fd 0.
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

            fn execute<
                SE: brush_core::extensions::ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            >(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() {
                    argv.push(OsString::from(&name));
                }

                let cwd = context.shell.working_dir().to_path_buf();
                let out = context
                    .try_fd(1)
                    .unwrap_or_else(|| std::io::stdout().into());
                let err = context
                    .try_fd(2)
                    .unwrap_or_else(|| std::io::stderr().into());
                let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

                let code = run_coreutil_scoped(
                    $thread,
                    context.shell.umask(),
                    exported_env_snapshot(&context),
                    move || {
                        use std::io::Write;
                        let mut out = out;
                        let mut err = err;
                        let stdin_src: Box<dyn std::io::BufRead> =
                            Box::new(std::io::BufReader::new(inp));
                        let r = match $entry(argv.into_iter(), &cwd, &mut out, &mut err, stdin_src)
                        {
                            Ok(()) => 0,
                            Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                        };
                        let _ = out.flush();
                        let _ = err.flush();
                        r
                    },
                );
                Ok(brush_core::results::ExecutionResult::new(
                    (code & 0xff) as u8,
                ))
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

            fn execute<
                SE: brush_core::extensions::ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            >(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() {
                    argv.push(OsString::from(&name));
                }

                let cwd = context.shell.working_dir().to_path_buf();
                let out = context
                    .try_fd(1)
                    .unwrap_or_else(|| std::io::stdout().into());
                let err = context
                    .try_fd(2)
                    .unwrap_or_else(|| std::io::stderr().into());
                let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

                let code = run_coreutil_scoped(
                    $thread,
                    context.shell.umask(),
                    exported_env_snapshot(&context),
                    move || {
                        use std::io::Write;
                        let mut out = out;
                        let mut err = err;
                        let mut inp = inp;
                        let r = match $entry(argv.into_iter(), &cwd, &mut out, &mut err, &mut inp) {
                            Ok(()) => 0,
                            Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                        };
                        let _ = out.flush();
                        let _ = err.flush();
                        r
                    },
                );
                Ok(brush_core::results::ExecutionResult::new(
                    (code & 0xff) as u8,
                ))
            }
        }
    };
}

/// STREAM+ENV shape: `(args, cwd, env, out, err, in)`. `sort` reads `$TMPDIR` (its
/// external-sort spill dir) and both read the locale knobs (`LC_ALL`/`LC_CTYPE`/
/// `LANG`) from the shell's exported env, not the host's.
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

            fn execute<
                SE: brush_core::extensions::ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            >(
                context: brush_core::commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
                let name = context.command_name.clone();
                let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
                if argv.is_empty() {
                    argv.push(OsString::from(&name));
                }

                let cwd = context.shell.working_dir().to_path_buf();
                let envv = exported_env_snapshot(&context);
                let out = context
                    .try_fd(1)
                    .unwrap_or_else(|| std::io::stdout().into());
                let err = context
                    .try_fd(2)
                    .unwrap_or_else(|| std::io::stderr().into());
                let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

                let code = run_coreutil_scoped(
                    $thread,
                    context.shell.umask(),
                    envv.clone(),
                    move || {
                        use std::io::Write;
                        let mut out = out;
                        let mut err = err;
                        let mut inp = inp;
                        let r = match $entry(
                            argv.into_iter(),
                            &cwd,
                            &envv,
                            &mut out,
                            &mut err,
                            &mut inp,
                        ) {
                            Ok(()) => 0,
                            Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                        };
                        let _ = out.flush();
                        let _ = err.flush();
                        r
                    },
                );
                Ok(brush_core::results::ExecutionResult::new(
                    (code & 0xff) as u8,
                ))
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
/// single-use macro. See [`run_coreutil_scoped`].
struct TrBuiltin;

impl brush_core::builtins::SimpleCommand for TrBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O tr builtin\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() {
            argv.push(OsString::from(&name));
        }

        let out = context
            .try_fd(1)
            .unwrap_or_else(|| std::io::stdout().into());
        let err = context
            .try_fd(2)
            .unwrap_or_else(|| std::io::stderr().into());
        let inp = context.try_fd(0).unwrap_or_else(|| std::io::stdin().into());

        let code = run_coreutil_scoped(
            "uu_tr",
            context.shell.umask(),
            exported_env_snapshot(&context),
            move || {
                use std::io::Write;
                let mut out = out;
                let mut err = err;
                let mut inp = inp;
                let r = match uu_tr::tr(argv.into_iter(), &mut out, &mut err, &mut inp) {
                    Ok(()) => 0,
                    Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                };
                let _ = out.flush();
                let _ = err.flush();
                r
            },
        );
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}

// FILESYSTEM builtins.
fs_builtin!(MkdirBuiltin, "mkdir", uu_mkdir::mkdir_main, "uu_mkdir");
fs_builtin!(RmdirBuiltin, "rmdir", uu_rmdir::rmdir_main, "uu_rmdir");
fs_builtin!(
    RealpathBuiltin,
    "realpath",
    uu_realpath::realpath,
    "uu_realpath"
);
fs_builtin!(ChmodBuiltin, "chmod", uu_chmod::chmod_main, "uu_chmod");
fs_builtin!(ChownBuiltin, "chown", uu_chown::chown_main, "uu_chown");
fs_builtin!(
    InstallBuiltin,
    "install",
    uu_install::install_main,
    "uu_install"
);
fs_env_builtin!(CpBuiltin, "cp", uu_cp::cp, "uu_cp");
fs_env_builtin!(
    ReadlinkBuiltin,
    "readlink",
    uu_readlink::readlink,
    "uu_readlink"
);
fs_env_builtin!(MktempBuiltin, "mktemp", uu_mktemp::mktemp_main, "uu_mktemp");
fs_builtin_stdin!(RmBuiltin, "rm", uu_rm::rm_main, "uu_rm");
fs_builtin_stdin!(MvBuiltin, "mv", uu_mv::mv_main, "uu_mv");
fs_builtin_stdin!(LnBuiltin, "ln", uu_ln::ln_main, "uu_ln");

/// `touch` — FILESYSTEM shape but hand-written: the `-` operand passes the logical
/// fd 1 as a raw fd so `touch -` updates the logical stdout's referent, and its
/// entry also takes the shell's exported env for the obsolete `_POSIX2_VERSION`
/// knob. The sole member of that shape. See [`run_coreutil_scoped`].
struct TouchBuiltin;

impl brush_core::builtins::SimpleCommand for TouchBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: native injected-I/O touch builtin\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() {
            argv.push(OsString::from(&name));
        }

        let cwd = context.shell.working_dir().to_path_buf();
        let envv = exported_env_snapshot(&context);
        let out = context
            .try_fd(1)
            .unwrap_or_else(|| std::io::stdout().into());
        let err = context
            .try_fd(2)
            .unwrap_or_else(|| std::io::stderr().into());

        let code =
            run_coreutil_scoped("uu_touch", context.shell.umask(), envv.clone(), move || {
                use std::io::Write;
                use std::os::fd::AsRawFd;
                let mut out = out;
                let mut err = err;
                // Raw fd for the logical stdout, for the `-` operand only;
                // borrowed for the call's duration (the OpenFile outlives it).
                let out_fd = out.try_borrow_as_fd().ok().map(|b| b.as_raw_fd());
                let r = match uu_touch::touch_main(argv.into_iter(), &cwd, &envv, out_fd, &mut err)
                {
                    Ok(()) => 0,
                    Err(e) => finish_uutil_error(&*e, &name, &mut out, &mut err),
                };
                let _ = err.flush();
                r
            });
        Ok(brush_core::results::ExecutionResult::new(
            (code & 0xff) as u8,
        ))
    }
}

// INFO builtins.
info_builtin!(
    BasenameBuiltin,
    "basename",
    uu_basename::basename,
    "uu_basename"
);
info_builtin!(DirnameBuiltin, "dirname", uu_dirname::dirname, "uu_dirname");
info_builtin!(SeqBuiltin, "seq", uu_seq::seq, "uu_seq");
info_builtin!(ExprBuiltin, "expr", uu_expr::expr, "uu_expr");
info_builtin!(UnameBuiltin, "uname", uu_uname::uname_main, "uu_uname");
info_builtin!(WhoamiBuiltin, "whoami", uu_whoami::whoami_main, "uu_whoami");
info_env_builtin!(IdBuiltin, "id", uu_id::id_main, "uu_id");
info_env_builtin!(NprocBuiltin, "nproc", uu_nproc::nproc_main, "uu_nproc");

/// Add Bumba's injected-I/O uutils commands to a Brush builtin registry.
///
/// Each utility runs in-process against Brush's logical stdin/stdout, cwd,
/// environment, and umask. Registrations are marked as optimized external
/// commands so shell discovery still reports the executable from `PATH`.
pub fn extend<SE: brush_core::extensions::ShellExtensions>(
    commands: &mut std::collections::HashMap<
        String,
        brush_core::builtins::Registration<SE>,
    >,
) {
    use brush_core::builtins::simple_builtin;

    commands.insert("cat".into(), simple_builtin::<CatBuiltin, SE>());
    commands.insert("head".into(), simple_builtin::<HeadBuiltin, SE>());
    commands.insert("tail".into(), simple_builtin::<TailBuiltin, SE>());
    commands.insert("wc".into(), simple_builtin::<WcBuiltin, SE>());
    commands.insert("nl".into(), simple_builtin::<NlBuiltin, SE>());
    commands.insert("tac".into(), simple_builtin::<TacBuiltin, SE>());
    commands.insert("basename".into(), simple_builtin::<BasenameBuiltin, SE>());
    commands.insert("dirname".into(), simple_builtin::<DirnameBuiltin, SE>());
    commands.insert("seq".into(), simple_builtin::<SeqBuiltin, SE>());
    commands.insert("expr".into(), simple_builtin::<ExprBuiltin, SE>());
    commands.insert("tr".into(), simple_builtin::<TrBuiltin, SE>());
    commands.insert("cut".into(), simple_builtin::<CutBuiltin, SE>());
    commands.insert("uniq".into(), simple_builtin::<UniqBuiltin, SE>());
    commands.insert("sort".into(), simple_builtin::<SortBuiltin, SE>());
    commands.insert("uname".into(), simple_builtin::<UnameBuiltin, SE>());
    commands.insert("nproc".into(), simple_builtin::<NprocBuiltin, SE>());
    commands.insert("id".into(), simple_builtin::<IdBuiltin, SE>());
    commands.insert("whoami".into(), simple_builtin::<WhoamiBuiltin, SE>());
    commands.insert("cp".into(), simple_builtin::<CpBuiltin, SE>());
    commands.insert("mkdir".into(), simple_builtin::<MkdirBuiltin, SE>());
    commands.insert("rmdir".into(), simple_builtin::<RmdirBuiltin, SE>());
    commands.insert("rm".into(), simple_builtin::<RmBuiltin, SE>());
    commands.insert("mv".into(), simple_builtin::<MvBuiltin, SE>());
    commands.insert("ln".into(), simple_builtin::<LnBuiltin, SE>());
    commands.insert("touch".into(), simple_builtin::<TouchBuiltin, SE>());
    commands.insert("readlink".into(), simple_builtin::<ReadlinkBuiltin, SE>());
    commands.insert("realpath".into(), simple_builtin::<RealpathBuiltin, SE>());
    commands.insert("mktemp".into(), simple_builtin::<MktempBuiltin, SE>());
    commands.insert("tee".into(), simple_builtin::<TeeBuiltin, SE>());
    commands.insert("chmod".into(), simple_builtin::<ChmodBuiltin, SE>());
    commands.insert("chown".into(), simple_builtin::<ChownBuiltin, SE>());
    commands.insert("install".into(), simple_builtin::<InstallBuiltin, SE>());

    for name in [
        "cat", "head", "tail", "wc", "nl", "tac", "basename", "dirname",
        "seq", "expr", "tr", "cut", "uniq", "sort", "uname", "nproc",
        "id", "whoami", "cp", "mkdir", "rmdir", "rm", "mv", "ln",
        "touch", "readlink", "realpath", "mktemp", "tee", "chmod", "chown",
        "install",
    ] {
        let registration = commands.get_mut(name).expect("inserted above");
        registration.external_command = true;
        // `touch -` deliberately asks for stdout's native descriptor so it can
        // update the referent. Every other registration here is a leaf command
        // whose injected Read/Write streams have a descriptor-free fallback.
        if name != "touch" {
            registration.userspace_pipe_safe = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run_coreutil_scoped;

    #[test]
    fn coreutil_context_runs_inline_and_restores_its_caller() {
        uucore::logical_env::clear();
        let caller = std::thread::current().id();
        let _outer = uucore::logical_env::push(
            vec![("BUMBA_CONTEXT".into(), "outer".into())],
            0o077,
        );

        let result = run_coreutil_scoped(
            "uu_cat",
            0o022,
            vec![("BUMBA_CONTEXT".into(), "inner".into())],
            || {
                assert_eq!(std::thread::current().id(), caller);
                assert_eq!(
                    uucore::logical_env::get_string("BUMBA_CONTEXT").as_deref(),
                    Some("inner")
                );
                assert_eq!(uucore::logical_env::logical_umask(), Some(0o022));
                17
            },
        );

        assert_eq!(result, 17);
        assert_eq!(
            uucore::logical_env::get_string("BUMBA_CONTEXT").as_deref(),
            Some("outer")
        );
        assert_eq!(uucore::logical_env::logical_umask(), Some(0o077));
    }
}
