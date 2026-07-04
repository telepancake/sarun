// The box runner, ported to Rust (the ECHO/capture mux is a follow-on).
// Supports NESTED launch: a `run` invoked inside a running box reaches the
// engine by dialing the FD broker (SARUN_BROKER abstract UDS, served by the
// parent inner), which hands back a fresh engine conn via SCM_RIGHTS; it
// registers with a `relname` + its own pidfd (so the engine derives the
// enclosing box from the /proc ancestry), and roots bwrap on the
// parent-exposed /<KIDS_DIR>/<id>.
// Two subcommands of the sarun-engine binary:
//   run [NAME] -- CMD   host side: register the box with the engine, then bwrap
//                       CMD onto the box's overlay root, exec'ing `inner`.
//   inner --conn-fd N -- CMD   in-box pid-1-ish shim: holds the box channel fd
//                       (its EOF on exit is the engine's teardown signal) and
//                       execs CMD. Capture/echo will hang off this seam later.
// This makes a box fully Rust end to end — no Python in the runtime path.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

use serde_json::Value;
use serde_json::json;

use crate::paths;

const KIDS_DIR: &str = ".slopbox-kids";

/// Standard CA bundle paths the augmented bundle is bound over. Distro
/// coverage as of 2026: Debian/Ubuntu (ca-certificates.crt), RHEL/Fedora
/// (tls/certs/ca-bundle.crt + the .pem twin), Alpine (cert.pem). `--ro-bind-try`
/// silently skips paths the box's filesystem doesn't ship.
pub const CA_BUNDLE_TARGETS: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/pki/tls/certs/ca-bundle.trust.crt",
    "/etc/ssl/cert.pem",
    "/etc/ssl/ca-bundle.pem",
    "/var/lib/ca-certificates/ca-bundle.pem",
];

fn pidfd_open(pid: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }
}

// Note: shadowing is now done LAZILY inside the FUSE overlay's
// lookup/open path. No filesystem walk from this module. See
// overlay.rs::shadows for the actual matching, and
// paths::shadow_*_glob_path() for the config files that drive it.

/// pidfd_open, exposed for the brush module (same capture/MUTE wiring).
pub fn pidfd_open_pub(pid: i32) -> i32 { pidfd_open(pid) }

/// D9 nested-shell provenance: the brush-sh shim (see brush::brush_sh) sends ONE
/// newline-terminated JSON line to the engine, carrying ITS OWN pidfd as
/// SCM_RIGHTS so the engine resolves the enclosing box from the shim's
/// /proc ancestry (the same identity path register uses). This is a one-
/// shot control message — NOT a register, NOT a box channel: the engine
/// records the recipe's brushprov rows and closes. The conn is acquired
/// via the FD broker (`SARUN_BROKER` — bound by our parent inner). Best-
/// effort: any failure (no broker, send error) is swallowed so the recipe
/// still runs unchanged. `line` must already be newline-terminated.
pub fn send_nested_prov(line: &[u8]) {
    let Ok(name) = std::env::var("SARUN_BROKER") else { return; };
    let Ok(conn) = broker_dial(&name) else { return; };
    let pidfd = pidfd_open(std::process::id() as i32);
    send_register(&conn, line, pidfd, None);
    if pidfd >= 0 { unsafe { libc::close(pidfd); } }
    // Drain the engine's one-line ack (best-effort) so it isn't an abrupt RST.
    let mut s = String::new();
    let _ = BufReader::new(&conn).read_line(&mut s);
}
/// send_frame, exposed for the brush module's MUTE/UNMUTE/teardown frames.
pub fn send_frame_pub(conn_fd: i32, frame: &[u8], pidfd: Option<i32>) {
    send_frame(conn_fd, frame, pidfd)
}

/// recv_box_frame_bytes, exposed for the brush + pty readers so their
/// channel pump can pick up an SCM_RIGHTS-attached fd from FRAME_CONN.
pub fn recv_box_frame_bytes_pub(raw: i32, buf: &mut [u8],
                                fd: &mut Option<i32>) -> isize {
    recv_box_frame_bytes(raw, buf, fd)
}

/// runner_broker_handoff, exposed for the brush + pty readers so they
/// can complete the FD broker handshake.
pub fn runner_broker_handoff_pub(fd: i32) { runner_broker_handoff(fd) }

/// Send one register line plus our own pidfd as SCM_RIGHTS ancillary data, so
/// the engine derives our HOST-namespace pid from /proc/self/fdinfo/<pidfd>
/// (the wrap-immune identity path) — correct for both top-level and nested
/// runners, where our own getpid() is a parent-namespace pid the engine can't
/// use. Returns false on write error.
fn send_register(conn: &UnixStream, line: &[u8], pidfd: i32, tap_fd: Option<i32>) -> bool {
    send_register_fds(conn, line, pidfd, tap_fd, None)
}

/// Like send_register but with an ORDERED extra-fd tail after the pidfd:
/// fd[0] = pidfd, then the TAP fd (tap boxes), then the sud trace-pipe fd
/// (sud boxes). The engine (recv_first_fd + register) assigns roles from the
/// same want_sud/net_mode it reads out of `line`, so the order must match:
/// [pidfd, tap?, trace?].
fn send_register_fds(conn: &UnixStream, line: &[u8], pidfd: i32,
                     tap_fd: Option<i32>, trace_fd: Option<i32>) -> bool {
    let mut fds: Vec<i32> = Vec::with_capacity(3);
    if pidfd >= 0 { fds.push(pidfd); }
    if let Some(t) = tap_fd { fds.push(t); }
    if let Some(t) = trace_fd { fds.push(t); }
    if fds.is_empty() {
        return conn_write_all(conn, line);
    }
    let nbytes = (fds.len() * std::mem::size_of::<i32>()) as u32;
    let mut iov = libc::iovec {
        iov_base: line.as_ptr() as *mut libc::c_void,
        iov_len: line.len(),
    };
    let mut cmsg = [0u8; 64]; // CMSG_SPACE(3 * sizeof(i32)) rounded up
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(nbytes) } as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(nbytes) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr().cast(), libc::CMSG_DATA(c), nbytes as usize);
        libc::sendmsg(conn.as_raw_fd(), &msg, 0) >= 0
    }
}

fn conn_write_all(conn: &UnixStream, data: &[u8]) -> bool {
    let mut c = conn;
    c.write_all(data).is_ok()
}

fn provenance(cmd: &[String], full_env: bool) -> Value {
    let exe = std::fs::read_link("/proc/self/exe")
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut v = json!({
        "tgid": std::process::id(),
        "ppid": unsafe { libc::getppid() },
        "exe": exe, "cwd": cwd, "argv": cmd,
    });
    // -e: send our full HOST env so the box's ROOT process row has it even when
    // the engine can't /proc-read our tgid (a nested runner). Mirrors the Python
    // runner's read_provenance(full_env=True).
    if full_env { v["env"] = self_environ(); }
    v
}

/// Read /proc/self/environ as a JSON object for env capture (the runner's own
/// HOST environment, contributed to the box's root process row under -e).
fn self_environ() -> Value {
    let raw = std::fs::read("/proc/self/environ").unwrap_or_default();
    let mut map = serde_json::Map::new();
    for kv in raw.split(|b| *b == 0) {
        if kv.is_empty() { continue; }
        let s = String::from_utf8_lossy(kv);
        if let Some((k, v)) = s.split_once('=') {
            map.insert(k.to_string(), json!(v));
        }
    }
    Value::Object(map)
}

fn clear_cloexec(fd: i32) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        if f >= 0 { libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC); }
    }
}

/// Write ALL of `data` to `fd`, looping over partial writes (audit L4). A bare
/// `libc::write` may write fewer bytes than requested — a short write on the
/// length-prefixed box channel desyncs the frame stream, and a short write on a
/// replay/echo fd silently truncates the box's visible output. Retries EINTR;
/// returns false on a real error or EOF (write returning 0), so callers can
/// stop instead of spinning. Async-signal-unsafe (fine: never called between
/// fork and exec).
fn write_all_fd(fd: i32, data: &[u8]) -> bool {
    let mut off = 0usize;
    while off < data.len() {
        let n = unsafe {
            libc::write(fd, data[off..].as_ptr().cast(), data.len() - off)
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            return false; // real write error (EPIPE, EBADF, …)
        }
        if n == 0 { return false; } // no progress: treat as EOF
        off += n as usize;
    }
    true
}

// ── tty job-control (interactive boxes) ──────────────────────────────────────
// We are PID 1 in the box's pid namespace, so process-group ids put on the
// controlling terminal are namespace-local — which lets a job-control shell
// (bash/dash) save/restore the terminal's foreground group without hitting
// "Cannot set tty process group". Mirrors the Python inner: find the tty among
// fd 0/1/2, run the child in its OWN process group, hand it the terminal
// foreground (SIGTTOU ignored during the handoff), and restore on exit.

/// The first of fd 0/1/2 that is a tty, plus its current foreground pgrp.
fn tty_grab() -> (Option<i32>, Option<i32>) {
    let tty_fd = (0..3).find(|&fd| unsafe { libc::isatty(fd) } == 1);
    let old_fg = tty_fd.and_then(|fd| {
        let g = unsafe { libc::tcgetpgrp(fd) };
        if g >= 0 { Some(g) } else { None }
    });
    (tty_fd, old_fg)
}

/// Hand the terminal foreground to `child_pid`'s group; returns the prior
/// SIGTTOU handler to restore. No-op when there is no tty.
fn tty_give(tty_fd: Option<i32>, child_pid: i32) -> Option<libc::sighandler_t> {
    tty_fd.map(|fd| unsafe {
        let old = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::tcsetpgrp(fd, child_pid);
        old
    })
}

/// Restore the saved foreground pgrp + SIGTTOU handler after the child exits.
fn tty_restore(tty_fd: Option<i32>, old_fg: Option<i32>,
               old_ttou: Option<libc::sighandler_t>) {
    if let Some(fd) = tty_fd {
        if let Some(fg) = old_fg { unsafe { libc::tcsetpgrp(fd, fg); } }
        if let Some(h) = old_ttou { unsafe { libc::signal(libc::SIGTTOU, h); } }
    }
}

