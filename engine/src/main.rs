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

pub use net::NetMode;

mod brush;
mod capture;
mod control;
mod discover;
mod frames;
mod katirun;
mod n2run;
mod net;
mod overlay;
mod paths;
mod pty;
mod review;
mod rules;
mod runner;
mod ui;
mod views;

// m2 `serve` mode: the control socket at the instance's namespaced path,
// speaking the Python ChannelServer's protocol (single-instance guard, ui
// verbs over on-disk box discovery, subscribe event feed). No boxes yet —
// register is refused politely; the overlay arrives at m3.
static SOCK_FOR_SIGNAL: std::sync::OnceLock<std::ffi::CString> =
    std::sync::OnceLock::new();
static MNT_FOR_SIGNAL: std::sync::OnceLock<std::ffi::CString> =
    std::sync::OnceLock::new();

extern "C" fn on_term(_sig: i32) {
    // async-signal-safe teardown: lazy-unmount our FUSE overlay (MNT_DETACH
    // lets the kernel finalize when the last reference drops, so the call
    // itself can't block on in-flight handlers), drop the socket, exit. The
    // detach prevents a "File exists (os error 17)" on the next startup —
    // without it, the dead mountpoint stays in the kernel's mount table and
    // `create_dir_all` on it returns EEXIST instead of "already a dir".
    if let Some(p) = MNT_FOR_SIGNAL.get() {
        unsafe { libc::umount2(p.as_ptr(), libc::MNT_DETACH); }
    }
    if let Some(p) = SOCK_FOR_SIGNAL.get() {
        unsafe { libc::unlink(p.as_ptr()) };
    }
    unsafe { libc::_exit(0) };
}

