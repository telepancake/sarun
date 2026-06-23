// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) ugoa cmode RAII

use clap::builder::ValueParser;
use clap::{Arg, ArgAction, ArgMatches, Command};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(all(unix, target_os = "linux"))]
use uucore::error::FromIo;
use uucore::error::{UResult, USimpleError};
use uucore::translate;

#[cfg(not(windows))]
use uucore::mode;
use uucore::{display::Quotable, fs::dir_strip_dot_for_creation};
use uucore::format_usage;

static DEFAULT_PERM: u32 = 0o777;

// ── Injected-I/O plumbing for the in-process brush builtin ───────────────────
//
// FILESYSTEM template, mirroring uu_cp: [`mkdir_main`] resolves every relative
// operand against the shell's LOGICAL cwd (the process is never `chdir`'d) and
// routes all output to the shell's logical sinks. `mkdir` runs on a FRESH worker
// thread per call (the engine's `run_coreutil_localized`), so these thread-
// locals are per-instance: `MKDIR_OUT` buffers verbose output, `MKDIR_ERR`
// diagnostics, `MKDIR_EXIT` the deferred exit code. The crate-local `show!` /
// `show_if_err!` macros below SHADOW uucore's (which write fd 2 + the process-
// global exit code), and the verbose `writeln!` sites target [`MkdirOut`]. Both
// the logical entry and standalone [`uumain`] drain the buffers (the latter to
// real stdio), so standalone behavior is unchanged.
thread_local! {
    static MKDIR_OUT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static MKDIR_ERR: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static MKDIR_EXIT: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// `Write` sink for verbose output; buffers into [`MKDIR_OUT`] (drained to the
/// logical or real stdout by the entry point). Replaces upstream's
/// `writeln!(stdout(), …)`.
struct MkdirOut;
impl Write for MkdirOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        MKDIR_OUT.with(|b| b.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Shadows [`uucore::show!`]: records the error's code into [`MKDIR_EXIT`] and
/// writes `<util_name>: <err>` to the logical stderr buffer.
macro_rules! show {
    ($err:expr) => {{
        #[allow(unused_imports)]
        use uucore::error::UError as _;
        use std::io::Write as _;
        let e = $err;
        $crate::MKDIR_EXIT.with(|c| c.set(e.code()));
        $crate::MKDIR_ERR.with(|b| {
            let _ = writeln!(b.borrow_mut(), "{}: {e}", uucore::util_name());
        });
    }};
}

/// Shadows [`uucore::show_if_err!`], routing through the crate-local `show!`.
macro_rules! show_if_err {
    ($res:expr) => {{
        if let Err(e) = $res {
            show!(e);
        }
    }};
}

mod options {
    pub const MODE: &str = "mode";
    pub const PARENTS: &str = "parents";
    pub const VERBOSE: &str = "verbose";
    pub const DIRS: &str = "dirs";
    pub const SECURITY_CONTEXT: &str = "z";
    pub const CONTEXT: &str = "context";
}

/// Configuration for directory creation.
pub struct Config<'a> {
    /// Create parent directories as needed.
    pub recursive: bool,

    /// File permissions (octal).
    pub mode: u32,

    /// Print message for each created directory.
    pub verbose: bool,

    /// Set security context (SELinux/SMACK).
    pub set_security_context: bool,

    /// Specific `SELinux` context.
    pub context: Option<&'a String>,
}

#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "fn sig must match on all platforms"
)]
fn get_mode(_matches: &ArgMatches) -> Result<u32, String> {
    Ok(DEFAULT_PERM)
}

#[cfg(not(windows))]
fn get_mode(matches: &ArgMatches) -> Result<u32, String> {
    // Not tested on Windows
    if let Some(m) = matches.get_one::<String>(options::MODE) {
        mode::parse_chmod(DEFAULT_PERM, m, true, mode::get_umask())
    } else {
        // If no mode argument is specified return the mode derived from umask
        Ok(!mode::get_umask() & DEFAULT_PERM)
    }
}