pub fn run(name: Option<String>, passthrough: bool, direct: bool, env: bool,
           pty: bool, brush: bool, api: bool,
           no_parent: bool, readonly_parent: bool, chdir: Option<String>,
           net_mode: crate::net::NetMode,
           cmd: Vec<String>) -> i32 {
    // Note: an EMPTY cmd is no longer fatal here — when the parent chain
    // carries an OCI image config, we fall back to the image's
    // Entrypoint + Cmd after the register ack returns. A non-OCI box with
    // no cmd still errors out (in the cmd.is_empty() branch after the ack).
    // -b brush REQUIRES the overlay+capture (provenance + recorded writes flow
    // through it). -d has no overlay, so -b+-d is incoherent — error VISIBLY
    // here rather than letting the box fall through to a plain /bin/sh run (the
    // D9 no-silent-downgrade rule applies at selection time too).
    if brush && direct {
        eprintln!("sarun-engine run: -b (brush shell) is incompatible with -d \
                   (no overlay to capture provenance/writes into).");
        return 2;
    }
    // -t passthrough suppresses output capture; -d direct has no overlay so no
    // sinks either (capture = not -t and not -d, mirroring the Python runner).
    // -p PTY mode always wants its output recorded, so it forces capture on
    // even under -t (but never under -d: there is no overlay to capture into).
    // -b brush also wants capture (so provenance frames + writes are recorded),
    // mirroring -p; never under -d (no overlay to capture/provenance into).
    let want_capture = (!passthrough && !direct) || (pty && !direct)
        || (brush && !direct);
    // IN-BOX vs HOST: presence of SARUN_BROKER is the sole in-box signal.
    // bwrap propagates SARUN_BROKER to every box child; the parent inner
    // serves it as an abstract UDS, so the FD broker is reachable from
    // any in-box process — including private-netns / `-n` boxes, since
    // the broker socket lives in the SAME netns as the box's children.
    //   in-box: dial broker, recvmsg a fresh engine conn via SCM_RIGHTS.
    //   host:   dial the engine's filesystem UDS (the universal contract;
    //           the engine's abstract listener exists for in-box, not
    //           here).
    let broker_name = std::env::var("SARUN_BROKER").ok()
        .filter(|s| !s.is_empty());
    let in_box = broker_name.is_some();
    let sock = paths::sock_path();
    let conn = match if let Some(name) = broker_name.as_ref() {
        broker_dial(name)
    } else {
        UnixStream::connect(&sock)
    } {
        Ok(c) => c,
        Err(_) => {
            eprintln!("sarun-engine: no engine running (control socket {}).",
                      sock.display());
            return 3;
        }
    };
    // register handshake. We always send our own pidfd as SCM_RIGHTS so the
    // engine can derive our HOST pid (and, for a nested box, the enclosing box
    // from the /proc ancestry). IN-BOX sends a single-segment `relname` (never
    // an absolute name — a box must not influence its own parent); HOST sends
    // the optional NAME as `session_id`. The SAME connection becomes the box
    // channel.
    let mut reg = json!({"type": "register",
                         "cmd": cmd, "prov": provenance(&cmd, env),
                         "want_capture": want_capture,
                         "want_direct": direct,
                         "want_env": env,
                         "want_brush": brush,
                         "want_api": api,
                         "net_mode": net_mode.as_str(),
                         "want_no_parent": no_parent,
                         "want_readonly_parent": readonly_parent,
                         // Advisory: the engine reruns iff a sibling NAME exists.
                         // A named launch is always rerun-eligible.
                         "want_rerun": name.is_some()});
    if in_box {
        reg["relname"] = json!(name.unwrap_or_default());
    } else {
        reg["session_id"] = json!(name.unwrap_or_default());
    }
    // Tap mode: WE create the netns + TAP device (the engine creates none) and
    // hand the engine the TAP fd as register's second SCM_RIGHTS fd. The
    // unshare(CLONE_NEWNET) moves THIS process into the fresh netns; bwrap,
    // spawned later WITHOUT --unshare-net, inherits it. Fail LOUD on error —
    // never silently fall through to some other network.
    let tap_fd: Option<std::os::fd::OwnedFd> =
        if net_mode == crate::net::NetMode::Tap {
            match crate::net::tap::create_netns_tap() {
                Ok(fd) => Some(fd),
                Err(e) => {
                    eprintln!("sarun-engine: tap setup failed: {e}");
                    eprintln!("hint: tap now self-acquires netns privileges via \
                               an unprivileged user namespace, so this is most \
                               likely /dev/net/tun being root-only — `ls -l \
                               /dev/net/tun` should be crw-rw-rw- (0666); \
                               otherwise pass `--net host` (-N) or `--net off`");
                    return 1;
                }
            }
        } else { None };
    let pidfd = pidfd_open(std::process::id() as i32);
    let tap_raw = tap_fd.as_ref().map(|f| f.as_raw_fd());
    if !send_register(&conn, format!("{reg}\n").as_bytes(), pidfd, tap_raw) {
        eprintln!("sarun-engine: register write failed");
        return 1;
    }
    if pidfd >= 0 { unsafe { libc::close(pidfd); } }
    // The engine dup'd the TAP fd via SCM_RIGHTS; close our copy (the device
    // stays alive on the engine's fd + our netns).
    drop(tap_fd);
    let mut line = String::new();
    if BufReader::new(&conn).read_line(&mut line).is_err() {
        eprintln!("sarun-engine: register read failed");
        return 1;
    }
    let ack: Value = match serde_json::from_str(&line) {
        Ok(v) => v, Err(_) => { eprintln!("sarun-engine: bad ack"); return 1; }
    };
    if ack.get("ok").and_then(Value::as_bool) != Some(true) {
        eprintln!("sarun-engine: {}",
                  ack.get("error").and_then(Value::as_str).unwrap_or("register failed"));
        return 1;
    }
    let mount = ack.get("mount").and_then(Value::as_str).unwrap_or("").to_string();
    let sid = ack.get("session_id").and_then(Value::as_str).unwrap_or("?").to_string();
    let _box_name_str = ack.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    // Pulled up so `inner_args` (built earlier) can pass `--api` straight
    // to the inner; the later `--api` block re-uses this same value.
    let api_on = ack.get("api").and_then(Value::as_bool).unwrap_or(false);
    // D-oci: when an ancestor box is an OCI image layer (it carries an
    // `oci_config` meta key), the engine echoes the image's runtime fields
    // in `ack.oci` — env / cwd / cmd / entrypoint / user. We apply each
    // unless the user explicitly overrode it:
    //   * -C wins over the image's WorkingDir
    //   * a supplied cmd wins over the image's Entrypoint + Cmd
    //   * the image's Env is unioned INTO the inherited host env (entries
    //     in the image win on collision)
    let oci_runtime = ack.get("oci").cloned();
    let oci_cwd = oci_runtime.as_ref()
        .and_then(|o| o.get("cwd")).and_then(Value::as_str)
        .map(String::from)
        .filter(|s| !s.is_empty());
    let oci_env: Vec<String> = oci_runtime.as_ref()
        .and_then(|o| o.get("env")).and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let oci_user = oci_runtime.as_ref()
        .and_then(|o| o.get("user")).and_then(Value::as_str)
        .map(String::from)
        .filter(|s| !s.is_empty());
    // Working directory: -C wins, else the image's WorkingDir, else a default
    // chosen by HOST VISIBILITY (the engine's `no_host` ack):
    //   * no host visibility (closed chain — an OCI image rootfs, or an explicit
    //     --no-parent box) → "/", exactly what Docker/Podman/containerd do when
    //     an image sets no WorkingDir. The host's cwd does not exist inside a
    //     closed rootfs, so inheriting it made bwrap fail `chdir`.
    //   * has host visibility (a plain host-rooted box) → the runner's own cwd,
    //     so `sarun run -- cmd` behaves like a shell: it runs where you are.
    let no_host_box = ack.get("no_host").and_then(Value::as_bool).unwrap_or(false);
    let cwd = chdir
        .or(oci_cwd)
        .unwrap_or_else(|| {
            if no_host_box {
                "/".to_string()
            } else {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "/".into())
            }
        });
    // No-cmd path: pull Entrypoint + Cmd from the image config and use it.
    let cmd = if cmd.is_empty() {
        let mut combined: Vec<String> = oci_runtime.as_ref()
            .and_then(|o| o.get("entrypoint")).and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let oci_cmd: Vec<String> = oci_runtime.as_ref()
            .and_then(|o| o.get("cmd")).and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        combined.extend(oci_cmd);
        if combined.is_empty() {
            eprintln!("sarun-engine: no command given (and the image config \
                       has neither Entrypoint nor Cmd to fall back on).");
            return 2;
        }
        combined
    } else {
        cmd
    };
    eprintln!("sarun-engine: box {sid}  (overlay root: {mount})  UI connected");

    // bwrap CMD onto the box's overlay root, exec'ing our own `inner`. The box
    // channel fd is passed (CLOEXEC cleared) and held open by inner.
    let fd = conn.as_raw_fd();
    clear_cloexec(fd);
    let self_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "sarun-engine".into());
    // Root: a top-level box binds its overlay by host path (<mnt>/<id>); a
    // NESTED box can't reach that path inside the parent, so it binds the
    // parent-exposed synthetic /<KIDS_DIR>/<id> (served by the same overlay,
    // routing to this child's real root). Both bind a directory bwrap holds
    // CAP_SYS_ADMIN over inside its own userns — no ambient caps needed.
    let root_src = if in_box { format!("/{KIDS_DIR}/{sid}") } else { mount.clone() };
    let fd_s = fd.to_string();
    // Honor the engine's capture decision (it downgrades for -t/-d): only pass
    // --capture to inner when the ack confirms capture is active.
    let capture_on = want_capture
        && ack.get("capture").and_then(Value::as_bool).unwrap_or(false);
    // Ferry the engine binary into the box as an INHERITED fd and exec it as
    // /proc/self/fd/N (fexecve-style) — no bind mount, no /run/sarun tmpfs. A
    // bind needed an engine-owned mountpoint under the box root, and that tmpfs
    // can't be planted on a closed OCI rootfs whose /run is a FUSE-synthesized
    // dir (the mount fails ENOENT and the box never starts). The fd rides into
    // bwrap with CLOEXEC cleared (exactly like the box-channel fd), and /proc is
    // mounted (--proc /proc), so /proc/self/fd/N resolves to the engine at the
    // moment bwrap exec's it — a regular box AND a closed `--no-parent` box both
    // start, with nothing engine-owned bound inside the box.
    let bin_fd = unsafe {
        libc::open(
            std::ffi::CString::new(self_exe.as_bytes()).unwrap().as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC)
    };
    if bin_fd < 0 {
        eprintln!("sarun-engine: open engine binary {self_exe}: {}",
                  std::io::Error::last_os_error());
        return 1;
    }
    clear_cloexec(bin_fd);
    let inner_exe = format!("/proc/self/fd/{bin_fd}");
    let mut inner_args: Vec<&str> = vec![
        inner_exe.as_str(), "inner", "--conn-fd", &fd_s];
    if capture_on { inner_args.push("--capture"); }
    // PTY needs the capture sink files to record into; if the engine declined
    // capture (-d) there is nothing to PTY into, so gate --pty on capture_on.
    if pty && capture_on { inner_args.push("--pty"); }
    // -b brush likewise needs the capture sinks to record provenance + writes.
    if brush && capture_on { inner_args.push("--brush"); }
    // --api passes through to inner so an in-box `oaita` client knows to reach
    // the engine's LLM proxy by dialing the FD broker (SARUN_BROKER abstract
    // UDS) per call — no in-box socket node, no host UDS, attribution implicit.
    if api_on { inner_args.push("--api"); }
    inner_args.push("--");
    let mut bwrap = Command::new("bwrap");
    bwrap.args(["--bind", &root_src, "/",
                "--proc", "/proc", "--dev", "/dev",
                // Expose the TUN device node so a NESTED box can build its own
                // TAP netns the same way a top-level runner does (it creates the
                // TAP inside its own network namespace — netns isolation means
                // this grants no host reach). --dev-bind-try: harmless if absent.
                "--dev-bind-try", "/dev/net/tun", "/dev/net/tun",
                "--ro-bind-try", "/sys", "/sys"]);
    // /tmp policy: non-api boxes get the bwrap-private tmpfs (clean
    // isolation, but nothing written there ever reaches the overlay —
    // apply/inspect can't see it, the model's strongest write-target
    // prior becomes a black hole). For oaita `--api` boxes we instead
    // symlink /tmp at a per-box dir under oaita's state home: writes
    // resolve into the box's regular overlay coverage so they stage
    // for review, survive apply, sit alongside the session's other
    // context for parent inspect, and discard rolls them back with
    // the rest of the box state. The directory is precreated and
    // 0700; nothing here is visible to other boxes.
    if api_on {
        // The overlay (Overlay::resolve) presents /tmp as a symlink to a
        // per-box host dir under oaita's state home for --api boxes.
        // Precreate the target so the very first /tmp lookup resolves to
        // an extant directory; bwrap inherits the symlink from the FUSE
        // (no --tmpfs needed and no --symlink either — the FS layer below
        // does the substitution).
        let key = sid.to_string();
        let p = crate::paths::oaita_state_home().join(".tmp").join(&key);
        let _ = std::fs::create_dir_all(&p);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p,
            std::fs::Permissions::from_mode(0o700));
    } else {
        bwrap.args(["--tmpfs", "/tmp"]);
    }
    // No bwrap binds for the engine binary, no /run/sarun tmpfs, no
    // /usr/local/bin/{oaita,sarun} FUSE shadow. The runner exec's `inner` from
    // the inherited fd at /proc/self/fd/N (see above); that path is ALWAYS
    // reachable, so a closed OCI rootfs (no /run, no /usr/local to traverse)
    // starts without depending on any in-box path. A nested `sarun`/`oaita`
    // (the oaita driver + sub-agents, or an interactive brush builtin) re-execs
    // the inner runner's OWN executable via /proc/self/exe — see
    // oaita::exec::default_sarun and brush::EngineSelfCommand. (The brush
    // /bin/sh,make,ninja shadows below are unrelated and stay.)
    // FD broker: pick a per-box abstract-UDS name and propagate it to
    // the inner AND every child process via --setenv. The inner binds it
    // (inner_broker_serve), in-box clients dial it (broker_dial). Keying
    // on the engine-assigned SID gives us a name that's unique across
    // boxes — important because Host-netns boxes share a single abstract
    // namespace.
    let broker_name = format!("sarun-broker:{sid}");
    bwrap.args(["--setenv", "SARUN_BROKER", &broker_name]);
    // Engine self-reference for IN-BOX re-execs. The box CMD and its
    // descendants (the oaita driver, brush's `sarun`/`oaita` builtins, a
    // nested `sarun run`) must be able to re-exec the engine — but the box's
    // rootfs may NOT contain the engine binary's host path (a closed OCI
    // rootfs like alpine, an `oaita run --on <image>` parent). `/proc/self/exe`
    // then fails to resolve. The engine is already ferried in as the inherited
    // fd `bin_fd` (see above) and exec'd as `/proc/self/fd/N`; that path
    // resolves through the process's OWN fd table regardless of rootfs. bin_fd
    // is CLOEXEC-cleared, so it survives every fork+exec down the box's process
    // tree — the same fd number stays valid everywhere. Publish it so the
    // re-exec sites use it instead of guessing a path.
    bwrap.args(["--setenv", "SARUN_EXE", &inner_exe]);
    // D9 follow-on — NESTED shell IS brush (brush boxes, capture on only).
    // We shadow the box's /bin/sh, /bin/bash (and /usr/bin/{sh,bash}) with the
    // ENGINE binary: the shim (brush_sh, gated on SARUN_BRUSH_SH=1) RUNS the
    // nested `sh -c RECIPE` THROUGH embedded brush-core — there is NO real-shell
    // fallback, no stash. A construct brush cannot run is a VISIBLE error and a
    // non-zero exit, matching the D9 no-silent-downgrade rule that already
    // governs the top-level brush body. Non-brush boxes are NOT touched (their
    // /bin/sh is the real system shell).
    if brush && capture_on {
        // The /bin/sh, /usr/bin/make, /bin/ninja etc. shadowing is
        // applied LAZILY by the FUSE overlay at lookup time (it
        // reads shadow_*.glob and matches each open against the
        // patterns). No pre-enumeration of the host filesystem here.
        // We still need to tell the box that the shim should kick in
        // when it gets exec'd as /bin/sh.
        bwrap.args(["--setenv", "SARUN_BRUSH_SH", "1"]);
    }
    // D-oci: apply the image config's Env (PATH, etc.) so a /bin/sh inside the
    // closed image actually finds /usr/bin/* and the user's command resolves
    // without needing `-C / env PATH=...` boilerplate. Each `KEY=VALUE` from
    // the image's Env becomes a --setenv. Image entries WIN over the host's
    // inherited env on collision — that's the OCI runtime semantic (the
    // image's PATH is the right one for the image's filesystem).
    for kv in &oci_env {
        if let Some((k, v)) = kv.split_once('=') {
            bwrap.args(["--setenv", k, v]);
        }
    }
    // D-oci: apply User as bwrap --uid/--gid. The user spec is
    // "uid[:gid]" or a name we don't try to resolve (containers usually
    // ship a numeric uid in their config). Skipping the parse on a name
    // keeps us safe rather than crashing on a non-numeric User.
    if let Some(u) = &oci_user {
        let (uid, gid) = u.split_once(':')
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((u.as_str(), None));
        if let Ok(uid_n) = uid.parse::<u32>() {
            // Hold the formatted strings in locals so they outlive the .args() call.
            let uid_s = uid_n.to_string();
            bwrap.args(["--uid", &uid_s]);
            if let Some(g) = gid.and_then(|g| g.parse::<u32>().ok()) {
                let gid_s = g.to_string();
                bwrap.args(["--gid", &gid_s]);
            }
        }
    }
    // --api: the in-box oaita HTTP client dials the FD broker (SARUN_
    // BROKER, already in env) to get a fresh engine conn, then sends the
    // `api.proxy` verb header on it — the engine takes over and HTTP-
    // proxies to the configured upstream LLM. No in-box UDS, no
    // FRAME_API_* multiplex on the box-channel, no peer-pid walk:
    // attribution comes from the broker's box-id hint at handoff time.
    if api_on {
        // The in-box client uses OPENAI_BASE_URL to extract a path prefix
        // (`/v1`) for outgoing HTTP request URLs. The host part is
        // irrelevant once `SARUN_BROKER` is set — the dial doesn't use it.
        // A nested `sarun oaita` re-execs /proc/self/exe (the inner runner's
        // own binary); no in-box path, no FUSE shadow. The in-box vs host
        // dispatch is decided by the `--inbox` cli flag spawn_in_box passes
        // on its re-exec — not an env marker.
        bwrap.args(["--setenv", "OPENAI_BASE_URL", "http://oaita-proxy/v1"]);
    }
    bwrap.args(["--unshare-pid", "--unshare-ipc", "--unshare-uts",
                "--die-with-parent"]);
    // Netns dispatch:
    //   Off  → bwrap --unshare-net  (brand new empty netns; dials fail closed)
    //   Tap  → WE already unshare(CLONE_NEWNET)'d above (create_netns_tap) and
    //          handed the engine the TAP fd, so this process is ALREADY in the
    //          equipped netns. bwrap must NOT --unshare-net — it inherits ours.
    //   Host → no --unshare-net (box shares the launcher's netns).
    let _dns_ip = ack.get("dns_ip").and_then(Value::as_str)
        .map(|s| s.to_string()).unwrap_or_default();
    let ca_pem = ack.get("ca_pem").and_then(Value::as_str)
        .map(|s| s.to_string()).unwrap_or_default();
    match net_mode {
        crate::net::NetMode::Off => { bwrap.arg("--unshare-net"); }
        crate::net::NetMode::Host => { /* leave the launcher's netns */ }
        crate::net::NetMode::Tap => { /* already in our own TAP netns */ }
    }
    // Tap boxes: a caller *_proxy env var pointing at LOOPBACK is
    // guaranteed dead inside the box — 127.0.0.1 in the box netns is the
    // box itself, and tap flows are transparently proxied by the engine
    // anyway. Leaking it silently blackholes every proxy-honoring HTTP
    // client (Chromium in the carbonyl image dialed the host's local
    // agent proxy and got ERR_PROXY_CONNECTION_FAILED instead of loading
    // anything). Non-loopback proxy values are kept — they may be
    // reachable through the tap.
    if net_mode == crate::net::NetMode::Tap {
        for k in ["http_proxy", "https_proxy", "ftp_proxy", "all_proxy",
                  "HTTP_PROXY", "HTTPS_PROXY", "FTP_PROXY", "ALL_PROXY"] {
            if std::env::var(k).is_ok_and(|v| v.contains("127.0.0.")
                || v.contains("localhost") || v.contains("[::1]")) {
                bwrap.env_remove(k);
            }
        }
    }
    if !ca_pem.is_empty() {
        // CA bundle for the engine's MITM. The engine writes a single
        // host-side bundle once at startup (paths::api_box_ca_pem_path);
        // the FUSE overlay shadows /etc/ssl/certs/ca-certificates.crt
        // (and the rest of CA_BUNDLE_TARGETS) for every `--api` box,
        // serving the engine's bundle bytes when the box reads them.
        // Same pattern the safe-oaita.toml shadow uses for --api boxes —
        // see overlay::Overlay::attr_of / matches_host_oaita_config.
        //
        // The runner needs no bwrap binds for any of those paths. We
        // only set the env vars pointing at the canonical path so tools
        // that look up SSL_CERT_FILE / CURL_CA_BUNDLE / etc resolve to
        // it.
        let canonical_inside = "/etc/ssl/certs/ca-certificates.crt";
        for (k, v) in [("SSL_CERT_FILE", canonical_inside),
                       ("CURL_CA_BUNDLE", canonical_inside),
                       ("NODE_EXTRA_CA_CERTS", canonical_inside),
                       ("REQUESTS_CA_BUNDLE", canonical_inside),
                       ("GIT_SSL_CAINFO", canonical_inside)] {
            bwrap.args(["--setenv", k, v]);
        }
    }
    // resolv.conf: also a FUSE shadow for --api / Tap boxes — the
    // overlay synthesizes "nameserver <dns_ip>\n" when the box reads
    // /etc/resolv.conf. No bwrap bind required.
    bwrap.args(["--chdir", &cwd, "--"]);
    let status = bwrap.args(&inner_args).args(&cmd).status();
    drop(conn); // our copy; inner (in the box) is the channel's sole holder now
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => { eprintln!("sarun-engine: bwrap failed: {e}"); 1 }
    }
}

