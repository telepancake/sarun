// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) ENOTDIR

use clap::builder::ValueParser;
use clap::{Arg, ArgAction, Command};
use std::ffi::OsString;
use std::fs::{read_dir, remove_dir};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uucore::display::Quotable;
use uucore::error::{UResult, USimpleError, strip_errno};
use uucore::translate;

use uucore::format_usage;

// ── Injected-I/O plumbing for the in-process brush builtin ───────────────────
//
// FILESYSTEM template, mirroring uu_cp: [`rmdir_main`] resolves every relative
// operand against the shell's LOGICAL cwd (the process is never `chdir`'d) and
// routes output to the shell's logical sinks. Runs on a FRESH worker thread per
// call (the engine's `run_coreutil_localized`), so these thread-locals are
// per-instance: `RMDIR_OUT` buffers verbose output, `RMDIR_ERR` diagnostics,
// `RMDIR_EXIT` the deferred exit code. The crate-local `show_error!` and
// `set_exit_code` shims below SHADOW uucore's (which write fd 2 + the process-
// global exit code); the verbose `println!` site targets `RMDIR_OUT`. Both the
// logical entry and standalone [`uumain`] drain the buffers, so standalone
// behavior is unchanged.
thread_local! {
    static RMDIR_OUT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static RMDIR_ERR: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static RMDIR_EXIT: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Route the deferred exit code into [`RMDIR_EXIT`] instead of uucore's
/// process-global `EXIT_CODE`.
fn set_exit_code(code: i32) {
    RMDIR_EXIT.with(|c| c.set(code));
}

/// Shadows [`uucore::show_error!`]: writes `<util_name>: <msg>` to the logical
/// stderr buffer (never fd 2).
macro_rules! show_error {
    ($($args:tt)+) => {{
        use std::io::Write as _;
        $crate::RMDIR_ERR.with(|b| {
            let mut b = b.borrow_mut();
            let _ = write!(b, "{}: ", uucore::util_name());
            let _ = writeln!(b, $($args)+);
        });
    }};
}

static OPT_IGNORE_FAIL_NON_EMPTY: &str = "ignore-fail-on-non-empty";
static OPT_PARENTS: &str = "parents";
static OPT_VERBOSE: &str = "verbose";

static ARG_DIRS: &str = "dirs";

/// Logical entry point for the in-process brush `rmdir` builtin.
///
/// Mirrors [`uumain`] but (1) resolves every relative directory operand against
/// the shell's LOGICAL `cwd` (the process is never `chdir`'d), and (2) never
/// touches process fd 1/2 — verbose output drains to `out`, diagnostics to
/// `err`, the deferred exit code surfaces as the returned status.
pub fn rmdir_main(
    args: impl uucore::Args,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> UResult<()> {
    RMDIR_OUT.with(|b| b.borrow_mut().clear());
    RMDIR_ERR.with(|b| b.borrow_mut().clear());
    RMDIR_EXIT.with(|c| c.set(0));

    let result = run(args, Some(cwd));

    let produced_out = RMDIR_OUT.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let produced_err = RMDIR_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = out.write_all(&produced_out);
    let _ = out.flush();
    let _ = err.write_all(&produced_err);
    let _ = err.flush();

    let deferred = RMDIR_EXIT.with(std::cell::Cell::get);
    match result {
        Ok(()) if deferred != 0 => Err(USimpleError::new(deferred, String::new())),
        other => other,
    }
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    RMDIR_OUT.with(|b| b.borrow_mut().clear());
    RMDIR_ERR.with(|b| b.borrow_mut().clear());
    RMDIR_EXIT.with(|c| c.set(0));

    let result = run(args, None);

    let produced_out = RMDIR_OUT.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let produced_err = RMDIR_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = io::stdout().write_all(&produced_out);
    let _ = io::stderr().write_all(&produced_err);

    let deferred = RMDIR_EXIT.with(std::cell::Cell::get);
    match result {
        Ok(()) if deferred != 0 => Err(USimpleError::new(deferred, String::new())),
        other => other,
    }
}

/// Shared body of [`rmdir_main`] and [`uumain`]. With `cwd` `Some`, relative
/// operands root at the shell's logical cwd; with `None` (standalone) they
/// resolve against the process cwd, as upstream.
fn run(args: impl uucore::Args, cwd: Option<&Path>) -> UResult<()> {
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let opts = Opts {
        ignore: matches.get_flag(OPT_IGNORE_FAIL_NON_EMPTY),
        parents: matches.get_flag(OPT_PARENTS),
        verbose: matches.get_flag(OPT_VERBOSE),
    };

    let dirs: Vec<PathBuf> = matches
        .get_many::<OsString>(ARG_DIRS)
        .unwrap_or_default()
        .map(PathBuf::from)
        .map(|p| match cwd {
            Some(cwd) if p.is_relative() => cwd.join(p),
            _ => p,
        })
        .collect();

    for path in dirs.iter().map(PathBuf::as_path) {
        if let Err(error) = remove(path, opts, cwd) {
            let Error { error, path } = error;

            if opts.ignore && dir_not_empty(&error, path) {
                continue;
            }

            set_exit_code(1);

            // If `foo` is a symlink to a directory then `rmdir foo/` may give
            // a "not a directory" error. This is confusing as `rm foo/` says
            // "is a directory".
            // This differs from system to system. Some don't give an error.
            // Windows simply allows calling RemoveDirectory on symlinks so we
            // don't need to worry about it here.
            // GNU rmdir seems to print "Symbolic link not followed" if:
            // - It has a trailing slash
            // - It's a symlink
            // - It either points to a directory or dangles
            #[cfg(unix)]
            {
                use std::ffi::OsStr;
                use std::os::unix::ffi::OsStrExt;

                fn points_to_directory(path: &Path) -> io::Result<bool> {
                    Ok(path.metadata()?.file_type().is_dir())
                }

                let mut bytes = path.as_os_str().as_bytes();
                if error.raw_os_error() == Some(libc::ENOTDIR) && bytes.ends_with(b"/") {
                    // Strip the trailing slash or .symlink_metadata() will follow the symlink
                    bytes = strip_trailing_slashes_from_path(bytes);
                    let no_slash: &Path = OsStr::from_bytes(bytes).as_ref();
                    if no_slash.is_symlink() && points_to_directory(no_slash).unwrap_or(true) {
                        show_error!(
                            "{}",
                            translate!("rmdir-error-symbolic-link-not-followed", "path" => path.quote())
                        );
                        continue;
                    }
                }
            }

            show_error!(
                "{}",
                translate!("rmdir-error-failed-to-remove", "path" => path.quote(), "err" => strip_errno(&error))
            );
        }
    }

    Ok(())
}

struct Error<'a> {
    error: io::Error,
    path: &'a Path,
}

fn remove<'a>(mut path: &'a Path, opts: Opts, boundary: Option<&Path>) -> Result<(), Error<'a>> {
    remove_single(path, opts)?;
    if opts.parents {
        while let Some(new) = path.parent() {
            path = new;
            if path.as_os_str().is_empty() {
                break;
            }
            // When operands were resolved against the shell's LOGICAL cwd
            // (`boundary`), `rmdir -p a/b/c` must walk only the OPERAND's own
            // ancestors (a/b, a) — not the cwd or its filesystem ancestors, which
            // GNU never touches. Stop once we reach the cwd boundary.
            if boundary.is_some_and(|b| path == b) {
                break;
            }
            remove_single(path, opts)?;
        }
    }
    Ok(())
}

