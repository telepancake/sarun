// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) fullname

use clap::builder::ValueParser;
use clap::{Arg, ArgAction, Command};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use uucore::display::Quotable;
use uucore::error::{UResult, UUsageError};
use uucore::format_usage;
use uucore::line_ending::LineEnding;

use uucore::translate;

pub mod options {
    pub static MULTIPLE: &str = "multiple";
    pub static NAME: &str = "name";
    pub static SUFFIX: &str = "suffix";
    pub static ZERO: &str = "zero";
}

/// Logical entry point for the in-process brush builtin.
///
/// `basename` consumes only operands (it never reads stdin), so the signature
/// takes no input source: it writes the computed name(s) to the injected `out`
/// sink and routes nothing to `err` itself on the success path. Argument/usage
/// errors are RETURNED as a `UResult` (a `UUsageError`/`UError` carrying the
/// proper exit code) rather than printed to a process-global stderr — the caller
/// decides where to surface them on the LOGICAL `err`. This entry NEVER touches
/// the process's fd 0/1/2: no `io::stdout()`/`stdin()`/`stderr()`, no `print!`
/// family, no uucore `show!`/`set_exit_code`. That makes it parallel-safe.
///
/// `err` is currently unused on the success path (basename emits no diagnostics
/// of its own — every failure is a returned `UError`); it is accepted to match
/// the engine's uniform `(args, out, err)` builtin entry shape and to leave room
/// for future logical diagnostics without another signature change.
pub fn basename(
    args: impl uucore::Args,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> UResult<()> {
    //
    // Argument parsing
    //
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let line_ending = LineEnding::from_zero_flag(matches.get_flag(options::ZERO));

    let mut name_args = matches
        .get_many::<OsString>(options::NAME)
        .unwrap_or_default()
        .collect::<Vec<_>>();
    if name_args.is_empty() {
        return Err(UUsageError::new(
            1,
            translate!("basename-error-missing-operand"),
        ));
    }
    let multiple_paths = matches.get_one::<OsString>(options::SUFFIX).is_some()
        || matches.get_flag(options::MULTIPLE);
    let suffix = if multiple_paths {
        matches
            .get_one::<OsString>(options::SUFFIX)
            .cloned()
            .unwrap_or_default()
    } else {
        // "simple format"
        match name_args.len() {
            0 => panic!("already checked"),
            1 => OsString::default(),
            2 => name_args.pop().unwrap().clone(),
            _ => {
                return Err(UUsageError::new(
                    1,
                    translate!("basename-error-extra-operand",
                               "operand" => name_args[2].quote()),
                ));
            }
        }
    };

    //
    // Main Program Processing
    //

    // Upstream re-acquired `stdout()` once per operand and wrote to it directly.
    // We write to the single injected `out` sink instead — behavior-identical
    // byte stream (name + line_ending per operand, in order), but never touching
    // the process's global stdout.
    for path in name_args {
        out.write_all(&base_name(path, &suffix)?)?;
        write!(out, "{line_ending}")?;
    }

    Ok(())
}

/// Descriptor-based entry point for the standalone `basename` binary.
///
/// Here the process's own fd 1/2 *are* the intended I/O, so this is a thin
/// bridge that locks them and hands them to [`basename`]. The in-process brush
/// builtin must NOT route through here — it calls [`basename`] directly with the
/// shell's logical sinks, so it never touches process-global stdio. basename has
/// no stdin, so none is acquired.
#[uucore::main(no_signals)]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let result = basename(args, &mut out, &mut err);
    // GNU `basename` flushes stdout before exiting; the locked Stdout is
    // line-buffered, so flush explicitly.
    let _ = out.flush();
    result
}

