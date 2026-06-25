//! Test driver: run a coreutils blob applet on the hand-written preview1 host,
//! with in-memory stdio. Usage:
//!   runblob <blob.wasm> <applet> [args...]      (stdin piped from this process)

use std::io::Read;

fn main() {
    let mut a = std::env::args().skip(1);
    let blob = a.next().expect("usage: runblob <blob.wasm> <applet> [args...]");
    let applet = a.next().expect("need an applet name (e.g. seq, tr, sort)");
    let rest: Vec<String> = a.collect();

    let wasm = std::fs::read(&blob).expect("read blob");
    // argv[0] = applet so the blob's busybox dispatch picks it.
    let mut argv: Vec<&str> = vec![applet.as_str()];
    argv.extend(rest.iter().map(|s| s.as_str()));

    let mut stdin = Vec::new();
    // Only drain stdin when it's not a TTY-ish empty; simplest: read whatever's piped.
    let _ = std::io::stdin().read_to_end(&mut stdin);

    match sarun_wasm_host::run_blob(&wasm, &argv, &[], stdin) {
        Ok(out) => {
            use std::io::Write;
            std::io::stdout().write_all(&out.stdout).unwrap();
            std::io::stderr().write_all(&out.stderr).unwrap();
            std::process::exit(out.code);
        }
        Err(e) => {
            eprintln!("runblob: {e}");
            std::process::exit(70);
        }
    }
}
