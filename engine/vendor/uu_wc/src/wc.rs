// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// cSpell:ignore ilog wc wc's

mod count_fast;
mod countable;
mod utf8;
mod word_count;

use std::{
    borrow::{Borrow, Cow},
    cmp::max,
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read, Write},
    iter,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::os::fd::BorrowedFd;

use clap::{Arg, ArgAction, ArgMatches, Command, builder::ValueParser};
use thiserror::Error;
use unicode_width::UnicodeWidthChar;
use utf8::{BufReadDecoder, BufReadDecoderError};
use uucore::{display::Quotable, translate};

use uucore::{
    error::{FromIo, UError, UResult},
    format_usage,
    hardware::{HardwareFeature, HasHardwareFeatures as _, SimdPolicy},
    parser::shortcut_value_parser::ShortcutValueParser,
    quoting_style::{self, QuotingStyle},
};

use crate::{
    count_fast::{count_bytes_chars_and_lines_fast, count_bytes_fast},
    countable::WordCountable,
    word_count::WordCount,
};

/// The minimum character width for formatting counts when reading from stdin.
const MINIMUM_WIDTH: usize = 7;

struct Settings<'a> {
    show_bytes: bool,
    show_chars: bool,
    show_lines: bool,
    show_words: bool,
    show_max_line_length: bool,
    debug: bool,
    files0_from: Option<Input<'a>>,
    total_when: TotalWhen,
}

impl Default for Settings<'_> {
    fn default() -> Self {
        // Defaults if none of -c, -m, -l, -w, nor -L are specified.
        Self {
            show_bytes: true,
            show_chars: false,
            show_lines: true,
            show_words: true,
            show_max_line_length: false,
            debug: false,
            files0_from: None,
            total_when: TotalWhen::default(),
        }
    }
}

impl<'a> Settings<'a> {
    fn new(matches: &'a ArgMatches) -> Self {
        let files0_from = matches
            .get_one::<OsString>(options::FILES0_FROM)
            .map(Into::into);

        let total_when = matches
            .get_one::<String>(options::TOTAL)
            .map(Into::into)
            .unwrap_or_default();

        let settings = Self {
            show_bytes: matches.get_flag(options::BYTES),
            show_chars: matches.get_flag(options::CHAR),
            show_lines: matches.get_flag(options::LINES),
            show_words: matches.get_flag(options::WORDS),
            show_max_line_length: matches.get_flag(options::MAX_LINE_LENGTH),
            debug: matches.get_flag(options::DEBUG),
            files0_from,
            total_when,
        };

        if settings.number_enabled() > 0 {
            settings
        } else {
            Self {
                files0_from: settings.files0_from,
                total_when,
                debug: settings.debug,
                ..Default::default()
            }
        }
    }

    fn number_enabled(&self) -> u32 {
        [
            self.show_bytes,
            self.show_chars,
            self.show_lines,
            self.show_max_line_length,
            self.show_words,
        ]
        .into_iter()
        .map(Into::<u32>::into)
        .sum()
    }
}

mod options {
    pub static BYTES: &str = "bytes";
    pub static CHAR: &str = "chars";
    pub static FILES0_FROM: &str = "files0-from";
    pub static LINES: &str = "lines";
    pub static MAX_LINE_LENGTH: &str = "max-line-length";
    pub static TOTAL: &str = "total";
    pub static WORDS: &str = "words";
    pub static DEBUG: &str = "debug";
}
static ARG_FILES: &str = "files";
static STDIN_REPR: &str = "-";

/// Supported inputs.
#[derive(Debug)]
enum Inputs<'a> {
    /// Default Standard input, i.e. no arguments.
    Stdin,
    /// Files; "-" means stdin, possibly multiple times!
    Paths(Vec<Input<'a>>),
    /// --files0-from; "-" means stdin.
    Files0From(Input<'a>),
}

impl<'a> Inputs<'a> {
    fn new(matches: &'a ArgMatches) -> UResult<Self> {
        let arg_files = matches.get_many::<OsString>(ARG_FILES);
        let files0_from = matches.get_one::<OsString>(options::FILES0_FROM);

        match (arg_files, files0_from) {
            (None, None) => Ok(Self::Stdin),
            (Some(files), None) => Ok(Self::Paths(files.map(Into::into).collect())),
            (None, Some(path)) => {
                // If path is a file, and the file isn't too large, we'll load it ahead
                // of time. Every path within the file will have its length checked to
                // hopefully better align the output columns.
                let input = Input::from(path);
                match input.try_as_files0()? {
                    Some(paths) => Ok(Self::Paths(paths)),
                    None => Ok(Self::Files0From(input)),
                }
            }
            (Some(mut files), Some(_)) => {
                Err(WcError::files_disabled(files.next().unwrap()).into())
            }
        }
    }

    /// Creates an iterator which yields values borrowed from the command line arguments.
    /// Returns an error if the file specified in --files0-from cannot be opened.
    fn try_iter(
        &'a self,
        settings: &'a Settings<'a>,
    ) -> UResult<impl Iterator<Item = InputIterItem<'a>>> {
        let base: Box<dyn Iterator<Item = _>> = match self {
            Self::Stdin => Box::new(iter::once(Ok(Input::Stdin(StdinKind::Implicit)))),
            Self::Paths(inputs) => Box::new(inputs.iter().map(|i| Ok(i.as_borrowed()))),
            Self::Files0From(input) => match input {
                Input::Path(path) => Box::new(files0_iter_file(path)?),
                Input::Stdin(_) => Box::new(files0_iter_stdin()),
            },
        };

        // The 1-based index of each yielded item must be tracked for error reporting.
        let mut with_idx = base.enumerate().map(|(i, v)| (i + 1, v));
        let files0_from_path = settings.files0_from.as_ref().map(Input::as_borrowed);

        let iter = iter::from_fn(move || {
            let (idx, next) = with_idx.next()?;
            match next {
                // filter zero length file names...
                Ok(Input::Path(p)) if p.as_os_str().is_empty() => Some(Err({
                    let maybe_ctx = files0_from_path.as_ref().map(|p| (p, idx));
                    WcError::zero_len(maybe_ctx).into()
                })),
                _ => Some(next),
            }
        });
        Ok(iter)
    }
}

