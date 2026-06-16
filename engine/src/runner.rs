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
const UI_SOCK_INBOX: &str = "/tmp/.slopbox/ui.sock";
const KIDS_DIR: &str = ".slopbox-kids";

fn pidfd_open(pid: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }
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
           pty: bool, chdir: Option<String>, cmd: Vec<String>) -> i32 {
    if cmd.is_empty() {
        eprintln!("sarun-engine run: no command after --");
        return 2;
    }
    // -t passthrough suppresses output capture; -d direct has no overlay so no
    // sinks either (capture = not -t and not -d, mirroring the Python runner).
    // -p PTY mode always wants its output recorded, so it forces capture on
    // even under -t (but never under -d: there is no overlay to capture into).
    let want_capture = (!passthrough && !direct) || (pty && !direct);
    // IN-BOX vs HOST: a nested runner reaches the engine via the socket
    // bind-mounted at UI_SOCK_INBOX; a top-level runner uses the host socket.
    // Pure path-presence, no env var (mirrors the Python runner).
    let in_box = std::path::Path::new(UI_SOCK_INBOX).exists();
    let sock = if in_box { std::path::PathBuf::from(UI_SOCK_INBOX) }
               else { paths::sock_path() };
    let mut conn = match UnixStream::connect(&sock) {
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
    // -C overrides the box's working directory; else inherit ours.
    let cwd = chdir.unwrap_or_else(|| std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into()));
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
    // DEEPER nested runner can reach the engine. Bound after --tmpfs /tmp so it
    // lands on the tmpfs (bwrap creates the parent dir).
    let sock_src = if in_box { UI_SOCK_INBOX.to_string() }
                   else { paths::sock_path().to_string_lossy().into_owned() };
    let fd_s = fd.to_string();
    // Honor the engine's capture decision (it downgrades for -t/-d): only pass
    // --capture to inner when the ack confirms capture is active.
    let capture_on = want_capture
        && ack.get("capture").and_then(Value::as_bool).unwrap_or(false);
    let mut inner_args: Vec<&str> = vec![
        &self_exe, "inner", "--conn-fd", &fd_s];
    if capture_on { inner_args.push("--capture"); }
    // PTY needs the capture sink files to record into; if the engine declined
    // capture (-d) there is nothing to PTY into, so gate --pty on capture_on.
    if pty && capture_on { inner_args.push("--pty"); }
    inner_args.push("--");
    let status = Command::new("bwrap")
        .args(["--bind", &root_src, "/",
               "--proc", "/proc", "--dev", "/dev",
               "--ro-bind-try", "/sys", "/sys",
               "--tmpfs", "/tmp",
               "--ro-bind", &sock_src, UI_SOCK_INBOX,
               "--unshare-pid", "--unshare-ipc", "--unshare-uts",
               "--die-with-parent",
               "--chdir", &cwd, "--"])
        .args(&inner_args)
        .args(&cmd)
        .status();
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

pub fn inner(conn_fd: i32, capture: bool, pty: bool, cmd: Vec<String>) -> i32 {
    if cmd.is_empty() { return 2; }
    // Hold the box-channel fd open (not CLOEXEC) so the engine sees EOF — its
    // teardown signal — only when this process (and CMD) finally exits.
    if conn_fd >= 0 { clear_cloexec(conn_fd); }
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
    unsafe { libc::signal(libc::SIGWINCH, on_winch as libc::sighandler_t); }

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
