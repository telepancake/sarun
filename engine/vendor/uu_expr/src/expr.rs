// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use clap::{Arg, ArgAction, Command};
use std::io::Write;
use syntax_tree::{AstNode, is_truthy};
use thiserror::Error;
use uucore::os_string_to_vec;
use uucore::translate;
use uucore::{
    display::Quotable,
    error::{UError, UResult, USimpleError},
    format_usage,
};

mod locale_aware;
mod syntax_tree;

mod options {
    pub const VERSION: &str = "version";
    pub const HELP: &str = "help";
    pub const EXPRESSION: &str = "expression";
}

pub type ExprResult<T> = Result<T, ExprError>;

#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum ExprError {
    #[error("{}", translate!("expr-error-unexpected-argument", "arg" => _0.quote()))]
    UnexpectedArgument(String),
    #[error("{}", translate!("expr-error-missing-argument", "arg" => _0.quote()))]
    MissingArgument(String),
    #[error("{}", translate!("expr-error-non-integer-argument"))]
    NonIntegerArgument,
    #[error("{}", translate!("expr-error-missing-operand"))]
    MissingOperand,
    #[error("{}", translate!("expr-error-division-by-zero"))]
    DivisionByZero,
    #[error("{}", translate!("expr-error-invalid-regex-expression"))]
    InvalidRegexExpression,
    #[error("{}", translate!("expr-error-expected-closing-brace-after", "arg" => _0.quote()))]
    ExpectedClosingBraceAfter(String),
    #[error("{}", translate!("expr-error-expected-closing-brace-instead-of", "arg" => _0.quote()))]
    ExpectedClosingBraceInsteadOf(String),
    #[error("{}", translate!("expr-error-unmatched-opening-parenthesis"))]
    UnmatchedOpeningParenthesis,
    #[error("{}", translate!("expr-error-unmatched-closing-parenthesis"))]
    UnmatchedClosingParenthesis,
    #[error("{}", translate!("expr-error-unmatched-opening-brace"))]
    UnmatchedOpeningBrace,
    #[error("{}", translate!("expr-error-invalid-bracket-content"))]
    InvalidBracketContent,
    #[error("{}", translate!("expr-error-trailing-backslash"))]
    TrailingBackslash,
    #[error("{}", translate!("expr-error-too-big-range-quantifier-index"))]
    TooBigRangeQuantifierIndex,
    #[error("{}", translate!("expr-error-match-utf8", "arg" => _0.quote()))]
    UnsupportedNonUtf8Match(String),
}

impl UError for ExprError {
    fn code(&self) -> i32 {
        2
    }

    fn usage(&self) -> bool {
        *self == Self::MissingOperand
    }
}

pub fn uu_app() -> Command {
    Command::new("expr")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("expr"))
        .about(translate!("expr-about"))
        .override_usage(format_usage(&translate!("expr-usage")))
        .after_help(translate!("expr-after-help"))
        .infer_long_args(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(
            Arg::new(options::VERSION)
                .long(options::VERSION)
                .help(translate!("expr-help-version"))
                .action(ArgAction::Version),
        )
        .arg(
            Arg::new(options::HELP)
                .long(options::HELP)
                .help(translate!("expr-help-help"))
                .action(ArgAction::Help),
        )
        .arg(
            Arg::new(options::EXPRESSION)
                .action(ArgAction::Append)
                .allow_hyphen_values(true),
        )
}

/// Injected-I/O logical entry for the in-process brush builtin.
///
/// This is the genuine port of `expr`: the evaluated result is written to the
/// caller-supplied `out` sink and every diagnostic to `err`; NO descriptor of
/// the calling process (fd 0/1/2) is touched, and there is no `print!`,
/// `io::stdout`, `process::exit`, or `set_exit_code`.
///
/// ## Exit codes (GNU-faithful, surfaced via the returned `UResult`)
/// expr's exit status is load-bearing, so it is carried on the error arm and
/// the caller (brush.rs) reads it through `UError::code()`:
///
/// * `Ok(())`   — the result is neither null nor `0`  → status **0**.
/// * `Err(1)`   — the result IS null or `0` (false). The result has already
///   been written to `out`; the error carries an EMPTY message so the caller
///   prints nothing extra. (`USimpleError::new(1, "")`.)
/// * `Err(2)`   — an invalid expression: any `ExprError` propagates with its
///   own `code() == 2` and a real diagnostic message for `err`.
/// * `Err(3)`   — reserved for an internal error. uutils expr has no distinct
///   internal-error path in 0.8.0 (every evaluation failure is an `ExprError`,
///   i.e. code 2), so this code is never produced here; it is documented so the
///   contract matches GNU and a future internal-failure path can return it.
///
/// Upstream produced the 0/1 distinction with `return Err(1.into())` (an
/// `ExitCode` wrapper that printed nothing) and the result with
/// `stdout().write_all`. We keep the SAME 0/1/2 semantics but drive them
/// through `out`/`err` and `USimpleError`, with no global I/O.
pub fn expr(
    args: impl uucore::Args,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> UResult<()> {
    // For expr utility we do not want getopts.
    // The following usage should work without escaping hyphens: `expr -15 = 1 + 2 \* \( 3 - -4 \)`
    let args = args
        .skip(1) // Skip binary name
        .map(os_string_to_vec)
        .collect::<Result<Vec<_>, _>>()?;

    if args.len() == 1 && args[0] == b"--help" {
        // Render help to the LOGICAL sink instead of the process stdout. (The
        // brush gate keeps --help/--version on the host binary, so this path is
        // only reached by the standalone bridge below.)
        let help = uu_app().render_help();
        out.write_all(help.to_string().as_bytes())?;
    } else if args.len() == 1 && args[0] == b"--version" {
        writeln!(out, "expr {}", uucore::crate_version!())?;
    } else {
        // The first argument may be "--" and should be be ignored.
        let args = if !args.is_empty() && args[0] == b"--" {
            &args[1..]
        } else {
            &args
        };

        // Parse/eval errors are ExprError → UError::code() == 2 (invalid
        // expression); they carry a real message that the caller emits on err.
        let res = AstNode::parse(args)?.eval()?.eval_as_string();
        // Result → the LOGICAL stdout (was stdout().write_all upstream). expr's
        // output may be non-UTF-8 bytes, so write the raw Vec<u8> verbatim.
        out.write_all(&res)?;
        out.write_all(b"\n")?;

        if !is_truthy(&res.into()) {
            // The result is null or "0": GNU expr exits 1. The value was
            // already written; carry an EMPTY-message error so the caller adds
            // no diagnostic (mirrors upstream's silent `1.into()` ExitCode).
            let _ = err; // err unused on this path; kept for a uniform signature
            return Err(USimpleError::new(1, String::new()));
        }
    }

    Ok(())
}

#[uucore::main(no_signals)]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    // Thin descriptor-based bridge for the standalone binary and the
    // --invoke-bundled subprocess dispatcher, where THIS process's own fd 1/2
    // are the intended sinks. The exit-code logic lives entirely in expr();
    // we forward its UResult unchanged so the 0/1/2 status is preserved.
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let r = expr(args, &mut out, &mut err);
    let _ = out.flush();
    r
}

