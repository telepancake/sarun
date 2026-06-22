// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

mod operation;
mod simd;
mod unicode_table;

use clap::{Arg, ArgAction, Command, value_parser};
use operation::{
    DeleteOperation, Sequence, SqueezeOperation, SymbolTranslator, TranslateOperation,
    flush_output, translate_input,
};
use simd::process_input;
use std::ffi::OsString;
use std::io::{BufReader, Read, Write};
use uucore::display::Quotable;
use uucore::error::{UResult, UUsageError};
use uucore::translate;
use uucore::{format_usage, os_str_as_bytes};

mod options {
    pub const COMPLEMENT: &str = "complement";
    pub const DELETE: &str = "delete";
    pub const SQUEEZE: &str = "squeeze-repeats";
    pub const TRUNCATE_SET1: &str = "truncate-set1";
    pub const SETS: &str = "sets";
}

/// Drain any parse-time warnings collected during `Sequence::from_str`/
/// `solve_set_characters` to the logical `err` sink, shaped exactly like
/// upstream's `show_warning!` (`<name>: warning: <body>`). Upstream wrote these
/// to the process's real fd 2; the in-process builtin must use the box's
/// logical stderr instead. `name` is `uucore::util_name()` so the prefix matches
/// the standalone binary verbatim.
///
/// Always drains (even on the success path) so a warning never leaks into a
/// later invocation on the same thread.
fn drain_parse_warnings(err: &mut dyn Write) {
    let name = uucore::util_name();
    operation::PARSE_WARNINGS.with(|w| {
        for body in w.borrow_mut().drain(..) {
            let _ = writeln!(err, "{name}: warning: {body}");
        }
    });
}

