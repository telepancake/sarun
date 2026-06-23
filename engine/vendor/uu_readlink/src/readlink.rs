// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) errno

use clap::{Arg, ArgAction, Command};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use uucore::display::Quotable;
use uucore::error::{FromIo, UResult, UUsageError};
use uucore::fs::{MissingHandling, ResolveMode, canonicalize};
use uucore::libc::EINVAL;
use uucore::line_ending::LineEnding;
use uucore::translate;
use uucore::format_usage;

// ── Injected-I/O plumbing for the in-process brush builtin ───────────────────
//
// CWD template, mirroring uu_realpath: [`readlink`] resolves every relative
// operand against the shell's LOGICAL cwd (the process is never `chdir`'d), and
// writes resolved paths to the injected logical stdout, diagnostics to the
// injected stderr. `readlink` runs on a FRESH worker thread per call (the
// engine's `run_coreutil_localized`), so this thread-local err buffer is
// per-instance. The crate-local `show_error!` SHADOWS uucore's (which writes fd
// 2); both the logical entry and the standalone [`uumain`] drain it (the latter
// to real stderr), so standalone behavior is unchanged.
thread_local! {
    static RL_ERR: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Shadows [`uucore::show_error!`]: writes `<util_name>: <msg>` to the logical
/// stderr buffer instead of process fd 2.
macro_rules! show_error {
    ($($args:tt)+) => ({
        use std::io::Write as _;
        $crate::RL_ERR.with(|b| {
            let mut b = b.borrow_mut();
            let _ = write!(b, "{}: ", uucore::util_name());
            let _ = writeln!(b, $($args)+);
        });
    });
}

const OPT_CANONICALIZE: &str = "canonicalize";
const OPT_CANONICALIZE_MISSING: &str = "canonicalize-missing";
const OPT_CANONICALIZE_EXISTING: &str = "canonicalize-existing";
const OPT_NO_NEWLINE: &str = "no-newline";
const OPT_QUIET: &str = "quiet";
const OPT_SILENT: &str = "silent";
const OPT_VERBOSE: &str = "verbose";
const OPT_ZERO: &str = "zero";

const ARG_FILES: &str = "files";

/// Logical entry point for the in-process brush `readlink` builtin.
///
/// Mirrors [`uumain`] but (1) resolves every relative operand against the
/// shell's LOGICAL `cwd` (the process is never `chdir`'d), and (2) never touches
/// process fd 1/2 — resolved paths go to `out`, diagnostics to `err`.
pub fn readlink(
    args: impl uucore::Args,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> UResult<()> {
    RL_ERR.with(|b| b.borrow_mut().clear());
    let result = run(args, Some(cwd), out);
    let produced_err = RL_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = out.flush();
    let _ = err.write_all(&produced_err);
    let _ = err.flush();
    result
}

#[uucore::main(no_signals)]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    RL_ERR.with(|b| b.borrow_mut().clear());
    let mut out = std::io::stdout();
    let result = run(args, None, &mut out);
    let produced_err = RL_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = std::io::stderr().write_all(&produced_err);
    result
}

/// Shared body of [`readlink`] and [`uumain`]. When `cwd` is `Some`, relative
/// operands are rooted at the shell's logical cwd; when `None` (standalone) they
/// resolve against the process cwd, as upstream.
fn run(args: impl uucore::Args, cwd: Option<&Path>, out: &mut dyn Write) -> UResult<()> {
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let mut no_trailing_delimiter = matches.get_flag(OPT_NO_NEWLINE);
    let use_zero = matches.get_flag(OPT_ZERO);
    let verbose = matches.get_flag(OPT_VERBOSE) || env::var("POSIXLY_CORRECT").is_ok();

    // GNU readlink -f/-e/-m follows symlinks first and then applies `..` (physical resolution).
    // ResolveMode::Logical collapses `..` before following links, which yields the opposite order,
    // so we choose Physical here for GNU compatibility.
    let res_mode = if matches.get_flag(OPT_CANONICALIZE)
        || matches.get_flag(OPT_CANONICALIZE_EXISTING)
        || matches.get_flag(OPT_CANONICALIZE_MISSING)
    {
        ResolveMode::Physical
    } else {
        ResolveMode::None
    };

    let can_mode = if matches.get_flag(OPT_CANONICALIZE_EXISTING) {
        MissingHandling::Existing
    } else if matches.get_flag(OPT_CANONICALIZE_MISSING) {
        MissingHandling::Missing
    } else {
        MissingHandling::Normal
    };

    let files: Vec<PathBuf> = matches
        .get_many::<OsString>(ARG_FILES)
        .map(|v| {
            v.map(PathBuf::from)
                .map(|p| match cwd {
                    Some(cwd) if p.is_relative() => cwd.join(p),
                    _ => p,
                })
                .collect()
        })
        .unwrap_or_default();

    if files.is_empty() {
        return Err(UUsageError::new(
            1,
            translate!("readlink-error-missing-operand"),
        ));
    }

    if no_trailing_delimiter && files.len() > 1 {
        show_error!("{}", translate!("readlink-error-ignoring-no-newline"));
        no_trailing_delimiter = false;
    }

    let line_ending = if no_trailing_delimiter {
        None
    } else {
        Some(LineEnding::from_zero_flag(use_zero))
    };

    for p in &files {
        let path_result = if res_mode == ResolveMode::None {
            fs::read_link(p)
        } else {
            canonicalize(p, can_mode, res_mode)
        };

        match path_result {
            Ok(path) => {
                show(out, &path, line_ending).map_err_context(String::new)?;
            }
            Err(err) => {
                if !verbose {
                    return Err(1.into());
                }

                let message = if err.raw_os_error() == Some(EINVAL) {
                    translate!("readlink-error-invalid-argument", "path" => p.maybe_quote())
                } else {
                    err.map_err_context(|| p.maybe_quote().to_string())
                        .to_string()
                };
                show_error!("{message}");
                return Err(1.into());
            }
        }
    }
    Ok(())
}

pub fn uu_app() -> Command {
    Command::new("readlink")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("readlink"))
        .about(translate!("readlink-about"))
        .override_usage(format_usage(&translate!("readlink-usage")))
        .infer_long_args(true)
        .arg(
            Arg::new(OPT_CANONICALIZE)
                .short('f')
                .long(OPT_CANONICALIZE)
                .help(translate!("readlink-help-canonicalize"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_CANONICALIZE_EXISTING)
                .short('e')
                .long("canonicalize-existing")
                .help(translate!("readlink-help-canonicalize-existing"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_CANONICALIZE_MISSING)
                .short('m')
                .long(OPT_CANONICALIZE_MISSING)
                .help(translate!("readlink-help-canonicalize-missing"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_NO_NEWLINE)
                .short('n')
                .long(OPT_NO_NEWLINE)
                .help(translate!("readlink-help-no-newline"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_QUIET)
                .short('q')
                .long(OPT_QUIET)
                .help(translate!("readlink-help-quiet"))
                .overrides_with_all([OPT_QUIET, OPT_SILENT, OPT_VERBOSE])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_SILENT)
                .short('s')
                .long(OPT_SILENT)
                .help(translate!("readlink-help-silent"))
                .overrides_with_all([OPT_QUIET, OPT_SILENT, OPT_VERBOSE])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_VERBOSE)
                .short('v')
                .long(OPT_VERBOSE)
                .help(translate!("readlink-help-verbose"))
                .overrides_with_all([OPT_QUIET, OPT_SILENT, OPT_VERBOSE])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_ZERO)
                .short('z')
                .long(OPT_ZERO)
                .help(translate!("readlink-help-zero"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(ARG_FILES)
                .action(ArgAction::Append)
                .value_parser(clap::value_parser!(OsString))
                .value_hint(clap::ValueHint::AnyPath),
        )
}

fn show(
    out: &mut dyn Write,
    path: &Path,
    line_ending: Option<LineEnding>,
) -> std::io::Result<()> {
    // Write the path's raw bytes (preserving non-UTF-8 names) to the injected
    // logical stdout, the same information `uucore::display::print_verbatim`
    // writes to fd 1 in the standalone path.
    use std::os::unix::ffi::OsStrExt as _;
    out.write_all(path.as_os_str().as_bytes())?;
    if let Some(line_ending) = line_ending {
        write!(out, "{line_ending}")?;
    }
    out.flush()
}
