//! Thin bin shim — the CLI lives in the library (`wikimak_wikipedia::cli_main`,
//! behind `fetch`) so the sarun engine can embed it as a multi-call subcommand.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Restore the default SIGPIPE disposition. The Rust runtime installs
    // SIG_IGN, which turns a downstream `head`/`less` closing the pipe
    // into an EPIPE the print macros translate into a panic ("failed
    // printing to stdout: Broken pipe"). A CLI streaming `history`/`text`
    // must instead die quietly on the signal (exit 141), like every other
    // Unix filter. Done only in the standalone binary — the sarun engine
    // embeds `cli_main` in-process and must keep its own SIGPIPE handling.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(wikimak_wikipedia::cli_main(&args) as u8)
}
