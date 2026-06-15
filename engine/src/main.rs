// sarun-engine — milestone 1: a multithreaded read-only passthrough FUSE
// filesystem over a lower root. The point of this milestone is NOT features —
// it exists to measure the serving-loop scaling that the Python engine's
// single GIL thread cannot deliver (see bench/FINDINGS.md "parallel builds").
//
//   sarun-engine <mountpoint> [--lower /] [--threads N]
//
// Serves lookup/getattr/readdir(plus)/readlink/open/read, nothing else; every
// answer comes straight from the lower tree (no overlay, no capture yet).

use std::path::PathBuf;
use std::time::Duration;

use fuser::Config;
use fuser::MountOption;

mod capture;
mod control;
mod discover;
mod frames;
mod overlay;
mod paths;
mod review;
mod rules;
mod runner;
mod ui;

// m2 `serve` mode: the control socket at the instance's namespaced path,
// speaking the Python ChannelServer's protocol (single-instance guard, ui
// verbs over on-disk box discovery, subscribe event feed). No boxes yet —
// register is refused politely; the overlay arrives at m3.
static SOCK_FOR_SIGNAL: std::sync::OnceLock<std::ffi::CString> =
    std::sync::OnceLock::new();

extern "C" fn on_term(_sig: i32) {
    // async-signal-safe teardown: drop the socket, exit clean.
    if let Some(p) = SOCK_FOR_SIGNAL.get() {
        unsafe { libc::unlink(p.as_ptr()) };
    }
    unsafe { libc::_exit(0) };
}

fn serve() -> i32 {
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("sarun-engine: cannot create instance dirs: {e}");
        return 1;
    }
    let sock = paths::sock_path();
    // Single-instance guard, same semantics as the Python engine/UI: a live
    // socket means an instance is running; a dead file is stale and replaced.
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        eprintln!("sarun-engine: an engine/UI is already running \
                   (control socket {}).", sock.display());
        return 4;
    }
    let c = std::ffi::CString::new(sock.as_os_str().as_encoded_bytes()).unwrap();
    let _ = SOCK_FOR_SIGNAL.set(c);
    unsafe {
        libc::signal(libc::SIGTERM, on_term as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as libc::sighandler_t);
    }
    // Mount the multi-box overlay at the instance mountpoint (threads = cores).
    let mnt = paths::mnt_point();
    let ov = overlay::Overlay::new(PathBuf::from("/"));
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::FSName("sarun-rs".into())];
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cfg.n_threads = Some(n);
    cfg.clone_fd = n > 1;
    let session = match fuser::spawn_mount2(ov.clone(),
                                            &mnt, &cfg) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("sarun-engine: overlay mount FAILED: {e} — boxes cannot run");
            None
        }
    };
    let state: control::State = Default::default();
    state.lock().unwrap().overlay = Some(ov.clone());
    println!("sarun-engine: listening · {}  ·  overlay {}",
             sock.display(), mnt.display());
    let rc = match control::serve(state, &sock) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sarun-engine: serve failed: {e}");
            1
        }
    };
    drop(session); // unmount
    rc
}

/// Launch the UI, auto-spawning a detached engine (`serve`) first if the
/// control socket isn't already up. Mirrors Python's bare-`slopbox`/`attach`.
fn ui_launch(args: &[String]) -> i32 {
    let sock = paths::sock_path();
    if std::os::unix::net::UnixStream::connect(&sock).is_err() {
        // No engine running — spawn one detached and wait (bounded) for the
        // control socket to appear. current_exe() is a path, so it keeps
        // working under the renamed `sarun` binary.
        if let Ok(exe) = std::env::current_exe() {
            let spawned = std::process::Command::new(exe)
                .arg("serve")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Err(e) = spawned {
                eprintln!("sarun: failed to spawn engine: {e}");
                return 1;
            }
        } else {
            eprintln!("sarun: cannot locate own executable to spawn engine");
            return 1;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("sarun: engine control socket never appeared at {}",
                          sock.display());
                return 1;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    ui::ui_main(args)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        // Bare launch / explicit `attach` / `--once` headless render → UI role,
        // auto-spawning the engine when its socket is down.
        None => std::process::exit(ui_launch(&argv)),
        Some("attach") => std::process::exit(ui_launch(&argv[1..])),
        Some("--once") | Some("--sock") | Some("-h") | Some("--help") =>
            std::process::exit(ui_launch(&argv)),
        // `engine` is the headless-serve alias Python uses; `serve` still works.
        Some("engine") | Some("serve") => std::process::exit(serve()),
        Some("run") => {
            // run [-t] [-d] [-e] [NAME] -- CMD...
            //   -t  passthrough: no stdout/stderr capture (inner just execs)
            //   -d  direct: no overlay — writes land on the real host, uncaptured
            //   -e  env: record each writer's full environment
            let rest = &argv[1..];
            let sep = rest.iter().position(|a| a == "--");
            let (pre, cmd) = match sep {
                Some(i) => (&rest[..i], rest[i + 1..].to_vec()),
                None => (rest, vec![]),
            };
            let mut passthrough = false;
            let mut direct = false;
            let mut env = false;
            let mut chdir: Option<String> = None;
            let mut name: Option<String> = None;
            let mut it = pre.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-t" => passthrough = true,
                    "-d" => direct = true,
                    "-e" => env = true,
                    "-C" => chdir = it.next().cloned(),
                    _ => if name.is_none() { name = Some(a.clone()); },
                }
            }
            std::process::exit(runner::run(name, passthrough, direct, env, chdir, cmd));
        }
        Some("inner") => {
            // inner --conn-fd N -- CMD...
            let rest = &argv[1..];
            let mut conn_fd = -1;
            let mut capture = false;
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "--conn-fd" && i + 1 < rest.len() {
                    conn_fd = rest[i + 1].parse().unwrap_or(-1); i += 2;
                } else if rest[i] == "--capture" { capture = true; i += 1; }
                else if rest[i] == "--" { i += 1; break; }
                else { i += 1; }
            }
            std::process::exit(runner::inner(conn_fd, capture, rest[i..].to_vec()));
        }
        // CLI conveniences mirroring `slopbox NAME <op>`: a leading all-caps
        // (optionally dotted) box NAME selects it, and an optional op acts on it
        // over the control socket (the verbs already exist engine-side).
        Some(first) if control::is_box_name(first) => {
            std::process::exit(control::cli_box_op(&argv));
        }
        // Any other first token is neither a known subcommand nor a box NAME →
        // launch the UI (Python's bare-slopbox behavior). The old m1 passthrough
        // `MOUNTPOINT --lower --threads` dev-tool fallthrough is retired.
        _ => std::process::exit(ui_launch(&argv)),
    }
}