// ── sud launcher (absorbed from tv/sud/sudtrace.c) ──────────────────────────
// The runner IS the sud launcher now: it owns the trace fd (1023) and the
// shared wire-state page (1022), writes the TRACE version atom + launcher
// EV_EXIT events (crate::sudwire), and execs the sud64 wrapper directly
// with the argv flag block from tv/sud/runtime_config.h. tv's own sudtrace
// binary is no longer in the loop. See engine/DESIGN-sud.md (WIP).

/// The two high fds the sud wrapper contract reserves (tv/sud/sudtrace.c):
/// 1023 = trace output, 1022 = MAP_SHARED wire-state page (stream-id
/// counter). Every traced child inherits both.
const SUD_OUTPUT_FD: i32 = 1023;
const SUD_STATE_FD: i32 = 1022;

/// Resolve `cmd0` through PATH like sudtrace's build_wrapper_argv (only
/// used for probing the target — the wrapper gets the user's argv).
fn sud_resolve_target(cmd0: &str) -> String {
    if cmd0.contains('/') { return cmd0.to_string(); }
    let pathenv = std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/bin:/bin".into());
    for seg in pathenv.split(':').filter(|s| !s.is_empty()) {
        let cand = format!("{seg}/{cmd0}");
        if unsafe {
            libc::access(std::ffi::CString::new(cand.as_bytes())
                .unwrap().as_ptr(), libc::X_OK) == 0
        } {
            return cand;
        }
    }
    cmd0.to_string()
}