fn serve() -> i32 {
    let sock = paths::sock_path();
    // Single-instance guard FIRST, before any self-heal — a live socket
    // means another engine is up, and we must not touch its mountpoint.
    // Python uses the same semantics: a live socket means an instance is
    // running; a dead file is stale and replaced.
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        eprintln!("sarun-engine: an engine/UI is already running \
                   (control socket {}).", sock.display());
        return 4;
    }
    // No live engine — safe to clean up after a previous crashed one. A
    // stale FUSE mount at mnt_point() makes ensure_dirs() fail with
    // EEXIST: stat() on a dead-daemon FUSE mount returns ENOTCONN, so
    // create_dir_all() can't verify the path is a directory and the
    // error bubbles up. Path::exists() ALSO returns false on a dead
    // mount (same ENOTCONN), so we just try unconditionally —
    // fusermount3 silently no-ops when there's nothing to unmount.
    let mnt = paths::mnt_point();
    let _ = std::process::Command::new("fusermount3")
        .arg("-u").arg(&mnt)
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status();
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("sarun-engine: cannot create instance dirs: {e}");
        return 1;
    }
    let c = std::ffi::CString::new(sock.as_os_str().as_encoded_bytes()).unwrap();
    let _ = SOCK_FOR_SIGNAL.set(c);
    let mc = std::ffi::CString::new(mnt.as_os_str().as_encoded_bytes()).unwrap();
    let _ = MNT_FOR_SIGNAL.set(mc);
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
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
    // Engine-side networking registry. Lazily loaded — only `-n` boxes will
    // ever invoke it. Failure here (e.g. can't write the CA dir) is not
    // fatal: `-n` will refuse at register time, other modes work normally.
    match net::Net::new() {
        Ok(n) => state.lock().unwrap().net = Some(std::sync::Arc::new(n)),
        Err(e) => eprintln!("sarun-engine: net init failed (-n disabled): {e}"),
    }
    // One tokio runtime, multi-thread; long-lived. Dispatcher tasks (one
    // per accepted box-side connection) live on this. Leaked so the handle
    // stays valid for the engine's entire lifetime.
    let net_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().worker_threads(2).build()
        .map(|rt| { let h = rt.handle().clone(); Box::leak(Box::new(rt)); h });
    if let Ok(h) = net_rt { state.lock().unwrap().net_rt = Some(h); }
    println!("sarun-engine: listening · {}  ·  overlay {}",
             sock.display(), mnt.display());
    // Engine -> UI event broadcaster. Lives for the engine process's
    // lifetime. Drains ONE shared queue and COALESCES bursts so a
    // write-storm in one box doesn't flood every subscriber. The rules:
    //
    //   * Each tick (~100 ms) we drain the shared queue. Many events
    //     may have been pushed since the last tick.
    //   * Per (sid, kind) bucket — where kind is "overlay" or
    //     "process_added" — we send AT MOST one notification per
    //     COALESCE_WINDOW (250 ms). The notification carries the count
    //     of events that were folded into it so the UI knows it wasn't
    //     just one row that changed.
    //   * Any (sid, kind) we already broadcast THIS WINDOW from is
    //     simply tracked: we count what arrived but don't re-send.
    //   * For overlay events we send {rel} from the LAST event of the
    //     batch (newest path the producer touched) — the UI doesn't
    //     consume rel for anything beyond a debug status line; it
    //     re-fetches the relevant view anyway.
    //
    // No tick on the UI side; this just stops the engine from being
    // chatty per fs op. A `make` doing 50k writes lands as ~4-5
    // notifications per second per box, not 50k.
    const COALESCE_WINDOW: Duration = Duration::from_millis(250);
    {
        let ov = ov.clone();
        let state = state.clone();
        std::thread::spawn(move || {
            use std::collections::HashMap;
            // last broadcast wall-clock per (sid, kind) bucket
            let mut last_sent: HashMap<(i64, &'static str), std::time::Instant> = HashMap::new();
            // pending (sid, kind) → (count, last_rel)
            let mut pending: HashMap<(i64, &'static str), (u64, String)> = HashMap::new();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                for (sid, rel, op) in ov.drain_events() {
                    let kind: &'static str = if op == "process_added" {
                        "process_added"
                    } else { "overlay" };
                    let e = pending.entry((sid, kind))
                        .or_insert_with(|| (0, String::new()));
                    e.0 += 1;
                    if !rel.is_empty() { e.1 = rel; }
                }
                let now = std::time::Instant::now();
                pending.retain(|&(sid, kind), &mut (count, ref rel)| {
                    let allowed = last_sent.get(&(sid, kind))
                        .map(|t| now.duration_since(*t) >= COALESCE_WINDOW)
                        .unwrap_or(true);
                    if !allowed { return true; }   // still in cooldown — keep pending
                    let payload = if kind == "process_added" {
                        serde_json::json!({
                            "type": "process_added",
                            "sid": sid.to_string(),
                            "n": count,
                        })
                    } else {
                        serde_json::json!({
                            "type": "overlay",
                            "sid": sid.to_string(),
                            "rel": rel,
                            "n": count,
                        })
                    };
                    control::broadcast(&state, &payload);
                    last_sent.insert((sid, kind), now);
                    false  // drop from pending — flushed
                });
            }
        });
    }
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
        // working under the renamed `sarun` binary. The engine's stderr is
        // captured to engine.log under data_home — if startup fails (deadline
        // hit, missing fuse3, mount permission, etc.), the user has somewhere
        // to look instead of a silent timeout.
        let log_path = paths::data_home().join("engine.log");
        let _ = std::fs::create_dir_all(paths::data_home());
        let log = match std::fs::OpenOptions::new()
            .create(true).append(true).open(&log_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("sarun: cannot open {}: {e}", log_path.display());
                return 1;
            }
        };
        let log_err = log.try_clone().unwrap_or_else(|_| {
            std::fs::OpenOptions::new().create(true).append(true)
                .open(&log_path).expect("reopen engine.log")
        });
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("sarun: cannot locate own executable to spawn engine");
                return 1;
            }
        };
        let spawned = std::process::Command::new(&exe)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(log_err))
            .spawn();
        if let Err(e) = spawned {
            eprintln!("sarun: failed to spawn engine: {e}");
            return 1;
        }
        let mut proc = spawned.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                break;
            }
            // If the engine already exited, surface its log immediately
            // rather than waiting out the full deadline.
            if let Ok(Some(status)) = proc.try_wait() {
                eprintln!("sarun: engine exited before serving (status: {status}). \
                          Last log lines from {}:", log_path.display());
                tail_log(&log_path, 20);
                return 1;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("sarun: engine control socket never appeared at {} \
                          (20s deadline). Last log lines from {}:",
                          sock.display(), log_path.display());
                tail_log(&log_path, 20);
                return 1;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    ui::ui_main(args)
}