/// Logical entry point for the in-process brush `mkdir` builtin.
///
/// Mirrors [`uumain`] but (1) resolves every relative directory operand against
/// the shell's LOGICAL `cwd` (the process is never `chdir`'d), and (2) never
/// touches process fd 1/2 — verbose output drains to `out`, diagnostics to
/// `err`, the deferred exit code surfaces as the returned status.
pub fn mkdir_main(
    args: impl uucore::Args,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> UResult<()> {
    MKDIR_OUT.with(|b| b.borrow_mut().clear());
    MKDIR_ERR.with(|b| b.borrow_mut().clear());
    MKDIR_EXIT.with(|c| c.set(0));

    let result = run(args, Some(cwd));

    let produced_out = MKDIR_OUT.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let produced_err = MKDIR_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = out.write_all(&produced_out);
    let _ = out.flush();
    let _ = err.write_all(&produced_err);
    let _ = err.flush();

    let deferred = MKDIR_EXIT.with(std::cell::Cell::get);
    match result {
        Ok(()) if deferred != 0 => Err(USimpleError::new(deferred, String::new())),
        other => other,
    }
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    MKDIR_OUT.with(|b| b.borrow_mut().clear());
    MKDIR_ERR.with(|b| b.borrow_mut().clear());
    MKDIR_EXIT.with(|c| c.set(0));

    let result = run(args, None);

    let produced_out = MKDIR_OUT.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let produced_err = MKDIR_ERR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let _ = std::io::stdout().write_all(&produced_out);
    let _ = std::io::stderr().write_all(&produced_err);

    let deferred = MKDIR_EXIT.with(std::cell::Cell::get);
    match result {
        Ok(()) if deferred != 0 => Err(USimpleError::new(deferred, String::new())),
        other => other,
    }
}

/// Shared body of [`mkdir_main`] and [`uumain`]. When `cwd` is `Some`, relative
/// operands are rooted at the shell's logical cwd; when `None` (standalone) they
/// resolve against the process cwd, as upstream.
fn run(args: impl uucore::Args, cwd: Option<&Path>) -> UResult<()> {
    // Linux-specific options, not implemented
    // opts.optflag("Z", "context", "set SELinux security context" +
    // " of each created directory to CTX"),
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    let dirs: Vec<PathBuf> = matches
        .get_many::<OsString>(options::DIRS)
        .unwrap_or_default()
        .map(PathBuf::from)
        .map(|p| match cwd {
            Some(cwd) if p.is_relative() => cwd.join(p),
            _ => p,
        })
        .collect();
    let verbose = matches.get_flag(options::VERBOSE);
    let recursive = matches.get_flag(options::PARENTS);

    // Extract the SELinux related flags and options
    let set_security_context = matches.get_flag(options::SECURITY_CONTEXT);
    let context = matches.get_one::<String>(options::CONTEXT);

    match get_mode(&matches) {
        Ok(mode) => {
            let config = Config {
                recursive,
                mode,
                verbose,
                set_security_context: set_security_context || context.is_some(),
                context,
            };
            exec(&dirs, &config);
            Ok(())
        }
        Err(f) => Err(USimpleError::new(1, f)),
    }
}

pub fn uu_app() -> Command {
    Command::new("mkdir")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("mkdir"))
        .about(translate!("mkdir-about"))
        .override_usage(format_usage(&translate!("mkdir-usage")))
        .infer_long_args(true)
        .after_help(translate!("mkdir-after-help"))
        .arg(
            Arg::new(options::MODE)
                .short('m')
                .long(options::MODE)
                .help(translate!("mkdir-help-mode"))
                .allow_hyphen_values(true)
                .num_args(1),
        )
        .arg(
            Arg::new(options::PARENTS)
                .short('p')
                .long(options::PARENTS)
                .help(translate!("mkdir-help-parents"))
                .overrides_with(options::PARENTS)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::VERBOSE)
                .short('v')
                .long(options::VERBOSE)
                .help(translate!("mkdir-help-verbose"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::SECURITY_CONTEXT)
                .short('Z')
                .help(translate!("mkdir-help-selinux"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::CONTEXT)
                .long(options::CONTEXT)
                .value_name("CTX")
                .help(translate!("mkdir-help-context")),
        )
        .arg(
            Arg::new(options::DIRS)
                .action(ArgAction::Append)
                .num_args(1..)
                .required(true)
                .value_parser(ValueParser::os_string())
                .value_hint(clap::ValueHint::DirPath),
        )
}

/**
 * Create the list of new directories
 */
fn exec(dirs: &[PathBuf], config: &Config) {
    for dir in dirs {
        show_if_err!(mkdir(dir.as_path(), config));
    }
}

/// Create directory at a given `path`.
///
/// ## Options
///
/// * `recursive` --- create parent directories for the `path`, if they do not
///   exist.
/// * `mode` --- file mode for the directories (not implemented on windows).
/// * `verbose` --- print a message for each printed directory.
///
/// ## Trailing dot
///
/// To match the GNU behavior, a path with the last directory being a single dot
/// (like `some/path/to/.`) is created (with the dot stripped).
pub fn mkdir(path: &Path, config: &Config) -> UResult<()> {
    if path.as_os_str().is_empty() {
        return Err(USimpleError::new(
            1,
            translate!("mkdir-error-empty-directory-name"),
        ));
    }
    // Special case to match GNU's behavior:
    // mkdir -p foo/. should work and just create foo/
    // std::fs::create_dir("foo/."); fails in pure Rust
    let path_buf = dir_strip_dot_for_creation(path);
    let path = path_buf.as_path();
    create_dir(path, false, config)
}

/// Only needed on Linux to add ACL permission bits after directory creation.
#[cfg(all(unix, target_os = "linux"))]
fn chmod(path: &Path, mode: u32) -> UResult<()> {
    use std::fs::{Permissions, set_permissions};
    use std::os::unix::fs::PermissionsExt;
    let mode = Permissions::from_mode(mode);
    set_permissions(path, mode).map_err_context(
        || translate!("mkdir-error-cannot-set-permissions", "path" => path.quote()),
    )
}

// Create a directory at the given path.
// Uses iterative approach instead of recursion to avoid stack overflow with deep nesting.
fn create_dir(path: &Path, is_parent: bool, config: &Config) -> UResult<()> {
    let path_exists = path.exists();
    if path_exists && !config.recursive {
        return Err(USimpleError::new(
            1,
            translate!("mkdir-error-file-exists", "path" => path.maybe_quote()),
        ));
    }
    if path == Path::new("") {
        return Ok(());
    }

    // Iterative implementation: collect all directories to create, then create them
    // This avoids stack overflow with deeply nested directories
    if config.recursive {
        // Pre-allocate approximate capacity to avoid reallocations
        let mut dirs_to_create = Vec::with_capacity(16);
        let mut current = path;

        // First pass: collect all parent directories
        while let Some(parent) = current.parent() {
            if parent == Path::new("") {
                break;
            }
            dirs_to_create.push(parent);
            current = parent;
        }

        // Second pass: create directories from root to leaf
        // Only create those that don't exist
        for dir in dirs_to_create.iter().rev() {
            if !dir.exists() {
                create_single_dir(dir, true, config)?;
            }
        }
    }

    // Create the target directory
    create_single_dir(path, is_parent, config)
}

/// RAII guard to restore umask on drop, ensuring cleanup even on panic.
#[cfg(unix)]
struct UmaskGuard(rustix::fs::Mode);

#[cfg(unix)]
impl UmaskGuard {
    /// Set umask to the given value and return a guard that restores the original on drop.
    fn set(new_mask: rustix::fs::Mode) -> Self {
        let old_mask = rustix::process::umask(new_mask);
        Self(old_mask)
    }
}

#[cfg(unix)]
impl Drop for UmaskGuard {
    fn drop(&mut self) {
        rustix::process::umask(self.0);
    }
}

/// Create a directory with the exact mode specified, bypassing umask.
///
/// GNU mkdir temporarily sets umask to 0 before calling mkdir(2), ensuring the
/// directory is created atomically with the correct permissions. This avoids a
/// race condition where the directory briefly exists with umask-based permissions.
#[cfg(unix)]
fn create_dir_with_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    // Temporarily set umask to 0 so the directory is created with the exact mode.
    // The guard restores the original umask on drop, even if we panic.
    let _guard = UmaskGuard::set(rustix::fs::Mode::empty());

    std::fs::DirBuilder::new().mode(mode).create(path)
}