/// Probe `path` head bytes: Some((interp, Some(arg))) for a shebang
/// script, None for anything else. Mirrors sudtrace's parse (first
/// whitespace-separated token = interpreter, rest = one argument).
fn sud_shebang(path: &str) -> Option<(String, Option<String>)> {
    let head = {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut buf = [0u8; 512];
        let n = f.read(&mut buf).ok()?;
        buf[..n].to_vec()
    };
    if head.len() < 3 || &head[..2] != b"#!" { return None; }
    let line_end = head.iter().position(|b| *b == b'\n')
        .unwrap_or(head.len());
    let line = String::from_utf8_lossy(&head[2..line_end]).into_owned();
    let line = line.trim_matches(|c| c == ' ' || c == '\t' || c == '\r');
    let mut it = line.splitn(2, [' ', '\t']);
    let interp = it.next()?.to_string();
    if interp.is_empty() { return None; }
    let arg = it.next()
        .map(|a| a.trim_matches(|c| c == ' ' || c == '\t' || c == '\r'))
        .filter(|a| !a.is_empty())
        .map(String::from);
    Some((interp, arg))
}

/// ELF class of `path`: 1 = 32-bit, 2 = 64-bit, 0 = not readable/ELF.
fn sud_elf_class(path: &str) -> u8 {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return 0 };
    let mut e = [0u8; 5];
    if f.read_exact(&mut e).is_err() { return 0; }
    if &e[..4] != b"\x7fELF" { return 0; }
    e[4]
}

/// /proc/<pid>/status field parse with sudtrace's fallbacks
/// (tgid → pid, ppid → 0) — reaped pids read back absent.
fn sud_proc_field(pid: i32, field: &str, fallback: i32) -> i32 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status"))
        else { return fallback };
    s.lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(fallback)
}