pub fn uu_app() -> Command {
    Command::new("basename")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("basename"))
        .about(translate!("basename-about"))
        .override_usage(format_usage(&translate!("basename-usage")))
        .infer_long_args(true)
        .arg(
            Arg::new(options::MULTIPLE)
                .short('a')
                .long(options::MULTIPLE)
                .help(translate!("basename-help-multiple"))
                .action(ArgAction::SetTrue)
                .overrides_with(options::MULTIPLE),
        )
        .arg(
            Arg::new(options::NAME)
                .action(ArgAction::Append)
                .value_parser(ValueParser::os_string())
                .value_hint(clap::ValueHint::AnyPath)
                .hide(true)
                .trailing_var_arg(true),
        )
        .arg(
            Arg::new(options::SUFFIX)
                .short('s')
                .long(options::SUFFIX)
                .value_name("SUFFIX")
                .value_parser(ValueParser::os_string())
                .help(translate!("basename-help-suffix"))
                .overrides_with(options::SUFFIX),
        )
        .arg(
            Arg::new(options::ZERO)
                .short('z')
                .long(options::ZERO)
                .help(translate!("basename-help-zero"))
                .action(ArgAction::SetTrue)
                .overrides_with(options::ZERO),
        )
}

// We return a Vec<u8>. Returning a seemingly more proper `OsString` would
// require back and forth conversions as we need a &[u8] for printing anyway.
//
// Renamed from the upstream private `basename` to `base_name` so the public
// logical entry above can own the `basename` name. The body is byte-for-byte
// upstream — pure computation, no I/O.
fn base_name(fullname: &OsString, suffix: &OsString) -> UResult<Vec<u8>> {
    let fullname_bytes = uucore::os_str_as_bytes(fullname)?;

    // Handle special case where path ends with /.
    if fullname_bytes.ends_with(b"/.") {
        return Ok(b".".into());
    }

    // Convert to path buffer and get last path component
    let pb = PathBuf::from(fullname);

    pb.components().next_back().map_or(Ok([].into()), |c| {
        let name = c.as_os_str();
        let name_bytes = uucore::os_str_as_bytes(name)?;
        if name == suffix {
            Ok(name_bytes.into())
        } else {
            let suffix_bytes = uucore::os_str_as_bytes(suffix)?;
            Ok(name_bytes
                .strip_suffix(suffix_bytes)
                .unwrap_or(name_bytes)
                .into())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Drive the public logical entry with an in-memory out/err sink (Vec<u8>),
    // exactly as the engine's brush builtin does — proving it never needs the
    // process's real stdout/stderr.
    fn run(args: &[&str]) -> (Vec<u8>, Vec<u8>, Option<i32>) {
        let argv: Vec<OsString> = std::iter::once(OsString::from("basename"))
            .chain(args.iter().map(OsString::from))
            .collect();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = match basename(argv.into_iter(), &mut out, &mut err) {
            Ok(()) => None,
            Err(e) => Some(e.code()),
        };
        (out, err, code)
    }

    #[test]
    fn plain() {
        let (out, _err, code) = run(&["/a/b/c"]);
        assert_eq!(out, b"c\n");
        assert_eq!(code, None);
    }

    #[test]
    fn with_suffix() {
        let (out, _err, code) = run(&["/a/b.txt", ".txt"]);
        assert_eq!(out, b"b\n");
        assert_eq!(code, None);
    }

    #[test]
    fn multiple() {
        let (out, _err, code) = run(&["-a", "/a/x", "/b/y"]);
        assert_eq!(out, b"x\ny\n");
        assert_eq!(code, None);
    }

    #[test]
    fn suffix_flag() {
        let (out, _err, code) = run(&["-s", ".o", "lib/foo.o", "bar.o"]);
        assert_eq!(out, b"foo\nbar\n");
        assert_eq!(code, None);
    }

    #[test]
    fn zero_terminated() {
        let (out, _err, code) = run(&["-z", "/a/b/c"]);
        assert_eq!(out, b"c\0");
        assert_eq!(code, None);
    }

    #[test]
    fn root_slash() {
        let (out, _err, code) = run(&["/"]);
        assert_eq!(out, b"/\n");
        assert_eq!(code, None);
    }

    #[test]
    fn trailing_slashes() {
        let (out, _err, code) = run(&["/a/b/c///"]);
        assert_eq!(out, b"c\n");
        assert_eq!(code, None);
    }

    #[test]
    fn no_slash() {
        let (out, _err, code) = run(&["hello"]);
        assert_eq!(out, b"hello\n");
        assert_eq!(code, None);
    }

    #[test]
    fn missing_operand_is_error() {
        let (out, _err, code) = run(&[]);
        assert!(out.is_empty());
        assert_eq!(code, Some(1));
    }
}
