// The box runner, ported to Rust (passthrough mode; the ECHO/capture mux is a
// follow-on). Two subcommands of the sarun-engine binary:
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
    let sock = paths::sock_path();
    let mut conn = match UnixStream::connect(&sock) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("sarun-engine: no engine running (control socket {}).",
                      sock.display());
            return 3;
        }
    };
    // register handshake (plain JSON; the engine peeks for an optional pidfd and
    // tolerates none). The SAME connection becomes the box channel.
    let reg = json!({"type": "register",
                     "session_id": name.unwrap_or_default(),
                     "cmd": cmd, "prov": provenance(&cmd)});
    if conn.write_all(format!("{reg}\n").as_bytes()).is_err() {
        eprintln!("sarun-engine: register write failed");
        return 1;
    }
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
    let sid = ack.get("session_id").and_then(Value::as_str).unwrap_or("?");
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into());
    eprintln!("sarun-engine: box {sid}  (overlay root: {mount})");

    // bwrap CMD onto the box's overlay root, exec'ing our own `inner`. The box
    // channel fd is passed (CLOEXEC cleared) and held open by inner.
    let fd = conn.as_raw_fd();
    clear_cloexec(fd);
    let self_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "sarun-engine".into());
    let status = Command::new("bwrap")
        .args(["--bind", &mount, "/",
               "--proc", "/proc", "--dev", "/dev",
               "--ro-bind-try", "/sys", "/sys",
               "--tmpfs", "/tmp",
               "--unshare-pid", "--unshare-ipc", "--unshare-uts",
               "--die-with-parent",
               "--chdir", &cwd,
               "--", &self_exe, "inner", "--conn-fd", &fd.to_string(),
               "--capture", "--"])
        .args(&cmd)
        .status();
    drop(conn); // our copy; inner (in the box) is the channel's sole holder now
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => { eprintln!("sarun-engine: bwrap failed: {e}"); 1 }
    }
}

pub fn inner(conn_fd: i32, capture: bool, cmd: Vec<String>) -> i32 {
    if cmd.is_empty() { return 2; }
    // Hold the box-channel fd open (not CLOEXEC) so the engine sees EOF — its
    // teardown signal — only when this process (and CMD) finally exits.
    if conn_fd >= 0 { clear_cloexec(conn_fd); }
    if !capture {
        let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
        eprintln!("sarun-engine inner: exec {}: {err}", cmd[0]);
        return 127;
    }
    // Capture: tee the child's stdout/stderr to our real fd 1/2 (live) AND to
    // the box-root sink paths, which the overlay routes to the outputs table.
    use std::io::Read;
    use std::io::Write;
    use std::process::Stdio;
    let mut child = match Command::new(&cmd[0]).args(&cmd[1..])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => { eprintln!("sarun-engine inner: spawn {}: {e}", cmd[0]); return 127; }
    };
    fn tee(mut src: impl Read + Send + 'static, realfd: i32, sink: &str)
           -> std::thread::JoinHandle<()> {
        let mut sf = std::fs::OpenOptions::new().write(true).open(sink).ok();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                match src.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        unsafe { libc::write(realfd, buf.as_ptr().cast(), n); }
                        if let Some(f) = sf.as_mut() { let _ = f.write_all(&buf[..n]); }
                    }
                }
            }
        })
    }
    let so = child.stdout.take().map(|s| tee(s, 1, "/.slopbox-stdout"));
    let se = child.stderr.take().map(|s| tee(s, 2, "/.slopbox-stderr"));
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    if let Some(h) = so { let _ = h.join(); }
    if let Some(h) = se { let _ = h.join(); }
    code
}