/// `run --sud` (WIP, see engine/DESIGN-sud.md): run CMD under the sud64
/// wrapper instead of bwrap+FUSE. Registers with `want_sud`, gets the
/// engine-owned upper directory back in the ack, sets up the sud launcher
/// contract itself (fds 1022/1023, version atom, EXIT events), execs
///   sud64 --remap-rule … resolved CMD
/// and, after the child exits, asks the engine (fresh conn, `sud_ingest`)
/// to sweep the upper into the box's sqlar. The register conn stays open
/// for the duration — its EOF after the sweep is the normal box teardown.
pub fn run_sud(name: Option<String>, env: bool, chdir: Option<String>,
               net_mode: crate::net::NetMode, brush: bool,
               cmd: Vec<String>) -> i32 {
    if cmd.is_empty() {
        eprintln!("sarun-engine run --sud: needs a command");
        return 2;
    }
    if std::env::var("SARUN_BROKER").is_ok_and(|s| !s.is_empty()) {
        eprintln!("sarun-engine run --sud: nested sud boxes are not \
                   supported yet (see engine/DESIGN-sud.md).");
        return 2;
    }
    let sud64 = std::env::var("SARUN_SUD64")
        .ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sud64".to_string());
    // The 32-bit twin: $SARUN_SUD32, else sud64's sibling (the wrapper
    // itself derives its cross-class sibling the same way — dir(self) +
    // "/sud32" — so keeping them adjacent is already the contract).
    let sud32 = std::env::var("SARUN_SUD32")
        .ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| match sud64.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/sud32"),
            None => "sud32".to_string(),
        });
    let sock = paths::sock_path();
    let conn = match UnixStream::connect(&sock) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("sarun-engine: no engine running (control socket {}).",
                      sock.display());
            return 3;
        }
    };
    // Trace pipe: the wrapper contract's fd 1023 becomes the WRITE end of a
    // pipe whose READ end rides to the engine as an SCM_RIGHTS fd; the engine
    // streams events live and tees the raw bytes to live/<id>/sud.trace.
    let mut pfd = [0i32; 2];
    if unsafe { libc::pipe(pfd.as_mut_ptr()) } < 0 {
        eprintln!("sarun-engine: trace pipe: {}",
                  std::io::Error::last_os_error());
        return 1;
    }
    let (trace_r, trace_w) = (pfd[0], pfd[1]);
    // Networking: same as a FUSE box. Tap mode → WE create the netns + TAP
    // device (unshare(CLONE_NEWNET) moves THIS process into the fresh netns;
    // the wrapper, spawned later, inherits it) and hand the engine the TAP
    // fd; the engine wires it to its in-process TCP/IP stack (DHCP, DNS,
    // HTTPS MITM). Off → an empty netns (every dial fails closed). Host →
    // share the launcher's netns. Fail LOUD on tap setup error.
    let tap_fd: Option<std::os::fd::OwnedFd> =
        if net_mode == crate::net::NetMode::Tap {
            match crate::net::tap::create_netns_tap() {
                Ok(fd) => Some(fd),
                Err(e) => {
                    eprintln!("sarun-engine run --sud: tap setup failed: {e}");
                    eprintln!("hint: `ls -l /dev/net/tun` should be 0666; \
                               otherwise pass `--net host` or `--net off`");
                    unsafe { libc::close(trace_r); libc::close(trace_w); }
                    return 1;
                }
            }
        } else { None };
    if net_mode == crate::net::NetMode::Off {
        // Empty netns so every dial fails closed — the wrapper (spawned
        // later in this process's netns) inherits it. No bwrap to do it.
        if let Err(e) = crate::net::tap::unshare_netns() {
            eprintln!("sarun-engine run --sud: --net off netns: {e}");
            unsafe { libc::close(trace_r); libc::close(trace_w); }
            return 1;
        }
    }
    let reg = json!({"type": "register",
                     "cmd": cmd, "prov": provenance(&cmd, env),
                     "want_capture": false,
                     "want_direct": false,
                     "want_env": env,
                     "want_sud": true,
                     "net_mode": net_mode.as_str(),
                     "session_id": name.clone().unwrap_or_default(),
                     "want_rerun": name.is_some()});
    let pidfd = pidfd_open(std::process::id() as i32);
    let tap_raw = tap_fd.as_ref().map(|f| f.as_raw_fd());
    if !send_register_fds(&conn, format!("{reg}\n").as_bytes(), pidfd,
                          tap_raw, Some(trace_r)) {
        eprintln!("sarun-engine: register write failed");
        return 1;
    }
    if pidfd >= 0 { unsafe { libc::close(pidfd); } }
    drop(tap_fd); // engine dup'd it; device stays alive on its fd + our netns
    unsafe { libc::close(trace_r); } // engine holds its dup now
    let mut line = String::new();
    if BufReader::new(&conn).read_line(&mut line).is_err() {
        eprintln!("sarun-engine: register read failed");
        return 1;
    }
    let ack: Value = match serde_json::from_str(&line) {
        Ok(v) => v, Err(_) => { eprintln!("sarun-engine: bad ack"); return 1; }
    };
    if ack.get("ok").and_then(Value::as_bool) != Some(true) {
        eprintln!("sarun-engine: {}",
                  ack.get("error").and_then(Value::as_str)
                      .unwrap_or("register failed"));
        return 1;
    }
    let sid = ack.get("session_id").and_then(Value::as_str)
        .unwrap_or("?").to_string();
    let upper = ack.get("sud_upper").and_then(Value::as_str)
        .unwrap_or("").to_string();
    // Nested (same-in-same) sud box: the engine materialized each
    // ancestor's captured state and hands the lower list back; the
    // overlay stacks upper → lowers → host in that priority order.
    let lowers: Vec<String> = ack.get("sud_lowers")
        .and_then(Value::as_array)
        .map(|a| a.iter()
             .filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if upper.is_empty() {
        eprintln!("sarun-engine: engine did not allocate a sud upper \
                   (engine older than this runner?)");
        return 1;
    }
    eprintln!("sarun-engine: box {sid}  (sud upper: {upper})");
    // -b brush: the box's shell IS the embedded brush, exactly like a FUSE
    // -b box — but here brush is the TRACED TARGET: the engine binary runs
    // under the wrapper via the explicit `brush-sh` subcommand, so brush,
    // the coreutils builtins, find/xargs, and the embedded make (kati) /
    // ninja (n2) all execute IN ONE traced process. The in-process
    // advantage survives the backend swap — and compounds: a builtin's
    // file I/O is a SIGSYS trap into the same address space (userland
    // overlay), not a kernel round-trip into the engine's fuser threads.
    // The FUSE shadow binds become `remap:` rules below. Gap (documented
    // in DESIGN-sud.md): no box channel in the traced process, so brush's
    // semantic-provenance frames are skipped (send_nested_prov no-ops
    // without SARUN_BROKER); provenance comes from the trace stream.
    let self_exe: Option<String> = std::env::current_exe().ok()
        .and_then(|p| p.to_str().map(String::from));
    let cmd = if brush {
        let Some(exe) = self_exe.clone() else {
            eprintln!("sarun-engine run --sud: -b needs current_exe()");
            return 1;
        };
        let script = crate::brush::script_from_argv(&cmd);
        vec![exe, "brush-sh".into(), "--".into(),
             "sh".into(), "-c".into(), script]
    } else { cmd };
    // Probe the target the way sudtrace did: PATH-resolve, shebang, ELF
    // class → pick sud32 or sud64 for the initial exec (the wrapper
    // handles cross-class children itself via its dir-sibling paths).
    let resolved = sud_resolve_target(&cmd[0]);
    let shebang = sud_shebang(&resolved);
    let probe = shebang.as_ref().map(|(i, _)| i.as_str())
        .unwrap_or(resolved.as_str());
    let wrapper = if sud_elf_class(probe) == 1 { &sud32 } else { &sud64 };
    // Launcher contract, absorbed from tv/sud/sudtrace.c: the trace pipe's
    // write end on fd 1023 and the 4 KiB MAP_SHARED wire-state page
    // (stream-id counter) on fd 1022, inherited by every traced child.
    // CRITICAL for in-box nesting: the fds are installed in the CHILD
    // (pre_exec, between fork and exec), never in this process — a nested
    // runner is itself traced by the OUTER wrapper, whose trace addin
    // writes outer events to fd 1023 in our process; replumbing our own
    // 1022/1023 would splice outer-stream events (with colliding stream
    // ids from the outer counter page) into the inner trace. The launcher
    // writes its own version atom + EXIT events through trace_w directly.
    let stream_id: u32;
    let state_page: *mut u32;
    let mfd: i32;
    unsafe {
        mfd = libc::syscall(libc::SYS_memfd_create,
                            c"sud_wire_state".as_ptr(), 0u32) as i32;
        if mfd < 0 || libc::ftruncate(mfd, 4096) < 0 {
            eprintln!("sarun-engine: wire state page: {}",
                      std::io::Error::last_os_error());
            return 1;
        }
        let p = libc::mmap(std::ptr::null_mut(), 4096,
                           libc::PROT_READ | libc::PROT_WRITE,
                           libc::MAP_SHARED, mfd, 0);
        if p == libc::MAP_FAILED {
            eprintln!("sarun-engine: mmap wire state: {}",
                      std::io::Error::last_os_error());
            return 1;
        }
        state_page = p.cast();
        // struct sud_shared { volatile uint32_t next_stream_id; } — the
        // page is zero-filled; post-increment value is our stream id
        // (launcher = 1, children take 2, 3, … the same way).
        stream_id = (*std::sync::atomic::AtomicU32::from_ptr(state_page))
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let va = crate::sudwire::version_atom();
        let _ = write_all_fd(trace_w, &va);
    }
    // Wrapper argv: flag block (runtime_config.h shapes), then the target.
    // Rule order matters (first-prefix-match wins): carve the pseudo
    // filesystems and sarun's own state tree out BEFORE the wide `/`
    // overlay. /tmp passthrough is a step-1 stopgap — see DESIGN-sud.md.
    let state_dir = crate::paths::state_home();
    let mut sc = Command::new(wrapper);
    for p in ["/proc", "/dev", "/sys"] {
        sc.args(["--remap-rule", &format!("passthrough:{p}")]);
    }
    sc.arg("--remap-rule")
        .arg(format!("passthrough:{}", state_dir.display()));
    // The engine's FUSE mountpoint too: a nested NON-sud box launched
    // from inside this box binds <mnt>/<id> and writes through it —
    // those writes belong to the nested box's own capture, not to this
    // box's upper (sud × FUSE composition, DESIGN-sud.md).
    sc.arg("--remap-rule")
        .arg(format!("passthrough:{}", crate::paths::mnt_point().display()));
    // /tmp: an inramfs mount — served from the shared-memory store the
    // engine keyed for this box; captured at sweep. Replaces the old
    // /tmp passthrough stopgap (whose writes were never captured). Listed
    // AFTER every narrower carve-out above: if the engine's state/mnt
    // dirs live under /tmp (test rigs do this), first-prefix-match must
    // still route them to the host, not into the store.
    let ir_key = ack.get("sud_ir_key").and_then(Value::as_str)
        .unwrap_or("").to_string();
    if !ir_key.is_empty() {
        sc.args(["--remap-rule", "inramfs:/tmp"]);
        sc.args(["--inramfs-key", &ir_key]);
    }
    // Tap networking: the box reaches the net through the engine's MITM
    // proxy, so it must trust the engine's CA and resolve via the gateway.
    // A FUSE box gets these as overlay SHADOWS; a sud box gets them as
    // `remap` rules pointing the CA-bundle + resolv.conf paths at host
    // files the runner materializes (ca_pem / dns_ip come back in the ack).
    // Listed BEFORE the wide overlay:/ rule (first-prefix-match wins).
    let ca_pem = ack.get("ca_pem").and_then(Value::as_str).unwrap_or("");
    let dns_ip = ack.get("dns_ip").and_then(Value::as_str).unwrap_or("");
    let backing = ack.get("shm_dir").and_then(Value::as_str)
        .map(std::path::PathBuf::from);
    if net_mode == crate::net::NetMode::Tap && !ca_pem.is_empty() {
        if let Some(bk) = &backing {
            let ca_path = bk.join("sud-ca.pem");
            if std::fs::write(&ca_path, ca_pem).is_ok() {
                for tgt in CA_BUNDLE_TARGETS {
                    sc.args(["--remap-rule",
                             &format!("remap:{tgt}={}", ca_path.display())]);
                }
                let canonical = "/etc/ssl/certs/ca-certificates.crt";
                for k in ["SSL_CERT_FILE", "CURL_CA_BUNDLE",
                          "NODE_EXTRA_CA_CERTS", "REQUESTS_CA_BUNDLE",
                          "GIT_SSL_CAINFO"] {
                    sc.env(k, canonical);
                }
            }
            if !dns_ip.is_empty() {
                let rc_path = bk.join("sud-resolv.conf");
                if std::fs::write(&rc_path,
                                  format!("nameserver {dns_ip}\n")).is_ok() {
                    sc.args(["--remap-rule",
                             &format!("remap:/etc/resolv.conf={}",
                                      rc_path.display())]);
                }
            }
        }
    }
    // -b shadow rules: the sud analogue of the FUSE overlay's lazy
    // /bin/sh + make + ninja shadowing (overlay.rs::Shadows defaults). A
    // nested tool's execve of a shadowed path is remapped to the engine
    // binary; argv[0] keeps the shadowed name and SARUN_BRUSH_SH=1 gates
    // dispatch (is_brush_sh_invocation / is_make_invocation /
    // is_ninja_invocation), so recipes run through embedded brush and
    // make/ninja run in-process (kati/n2) — no real-shell storm. The
    // remap matcher is component-boundary-safe (/bin/sh ≠ /bin/shred)
    // and execve paths go through the same resolver as opens.
    // The remap DESTINATION is a per-box symlink NAMED AFTER THE TOOL
    // (live/<id>/shadow-bin/sh → engine), not the engine path itself:
    // the wrapper's exec rewrite substitutes the resolved target path
    // as the child's argv[0] (handler.c build_exec_argv), so remapping
    // straight to the engine binary would lose the invocation name the
    // dispatch gates key on. The symlink keeps the basename; the
    // engine-state passthrough rule keeps the link itself host-served.
    if brush {
        let exe = self_exe.as_deref().unwrap_or_default();
        sc.env("SARUN_BRUSH_SH", "1");
        let shadow_dir = std::path::Path::new(&upper)
            .parent().map(|p| p.join("shadow-bin"))
            .unwrap_or_else(|| std::path::PathBuf::from("shadow-bin"));
        let _ = std::fs::create_dir_all(&shadow_dir);
        for name in ["sh", "bash", "dash", "make", "gmake", "ninja"] {
            let link = shadow_dir.join(name);
            let _ = std::fs::remove_file(&link);
            if let Err(e) = std::os::unix::fs::symlink(exe, &link) {
                eprintln!("sarun-engine run --sud: -b shadow link \
                           {}: {e}", link.display());
                return 1;
            }
            for dir in ["/bin", "/usr/bin"] {
                sc.args(["--remap-rule",
                         &format!("remap:{dir}/{name}={}", link.display())]);
            }
        }
    }
    // rules.h caps an overlay rule at 9 layers (upper + 8): chain depth
    // beyond that would be SILENTLY dropped by the wrapper parser — fail
    // loud instead.
    if 2 + lowers.len() > 9 {
        eprintln!("sarun-engine run --sud: box chain too deep for one \
                   overlay rule ({} layers > 9)", 2 + lowers.len());
        return 2;
    }
    let mut layers = upper.clone();
    for l in &lowers {
        layers.push('+');
        layers.push_str(l);
    }
    sc.arg("--remap-rule").arg(format!("overlay:/={layers}+/"));
    match &shebang {
        Some((interp, arg)) => {
            // Script: wrapper runs the interpreter with the kernel's
            // shebang argv shape (interp [arg] script args…).
            sc.arg(interp);
            if let Some(a) = arg { sc.arg(a); }
            sc.arg(&resolved);
            sc.args(&cmd[1..]);
        }
        None => { sc.args(&cmd); }
    }
    if let Some(d) = &chdir { sc.current_dir(d); }
    // Install the wrapper-contract fds in the child only (see above).
    // dup2 targets are not CLOEXEC, so they survive the exec.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        sc.pre_exec(move || {
            if libc::dup2(trace_w, SUD_OUTPUT_FD) < 0
                || libc::dup2(mfd, SUD_STATE_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = match sc.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sarun-engine: exec {wrapper}: {e}\n\
                       hint: build it with `make -C tv sud64 sud32 \
                       SUD_ADDINS=\"sud/trace sud/path_remap sud/cmd-rewrite \
                       sud/fake-exec sud/inramfs\"` and put them on PATH or \
                       point SARUN_SUD64/SARUN_SUD32 at them.");
            return 127;
        }
    };
    // Launcher wait loop (sudtrace's): reap every descendant that lands
    // on us, emit an EV_EXIT per real termination of a thread-group
    // leader, stop when the wrapper child itself is done.
    let child_pid = child.id() as i32;
    let mut ev = crate::sudwire::EvState::default();
    let mut code = 1;
    loop {
        let mut wstatus: i32 = 0;
        let wpid = unsafe { libc::waitpid(-1, &mut wstatus, libc::__WALL) };
        if wpid < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            break; // ECHILD: nothing left to reap
        }
        if wpid == 0 { continue; }
        if !libc::WIFEXITED(wstatus) && !libc::WIFSIGNALED(wstatus) {
            continue; // stopped/continued — keep waiting
        }
        let tgid = sud_proc_field(wpid, "Tgid:", wpid);
        if wpid == tgid || wpid == child_pid {
            let ppid = sud_proc_field(wpid, "PPid:", 0);
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64).unwrap_or(0);
            let buf = ev.build_exit(stream_id, ts, wpid as i64,
                                    tgid as i64, ppid as i64, wstatus);
            let _ = write_all_fd(trace_w, &buf);
        }
        if wpid == child_pid {
            code = if libc::WIFEXITED(wstatus) {
                libc::WEXITSTATUS(wstatus)
            } else {
                128 + libc::WTERMSIG(wstatus)
            };
            break;
        }
    }
    unsafe {
        libc::munmap(state_page.cast(), 4096);
        libc::close(trace_w);
        libc::close(mfd);
    }
    drop(child); // already reaped by our waitpid; Child::drop doesn't wait
    // Sweep the upper into the box's sqlar on a FRESH conn (the register
    // conn is the box channel; a verb on it would desync teardown).
    match UnixStream::connect(&sock) {
        Ok(c) => {
            let req = json!({"type": "sud_ingest", "sid": sid});
            if conn_write_all(&c, format!("{req}\n").as_bytes()) {
                let mut resp = String::new();
                let _ = BufReader::new(&c).read_line(&mut resp);
                match serde_json::from_str::<Value>(&resp) {
                    Ok(v) if v.get("ok").and_then(Value::as_bool)
                        == Some(true) => {
                        let n = v.get("ingested").and_then(Value::as_i64)
                            .unwrap_or(0);
                        eprintln!("sarun-engine: sud sweep: {n} entries \
                                   captured into box {sid}");
                        if let Some(errs) = v.get("errors")
                            .and_then(Value::as_array)
                            .filter(|a| !a.is_empty()) {
                            for e in errs {
                                eprintln!("sarun-engine: sud sweep: {e}");
                            }
                        }
                    }
                    _ => eprintln!("sarun-engine: sud sweep failed: {}",
                                   resp.trim()),
                }
            }
        }
        Err(e) => eprintln!("sarun-engine: sud sweep: dial engine: {e}"),
    }
    drop(conn); // box channel EOF → teardown
    code
}

