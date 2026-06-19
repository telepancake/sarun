// The box runner, ported to Rust (the ECHO/capture mux is a follow-on).
// Supports NESTED launch: a `run` invoked inside a running box reaches the
// engine via the socket bind-mounted at UI_SOCK_INBOX, registers with a
// `relname` + its own pidfd (so the engine derives the enclosing box from the
// /proc ancestry), and roots bwrap on the parent-exposed /<KIDS_DIR>/<id>.
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

/// The host control socket, bind-mounted read-only into every box at this fixed
/// path so a NESTED runner (one launched inside a running box) can reach the
/// engine. Path-presence is the sole in-box signal — no env var.
// Bound by bwrap as the in-box mirror of the engine's host control socket.
// /tmp is now an --api-box symlink (see Overlay::resolve) and bwrap can't
// create a bind destination under a symlink — so this lives under /run/
// sarun/, outside /tmp. Host runners use the filesystem path or the
// abstract socket; in-box runners go through this bind. -n boxes (private
// netns) lose the abstract path but still reach the engine through this
// bind, so the control channel survives netns unsharing.
const UI_SOCK_INBOX: &str = "/run/sarun/ui.sock";

/// Helper: dial the engine's abstract Unix socket (no filesystem). The name
/// matches `crate::control::abstract_name(sock_path)` — `sarun:<path>`.
/// Reachable only inside the same netns the engine is running in; in-box
/// callers that unshared netns fall through to the filesystem path at
/// UI_SOCK_INBOX.
pub fn abstract_connect(sock: &std::path::Path) -> std::io::Result<UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    let name = crate::control::abstract_name(sock);
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    UnixStream::connect_addr(&addr)
}
const KIDS_DIR: &str = ".slopbox-kids";

