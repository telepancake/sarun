#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn send(master: &mut File, bytes: &[u8]) {
    master.write_all(bytes).expect("write PTY input");
    master.flush().expect("flush PTY input");
}

#[test]
fn standalone_brush_completes_builtin_argument_through_relation() {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty initializes two fresh descriptors or reports failure.
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: openpty returned two owned descriptors.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    // SAFETY: same as above.
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_sarun"));
    command
        .arg("brush")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: only async-signal-safe session/controlling-terminal calls occur
    // between fork and exec; stdio is already installed on the slave PTY.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn static sarun brush");
    drop(slave);

    let started = Instant::now();
    let deadline = started + Duration::from_secs(15);
    let mut output = Vec::new();
    let mut dsr_replies = 0usize;
    let mut completion_sent = false;
    let mut interrupted_at = None;
    let mut exit_sent = false;

    while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        let mut poll_fd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd remains valid for this call.
        let _ = unsafe { libc::poll(&mut poll_fd, 1, 40) };
        let mut buffer = [0u8; 65536];
        loop {
            match master.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("read PTY: {error}"),
            }
        }

        let requests = occurrences(&output, b"\x1b[6n");
        while dsr_replies < requests {
            send(&mut master, b"\x1b[1;1R");
            dsr_replies += 1;
        }

        let now = Instant::now();
        if !completion_sent && now.duration_since(started) > Duration::from_secs(1) {
            send(&mut master, b"bind -m \t");
            completion_sent = true;
        }
        if completion_sent
            && interrupted_at.is_none()
            && contains(&output, b"emacs-standard")
            && contains(&output, b"vi-insert")
        {
            send(&mut master, b"\x03");
            interrupted_at = Some(now);
        }
        if interrupted_at.is_some_and(|interrupted| {
            now.duration_since(interrupted) > Duration::from_millis(700)
        }) && !exit_sent
        {
            send(&mut master, b"exit 0\r");
            exit_sent = true;
        }
    }

    let status = match child.try_wait().unwrap() {
        Some(status) => status,
        None => {
            child.kill().unwrap();
            child.wait().unwrap()
        }
    };
    assert!(
        exit_sent,
        "relation completion menu did not show canonical bind values; captured {} bytes",
        output.len()
    );
    assert!(status.success(), "Brush exited {status}");
}