#[cfg(test)]
mod injected_io_tests {
    use super::expr;
    use std::ffi::OsString;

    /// Drive expr() with in-memory Vec<u8> sinks; return (stdout, stderr, code,
    /// err_message). `code`/`err_message` mirror what brush.rs derives from the
    /// returned UResult: Ok => (0, ""), Err => (e.code(), e.to_string()). The
    /// diagnostic text lives in the error's Display (the caller emits it on the
    /// logical stderr, exactly like head/cat/nl) — expr() never writes it to
    /// `err` itself, so the `err` sink stays empty on the error path.
    fn run(words: &[&str]) -> (Vec<u8>, Vec<u8>, i32, String) {
        // argv[0] is the program name (expr() does .skip(1)).
        let mut argv: Vec<OsString> = vec![OsString::from("expr")];
        argv.extend(words.iter().map(OsString::from));
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let (code, msg) = match expr(argv.into_iter(), &mut out, &mut err) {
            Ok(()) => (0, String::new()),
            Err(e) => (e.code(), e.to_string()),
        };
        (out, err, code, msg)
    }

    #[test]
    fn truthy_result_exits_0() {
        let (out, err, code, _m) = run(&["5"]);
        assert_eq!(out, b"5\n");
        assert!(err.is_empty());
        assert_eq!(code, 0);
    }

    #[test]
    fn zero_result_exits_1_but_still_prints() {
        let (out, err, code, msg) = run(&["0"]);
        assert_eq!(out, b"0\n", "the value is still written before the false status");
        assert!(err.is_empty(), "expr() never writes to the err sink itself");
        assert!(msg.is_empty(), "the false path carries an EMPTY message");
        assert_eq!(code, 1);
    }

    #[test]
    fn empty_string_result_exits_1() {
        let (out, _err, code, _m) = run(&[""]);
        assert_eq!(out, b"\n");
        assert_eq!(code, 1);
    }

    #[test]
    fn comparison_false_exits_1() {
        let (out, _err, code, _m) = run(&["1", "=", "2"]);
        assert_eq!(out, b"0\n");
        assert_eq!(code, 1);
    }

    #[test]
    fn comparison_true_exits_0() {
        let (out, _err, code, _m) = run(&["1", "=", "1"]);
        assert_eq!(out, b"1\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn arithmetic_precedence() {
        let (out, _err, code, _m) = run(&["2", "+", "3", "*", "4"]);
        assert_eq!(out, b"14\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn invalid_expression_exits_2_with_message() {
        let (out, err, code, msg) = run(&["1", "+"]);
        assert!(out.is_empty(), "nothing written on the error path");
        assert!(err.is_empty(), "expr() routes the diagnostic via the error, not the sink");
        assert!(!msg.is_empty(), "the ExprError Display carries the diagnostic");
        assert_eq!(code, 2, "invalid expression => exit 2");
    }

    #[test]
    fn bignum_result() {
        let (out, _err, code, _m) =
            run(&["99999999999999999999999999", "+", "1"]);
        assert_eq!(out, b"100000000000000000000000000\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn string_length_substr_index() {
        assert_eq!(run(&["length", "abcdef"]).0, b"6\n");
        assert_eq!(run(&["substr", "abcdef", "2", "3"]).0, b"bcd\n");
        assert_eq!(run(&["index", "abcdef", "cd"]).0, b"3\n");
    }

    #[test]
    fn colon_regex_capture_and_count() {
        // capture group => the matched text; no group => match length
        assert_eq!(run(&["abcdef", ":", "a\\(bc\\)"]).0, b"bc\n");
        assert_eq!(run(&["abcdef", ":", "abc"]).0, b"3\n");
        // a non-matching regex yields "" / 0 => exit 1
        let (out, _e, code, _m) = run(&["abcdef", ":", "z"]);
        assert_eq!(out, b"0\n");
        assert_eq!(code, 1);
    }
}
