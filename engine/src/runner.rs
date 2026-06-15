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

fn provenance(cmd: &[String]) -> Value {
    let exe = std::fs::read_link("/proc/self/exe")
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    json!({
        "tgid": std::process::id(),
        "ppid": unsafe { libc::getppid() },
        "exe": exe, "cwd": cwd, "argv": cmd,
    })
}

fn clear_cloexec(fd: i32) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        if f >= 0 { libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC); }
    }
}

pub fn run(name: Option<String>, cmd: Vec<String>) -> i32 {
    if cmd.is_empty() {
        eprintln!("sarun-engine run: no command after --");
        return 2;
    }
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
                         "cmd": cmd, "prov": provenance(&cmd)});
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
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into());
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
    let status = Command::new("bwrap")
        .args(["--bind", &root_src, "/",
               "--proc", "/proc", "--dev", "/dev",
               "--ro-bind-try", "/sys", "/sys",
               "--tmpfs", "/tmp",
               "--ro-bind", &sock_src, UI_SOCK_INBOX,
               "--unshare-pid", "--unshare-ipc", "--unshare-uts",
               "--die-with-parent",
               "--chdir", &cwd,
               "--", &self_exe, "inner", "--conn-fd", &fd_s,
               "--capture", "--"])
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

pub fn inner(conn_fd: i32, capture: bool, cmd: Vec<String>) -> i32 {
    if cmd.is_empty() { return 2; }
    // Hold the box-channel fd open (not CLOEXEC) so the engine sees EOF — its
    // teardown signal — only when this process (and CMD) finally exits.
    if conn_fd >= 0 { clear_cloexec(conn_fd); }
    if !capture || conn_fd < 0 {
        let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
        eprintln!("sarun-engine inner: exec {}: {err}", cmd[0]);
        return 127;
    }
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
    let child = unsafe {
        Command::new(&cmd[0]).args(&cmd[1..])
            .stdout(Stdio::from_raw_fd(out.into_raw_fd()))
            .stderr(Stdio::from_raw_fd(err.into_raw_fd()))
            .spawn()
    };
    let mut child = match child {
        Ok(c) => c,
        Err(e) => { eprintln!("sarun-engine inner: spawn {}: {e}", cmd[0]); return 127; }
    };
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
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
