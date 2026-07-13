// sarun-engine — the standalone Rust engine + UI (see engine/DESIGN.md). One
// static musl binary that is the whole product: a multithreaded FUSE overlay
// with copy-on-write capture, the control socket + subscribe feed, per-box
// `-n` networking (TAP + userland TCP/IP + MITM proxy), OCI, the oaita agent
// runner, engine-held PTYs, and its own ratatui UI.
//
// The subcommands are dispatched below in `main` — the big ones:
//   (no args)                 start the engine (if needed) + interactive UI
//   run [FLAGS] [NAME] -- CMD  run CMD in a captured box (see runner.rs)
//   oci  load|run|build|…      OCI images + containers (see oci.rs)
//   oaita gen|run|call|…       the agent runner (see oaita/)
//   serve                      headless engine (the FUSE + control loop)
// The bare `<mountpoint>` FUSE-passthrough mode below is the original m1
// benchmark harness, kept for the serving-loop scaling measurement it exists
// for (bench/FINDINGS.md "parallel builds"), not the product entry point.

use std::path::PathBuf;
use std::time::Duration;

use fuser::Config;
use fuser::MountOption;

pub use net::NetMode;

mod brush;
mod builtin_exec;
mod capture;
mod exec_wrappers;
mod find_builtin;
mod xargs_builtin;
mod containers_conf;
mod control;
mod depot;
mod discover;
// Dockerfile/Containerfile parser for `sarun oci build` / `oci run`. Lands
// ahead of its consumer (the build driver), so allow the not-yet-used items.
#[allow(dead_code)]
mod dockerfile;
mod editor;
mod frames;
mod hostfs;
mod jobserver;
mod katirun;
mod attach;
mod browser;
mod mirrors;
mod n2run;
mod net;
mod oaita;
mod oci;
mod oci_verify;
mod overlay;
mod paths;
mod pty;
mod reader;
mod registry;
mod parser;
mod review;
mod rules;
mod runner;
mod selfbt;
mod sixel;
mod slippool;
mod sud;
mod sudir;
mod sudwire;
mod ui;
mod views;
mod wacz;

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

fn top_level_help() -> &'static str {
    "sarun — run a command over a copy-on-write overlay of your filesystem,\n\
     capturing its writes, processes, and output for review before they touch the host.\n\
     \n\
     usage:\n  \
       sarun                              start the engine (if needed) + interactive UI\n  \
       sarun attach [--sock PATH]         interactive UI against a running engine\n  \
       sarun serve                        run the engine headless (no UI)\n  \
       sarun run [FLAGS] [NAME] -- CMD    run CMD in a sandbox box (needs a running engine/UI)\n  \
       sarun <NAME> [apply|discard|rename NEW|patch|stuck]  operate on a box from the CLI\n  \
       sarun <NAME> checkout STORE REF [DEST] [SUB]    check a commit out of a git mirror into the box\n  \
       sarun <NAME> attach wiki|ietf SRC REF [AT]      attach a mirror object as a read-only reference\n  \
       sarun mirror <ls|add|run|pause|resume|rm> ...   scheduled mirror updates\n  \
       sarun gitdepot|wikimak|ietfmak ...              embedded mirror-driver CLIs\n  \
       sarun verbs [FILTER]               list the engine's UI verbs (args + help)\n  \
       sarun oci <load|run|build|save|dockerfile|author> ...   OCI images (`sarun oci -h`)\n  \
       sarun oaita <gen|run|call|tail|add|where> NAME          LLM chat/agent runner (`oaita -h`)\n  \
       sarun web <export-wacz|import-wacz> ...                 WACZ web-archive interop\n  \
       sarun browser [FLAGS] URL        engine-native textmode browser (headless Chromium)\n  \
       sarun engine                     alias for serve (headless)\n  \
       sarun --once --sock PATH           render one UI frame and exit (headless)\n\
     \n\
     run FLAGS:\n  \
       -n / -N / --net off|tap|host   per-box networking (default: tap, a proxied per-box netns)\n  \
       -t passthrough   -d direct (no overlay)   -e record-env   -b brush-shell   -p pty\n  \
       -C DIR   --no-parent   --readonly-parent   --api (oaita proxy)   --vars (variable provenance)\n"
}