#[cfg(not(unix))]
fn create_dir_with_mode(path: &Path, _mode: u32) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

// Helper function to create a single directory with appropriate permissions
// `is_parent` argument is not used on windows
#[allow(unused_variables)]
fn create_single_dir(path: &Path, is_parent: bool, config: &Config) -> UResult<()> {
    let path_exists = path.exists();

    // Calculate the mode to use for directory creation
    #[cfg(unix)]
    let create_mode = if is_parent {
        // For parent directories with -p, use umask-derived mode with u+wx
        (!mode::get_umask() & 0o777) | 0o300
    } else {
        config.mode
    };
    #[cfg(not(unix))]
    let create_mode = config.mode;

    match create_dir_with_mode(path, create_mode) {
        Ok(()) => {
            if config.verbose {
                writeln!(
                    MkdirOut,
                    "{}",
                    translate!("mkdir-verbose-created-directory", "util_name" => "mkdir", "path" => path.quote())
                )?;
            }

            // On Linux, we may need to add ACL permission bits via chmod.
            // On other Unix systems, the directory was already created with the correct mode.
            #[cfg(all(unix, target_os = "linux"))]
            if !path_exists {
                // TODO: Make this macos and freebsd compatible by creating a function to get permission bits from
                // acl in extended attributes
                let acl_perm_bits = uucore::fsxattr::get_acl_perm_bits_from_xattr(path);
                if acl_perm_bits != 0 {
                    chmod(path, create_mode | acl_perm_bits)?;
                }
            }

            // Apply SELinux context if requested
            #[cfg(feature = "selinux")]
            if config.set_security_context && uucore::selinux::is_selinux_enabled() {
                if let Err(e) = uucore::selinux::set_selinux_security_context(path, config.context)
                {
                    let _ = std::fs::remove_dir(path);
                    return Err(USimpleError::new(1, e.to_string()));
                }
            }

            // Apply SMACK context if requested
            #[cfg(feature = "smack")]
            if config.set_security_context {
                uucore::smack::set_smack_label_and_cleanup(path, config.context, |p| {
                    std::fs::remove_dir(p)
                })?;
            }
            Ok(())
        }

        Err(_) if path.is_dir() => {
            // Directory already exists - check if this is a logical directory creation
            // (i.e., not just a parent reference like "test_dir/..")
            let ends_with_parent_dir = matches!(
                path.components().next_back(),
                Some(std::path::Component::ParentDir)
            );

            // Print verbose message for logical directories, even if they exist
            // This matches GNU behavior for paths like "test_dir/../test_dir_a"
            if config.verbose && is_parent && config.recursive && !ends_with_parent_dir {
                writeln!(
                    MkdirOut,
                    "{}",
                    translate!("mkdir-verbose-created-directory", "util_name" => "mkdir", "path" => path.quote())
                )?;
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
