#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::fs::{File, OpenOptions};
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

fn run_editor_case(
    label: &str,
    initial_text: &[u8],
    before_tab: &[u8],
    selection_downs: u8,
    expected: &[u8],
) {
    let path = std::env::temp_dir().join(format!(
        "sarun_editor_builtin_pty_{}_{}.sh",
        std::process::id(),
        label
    ));
    let _ = std::fs::remove_file(&path);

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty initializes both descriptors or returns an error. The
    // descriptors are immediately wrapped in owned Files below.
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
    // SAFETY: openpty returned two fresh owned descriptors.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    // SAFETY: same as above.
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK,) },
        0
    );

    let stdin = slave.try_clone().expect("clone slave for stdin");
    let stdout = slave.try_clone().expect("clone slave for stdout");
    let stderr = slave.try_clone().expect("clone slave for stderr");
    let mut command = Command::new(env!("CARGO_BIN_EXE_sarun"));
    command
        .arg("brush")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // SAFETY: this closure uses only async-signal-safe libc calls between fork
    // and exec. Stdio has already been installed on descriptors 0/1/2.
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
    let mut command_sent = false;
    let mut editor_entered = None;
    let mut phase = 0u8;
    let mut downs = 0u8;
    let mut last_down = started;
    let mut exit_sent = false;

    while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        let mut poll_fd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd points to one valid pollfd for the duration of call.
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
        if !command_sent && now.duration_since(started) > Duration::from_secs(1) {
            send(&mut master, format!("edit {}\r", path.display()).as_bytes());
            command_sent = true;
        }
        if command_sent && editor_entered.is_none() && contains(&output, b"\x1b[?1049h") {
            send(&mut master, b"i");
            send(&mut master, initial_text);
            editor_entered = Some(now);
            phase = 1;
        }
        if let Some(entered) = editor_entered {
            let elapsed = now.duration_since(entered);
            if phase == 1 && elapsed > Duration::from_millis(500) {
                if !before_tab.is_empty() {
                    send(&mut master, b"\x1b");
                }
                phase = 2;
            } else if phase == 2 && elapsed > Duration::from_millis(800) {
                if !before_tab.is_empty() {
                    send(&mut master, before_tab);
                }
                phase = 3;
            } else if phase == 3 && elapsed > Duration::from_millis(1000) {
                send(&mut master, b"\t");
                phase = 4;
            } else if phase == 4 && contains(&output, b"relation completions") {
                if downs < selection_downs
                    && now.duration_since(last_down) > Duration::from_millis(160)
                {
                    send(&mut master, b"\x1b[B");
                    downs += 1;
                    last_down = now;
                } else if downs == selection_downs
                    && now.duration_since(last_down) > Duration::from_millis(250)
                {
                    send(&mut master, b"\r");
                    phase = 5;
                }
            } else if phase == 5 && elapsed > Duration::from_millis(2200) {
                send(&mut master, b"\x1b");
                phase = 6;
            } else if phase == 6 && elapsed > Duration::from_millis(2500) {
                send(&mut master, b"\x13");
                phase = 7;
            } else if phase == 7 && elapsed > Duration::from_millis(2800) {
                send(&mut master, b"\x1b");
                phase = 8;
            }
        }
        if phase == 8 && !exit_sent && contains(&output, b"\x1b[?1049l") {
            send(&mut master, b"exit\r");
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
    let content = OpenOptions::new()
        .read(true)
        .open(&path)
        .and_then(|mut file| {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        });
    let _ = std::fs::remove_file(&path);

    assert!(
        status.success(),
        "Brush exited {status}; captured {} bytes",
        output.len()
    );
    assert!(exit_sent, "editor never restored the Brush terminal");
    assert_eq!(content.expect("editor must save the file"), expected);
    for border in ["╔", "╗", "╚", "╝", "║", "═"] {
        assert!(
            !contains(&output, border.as_bytes()),
            "standalone editor rendered UI-pane frame glyph {border}"
        );
    }
}

#[test]
fn standalone_brush_edit_builtin_propagates_argument_value_and_restores_terminal() {
    run_editor_case(
        "argument_value",
        b"A=\"\"; find . -type $A",
        b"0llli",
        5,
        b"A=\"f\"; find . -type $A",
    );
}

#[test]
fn standalone_brush_edit_builtin_completes_visible_local_after_dollar() {
    run_editor_case(
        "local_name",
        b"#!/bin/bash\rA=\"\"\rfind . -type $",
        b"",
        0,
        b"#!/bin/bash\nA=\"\"\nfind . -type $A",
    );
}
