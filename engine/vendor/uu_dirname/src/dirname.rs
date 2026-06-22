// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use clap::{Arg, ArgAction, Command};
use std::borrow::Cow;
use std::ffi::OsString;
use std::io::Write;
use uucore::error::{UResult, UUsageError};
use uucore::format_usage;
use uucore::line_ending::LineEnding;

use uucore::translate;

mod options {
    pub const ZERO: &str = "zero";
    pub const DIR: &str = "dir";
}

/// Perform dirname as pure string manipulation per POSIX/GNU behavior.
///
/// dirname should NOT normalize paths. It does simple string manipulation:
/// 1. Strip trailing slashes (unless path is all slashes)
/// 2. If ends with `/.` (possibly `//.` or `///.`), strip the `/+.` pattern
/// 3. Otherwise, remove everything after the last `/`
/// 4. If no `/` found, return `.`
/// 5. Strip trailing slashes from result (unless result would be empty)
///
/// Examples:
/// - `foo/.` → `foo`
/// - `foo/./bar` → `foo/.`
/// - `foo/bar` → `foo`
/// - `a/b/c` → `a/b`
///
/// Per POSIX.1-2017 dirname specification and GNU coreutils manual:
/// - POSIX: <https://pubs.opengroup.org/onlinepubs/9699919799/utilities/dirname.html>
/// - GNU: <https://www.gnu.org/software/coreutils/manual/html_node/dirname-invocation.html>
///
/// See issue #8910 and similar fix in basename (#8373, commit c5268a897).
fn dirname_string_manipulation(path_bytes: &[u8]) -> Cow<'_, [u8]> {
    if path_bytes.is_empty() {
        return Cow::Borrowed(b".");
    }

    let mut bytes = path_bytes;

    // Step 1: Strip trailing slashes (but not if the entire path is slashes)
    let all_slashes = bytes.iter().all(|&b| b == b'/');
    if all_slashes {
        return Cow::Borrowed(b"/");
    }

    while bytes.len() > 1 && bytes.ends_with(b"/") {
        bytes = &bytes[..bytes.len() - 1];
    }

    // Step 2: Check if it ends with `/.` and strip the `/+.` pattern
    if bytes.ends_with(b".") && bytes.len() >= 2 {
        let dot_pos = bytes.len() - 1;
        if bytes[dot_pos - 1] == b'/' {
            // Find where the slashes before the dot start
            let mut slash_start = dot_pos - 1;
            while slash_start > 0 && bytes[slash_start - 1] == b'/' {
                slash_start -= 1;
            }
            // Return the stripped result
            if slash_start == 0 {
                // Result would be empty
                return if path_bytes.starts_with(b"/") {
                    Cow::Borrowed(b"/")
                } else {
                    Cow::Borrowed(b".")
                };
            }
            return Cow::Borrowed(&bytes[..slash_start]);
        }
    }

    // Step 3: Normal dirname - find last / and remove everything after it
    if let Some(last_slash_pos) = bytes.iter().rposition(|&b| b == b'/') {
        // Found a slash, remove everything after it
        let mut result = &bytes[..last_slash_pos];

        // Strip trailing slashes from result (but keep at least one if at the start)
        while result.len() > 1 && result.ends_with(b"/") {
            result = &result[..result.len() - 1];
        }

        if result.is_empty() {
            return Cow::Borrowed(b"/");
        }

        return Cow::Borrowed(result);
    }

    // No slash found, return "."
    Cow::Borrowed(b".")
}