#[derive(Clone, Copy, Debug)]
enum StdinKind {
    /// Specified on command-line with "-" ([`STDIN_REPR`])
    Explicit,
    /// Implied by the lack of any arguments
    Implicit,
}

/// Represents a single input, either to be counted or processed for other files names via
/// --files0-from.
#[derive(Debug)]
enum Input<'a> {
    Path(Cow<'a, Path>),
    Stdin(StdinKind),
}

impl From<PathBuf> for Input<'_> {
    fn from(p: PathBuf) -> Self {
        if p.as_os_str() == STDIN_REPR {
            Self::Stdin(StdinKind::Explicit)
        } else {
            Self::Path(Cow::Owned(p))
        }
    }
}

impl<'a, T: AsRef<Path> + ?Sized> From<&'a T> for Input<'a> {
    fn from(p: &'a T) -> Self {
        let p = p.as_ref();
        if p.as_os_str() == STDIN_REPR {
            Self::Stdin(StdinKind::Explicit)
        } else {
            Self::Path(Cow::Borrowed(p))
        }
    }
}

impl<'a> Input<'a> {
    /// Translates `Path(Cow::Owned(_))` to `Path(Cow::Borrowed(_))`.
    fn as_borrowed(&'a self) -> Self {
        match self {
            Self::Path(p) => Self::Path(Cow::Borrowed(p.borrow())),
            Self::Stdin(k) => Self::Stdin(*k),
        }
    }

    /// Converts input to title that appears in stats.
    fn to_title(&self) -> Option<Cow<'_, OsStr>> {
        match self {
            Self::Path(path) => {
                let path = path.as_os_str();
                if path.to_string_lossy().contains('\n') {
                    Some(Cow::Owned(quoting_style::locale_aware_escape_name(
                        path,
                        QuotingStyle::SHELL_ESCAPE,
                    )))
                } else {
                    Some(Cow::Borrowed(path))
                }
            }
            Self::Stdin(StdinKind::Explicit) => Some(Cow::Borrowed(OsStr::new(STDIN_REPR))),
            Self::Stdin(StdinKind::Implicit) => None,
        }
    }

    /// Converts input into the form that appears in errors.
    fn path_display(&self) -> String {
        match self {
            Self::Path(path) => escape_name_wrapper(path.as_os_str()),
            Self::Stdin(_) => translate!("wc-standard-input"),
        }
    }

    /// When given --files0-from, we may be given a path or stdin. Either may be a stream or
    /// a regular file. If given a file less than 10 MiB, it will be consumed and turned into
    /// a Vec of [`Input::Path`] which can be scanned to determine the widths of the columns that
    /// will ultimately be printed.
    fn try_as_files0(&self) -> UResult<Option<Vec<Input<'static>>>> {
        match self {
            Self::Path(path) => match fs::metadata(path) {
                Ok(meta) if meta.is_file() && meta.len() <= (10 << 20) => Ok(Some(
                    files0_iter_file(path)?.collect::<Result<Vec<_>, _>>()?,
                )),
                _ => Ok(None),
            },
            Self::Stdin(_) if is_stdin_small_file() => {
                Ok(Some(files0_iter_stdin().collect::<Result<Vec<_>, _>>()?))
            }
            Self::Stdin(_) => Ok(None),
        }
    }
}

#[cfg(unix)]
fn is_stdin_small_file() -> bool {
    use std::os::fd::AsFd;

    matches!(
        rustix::fs::fstat(io::stdin().as_fd()),
        Ok(meta) if meta.st_mode as libc::mode_t & libc::S_IFMT == libc::S_IFREG && meta.st_size <= (10 << 20)
    )
}