/// Standard CA bundle paths the augmented bundle is bound over. Distro
/// coverage as of 2026: Debian/Ubuntu (ca-certificates.crt), RHEL/Fedora
/// (tls/certs/ca-bundle.crt + the .pem twin), Alpine (cert.pem). `--ro-bind-try`
/// silently skips paths the box's filesystem doesn't ship.
const CA_BUNDLE_TARGETS: &[&str] = &[
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
/// newline-terminated JSON line over the engine control socket at UI_SOCK_INBOX,
/// carrying ITS OWN pidfd as SCM_RIGHTS so the engine resolves the enclosing box
/// from the shim's /proc ancestry (the same identity path register uses). This
/// is a one-shot control message — NOT a register, NOT a box channel: the engine
/// records the recipe's brushprov rows and closes. Best-effort: any failure (no
/// socket, send error) is swallowed so the recipe still runs unchanged. `line`
/// must already be newline-terminated.
pub fn send_nested_prov(line: &[u8]) {
    let Ok(conn) = UnixStream::connect(UI_SOCK_INBOX) else { return; };
    let pidfd = pidfd_open(std::process::id() as i32);
    send_register(&conn, line, pidfd);
    if pidfd >= 0 { unsafe { libc::close(pidfd); } }
    // Drain the engine's one-line ack (best-effort) so it isn't an abrupt RST.
    let mut s = String::new();
    let _ = BufReader::new(&conn).read_line(&mut s);
}
/// send_frame, exposed for the brush module's MUTE/UNMUTE/teardown frames.
pub fn send_frame_pub(conn_fd: i32, frame: &[u8], pidfd: Option<i32>) {
    send_frame(conn_fd, frame, pidfd)
}

/// Send one register line plus our own pidfd as SCM_RIGHTS ancillary data, so
/// the engine derives our HOST-namespace pid from /proc/self/fdinfo/<pidfd>
/// (the wrap-immune identity path) — correct for both top-level and nested
/// runners, where our own getpid() is a parent-namespace pid the engine can't
/// use. Returns false on write error.
fn send_register(conn: &UnixStream, line: &[u8], pidfd: i32) -> bool {
    if pidfd < 0 {
        return conn_write_all(conn, line);
    }
    let mut iov = libc::iovec {
        iov_base: line.as_ptr() as *mut libc::c_void,
        iov_len: line.len(),
    };
    let mut cmsg = [0u8; 32]; // CMSG_SPACE(sizeof(i32)) rounded up
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
        std::ptr::copy_nonoverlapping(
            (&pidfd as *const i32).cast(), libc::CMSG_DATA(c), 4);
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
    // IN-BOX vs HOST: a nested runner reaches the engine via the socket
    // bind-mounted at UI_SOCK_INBOX (works regardless of netns sharing —
    // -n boxes with private netns still reach the engine through this
    // bind). A top-level runner uses the host filesystem socket OR the
    // abstract socket; abstract is preferred for speed but the filesystem
    // path is the long-term contract for host UIs.
    let in_box = std::path::Path::new(UI_SOCK_INBOX).exists();
    let sock = if in_box { std::path::PathBuf::from(UI_SOCK_INBOX) }
               else { paths::sock_path() };
    let conn = match (if in_box {
        UnixStream::connect(&sock)
    } else {
        abstract_connect(&sock).or_else(|_| UnixStream::connect(&sock))
    }) {
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
    let pidfd = pidfd_open(std::process::id() as i32);
    if !send_register(&conn, format!("{reg}\n").as_bytes(), pidfd) {
        eprintln!("sarun-engine: register write failed");
        return 1;
    }
    if pidfd >= 0 { unsafe { libc::close(pidfd); } }
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
    let box_name_str = ack.get("name").and_then(Value::as_str).unwrap_or("").to_string();
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
    // -C overrides the box's working directory; else the image's WorkingDir;
    // else our own cwd.
    let cwd = chdir
        .or(oci_cwd)
        .unwrap_or_else(|| std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into()));
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
    // Forward the engine socket into the box at the fixed inbox path so a
    // DEEPER nested runner can reach the engine. Lives under /run/sarun/
    // rather than under /tmp (the /tmp redirect for --api boxes would
    // collide with bwrap's bind destination there).
    let sock_src = if in_box { UI_SOCK_INBOX.to_string() }
                   else { paths::sock_path().to_string_lossy().into_owned() };
    let fd_s = fd.to_string();
    // Honor the engine's capture decision (it downgrades for -t/-d): only pass
    // --capture to inner when the ack confirms capture is active.
    let capture_on = want_capture
        && ack.get("capture").and_then(Value::as_bool).unwrap_or(false);
    // Bind the engine binary into the box at a fixed path next to the socket
    // and exec --inner from THAT path, not from the host path. The host path
    // happens to resolve through a regular box's lower-chain fall-through to
    // host, but a `--no-parent` box has no fall-through — without this bind,
    // bwrap fails with execvp ENOENT on the engine. The bind is harmless for
    // ordinary boxes (a redundant ro-bind onto the already-resolvable path).
    // Moved out of /tmp for the same reason ui.sock did — /tmp is a
    // per-box symlink in --api boxes (Overlay::resolve's substitute) and
    // bwrap can't create bind destinations inside a symlink. /run/sarun/
    // is the new tenancy for engine-injected paths.
    let inner_exe = "/run/sarun/engine";
    let mut inner_args: Vec<&str> = vec![
        inner_exe, "inner", "--conn-fd", &fd_s];
    if capture_on { inner_args.push("--capture"); }
    // PTY needs the capture sink files to record into; if the engine declined
    // capture (-d) there is nothing to PTY into, so gate --pty on capture_on.
    if pty && capture_on { inner_args.push("--pty"); }
    // -b brush likewise needs the capture sinks to record provenance + writes.
    if brush && capture_on { inner_args.push("--brush"); }
    // --api flag passes through to inner so it knows to serve the in-box
    // /run/sarun/api.sock UDS, framing each accepted client as FRAME_API_*
    // on the existing box-channel. One conn per box (the channel itself),
    // no second host UDS, attribution is implicit.
    if api_on { inner_args.push("--api"); }
    inner_args.push("--");
    let mut bwrap = Command::new("bwrap");
    bwrap.args(["--bind", &root_src, "/",
                "--proc", "/proc", "--dev", "/dev",
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
    bwrap.args(["--ro-bind", &sock_src, UI_SOCK_INBOX,
                "--ro-bind", &self_exe, inner_exe]);
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
    // --api: the inner (running INSIDE the box's mount namespace) serves
    // /run/sarun/api.sock for box processes and tunnels each accepted
    // connection as FRAME_API_OPEN/DATA/CLOSE on the existing box-channel.
    // Two consequences:
    //   * Zero host UDSes beyond ui.sock — the box never sees a path to
    //     the engine's control socket, can't dial out to anything host-
    //     side, and a nested act delegation works without a wandering
    //     host-path bind-mount.
    //   * Attribution is implicit — every API call on this channel comes
    //     from this box, no peer-pid walk on the engine side.
    if api_on {
        bwrap.args(["--setenv", "OAITA_API_SOCK", "/run/sarun/api.sock"]);
        // The in-box oaita client also needs a base_url string to satisfy
        // its parser — anything non-empty works because the UDS endpoint
        // wins over it.
        bwrap.args(["--setenv", "OPENAI_BASE_URL", "http://oaita-proxy/v1"]);
        // Expose the engine binary inside the box as both `oaita` and
        // `sarun` so an in-box `oaita run X` reaches the symlinked-as-oaita
        // dispatch (and a nested `sarun ...` for the shell executor reaches
        // the normal subcommand path). Both shadow over /usr/local/bin —
        // standard PATH on every distro we target.
        bwrap.args(["--ro-bind", &self_exe, "/usr/local/bin/oaita"]);
        bwrap.args(["--ro-bind", &self_exe, "/usr/local/bin/sarun"]);
        // Forward the trace endpoint into the box. bwrap clears the env
        // by default; without this --setenv the sub-agent's oaita
        // process can't find $OAITA_TRACE and its gen.request /
        // gen.reply events vanish.
        //
        // Netns semantics:
        //   /path     filesystem-socket UDS — works IF we also bind-mount
        //             the socket file into the box at the same path.
        //             Abstract @name socket would NOT work because --api
        //             boxes get --unshare-net by default (NetMode::Off)
        //             and abstract Unix is netns-scoped.
        //   @name     would require the box share host netns (NetMode::Host
        //             at run time). We just forward — build_sink silently
        //             no-ops when unreachable.
        if let Ok(ep) = std::env::var("OAITA_TRACE") {
            if !ep.is_empty() {
                bwrap.args(["--setenv", "OAITA_TRACE", &ep]);
                // Filesystem-path UDS: bind the socket file into the box
                // at the same path so the sub-agent can send to it.
                if ep.starts_with('/') && std::path::Path::new(&ep).exists() {
                    bwrap.args(["--ro-bind", &ep, &ep]);
                }
            }
        }
        // Same logic for OAITA_DEPTH — set by the parent's act_script;
        // a deeper level needs to see it.
        if let Ok(d) = std::env::var("OAITA_DEPTH") {
            if !d.is_empty() {
                bwrap.args(["--setenv", "OAITA_DEPTH", &d]);
            }
        }
    }
    bwrap.args(["--unshare-pid", "--unshare-ipc", "--unshare-uts",
                "--die-with-parent"]);
    // Netns dispatch (driven by ack info the engine already prepared based on
    // the register's net_mode):
    //   Off  → bwrap --unshare-net  (brand new empty netns; dials fail closed)
    //   Tap  → engine pre-equipped a netns; ack carries its /proc/<a>/ns/net.
    //          We open the fd here and Command::pre_exec setns(2)'s the bwrap
    //          child into it. No --unshare-net → inherits the equipped netns.
    //   Host → no --unshare-net (box shares the engine's host netns).
    let netns_path = ack.get("netns_path").and_then(Value::as_str)
        .map(|s| s.to_string());
    let dns_ip = ack.get("dns_ip").and_then(Value::as_str)
        .map(|s| s.to_string()).unwrap_or_default();
    let ca_pem_path = ack.get("ca_pem_path").and_then(Value::as_str)
        .map(|s| s.to_string()).unwrap_or_default();
    match net_mode {
        crate::net::NetMode::Off => { bwrap.arg("--unshare-net"); }
        crate::net::NetMode::Host => { /* leave host netns */ }
        crate::net::NetMode::Tap => {
            if let Some(p) = netns_path.clone() {
                // Open the netns fd once HERE (parent), then setns in pre_exec
                // in the child. The fd is kept open for the lifetime of the
                // bwrap child via FD_CLOEXEC clear → bwrap inherits it.
                let fd = unsafe { libc::open(
                    std::ffi::CString::new(p.as_bytes()).unwrap().as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC) };
                if fd < 0 {
                    eprintln!("sarun-engine: open netns {p}: {}",
                              std::io::Error::last_os_error());
                    return 1;
                }
                use std::os::unix::process::CommandExt;
                unsafe {
                    bwrap.pre_exec(move || {
                        if libc::setns(fd, libc::CLONE_NEWNET) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            } else {
                eprintln!("sarun-engine: -n requested but engine returned no \
                           netns_path");
                return 1;
            }
        }
    }
    if !ca_pem_path.is_empty() {
        // CA bundle augmentation (sakar parity): bind the augmented bundle
        // over each common system CA path. The env vars need to point at a
        // path that exists INSIDE the box — use the canonical Debian/Ubuntu
        // one (it's the most common and we also bind it). We bind it
        // unconditionally (NOT --ro-bind-try) so the env-var path is
        // guaranteed to resolve.
        let canonical_inside = "/etc/ssl/certs/ca-certificates.crt";
        bwrap.args(["--ro-bind", &ca_pem_path, canonical_inside]);
        for tgt in CA_BUNDLE_TARGETS {
            if *tgt == canonical_inside { continue; }
            bwrap.arg("--ro-bind-try").arg(&ca_pem_path).arg(tgt);
        }
        for (k, v) in [("SSL_CERT_FILE", canonical_inside),
                       ("CURL_CA_BUNDLE", canonical_inside),
                       ("NODE_EXTRA_CA_CERTS", canonical_inside),
                       ("REQUESTS_CA_BUNDLE", canonical_inside),
                       ("GIT_SSL_CAINFO", canonical_inside)] {
            bwrap.args(["--setenv", k, v]);
        }
    }
    if !dns_ip.is_empty() {
        // resolv.conf override: the box's stub resolver dials the engine's
        // gateway IP for DNS. One synthetic file under the runner's tempdir,
        // bound over /etc/resolv.conf inside the box.
        let resolv = format!("nameserver {dns_ip}\n");
        let tmp = std::env::temp_dir().join(format!("sarun-resolv-{}", std::process::id()));
        let _ = std::fs::write(&tmp, resolv);
        let tmp_s = tmp.to_string_lossy().into_owned();
        bwrap.args(["--ro-bind", &tmp_s, "/etc/resolv.conf"]);
    }
    bwrap.args(["--chdir", &cwd, "--"]);
    let status = bwrap.args(&inner_args).args(&cmd).status();
    drop(conn); // our copy; inner (in the box) is the channel's sole holder now
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => { eprintln!("sarun-engine: bwrap failed: {e}"); 1 }
    }
}

/// Send one frame (optionally with our pidfd as SCM_RIGHTS) over the box channel.
fn send_frame(conn_fd: i32, frame: &[u8], pidfd: Option<i32>) {
    let Some(fd) = pidfd else {
        unsafe { libc::write(conn_fd, frame.as_ptr().cast(), frame.len()); }
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

pub fn inner(conn_fd: i32, capture: bool, pty: bool, brush: bool,
             api: bool, cmd: Vec<String>) -> i32 {
    if cmd.is_empty() { return 2; }
    // Hold the box-channel fd open (not CLOEXEC) so the engine sees EOF — its
    // teardown signal — only when this process (and CMD) finally exits.
    if conn_fd >= 0 { clear_cloexec(conn_fd); }
    // --api: spin up the in-box LLM-API proxy listener BEFORE we hand the
    // box channel to the chosen inner mode. The listener owns its own
    // background thread and frames each accepted box-side connection as
    // FRAME_API_* over the conn fd; the inner mode's existing reader will
    // demux engine→runner FRAME_API_DATA back to the right box-side conn.
    // See inner_api_serve.
    if api && conn_fd >= 0 {
        inner_api_serve(conn_fd);
    }
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

// ── in-box oaita API mux (runner side) ──────────────────────────────────────
//
// The inner process serves /run/sarun/api.sock for box-internal `oaita`
// clients and tunnels each accepted connection as FRAME_API_* over the
// existing box channel. ONE process per box (the inner), ONE conn to the
// engine — every API call rides the box-channel as logical streams. There
// is no second host UDS the box can reach, and no path inside the box that
// leads to the engine's control socket.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Per-process API mux. Box-internal — there is exactly one inner process
/// per box, so a static singleton is the natural home.
struct RunnerApiMux {
    next_id: std::sync::atomic::AtomicU32,
    conn_fd: i32,
    /// stream_id → write half of the accepted box-side conn. Wrapped so the
    /// demux thread (which reads FRAME_API_DATA bytes off conn_fd) can write
    /// response bytes to the right box conn without juggling per-stream
    /// channels.
    streams: std::sync::Mutex<HashMap<u32, std::os::unix::net::UnixStream>>,
}

static RUNNER_API: OnceLock<RunnerApiMux> = OnceLock::new();

/// Bind /run/sarun/api.sock inside the box and spawn the accept thread.
/// `conn_fd` is the box-channel raw fd (the inner already holds it).
fn inner_api_serve(conn_fd: i32) {
    if RUNNER_API.get().is_some() { return; }
    let _ = std::fs::create_dir_all("/run/sarun");
    let sock_path = "/run/sarun/api.sock";
    let _ = std::fs::remove_file(sock_path);
    let listener = match std::os::unix::net::UnixListener::bind(sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sarun-engine inner: api.sock bind: {e}");
            return;
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(sock_path,
        std::fs::Permissions::from_mode(0o666));
    let _ = RUNNER_API.set(RunnerApiMux {
        next_id: std::sync::atomic::AtomicU32::new(1),
        conn_fd,
        streams: std::sync::Mutex::new(HashMap::new()),
    });
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || handle_box_api_conn(conn));
        }
    });
}

/// Box-side API conn lifecycle: assign stream_id, frame OPEN, copy bytes
/// box→engine as DATA frames, frame CLOSE on EOF. The demux side
/// (`runner_api_dispatch`) feeds engine→box bytes onto the same conn via
/// the stream map.
fn handle_box_api_conn(mut conn: std::os::unix::net::UnixStream) {
    let Some(mux) = RUNNER_API.get() else { return; };
    let id = mux.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Register the conn's write side BEFORE OPEN — engine may start
    // responding immediately after the upstream answers.
    let conn_clone = match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    mux.streams.lock().unwrap().insert(id, conn_clone);
    send_frame(mux.conn_fd,
        &crate::frames::encode(crate::frames::FRAME_API_OPEN,
                               &crate::frames::api_id_payload(id)),
        None);
    use std::io::Read;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                send_frame(mux.conn_fd,
                    &crate::frames::encode(crate::frames::FRAME_API_DATA,
                                           &crate::frames::api_data_payload(id, &buf[..n])),
                    None);
            }
        }
    }
    send_frame(mux.conn_fd,
        &crate::frames::encode(crate::frames::FRAME_API_CLOSE,
                               &crate::frames::api_id_payload(id)),
        None);
    mux.streams.lock().unwrap().remove(&id);
}

/// Called from the inner-mode reader thread when an API frame arrives off
/// conn_fd. Returns true if the frame was consumed (so the reader can
/// skip its own dispatch for it).
fn runner_api_dispatch(ft: u8, payload: &[u8]) -> bool {
    let Some(mux) = RUNNER_API.get() else { return false; };
    let Some((stream_id, body)) = crate::frames::api_parse(payload) else {
        return matches!(ft, crate::frames::FRAME_API_OPEN
                          | crate::frames::FRAME_API_DATA
                          | crate::frames::FRAME_API_CLOSE);
    };
    match ft {
        crate::frames::FRAME_API_DATA => {
            use std::io::Write;
            let conn = mux.streams.lock().unwrap().get(&stream_id)
                .and_then(|s| s.try_clone().ok());
            if let Some(mut c) = conn { let _ = c.write_all(body); }
            true
        }
        crate::frames::FRAME_API_CLOSE => {
            // Engine→box close: drop the box-conn so its writer thread (if
            // any) sees EOF. Dropping closes the fd.
            mux.streams.lock().unwrap().remove(&stream_id);
            true
        }
        crate::frames::FRAME_API_OPEN => true, // engine never sends OPEN
        _ => false,
    }
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
            let n = unsafe { libc::read(rfd, tmp.as_mut_ptr().cast(), tmp.len()) };
            if n <= 0 { break; }
            buf.extend_from_slice(&tmp[..n as usize]);
            let (frames, used) = crate::frames::decode(&buf);
            buf.drain(..used);
            for (ft, payload) in frames {
                if runner_api_dispatch(ft, &payload) { continue; }
                if ft == crate::frames::FRAME_ECHO && !payload.is_empty() {
                    let realfd = if payload[0] == 1 { 2 } else { 1 };
                    unsafe { libc::write(realfd, payload[1..].as_ptr().cast(),
                                         payload.len() - 1); }
                } else if ft == crate::frames::FRAME_ECHO_DONE {
                    done2.store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
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
            let n = unsafe { libc::read(rfd, tmp.as_mut_ptr().cast(), tmp.len()) };
            if n <= 0 { break; }
            buf.extend_from_slice(&tmp[..n as usize]);
            let (frames, used) = crate::frames::decode(&buf);
            buf.drain(..used);
            for (ft, _payload) in frames {
                if ft == crate::frames::FRAME_ECHO_DONE { return; }
            }
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
                unsafe { libc::write(1, s.as_ptr().cast(), s.len()); }
                // Recorded copy: a real write through the FUSE sink (captured).
                let _ = (&sink).write_all(s);
            }
        }
        // real stdin → master (keystrokes). On EOF/HUP stop polling stdin but
        // keep relaying master output until the child exits.
        if stdin_open && fds[1].revents & libc::POLLIN != 0 {
            let mut b = [0u8; 65536];
            let n = unsafe { libc::read(stdin_fd, b.as_mut_ptr().cast(), b.len()) };
            if n > 0 { unsafe { libc::write(master, b.as_ptr().cast(), n as usize); } }
            else { stdin_open = false; } // EOF
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
