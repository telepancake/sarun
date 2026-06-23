// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore espidf nopipe

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Error, ErrorKind, Read, Result, Write, stderr, stdin, stdout};
use std::path::{Path, PathBuf};
use uucore::display::Quotable;
use uucore::error::{UResult, strip_errno};
use uucore::translate;

mod cli;
pub use crate::cli::uu_app;
use crate::cli::{Options, OutputErrorMode, options};

#[cfg(target_os = "linux")]
use uucore::signals::ensure_stdout_not_broken;
#[cfg(unix)]
use uucore::signals::{disable_pipe_errors, ignore_interrupts};

// ── Injected-I/O plumbing for the in-process brush builtin ───────────────────
//
// STREAM + CWD template: [`tee_main`] reads the shell's LOGICAL stdin, writes
// the shell's LOGICAL stdout AND its file operands (relative ones rooted at the
// shell's LOGICAL cwd — the process is never `chdir`'d), and routes diagnostics
// to the shell's LOGICAL stderr. `tee` runs on a FRESH worker thread per call
// (the engine's `run_coreutil_localized`), so this thread-local err buffer is
// per-instance. The crate-local `tee_err!` SHADOWS the upstream
// `writeln!(stderr(), …)` sites; both [`tee_main`] and the standalone
// [`uumain`] drain it (the latter to real stderr), so standalone behavior is
// unchanged. The logical stdout/stdin are passed by reference, carried as the
// borrowed [`Writer::Logical`] first writer and the [`NamedReader`] source.
thread_local! {
    static TEE_ERR: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Buffers a `format!`-style diagnostic (with a trailing newline) into the
/// logical stderr buffer. Replaces the upstream `writeln!(stderr(), …)` sites.
macro_rules! tee_err {
    ($($args:tt)+) => ({
        use std::io::Write as _;
        $crate::TEE_ERR.with(|b| {
            let _ = writeln!(b.borrow_mut(), $($args)+);
        });
    });
}

/// Build [`Options`] from parsed args, shared by [`tee_main`] and [`uumain`].
fn options_from(matches: &clap::ArgMatches) -> Options {
    let append = matches.get_flag(options::APPEND);
    let ignore_interrupts = matches.get_flag(options::IGNORE_INTERRUPTS);
    let ignore_pipe_errors = matches.get_flag(options::IGNORE_PIPE_ERRORS);
    let output_error = matches
        .get_one::<String>(options::OUTPUT_ERROR)
        .map(|s| match s.as_str() {
            "warn" => OutputErrorMode::Warn,
            "warn-nopipe" => OutputErrorMode::WarnNoPipe,
            "exit" => OutputErrorMode::Exit,
            "exit-nopipe" => OutputErrorMode::ExitNoPipe,
            _ => unreachable!("clap excluded it"),
        })
        .or_else(|| ignore_pipe_errors.then_some(OutputErrorMode::WarnNoPipe));

    let files = matches
        .get_many::<OsString>(options::FILE)
        .map(|v| v.cloned().collect())
        .unwrap_or_default();

    Options {
        append,
        ignore_interrupts,
        ignore_pipe_errors,
        files,
        output_error,
    }
}

/// Logical entry point for the in-process brush `tee` builtin.
///
/// Reads from `inp` (the shell's logical stdin — never the engine's fd 0),
/// writes to `out` (the shell's logical stdout) AND to the file operands
/// (relative ones rooted at `cwd`), with diagnostics drained to `err`.
pub fn tee_main(
    args: impl uucore::Args,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
    inp: &mut dyn Read,
) -> UResult<()> {
    TEE_ERR.with(|b| b.borrow_mut().clear());
    let matches = match uucore::clap_localization::handle_clap_result(uu_app(), args) {
        Ok(m) => m,
        Err(e) => {
            let produced = TEE_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
            let _ = err.write_all(&produced);
            let _ = err.flush();
            return Err(e);
        }
    };
    let options = options_from(&matches);
    let res = tee(&options, Some(cwd), out, inp);
    let produced = TEE_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = out.flush();
    let _ = err.write_all(&produced);
    let _ = err.flush();
    res.map_err(|_| 1.into())
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    TEE_ERR.with(|b| b.borrow_mut().clear());
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;
    let options = options_from(&matches);
    let mut out = stdout();
    let mut inp = stdin();
    let res = tee(&options, None, &mut out, &mut inp);
    let produced = TEE_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = stderr().write_all(&produced);
    res.map_err(|_| 1.into())
}

/// `tee`'s core copy loop. `cwd` (when `Some`) roots relative file operands at
/// the shell's logical cwd; `out` is the logical stdout writer (the first
/// [`NamedWriter`]); `inp` is the logical stdin source.
fn tee(
    options: &Options,
    cwd: Option<&Path>,
    out: &mut dyn Write,
    inp: &mut dyn Read,
) -> Result<()> {
    #[cfg(unix)]
    {
        // ErrorKind::Other is raised by MultiWriter when all writers have exited.
        // This is therefore just a clever way to stop all writers

        if options.ignore_interrupts {
            ignore_interrupts().map_err(|_| Error::from(ErrorKind::Other))?;
        }
        if options.output_error.is_some() {
            disable_pipe_errors().map_err(|_| Error::from(ErrorKind::Other))?;
        }
    }
    let mut writers: Vec<NamedWriter> = options
        .files
        .iter()
        .filter_map(|file| open(file, cwd, options.append, options.output_error.as_ref()))
        .collect::<Result<Vec<NamedWriter>>>()?;
    let had_open_errors = writers.len() != options.files.len();

    writers.insert(
        0,
        NamedWriter {
            name: translate!("tee-standard-output").into(),
            inner: Writer::Logical(out),
        },
    );

    let mut output = MultiWriter::new(writers, options.output_error.clone());
    let input = NamedReader { inner: inp };

    #[cfg(target_os = "linux")]
    if options.ignore_pipe_errors && !ensure_stdout_not_broken()? && output.writers.len() == 1 {
        return Ok(());
    }

    // We cannot use std::io::copy here as it doesn't flush the output buffer
    let res = match copy(input, &mut output) {
        // ErrorKind::Other is raised by MultiWriter when all writers
        // have exited, so that copy will abort. It's equivalent to
        // success of this part (if there was an error that should
        // cause a failure from any writer, that error would have been
        // returned instead).
        Err(e) if e.kind() != ErrorKind::Other => Err(e),
        _ => Ok(()),
    };

    if had_open_errors || res.is_err() || output.flush().is_err() || output.error_occurred() {
        Err(Error::from(ErrorKind::Other))
    } else {
        Ok(())
    }
}

/// Copies all bytes from the input buffer to the output buffer.
///
/// Returns the number of written bytes.
fn copy(mut input: impl Read, mut output: impl Write) -> Result<usize> {
    // The implementation for this function is adopted from the generic buffer copy implementation from
    // the standard library:
    // https://github.com/rust-lang/rust/blob/2feb91181882e525e698c4543063f4d0296fcf91/library/std/src/io/copy.rs#L271-L297

    // Use small buffer size from std implementation for small input
    // https://github.com/rust-lang/rust/blob/2feb91181882e525e698c4543063f4d0296fcf91/library/std/src/sys/io/mod.rs#L44
    const FIRST_BUF_SIZE: usize = if cfg!(target_os = "espidf") {
        512
    } else {
        8 * 1024
    };
    let mut buffer = [0u8; FIRST_BUF_SIZE];
    let mut len = 0;
    match input.read(&mut buffer) {
        Ok(0) => return Ok(0),
        Ok(bytes_count) => {
            output.write_all(&buffer[0..bytes_count])?;
            len = bytes_count;
            if bytes_count < FIRST_BUF_SIZE {
                // flush the buffer to comply with POSIX requirement that
                // `tee` does not buffer the input.
                output.flush()?;
                return Ok(len);
            }
        }
        Err(e) if e.kind() == ErrorKind::Interrupted => (),
        Err(e) => return Err(e),
    }

    // but optimize buffer size also for large file
    let mut buffer = vec![0u8; 4 * FIRST_BUF_SIZE]; //stack array makes code path for smaller file slower
    loop {
        match input.read(&mut buffer) {
            Ok(0) => return Ok(len), // end of file
            Ok(received) => {
                output.write_all(&buffer[..received])?;
                // flush the buffer to comply with POSIX requirement that
                // `tee` does not buffer the input.
                output.flush()?;
                len += received;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Tries to open the indicated file and return it. Reports an error if that's not possible.
/// If that error should lead to program termination, this function returns Some(Err()),
/// otherwise it returns None.
fn open<'a>(
    name: &OsString,
    cwd: Option<&Path>,
    append: bool,
    output_error: Option<&OutputErrorMode>,
) -> Option<Result<NamedWriter<'a>>> {
    // Root a relative file operand at the shell's logical cwd (the process is
    // never `chdir`'d); the diagnostic name stays the operand as given.
    let path = match cwd {
        Some(cwd) if Path::new(name).is_relative() => cwd.join(name),
        _ => PathBuf::from(name),
    };
    let mut options = OpenOptions::new();
    let mode = if append {
        options.append(true)
    } else {
        options.truncate(true)
    };
    match mode.write(true).create(true).open(path.as_path()) {
        Ok(file) => Some(Ok(NamedWriter {
            inner: Writer::File(file),
            name: name.clone(),
        })),
        Err(f) => {
            tee_err!("{}: {f}", name.maybe_quote());
            match output_error {
                Some(OutputErrorMode::Exit | OutputErrorMode::ExitNoPipe) => Some(Err(f)),
                _ => None,
            }
        }
    }
}

struct MultiWriter<'a> {
    writers: Vec<NamedWriter<'a>>,
    output_error_mode: Option<OutputErrorMode>,
    ignored_errors: usize,
}

impl<'a> MultiWriter<'a> {
    fn new(writers: Vec<NamedWriter<'a>>, output_error_mode: Option<OutputErrorMode>) -> Self {
        Self {
            writers,
            output_error_mode,
            ignored_errors: 0,
        }
    }

    fn error_occurred(&self) -> bool {
        self.ignored_errors != 0
    }
}

fn process_error(
    mode: Option<&OutputErrorMode>,
    f: Error,
    writer: &NamedWriter,
    ignored_errors: &mut usize,
) -> Result<()> {
    match mode {
        Some(OutputErrorMode::Warn) => {
            tee_err!("{}: {f}", writer.name.maybe_quote());
            *ignored_errors += 1;
            Ok(())
        }
        Some(OutputErrorMode::WarnNoPipe) | None => {
            if f.kind() != ErrorKind::BrokenPipe {
                tee_err!("{}: {f}", writer.name.maybe_quote());
                *ignored_errors += 1;
            }
            Ok(())
        }
        Some(OutputErrorMode::Exit) => {
            tee_err!("{}: {f}", writer.name.maybe_quote());
            Err(f)
        }
        Some(OutputErrorMode::ExitNoPipe) => {
            if f.kind() == ErrorKind::BrokenPipe {
                Ok(())
            } else {
                tee_err!("{}: {f}", writer.name.maybe_quote());
                Err(f)
            }
        }
    }
}

impl Write for MultiWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let mut aborted = None;
        let mode = self.output_error_mode.clone();
        let mut errors = 0;
        self.writers.retain_mut(|writer| {
            let result = writer.write_all(buf);
            match result {
                Err(f) => {
                    if let Err(e) = process_error(mode.as_ref(), f, writer, &mut errors) {
                        if aborted.is_none() {
                            aborted = Some(e);
                        }
                    }
                    false
                }
                _ => true,
            }
        });
        self.ignored_errors += errors;
        if let Some(e) = aborted {
            Err(e)
        } else if self.writers.is_empty() {
            // This error kind will never be raised by the standard
            // library, so we can use it for early termination of
            // `copy`
            Err(Error::from(ErrorKind::Other))
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> Result<()> {
        let mut aborted = None;
        let mode = self.output_error_mode.clone();
        let mut errors = 0;
        self.writers.retain_mut(|writer| {
            let result = writer.flush();
            match result {
                Err(f) => {
                    if let Err(e) = process_error(mode.as_ref(), f, writer, &mut errors) {
                        if aborted.is_none() {
                            aborted = Some(e);
                        }
                    }
                    false
                }
                _ => true,
            }
        });
        self.ignored_errors += errors;
        if let Some(e) = aborted {
            Err(e)
        } else {
            Ok(())
        }
    }
}

enum Writer<'a> {
    File(std::fs::File),
    /// The shell's logical stdout, borrowed for the duration of the `tee` call.
    /// Replaces the upstream `Stdout` variant on the in-process path; the
    /// standalone [`uumain`] passes `std::io::stdout()` here by reference.
    Logical(&'a mut dyn Write),
}

impl Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        match self {
            Self::File(f) => f.write(buf),
            Self::Logical(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> Result<()> {
        match self {
            Self::File(f) => f.flush(),
            Self::Logical(s) => s.flush(),
        }
    }
}

struct NamedWriter<'a> {
    inner: Writer<'a>,
    pub name: OsString,
}

impl Write for NamedWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

struct NamedReader<'a> {
    inner: &'a mut dyn Read,
}

impl Read for NamedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.inner.read(buf) {
            Err(f) => {
                tee_err!(
                    "tee: {}",
                    translate!("tee-error-stdin", "error" => strip_errno(&f))
                );
                Err(f)
            }
            okay => okay,
        }
    }
}