/// Print the tail of `path` to stderr, for surfacing engine startup
/// errors to the user without making them go fish for the log file.
fn tail_log(path: &std::path::Path, n_lines: usize) {
    let Ok(s) = std::fs::read_to_string(path) else {
        eprintln!("  (could not read {})", path.display());
        return;
    };
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n_lines);
    for l in &lines[start..] {
        eprintln!("  | {l}");
    }
}

fn main() {
    // D9 follow-on — brush-sh shim. When a -b box shadows /bin/sh (etc.) with
    // this engine binary, a nested `sh -c RECIPE` execs us under the original
    // program name. Detect that BEFORE normal subcommand dispatch (argv[0]'s
    // basename is a shell name AND SARUN_BRUSH_SH=1), emit the recipe's
    // provenance, then exec the REAL shell with the original argv unchanged.
    if brush::is_brush_sh_invocation() {
        let full: Vec<String> = std::env::args().collect();
        std::process::exit(brush::brush_sh(&full));
    }
    // Phase 1 — embedded ninja. A -b box shadows /bin/ninja with this engine
    // binary; when the box runs `ninja`, we land HERE (argv[0] basename ==
    // "ninja" && SARUN_BRUSH_SH=1) and run the vendored n2 in-process, executing
    // each recipe through embedded brush. Detected BEFORE normal dispatch.
    if n2run::is_ninja_invocation() {
        let full: Vec<String> = std::env::args().collect();
        std::process::exit(n2run::n2_main(&full));
    }
    // Phase 2 — embedded make. A -b box shadows make/gmake (and /usr/bin/make,
    // /bin/make) with this engine binary; when the box runs `make`, we land HERE
    // (argv[0] basename == "make"/"gmake" && SARUN_BRUSH_SH=1) and run vendored
    // kati in-process to PARSE the Makefile → ninja graph, then hand that graph
    // to the embedded n2 to EXECUTE (recipes through brush). Detected BEFORE
    // normal dispatch, like the ninja path.
    if katirun::is_make_invocation() {
        let full: Vec<String> = std::env::args().collect();
        std::process::exit(katirun::make_main(&full));
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Explicit `brush-sh -- <argv...>` subcommand for DIRECT testing of the shim
    // without the bwrap shadow binds: everything after `--` is the shell argv
    // (argv[0] = the shell name). The env stash vars still select the real shell.
    if argv.first().map(String::as_str) == Some("brush-sh") {
        let rest = &argv[1..];
        let sep = rest.iter().position(|a| a == "--");
        let shell_argv: Vec<String> = match sep {
            Some(i) => rest[i + 1..].to_vec(),
            None => rest.to_vec(),
        };
        std::process::exit(brush::brush_sh(&shell_argv));
    }
    match argv.first().map(String::as_str) {
        // Bare launch / explicit `attach` / `--once` headless render → UI role,
        // auto-spawning the engine when its socket is down.
        None => std::process::exit(ui_launch(&argv)),
        Some("attach") => std::process::exit(ui_launch(&argv[1..])),
        Some("--once") | Some("--sock") | Some("-h") | Some("--help") =>
            std::process::exit(ui_launch(&argv)),
        // `engine` is the headless-serve alias Python uses; `serve` still works.
        Some("engine") | Some("serve") => std::process::exit(serve()),
        // ruletest <rulesfile> <rel> <box> <exe> <cwd> [argv...] — test hook for
        // the clause-engine parity cross-check. Loads the rules file, builds the
        // Subject, and prints "<action|none> pt-read:<0|1>" (the full-grammar
        // decision + the D5 path-only-passthrough read gate).
        Some("ruletest") => {
            let a = &argv[1..];
            if a.len() < 5 {
                eprintln!("usage: ruletest <rulesfile> <rel> <box> <exe> <cwd> [argv...]");
                std::process::exit(2);
            }
            let text = std::fs::read_to_string(&a[0]).unwrap_or_default();
            let rules = rules::Rules::parse(&text);
            let rel = &a[1];
            let subject = rules::Subject {
                box_name: a[2].clone(), exe: a[3].clone(), cwd: a[4].clone(),
                argv: a[5..].to_vec(),
            };
            let act = match rules.decide(rel, &subject) {
                Some(rules::Action::Apply) => "apply",
                Some(rules::Action::Discard) => "discard",
                Some(rules::Action::Passthrough) => "passthrough",
                None => "none",
            };
            let pt_read = rules.passthrough_path_only(rel) as u8;
            println!("{act} pt-read:{pt_read}");
            std::process::exit(0);
        }
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
            let mut pty = false;
            let mut brush = false;
            let mut chdir: Option<String> = None;
            let mut name: Option<String> = None;
            let mut net_mode = NetMode::Off;
            let mut it = pre.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-t" => passthrough = true,
                    "-d" => direct = true,
                    "-e" => env = true,
                    // -p  PTY: run the box on an interactive pseudo-terminal
                    //     (real tty inside the box), captured like ordinary
                    //     capture mode. Implies capture-on; ignored under -d.
                    "-p" => pty = true,
                    // -b  brush: run the box's command THROUGH the embedded
                    //     brush shell (brush-core/brush-parser) instead of
                    //     /bin/sh, emitting SEMANTIC-PROVENANCE frames. An
                    //     EXPLICIT toggle — no silent fallback to /bin/sh (D9).
                    //     Implies capture-on (so provenance + writes are
                    //     recorded), except under -d which has no overlay.
                    "-b" => brush = true,
                    "-C" => chdir = it.next().cloned(),
                    // -n  network: per-box netns with a TAP whose other end
                    //     terminates at the engine's userland TCP/IP stack
                    //     (DHCP, DNS, MITM proxy). Outbound from box's POV is
                    //     ordinary; engine originates the real upstream sockets
                    //     in the host netns. See engine/src/net/mod.rs.
                    // -N  no netns: keep the host network namespace (the
                    //     pre-default-empty behavior; useful for boxes that
                    //     need to dial localhost services or your VPN).
                    "-n" => net_mode = NetMode::Tap,
                    "-N" => net_mode = NetMode::Host,
                    _ => if name.is_none() { name = Some(a.clone()); },
                }
            }
            std::process::exit(runner::run(name, passthrough, direct, env, pty, brush, chdir, net_mode, cmd));
        }
        Some("inner") => {
            // inner --conn-fd N -- CMD...
            let rest = &argv[1..];
            let mut conn_fd = -1;
            let mut capture = false;
            let mut pty = false;
            let mut brush = false;
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "--conn-fd" && i + 1 < rest.len() {
                    conn_fd = rest[i + 1].parse().unwrap_or(-1); i += 2;
                } else if rest[i] == "--capture" { capture = true; i += 1; }
                else if rest[i] == "--pty" { pty = true; i += 1; }
                else if rest[i] == "--brush" { brush = true; i += 1; }
                else if rest[i] == "--" { i += 1; break; }
                else { i += 1; }
            }
            std::process::exit(runner::inner(conn_fd, capture, pty, brush, rest[i..].to_vec()));
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