fn serve() -> i32 {
    // rustls 0.23 requires an explicit process-level CryptoProvider when more
    // than one is in the dependency graph — ours has both `ring` (our direct
    // rustls/tokio-rustls features) and `aws-lc-rs` (pulled by oci-client's
    // rustls-tls). Auto-detection then refuses to choose, so the net stack's
    // first TLS config build (the Tap MITM ServerConfig / upstream ClientConfig
    // at netns-equip time) panics the net thread and every `-n`/Tap box hangs
    // waiting on a register ack that never comes. Pin `ring` here, once, before
    // any box networking starts. Idempotent: a redundant install is harmless.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sock = paths::sock_path();
    // Single-instance guard FIRST, before any self-heal — we must not touch
    // another live engine's mountpoint/socket. Audit M2: the old guard was
    // TOCTOU (`connect(sock).is_ok()` probe, then a much-later remove+bind in
    // control::serve), so two engines launched together could both pass the
    // probe and race the bind. Take an exclusive advisory `flock` on a lock
    // file beside the socket instead — the kernel grants it to exactly one
    // process and auto-releases it on exit (even a crash), so a stale lock
    // never survives a dead daemon. KEEP `_instance_lock` bound for the whole
    // serve() scope; dropping it would release the lock.
    let _instance_lock = match control::acquire_instance_lock(&sock) {
        Ok(control::InstanceLock::Held(fd)) => fd,
        Ok(control::InstanceLock::AlreadyRunning) => {
            eprintln!("sarun-engine: an engine/UI is already running \
                       (control socket {}).", sock.display());
            return 4;
        }
        Err(e) => {
            eprintln!("sarun-engine: cannot take instance lock: {e}");
            return 1;
        }
    };
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
    // Bind the control socket NOW, under the instance lock and before the FUSE
    // mount / the rest of init. This is the fix for the startup race: the socket
    // file must only appear once it is a live listening socket, so a client that
    // connects during startup queues in the backlog (served when the accept loop
    // runs) instead of racing a stale socket left by a dead daemon. Held until
    // control::serve consumes it below.
    let listener = match control::bind_listener(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sarun-engine: cannot bind control socket {}: {e}", sock.display());
            return 1;
        }
    };
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
    control::install_state_handle(state.clone());
    // Engine-side networking registry. Lazily loaded — only `-n` boxes will
    // ever invoke it. Failure here (e.g. can't write the CA dir) is not
    // fatal: `-n` will refuse at register time, other modes work normally.
    match net::Net::new() {
        Ok(n) => {
            // Pre-write the host-side files the FUSE overlay shadows
            // into `--api` boxes: the augmented CA bundle (host system
            // bundle + engine's MITM CA root) and a synthetic
            // /etc/resolv.conf pointing at the engine's per-box stack
            // gateway. Same shape as the safe-oaita.toml shadow below —
            // the box reads canonical paths and the overlay serves
            // engine-controlled content, no bwrap binds, no on-disk
            // writes by the runner (which for a nested --api box would
            // land in the parent box's overlay as
            // `.tmp/N/sarun-ca-*.pem` noise).
            if let Err(e) = control::write_api_box_net_shadows(&n) {
                eprintln!("sarun-engine: api-box net shadow files: {e}");
            }
            state.lock().unwrap().net = Some(std::sync::Arc::new(n));
        }
        Err(e) => eprintln!("sarun-engine: net init failed (-n disabled): {e}"),
    }
    // One tokio runtime, multi-thread; long-lived. Dispatcher tasks (one
    // per accepted box-side connection) live on this. Leaked so the handle
    // stays valid for the engine's entire lifetime.
    let net_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().worker_threads(2).build()
        .map(|rt| { let h = rt.handle().clone(); Box::leak(Box::new(rt)); h });
    if let Ok(h) = net_rt { state.lock().unwrap().net_rt = Some(h); }
    // oaita API proxy: now lives as FRAME_API_OPEN/DATA/CLOSE on the
    // existing box-channel. The in-box runner serves /run/sarun/api.sock
    // inside the box and tunnels each accepted connection as logical
    // streams over the same UDS its register handshake rides on. The
    // Proxy registry on shared
    // state still tracks --api-enabled boxes, holds the upstream config,
    // and logs into each box's api_log sqlar table. No second host UDS;
    // nothing to bind-mount; nested-act delegations work because the
    // control socket bind-mount is already wired by every register.
    let proxy = std::sync::Arc::new(oaita::proxy::Proxy::new());
    proxy.set_overlay(ov.clone());
    state.lock().unwrap().api_proxy = Some(proxy.clone());
    // Safe-for-box oaita.toml: model name from the HOST config, no api_key,
    // and a base_url that's a marker (the in-box client only uses the URL
    // as a fallback when OAITA_API_SOCK is unset; for --api boxes the env
    // var wins and points at the bind-mounted ui.sock). Refreshed again as
    // each --api box registers (control.rs), so a config written AFTER
    // startup — e.g. `oaita local` — reaches the box.
    control::write_api_box_oaita_toml();
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
    // Mirror-update scheduler: a minute tick starting whatever jobs are
    // due (mirrors.db). No jobs → pure no-op loop.
    mirrors::scheduler_thread();
    let rc = match control::serve(state, listener) {
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

/// Multi-call dispatch for the embedded mirror drivers: run the driver CLI
/// when argv[0]'s basename IS a driver name (symlink launch) or when the
/// first argument names one (`sarun gitdepot …`). Returns the exit code to
/// use, or None when this isn't a driver invocation.
fn driver_invocation() -> Option<i32> {
    fn run(name: &str, args: &[String]) -> Option<i32> {
        match name {
            "gitdepot" => Some(gitdepot::cli_main(args)),
            "wikimak" => Some(wikimak_wikipedia::cli_main(args)),
            "ietfmak" => Some(ietf_mirror::cli_main(args)),
            _ => None,
        }
    }
    let argv: Vec<String> = std::env::args().collect();
    let arg0 = argv.first().map(String::as_str).unwrap_or("");
    let base = std::path::Path::new(arg0)
        .file_name().and_then(|s| s.to_str()).unwrap_or("");
    if let Some(code) = run(base, &argv[1..]) {
        return Some(code);
    }
    argv.get(1).and_then(|sub| run(sub, &argv[2..]))
}

fn main() {
    // Symlinked-as-`oaita` dispatch — same trick brush_sh / ninja / make use
    // below: when this engine binary is invoked under the name `oaita` (a
    // symlink to it), route straight to the oaita CLI. Detected BEFORE normal
    // subcommand dispatch so the top-level `sarun` argument grammar doesn't
    // try to parse oaita's. Inside a sarun-launched box the runner exports
    // PATH so that `oaita` resolves to the engine binary itself.
    if oaita::is_oaita_invocation() {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        std::process::exit(oaita::cli::main(&argv));
    }
    // Mirror drivers — same multi-call trick: `gitdepot` / `wikimak` /
    // `ietfmak` are compiled into this binary (mirrors.rs re-execs it with
    // the driver name, so the ENGINE process never dials out — fetch runs
    // in the child). An argv[0] symlink named after a driver works too.
    if let Some(code) = driver_invocation() {
        std::process::exit(code);
    }
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
        // `sarun oaita …` — the second entry into the oaita duty (symlinked
        // `oaita` binary is the first, handled above). Equivalent code path.
        Some("oaita") => std::process::exit(oaita::cli::main(&argv[1..])),
        // `sarun browser [--dump|--dump-text] [--size WxH] URL` — the
        // engine-native textmode browser (DESIGN-cellulose.md), driving a
        // stock headless Chromium over CDP. Replaces the carbonyl launcher.
        Some("browser") => std::process::exit(browser::launch::browser_cli(&argv[1..])),
        // Bare launch / explicit `attach` / `--once` headless render → UI role,
        // auto-spawning the engine when its socket is down.
        None => std::process::exit(ui_launch(&argv)),
        Some("attach") => std::process::exit(ui_launch(&argv[1..])),
        Some("--once") | Some("--sock") =>
            std::process::exit(ui_launch(&argv)),
        Some("-h") | Some("--help") => { print!("{}", top_level_help()); std::process::exit(0); }
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
                // Ask is a net-rule action; on the file-decision path
                // treat it as a no-decision so the caller's default (or
                // a later matching rule) takes over.
                Some(rules::Action::Ask) => "none",
                None => "none",
            };
            let pt_read = rules.passthrough_path_only(rel) as u8;
            println!("{act} pt-read:{pt_read}");
            std::process::exit(0);
        }
        Some("run") => {
            // run [-t] [-d] [-e] [--no-parent] [--readonly-parent] [NAME] -- CMD...
            //   -t                  passthrough: no stdout/stderr capture (inner just execs)
            //   -d                  direct: no overlay — writes land on the real host, uncaptured
            //   -e                  env: record each writer's full environment
            //   --no-parent         strip kernel-derived parent AND close the lower chain at
            //                       this box (no host / bleed-through); the box's own
            //                       contents are its entire filesystem
            //   --readonly-parent   `apply` refuses to promote into the parent
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
            let mut no_parent = false;
            let mut readonly_parent = false;
            let mut api = false;
            let mut sud = false;
            let mut chdir: Option<String> = None;
            let mut name: Option<String> = None;
            // Box networking defaults to Tap (proxied): the box gets a per-box
            // netns wired to the engine's in-process TCP/IP stack. Opt out with
            // `--net off` (air-gapped, fail-closed) or `-N`/`--net host` (raw
            // host connectivity). See engine/src/net/mod.rs.
            let mut net_mode = NetMode::Tap;
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
                    "--no-parent" => no_parent = true,
                    "--readonly-parent" => readonly_parent = true,
                    "-C" => chdir = it.next().cloned(),
                    // Box networking (default Tap — see net_mode init):
                    // -n   Tap   per-box netns with a TAP whose other end
                    //            terminates at the engine's userland TCP/IP
                    //            stack (DHCP, DNS, HTTPS MITM w/ CA injection,
                    //            per-flow policy). Outbound from the box's POV
                    //            is ordinary; the engine originates the real
                    //            upstream sockets in the host netns. Now the
                    //            default; -n is its explicit spelling.
                    // -N   Host  keep the host network namespace (raw
                    //            connectivity — localhost services, your VPN).
                    // --net off|tap|host  canonical selector. `off` is an
                    //            empty netns where getaddrinfo and every dial
                    //            fail closed. See engine/src/net/mod.rs.
                    "-n" => net_mode = NetMode::Tap,
                    "-N" => net_mode = NetMode::Host,
                    "--net" => match it.next().map(String::as_str) {
                        Some(m) => match NetMode::parse(m) {
                            Some(nm) => net_mode = nm,
                            None => {
                                eprintln!("sarun: --net wants off|tap|host, \
                                           got '{m}'");
                                std::process::exit(2);
                            }
                        },
                        None => {
                            eprintln!("sarun: --net needs an argument \
                                       (off|tap|host)");
                            std::process::exit(2);
                        }
                    },
                    // --api  enable the oaita API proxy for this box. The
                    //        inner runner serves /run/sarun/api.sock inside
                    //        the box and tunnels each accepted connection
                    //        as FRAME_API_* on the box channel — so an
                    //        in-box `oaita gen` routes through the engine
                    //        with no api key in the box and no extra UDS.
                    "--api" => api = true,
                    // --sud  EXPERIMENTAL (engine/DESIGN-sud.md, WIP): run
                    //        CMD under tv's sudtrace (Syscall User Dispatch
                    //        + userland overlay) instead of bwrap+FUSE; a
                    //        post-exit sweep captures the upper dir into
                    //        the box's sqlar. Host netns, no capture mux,
                    //        incompatible with -t/-d/-p/-b/--api.
                    "--sud" => sud = true,
                    // --webcap  OPT-IN web capture (DESIGN-web.md W2): tee
                    //           every HTTP(S) request/response this tap box
                    //           makes into its `webcap` table. An env toggle
                    //           (like --vars) so it reaches the register
                    //           message without threading a new bool through
                    //           runner::run's signature. tap-only (gated in
                    //           the register message).
                    "--webcap" => unsafe {
                        std::env::set_var("SARUN_WEBCAP", "1");
                    },
                    // --webfilter  OPT-IN proxy-side filtering (DESIGN-web.md
                    //              W7): adblock + response rewrite from
                    //              {config_home}/webfilter, in the engine,
                    //              outside the browser. tap-only.
                    "--webfilter" => unsafe {
                        std::env::set_var("SARUN_WEBFILTER", "1");
                    },
                    // --vars  OPT-IN variable provenance: record every make
                    //         and shell variable assignment (name, site,
                    //         value, unexpanded rhs + its dereferences) into
                    //         the box's makevar table for the UI's Vars view.
                    //         An env toggle because the recorders run in the
                    //         box's shadowed makes/shells, which inherit the
                    //         box environment through bwrap.
                    // Single-threaded here (argv parsing, pre-spawn) — safe.
                    "--vars" => unsafe {
                        std::env::set_var("SARUN_TRACE_VARS", "1");
                    },
                    // --fuse  the explicit spelling of the default backend
                    //         (bwrap + FUSE overlay) — the counterpart of
                    //         --sud. Accepted so scripts can pin the backend;
                    //         selects nothing new.
                    "--fuse" => sud = false,
                    // A bare word is the box NAME. A dash-word is a TYPO or
                    // an unknown flag — refuse it loudly: silently taking it
                    // as the NAME named every `sarun run --fuse …` box
                    // "--fuse", and the SECOND parallel run then collided
                    // with the first ("slopbox is already running").
                    a if a.starts_with('-') => {
                        eprintln!("sarun: unknown flag '{a}' for run \
                                   (a box NAME cannot start with '-')");
                        std::process::exit(2);
                    }
                    _ => if name.is_none() { name = Some(a.clone()); },
                }
            }
            if cmd.is_empty() && !api {
                eprintln!("usage: sarun run [FLAGS] [NAME] -- CMD...   (needs a running engine/UI)\n\
                    \x20 flags: -n/-N/--net off|tap|host  -t passthrough  -d direct  -e record-env\n\
                    \x20        -b brush-shell  -p pty  -C DIR  --no-parent  --readonly-parent  --api\n\
                    \x20        --vars record variable assignments (Vars view)");
                std::process::exit(2);
            }
            if sud {
                if passthrough || direct || pty || api {
                    eprintln!("sarun: --sud is incompatible with \
                               -t/-d/-p/--api (step-1 scope, see \
                               engine/DESIGN-sud.md)");
                    std::process::exit(2);
                }
                std::process::exit(
                    runner::run_sud(name, env, chdir, net_mode, brush, cmd));
            }
            std::process::exit(runner::run(name, passthrough, direct, env,
                pty, brush, api, no_parent, readonly_parent, chdir,
                net_mode, cmd));
        }
        Some("verbs") => {
            // sarun verbs [FILTER] — the engine's UI-verb surface, from the
            // running engine's own table (control::VERB_DOCS via the "verbs"
            // verb). Works in-box too (SARUN_BROKER channel).
            std::process::exit(control::cli_verbs(&argv[1..]));
        }
        Some("mirror") => {
            // sarun mirror ls|add|run|pause|resume|rm — mirror-update jobs.
            std::process::exit(control::cli_mirror(&argv[1..]));
        }
        Some("oci") => {
            // sarun oci load <ref> [NAME]  → populate a chain of at-rest sarun
            // boxes from an OCI image. See engine/src/oci.rs.
            std::process::exit(oci::cli_oci(&argv[1..]));
        }
        Some("web") => {
            // sarun web export-wacz <box> <out> | import-wacz <in> [NAME] —
            // WACZ interop for the web archive (see engine/src/wacz.rs).
            std::process::exit(wacz::cli(&argv[1..]));
        }
        Some("inner") => {
            // inner --conn-fd N -- CMD...
            let rest = &argv[1..];
            let mut conn_fd = -1;
            let mut capture = false;
            let mut pty = false;
            let mut brush = false;
            let mut api = false;
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "--conn-fd" && i + 1 < rest.len() {
                    conn_fd = rest[i + 1].parse().unwrap_or(-1); i += 2;
                } else if rest[i] == "--capture" { capture = true; i += 1; }
                else if rest[i] == "--pty" { pty = true; i += 1; }
                else if rest[i] == "--brush" { brush = true; i += 1; }
                else if rest[i] == "--api" { api = true; i += 1; }
                else if rest[i] == "--" { i += 1; break; }
                else { i += 1; }
            }
            std::process::exit(runner::inner(conn_fd, capture, pty, brush, api, rest[i..].to_vec()));
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