/// Send one frame (optionally with our pidfd as SCM_RIGHTS) over the box channel.
fn send_frame(conn_fd: i32, frame: &[u8], pidfd: Option<i32>) {
    let Some(fd) = pidfd else {
        // No ancillary fd: a plain stream write, but it MUST go out whole — a
        // short write here desyncs the length-prefixed channel for every later
        // frame. Loop until fully written (audit L4).
        let _ = write_all_fd(conn_fd, frame);
        return;
    };
    let mut iov = libc::iovec {
        iov_base: frame.as_ptr() as *mut libc::c_void,
        iov_len: frame.len(),
    };
    let mut cmsg = [0u8; 32];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping((&fd as *const i32).cast(), libc::CMSG_DATA(c), 4);
        libc::sendmsg(conn_fd, &msg, 0);
    }
}

/// The engine binary to re-exec from INSIDE a box. Prefer `SARUN_EXE` (the
/// inherited-fd path `/proc/self/fd/N` the runner ferried in and published —
/// always reachable, even on a closed rootfs whose files don't include the
/// engine's host path). Fall back to `/proc/self/exe` for the top-level /
/// host case where no box ferried an fd. This is the single source of truth
/// for every in-box re-exec site (inner CMD, oaita driver, brush builtins).
pub fn in_box_self_exe() -> String {
    std::env::var("SARUN_EXE")
        .ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/proc/self/exe".to_string())
}

pub fn inner(conn_fd: i32, capture: bool, pty: bool, brush: bool,
             api: bool, mut cmd: Vec<String>) -> i32 {
    if cmd.is_empty() { return 2; }
    // The box CMD may name `/proc/self/exe` as argv[0] to mean "re-exec the
    // engine" (e.g. `oaita run --on <box>` ships `/proc/self/exe oaita run
    // --inbox …`). Resolve it through the ferried fd so it works when the box
    // rootfs doesn't contain the engine's host path (closed OCI image).
    if cmd[0] == "/proc/self/exe" {
        cmd[0] = in_box_self_exe();
    }
    // Hold the box-channel fd open (not CLOEXEC) so the engine sees EOF — its
    // teardown signal — only when this process (and CMD) finally exits.
    if conn_fd >= 0 { clear_cloexec(conn_fd); }
    // FD broker: bind the abstract UDS named by SARUN_BROKER so box-
    // internal processes (a nested `sarun run`, in-box `oaita`) can ask
    // for their OWN fresh engine connection without us bind-mounting a
    // host path inside the box. The actual FRAME_CONN handoff lives in
    // the mode-specific reader (currently inner_capture); for other
    // inner modes a FRAME_CONN that arrives is closed dropped (the
    // broker accept thread still queues callers but won't be drained).
    if conn_fd >= 0 {
        if let Ok(name) = std::env::var("SARUN_BROKER") {
            if !name.is_empty() {
                inner_broker_serve(conn_fd, &name);
            }
        }
    }
    // --api boxes reach the engine's HTTP proxy by dialing the FD broker
    // (oaita::client::Endpoint::Broker) — no in-box UDS, no per-channel
    // FRAME_API_* mux, the LLM-API conn IS an engine control conn with
    // verb `api.proxy` as its first line. The runner has nothing to set
    // up here; admission control happens at engine register time via
    // Proxy::enable_box.
    let _ = api;
    // -b brush (D9): the box's shell IS the embedded brush, not /bin/sh. It
    // needs the capture sinks (provenance + recorded writes), so it only
    // engages when capture is active; otherwise this is a misuse (-b under -d)
    // and we error VISIBLY rather than silently exec'ing /bin/sh.
    if brush {
        if !capture || conn_fd < 0 {
            eprintln!("sarun-engine inner: -b (brush) requires capture; \
                       it is incompatible with -d (no overlay).");
            return 2;
        }
        return crate::brush::inner_brush(conn_fd, cmd);
    }
    // PTY mode (third path): an interactive controlling-tty box whose output is
    // ALSO captured. `-p` means "give the CHILD a pty" — it does NOT require the
    // runner to have a tty (cf. `script`, `docker -t`, `ssh -tt`): a non-tty
    // runner just gets a degraded bridge (no raw mode / live winsize), the child
    // still runs on a real pty. We honor it or fail visibly — never a silent
    // downgrade to non-pty capture (a box you asked to be interactive would
    // otherwise come back headless without you knowing).
    if pty && capture && conn_fd >= 0 {
        return inner_pty(conn_fd, cmd);
    }
    if !capture || conn_fd < 0 {
        // Passthrough (-t/-d): stdio is inherited. Spawn (not exec) so we can do
        // tty job-control + restore for an interactive shell; the box channel,
        // if any, stays held open by us until the child exits (teardown EOF).
        // Even with no ECHO traffic to consume, we need a broker pump on the
        // channel: an in-box `sarun run NESTED` issued from the child's shell
        // sends FRAME_OPEN_CONN, the engine replies FRAME_CONN+SCM_RIGHTS,
        // and that fd has to get forwarded back to the waiting child. The
        // pump terminates when the channel EOFs (we close it after wait).
        if conn_fd >= 0 {
            std::thread::spawn(move || {
                let mut tmp = [0u8; 4096];
                let mut buf: Vec<u8> = vec![];
                loop {
                    let mut got_fd: Option<i32> = None;
                    let n = recv_box_frame_bytes(conn_fd, &mut tmp, &mut got_fd);
                    if n <= 0 { break; }
                    buf.extend_from_slice(&tmp[..n as usize]);
                    let (frames, used) = crate::frames::decode(&buf);
                    buf.drain(..used);
                    for (ft, _payload) in frames {
                        if ft == crate::frames::FRAME_CONN {
                            if let Some(fd) = got_fd.take() {
                                runner_broker_handoff(fd);
                            }
                        }
                    }
                    if let Some(fd) = got_fd { unsafe { libc::close(fd); } }
                }
            });
        }
        let (tty_fd, old_fg) = tty_grab();
        let mut child = match Command::new(&cmd[0]).args(&cmd[1..])
            .process_group(0).spawn() {
            Ok(c) => c,
            Err(e) => { eprintln!("sarun-engine inner: spawn {}: {e}", cmd[0]); return 127; }
        };
        let old_ttou = tty_give(tty_fd, child.id() as i32);
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
        tty_restore(tty_fd, old_fg, old_ttou);
        return code;
    }
    inner_capture(conn_fd, cmd)
}

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;