/// Logical entry point for the bundled `tr` builtin.
///
/// Unlike upstream's `uumain`, this never touches the process's own stdio: it
/// translates the logical input source (`stdin`) onto the shell's logical output
/// sink (`out`) and routes its own diagnostics — both the clap/usage errors it
/// RETURNS and the parse-time warnings it collects — to the logical error sink
/// (`err`). `tr` is a pure stdin→stdout byte filter with no file operands and no
/// `splice`/`copy_file_range`/seek fast path, so it needs NO backing raw
/// descriptor (the `out_fd`/`stdin_fd` that `cat`/`head` carry are deliberately
/// omitted — there is no fd path to feed them to). The `is_stdin_directory`
/// check upstream did on the real fd 0 is dropped here: a logical reader has no
/// stable descriptor to fstat, and an in-process pipeline never hands `tr` a
/// directory as its stdin. It holds no process-global state — the trailing-
/// backslash warning that upstream emitted via `show!` (process fd 2 +
/// `set_exit_code`) is written to `err` directly, and parse warnings come from a
/// per-thread buffer drained here — so a pipeline stage can run it concurrently.
///
/// All of the SET solving and the translate/delete/squeeze operations
/// (`operation.rs`, `simd.rs`) are byte-for-byte upstream; only the I/O endpoints
/// and the diagnostic destinations are adapted.
pub fn tr(
    args: impl uucore::Args,
    out: &mut dyn Write,
    err: &mut dyn Write,
    stdin: &mut dyn Read,
) -> UResult<()> {
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let delete_flag = matches.get_flag(options::DELETE);
    let complement_flag = matches.get_flag(options::COMPLEMENT);
    let squeeze_flag = matches.get_flag(options::SQUEEZE);
    let truncate_set1_flag = matches.get_flag(options::TRUNCATE_SET1);

    // Ultimately this should be OsString, but we might want to wait for the
    // pattern API on OsStr
    let sets: Vec<_> = matches
        .get_many::<OsString>(options::SETS)
        .into_iter()
        .flatten()
        .map(ToOwned::to_owned)
        .collect();

    if sets.is_empty() {
        return Err(UUsageError::new(1, translate!("tr-error-missing-operand")));
    }

    let sets_len = sets.len();

    if !(delete_flag || squeeze_flag) && sets_len == 1 {
        return Err(UUsageError::new(
            1,
            translate!("tr-error-missing-operand-translating", "set" => sets[0].quote()),
        ));
    }

    if delete_flag && squeeze_flag && sets_len == 1 {
        return Err(UUsageError::new(
            1,
            translate!("tr-error-missing-operand-deleting-squeezing", "set" => sets[0].quote()),
        ));
    }

    if sets_len > 1 {
        if delete_flag && !squeeze_flag {
            let op = sets[1].quote();
            let msg = if sets_len == 2 {
                translate!("tr-error-extra-operand-deleting-without-squeezing", "operand" => op)
            } else {
                translate!("tr-error-extra-operand-simple", "operand" => op)
            };
            return Err(UUsageError::new(1, msg));
        }
        if sets_len > 2 {
            let op = sets[2].quote();
            let msg = translate!("tr-error-extra-operand-simple", "operand" => op);
            return Err(UUsageError::new(1, msg));
        }
    }

    if let Some(first) = sets.first() {
        let slice = os_str_as_bytes(first)?;
        let trailing_backslashes = slice.iter().rev().take_while(|&&c| c == b'\\').count();
        if trailing_backslashes % 2 == 1 {
            // The trailing backslash has a non-backslash character before it.
            //
            // sarun: upstream did `show!(USimpleError::new(0, …))`, which writes
            // the process's fd 2 AND calls `set_exit_code(0)` (a uucore global).
            // Both are process-global and forbidden in-process. The code is 0, so
            // there is nothing to remember for the exit status. `show!` shapes the
            // line as `<name>: <message>` (NOT `<name>: warning: <message>` — the
            // `tr-warning-unescaped-backslash` fluent string ALREADY begins with
            // "warning: "), so we reproduce exactly that here, to the LOGICAL
            // `err` sink instead of the process global.
            let _ = writeln!(
                err,
                "{}: {}",
                uucore::util_name(),
                translate!("tr-warning-unescaped-backslash")
            );
        }
    }

    // sarun: read the logical input through a `BufReader` (the operations want a
    // `BufRead`), and write to the logical `out` sink directly. Upstream locked
    // the process's stdin/stdout here; we take the injected trait objects.
    let mut buffered_stdin = BufReader::new(stdin);

    // According to the man page: translating only happens if deleting or if a second set is given
    let translating = !delete_flag && sets.len() > 1;
    let mut sets_iter = sets.iter().map(OsString::as_os_str);
    let set_result = Sequence::solve_set_characters(
        os_str_as_bytes(sets_iter.next().unwrap_or_default())?,
        os_str_as_bytes(sets_iter.next().unwrap_or_default())?,
        complement_flag,
        // if we are not translating then we don't truncate set1
        truncate_set1_flag && translating,
        translating,
    );

    // sarun: drain SET-parse warnings (e.g. ambiguous octal escape) to the
    // logical `err` sink BEFORE handling a parse error, so warnings never leak
    // into a later same-thread invocation, regardless of whether solving
    // succeeded or failed.
    drain_parse_warnings(err);

    let (set1, set2) = set_result?;

    // sarun: upstream's `is_stdin_directory(&stdin)` check is omitted — a logical
    // reader has no stable fd to fstat, and an in-process pipeline never feeds a
    // directory in as `tr`'s stdin.

    // '*_op' are the operations that need to be applied, in order.
    if delete_flag {
        if squeeze_flag {
            let delete_op = DeleteOperation::new(set1);
            let squeeze_op = SqueezeOperation::new(set2);
            let op = delete_op.chain(squeeze_op);
            translate_input(&mut buffered_stdin, out, op)?;
        } else {
            let op = DeleteOperation::new(set1);
            process_input(&mut buffered_stdin, out, &op)?;
        }
    } else if squeeze_flag {
        if sets_len == 1 {
            let op = SqueezeOperation::new(set1);
            translate_input(&mut buffered_stdin, out, op)?;
        } else {
            let translate_op = TranslateOperation::new(set1, set2.clone())?;
            let squeeze_op = SqueezeOperation::new(set2);
            let op = translate_op.chain(squeeze_op);
            translate_input(&mut buffered_stdin, out, op)?;
        }
    } else {
        let op = TranslateOperation::new(set1, set2)?;
        process_input(&mut buffered_stdin, out, &op)?;
    }

    flush_output(out)?;

    Ok(())
}

/// Descriptor-based entry point for the standalone `tr` binary and the
/// `--invoke-bundled` subprocess dispatcher.
///
/// In those contexts the process's own fd 0/1/2 *are* the intended I/O, so this
/// is a thin bridge that locks them and hands them to [`tr`]. The in-process
/// brush builtin must NOT route through here — it calls [`tr`] directly with the
/// shell's logical sink/source, so it never touches process-global stdio.
#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    let result = tr(args, &mut out, &mut err, &mut input);
    // GNU `tr` flushes stdout before exiting; `tr()` already flushed its sink,
    // but the locked Stdout is line-buffered, so flush again to be safe.
    let _ = out.flush();
    result
}