fn remove_single(path: &Path, opts: Opts) -> Result<(), Error<'_>> {
    if opts.verbose {
        RMDIR_OUT.with(|b| {
            let _ = writeln!(
                b.borrow_mut(),
                "{}",
                translate!("rmdir-verbose-removing-directory", "util_name" => "rmdir", "path" => path.quote())
            );
        });
    }
    remove_dir(path).map_err(|error| Error { error, path })
}

#[cfg(unix)]
fn strip_trailing_slashes_from_path(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

// POSIX: https://pubs.opengroup.org/onlinepubs/009696799/functions/rmdir.html
#[cfg(not(windows))]
const NOT_EMPTY_CODES: &[i32] = &[libc::ENOTEMPTY, libc::EEXIST];

// 145 is ERROR_DIR_NOT_EMPTY, determined experimentally.
#[cfg(windows)]
const NOT_EMPTY_CODES: &[i32] = &[145];

// Other error codes you might get for directories that could be found and are
// not empty.
// This is a subset of the error codes listed in rmdir(2) from the Linux man-pages
// project. Maybe other systems have additional codes that apply?
#[cfg(not(windows))]
const PERHAPS_EMPTY_CODES: &[i32] = &[libc::EACCES, libc::EBUSY, libc::EPERM, libc::EROFS];

// Probably incomplete, I can't find a list of possible errors for
// RemoveDirectory anywhere.
#[cfg(windows)]
const PERHAPS_EMPTY_CODES: &[i32] = &[
    5, // ERROR_ACCESS_DENIED, found experimentally.
];

fn dir_not_empty(error: &io::Error, path: &Path) -> bool {
    if let Some(code) = error.raw_os_error() {
        if NOT_EMPTY_CODES.contains(&code) {
            return true;
        }
        // If --ignore-fail-on-non-empty is used then we want to ignore all errors
        // for non-empty directories, even if the error was e.g. because there's
        // no permission. So we do an additional check.
        if PERHAPS_EMPTY_CODES.contains(&code) {
            if let Ok(mut iterator) = read_dir(path) {
                if iterator.next().is_some() {
                    return true;
                }
            }
        }
    }
    false
}

#[derive(Clone, Copy, Debug)]
struct Opts {
    ignore: bool,
    parents: bool,
    verbose: bool,
}

pub fn uu_app() -> Command {
    Command::new("rmdir")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("rmdir"))
        .about(translate!("rmdir-about"))
        .override_usage(format_usage(&translate!("rmdir-usage")))
        .infer_long_args(true)
        .arg(
            Arg::new(OPT_IGNORE_FAIL_NON_EMPTY)
                .long(OPT_IGNORE_FAIL_NON_EMPTY)
                .help(translate!("rmdir-help-ignore-fail-non-empty"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_PARENTS)
                .short('p')
                .long(OPT_PARENTS)
                .help(translate!("rmdir-help-parents"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_VERBOSE)
                .short('v')
                .long(OPT_VERBOSE)
                .help(translate!("rmdir-help-verbose"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(ARG_DIRS)
                .action(ArgAction::Append)
                .num_args(1..)
                .required(true)
                .value_parser(ValueParser::os_string())
                .value_hint(clap::ValueHint::DirPath),
        )
}