// ── FD broker (runner side) ────────────────────────────────────────────────
//
// The inner serves an abstract UDS inside the box's netns for box-internal
// processes that need their OWN engine connection (a nested `sarun run`, an
// oaita CLI invocation from a shell, etc.). The abstract name lives in the
// SARUN_BROKER env var, which bwrap propagates to every child. Protocol:
//
//   child → inner   : connect to abstract @SARUN_BROKER; write nothing
//                     (a connect is the whole request).
//   inner → engine  : FRAME_OPEN_CONN on the box-channel.
//   engine → inner  : FRAME_CONN with SCM_RIGHTS-attached fd (the runner
//                     side of a fresh handler socketpair).
//   inner → child   : sendmsg the received fd via SCM_RIGHTS on the child's
//                     conn; close our copy. The child wraps it as a
//                     UnixStream and does the register handshake on it.
//
// FIFO is preserved across both legs so the engine's reply lands at the
// child that asked for it. Attribution is intrinsic — the channel IS the
// box.

static BROKER_QUEUE: OnceLock<Mutex<VecDeque<std::os::unix::net::UnixStream>>>
    = OnceLock::new();

/// Bind the broker's abstract UDS inside the box and spawn the accept thread.
/// `abstract_name` is the SARUN_BROKER name (bwrap propagated it to us and to
/// every box child). Idempotent.
fn inner_broker_serve(conn_fd: i32, abstract_name: &str) {
    if BROKER_QUEUE.get().is_some() { return; }
    use std::os::linux::net::SocketAddrExt;
    let addr = match std::os::unix::net::SocketAddr::from_abstract_name(
        abstract_name.as_bytes()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sarun-engine inner: broker addr: {e}");
            return;
        }
    };
    let listener = match std::os::unix::net::UnixListener::bind_addr(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sarun-engine inner: broker bind {abstract_name}: {e}");
            return;
        }
    };
    let _ = BROKER_QUEUE.set(Mutex::new(VecDeque::new()));
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            // Queue first, THEN send the request, so the reader's FRAME_CONN
            // handler is guaranteed to find a waiter.
            BROKER_QUEUE.get().unwrap().lock().unwrap().push_back(client);
            send_frame(conn_fd,
                &crate::frames::encode(crate::frames::FRAME_OPEN_CONN, &[]),
                None);
        }
    });
}

/// Handle a FRAME_CONN: pop the front-of-queue waiter and forward `fd` to it
/// via SCM_RIGHTS. Closes our copy. If nothing is waiting (shouldn't happen
/// in normal flow), drops the fd.
fn runner_broker_handoff(fd: i32) {
    let waiter = BROKER_QUEUE.get()
        .and_then(|q| q.lock().unwrap().pop_front());
    let Some(client) = waiter else {
        unsafe { libc::close(fd); }
        return;
    };
    // One-byte body so the SCM cmsg has data to ride on. The child ignores
    // the byte; it cares only about the attached fd.
    let body = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: body.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let mut cmsg = [0u8; 32];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping((&fd as *const i32).cast(),
                                       libc::CMSG_DATA(c), 4);
        libc::sendmsg(client.as_raw_fd(), &msg, 0);
        libc::close(fd);
    }
}

/// recvmsg analogue of the engine-side helper: read up to `buf.len()` bytes
/// off `raw` (the box-channel) AND extract the first SCM_RIGHTS fd (a
/// FRAME_CONN attaches one). Returns the byte count (0 = EOF, <0 = error)
/// and sets `*fd` if one came in.
fn recv_box_frame_bytes(raw: i32, buf: &mut [u8], fd: &mut Option<i32>) -> isize {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len() as _;
    let n = unsafe { libc::recvmsg(raw, &mut msg, 0) };
    if n > 0 {
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET
                    && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let mut got = 0i32;
                    std::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(c), (&mut got as *mut i32).cast(), 4);
                    if fd.is_none() { *fd = Some(got); }
                    else { libc::close(got); }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
    }
    n
}

