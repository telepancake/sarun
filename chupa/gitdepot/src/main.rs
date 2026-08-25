//! Thin bin shim — the CLI lives in the library (`gitdepot::cli_main`) so
//! the sarun engine can embed it as a multi-call subcommand.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(gitdepot::cli_main(&args) as u8)
}