pub fn uu_app() -> Command {
    Command::new("tr")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("tr"))
        .about(translate!("tr-about"))
        .override_usage(format_usage(&translate!("tr-usage")))
        .after_help(translate!("tr-after-help"))
        .infer_long_args(true)
        .trailing_var_arg(true)
        .arg(
            Arg::new(options::COMPLEMENT)
                .visible_short_alias('C')
                .short('c')
                .long(options::COMPLEMENT)
                .help(translate!("tr-help-complement"))
                .action(ArgAction::SetTrue)
                .overrides_with(options::COMPLEMENT),
        )
        .arg(
            Arg::new(options::DELETE)
                .short('d')
                .long(options::DELETE)
                .help(translate!("tr-help-delete"))
                .action(ArgAction::SetTrue)
                .overrides_with(options::DELETE),
        )
        .arg(
            Arg::new(options::SQUEEZE)
                .long(options::SQUEEZE)
                .short('s')
                .help(translate!("tr-help-squeeze"))
                .action(ArgAction::SetTrue)
                .overrides_with(options::SQUEEZE),
        )
        .arg(
            Arg::new(options::TRUNCATE_SET1)
                .long(options::TRUNCATE_SET1)
                .short('t')
                .help(translate!("tr-help-truncate-set1"))
                .action(ArgAction::SetTrue)
                .overrides_with(options::TRUNCATE_SET1),
        )
        .arg(
            Arg::new(options::SETS)
                .num_args(1..)
                .value_parser(value_parser!(OsString)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// Run the injected-I/O `tr` entry with an in-memory `&[u8]` stdin and
    /// `Vec<u8>` out/err sinks — the exact shape a pipeline hands when `tr` is
    /// fed by another in-process builtin and its output is captured to memory.
    /// No process fd 0/1/2 is touched. Returns (stdout, stderr, exit_code).
    fn run(argv: &[&str], input: &[u8]) -> (Vec<u8>, Vec<u8>, i32) {
        let mut src: &[u8] = input;
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let args =
            std::iter::once(OsString::from("tr")).chain(argv.iter().map(OsString::from));
        let code = match tr(args, &mut out, &mut err, &mut src) {
            Ok(()) => 0,
            Err(e) => {
                use uucore::error::UError;
                let msg = e.to_string();
                if !msg.is_empty() {
                    let _ = writeln!(err, "tr: {msg}");
                }
                e.code()
            }
        };
        (out, err, code)
    }

    #[test]
    fn test_tr_translate_lower_upper() {
        let (out, err, code) = run(&["a-z", "A-Z"], b"Hello, World!\n");
        assert_eq!(out, b"HELLO, WORLD!\n");
        assert!(err.is_empty(), "stderr: {err:?}");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_delete_digits() {
        let (out, _err, code) = run(&["-d", "0-9"], b"a1b2c3\n");
        assert_eq!(out, b"abc\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_squeeze_spaces() {
        let (out, _err, code) = run(&["-s", " "], b"a    b   c\n");
        assert_eq!(out, b"a b c\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_complement_delete() {
        // delete everything that is NOT a digit
        let (out, _err, code) = run(&["-cd", "0-9"], b"a1b2c3\n");
        assert_eq!(out, b"123");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_class_upper() {
        let (out, _err, code) = run(&["[:lower:]", "[:upper:]"], b"abcXYZ\n");
        assert_eq!(out, b"ABCXYZ\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_char_repeat_pads_set2() {
        // [x*3] expands to "xxx"; set1 "abc" -> set2 "xxx"
        let (out, _err, code) = run(&["abc", "[x*3]"], b"abcd\n");
        assert_eq!(out, b"xxxd\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_escapes() {
        // translate tab -> newline
        let (out, _err, code) = run(&["\\t", "\\n"], b"a\tb\tc");
        assert_eq!(out, b"a\nb\nc");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_set1_longer_than_set2_pads_with_last() {
        // GNU pads set2 with its last char: a->x, b->y, c->y, d->y
        let (out, _err, code) = run(&["abcd", "xy"], b"abcd\n");
        assert_eq!(out, b"xyyy\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_truncate_set1() {
        // -t truncates set1 to set2 length: only a->x and b->y apply
        let (out, _err, code) = run(&["-t", "abcd", "xy"], b"abcd\n");
        assert_eq!(out, b"xycd\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_empty_input() {
        let (out, err, code) = run(&["a-z", "A-Z"], b"");
        assert!(out.is_empty());
        assert!(err.is_empty());
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_no_trailing_newline() {
        let (out, _err, code) = run(&["a", "b"], b"aaa");
        assert_eq!(out, b"bbb");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_nul_input() {
        let (out, _err, code) = run(&["\\0", "X"], b"a\0b\0c");
        assert_eq!(out, b"aXbXc");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_tr_missing_operand_is_error() {
        let (out, _err, code) = run(&[], b"x");
        assert!(out.is_empty());
        assert_ne!(code, 0);
    }

    #[test]
    fn test_tr_trailing_backslash_warning_to_err_not_out() {
        // A single trailing backslash with a non-backslash before it warns.
        let (out, err, code) = run(&["a\\", "b"], b"aa");
        // Output is unaffected (a->b), exit 0.
        assert_eq!(out, b"bb");
        assert_eq!(code, 0);
        // The warning landed on the LOGICAL err sink, never stdout.
        let err_s = String::from_utf8_lossy(&err);
        assert!(
            err_s.contains("warning") && err_s.contains("backslash"),
            "expected backslash warning on err, got: {err_s:?}"
        );
    }
}