/// Dial the broker via abstract UDS named by SARUN_BROKER; recvmsg the
/// SCM_RIGHTS-attached engine conn fd; wrap as a UnixStream. Used by
/// in-box `sarun run` and any other in-box engine client.
pub fn broker_dial(abstract_name: &str) -> std::io::Result<UnixStream> {
    use std::os::fd::FromRawFd;
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(
        abstract_name.as_bytes())?;
    let conn = UnixStream::connect_addr(&addr)?;
    // recvmsg the one-byte body + SCM_RIGHTS fd. Loop tolerates EINTR.
    let mut buf = [0u8; 1];
    let mut cmsg = [0u8; 64];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len() as _;
    let n = unsafe { libc::recvmsg(conn.as_raw_fd(), &mut msg, 0) };
    if n <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut got: Option<i32> = None;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET
                && (*c).cmsg_type == libc::SCM_RIGHTS {
                let mut f = 0i32;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(c), (&mut f as *mut i32).cast(), 4);
                if got.is_none() { got = Some(f); }
                else { libc::close(f); }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    drop(conn);
    let fd = got.ok_or_else(|| std::io::Error::new(
        std::io::ErrorKind::Other, "broker reply: no fd attached"))?;
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

fn inner_capture(conn_fd: i32, cmd: Vec<String>) -> i32 {
    // Capture via the echo mux: the child's stdout/stderr ARE the box-root sink
    // files, so every write flows THROUGH the overlay (recorded with per-write
    // pid attribution). The engine frames those bytes back as ECHO; we replay
    // them to our real fd 1/2 for live, upward-chaining visibility. stdin stays
    // inherited. A nested box's echo readback hits the PARENT sink, so we MUTE
    // our own host pid first: the parent echoes the bytes onward but never
    // re-records them (already captured once here, the origin box).
    use std::os::fd::FromRawFd;
    use std::os::fd::IntoRawFd;
    use std::process::Stdio;
    let out = match std::fs::OpenOptions::new().write(true).open("/.slopbox-stdout") {
        Ok(f) => f, Err(e) => { eprintln!("inner: open stdout sink: {e}"); return 127; }
    };
    let err = match std::fs::OpenOptions::new().write(true).open("/.slopbox-stderr") {
        Ok(f) => f, Err(e) => { eprintln!("inner: open stderr sink: {e}"); return 127; }
    };
    // MUTE: tell the engine not to RECORD writes by our host pid (only echo
    // them) — sent before the child can emit a byte that loops back to us.
    let pidfd = pidfd_open(std::process::id() as i32);
    if pidfd >= 0 {
        send_frame(conn_fd, &crate::frames::encode(crate::frames::FRAME_MUTE, &[]),
                   Some(pidfd));
        unsafe { libc::close(pidfd); }
    }
    // Reader: ECHO frames → real fd 1/2; ECHO_DONE → stop. Runs until the engine
    // closes the channel or flags ECHO_DONE (all captured bytes framed).
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    let rfd = conn_fd;
    let reader = std::thread::spawn(move || {
        let mut buf: Vec<u8> = vec![];
        let mut tmp = [0u8; 65536];
        loop {
            // recvmsg (not plain read) so we can pick up the SCM_RIGHTS fd a
            // FRAME_CONN brings along. Best effort: at most one fd per recvmsg
            // — the engine attaches one per sendmsg, so we associate it with
            // the first FRAME_CONN frame in this batch.
            let mut got_fd: Option<i32> = None;
            let n = recv_box_frame_bytes(rfd, &mut tmp, &mut got_fd);
            if n <= 0 { break; }
            buf.extend_from_slice(&tmp[..n as usize]);
            let (frames, used) = crate::frames::decode(&buf);
            buf.drain(..used);
            for (ft, payload) in frames {
                if ft == crate::frames::FRAME_CONN {
                    if let Some(fd) = got_fd.take() {
                        runner_broker_handoff(fd);
                    }
                    continue;
                }
                if ft == crate::frames::FRAME_ECHO && !payload.is_empty() {
                    let realfd = if payload[0] == 1 { 2 } else { 1 };
                    // Replay the full echo: a short write would silently truncate
                    // the box's live, upward-chaining output (audit L4).
                    let _ = write_all_fd(realfd, &payload[1..]);
                } else if ft == crate::frames::FRAME_ECHO_DONE {
                    done2.store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
            // A FRAME_CONN sent with no matching waiter (e.g. lost-race or
            // engine sent an extra) — close the dangling fd rather than leak.
            if let Some(fd) = got_fd { unsafe { libc::close(fd); } }
        }
    });
    // tty job-control: child in its own group, given the terminal foreground
    // (stdin stays the inherited tty; stdout/stderr are the sinks).
    let (tty_fd, old_fg) = tty_grab();
    let child = unsafe {
        Command::new(&cmd[0]).args(&cmd[1..])
            .process_group(0)
            .stdout(Stdio::from_raw_fd(out.into_raw_fd()))
            .stderr(Stdio::from_raw_fd(err.into_raw_fd()))
            .spawn()
    };
    let mut child = match child {
        Ok(c) => c,
        Err(e) => { eprintln!("sarun-engine inner: spawn {}: {e}", cmd[0]); return 127; }
    };
    let old_ttou = tty_give(tty_fd, child.id() as i32);
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    tty_restore(tty_fd, old_fg, old_ttou);
    // The child's sink fds are now closed (it exited): the engine will frame any
    // remaining ECHO then ECHO_DONE. Wait briefly for the reader to drain so the
    // tail of the output isn't truncated, then unmute and let the channel close.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done.load(std::sync::atomic::Ordering::SeqCst)
        && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    send_frame(conn_fd, &crate::frames::encode(crate::frames::FRAME_UNMUTE, &[]), None);
    let _ = reader;
    code
}

// ── PTY mode (third execution path) ──────────────────────────────────────────
// An interactive controlling-tty box whose output is ALSO captured. We allocate
// a pty, give the child the SLAVE as stdin/stdout/stderr + controlling terminal
// (setsid + TIOCSCTTY in a pre_exec), put OUR real stdin into raw mode, and
// bridge bidirectionally:
//   real stdin  → pty master   (keystrokes reach the child)
//   pty master  → real stdout  (live, the user sees the child's tty output)
//               → the box stdout sink file (so it is RECORDED into outputs,
//                 exactly like capture mode — the engine attributes the write to
//                 us, the non-muted runner pid, then echoes it back; we DISCARD
//                 the echo because the master already gave us the live bytes).
// Window size is propagated initially (TIOCGWINSZ→TIOCSWINSZ) and on SIGWINCH.
// On exit we restore termios + the terminal foreground group.

static WINCH_FLAG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
extern "C" fn on_winch(_sig: i32) {
    WINCH_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Read the window size of `fd` (a tty), if any.
fn get_winsize(fd: i32) -> Option<libc::winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if r == 0 { Some(ws) } else { None }
}

fn set_winsize(fd: i32, ws: &libc::winsize) {
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, ws as *const libc::winsize); }
}

fn inner_pty(conn_fd: i32, cmd: Vec<String>) -> i32 {
    use std::os::fd::FromRawFd;
    use std::process::Stdio;

    // The real terminal among our fds (its size + termios we mirror/restore).
    let real_tty = (0..3).find(|&fd| unsafe { libc::isatty(fd) } == 1).unwrap_or(0);

    // Allocate the pty pair, seeding the master's window size from the real tty.
    let mut master: i32 = -1;
    let mut slave: i32 = -1;
    let initial_ws = get_winsize(real_tty);
    let ws_ptr = initial_ws.as_ref()
        .map(|w| w as *const libc::winsize).unwrap_or(std::ptr::null());
    let rc = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(),
                      std::ptr::null(), ws_ptr)
    };
    if rc != 0 {
        // Can't honor -p at all: error VISIBLY rather than silently producing a
        // non-pty box (per the no-silent-downgrade rule).
        eprintln!("sarun-engine inner: -p requested but openpty failed: {}",
                  std::io::Error::last_os_error());
        return 1;
    }

    // Open the box stdout sink: bytes we write here flow through the overlay and
    // are RECORDED (per-write pid attribution → us, the runner). The child's tty
    // I/O goes to the master/slave, not here; we relay master→sink ourselves.
    let sink = match std::fs::OpenOptions::new().write(true).open("/.slopbox-stdout") {
        Ok(f) => f,
        Err(e) => {
            eprintln!("sarun-engine inner: -p capture sink unavailable: {e}");
            unsafe { libc::close(master); libc::close(slave); }
            return 1;
        }
    };

    // Put the real terminal into raw mode so keystrokes pass through unbuffered
    // and unechoed (the child's tty does its own echo). Saved for restore.
    let mut saved_termios: libc::termios = unsafe { std::mem::zeroed() };
    let have_termios = unsafe { libc::tcgetattr(real_tty, &mut saved_termios) } == 0;
    if have_termios {
        let mut raw = saved_termios;
        unsafe { libc::cfmakeraw(&mut raw); }
        unsafe { libc::tcsetattr(real_tty, libc::TCSANOW, &raw); }
    }

    // SIGWINCH → propagate the new size to the master on the next loop tick.
    unsafe { libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t); }

    // Spawn the child with the SLAVE as stdin/stdout/stderr and as its
    // controlling terminal: new session (setsid) + TIOCSCTTY in a pre_exec.
    let slave_for_pre = slave;
    let child = unsafe {
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..])
            .stdin(Stdio::from_raw_fd(libc::dup(slave)))
            .stdout(Stdio::from_raw_fd(libc::dup(slave)))
            .stderr(Stdio::from_raw_fd(libc::dup(slave)));
        c.pre_exec(move || {
            // Own session → no controlling tty inherited; then claim the slave.
            if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()); }
            if libc::ioctl(slave_for_pre, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
        c.spawn()
    };
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sarun-engine inner: spawn {}: {e}", cmd[0]);
            if have_termios { unsafe { libc::tcsetattr(real_tty, libc::TCSANOW, &saved_termios); } }
            unsafe { libc::close(master); libc::close(slave); }
            return 127;
        }
    };
    // The child holds its own dup'd slave fds; close ours so the master sees EOF
    // when the child exits (otherwise the master read never ends).
    unsafe { libc::close(slave); }

    // Drain the box channel (engine echoes our sink writes back as ECHO; we
    // DISCARD them — the master is our live source). Watch for ECHO_DONE.
    let rfd = conn_fd;
    let drainer = std::thread::spawn(move || {
        let mut buf: Vec<u8> = vec![];
        let mut tmp = [0u8; 65536];
        loop {
            // recvmsg so a FRAME_CONN's SCM_RIGHTS fd reaches the FD broker
            // — the in-box dialer handoff. Otherwise identical to the
            // capture/brush readers, with the PTY-specific subtlety that
            // the master is our live source for ECHO bytes, so the engine's
            // FRAME_ECHO copies are discarded here (we wait only for the
            // ECHO_DONE marker to know capture has flushed).
            let mut got_fd: Option<i32> = None;
            let n = recv_box_frame_bytes(rfd, &mut tmp, &mut got_fd);
            if n <= 0 { break; }
            buf.extend_from_slice(&tmp[..n as usize]);
            let (frames, used) = crate::frames::decode(&buf);
            buf.drain(..used);
            for (ft, _payload) in frames {
                if ft == crate::frames::FRAME_CONN {
                    if let Some(fd) = got_fd.take() {
                        runner_broker_handoff(fd);
                    }
                    continue;
                }
                if ft == crate::frames::FRAME_ECHO_DONE { return; }
            }
            if let Some(fd) = got_fd { unsafe { libc::close(fd); } }
        }
    });

    // Bidirectional bridge. We poll master + real stdin; on master EOF the child
    // has closed all slave fds (it has exited or is exiting). A SIGWINCH flag
    // resize is applied on each tick. Bounded by the child's lifetime.
    let stdin_fd = 0;
    let mut master_eof = false;
    // Stop watching stdin once it hits EOF/HUP (a piped or /dev/null runner
    // stdin would otherwise stay perpetually "ready" and busy-spin the poll).
    // poll() ignores a pollfd whose fd is negative, so we flip it to -1.
    let mut stdin_open = true;
    while !master_eof {
        if WINCH_FLAG.swap(false, std::sync::atomic::Ordering::SeqCst) {
            if let Some(ws) = get_winsize(real_tty) { set_winsize(master, &ws); }
        }
        let mut fds = [
            libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if stdin_open { stdin_fd } else { -1 },
                           events: libc::POLLIN, revents: 0 },
        ];
        let pr = unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) };
        if pr < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; } // SIGWINCH
            break;
        }
        // master → real stdout (live) + sink (recorded)
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut b = [0u8; 65536];
            let n = unsafe { libc::read(master, b.as_mut_ptr().cast(), b.len()) };
            if n <= 0 { master_eof = true; }
            else {
                let s = &b[..n as usize];
                // Full write to the live tty: a short write would drop part of
                // the child's terminal output (audit L4).
                let _ = write_all_fd(1, s);
                // Recorded copy: a real write through the FUSE sink (captured).
                let _ = (&sink).write_all(s);
            }
        }
        // real stdin → master (keystrokes). On EOF/HUP stop polling stdin but
        // keep relaying master output until the child exits.
        if stdin_open && fds[1].revents & libc::POLLIN != 0 {
            let mut b = [0u8; 65536];
            let n = unsafe { libc::read(stdin_fd, b.as_mut_ptr().cast(), b.len()) };
            if n > 0 {
                // Feed every keystroke byte to the pty master; a short write
                // would drop input the child never sees (audit L4).
                let _ = write_all_fd(master, &b[..n as usize]);
            } else { stdin_open = false; } // EOF
        }
        if stdin_open && fds[1].revents & (libc::POLLHUP | libc::POLLERR
                                           | libc::POLLNVAL) != 0 {
            stdin_open = false;
        }
    }

    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    // Close the sink so the engine flushes ECHO_DONE; restore the terminal.
    drop(sink);
    if have_termios { unsafe { libc::tcsetattr(real_tty, libc::TCSANOW, &saved_termios); } }
    unsafe { libc::signal(libc::SIGWINCH, libc::SIG_DFL); }
    unsafe { libc::close(master); }
    // Drain the channel briefly so the recorded tail isn't lost, then let it go.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !drainer.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let _ = drainer;
    code
}