/// Logical entry point for the bundled `dirname` builtin.
///
/// Unlike upstream's `uumain`, this never touches the process's own stdio: it
/// drives `dirname` against the shell's logical output sink (`out`) and routes
/// any diagnostic to the logical error sink (`err`) — and otherwise RETURNS the
/// error as a `UResult` (the caller decides how to render it). It holds no
/// process-global state and never `dup2`s, so it is safe to run in-process
/// beside other commands.
///
/// `dirname` reads NO stdin; it writes each computed parent directory to `out`.
/// On Unix the result is raw path bytes (matching upstream's `print_verbatim`,
/// which is just `stdout().write_all(bytes)` on Unix) followed by the
/// `line_ending` (newline, or NUL under `-z`/`--zero`). Output is byte-for-byte
/// identical to upstream — only the *destination* changed.
pub fn dirname(
    args: impl uucore::Args,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> UResult<()> {
    // `err` is reserved for the same role as upstream's stderr (clap usage/help
    // text), but uucore renders clap errors itself via the returned UResult, so
    // on the live path there is currently nothing to write here. Bind it so the
    // signature stays uniform with the other injected-I/O builtins.
    let _ = &err;

    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let line_ending = LineEnding::from_zero_flag(matches.get_flag(options::ZERO));

    let dirnames: Vec<OsString> = matches
        .get_many::<OsString>(options::DIR)
        .unwrap_or_default()
        .cloned()
        .collect();

    if dirnames.is_empty() {
        return Err(UUsageError::new(1, translate!("dirname-missing-operand")));
    }

    for path in &dirnames {
        let path_bytes = uucore::os_str_as_bytes(path.as_os_str()).unwrap_or(&[]);
        let result = dirname_string_manipulation(path_bytes);

        // Upstream writes `result` to stdout via `print_verbatim`, which on Unix
        // is exactly `stdout().write_all(result_bytes)`. We write the same bytes
        // to the injected logical sink instead. Any write error is RETURNED.
        out.write_all(&result)?;

        // `line_ending` is `Display`; upstream `print!("{line_ending}")`s it. It
        // is always plain ASCII ('\n' or '\0'), so formatting and writing its
        // bytes is byte-for-byte identical, with no process stdio.
        write!(out, "{line_ending}")?;
    }

    Ok(())
}

/// Descriptor-based entry point for the standalone `dirname` binary.
///
/// In that context the process's own fd 1/2 *are* the intended sinks, so this is
/// a thin bridge that locks the real `stdout`/`stderr` and hands them to
/// [`dirname`]. The in-process brush builtin must NOT route through here — it
/// calls [`dirname`] directly with the shell's logical sinks, so it never
/// touches process-global stdio.
#[uucore::main(no_signals)]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    dirname(args, &mut out, &mut err)
}

pub fn uu_app() -> Command {
    Command::new("dirname")
        .about(translate!("dirname-about"))
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template(uucore::util_name()))
        .override_usage(format_usage(&translate!("dirname-usage")))
        .args_override_self(true)
        .infer_long_args(true)
        .after_help(translate!("dirname-after-help"))
        .arg(
            Arg::new(options::ZERO)
                .long(options::ZERO)
                .short('z')
                .help(translate!("dirname-zero-help"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::DIR)
                .hide(true)
                .action(ArgAction::Append)
                .value_hint(clap::ValueHint::AnyPath)
                .value_parser(clap::value_parser!(OsString)),
        )
}

#[cfg(test)]
mod tests {
    use super::dirname;

    /// Drive the logical entry with `Vec<u8>` sinks (no process stdio) and
    /// return (stdout, stderr, ok).
    fn run(args: &[&str]) -> (String, String, bool) {
        let argv: Vec<std::ffi::OsString> = std::iter::once("dirname")
            .chain(args.iter().copied())
            .map(std::ffi::OsString::from)
            .collect();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let res = dirname(argv.into_iter(), &mut out, &mut err);
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            res.is_ok(),
        )
    }

    #[test]
    fn basic() {
        let (out, err, ok) = run(&["/a/b/c"]);
        assert!(ok);
        assert_eq!(out, "/a/b\n");
        assert_eq!(err, "");
    }

    #[test]
    fn multiple_operands() {
        let (out, _err, ok) = run(&["a/b", "c/d"]);
        assert!(ok);
        assert_eq!(out, "a\nc\n");
    }

    #[test]
    fn zero_separator() {
        let (out, _err, ok) = run(&["-z", "a/b", "c/d"]);
        assert!(ok);
        assert_eq!(out, "a\0c\0");
    }

    #[test]
    fn no_slash_is_dot() {
        let (out, _err, ok) = run(&["foo"]);
        assert!(ok);
        assert_eq!(out, ".\n");
    }

    #[test]
    fn root_and_trailing_slashes() {
        assert_eq!(run(&["/"]).0, "/\n");
        assert_eq!(run(&["/a/b/"]).0, "/a\n");
        assert_eq!(run(&["a//b"]).0, "a\n");
    }

    #[test]
    fn double_dash_terminator() {
        let (out, _err, ok) = run(&["--", "-x/y"]);
        assert!(ok);
        assert_eq!(out, "-x\n");
    }

    #[test]
    fn missing_operand_is_err_and_writes_nothing() {
        let (out, err, ok) = run(&[]);
        assert!(!ok);
        assert_eq!(out, "");
        assert_eq!(err, "");
    }
}