#[cfg(not(unix))]
/// Windows presents a piped stdin as a "normal file" with a length equal to however many bytes
/// have been buffered at the time it's checked. To be safe, we must never assume it's a file.
fn is_stdin_small_file() -> bool {
    false
}

/// When to show the "total" line
#[derive(Clone, Copy, Default, PartialEq)]
enum TotalWhen {
    #[default]
    Auto,
    Always,
    Only,
    Never,
}

impl<T: AsRef<str>> From<T> for TotalWhen {
    fn from(s: T) -> Self {
        match s.as_ref() {
            "auto" => Self::Auto,
            "always" => Self::Always,
            "only" => Self::Only,
            "never" => Self::Never,
            _ => unreachable!("Should have been caught by clap"),
        }
    }
}

impl TotalWhen {
    fn is_total_row_visible(self, num_inputs: usize) -> bool {
        match self {
            Self::Auto => num_inputs > 1,
            Self::Always | Self::Only => true,
            Self::Never => false,
        }
    }
}

#[derive(Debug, Error)]
enum WcError {
    #[error("{}", translate!("wc-error-files-disabled", "extra" => extra.quote()))]
    FilesDisabled { extra: Cow<'static, OsStr> },
    #[error("{}", translate!("wc-error-stdin-repr-not-allowed"))]
    StdinReprNotAllowed,
    #[error("{}", translate!("wc-error-zero-length-filename"))]
    ZeroLengthFileName,
    #[error("{}", translate!("wc-error-zero-length-filename-ctx", "path" => path, "idx" => idx))]
    ZeroLengthFileNameCtx { path: Cow<'static, str>, idx: usize },
}

impl WcError {
    fn zero_len(ctx: Option<(&Input, usize)>) -> Self {
        match ctx {
            Some((input, idx)) => {
                let path = match input {
                    Input::Stdin(_) => STDIN_REPR.into(),
                    Input::Path(path) => escape_name_wrapper(path.as_os_str()).into(),
                };
                Self::ZeroLengthFileNameCtx { path, idx }
            }
            None => Self::ZeroLengthFileName,
        }
    }
    fn files_disabled(first_extra: &OsString) -> Self {
        let extra = first_extra.clone().into();
        Self::FilesDisabled { extra }
    }
}

impl UError for WcError {
    fn usage(&self) -> bool {
        matches!(self, Self::FilesDisabled { .. })
    }
}

/// Logical entry point for the bundled `wc` builtin.
///
/// Unlike upstream's `uumain`, this never touches the process's own stdio: the
/// counts table is written to the shell's logical output sink (`out`), every
/// per-file diagnostic to the logical error sink (`err`), and stdin counting
/// reads the shell's logical input source (`stdin`, with its backing descriptor
/// `stdin_fd` when one exists). With no backing fd the `-c` `fstat`/splice fast
/// path is skipped and bytes are read through the trait object, so it is correct
/// for both real pipes/files and in-memory streams and holds no process-global
/// state.
///
/// `out_fd` is accepted for signature parity with the other injected-I/O
/// builtins; `wc` writes a small formatted table and never needs the raw output
/// descriptor (no splice-to-stdout fast path), so it is intentionally ignored.
pub fn wc(
    args: impl uucore::Args,
    out: &mut dyn Write,
    out_fd: Option<BorrowedFd<'_>>,
    err: &mut dyn Write,
    stdin: &mut dyn Read,
    stdin_fd: Option<BorrowedFd<'_>>,
) -> UResult<()> {
    let _ = out_fd; // see doc comment: wc has no output-fd fast path.
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let settings = Settings::new(&matches);
    let inputs = Inputs::new(&matches)?;

    let raw = stdin_fd.map(|fd| {
        use std::os::fd::AsRawFd;
        fd.as_raw_fd()
    });
    let mut handle = Some(countable::LogicalStdin::new(stdin, raw));
    wc_impl(&inputs, &settings, out, err, &mut handle)
}

/// Descriptor-based entry point for the standalone `wc` binary and the
/// `--invoke-bundled` subprocess dispatcher.
///
/// In those contexts the process's own fd 0/1/2 *are* the intended I/O, so this
/// is a thin bridge that hands them to [`wc`]. The in-process brush builtin must
/// NOT route through here — it calls [`wc`] directly with the shell's logical
/// sinks/source (and their backing fds, when any), so it never touches
/// process-global stdio.
#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    // SAFETY: fd 0/1/2 are the process's standard streams, open and valid for
    // the whole duration of this call.
    let out_fd = Some(unsafe { BorrowedFd::borrow_raw(1) });
    let in_fd = Some(unsafe { BorrowedFd::borrow_raw(0) });
    wc(args, &mut out, out_fd, &mut err, &mut input, in_fd)
}

pub fn uu_app() -> Command {
    Command::new("wc")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template(uucore::util_name()))
        .about(translate!("wc-about"))
        .override_usage(format_usage(&translate!("wc-usage")))
        .infer_long_args(true)
        .args_override_self(true)
        .arg(
            Arg::new(options::BYTES)
                .short('c')
                .long(options::BYTES)
                .help(translate!("wc-help-bytes"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::CHAR)
                .short('m')
                .long(options::CHAR)
                .help(translate!("wc-help-chars"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::FILES0_FROM)
                .long(options::FILES0_FROM)
                .value_name("F")
                .help(translate!("wc-help-files0-from"))
                .value_parser(ValueParser::os_string())
                .value_hint(clap::ValueHint::FilePath),
        )
        .arg(
            Arg::new(options::LINES)
                .short('l')
                .long(options::LINES)
                .help(translate!("wc-help-lines"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::MAX_LINE_LENGTH)
                .short('L')
                .long(options::MAX_LINE_LENGTH)
                .help(translate!("wc-help-max-line-length"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::TOTAL)
                .long(options::TOTAL)
                .value_parser(ShortcutValueParser::new([
                    "auto", "always", "only", "never",
                ]))
                .value_name("WHEN")
                .hide_possible_values(true)
                .help(translate!("wc-help-total")),
        )
        .arg(
            Arg::new(options::WORDS)
                .short('w')
                .long(options::WORDS)
                .help(translate!("wc-help-words"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::DEBUG)
                .long(options::DEBUG)
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new(ARG_FILES)
                .action(ArgAction::Append)
                .value_parser(ValueParser::os_string())
                .value_hint(clap::ValueHint::FilePath),
        )
}

fn word_count_from_reader<T: WordCountable>(
    mut reader: T,
    settings: &Settings,
) -> (WordCount, Option<io::Error>) {
    match (
        settings.show_bytes,
        settings.show_chars,
        settings.show_lines,
        settings.show_max_line_length,
        settings.show_words,
    ) {
        // Specialize scanning loop to improve the performance.
        (false, false, false, false, false) => unreachable!(),

        // show_bytes
        (true, false, false, false, false) => {
            // Fast path when only show_bytes is true.
            let (bytes, error) = count_bytes_fast(&mut reader);
            (
                WordCount {
                    bytes,
                    ..WordCount::default()
                },
                error,
            )
        }

        // Fast paths that can be computed without Unicode decoding.
        // show_lines
        (false, false, true, false, false) => {
            count_bytes_chars_and_lines_fast::<_, false, false, true>(&mut reader)
        }
        // show_chars
        (false, true, false, false, false) => {
            count_bytes_chars_and_lines_fast::<_, false, true, false>(&mut reader)
        }
        // show_chars, show_lines
        (false, true, true, false, false) => {
            count_bytes_chars_and_lines_fast::<_, false, true, true>(&mut reader)
        }
        // show_bytes, show_lines
        (true, false, true, false, false) => {
            count_bytes_chars_and_lines_fast::<_, true, false, true>(&mut reader)
        }
        // show_bytes, show_chars
        (true, true, false, false, false) => {
            count_bytes_chars_and_lines_fast::<_, true, true, false>(&mut reader)
        }
        // show_bytes, show_chars, show_lines
        (true, true, true, false, false) => {
            count_bytes_chars_and_lines_fast::<_, true, true, true>(&mut reader)
        }
        // show_words
        (_, false, false, false, true) => {
            word_count_from_reader_specialized::<_, false, false, false, true>(reader)
        }
        // show_max_line_length
        (_, false, false, true, false) => {
            word_count_from_reader_specialized::<_, false, false, true, false>(reader)
        }
        // show_max_line_length, show_words
        (_, false, false, true, true) => {
            word_count_from_reader_specialized::<_, false, false, true, true>(reader)
        }
        // show_lines, show_words
        (_, false, true, false, true) => {
            word_count_from_reader_specialized::<_, false, true, false, true>(reader)
        }
        // show_lines, show_max_line_length
        (_, false, true, true, false) => {
            word_count_from_reader_specialized::<_, false, true, true, false>(reader)
        }
        // show_lines, show_max_line_length, show_words
        (_, false, true, true, true) => {
            word_count_from_reader_specialized::<_, false, true, true, true>(reader)
        }
        // show_chars, show_words
        (_, true, false, false, true) => {
            word_count_from_reader_specialized::<_, true, false, false, true>(reader)
        }
        // show_chars, show_max_line_length
        (_, true, false, true, false) => {
            word_count_from_reader_specialized::<_, true, false, true, false>(reader)
        }
        // show_chars, show_max_line_length, show_words
        (_, true, false, true, true) => {
            word_count_from_reader_specialized::<_, true, false, true, true>(reader)
        }
        // show_chars, show_lines, show_words
        (_, true, true, false, true) => {
            word_count_from_reader_specialized::<_, true, true, false, true>(reader)
        }
        // show_chars, show_lines, show_max_line_length
        (_, true, true, true, false) => {
            word_count_from_reader_specialized::<_, true, true, true, false>(reader)
        }
        // show_chars, show_lines, show_max_line_length, show_words
        (_, true, true, true, true) => {
            word_count_from_reader_specialized::<_, true, true, true, true>(reader)
        }
    }
}

fn process_chunk<
    const SHOW_CHARS: bool,
    const SHOW_LINES: bool,
    const SHOW_MAX_LINE_LENGTH: bool,
    const SHOW_WORDS: bool,
>(
    total: &mut WordCount,
    text: &str,
    current_len: &mut usize,
    in_word: &mut bool,
    posixly_correct: bool,
) {
    for ch in text.chars() {
        if SHOW_WORDS {
            let is_space = if posixly_correct {
                matches!(ch, '\t'..='\r' | ' ')
            } else {
                ch.is_whitespace()
            };

            if is_space {
                *in_word = false;
            } else if !(*in_word) {
                // This also counts control characters! (As of GNU coreutils 9.5)
                *in_word = true;
                total.words += 1;
            }
        }
        if SHOW_MAX_LINE_LENGTH {
            match ch {
                '\n' | '\r' | '\x0c' => {
                    total.max_line_length = max(*current_len, total.max_line_length);
                    *current_len = 0;
                }
                '\t' => {
                    *current_len -= *current_len % 8;
                    *current_len += 8;
                }
                _ => {
                    *current_len += ch.width().unwrap_or(0);
                }
            }
        }
        if SHOW_LINES && ch == '\n' {
            total.lines += 1;
        }
        if SHOW_CHARS {
            total.chars += 1;
        }
    }
    total.bytes += text.len();

    total.max_line_length = max(*current_len, total.max_line_length);
}

fn handle_error(
    error: BufReadDecoderError<'_>,
    total: &mut WordCount,
    in_word: &mut bool,
) -> Option<io::Error> {
    match error {
        BufReadDecoderError::InvalidByteSequence(bytes) => {
            total.bytes += bytes.len();
            if !(*in_word) {
                *in_word = true;
                total.words += 1;
            }
        }
        BufReadDecoderError::Io(e) => return Some(e),
    }
    None
}

fn word_count_from_reader_specialized<
    T: WordCountable,
    const SHOW_CHARS: bool,
    const SHOW_LINES: bool,
    const SHOW_MAX_LINE_LENGTH: bool,
    const SHOW_WORDS: bool,
>(
    reader: T,
) -> (WordCount, Option<io::Error>) {
    let mut total = WordCount::default();
    let mut reader = BufReadDecoder::new(reader.buffered());
    let mut in_word = false;
    let mut current_len = 0;
    let posixly_correct = env::var_os("POSIXLY_CORRECT").is_some();
    while let Some(chunk) = reader.next_strict() {
        match chunk {
            Ok(text) => {
                process_chunk::<SHOW_CHARS, SHOW_LINES, SHOW_MAX_LINE_LENGTH, SHOW_WORDS>(
                    &mut total,
                    text,
                    &mut current_len,
                    &mut in_word,
                    posixly_correct,
                );
            }
            Err(e) => {
                if let Some(e) = handle_error(e, &mut total, &mut in_word) {
                    return (total, Some(e));
                }
            }
        }
    }

    (total, None)
}

enum CountResult {
    /// Nothing went wrong.
    Success(WordCount),
    /// Managed to open but failed to read.
    Interrupted(WordCount, io::Error),
    /// Didn't even manage to open.
    Failure(io::Error),
}

/// If we fail to open a file, we only show the error. If we fail reading the
/// file, we show a count for what we managed to read.
///
/// Therefore, the reading implementations always return a total and sometimes
/// return an error: ([`WordCount`], `Option<io::Error>`).
fn word_count_from_input(
    input: &Input<'_>,
    settings: &Settings,
    stdin: &mut Option<countable::LogicalStdin<'_>>,
) -> CountResult {
    let (total, maybe_err) = match input {
        Input::Stdin(_) => match stdin.take() {
            // Count the shell's logical stdin once. Repeated stdin entries
            // (e.g. `-` listed twice) drain the same stream, matching a real
            // pipe; the gate keeps such argvs off this path anyway.
            Some(handle) => word_count_from_reader(handle, settings),
            None => word_count_from_reader(
                countable::LogicalStdin::new(&mut io::empty(), None),
                settings,
            ),
        },
        Input::Path(path) => match File::open(path) {
            Ok(f) => word_count_from_reader(f, settings),
            Err(err) => return CountResult::Failure(err),
        },
    };
    match maybe_err {
        None => CountResult::Success(total),
        Some(err) => CountResult::Interrupted(total, err),
    }
}

/// Compute the number of digits needed to represent all counts in all inputs.
///
/// For [`Inputs::Stdin`], [`MINIMUM_WIDTH`] is returned, unless there is only one counter number
/// to be printed, in which case 1 is returned.
///
/// For [`Inputs::Files0From`], [`MINIMUM_WIDTH`] is returned.
///
/// An [`Inputs::Paths`] may include zero or more "-" entries, each of which represents reading
/// from `stdin`. The presence of any such entry causes this function to return a width that is at
/// least [`MINIMUM_WIDTH`].
///
/// If an [`Inputs::Paths`] contains only one path and only one number needs to be printed then
/// this function is optimized to return 1 without making any calls to get file metadata.
///
/// If file metadata could not be read from any of the [`Input::Path`] input, that input does not
/// affect number width computation.  Otherwise, the file sizes from the files' metadata are summed
/// and the number of digits in that total size is returned.
fn compute_number_width(inputs: &Inputs, settings: &Settings) -> usize {
    match inputs {
        Inputs::Stdin if settings.number_enabled() == 1 => 1,
        Inputs::Stdin => MINIMUM_WIDTH,
        Inputs::Files0From(_) => 1,
        Inputs::Paths(inputs) => {
            if settings.number_enabled() == 1 && inputs.len() == 1 {
                return 1;
            }

            let mut minimum_width = 1;
            let mut total: u64 = 0;
            for input in inputs {
                match input {
                    Input::Stdin(_) => minimum_width = MINIMUM_WIDTH,
                    Input::Path(path) => {
                        if let Ok(meta) = fs::metadata(path) {
                            if meta.is_file() {
                                total += meta.len();
                            } else {
                                minimum_width = MINIMUM_WIDTH;
                            }
                        }
                    }
                }
            }

            if total == 0 {
                minimum_width
            } else {
                let total_width = (1 + total.ilog10())
                    .try_into()
                    .expect("ilog of a u64 should fit into a usize");
                max(total_width, minimum_width)
            }
        }
    }
}

type InputIterItem<'a> = Result<Input<'a>, Box<dyn UError>>;

/// To be used with `--files0-from=-`, this applies a filter on the results of [`files0_iter`] to
/// translate '-' into the appropriate error.
fn files0_iter_stdin<'a>() -> impl Iterator<Item = InputIterItem<'a>> {
    files0_iter(io::stdin().lock(), STDIN_REPR.into()).map(|i| match i {
        Ok(Input::Stdin(_)) => Err(WcError::StdinReprNotAllowed.into()),
        _ => i,
    })
}

fn files0_iter_file<'a>(path: &Path) -> UResult<impl Iterator<Item = InputIterItem<'a>>> {
    match File::open(path) {
        Ok(f) => Ok(files0_iter(f, path.into())),
        Err(e) => Err(e.map_err_context(|| {
            translate!("wc-error-cannot-open-for-reading",
                "path" => quoting_style::locale_aware_escape_name(
                    path.as_os_str(),
                    QuotingStyle::SHELL_ESCAPE_QUOTE,
                )
                .into_string()
                .expect("All escaped names with the escaping option return valid strings.")
            )
        })),
    }
}

fn files0_iter<'a>(
    r: impl Read + 'static,
    err_path: OsString,
) -> impl Iterator<Item = InputIterItem<'a>> {
    use std::io::BufRead;
    let mut i = Some(
        io::BufReader::new(r)
            .split(b'\0')
            .map(move |res| match res {
                Ok(p) if p == STDIN_REPR.as_bytes() => Ok(Input::Stdin(StdinKind::Explicit)),
                Ok(p) => {
                    // On Unix systems, OsStrings are just strings of bytes, not necessarily UTF-8.
                    #[cfg(unix)]
                    {
                        use std::os::unix::ffi::OsStringExt;
                        Ok(Input::Path(PathBuf::from(OsString::from_vec(p)).into()))
                    }

                    // ...Windows does not, we must go through Strings.
                    #[cfg(not(unix))]
                    {
                        let s = String::from_utf8(p).map_err(io::Error::other)?;
                        Ok(Input::Path(PathBuf::from(s).into()))
                    }
                }
                Err(e) => Err(e.map_err_context(
                    || translate!("wc-error-read-error", "path" => escape_name_wrapper(&err_path)),
                ) as Box<dyn UError>),
            }),
    );
    // Loop until there is an error; yield that error and then nothing else.
    iter::from_fn(move || {
        let next = i.as_mut().and_then(Iterator::next);
        if matches!(next, Some(Err(_)) | None) {
            i = None;
        }
        next
    })
}

fn escape_name_wrapper(name: &OsStr) -> String {
    quoting_style::locale_aware_escape_name(name, QuotingStyle::SHELL_ESCAPE)
        .into_string()
        .expect("All escaped names with the escaping option return valid strings.")
}

fn hardware_feature_label(feature: HardwareFeature) -> &'static str {
    match feature {
        HardwareFeature::Avx512 => "AVX512F",
        HardwareFeature::Avx2 => "AVX2",
        HardwareFeature::PclMul => "PCLMUL",
        HardwareFeature::Vmull => "VMULL",
        HardwareFeature::Sse2 => "SSE2",
        HardwareFeature::Asimd => "ASIMD",
    }
}

fn is_simd_runtime_feature(feature: HardwareFeature) -> bool {
    matches!(
        feature,
        HardwareFeature::Avx2 | HardwareFeature::Sse2 | HardwareFeature::Asimd
    )
}

fn is_simd_debug_feature(feature: HardwareFeature) -> bool {
    matches!(
        feature,
        HardwareFeature::Avx512
            | HardwareFeature::Avx2
            | HardwareFeature::Sse2
            | HardwareFeature::Asimd
    )
}

struct WcSimdFeatures {
    enabled: Vec<HardwareFeature>,
    disabled: Vec<HardwareFeature>,
    disabled_runtime: Vec<HardwareFeature>,
}

fn wc_simd_features(policy: &SimdPolicy) -> WcSimdFeatures {
    let enabled = policy
        .iter_features()
        .filter(|v| is_simd_runtime_feature(*v))
        .collect();

    let mut disabled = Vec::new();
    let mut disabled_runtime = Vec::new();
    for feature in policy.disabled_features() {
        if is_simd_debug_feature(feature) {
            disabled.push(feature);
        }
        if is_simd_runtime_feature(feature) {
            disabled_runtime.push(feature);
        }
    }

    WcSimdFeatures {
        enabled,
        disabled,
        disabled_runtime,
    }
}

pub(crate) fn wc_simd_allowed(policy: &SimdPolicy) -> bool {
    let disabled_features = policy.disabled_features();
    if disabled_features.into_iter().any(is_simd_runtime_feature) {
        return false;
    }
    policy.iter_features().any(is_simd_runtime_feature)
}

fn wc_impl(
    inputs: &Inputs,
    settings: &Settings,
    out: &mut dyn Write,
    err: &mut dyn Write,
    stdin: &mut Option<countable::LogicalStdin<'_>>,
) -> UResult<()> {
    let mut total_word_count = WordCount::default();
    let mut num_inputs: usize = 0;

    // Highest non-zero exit code seen across all inputs. Diagnostics go to the
    // injected `err` sink (NOT the process stderr), and the code is returned to
    // the caller instead of stashed in uucore's process-global `set_exit_code`,
    // so this builtin is safe to run on a pipeline thread.
    let mut exit_code: i32 = 0;
    // Mirror of uucore's `show!`: print "wc: <diagnostic>" to the logical error
    // sink and remember the error's exit code. `$e` must be a `Box<dyn UError>`
    // (or convertible), exactly as `show!` expects.
    macro_rules! show_local {
        ($e:expr) => {{
            let e = $e;
            let code = e.code();
            if code != 0 {
                exit_code = exit_code.max(code);
            }
            let _ = writeln!(err, "{}: {e}", uucore::util_name());
        }};
    }

    let (number_width, are_stats_visible) = match settings.total_when {
        TotalWhen::Only => (1, false),
        _ => (compute_number_width(inputs, settings), true),
    };

    if settings.debug {
        let policy = SimdPolicy::detect();
        let features = wc_simd_features(policy);

        let enabled: Vec<&'static str> = features
            .enabled
            .iter()
            .copied()
            .map(hardware_feature_label)
            .collect();
        let disabled: Vec<&'static str> = features
            .disabled
            .iter()
            .copied()
            .map(hardware_feature_label)
            .collect();

        let enabled_empty = enabled.is_empty();
        let disabled_empty = disabled.is_empty();
        let runtime_disabled = !features.disabled_runtime.is_empty();

        if enabled_empty && !runtime_disabled {
            let _ = writeln!(err, "{}", translate!("wc-debug-hw-unavailable"));
        } else if runtime_disabled {
            let _ = writeln!(
                err,
                "{}",
                translate!("wc-debug-hw-disabled-glibc", "features" => disabled.join(", "))
            );
        } else if !enabled_empty && disabled_empty {
            let _ = writeln!(
                err,
                "{}",
                translate!("wc-debug-hw-using", "features" => enabled.join(", "))
            );
        } else {
            let _ = writeln!(
                err,
                "{}",
                translate!(
                    "wc-debug-hw-limited-glibc",
                    "disabled" => disabled.join(", "),
                    "enabled" => enabled.join(", ")
                )
            );
        }
    }

    for maybe_input in inputs.try_iter(settings)? {
        num_inputs += 1;

        let input = match maybe_input {
            Ok(input) => input,
            Err(e) => {
                show_local!(e);
                continue;
            }
        };

        // Store any I/O error from reading to print AFTER stats (matches GNU wc behavior)
        let (word_count, deferred_error) =
            match word_count_from_input(&input, settings, stdin) {
                CountResult::Success(word_count) => (word_count, None),
                CountResult::Interrupted(word_count, e) => (
                    word_count,
                    Some(e.map_err_context(|| input.path_display())),
                ),
                CountResult::Failure(e) => {
                    show_local!(e.map_err_context(|| input.path_display()));
                    continue;
                }
            };
        total_word_count += word_count;
        if are_stats_visible {
            let maybe_title = input.to_title();
            let maybe_title_str = maybe_title.as_deref();
            if let Err(e) = print_stats(out, settings, &word_count, maybe_title_str, number_width) {
                let title = maybe_title_str.unwrap_or(OsStr::new("<stdin>"));
                show_local!(e.map_err_context(|| translate!("wc-error-failed-to-print-result", "title" => title.to_string_lossy())));
                return finish(exit_code);
            }
        }
        // Print deferred error after stats to match GNU wc output order
        if let Some(e) = deferred_error {
            let _ = out.flush();
            show_local!(e);
        }
    }

    if settings.total_when.is_total_row_visible(num_inputs) {
        let wc_total_msg = translate!("wc-total");
        let title = are_stats_visible.then_some(OsStr::new(&wc_total_msg));
        if let Err(e) = print_stats(out, settings, &total_word_count, title, number_width) {
            show_local!(e.map_err_context(|| translate!("wc-error-failed-to-print-total")));
        }
    }

    let _ = out.flush();
    finish(exit_code)
}

/// Translate an accumulated non-zero exit code into a `UResult`. The standalone
/// `uumain` bridge maps the returned error code to the process exit status; the
/// in-process builtin reads it the same way — neither relies on uucore's
/// process-global `set_exit_code`, keeping this builtin pipeline-thread safe.
fn finish(exit_code: i32) -> UResult<()> {
    if exit_code == 0 {
        Ok(())
    } else {
        // Empty message: every diagnostic was already written to `err`; this
        // only carries the exit status. The standalone binary's runner prints
        // nothing extra for an empty message.
        Err(uucore::error::USimpleError::new(exit_code, String::new()))
    }
}

fn print_stats(
    out: &mut dyn Write,
    settings: &Settings,
    result: &WordCount,
    title: Option<&OsStr>,
    number_width: usize,
) -> io::Result<()> {
    let maybe_cols = [
        (settings.show_lines, result.lines),
        (settings.show_words, result.words),
        (settings.show_chars, result.chars),
        (settings.show_bytes, result.bytes),
        (settings.show_max_line_length, result.max_line_length),
    ];

    let mut space = "";
    for (_, num) in maybe_cols.iter().filter(|(show, _)| *show) {
        write!(out, "{space}{num:number_width$}")?;
        space = " ";
    }

    if let Some(title) = title {
        write!(out, "{space}")?;
        out.write_all(&uucore::os_str_as_bytes_lossy(title))?;
    }
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use super::wc;
    use std::ffi::OsString;

    /// Drive the logical `wc` entry with in-memory streams and NO backing
    /// descriptors (out_fd=None, stdin_fd=None), asserting the counts table is
    /// written to `out` and nothing leaks to the process stdio. This is the
    /// path the in-process brush builtin uses for a sink/source with no fd.
    fn run(flags: &[&str], input: &[u8]) -> (String, String, Result<(), i32>) {
        let mut argv: Vec<OsString> = vec![OsString::from("wc")];
        argv.extend(flags.iter().map(OsString::from));
        // No file args -> counts the injected stdin.
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let mut stdin: &[u8] = input;
        let res = wc(
            argv.into_iter(),
            &mut out,
            None,
            &mut err,
            &mut stdin,
            None,
        );
        let code = res.map_err(|e| e.code());
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            code,
        )
    }

    #[test]
    fn lines_only_stdin_no_fd() {
        // Single counter on stdin -> width 1, no leading padding.
        let (out, err, code) = run(&["-l"], b"a\nb\nc\n");
        assert_eq!(out, "3\n");
        assert_eq!(err, "");
        assert_eq!(code, Ok(()));
    }

    #[test]
    fn bytes_only_stdin_no_fd() {
        let (out, err, code) = run(&["-c"], b"hello world");
        assert_eq!(out, "11\n");
        assert_eq!(err, "");
        assert_eq!(code, Ok(()));
    }

    #[test]
    fn lines_and_bytes_stdin_no_fd() {
        // Two counters from a non-fd (in-memory) stdin -> MINIMUM_WIDTH (7),
        // matching GNU's behavior for a piped stdin.
        let (out, err, code) = run(&["-lc"], b"one\ntwo\n");
        assert_eq!(out, "      2       8\n");
        assert_eq!(err, "");
        assert_eq!(code, Ok(()));
    }

    #[test]
    fn empty_stdin_no_fd() {
        let (out, _err, code) = run(&["-lc"], b"");
        assert_eq!(out, "      0       0\n");
        assert_eq!(code, Ok(()));
    }

    #[test]
    fn no_trailing_newline_lines() {
        // GNU counts newlines, so a final line with no '\n' is not counted.
        let (out, _err, code) = run(&["-l"], b"a\nb");
        assert_eq!(out, "1\n");
        assert_eq!(code, Ok(()));
    }
}
