// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) sbytes slen dlen memmem memmap Mmap mmap SIGBUS

mod error;

use clap::{Arg, ArgAction, Command};
use memchr::memmem;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use uucore::error::UResult;
use uucore::format_usage;

use crate::error::TacError;

use uucore::translate;

mod options {
    pub static BEFORE: &str = "before";
    pub static REGEX: &str = "regex";
    pub static SEPARATOR: &str = "separator";
    pub static FILE: &str = "file";
}

/// Logical entry point for the bundled `tac` builtin.
///
/// Unlike upstream's `uumain`, this never touches the process's own stdio. It
/// drives `tac` against the shell's logical output sink (`out`), the logical
/// diagnostic sink (`err`), and the logical input source (`stdin`).
///
/// `tac` is inherently a buffer-the-whole-input utility (it must read every
/// byte before it can emit the LAST line first), so there is no streaming /
/// splice fast path to preserve and no backing descriptor is needed: stdin is
/// drained through the `stdin` reader into an owned `Vec<u8>`. Files are still
/// opened by path (so per-file open/read errors stay byte-faithful to GNU) and
/// read fully into memory. This holds no process-global state and is safe to
/// run as one stage of a concurrent pipeline.
///
/// Per-file open/read failures are written to `err` (each prefixed with the
/// "tac: " program name, matching upstream's `show!`) and processing continues
/// with the next file; the call then returns a non-zero `UResult` so the
/// caller can surface GNU's exit status (1 on any such error). A write error
/// to `out` aborts immediately, exactly as upstream.
pub fn tac(
    args: impl uucore::Args,
    out: &mut dyn Write,
    err: &mut dyn Write,
    stdin: &mut dyn Read,
) -> UResult<()> {
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let before = matches.get_flag(options::BEFORE);
    let regex = matches.get_flag(options::REGEX);
    let raw_separator = matches
        .get_one::<OsString>(options::SEPARATOR)
        .map_or(OsStr::new("\n"), |s| s.as_os_str());

    let separator = if raw_separator.is_empty() {
        OsStr::new("\0")
    } else {
        raw_separator
    };

    let files: Vec<OsString> = match matches.get_many::<OsString>(options::FILE) {
        Some(v) => v.cloned().collect(),
        None => vec![OsString::from("-")],
    };

    tac_impl(&files, before, regex, separator, out, err, stdin)
}

/// Descriptor-based entry point for the standalone `tac` binary and the
/// `--invoke-bundled` subprocess dispatcher.
///
/// In those contexts the process's own fd 0/1/2 *are* the intended input,
/// output, and diagnostics, so this is a thin bridge that hands them to
/// [`tac`]. The in-process brush builtin must NOT route through here — it calls
/// [`tac`] directly with the shell's logical sinks/source, so it never touches
/// process-global stdio.
#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    tac(args, &mut out, &mut err, &mut input)
}

