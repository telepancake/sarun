// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use clap::Command;
use std::ffi::OsString;
use std::io::Write;
use uucore::display::println_verbatim;
use uucore::error::{FromIo, UResult};
use uucore::translate;

mod platform;

/// Logical entry point for the in-process brush builtin.
///
/// `whoami` reads no stdin; it resolves the effective username and writes it
/// (verbatim, trailing newline) to the injected `out` sink — never the process's
/// global stdout. `err` is accepted for the engine's uniform `(args, out, err)`
/// entry shape; whoami emits no diagnostics of its own (failures are returned).
pub fn whoami_main(
    args: impl uucore::Args,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> UResult<()> {
    uucore::clap_localization::handle_clap_result(uu_app(), args)?;
    let username = whoami()?;
    write_username(out, &username).map_err_context(|| translate!("whoami-error-failed-to-print"))
}

/// Write `username` verbatim with a trailing newline, mirroring upstream's
/// `println_verbatim` but to the injected sink (Unix preserves the raw bytes).
fn write_username(out: &mut dyn Write, username: &OsString) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        out.write_all(username.as_os_str().as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        out.write_all(username.to_string_lossy().as_bytes())?;
    }
    out.write_all(b"\n")
}

#[uucore::main(no_signals)]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    uucore::clap_localization::handle_clap_result(uu_app(), args)?;
    let username = whoami()?;
    println_verbatim(username).map_err_context(|| translate!("whoami-error-failed-to-print"))?;
    Ok(())
}

/// Get the current username
pub fn whoami() -> UResult<OsString> {
    platform::get_username().map_err_context(|| translate!("whoami-error-failed-to-get"))
}

pub fn uu_app() -> Command {
    Command::new("whoami")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("whoami"))
        .about(translate!("whoami-about"))
        .override_usage(translate!("whoami-usage"))
        .infer_long_args(true)
}