pub fn uu_app() -> Command {
    Command::new("tac")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("tac"))
        .override_usage(format_usage(&translate!("tac-usage")))
        .about(translate!("tac-about"))
        .infer_long_args(true)
        .arg(
            Arg::new(options::BEFORE)
                .short('b')
                .long(options::BEFORE)
                .help(translate!("tac-help-before"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::REGEX)
                .short('r')
                .long(options::REGEX)
                .help(translate!("tac-help-regex"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::SEPARATOR)
                .short('s')
                .long(options::SEPARATOR)
                .help(translate!("tac-help-separator"))
                .value_parser(clap::value_parser!(OsString))
                .value_name("STRING"),
        )
        .arg(
            Arg::new(options::FILE)
                .hide(true)
                .action(ArgAction::Append)
                .value_parser(clap::value_parser!(OsString))
                .value_hint(clap::ValueHint::FilePath),
        )
}

/// Print lines of a buffer in reverse, with line separator given as a regex.
///
/// `data` contains the bytes of the file.
///
/// `pattern` is the regular expression given as a
/// [`regex::bytes::Regex`] (not a [`regex::Regex`], since the input is
/// given as a slice of bytes). If `before` is `true`, then each match
/// of this pattern in `data` is interpreted as the start of a line. If
/// `before` is `false`, then each match of this pattern is interpreted
/// as the end of a line.
///
/// This function writes each line in `data` to `out` in reverse.
///
/// # Errors
///
/// If there is a problem writing to `out`, then this function
/// returns [`std::io::Error`].
fn buffer_tac_regex(
    data: &[u8],
    pattern: &regex::bytes::Regex,
    before: bool,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    // The index of the line separator for the current line.
    //
    // As we scan through the `data` from right to left, we update this
    // variable each time we find a new line separator. We restrict our
    // regular expression search to only those bytes up to the line
    // separator.
    let mut this_line_end = data.len();

    // The index of the start of the next line in the `data`.
    //
    // As we scan through the `data` from right to left, we update this
    // variable each time we find a new line.
    //
    // If `before` is `true`, then each line starts immediately before
    // the line separator. Otherwise, each line starts immediately after
    // the line separator.
    let mut following_line_start = data.len();

    // Iterate over each byte in the buffer in reverse. When we find a
    // line separator, write the line to the output.
    //
    // The `before` flag controls whether the line separator appears at
    // the end of the line (as in "abc\ndef\n") or at the beginning of
    // the line (as in "/abc/def").
    for i in (0..data.len()).rev() {
        // Determine if there is a match for `pattern` starting at index
        // `i` in `data`. Only search up to the line ending that was
        // found previously.
        if let Some(match_) = pattern.find_at(&data[..this_line_end], i)
            && match_.start() == i
        {
            // Record this index as the ending of the current line.
            this_line_end = i;

            // The length of the match (that is, the line separator), in bytes.
            let slen = match_.end() - match_.start();

            if before {
                out.write_all(&data[i..following_line_start])?;
                following_line_start = i;
            } else {
                out.write_all(&data[i + slen..following_line_start])?;
                following_line_start = i + slen;
            }
        }
    }

    // After the loop terminates, write whatever bytes are remaining at
    // the beginning of the buffer.
    out.write_all(&data[0..following_line_start])?;
    out.flush()?;
    Ok(())
}

/// Write lines from `data` to `out` in reverse.
///
/// This function writes to `out` each line appearing in `data`,
/// starting with the last line and ending with the first line. The
/// `separator` parameter defines what characters to use as a line
/// separator.
///
/// If `before` is `false`, then this function assumes that the
/// `separator` appears at the end of each line, as in `"abc\ndef\n"`.
/// If `before` is `true`, then this function assumes that the
/// `separator` appears at the beginning of each line, as in
/// `"/abc/def"`.
fn buffer_tac(
    data: &[u8],
    before: bool,
    separator: &OsStr,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    // The number of bytes in the line separator.
    let slen = separator.len();

    // The index of the start of the next line in the `data`.
    //
    // As we scan through the `data` from right to left, we update this
    // variable each time we find a new line.
    //
    // If `before` is `true`, then each line starts immediately before
    // the line separator. Otherwise, each line starts immediately after
    // the line separator.
    let mut following_line_start = data.len();

    // Iterate over each byte in the buffer in reverse. When we find a
    // line separator, write the line to the output.
    //
    // The `before` flag controls whether the line separator appears at
    // the end of the line (as in "abc\ndef\n") or at the beginning of
    // the line (as in "/abc/def").
    for i in memmem::rfind_iter(data, separator.as_encoded_bytes()) {
        if before {
            out.write_all(&data[i..following_line_start])?;
            following_line_start = i;
        } else {
            out.write_all(&data[i + slen..following_line_start])?;
            following_line_start = i + slen;
        }
    }

    // After the loop terminates, write whatever bytes are remaining at
    // the beginning of the buffer.
    out.write_all(&data[0..following_line_start])?;
    out.flush()?;
    Ok(())
}

/// Make the regex flavor compatible with `regex` crate
///
/// Concretely:
/// - Toggle escaping of (), |, {}
/// - Escape ^ and $ when not at edges
/// - Leave only ASCII bytes inside []
/// - Escape non-ASCII bytes as `(?-u:\xFF)` outside []
fn translate_regex_flavor(bytes: &[u8]) -> String {
    let mut result = Vec::new();
    let mut i = 0;
    let mut inside_brackets = false;
    let mut prev_was_backslash = false;
    let mut last_byte: Option<u8> = None;

    while let Some(b) = bytes.get(i) {
        let is_escaped = prev_was_backslash;
        prev_was_backslash = false;

        match b {
            _ if inside_brackets && !b.is_ascii() => {
                i += 1;
                continue;
            }
            // Unescape escaped (), |, {} when not inside brackets
            b'\\' if !inside_brackets && !is_escaped => {
                if let Some(next) = bytes.get(i + 1) {
                    if matches!(next, b'(' | b')' | b'|' | b'{' | b'}') {
                        result.push(*next);
                        last_byte = Some(*next);
                        i += 2;
                        continue;
                    }
                }

                result.push(b'\\');
                last_byte = Some(b'\\');
                prev_was_backslash = true;
            }
            // Bracket tracking
            b'[' => {
                inside_brackets = true;
                result.push(*b);
                last_byte = Some(*b);
            }
            b']' => {
                inside_brackets = false;
                result.push(*b);
                last_byte = Some(*b);
            }
            // Escape (), |, {} when not escaped and outside brackets
            b'(' | b')' | b'|' | b'{' | b'}' if !inside_brackets && !is_escaped => {
                result.push(b'\\');
                result.push(*b);
                last_byte = Some(*b);
            }
            b'^' if !inside_brackets && !is_escaped => {
                let is_anchor_position =
                    result.is_empty() || matches!(last_byte, Some(b'(' | b'|'));
                if !is_anchor_position {
                    result.push(b'\\');
                }
                result.push(*b);
                last_byte = Some(*b);
            }
            b'$' if !inside_brackets && !is_escaped => {
                let next_is_anchor_position = match bytes.get(i + 1) {
                    None => true,
                    Some(b')' | b'|') => true,
                    Some(b'\\') => {
                        // Peek two ahead to see if it's \) or \|
                        matches!(bytes.get(i + 2), Some(b')' | b'|'))
                    }
                    _ => false,
                };
                if !next_is_anchor_position {
                    result.push(b'\\');
                }
                result.push(*b);
                last_byte = Some(*b);
            }
            _ if !b.is_ascii() => {
                let _ = write!(result, r"(?-u:\x{b:02x})");
                last_byte = None;
            }
            _ => {
                result.push(*b);
                last_byte = Some(*b);
            }
        }

        i += 1;
    }

    String::from_utf8(result).expect("produces ASCII bytes")
}

#[allow(clippy::cognitive_complexity)]
fn tac_impl(
    filenames: &[OsString],
    before: bool,
    regex: bool,
    separator: &OsStr,
    out: &mut dyn Write,
    err: &mut dyn Write,
    stdin: &mut dyn Read,
) -> UResult<()> {
    // Compile the regular expression pattern if it is provided.
    let maybe_pattern = if regex {
        match regex::bytes::RegexBuilder::new(&translate_regex_flavor(separator.as_encoded_bytes()))
            .multi_line(true)
            .build()
        {
            Ok(p) => Some(p),
            Err(e) => return Err(TacError::InvalidRegex(e).into()),
        }
    } else {
        None
    };

    // Tracks whether any per-file open/read error occurred. Upstream uses
    // `show!` + `set_exit_code(1)` + `continue`; here we route the diagnostic
    // to the logical `err` sink and remember that the final status must be
    // non-zero (GNU exits 1 on any such error, but still processes the rest).
    let mut had_error = false;

    for filename in filenames {
        let buf;

        let data: &[u8] = if filename == "-" {
            // tac buffers the entire input before emitting anything (last line
            // first), so there is no streaming/splice path to preserve: drain
            // the logical stdin reader fully into an owned buffer.
            let mut contents = Vec::new();
            match stdin.read_to_end(&mut contents) {
                Ok(_) => {
                    buf = contents;
                    &buf
                }
                Err(e) => {
                    let msg = TacError::ReadError(OsString::from("stdin"), e);
                    let _ = writeln!(err, "tac: {msg}");
                    had_error = true;
                    continue;
                }
            }
        } else {
            let mut file = match std::fs::File::open(std::path::Path::new(filename)) {
                Ok(f) => f,
                Err(e) => {
                    let msg = TacError::OpenError(filename.clone(), e);
                    let _ = writeln!(err, "tac: {msg}");
                    had_error = true;
                    continue;
                }
            };

            let mut contents = Vec::new();
            match file.read_to_end(&mut contents) {
                Ok(_) => {
                    buf = contents;
                    &buf
                }
                Err(e) => {
                    let msg = TacError::ReadError(filename.clone(), e);
                    let _ = writeln!(err, "tac: {msg}");
                    had_error = true;
                    continue;
                }
            }
        };

        // Select the appropriate `tac` algorithm based on whether the
        // separator is given as a regular expression or a fixed string.
        let result = match maybe_pattern {
            Some(ref pattern) => buffer_tac_regex(data, pattern, before, out),
            None => buffer_tac(data, before, separator, out),
        };

        // If there is any error in writing the output, terminate immediately.
        if let Err(e) = result {
            return Err(TacError::WriteError(e).into());
        }
    }

    if had_error {
        // The diagnostics were already emitted to `err`; surface GNU's exit
        // status (1) without printing anything further. An empty-message
        // USimpleError sets the code and prints nothing.
        Err(uucore::error::USimpleError::new(1, String::new()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests_hybrid_flavor {
    use super::translate_regex_flavor;

    #[test]
    fn test_grouping_and_alternation() {
        assert_eq!(translate_regex_flavor(br"\(abc\)"), r"(abc)");

        assert_eq!(translate_regex_flavor(br"(abc)"), r"\(abc\)");

        assert_eq!(translate_regex_flavor(br"a\|b"), r"a|b");

        assert_eq!(translate_regex_flavor(br"a|b"), r"a\|b");
    }

    #[test]
    fn test_quantifiers() {
        assert_eq!(translate_regex_flavor(b"a+"), "a+");

        assert_eq!(translate_regex_flavor(b"a*"), "a*");

        assert_eq!(translate_regex_flavor(b"a?"), "a?");

        assert_eq!(translate_regex_flavor(br"a\+"), r"a\+");

        assert_eq!(translate_regex_flavor(br"a\*"), r"a\*");

        assert_eq!(translate_regex_flavor(br"a\?"), r"a\?");
    }

    #[test]
    fn test_intervals() {
        assert_eq!(translate_regex_flavor(br"a\{1,3\}"), r"a{1,3}");

        assert_eq!(translate_regex_flavor(br"a{1,3}"), r"a\{1,3\}");
    }

    #[test]
    fn test_anchors_context() {
        assert_eq!(translate_regex_flavor(br"^abc$"), r"^abc$");

        assert_eq!(translate_regex_flavor(br"a^b"), r"a\^b");
        assert_eq!(translate_regex_flavor(br"a$b"), r"a\$b");

        // Anchors inside groups (reset by \(...\) regardless of position)
        assert_eq!(translate_regex_flavor(br"\(^abc\)"), r"(^abc)");
        assert_eq!(translate_regex_flavor(br"z\(^abc\)"), r"z(^abc)");
        assert_eq!(translate_regex_flavor(br"\(abc$\)"), r"(abc$)");
        assert_eq!(translate_regex_flavor(br"\(abc$\)z"), r"(abc$)z");

        // Anchors inside alternation (reset by \| regardless of position)
        assert_eq!(translate_regex_flavor(br"^a\|^b"), r"^a|^b");
        assert_eq!(translate_regex_flavor(br"x\|^b"), r"x|^b");
        assert_eq!(translate_regex_flavor(br"a$\|b$"), r"a$|b$");
    }

    #[test]
    fn test_character_classes() {
        assert_eq!(translate_regex_flavor(br"[a-z]"), r"[a-z]");

        assert_eq!(translate_regex_flavor(br"[.]"), r"[.]");
        assert_eq!(translate_regex_flavor(br"[+]"), r"[+]");

        assert_eq!(translate_regex_flavor(br"[]abc]"), r"[]abc]");

        assert_eq!(translate_regex_flavor(br"[^]abc]"), r"[^]abc]");
    }

    #[test]
    fn test_complex_strings() {
        assert_eq!(translate_regex_flavor(br"(\d+)[+*]"), r"\(\d+\)[+*]");

        assert_eq!(translate_regex_flavor(br"\(\d+\)\{2\}"), r"(\d+){2}");
    }

    #[test]
    fn test_edge_cases() {
        assert_eq!(translate_regex_flavor(br"abc\"), r"abc\");

        assert_eq!(translate_regex_flavor(br"\\"), r"\\");

        assert_eq!(translate_regex_flavor(br"\^"), r"\^");
    }

    // ── injected-I/O smoke tests (the in-process brush builtin path) ──
    // Drive the public `tac(...)` entry with in-memory Vec<u8> out/err and a
    // `&[u8]` stdin, asserting reversed bytes — no process fd is touched.
    use super::tac;

    fn run(args: &[&str], input: &[u8]) -> (Vec<u8>, Vec<u8>, i32) {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let mut stdin: &[u8] = input;
        let argv = args.iter().map(|s| std::ffi::OsString::from(*s));
        let code = match tac(argv, &mut out, &mut err, &mut stdin) {
            Ok(()) => 0,
            Err(e) => e.code(),
        };
        (out, err, code)
    }

    #[test]
    fn test_inmem_basic_reverse() {
        let (out, err, code) = run(&["tac"], b"a\nb\nc\n");
        assert_eq!(out, b"c\nb\na\n");
        assert!(err.is_empty());
        assert_eq!(code, 0);
    }

    #[test]
    fn test_inmem_no_trailing_newline() {
        let (out, _err, code) = run(&["tac"], b"a\nb\nc");
        assert_eq!(out, b"cb\na\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_inmem_empty_input() {
        let (out, err, code) = run(&["tac"], b"");
        assert!(out.is_empty());
        assert!(err.is_empty());
        assert_eq!(code, 0);
    }

    #[test]
    fn test_inmem_separator() {
        let (out, _err, code) = run(&["tac", "-s", ":"], b"a:b:c:");
        assert_eq!(out, b"c:b:a:");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_inmem_before() {
        let (out, _err, code) = run(&["tac", "-b", "-s", ":"], b":a:b:c");
        assert_eq!(out, b":c:b:a");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_inmem_regex_separator() {
        // Reverse on a run of digits as the separator (matches GNU
        // `printf 'a1b22c' | tac -r -s '[0-9]+'` => "c2b2a1").
        let (out, _err, code) = run(&["tac", "-r", "-s", "[0-9]+"], b"a1b22c");
        assert_eq!(out, b"c2b2a1");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_inmem_binary_input() {
        // NUL-separated reverse via empty separator (maps to \0).
        let (out, _err, code) = run(&["tac", "-s", ""], b"a\0b\0c\0");
        assert_eq!(out, b"c\0b\0a\0");
        assert_eq!(code, 0);
    }
}
