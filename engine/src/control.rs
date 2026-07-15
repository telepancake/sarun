use base64::Engine as _;
// Control socket — newline-JSON request/reply on a unix socket, speaking the
// SAME protocol as the Python ChannelServer: {"type":"ui","verb":...} verb
// calls, {"type":"subscribe"} converting the connection into a one-way event
// feed, explicit errors for unknown types/verbs. The first datagram on a
// connection may carry SCM_RIGHTS fds — the register handshake sends the
// runner's pidfd (and, for a `-n` box, its TAP fd). `register` (below) fully
// runs the box: it builds the overlay + capture state, equips the netns, and
// turns the SAME connection into the box channel.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;

use serde_json::Value;
use serde_json::json;

use crate::discover;

#[derive(Default)]
pub struct Shared {
    pub selected: Option<String>,
    pub subscribers: Vec<UnixStream>,
    pub overlay: Option<crate::overlay::Overlay>,
    pub box_pids: std::collections::HashMap<i64, i32>, // box_id -> runner pidfd
    pub box_runpids: std::collections::HashMap<i64, i32>, // box_id -> runner HOST pid
    /// Server-side materialized views (changes / procs / outputs) keyed by an
    /// opaque u64 the client got back from view.open. The values hold the
    /// per-box source rows + a Vec<usize> idx of survivors after the current
    /// filter, so view.window is a cheap slice.
    pub views: crate::views::Registry,
    pub next_view_id: u64,
    /// Per-box networking (`-n` mode only): the engine's CA + prompt queue. The
    /// per-box smoltcp stack is owned by its poll thread (driven by the box's
    /// TAP fd) and its dispatcher task; the engine keeps no handle to reap.
    pub net: Option<std::sync::Arc<crate::net::Net>>,
    /// Long-lived tokio runtime handle used by the dispatcher tasks (one
    /// per-conn task per box). One runtime is plenty: the network is rarely
    /// the bottleneck, and a single runtime keeps reasoning about lifetimes
    /// simple.
    pub net_rt: Option<tokio::runtime::Handle>,
    /// oaita API proxy. Owns the upstream config + the set of `--api`-enabled
    /// boxes. Held here so per-box-channel ApiMux instances (created lazily
    /// in the box-channel frame loop) can fetch the same registry; the
    /// proxy itself has no listener — see oaita::proxy_mux.
    pub api_proxy: Option<std::sync::Arc<crate::oaita::proxy::Proxy>>,
}

/// Open a pidfd for `pid` (>=0 on success). A live `pid` yields a valid fd; a
/// dead one fails (ESRCH). The caller closes it. Reused as a liveness probe.
pub(crate) fn pidfd_open(pid: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }
}

/// True if `pid` (a HOST pid/tgid) is currently alive — a pidfd probe.
pub(crate) fn pid_alive(pid: i32) -> bool {
    let fd = pidfd_open(pid);
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
        true
    } else {
        false
    }
}
fn pidfd_signal(pidfd: i32, sig: i32) {
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            sig,
            std::ptr::null::<libc::c_void>(),
            0,
        );
    }
}

/// The HOST-namespace pid named by `pidfd`, read from /proc/self/fdinfo/<fd>
/// ("Pid:" line; its FIRST field is the pid in our (init) namespace). 0 on any
/// failure. This is the wrap-immune identity path — the pidfd names one exact
/// process incarnation, so a reused pid can never alias a finished runner.
fn host_pid_from_pidfd(pidfd: i32) -> i32 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/self/fdinfo/{pidfd}")) else {
        return 0;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Pid:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// x86_64 syscall number → name, for the handful that a wedge parks in.
/// The blocking ones are what matter (read/write/recv/…); the rest render
/// as `sysN`. Kept tiny on purpose — this is a diagnosis aid, not strace.
fn syscall_name(nr: i64) -> &'static str {
    match nr {
        0 => "read",
        1 => "write",
        3 => "close",
        7 => "poll",
        8 => "lseek",
        17 => "pread64",
        18 => "pwrite64",
        22 => "pipe",
        23 => "select",
        42 => "connect",
        43 => "accept",
        44 => "sendto",
        45 => "recvfrom",
        46 => "sendmsg",
        47 => "recvmsg",
        61 => "wait4",
        202 => "futex",
        230 => "nanosleep",
        232 => "epoll_wait",
        270 => "pselect6",
        271 => "ppoll",
        281 => "epoll_pwait",
        288 => "accept4",
        _ => "",
    }
}

/// True when this syscall's FIRST argument is a file descriptor — so the
/// wedge report can resolve it to the pipe/socket/file it names.
fn syscall_arg0_is_fd(nr: i64) -> bool {
    matches!(
        nr,
        0 | 1 | 3 | 8 | 17 | 18 | 42 | 43 | 44 | 45 | 46 | 47 | 232 | 288
    )
}

/// Symbolized backtraces of every thread in `pids`, keyed by tid — the layer
/// `/proc` can't give. wchan/syscall answer WHERE a thread blocks in the
/// KERNEL, but a thread that reads "running" (spinning in userspace) or an
/// idle worker parked in futex leaves the actual SOURCE line invisible;
/// that is the case a wedge diagnosis most needs. The release binary now
/// keeps line-table debug info (Cargo `[profile.release] debug`), so gdb
/// resolves real `func (file:line)` frames. We shell out once per pid
/// (`thread apply all bt`), bounded by `timeout`, and parse the
/// `Thread N (LWP <tid> …)` blocks. Empty map when gdb/timeout are absent —
/// the /proc layer still stands on its own.
fn thread_backtraces(pids: &[i32], depth: usize) -> std::collections::HashMap<i32, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    // gdb must exist; if not, degrade silently to the /proc-only view.
    if std::process::Command::new("gdb")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        return out;
    }
    // Attach to every pid CONCURRENTLY: gdb attaching to a sud-traced process
    // is slow (it can hit the full timeout), so a serial loop over N pids
    // wedged `stuck` for N×timeout. Spawn them all, then collect — total wall
    // time is bounded by the single slowest attach, not their sum.
    let children: Vec<_> = pids
        .iter()
        .filter_map(|&pid| {
            std::process::Command::new("timeout")
                .args([
                    "5",
                    "gdb",
                    "-p",
                    &pid.to_string(),
                    "-batch",
                    "-nx",
                    "-ex",
                    "set pagination off",
                    "-ex",
                    &format!("thread apply all bt {depth}"),
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()
        })
        .collect();
    for child in children {
        let Ok(res) = child.wait_with_output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&res.stdout);
        let mut cur_tid: Option<i32> = None;
        for line in text.lines() {
            let l = line.trim_start();
            if let Some(rest) = l.strip_prefix("Thread ") {
                // "Thread N (LWP <tid> …)" — the LWP is the kernel tid.
                cur_tid = rest.find("LWP ").and_then(|i| {
                    rest[i + 4..]
                        .split(|c: char| !c.is_ascii_digit())
                        .next()
                        .and_then(|d| d.parse::<i32>().ok())
                });
            } else if l.starts_with('#') {
                if let Some(tid) = cur_tid {
                    // Keep the readable tail: "func (…) at file:line" —
                    // drop the leading "#N  0xADDR in ".
                    let frame = l
                        .split_once(" in ")
                        .map(|(_, r)| r)
                        .unwrap_or(l)
                        .trim()
                        .to_string();
                    out.entry(tid).or_insert_with(Vec::new).push(frame);
                }
            }
        }
    }
    out
}

/// In-process self-unwind backtraces — the layer that works UNDER sud, where
/// external gdb resolves the relocated engine to `?? ()`. We ask each on-CPU
/// (R-state) thread to dump its own symbolized stack (via a preinstalled
/// realtime-signal handler that writes `std::backtrace` to the host sink file
/// the runner opened) and parse the blocks back. `rp` is the box's runner host
/// pid (the sink-file key); `box_pids` its process set. Empty map when the box
/// predates this mechanism (no sink) or nothing on-CPU answered.
fn selfbt_backtraces(rp: i32, box_pids: &[i32]) -> std::collections::HashMap<i32, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    let path = crate::selfbt::sink_path(rp);
    // Truncate the sink so we read only THIS invocation's dumps.
    let Ok(f) = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
    else {
        return out;
    };
    drop(f);
    // Aim only at R-state threads: those are the spins gdb can't localize, and
    // signalling a blocked thread risks EINTR-perturbing the box.
    let sig = crate::selfbt::dump_signal();
    let mut aimed = 0;
    for &pid in box_pids {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            continue;
        };
        for te in tasks.flatten() {
            let Some(tid) = te.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let st = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat"))
                .ok()
                .and_then(|s| s.rfind(')').and_then(|i| s[i + 1..].trim().chars().next()));
            if st != Some('R') {
                continue;
            }
            unsafe {
                libc::syscall(
                    libc::SYS_tgkill,
                    pid as libc::c_long,
                    tid as libc::c_long,
                    sig as libc::c_long,
                );
            }
            aimed += 1;
        }
    }
    if aimed == 0 {
        return out;
    }
    // Give the handlers time to walk + write.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    // Parse blocks: "=== tid N bias 0xB ===" then hex return addresses. Collect
    // per tid the (bias, [addr…]); symbolize below with one addr2line pass.
    let mut raw: std::collections::HashMap<i32, (u64, Vec<u64>)> = Default::default();
    let mut cur: Option<i32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("=== tid ") {
            // "N bias 0xB ==="
            let mut it = rest.split_whitespace();
            let tid = it.next().and_then(|s| s.parse::<i32>().ok());
            let bias = it
                .nth(1) // skip "bias"
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            if let Some(tid) = tid {
                cur = Some(tid);
                raw.entry(tid).or_insert((bias, vec![]));
            }
            continue;
        }
        if let (Some(tid), Some(addr)) = (
            cur,
            line.trim()
                .strip_prefix("0x")
                .and_then(|h| u64::from_str_radix(h, 16).ok()),
        ) {
            raw.entry(tid).or_insert((0, vec![])).1.push(addr);
        }
    }
    let exe = std::env::current_exe().ok();
    for (tid, (_bias, addrs)) in raw {
        let frames = symbolize_addrs(exe.as_deref(), &addrs);
        if !frames.is_empty() {
            out.insert(tid, frames);
        }
    }
    out
}

/// Symbolize a captured chain of absolute return addresses against the engine's
/// own binary. sarun is a non-PIE static ET_EXEC, so a runtime address IS its
/// ELF virtual address — no relocation to undo. Prefers llvm-addr2line
/// (binutils addr2line can infinite-loop in its DWARF range walker on some of
/// these frames — a known upstream bug that wedged `stuck` outright); either
/// way a `timeout` + null stdin caps the wall time. Unresolved `??` frames are
/// dropped.
fn symbolize_addrs(exe: Option<&std::path::Path>, addrs: &[u64]) -> Vec<String> {
    let (Some(exe), false) = (exe, addrs.is_empty()) else {
        return vec![];
    };
    let vaddrs: Vec<String> = addrs.iter().map(|a| format!("0x{a:x}")).collect();
    let a2l = if std::process::Command::new("llvm-addr2line")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        "llvm-addr2line"
    } else {
        "addr2line"
    };
    let out = std::process::Command::new("timeout")
        .arg("15")
        .arg(a2l)
        .arg("-f")
        .arg("-C")
        .arg("-e")
        .arg(exe)
        .args(&vaddrs)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(o) = out else { return vec![] };
    let text = String::from_utf8_lossy(&o.stdout);
    // addr2line -f emits pairs of lines: <function>\n<file:line>.
    let lines: Vec<&str> = text.lines().collect();
    let mut frames = vec![];
    let mut i = 0;
    while i + 1 < lines.len() {
        let func = lines[i].trim();
        let loc = lines[i + 1].trim();
        i += 2;
        if func == "??" {
            continue;
        }
        if func.starts_with("sarun::selfbt::") {
            continue;
        }
        // Strip the ` (.llvm.<hash>)` cold-split suffix — pure noise.
        let func = func.split(" (.llvm.").next().unwrap_or(func).trim();
        let func = collapse_generics(func);
        let loc = loc.rsplit('/').next().unwrap_or(loc);
        frames.push(if loc == "??:0" || loc == "??:?" {
            func
        } else {
            format!("{func} ({loc})")
        });
    }
    frames
}

/// Collapse the monomorphized generic parameters in a demangled Rust symbol so
/// a frame fits on one line: each balanced `<…>` group (and the `$LT$…$GT$`
/// form llvm-addr2line leaves when it can't fully demangle) becomes a bare
/// `<…>`. `map<std::sys::fs::unix::FileAttr, …, fn(…) -> …>` — six wrapped
/// lines in the panel — shrinks to `map<…>`. Without this the stuck panel is
/// unreadable.
fn collapse_generics(s: &str) -> String {
    // Normalize the un-demangled bracket spelling first so both forms collapse.
    let s = s.replace("$LT$", "<").replace("$GT$", ">");
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                if depth == 0 {
                    out.push_str("<…>");
                }
                depth += 1;
            }
            '>' => {
                depth = depth.saturating_sub(1);
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// A box process's brush/engine text range [lo, hi), from /proc/<pid>/maps: the
/// largest anonymous `r-xp` mapping (the sud loader places the ET_EXEC text in
/// anonymous memory — that's exactly why external gdb can't symbolize it). A
/// box RIP inside this range is brush/engine code (walkable + symbolizable
/// against our binary); outside it is the sud dispatcher on its alt-stack.
/// (0, 0) if it can't be determined.
fn box_text_range(pid: i32) -> (u64, u64) {
    let Ok(maps) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else {
        return (0, 0);
    };
    let mut best = (0u64, 0u64);
    for line in maps.lines() {
        let mut it = line.split_whitespace();
        let range = it.next().unwrap_or("");
        let perms = it.next().unwrap_or("");
        let _off = it.next();
        let _dev = it.next();
        let inode = it.next().unwrap_or("");
        let path = it.next();
        // Anonymous executable segment: r-xp, inode 0, no pathname.
        if !perms.starts_with("r-x") || inode != "0" || path.is_some() {
            continue;
        }
        let Some((s, e)) = range.split_once('-') else {
            continue;
        };
        let (Ok(s), Ok(e)) = (u64::from_str_radix(s, 16), u64::from_str_radix(e, 16)) else {
            continue;
        };
        if e - s > best.1 - best.0 {
            best = (s, e);
        }
    }
    best
}

/// Ptrace-based backtraces — the ROBUST path that works regardless of thread
/// state and does NOT depend on an in-box signal handler (sud masks all
/// signals inside its SIGSYS dispatcher, so a syscall-heavy spin — the common
/// real wedge — never lets the in-box self-unwind run). sud uses SIGSYS user
/// dispatch, not ptrace, so the engine is free to PTRACE_SEIZE each box thread,
/// read its registers and stack memory from outside, walk the frame pointers,
/// and symbolize offline. Reads absolute addresses; sarun is ET_EXEC so those
/// are ELF vaddrs. Best-effort: a thread we can't seize is simply skipped.
fn ptrace_backtraces(box_pids: &[i32]) -> std::collections::HashMap<i32, Vec<String>> {
    use std::os::unix::fs::FileExt;
    const PTRACE_SEIZE: i32 = 0x4206;
    const PTRACE_INTERRUPT: i32 = 0x4207;
    const PTRACE_GETREGSET: i32 = 0x4204;
    const PTRACE_CONT: i32 = 7;
    const PTRACE_DETACH: i32 = 17;
    const NT_PRSTATUS: i32 = 1;
    const WALL: i32 = 0x4000_0000;
    let exe = std::env::current_exe().ok();
    let mut out = std::collections::HashMap::new();
    // The engine's own executable text range. sarun is a fixed-address
    // ET_EXEC, so the box runs the SAME vaddrs — a box RIP inside this range
    // is brush/engine code (walkable), a RIP outside it is the sud dispatcher
    // on its alt-stack (SA_ONSTACK), whose frames don't chain back to the
    // brush stack. We retry the stop until we catch the thread IN this range.
    for &pid in box_pids {
        let (text_lo, text_hi) = box_text_range(pid);
        // One /proc/<pid>/mem handle per process (threads share the address
        // space); pread it at the frame-pointer addresses while stopped.
        let mem = std::fs::File::open(format!("/proc/{pid}/mem")).ok();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            continue;
        };
        for te in tasks.flatten() {
            let Some(tid) = te.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let addrs = unsafe {
                if libc::ptrace(PTRACE_SEIZE as _, tid, 0, 0) < 0 {
                    continue;
                }
                let mut chain = vec![];
                // Retry: interrupt, sample RIP; if it's in the sud dispatcher
                // (outside the engine text) let it run and try again, up to a
                // bound. Catches a syscall-heavy spin in its brief brush-code
                // window between traps.
                for attempt in 0..24 {
                    libc::ptrace(PTRACE_INTERRUPT as _, tid, 0, 0);
                    let mut status = 0i32;
                    if libc::waitpid(tid, &mut status, WALL) < 0 {
                        break;
                    }
                    let mut regs: libc::user_regs_struct = std::mem::zeroed();
                    let mut iov = libc::iovec {
                        iov_base: (&mut regs as *mut libc::user_regs_struct).cast(),
                        iov_len: std::mem::size_of::<libc::user_regs_struct>(),
                    };
                    if libc::ptrace(
                        PTRACE_GETREGSET as _,
                        tid,
                        NT_PRSTATUS as *mut libc::c_void,
                        (&mut iov as *mut libc::iovec).cast::<libc::c_void>(),
                    ) < 0
                    {
                        break;
                    }
                    #[cfg(target_arch = "x86_64")]
                    let (rip, mut fp) = (regs.rip, regs.rbp);
                    #[cfg(target_arch = "aarch64")]
                    let (rip, mut fp) = (regs.pc, regs.regs[29]);
                    let in_text = text_hi > text_lo && rip >= text_lo && rip < text_hi;
                    // Accept when in brush code, or on the final attempt take
                    // whatever we have rather than nothing.
                    if in_text || attempt == 23 {
                        chain.push(rip);
                        if let Some(mem) = &mem {
                            let read_u64 = |a: u64| -> Option<u64> {
                                let mut b = [0u8; 8];
                                mem.read_exact_at(&mut b, a)
                                    .ok()
                                    .map(|_| u64::from_le_bytes(b))
                            };
                            let mut prev = fp;
                            for _ in 0..64 {
                                if fp == 0 || (fp & 7) != 0 || fp < prev {
                                    break;
                                }
                                let (Some(saved), Some(ret)) = (read_u64(fp), read_u64(fp + 8))
                                else {
                                    break;
                                };
                                if ret == 0 {
                                    break;
                                }
                                chain.push(ret);
                                if saved <= fp {
                                    break;
                                }
                                prev = fp;
                                fp = saved;
                            }
                        }
                        break;
                    }
                    // Not in brush code: resume and resample.
                    libc::ptrace(PTRACE_CONT as _, tid, 0, 0);
                }
                libc::ptrace(PTRACE_DETACH as _, tid, 0, 0);
                chain
            };
            if addrs.is_empty() {
                continue;
            }
            let frames = symbolize_addrs(exe.as_deref(), &addrs);
            if !frames.is_empty() {
                out.insert(tid, frames);
            }
        }
    }
    out
}

/// PPid of `pid` from /proc/<pid>/status (host namespace); 0 if unreadable.
fn ppid_of(pid: i32) -> i32 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Given the connecting runner's HOST pid, walk the /proc PPid chain upward and
/// return the box_id of the first LIVE box whose runner host pid is an ancestor
/// — the enclosing box of a nested launch. Kernel-derived and pid-trusted: the
/// box never supplies its own parent. None if no enclosing box is found.
fn derive_parent_box(state: &State, host_pid: i32) -> Option<i64> {
    let map: std::collections::HashMap<i32, i64> = {
        let s = lock(state);
        s.box_runpids.iter().map(|(b, p)| (*p, *b)).collect()
    };
    if map.is_empty() || host_pid <= 1 {
        return None;
    }
    let mut pid = host_pid;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        if pid <= 1 || !seen.insert(pid) {
            break;
        }
        if let Some(&b) = map.get(&pid) {
            return Some(b);
        }
        let pp = ppid_of(pid);
        if pp <= 1 {
            break;
        }
        pid = pp;
    }
    None
}

pub type State = Arc<Mutex<Shared>>;

/// Lock the shared state, RECOVERING from a poisoned mutex (audit M4). One
/// panic while a `state.lock()` guard was held used to poison the mutex
/// permanently: every later `lock(state)` would then panic, taking
/// down the whole control plane (a single bad connection handler → engine-wide
/// outage). The `Shared` struct holds plain collections + handles, so a panic
/// mid-mutation leaves it structurally intact (at worst a half-finished insert),
/// not memory-unsafe — so recovering the guard via `into_inner()` and carrying
/// on is the right call here. Connection handlers also run under `catch_unwind`
/// (see `handle`), so a panicking handler is contained instead of unwinding out
/// of its thread; this lock recovery is the second half of the same defense.
fn lock(state: &State) -> std::sync::MutexGuard<'_, Shared> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// The old api-proxy attribution shim (peer-pid → box-id lookup) was removed
// when the proxy moved onto the box-channel — attribution is now intrinsic
// to the channel the FRAME_API_* stream rides on.

/// Broadcast that box `box_id` has new api_log rows so the UI's API Logs pane
/// refreshes. Best-effort; the broadcaster swallows send errors.
pub fn broadcast_api_log(box_id: i64) {
    // We need the State to actually broadcast — go through a global handle
    // set up by serve().
    if let Some(state) = STATE_HANDLE.read().clone() {
        broadcast(
            &state,
            &json!({
                "type": "api_log_added",
                "sid": box_id.to_string(),
            }),
        );
    }
}

/// Broadcast that box `box_id` has new webcap rows so the UI's Captures pane
/// refreshes. Best-effort, mirroring `broadcast_api_log` (DESIGN-web.md W1).
pub fn broadcast_webcap(box_id: i64) {
    if let Some(state) = STATE_HANDLE.read().clone() {
        broadcast(
            &state,
            &json!({
                "type": "webcap_added",
                "sid": box_id.to_string(),
            }),
        );
    }
}

static STATE_HANDLE: parking_lot::RwLock<Option<State>> = parking_lot::RwLock::new(None);
pub fn install_state_handle(s: State) {
    *STATE_HANDLE.write() = Some(s.clone());
}

/// Record one D9 brush-shell provenance frame for box `id`: parse the JSON
/// payload, write it into the live box's sqlar `brushprov` table, and broadcast
/// a `brush_prov` event. Best-effort — a malformed payload is dropped quietly.
fn record_brush_prov(state: &State, ov: &Option<crate::overlay::Overlay>, id: i64, payload: &[u8]) {
    let Ok(rec) = serde_json::from_slice::<Value>(payload) else {
        return;
    };
    let cmd = rec
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The 0-based pipeline ordinal + the wall-clock spawn instant brush captured
    // right before running this pipeline's complete-command. The spawn_ts defines
    // the attribution window; the actual process↔pipeline stamping is done in one
    // race-free pass at box teardown (finalize_brush_links), since a process row
    // (e.g. a redirect target's writer) can be materialized long after its pipeline.
    let seq = rec.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let spawn_ts = rec.get("spawn_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let uid = rec.get("uid").and_then(Value::as_i64).unwrap_or(0);
    let parent_uid = rec.get("parent_uid").and_then(Value::as_i64).unwrap_or(0);
    let record_json = rec.to_string();
    let mut prov_id = 0i64;
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            prov_id = b.add_brushprov(&cmd, &record_json, seq, spawn_ts, uid, parent_uid);
            b.set_cur_brush_pipeline(prov_id);
            // Remember this pipeline's output-redirect targets for the exact
            // file→process linkage made at teardown.
            let targets: Vec<String> = rec
                .get("out_targets")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            b.on_brush_prov(prov_id, targets);
        }
    }
    broadcast(
        state,
        &json!({"type": "brush_prov",
                            "session_id": id.to_string(),
                            "brushprov_id": prov_id, "seq": seq,
                            "cmd": cmd, "record": rec}),
    );
}

/// D9 nested-shell provenance verb. The brush-sh shim (a `sh -c RECIPE` the box
/// spawned, exec'd as the engine binary) sends one `brush_prov_nested` message
/// carrying ITS OWN pidfd as SCM_RIGHTS. We resolve the enclosing box from the
/// shim's /proc ancestry — the EXACT path `register` uses for a nested box — and
/// record each record as a NESTED brushprov row, broadcasting a `brush_prov`
/// event per row. Best-effort: an unresolvable box or malformed message is
/// dropped quietly (the recipe runs regardless; provenance is optional). This is
/// a one-shot control reply — it does NOT create a box channel.
fn brush_prov_nested(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    // Resolve the shim's HOST pid from its pidfd (the wrap-immune identity path),
    // then derive the enclosing box from its /proc ancestry.
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let ov = lock(state).overlay.clone();
    let records = msg.get("records").and_then(Value::as_array);
    let Some(records) = records else {
        return json!({"ok": false, "error": "no records"});
    };
    let mut n = 0i64;
    for rec in records {
        let cmd = rec
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let seq = rec.get("seq").and_then(Value::as_i64).unwrap_or(0);
        let spawn_ts = rec.get("spawn_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let uid = rec.get("uid").and_then(Value::as_i64).unwrap_or(0);
        let parent_uid = rec.get("parent_uid").and_then(Value::as_i64).unwrap_or(0);
        let record_json = rec.to_string();
        let mut prov_id = 0i64;
        if let Some(ov) = ov.as_ref() {
            if let Some(b) = ov.live_box(id) {
                prov_id =
                    b.add_brushprov_nested(&cmd, &record_json, seq, spawn_ts, uid, parent_uid);
                let targets: Vec<String> = rec
                    .get("out_targets")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                b.on_brush_prov(prov_id, targets);
                // Set cur_brush_pipeline to the FIRST pipeline in this
                // complete-command — that is the one that executes first and
                // produces the initial output. Later pipelines in an and-or
                // list (cmd1 && cmd2) run after the first finishes; we cannot
                // track per-pipeline transitions within one complete-command,
                // but attributing to the first is correct for the common
                // single-pipeline case and for the first leg of a chain.
                if n == 0 {
                    b.set_cur_brush_pipeline(prov_id);
                }
            }
        }
        broadcast(
            state,
            &json!({"type": "brush_prov",
                                "session_id": id.to_string(),
                                "brushprov_id": prov_id, "seq": seq,
                                "nested": true, "cmd": cmd, "record": rec}),
        );
        n += 1;
    }
    json!({"ok": true, "recorded": n})
}

/// D9 pipeline completion. After a pipeline's complete-command finishes, the box
/// sends one message carrying the completed pipelines' `uids`, the `code`, and
/// the `done_ts` (wall clock), plus ITS OWN pidfd (resolve-the-box like
/// brush_prov_nested). We stamp done_ts + exit_code on those brushprov rows so a
/// reader can show per-pipeline wall time and tell running (done_ts==0) from
/// finished. Best-effort; one-shot reply.
fn brush_prov_done(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let uids: Vec<i64> = msg
        .get("uids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let code = msg.get("code").and_then(Value::as_i64).unwrap_or(0);
    let done_ts = msg.get("done_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            b.mark_brushprov_done(&uids, code, done_ts);
        }
    }
    json!({"ok": true})
}

/// Recipe fixup: after a $(shell) recipe finishes, the box sends the pipeline
/// uids and the recipe's start timestamp. We retroactively fix the
/// brush_pipeline_id on stderr output rows that the FUSE handler captured
/// during the recipe with a wrong (racy) attribution. The stderr flowed
/// through fd 2 normally for live backread — this just fixes the DB linkage.
fn recipe_fixup(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let uids: Vec<i64> = msg
        .get("uids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let start_ts = msg.get("start_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            let pipeline_id = uids
                .iter()
                .map(|u| b.brushprov_id_for_uid(*u))
                .find(|id| *id > 0)
                .unwrap_or(0);
            if pipeline_id > 0 {
                b.fixup_output_attribution(start_ts, pipeline_id);
            }
        }
    }
    json!({"ok": true})
}

/// Phase 1 embedded-ninja `build_edges` verb. The shadowed `ninja` (vendored n2,
/// in-process) sends ONE message carrying the FULL parsed build graph — every
/// edge {outs, ins, cmd}, INCLUDING up-to-date targets that never execute — plus
/// ITS OWN pidfd as SCM_RIGHTS. We resolve the enclosing box from /proc ancestry
/// (the same path register/brush_prov_nested use) and store each edge in the
/// box's `build_edges` table. One-shot control reply; not a box channel.
fn build_edges(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let ov = lock(state).overlay.clone();
    let Some(edges) = msg.get("edges").and_then(Value::as_array) else {
        return json!({"ok": false, "error": "no edges"});
    };
    let mut n = 0i64;
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            for e in edges {
                let outs = e.get("outs").cloned().unwrap_or_else(|| json!([]));
                let ins = e.get("ins").cloned().unwrap_or_else(|| json!([]));
                let cmd = e.get("cmd").and_then(Value::as_str);
                b.add_build_edge(&outs.to_string(), &ins.to_string(), cmd);
                n += 1;
            }
        }
    }
    broadcast(
        state,
        &json!({"type": "build_edges",
                            "session_id": id.to_string(), "edges": n}),
    );
    json!({"ok": true, "recorded": n})
}

/// A single build edge's run-state transition (started / finished), sent by the
/// in-process make/ninja executor as it enters and leaves each recipe (carrying
/// its OWN pidfd, resolved to the enclosing box exactly like `build_edges`). We
/// stamp `started_ts` / `ended_ts`+`exit_code` on the matching `build_edges`
/// row, so the targets pane can show only the targets currently building and
/// each target's wall time. One-shot control reply.
/// One recorded variable assignment (a `make_vars` frame row → makevar table).
pub struct MakeVarRow {
    pub name: String,
    pub loc: String,
    pub value: String,
    pub make_dir: String,
    /// Primary output of the recipe edge the assignment ran under, if any.
    pub edge_out: Option<String>,
    /// brushprov uid of the enclosing pipeline (0 = none).
    pub uid: i64,
    /// The UNEXPANDED assignment text (capped at capture).
    pub rhs: String,
    /// Space-joined variable names the rhs dereferences.
    pub refs: String,
    /// Compact assignment-kind tag (op + notable origin, or sh / sh x).
    pub flags: String,
}

/// `make_vars` frame: a batch of makefile variable assignments from a box's
/// embedded make(s), recorded into the box's makevar table.
fn make_vars(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let rows: Vec<MakeVarRow> = msg
        .get("rows")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(MakeVarRow {
                        name: r.get("name")?.as_str()?.to_string(),
                        loc: r
                            .get("loc")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        value: r
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        make_dir: r
                            .get("make")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        edge_out: r.get("edge").and_then(Value::as_str).map(str::to_string),
                        uid: r.get("uid").and_then(Value::as_i64).unwrap_or(0),
                        rhs: r
                            .get("rhs")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        refs: r
                            .get("refs")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        flags: r
                            .get("flags")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            b.add_makevars(&rows);
        }
    }
    json!({"ok": true})
}

/// `box_activity` frame: the box's live in-flight builtin work (kati
/// recipes / $(shell) / parse phases with ages) — stored ephemerally on the
/// BoxState for the UI's "what is it doing" feed.
fn box_activity(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let items: Vec<(String, u64)> = msg
        .get("items")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|it| {
                    let arr = it.as_array()?;
                    Some((
                        arr.first()?.as_str()?.to_string(),
                        arr.get(1).and_then(Value::as_u64).unwrap_or(0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            *b.activity.lock().unwrap() = items;
        }
    }
    json!({"ok": true})
}

fn build_edge_state(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let phase = msg.get("state").and_then(Value::as_str).unwrap_or("");
    let out = msg.get("out").and_then(Value::as_str);
    let cmd = msg.get("cmd").and_then(Value::as_str);
    let ts = msg.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let code = msg.get("code").and_then(Value::as_i64).unwrap_or(0);
    let excerpt = msg.get("excerpt").and_then(Value::as_str);
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            match phase {
                "start" => b.mark_build_edge_started(out, cmd, ts),
                "done" => b.mark_build_edge_done(out, cmd, code, ts, excerpt),
                _ => {}
            }
        }
    }
    broadcast(
        state,
        &json!({"type": "build_edges",
                            "session_id": id.to_string(), "edge_state": phase}),
    );
    json!({"ok": true})
}

pub fn broadcast(state: &State, ev: &Value) {
    let data = format!("{ev}\n");
    let mut s = lock(state);
    s.subscribers.retain(|conn| {
        let mut c = conn;
        c.write_all(data.as_bytes()).is_ok()
    });
}

/// Peek the SCM_RIGHTS fds sent with the connection's first bytes (the register
/// handshake carries the runner's pidfd, and for a `tap` box ALSO the runner's
/// TAP fd as a second fd). Return (pidfd, tap_fd): keep both, close any extras.
/// MSG_PEEK leaves the data bytes queued for the BufReader, and the real
/// (no-ancillary) read later discards the duplicate fd delivery.
fn recv_first_fd(conn: &UnixStream) -> (Option<i32>, Option<i32>, Option<i32>) {
    // Wait (bounded) for the first bytes to arrive before peeking: the runner's
    // sendmsg may still be in flight when we accept, and a non-blocking peek
    // that races ahead of it would miss the pidfd — dropping a nested box's
    // only correct host-pid source. poll for readability, then a blocking peek.
    let fd = conn.as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let pr = unsafe { libc::poll(&mut pfd, 1, 30_000) };
    if pr <= 0 {
        return (None, None, None);
    }
    let mut fdbuf = [0i32; 8];
    let mut io = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: io.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut cmsg = [0u8; 128];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    // msg_controllen is socklen_t (u32) on glibc but size_t (usize) on musl;
    // `as _` picks the field's type on each target.
    msg.msg_controllen = cmsg.len() as _;
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_PEEK) };
    if n < 0 {
        return (None, None, None);
    }
    // Keep up to THREE fds in order: [pidfd, then optionally a TAP fd, then
    // optionally a sud trace-pipe fd]. The runner sends them in that fixed
    // order; `register` assigns roles from want_sud + net_mode (a sud+tap
    // box sends all three; a fuse+tap box sends two; a plain box sends one).
    let mut first: Option<i32> = None;
    let mut second: Option<i32> = None;
    let mut third: Option<i32> = None;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let len = (*c).cmsg_len as usize - (libc::CMSG_DATA(c) as usize - c as usize);
                let count = len / std::mem::size_of::<i32>();
                for i in 0..count.min(fdbuf.len()) {
                    std::ptr::copy_nonoverlapping(
                        data.add(i * 4),
                        (&mut fdbuf[i] as *mut i32).cast(),
                        4,
                    );
                    if first.is_none() {
                        first = Some(fdbuf[i]);
                    } else if second.is_none() {
                        second = Some(fdbuf[i]);
                    } else if third.is_none() {
                        third = Some(fdbuf[i]);
                    } else {
                        libc::close(fdbuf[i]);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    (first, second, third)
}

fn dispatch(state: &State, msg: &Value) -> Value {
    let t = msg.get("type").and_then(Value::as_str).unwrap_or("");
    match t {
        "subscribe" => json!({"ok": true, "_subscribe": true}),
        "register" => register(state, msg, None, None, None),
        "select" => {
            let sid = msg.get("sid").and_then(Value::as_str).map(String::from);
            let boxes = discover::discover();
            match sid.as_deref().and_then(|s| resolve(&boxes, s)) {
                Some(id) => {
                    lock(state).selected = Some(id.to_string());
                    json!({"ok": true, "sid": id.to_string()})
                }
                None => json!({"ok": false,
                               "error": format!("no slopbox '{}'",
                                                sid.unwrap_or_default())}),
            }
        }
        "ui" => dispatch_ui(state, msg),
        // --sud sweep (engine/DESIGN-sud.md): the runner calls this after
        // its wrapper child exits. We resolve the box, then hand the live
        // BoxState to sud::sweep, which owns the upper/inramfs/trace ingest
        // and residue cleanup; here we only shape the report into a reply.
        "sud_ingest" => {
            let boxes = discover::discover();
            match msg
                .get("sid")
                .and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s))
            {
                Some(id) => {
                    let live = lock(state).overlay.clone().and_then(|o| o.live_box(id));
                    match live {
                        Some(b) => {
                            let runpid =
                                lock(state).box_runpids.get(&id).copied().unwrap_or(0) as u32;
                            let r = crate::sud::sweep(&b, id, runpid);
                            json!({"ok": true, "ingested": r.ingested,
                                   "errors": r.errors})
                        }
                        None => json!({"ok": false,
                                       "error": "box is not live"}),
                    }
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "patch" => {
            let boxes = discover::discover();
            match msg
                .get("sid")
                .and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s))
            {
                Some(id) => {
                    let data = crate::review::patch_text(id);
                    json!({"ok": true, "patch":
                        base64::engine::general_purpose::STANDARD.encode(&data)})
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        // The box's durable sud TRACE stream, decoded on demand into the
        // generated typed result (engine/DESIGN-sud.md step 2). The current
        // JSON listener projects that value only at its outer boundary. Reads
        // the sqlar blob via get_sudtrace — works for an at-rest box, no live
        // requirement. A box with no trace answers with a clean error.
        "sudtrace" => {
            let boxes = discover::discover();
            match msg
                .get("sid")
                .and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s))
            {
                Some(id) => {
                    let Ok(sid) = u64::try_from(id) else {
                        return json!({"ok": false, "error": "negative box id"});
                    };
                    match dispatch_action(
                        state,
                        crate::generated_wire::ActionRequest::Sudtrace { sid },
                    ) {
                        Ok(success) => legacy_control_reply(success),
                        Err(error) => json!({"ok": false, "error": error}),
                    }
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "apply" | "discard" => {
            let boxes = discover::discover();
            match msg
                .get("sid")
                .and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s))
            {
                Some(id) => {
                    let Ok(sid) = u64::try_from(id) else {
                        return json!({"ok": false, "error": "negative box id"});
                    };
                    let request = if t == "apply" {
                        crate::generated_wire::ActionRequest::Apply { sid }
                    } else {
                        crate::generated_wire::ActionRequest::Discard { sid }
                    };
                    match dispatch_action(state, request) {
                        Ok(success) => legacy_control_reply(success),
                        Err(error) => json!({"ok": false, "error": error}),
                    }
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "rename" => {
            let boxes = discover::discover();
            let newname = msg.get("name").and_then(Value::as_str).unwrap_or("");
            match msg
                .get("sid")
                .and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s))
            {
                Some(id) => {
                    let Ok(sid) = u64::try_from(id) else {
                        return json!({"ok": false, "error": "negative box id"});
                    };
                    let new = match crate::wire::BoundedText::new(newname.to_owned()) {
                        Ok(new) => new,
                        Err(error) => {
                            return json!({
                                "ok": false,
                                "error": format!("name exceeds relation bound: {error:?}"),
                            });
                        }
                    };
                    match dispatch_action(
                        state,
                        crate::generated_wire::ActionRequest::Rename { sid, new },
                    ) {
                        Ok(success) => legacy_control_reply(success),
                        Err(error) => json!({"ok": false, "error": error}),
                    }
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "shutdown" | "quit" => {
            // Stop every LIVE box first: quitting the engine (F10 / `q`)
            // must not leave runs going — a sud box's traced tree is
            // ordinary host processes and kept building after the engine
            // died. Same signal the `kill` verb sends; the runner forwards
            // it to the wrapper's process group and tears down normally
            // (sweep included, while the engine is still here to serve it).
            match dispatch_action(state, crate::generated_wire::ActionRequest::Quit) {
                Ok(success) => legacy_control_reply(success),
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
        other => json!({"ok": false,
                        "error": format!("unknown control type '{other}'")}),
    }
}

/// Execute top-level actions from their generated closed request type. Current
/// JSON branches immediately enter this typed path; the mandatory binary
/// listener cutover removes those outer projections and calls it directly.
fn existing_box_id(sid: u64) -> Result<i64, String> {
    let id = i64::try_from(sid).map_err(|_| "box id exceeds engine range")?;
    discover::discover().contains_key(&id).then_some(id)
        .ok_or_else(|| "no slopbox".into())
}

fn dispatch_action(
    state: &State,
    request: crate::generated_wire::ActionRequest,
) -> Result<crate::generated_wire::ActionSuccess, String> {
    use crate::generated_wire::{ActionRequest, ActionSuccess};
    match request {
        ActionRequest::Sudtrace { sid } => {
            let id = existing_box_id(sid)?;
            let live = lock(state)
                .overlay
                .clone()
                .and_then(|overlay| overlay.live_box(id));
            crate::sud::trace_events(live, id).map(|value| ActionSuccess::Sudtrace { value })
        }
        request @ (ActionRequest::Apply { sid } | ActionRequest::Discard { sid }) => {
            let applying = matches!(request, ActionRequest::Apply { .. });
            let id = existing_box_id(sid)?;
            if box_is_running(state, id) {
                return Err("box is running; stop it first".into());
            }
            let context = crate::review::NestCtx::new(lock(state).overlay.clone());
            let value = if applying {
                let result = crate::review::apply_typed(id, &Value::Null, &context)?;
                crate::generated_wire::ActionMutationResult {
                    r#box: sid,
                    count: result.applied.as_slice().len() as u64,
                    errors: result.errors,
                }
            } else {
                let result = crate::review::discard_typed(id, &Value::Null, &context)?;
                crate::generated_wire::ActionMutationResult {
                    r#box: sid,
                    count: result.discarded.as_slice().len() as u64,
                    errors: result.errors,
                }
            };
            drop_if_empty(state, id);
            Ok(if applying {
                ActionSuccess::Apply { value }
            } else {
                ActionSuccess::Discard { value }
            })
        }
        ActionRequest::Rename { sid, new } => {
            let id = i64::try_from(sid).map_err(|_| "box id exceeds engine range")?;
            let boxes = discover::discover();
            if !boxes.contains_key(&id) {
                return Err("no slopbox".into());
            }
            let old = discover::display_path(&boxes, id);
            let live = lock(state)
                .overlay
                .clone()
                .and_then(|overlay| overlay.live_box(id));
            match live {
                Some(capture) => capture.set_meta("name", new.as_str()),
                None => {
                    let capture = crate::capture::BoxState::create(id)
                        .map_err(|error| format!("open box for rename: {error}"))?;
                    capture.set_meta("name", new.as_str());
                }
            }
            broadcast(
                state,
                &json!({
                    "type": "session_renamed",
                    "session_id": id.to_string(),
                    "name": new.as_str(),
                }),
            );
            Ok(ActionSuccess::Rename {
                value: crate::generated_wire::RenameResult {
                    old_display_path: crate::wire::BoundedText::new(old).map_err(|error| {
                        format!("old display path exceeds relation bound: {error:?}")
                    })?,
                    name: new,
                },
            })
        }
        ActionRequest::Processes { sid } => {
            let id = existing_box_id(sid)?;
            let rows = crate::wire::BoundedVec::new(discover::processes_typed(id)?)
                .map_err(|error| format!("process rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::Processes { value: rows })
        }
        ActionRequest::ProcessesLive { sid } => {
            let id = i64::try_from(sid).map_err(|_| "box id exceeds engine range")?;
            let live = lock(state).box_runpids.contains_key(&id);
            let value = if live {
                Some(crate::wire::BoundedVec::new(discover::processes_typed(id)?)
                    .map_err(|error| format!(
                        "live process rows exceed relation bound: {error:?}"
                    ))?)
            } else {
                None
            };
            Ok(ActionSuccess::ProcessesLive { value })
        }
        ActionRequest::Outputs { sid } => {
            let id = existing_box_id(sid)?;
            let rows = crate::wire::BoundedVec::new(discover::outputs_typed(id)?)
                .map_err(|error| format!("output rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::Outputs { value: rows })
        }
        ActionRequest::Brushprov { sid } => {
            let id = existing_box_id(sid)?;
            let rows = crate::wire::BoundedVec::new(discover::brushprov_typed(id)?)
                .map_err(|error| format!("pipeline rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::Brushprov { value: rows })
        }
        ActionRequest::BuildEdges { sid } => {
            let id = existing_box_id(sid)?;
            let rows = crate::wire::BoundedVec::new(discover::build_edges_typed(id)?)
                .map_err(|error| format!("build edge rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::BuildEdges { value: rows })
        }
        ActionRequest::ApiLog { sid } => {
            let id = existing_box_id(sid)?;
            let value = crate::wire::BoundedVec::new(discover::api_log_typed(id)?)
                .map_err(|error| format!("API log rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::ApiLog { value })
        }
        ActionRequest::ApiLogDetail { sid, row } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::ApiLogDetail {
                value: discover::api_log_detail_typed(id, row)?,
            })
        }
        ActionRequest::Webcap { sid } => {
            let id = existing_box_id(sid)?;
            let value = crate::wire::BoundedVec::new(discover::webcap_typed(id)?)
                .map_err(|error| format!("web capture rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::Webcap { value })
        }
        ActionRequest::WebcapDetail { sid, row } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::WebcapDetail {
                value: discover::webcap_detail_typed(id, row)?,
            })
        }
        ActionRequest::WebcapBody { sid, row } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::WebcapBody {
                value: discover::webcap_body_typed(id, row)?,
            })
        }
        ActionRequest::ProcPipeline { sid, row } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::ProcPipeline {
                value: discover::proc_pipeline_typed(id, row)?,
            })
        }
        ActionRequest::OutputPipeline { sid, output } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::OutputPipeline {
                value: discover::output_pipeline_typed(id, output)?,
            })
        }
        ActionRequest::PipelineProcs { sid, pipeline } => {
            let id = existing_box_id(sid)?;
            let value = crate::wire::BoundedVec::new(
                discover::pipeline_procs_typed(id, pipeline)?)
                .map_err(|error| format!(
                    "pipeline process rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::PipelineProcs { value })
        }
        ActionRequest::OutputDetail { sid, output } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::OutputDetail {
                value: discover::output_detail_typed(id, output)?,
            })
        }
        ActionRequest::ProcInfo { sid, row } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::ProcInfo { value: discover::proc_info_typed(id, row)? })
        }
        ActionRequest::ProcProv { sid, row } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::ProcProv { value: discover::proc_prov_typed(id, row)? })
        }
        ActionRequest::ProcRoots { sid } => {
            let id = existing_box_id(sid)?;
            let value = crate::wire::BoundedVec::new(discover::proc_roots_typed(id)?)
                .map_err(|error| format!(
                    "process root rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::ProcRoots { value })
        }
        ActionRequest::ProcessEnv { sid, row } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::ProcessEnv { value: discover::process_env_typed(id, row)? })
        }
        ActionRequest::WriterId { sid, rel } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::WriterId {
                value: discover::last_writer_id_typed(id, rel.as_slice())?,
            })
        }
        ActionRequest::FirstWriterId { sid, rel } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::FirstWriterId {
                value: discover::first_writer_id_typed(id, rel.as_slice())?,
            })
        }
        ActionRequest::FirstWriterProv { sid, rel } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::FirstWriterProv {
                value: discover::first_writer_prov_typed(id, rel.as_slice())?,
            })
        }
        ActionRequest::DisplayPath { sid } => {
            let id = i64::try_from(sid).map_err(|_| "box id exceeds engine range")?;
            let boxes = discover::discover();
            let value = if boxes.contains_key(&id) {
                Some(crate::wire::BoundedText::new(discover::display_path(&boxes, id))
                    .map_err(|error| {
                        format!("display path exceeds relation bound: {error:?}")
                    })?)
            } else {
                None
            };
            Ok(ActionSuccess::DisplayPath { value })
        }
        ActionRequest::ResolveBox { name_or_id } => {
            let boxes = discover::discover();
            let value = resolve(&boxes, name_or_id.as_str())
                .and_then(|id| u64::try_from(id).ok());
            Ok(ActionSuccess::ResolveBox { value })
        }
        ActionRequest::Select { sid } => {
            let id = existing_box_id(sid)?;
            lock(state).selected = Some(id.to_string());
            Ok(ActionSuccess::Select { value: () })
        }
        ActionRequest::Ping => {
            broadcast(state, &json!({"type": "pong"}));
            Ok(ActionSuccess::Ping { value: () })
        }
        ActionRequest::ReloadRules => {
            if let Some(overlay) = lock(state).overlay.clone() {
                overlay.reload_rules();
            }
            Ok(ActionSuccess::ReloadRules { value: () })
        }
        ActionRequest::Verbs { filter } => {
            let filter = filter.as_ref().map_or("", |value| value.as_str());
            let rows = crate::prolog::global()?.ui_action_help_matching(filter)?;
            let value = crate::wire::BoundedVec::new(rows)
                .map_err(|error| format!("help rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::Verbs { value })
        }
        ActionRequest::SessionDicts => {
            let boxes = discover::discover();
            let (run_pids, overlay) = {
                let shared = lock(state);
                (shared.box_runpids.clone(), shared.overlay.clone())
            };
            let rows = boxes.values().map(|row| {
                let errors = overlay.as_ref()
                    .map(|overlay| overlay.ext_errors(row.box_id))
                    .unwrap_or_default();
                discover::session_typed(
                    &boxes,
                    row,
                    run_pids.get(&row.box_id).copied(),
                    &errors,
                )
            }).collect::<Result<Vec<_>, _>>()?;
            let value = crate::wire::BoundedVec::new(rows)
                .map_err(|error| format!("session rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::SessionDicts { value })
        }
        ActionRequest::ViewOpen {
            kind,
            r#box,
            filter,
            running_only,
        } => {
            let id = existing_box_id(r#box)?;
            let mut shared = lock(state);
            let live = shared.box_runpids.contains_key(&id);
            let Shared {
                views,
                next_view_id,
                ..
            } = &mut *shared;
            crate::views::open(
                views,
                next_view_id,
                kind,
                id,
                filter,
                running_only && live,
            )
            .map(|value| ActionSuccess::ViewOpen { value })
        }
        ActionRequest::ViewFilter { view, filter } => {
            crate::views::set_filter(&mut lock(state).views, view, filter)
                .map(|value| ActionSuccess::ViewFilter { value })
        }
        ActionRequest::ViewFind { view, row_id } => {
            crate::views::find(&lock(state).views, view, row_id)
                .map(|value| ActionSuccess::ViewFind { value })
        }
        ActionRequest::ViewWindow { view, start, size } => {
            crate::views::window(&lock(state).views, view, start, size)
                .map(|value| ActionSuccess::ViewWindow { value })
        }
        ActionRequest::ViewClose { view } => {
            crate::views::close(&mut lock(state).views, view)?;
            Ok(ActionSuccess::ViewClose { value: () })
        }
        ActionRequest::Quit => {
            let fds: Vec<i32> = lock(state).box_pids.values().copied().collect();
            for fd in fds {
                pidfd_signal(fd, libc::SIGTERM);
            }
            unsafe {
                libc::kill(libc::getpid(), libc::SIGTERM);
            }
            Ok(ActionSuccess::Quit { value: () })
        }
        other => Err(format!(
            "typed control action {} is not implemented",
            other.handler()
        )),
    }
}

fn legacy_path_errors(
    errors: crate::wire::BoundedVec<
        crate::generated_wire::PathError,
        0,
        { crate::generated_wire::LIMIT_COLLECTION_ITEMS },
    >,
) -> Value {
    Value::Array(
        errors
            .into_inner()
            .into_iter()
            .map(|error| {
                json!({
                    "path": error.path.map(|path|
                        String::from_utf8_lossy(path.as_slice()).into_owned()).unwrap_or_default(),
                    "error": error.message.into_inner(),
                })
            })
            .collect(),
    )
}

fn legacy_control_reply(success: crate::generated_wire::ActionSuccess) -> Value {
    use crate::generated_wire::ActionSuccess;
    match success {
        ActionSuccess::Sudtrace { value } => legacy_sudtrace_reply(value),
        ActionSuccess::Apply { value } | ActionSuccess::Discard { value } => json!({
            "ok": true,
            "count": value.count,
            "sid": value.r#box.to_string(),
            "errors": legacy_path_errors(value.errors),
        }),
        ActionSuccess::Rename { value } => json!({
            "ok": true,
            "old": value.old_display_path.into_inner(),
            "name": value.name.into_inner(),
        }),
        ActionSuccess::Quit { .. } => json!({"ok": true}),
        other => json!({
            "ok": false,
            "error": format!("wrong typed control result opcode: {}", other.code()),
        }),
    }
}

fn legacy_view_kind(value: Option<&str>) -> Result<crate::generated_wire::ViewKind, String> {
    match value {
        Some("changes") => Ok(crate::generated_wire::ViewKind::Changes),
        Some("procs") => Ok(crate::generated_wire::ViewKind::Processes),
        Some("outputs") => Ok(crate::generated_wire::ViewKind::Outputs),
        Some("pipelines") => Ok(crate::generated_wire::ViewKind::Pipelines),
        Some("build_edges") => Ok(crate::generated_wire::ViewKind::BuildEdges),
        Some(kind) => Err(format!("unknown view kind {kind:?}")),
        None => Err("missing view kind".into()),
    }
}

fn legacy_filter_spec(value: &Value) -> Result<Option<crate::generated_wire::FilterSpec>, String> {
    use crate::generated_wire::{FilterClause, FilterJoin, FilterKind};
    if value.is_null() {
        return Ok(None);
    }
    let clauses = value
        .as_array()
        .ok_or("view filter must be null or an array")?
        .iter()
        .map(|clause| {
            let kind = match clause.get("kind").and_then(Value::as_str) {
                Some("path") => FilterKind::Path,
                Some("box") => FilterKind::Box,
                Some("exe") => FilterKind::Exe,
                Some("cwd") => FilterKind::Cwd,
                Some("arg") => FilterKind::Arg,
                Some("ids") => FilterKind::Ids,
                Some("err") => FilterKind::Err,
                Some("cmd") => FilterKind::Cmd,
                Some("target") => FilterKind::Target,
                Some(kind) => return Err(format!("unknown filter kind {kind:?}")),
                None => return Err("filter clause has no kind".into()),
            };
            let pattern = clause
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or("filter clause has no pattern")?;
            let join = match clause.get("join").and_then(Value::as_str) {
                Some("and") => FilterJoin::And,
                Some("or") => FilterJoin::Or,
                Some(join) => return Err(format!("unknown filter join {join:?}")),
                None => return Err("filter clause has no join".into()),
            };
            Ok(FilterClause {
                kind,
                pattern: crate::wire::BoundedText::new(pattern.into())
                    .map_err(|error| format!("filter pattern exceeds relation bound: {error:?}"))?,
                join,
                negated: clause
                    .get("negate")
                    .and_then(Value::as_bool)
                    .ok_or("filter clause has no negate flag")?,
                enabled: clause
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or("filter clause has no enabled flag")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    crate::wire::BoundedVec::new(clauses)
        .map(Some)
        .map_err(|error| format!("filter clause count exceeds relation bound: {error:?}"))
}

fn legacy_bytes<const MAXIMUM: usize>(value: crate::wire::BoundedBytes<MAXIMUM>) -> String {
    String::from_utf8_lossy(value.as_slice()).into_owned()
}

fn legacy_pipeline_provenance(record: crate::generated_wire::PipelineProvenance) -> Value {
    crate::discover::pipeline_provenance_json(&record)
}

fn legacy_view_window(value: crate::generated_wire::ViewWindow) -> Value {
    use crate::generated_wire::{ChangeKind, EchoStream, ViewWindow};
    match value {
        ViewWindow::Changes { start, total, rows } => json!({
            "start": start,
            "total": total,
            "rows": rows.into_inner().into_iter().map(|row| {
                let kind = match row.kind {
                    ChangeKind::Changed => "changed",
                    ChangeKind::Deleted => "deleted",
                    ChangeKind::Symlink => "symlink",
                    ChangeKind::Created => "created",
                    ChangeKind::Modified => "modified",
                    ChangeKind::Xattr => "xattr",
                    ChangeKind::Directory => "dir",
                    ChangeKind::XattrOnly => "xattr-only",
                };
                let mut value = json!({
                    "path": legacy_bytes(row.path),
                    "name": legacy_bytes(row.name),
                    "kind": kind,
                    "size": row.size,
                    "depth": row.depth,
                    "connector": row.connector,
                });
                if let Some(path) = row.xattr_for {
                    value["xattr_for"] = Value::String(legacy_bytes(path));
                }
                if let Some(key) = row.xattr_key {
                    value["xattr_key"] = Value::String(legacy_bytes(key));
                }
                value
            }).collect::<Vec<_>>(),
        }),
        ViewWindow::Processes { start, total, rows } => json!({
            "start": start,
            "total": total,
            "rows": rows.into_inner().into_iter().map(|row| json!({
                "rid": row.id,
                "tgid": row.tgid,
                "ppid": row.ppid,
                "exe": legacy_bytes(row.executable),
                "argv": row.argv.into_inner().into_iter()
                    .map(legacy_bytes).collect::<Vec<_>>(),
                "depth": row.depth,
                "connector": row.connector,
            })).collect::<Vec<_>>(),
        }),
        ViewWindow::Outputs { start, total, rows } => json!({
            "start": start,
            "total": total,
            "rows": rows.into_inner().into_iter().map(|row| json!({
                "id": row.output.id,
                "ts": row.output.time,
                "process_id": row.output.process,
                "stream": match row.output.stream {
                    EchoStream::Stdout => 0,
                    EchoStream::Stderr => 1,
                },
                "len": row.output.length,
                "exe": legacy_bytes(row.executable),
                "tgid": row.tgid,
            })).collect::<Vec<_>>(),
        }),
        ViewWindow::Pipelines { start, total, rows } => json!({
            "start": start,
            "total": total,
            "rows": rows.into_inner().into_iter().map(|row| json!({
                "id": row.id,
                "ts": row.time,
                "cmd": row.command.into_inner(),
                "record": row.record.map(legacy_pipeline_provenance),
                "pipeline": row.pipeline,
                "spawn_ts": row.spawned_at,
                "nested": row.nested,
                "uid": row.uid.unwrap_or(0),
                "parent_uid": row.parent_uid.unwrap_or(0),
                "done_ts": row.done_at.unwrap_or(0.0),
                "exit_code": row.exit_code.unwrap_or(-1),
                "processes": row.processes.into_inner(),
            })).collect::<Vec<_>>(),
        }),
        ViewWindow::BuildEdges { start, total, rows } => json!({
            "start": start,
            "total": total,
            "rows": rows.into_inner().into_iter().map(|row| json!({
                "id": row.id,
                "ts": row.time,
                "outs": row.outputs.into_inner().into_iter()
                    .map(legacy_bytes).collect::<Vec<_>>(),
                "ins": row.inputs.into_inner().into_iter()
                    .map(legacy_bytes).collect::<Vec<_>>(),
                "cmd": row.command.map(|value| value.into_inner()),
                "started_ts": row.started_at,
                "ended_ts": row.ended_at,
                "exit_code": row.exit_code,
                "output_excerpt": row.output_excerpt.map(|value| value.into_inner()),
            })).collect::<Vec<_>>(),
        }),
    }
}

fn legacy_box_session(value: crate::generated_wire::BoxSession) -> Value {
    use crate::generated_wire::SessionStatus;
    let status = match value.status {
        SessionStatus::Running => "running",
        SessionStatus::Finished => "finished",
        SessionStatus::Failed => "failed",
        SessionStatus::Killed => "killed",
    };
    let killed = value.status == SessionStatus::Killed;
    let errored = value.status == SessionStatus::Failed;
    let run_pid = value.run_pid.unwrap_or(0);
    json!({
        "session_id": value.r#box.to_string(),
        "cmd": value.command.into_inner().into_iter().map(legacy_bytes)
            .collect::<Vec<_>>(),
        "shm_dir": legacy_bytes(value.shared_memory),
        "killed": killed,
        "errored": errored,
        "exit_code": value.exit_code,
        "live": value.live,
        "has_sqlar": value.has_archive,
        "box_id": value.r#box,
        "name": value.name.into_inner(),
        "run_pid": run_pid,
        "run_pidfd": -1,
        "parent_box_id": value.parent,
        "parents": value.parents.into_inner(),
        "attachments": value.attachments.into_inner().into_iter().map(|row| {
            let mut projected = json!({
                "name": row.name.into_inner(),
                "kind": row.kind.into_inner(),
                "rev": row.revision.into_inner(),
            });
            if let Some(error) = row.error {
                projected["error"] = Value::String(error.into_inner());
            }
            projected
        }).collect::<Vec<_>>(),
        "started": value.started_at,
        "pid": run_pid,
        "status": status,
        "upper": legacy_bytes(value.upper),
        "path": value.display_path.into_inner(),
    })
}

fn legacy_ui_action_reply(
    result: Result<crate::generated_wire::ActionSuccess, String>,
) -> Value {
    use crate::generated_wire::ActionSuccess;
    let payload = match result {
        Ok(ActionSuccess::ViewOpen { value }) => json!({
            "view_id": value.view,
            "total": value.total,
        }),
        Ok(ActionSuccess::ViewFilter { value }) => json!({"total": value.total}),
        Ok(ActionSuccess::ViewFind { value: Some(position) }) => {
            json!({"ok": true, "pos": position})
        }
        Ok(ActionSuccess::ViewFind { value: None }) => {
            json!({"ok": false, "error": "not found"})
        }
        Ok(ActionSuccess::ViewWindow { value }) => legacy_view_window(value),
        Ok(ActionSuccess::ViewClose { .. }) => json!({"ok": true}),
        Ok(ActionSuccess::Processes { value }) => {
            discover::process_rows_json(value.as_slice())
        }
        Ok(ActionSuccess::ProcessesLive { value }) => match value {
            Some(rows) => discover::process_rows_json(rows.as_slice()),
            None => Value::Null,
        },
        Ok(ActionSuccess::Outputs { value }) => {
            discover::output_rows_json(value.as_slice())
        }
        Ok(ActionSuccess::Brushprov { value }) => {
            discover::pipeline_rows_json(value.as_slice())
        }
        Ok(ActionSuccess::BuildEdges { value }) => {
            discover::build_edge_rows_json(value.as_slice())
        }
        Ok(ActionSuccess::ApiLog { value }) => discover::api_log_rows_json(value.as_slice()),
        Ok(ActionSuccess::ApiLogDetail { value }) => value.as_ref()
            .map(discover::api_log_detail_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::Webcap { value }) => discover::webcap_rows_json(value.as_slice()),
        Ok(ActionSuccess::WebcapDetail { value }) => value.as_ref()
            .map(discover::webcap_detail_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::WebcapBody { value }) => value.as_ref()
            .map(discover::webcap_body_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::ProcPipeline { value })
        | Ok(ActionSuccess::OutputPipeline { value }) => value.as_ref()
            .map(discover::pipeline_summary_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::PipelineProcs { value })
        | Ok(ActionSuccess::ProcRoots { value }) => json!(value.as_slice()),
        Ok(ActionSuccess::OutputDetail { value }) => value.as_ref()
            .map(discover::output_detail_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::ProcInfo { value }) => value.as_ref()
            .map(discover::process_info_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::ProcProv { value }) => value.as_ref()
            .map(discover::process_subject_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::ProcessEnv { value }) => discover::environment_json(&value),
        Ok(ActionSuccess::WriterId { value })
        | Ok(ActionSuccess::FirstWriterId { value }) => json!(value),
        Ok(ActionSuccess::FirstWriterProv { value }) => value.as_ref()
            .map(discover::writer_provenance_json).unwrap_or(Value::Null),
        Ok(ActionSuccess::DisplayPath { value }) => value
            .map(|value| Value::String(value.into_inner())).unwrap_or(Value::Null),
        Ok(ActionSuccess::ResolveBox { value }) => value
            .map(|value| Value::String(value.to_string())).unwrap_or(Value::Null),
        Ok(ActionSuccess::Select { .. }) => json!({"ok": true}),
        Ok(ActionSuccess::Ping { .. }) => json!("pong"),
        Ok(ActionSuccess::ReloadRules { .. }) => Value::Null,
        Ok(ActionSuccess::Verbs { value }) => Value::Array(value.into_inner().into_iter()
            .map(|row| json!({
                "verb": row.verb.into_inner(),
                "args": row.arguments.into_inner(),
                "help": row.description.into_inner(),
            }))
            .collect()),
        Ok(ActionSuccess::SessionDicts { value }) => Value::Array(value.into_inner()
            .into_iter().map(legacy_box_session).collect()),
        Ok(other) => {
            return json!({
                "ok": false,
                "error": format!("wrong typed UI result opcode: {}", other.code()),
            });
        }
        Err(error) => return json!({"ok": false, "error": error}),
    };
    json!({"ok": true, "r": payload})
}

/// Temporary projection for the still-active newline JSON listener. It is not
/// a wire schema or alternate dispatcher: the authoritative result is already
/// `SudTraceView`, and this function disappears with the coordinated socket
/// cutover.
fn legacy_sudtrace_reply(value: crate::generated_wire::SudTraceView) -> Value {
    use crate::generated_wire::SudEventKind;
    Value::Object(
        [
            ("ok".into(), Value::Bool(true)),
            (
                "events".into(),
                Value::Array(
                    value
                        .events
                        .into_inner()
                        .into_iter()
                        .map(|event| {
                            let kind = match event.kind {
                                SudEventKind::Exec => "EXEC".into(),
                                SudEventKind::Argv => "ARGV".into(),
                                SudEventKind::Env => "ENV".into(),
                                SudEventKind::Open => "OPEN".into(),
                                SudEventKind::Cwd => "CWD".into(),
                                SudEventKind::Stdout => "STDOUT".into(),
                                SudEventKind::Stderr => "STDERR".into(),
                                SudEventKind::Exit => "EXIT".into(),
                                SudEventKind::Prof => "PROF".into(),
                                SudEventKind::Unknown { code } => code.to_string(),
                            };
                            json!({
                                "ts_ns": event.time_ns,
                                "kind": kind,
                                "pid": event.pid,
                                "tgid": event.tgid,
                                "ppid": event.ppid,
                                "extras": event.extras.into_inner(),
                                "text": event.text.into_inner(),
                            })
                        })
                        .collect(),
                ),
            ),
            ("truncated".into(), Value::Bool(value.truncated)),
        ]
        .into_iter()
        .collect(),
    )
}

fn resolve(boxes: &std::collections::BTreeMap<i64, discover::Box_>, ident: &str) -> Option<i64> {
    if let Ok(id) = ident.parse::<i64>() {
        if boxes.contains_key(&id) {
            return Some(id);
        }
    }
    boxes
        .values()
        .find(|b| b.name == ident || discover::display_path(boxes, b.box_id) == ident)
        .map(|b| b.box_id)
}

/// The box_id of the box NAMED `name` whose parent is `parent` (None=top-level),
/// else None — the rerun/uniqueness lookup (siblings have unique NAMEs). Mirrors
/// the Python Supervisor._find_named_child (scans discovered on-disk boxes).
fn find_named_child(
    boxes: &std::collections::BTreeMap<i64, discover::Box_>,
    name: &str,
    parent: Option<i64>,
) -> Option<i64> {
    boxes
        .values()
        .find(|b| b.name == name && b.parent == parent)
        .map(|b| b.box_id)
}

fn selected_sid(state: &State) -> Option<i64> {
    lock(state)
        .selected
        .as_ref()
        .and_then(|s| s.parse::<i64>().ok())
}

fn flows_dir_for(box_id: i64) -> Option<std::path::PathBuf> {
    let d = crate::paths::state_home().join(format!("flows/box{box_id}"));
    if d.is_dir() { Some(d) } else { None }
}

// Accept the sid as either a JSON number OR a string-of-int. Most UI verbs
// send a string (cur_sid is a String) but a few — load_pipelines /
// load_build_edges and assorted tests — send the i64 straight from
// cur_sid_i64; without the dual parse those silently got None and the verb
// returned an empty default. view.open already had this same dual parse.
fn arg_sid(args: &[Value]) -> Option<i64> {
    let v = args.first()?;
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn legacy_u64(args: &[Value], index: usize) -> Option<u64> {
    let value = args.get(index)?;
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn legacy_path_arg(args: &[Value], index: usize) -> Option<crate::generated_wire::Path> {
    crate::wire::BoundedBytes::new(args.get(index)?.as_str()?.as_bytes().to_vec()).ok()
}

fn flow_args<'a>(state: &State, args: &'a [Value]) -> Option<(i64, &'a Value)> {
    match args {
        [value] => Some((selected_sid(state)?, value)),
        [sid, value] => Some((sid.as_str()?.parse().ok()?, value)),
        _ => None,
    }
}

/// Unconditionally remove a box: drop it from the overlay, delete its sqlar +
/// backing + pool blobs, broadcast session_removed. The `delete` verb's body.
/// Whether any box stacks ON `id` (i.e. `id` is a parent). A box with children
/// must not be raw-reaped — that would orphan them; delete cascades and the
/// auto-drop skips such boxes.
fn has_children(id: i64) -> bool {
    discover::discover().values().any(|b| b.parent == Some(id))
}

fn reap(state: &State, id: i64) {
    if let Some(ov) = lock(state).overlay.clone() {
        ov.remove_box(id);
    }
    let _ = std::fs::remove_file(crate::paths::state_home().join(format!("{id}.sqlar")));
    let _ = std::fs::remove_dir_all(crate::paths::live_home().join(id.to_string()));
    let _ = std::fs::remove_dir_all(crate::paths::live_home().join("blob").join(id.to_string()));
    broadcast(
        state,
        &json!({"type": "session_removed",
                             "session_id": id.to_string()}),
    );
}

/// dissolve: remove a box WITHOUT affecting any other box's view. The box's
/// changes are promoted DOWN — every path it captured is copied into each
/// child that has no entry of its own for it (copy_down_entry), so each child
/// keeps reading exactly what it saw through this box once it's gone — then
/// the children are re-parented onto this box's own parent, and the box is
/// reaped. Nothing is ever written to the parent or the host: promotion UP is
/// `apply`'s job, and only apply's.
///
/// (The three user actions: apply promotes a box's changes UP into its parent
/// or the host while preserving sibling views; dissolve removes a box
/// promoting its changes DOWN; rotate swaps a parent-child pair. None of the
/// three changes the merged view of any other box.)
///
/// Children may be LIVE: copy-down and re-parent both route through the live
/// BoxState (connection + RAM mirror) when the child is running, so a mounted
/// FUSE view keeps serving the right bytes — no rival on-disk handle racing
/// the serve thread.
fn dissolve(state: &State, id: i64) -> Value {
    free_box(state, id)
}

/// Free box `id`, KEEPING any boxes stacked on it — the shared core of
/// `dissolve` and `delete` (two names for the same operation). Whatever the
/// box contributed to its children's merged view is copied DOWN into each
/// child that lacks its own entry (so the children read exactly what they saw
/// before), then the children are re-parented onto this box's own parent,
/// then the box is reaped. This is why the operation can never orphan/corrupt
/// a child. The box's own writes never reach the parent or the host; only the
/// copied-down content survives, in the children.
///
/// A running box is refused when work needs its blobs stable (a copy-down to
/// children). A leaf delete (no children) is a plain reap and may proceed
/// even while running.
fn free_box(state: &State, id: i64) -> Value {
    let boxes = discover::discover();
    let Some(me) = boxes.get(&id) else {
        return json!({"ok": false, "error": "no slopbox"});
    };
    let grandparent = me.parent;
    let children: Vec<i64> = boxes
        .values()
        .filter(|b| b.parent == Some(id))
        .map(|b| b.box_id)
        .collect();
    if lock(state).box_pids.contains_key(&id) && !children.is_empty() {
        return json!({"ok": false, "error": "box is running; stop it first"});
    }
    let ov = lock(state).overlay.clone();
    // Copy-down: snapshot this box's contributed view into each child that has
    // no entry of its own, so freeing the parent doesn't change what the child
    // sees. A live child's copy-down goes through its live BoxState.
    // Fail-closed: if any copy errors, free nothing.
    if !children.is_empty() {
        let paths = crate::review::changed_paths(id);
        for &child in &children {
            let live = ov.as_ref().and_then(|o| o.live_box(child));
            for rel in &paths {
                if let Err(e) = crate::review::copy_down_entry(id, child, rel, live.as_deref()) {
                    return json!({"ok": false,
                        "error": format!("copy-down to box {child} failed: {e}"),
                        "path": rel});
                }
            }
        }
    }
    // Re-parent the children onto this box's own parent. For a LIVE child write
    // the meta through its BoxState (one connection); for one at rest write the
    // on-disk sqlar. Also update the overlay's in-RAM parent.
    //
    // Closure carry-down: if THIS box held `no_host_fallback` (an OCI image's
    // --no-parent rootfs base is the only box that does), it was the bottom that
    // closed the chain for every child. Re-parenting a child onto the
    // grandparent (None / top-level for a base) would drop that closure and let
    // resolve()/scan_dir() fall the child's absent paths through to the real
    // host. So each child inherits the bit — exactly like content copy-down, the
    // child must keep seeing what it saw before (closed), not gain host fs.
    let me_no_host = me.meta.get("no_host_fallback").map(String::as_str) == Some("1");
    for &child in &children {
        match ov.as_ref().and_then(|o| o.live_box(child)) {
            Some(cb) => {
                cb.set_meta(
                    "parent_box_id",
                    &grandparent.map(|p| p.to_string()).unwrap_or_default(),
                );
                if me_no_host {
                    cb.set_meta("no_host_fallback", "1");
                    cb.set_no_host_fallback(true);
                }
            }
            None => {
                let _ = crate::review::set_parent_meta(child, grandparent);
                if me_no_host {
                    let _ = crate::review::set_no_host_meta(child);
                }
            }
        }
        if let Some(ov) = &ov {
            ov.set_box_parent(child, grandparent);
        }
    }
    reap(state, id);
    json!({"ok": true, "reparented": children})
}

/// Apply box `id`'s changes onto a fresh COPY of its parent, leaving the real
/// parent (and its other children) untouched. Composes existing primitives:
/// create a new box beside the parent (child of the grandparent), copy the
/// parent's own changes into it (so it starts as a snapshot of the parent),
/// then promote `id`'s changes on top. The result is a new sibling box holding
/// "parent + id's changes"; nothing else in the tree moves.
fn apply_to_copy(
    state: &State,
    boxes: &std::collections::BTreeMap<i64, discover::Box_>,
    id: i64,
) -> Value {
    let Some(me) = boxes.get(&id) else {
        return json!({"ok": false, "error": "no slopbox"});
    };
    let Some(parent) = me.parent else {
        return json!({"ok": false,
            "error": "box has no parent box to copy (a top-level box applies to the host)"});
    };
    if box_is_running(state, id) || box_is_running(state, parent) {
        return json!({"ok": false, "error": "box or its parent is running; stop it first"});
    }
    let grandparent = boxes.get(&parent).and_then(|b| b.parent);
    let Some(ov) = lock(state).overlay.clone() else {
        return json!({"ok": false, "error": "overlay not mounted"});
    };
    let new_id = boxes.keys().max().copied().unwrap_or(0) + 1;
    let parent_name = boxes
        .get(&parent)
        .map(|b| b.name.clone())
        .unwrap_or_default();
    let new_name = if parent_name.is_empty() {
        format!("copy{new_id}")
    } else {
        format!("{parent_name}-copy")
    };
    // 1. Create the copy box as a child of the grandparent (a sibling of the
    //    real parent).
    match crate::capture::BoxState::create(new_id) {
        Ok(b) => {
            b.set_parent(grandparent);
            if let Some(gp) = grandparent {
                b.set_meta("parent_box_id", &gp.to_string());
            }
            b.set_meta("name", &new_name);
            ov.add_box(std::sync::Arc::new(b));
        }
        Err(e) => return json!({"ok": false, "error": format!("create copy box: {e}")}),
    }
    // 2. Copy the parent's OWN changes into the copy (snapshot of the parent).
    for rel in crate::review::changed_paths(parent) {
        if let Err(e) = crate::review::copy_down_entry(parent, new_id, &rel, None) {
            return json!({"ok": false, "error": format!("copy parent '{rel}': {e}")});
        }
    }
    // 3. Promote this box's changes onto the copy.
    let mut applied = 0usize;
    for rel in crate::review::changed_paths(id) {
        if let Err(e) = crate::review::promote_into_parent(id, new_id, None, &rel) {
            return json!({"ok": false, "error": format!("apply '{rel}' onto copy: {e}")});
        }
        applied += 1;
    }
    broadcast(
        state,
        &json!({"type": "session_new",
        "session_id": new_id.to_string(), "name": new_name}),
    );
    json!({"ok": true, "new_sid": new_id.to_string(), "name": new_name, "applied": applied})
}

/// Audit H3: apply/discard read the box's pool blobs (`blob_path(id, rowid)`) —
/// the exact files a LIVE FUSE write may be mid-`write_at` on. Reading one of
/// those while it's being written stamps a TORN blob onto the host. So, like
/// `dissolve`, apply/discard refuse a still-running box: the box must be stopped
/// (its writers quiesced) before its captured changes are applied or discarded.
/// True == running (caller should refuse). Mirrors dissolve's `box_pids` check.
fn box_is_running(state: &State, id: i64) -> bool {
    lock(state).box_pids.contains_key(&id)
}

/// After apply/discard, reap the box if it has no remaining changes.
fn drop_if_empty(state: &State, id: i64) {
    // Never auto-reap a box that other boxes stack on — dropping it would
    // orphan them (they read their inherited files THROUGH this layer). An
    // empty parent is a harmless empty layer; leave it until a real delete
    // (which cascades) or dissolve (which re-parents) removes it.
    if has_children(id) {
        return;
    }
    if crate::review::session_changes(id)
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(false)
    {
        reap(state, id);
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
        && !s.ends_with('-')
}

/// The runner register handshake. Mints a box_id, creates the backing sentinel
/// (live/<id>/up) and the box's sqlar (root process row from the message's
/// prov), registers the box on the overlay, and acks with the <mnt>/<id> bind
/// target. The SAME connection becomes the box channel — its EOF (handled by
/// the caller via the _box_sid marker) is teardown.
///
/// `peer_pidfd` is the runner's own pidfd (SCM_RIGHTS first fd): we derive its
/// HOST pid for kill + parent derivation, and HOLD it for pid-reuse-safe kill.
/// NESTED LAUNCH: a `relname` field means the runner is inside a box; the
/// enclosing box is derived from the runner's /proc ancestry (never trusted
/// from the message), and this box is parented under it. A relname with no
/// derivable enclosing box is an error (the box's pidfd closes on the early
/// return when the caller's loop tears down). Capture mode stays downgraded in
/// the ack (no echo/sinks yet — runner behaves as -t passthrough).
fn register(
    state: &State,
    msg: &Value,
    peer_pidfd: Option<i32>,
    fd2_raw: Option<i32>,
    fd3_raw: Option<i32>,
) -> Value {
    // Assign the post-pidfd SCM_RIGHTS fds to roles from the message. The
    // runner sends them in a fixed order: [tap (if net_mode==tap)] then
    // [sud trace pipe (if want_sud)]. So:
    //   fuse+tap : fd2=tap
    //   sud+tap  : fd2=tap,   fd3=trace
    //   sud+!tap : fd2=trace
    // Own each as an OwnedFd so every early-return path closes it; the tap
    // fd moves into prepare_net and the trace fd into stream_events only on
    // the success path.
    let want_sud_fd = msg
        .get("want_sud")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_tap_fd = msg.get("net_mode").and_then(Value::as_str) == Some("tap");
    let (tap_raw, trace_raw) = match (want_sud_fd, is_tap_fd) {
        (true, true) => (fd2_raw, fd3_raw),
        (true, false) => (None, fd2_raw),
        (false, _) => (fd2_raw, None),
    };
    // Close any fd that didn't get a role (shouldn't happen in normal flow).
    if want_sud_fd && !is_tap_fd {
        if let Some(fd) = fd3_raw {
            unsafe {
                libc::close(fd);
            }
        }
    }
    let tap_fd: Option<std::os::fd::OwnedFd> = tap_raw
        .map(|fd| unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    let sud_trace_owned: Option<std::os::fd::OwnedFd> = trace_raw
        .map(|fd| unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    let ov = lock(state).overlay.clone();
    let Some(ov) = ov else {
        if let Some(fd) = peer_pidfd {
            unsafe {
                libc::close(fd);
            }
        }
        return json!({"ok": false, "error": "overlay mount is not available"});
    };
    // Runner host pid: from the pidfd if sent (correct for nested runners whose
    // own getpid() is a parent-namespace pid); else the claimed tgid (top-level).
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .or_else(|| {
            msg.get("prov")
                .and_then(|p| p.get("tgid"))
                .and_then(Value::as_i64)
                .map(|t| t as i32)
        })
        .unwrap_or(0);
    let boxes = discover::discover();
    // ── PARENT + NAME RESOLUTION ───────────────────────────────────────────
    // IN-BOX (relname present): parent = kernel-derived enclosing box; the box
    //   supplies only a single-segment relative NAME (or "" → auto A<n>).
    // HOST (no relname): top-level by default; a supplied session_id may be a
    //   single NAME or a dotted display path (A.B) whose prefix names the parent.
    let relname = msg.get("relname").and_then(Value::as_str);
    let mut parent: Option<i64> = None;
    let mut name: Option<String> = None;
    if let Some(rel) = relname {
        if !rel.is_empty() && (!valid_name(rel) || rel.contains('.') || rel.contains('/')) {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return json!({"ok": false,
                "error": "invalid relname: must be a single NAME segment"});
        }
        match derive_parent_box(state, host_pid) {
            Some(p) => parent = Some(p),
            None => {
                if let Some(fd) = peer_pidfd {
                    unsafe {
                        libc::close(fd);
                    }
                }
                return json!({"ok": false,
                    "error": "relname supplied but no enclosing box found"});
            }
        }
        if !rel.is_empty() {
            name = Some(rel.to_string());
        }
    } else if let Some(want) = msg
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        if let Some((prefix, last)) = want.rsplit_once('.') {
            // Dotted display path: parent = prefix box (must exist), NAME = last.
            match resolve(&boxes, prefix) {
                Some(p) => {
                    parent = Some(p);
                    name = Some(last.to_string());
                }
                None => {
                    if let Some(fd) = peer_pidfd {
                        unsafe {
                            libc::close(fd);
                        }
                    }
                    return json!({"ok": false,
                        "error": format!("parent box '{prefix}' does not exist")});
                }
            }
        } else {
            name = Some(want.to_string());
        }
    }

    // ── CREATE-VS-RERUN ────────────────────────────────────────────────────
    // A named launch RERUNS the same box_id if a sibling with that NAME already
    // exists under the resolved parent (adds another root to its process forest).
    // An unnamed launch always CREATEs a fresh box_id. The runner's want_rerun is
    // advisory; the authoritative decision is the name lookup (mirrors Python).
    let mut rerun = false;
    let mut existing_id: Option<i64> = None;
    if let Some(ref nm) = name {
        if let Some(eid) = find_named_child(&boxes, nm, parent) {
            existing_id = Some(eid);
            rerun = true;
        }
    }
    if rerun && lock(state).box_pids.contains_key(&existing_id.unwrap()) {
        if let Some(fd) = peer_pidfd {
            unsafe {
                libc::close(fd);
            }
        }
        return json!({"ok": false, "error": "slopbox is already running"});
    }
    let live_max = ov.box_ids().into_iter().max().unwrap_or(0);
    let id =
        existing_id.unwrap_or_else(|| boxes.keys().max().copied().unwrap_or(0).max(live_max) + 1);
    let name = name.unwrap_or_else(|| format!("A{id}"));
    let env_capture = msg
        .get("want_env")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let direct = msg
        .get("want_direct")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // D-parent flags. `want_no_parent` is the runner's explicit "this box has
    // NO parent and the lower chain does NOT bottom at the host /": the box's
    // own contents are its entire filesystem (the bottom of an OCI image
    // stack). It overrides the kernel-derived parent walk, so even a runner
    // nested under another box can declare itself a rootfs.
    let want_no_parent = msg
        .get("want_no_parent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let want_readonly_parent = msg
        .get("want_readonly_parent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let want_capture = msg
        .get("want_capture")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        && !direct;
    let backing = crate::paths::live_home().join(id.to_string());
    if let Err(e) = std::fs::create_dir_all(backing.join("up")) {
        if let Some(fd) = peer_pidfd {
            unsafe {
                libc::close(fd);
            }
        }
        return json!({"ok": false, "error": format!("backing: {e}")});
    }
    let b = match crate::capture::BoxState::create(id) {
        Ok(b) => b,
        Err(e) => {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return json!({"ok": false, "error": format!("sqlar: {e}")});
        }
    };
    // RERUN: reopen the existing box's recorded state so prior writes show
    // through and prior process rows keep their ids (the new root is additive).
    if rerun {
        b.load_mirror();
    }
    b.set_env_capture(env_capture);
    b.set_direct(direct);
    b.set_is_brush(
        msg.get("want_brush")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    b.set_is_api(
        msg.get("want_api")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    // Tap boxes reach the network through the engine's MITM proxy + synthetic
    // DNS, so they need the engine's CA appended to their trust store and their
    // resolver pointed at the gateway. The overlay serves both as shadows gated
    // on this flag (see overlay.rs).
    b.set_is_tap(msg.get("net_mode").and_then(Value::as_str) == Some("tap"));
    b.set_meta("name", &name);
    // --sud (WIP, see engine/DESIGN-sud.md): the box runs under the sud64
    // wrapper with a directory upper instead of on the FUSE mount. Create
    // the upper here so the ack can hand its path to the runner; the
    // post-exit `sud_ingest` verb sweeps it into this BoxState. The trace
    // pipe (fd-1023 read end) came in as its own SCM_RIGHTS fd
    // (sud_trace_owned), separate from the tap fd — so a sud box can be a
    // TAP box too (tap fd → prepare_net, trace fd → stream_events).
    let want_sud = msg
        .get("want_sud")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut sud_trace_fd: Option<std::os::fd::OwnedFd> = sud_trace_owned;
    if want_sud {
        let up = backing.join("sud-up");
        if let Err(e) = std::fs::create_dir_all(&up) {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return json!({"ok": false, "error": format!("sud upper: {e}")});
        }
        b.set_meta("sud", "1");
    }
    // D-parent: `want_no_parent` strips any kernel-derived parent AND closes
    // the lower chain so reads never fall through to the real host. It's the
    // "OCI rootfs" / "Dockerfile FROM scratch" semantic. A child can
    // independently mark itself readonly-parent.
    let mut parent = parent;
    if want_no_parent {
        parent = None;
        b.set_no_host_fallback(true);
        b.set_meta("no_host_fallback", "1");
    }
    if want_readonly_parent {
        b.set_readonly_parent(true);
        b.set_meta("readonly_parent", "1");
    }
    // sud in-box nesting: a `run --sud` issued from INSIDE a running sud
    // box arrives with no relname (sud boxes set no SARUN_BROKER) and no
    // dotted name — derive the enclosing box from the runner's /proc
    // ancestry, exactly like relname registration does. Only a sud
    // enclosure nests (same-in-same); a runner inside a FUSE box never
    // reaches here (the runner rejects --sud under SARUN_BROKER).
    let mut parent = parent;
    if want_sud && parent.is_none() && !want_no_parent {
        if let Some(enc) = derive_parent_box(state, host_pid) {
            if boxes
                .get(&enc)
                .is_some_and(|bx| bx.meta.get("sud").map(String::as_str) == Some("1"))
            {
                parent = Some(enc);
            } else {
                if let Some(fd) = peer_pidfd {
                    unsafe {
                        libc::close(fd);
                    }
                }
                return json!({"ok": false, "error": format!(
                    "sud nesting is same-in-same: enclosing box {enc} is \
                     not a sud box (see engine/DESIGN-sud.md)")});
            }
        }
    }
    if let Some(p) = parent {
        b.set_parent(Some(p));
        b.set_meta("parent_box_id", &p.to_string());
    }
    // sud nesting is same-in-same and FLATTENED (DESIGN-sud.md): one
    // wrapper invocation whose overlay stacks child upper → materialized
    // ancestor states → host. Wrapper-in-wrapper can't work (fixed text
    // address), so the chain must be all-sud and at rest; a RERUN's own
    // prior state is exported as the nearest lower so earlier writes show
    // through (the FUSE analog is load_mirror). Lowers are materialized
    // from the sqlar — the authoritative state — never from the stale
    // sud-up directory.
    let mut sud_lowers: Vec<String> = Vec::new();
    if want_sud {
        // A rerun's own prior state is the nearest lower.
        if rerun {
            let dest = backing.join(format!("sud-lower-{id}"));
            match crate::sud::export_box(id, &dest) {
                Ok(_) => sud_lowers.push(dest.to_string_lossy().into_owned()),
                Err(e) => {
                    if let Some(fd) = peer_pidfd {
                        unsafe {
                            libc::close(fd);
                        }
                    }
                    return json!({"ok": false,
                        "error": format!("sud lower export: {e}")});
                }
            }
        }
        // Ancestor chain. An AT-REST ancestor's truth is its sqlar —
        // export and keep walking. A RUNNING ancestor's truth is its
        // LIVE upper directory stacked on its own register-time layer
        // list (which already covers everything above it) — take those
        // and stop.
        let mut cur = parent;
        let mut seen = std::collections::HashSet::new();
        while let Some(aid) = cur {
            if !seen.insert(aid) {
                break;
            }
            let Some(bx) = boxes.get(&aid) else { break };
            if bx.meta.get("sud").map(String::as_str) != Some("1") {
                if let Some(fd) = peer_pidfd {
                    unsafe {
                        libc::close(fd);
                    }
                }
                return json!({"ok": false, "error": format!(
                    "sud nesting is same-in-same: ancestor box {aid} is \
                     not a sud box (see engine/DESIGN-sud.md)")});
            }
            if lock(state).box_pids.contains_key(&aid) {
                match crate::sud::layers(aid) {
                    Some(mut ls) => {
                        sud_lowers.append(&mut ls);
                        break;
                    }
                    None => {
                        if let Some(fd) = peer_pidfd {
                            unsafe {
                                libc::close(fd);
                            }
                        }
                        return json!({"ok": false, "error": format!(
                            "running sud box {aid} has no recorded layer \
                             list (engine restarted under it?)")});
                    }
                }
            }
            let dest = backing.join(format!("sud-lower-{aid}"));
            match crate::sud::export_box(aid, &dest) {
                Ok(_) => sud_lowers.push(dest.to_string_lossy().into_owned()),
                Err(e) => {
                    if let Some(fd) = peer_pidfd {
                        unsafe {
                            libc::close(fd);
                        }
                    }
                    return json!({"ok": false,
                        "error": format!("sud lower export: {e}")});
                }
            }
            cur = bx.parent;
        }
        // A fresh run starts from an empty upper (a rerun's prior state
        // just became the nearest lower; stale upper contents would
        // re-ingest as phantom writes).
        let up = backing.join("sud-up");
        let _ = std::fs::remove_dir_all(&up);
        if let Err(e) = std::fs::create_dir_all(&up) {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return json!({"ok": false, "error": format!("sud upper: {e}")});
        }
        // Record this box's full layer list (upper-first) so a nested
        // launch while WE are running can flatten against it.
        let mut layers = vec![up.to_string_lossy().into_owned()];
        layers.extend(sud_lowers.iter().cloned());
        crate::sud::set_layers(id, layers);
        // /tmp is an inramfs mount (shared-memory store keyed per run;
        // the engine parses the region at sweep and drops the shms).
        // pid+id keying keeps concurrent engines and reruns collision-
        // free; the previous run's shms were unlinked at its sweep.
        let ir_key = format!("sarun{}b{id}", std::process::id());
        b.set_meta("sud_ir_key", &ir_key);
    }
    // Host visibility: a box whose chain is closed — its own --no-parent, or any
    // ancestor marked no_host_fallback (e.g. an OCI image's rootfs base) — sees
    // no host filesystem underneath. Surfaced to the runner so it can pick the
    // right default cwd ("/" when there's no host directory to inherit).
    let no_host = b.no_host_fallback() || chain_has_no_host(&boxes, parent);
    if let Some(prov) = msg.get("prov") {
        b.root_process(prov, host_pid as i64);
    }
    {
        let mut s = lock(state);
        if host_pid > 0 {
            s.box_runpids.insert(id, host_pid);
        }
        // Hold a pidfd on the runner so `kill` can signal it pid-reuse-safely.
        // Prefer the runner's own pidfd (valid across pid namespaces); else open
        // one from the claimed tgid (top-level fallback).
        if let Some(fd) = peer_pidfd {
            s.box_pids.insert(id, fd);
        } else if host_pid > 0 {
            let fd = pidfd_open(host_pid);
            if fd >= 0 {
                s.box_pids.insert(id, fd);
            }
        }
    }
    // --api opt-in: register this box with the oaita proxy so connections
    // from inside it are accepted (and route to its api_log table). Refresh
    // the runner-pid map the proxy uses for peer attribution.
    let want_api = msg
        .get("want_api")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if want_api {
        if let Some(p) = lock(state).api_proxy.clone() {
            p.enable_box(id);
        }
        // Regenerate the safe-for-box oaita.toml from the CURRENT host config.
        // It was first written at engine startup, but `oaita local` (and any
        // config edit) happens AFTER the engine is up — so a startup snapshot
        // had model="" and the box hit "no model set". Refreshing here, as
        // each --api box registers, means the box always reads the live model.
        write_api_box_oaita_toml();
    }
    ov.add_box(std::sync::Arc::new(b));
    // sud: start consuming the live trace stream now that the BoxState is
    // registered (events snapshot process rows / build the write-
    // attribution map / record outputs; raw bytes tee to sud.trace).
    if let Some(fd) = sud_trace_fd.take() {
        if let Some(bx) = ov.live_box(id) {
            use std::os::fd::IntoRawFd;
            crate::sud::stream_events(id, fd.into_raw_fd(), bx, backing.join("sud.trace"));
        }
    }
    // Announce the new box on the subscribe stream so attached UIs
    // rebuild their session list WITHOUT a manual refresh. on_event
    // already handles session_added/removed/renamed identically — it
    // just never fired here because we forgot to broadcast it.
    // session_removed (in delete / kill paths) and session_renamed
    // (in rename) were getting sent; this is the missing third leg.
    broadcast(
        state,
        &json!({
            "type": "session_added",
            "sid": id.to_string(),
            "name": name,
            "parent": parent,
        }),
    );
    let root = crate::paths::mnt_point().join(id.to_string());

    // ── Networking (-n boxes only) ────────────────────────────────────────
    // Tap mode: the RUNNER already created the netns + TAP and handed us its fd
    // (SCM_RIGHTS on this register conn). Build the StackRuntime + flows log
    // around that fd and return dns_ip + the CA bundle CONTENT so the runner can
    // wire bwrap up (it materializes the CA in its own namespace). The engine
    // creates no netns/device, so there is no netns_path.
    let (dns_ip, ca_pem) = prepare_net(state, id, msg, tap_fd).unwrap_or_default();

    // D-oci: if any ancestor in the parent chain has an oci_config meta key
    // (stamped by `sarun oci load` on the top layer of an image), surface
    // env / cwd / cmd / entrypoint / user in the ack so the runner can
    // bwrap with the image's PATH set, in the image's WorkingDir, with the
    // image's User — without which `sarun img -- /bin/sh` would inherit
    // the HOST's PATH (pointing at host bins that don't exist in a closed
    // box) and the HOST's cwd (likely a path outside the image).
    let oci = oci_runtime_from_chain(&boxes, parent);
    let mut reply = json!({
        "ok": true, "mount": root.to_string_lossy(),
        "shm_dir": backing.to_string_lossy(),
        "dns_ip": dns_ip,
        "ca_pem": ca_pem,
        "owner_token": format!("{:032x}", std::process::id() as u128
                               ^ (id as u128) << 64
                               ^ std::time::SystemTime::now()
                                 .duration_since(std::time::UNIX_EPOCH)
                                 .map(|d| d.as_nanos()).unwrap_or(0)),
        "box_id": id, "session_id": id.to_string(), "name": name,
        "capture": want_capture,      // sinks + live echo mux active (off for -t/-d)
        "api": want_api,              // proxy admits this box; inner serves the in-box UDS
        "no_host": no_host,           // chain is closed — no host fs underneath
        "_box_sid": id,               // caller marker: this conn is now the box channel
    });
    if let Some(o) = oci {
        reply["oci"] = o;
    }
    if want_sud {
        reply["sud_upper"] = json!(backing.join("sud-up").to_string_lossy());
        reply["sud_lowers"] = json!(sud_lowers);
        reply["sud_ir_key"] = json!(
            lock(state)
                .overlay
                .clone()
                .and_then(|o| o.live_box(id))
                .and_then(|bx| bx.get_meta("sud_ir_key"))
                .unwrap_or_default()
        );
    }
    reply
}

/// Walk the parent chain looking for an `oci_config` meta entry (stamped by
/// `sarun oci load` on the image's TOP layer). Returns the parsed runtime
/// view {env, cwd, cmd, entrypoint, user} the runner uses, or None when the
/// chain has no OCI ancestor (a non-OCI box). Reads from the discover()
/// snapshot's `Box_.meta` — no per-hop sqlar opens.
fn oci_runtime_from_chain(
    boxes: &std::collections::BTreeMap<i64, discover::Box_>,
    parent: Option<i64>,
) -> Option<Value> {
    let mut cur = parent;
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cur {
        if !seen.insert(id) {
            return None;
        }
        let b = boxes.get(&id)?;
        if let Some(cfg_json) = b.meta.get("oci_config") {
            return parse_oci_runtime(cfg_json);
        }
        cur = b.parent;
    }
    None
}

/// True if any ancestor in the parent chain (starting at `parent`) is marked
/// no_host_fallback — i.e. the chain bottoms out closed, with no host
/// filesystem underneath. Reads `Box_.meta` from the discover() snapshot, the
/// same way oci_runtime_from_chain walks for oci_config. The box's OWN
/// --no-parent is handled by the caller via `b.no_host_fallback()`.
fn chain_has_no_host(
    boxes: &std::collections::BTreeMap<i64, discover::Box_>,
    parent: Option<i64>,
) -> bool {
    let mut cur = parent;
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cur {
        if !seen.insert(id) {
            return false;
        }
        let Some(b) = boxes.get(&id) else {
            return false;
        };
        if b.meta.get("no_host_fallback").map(String::as_str) == Some("1") {
            return true;
        }
        cur = b.parent;
    }
    false
}

/// Pull env / cwd / cmd / entrypoint / user out of the raw OCI image config
/// JSON. We don't link `oci_spec` here on purpose — those fields are stable
/// across the OCI spec versions and a hand-rolled extractor avoids dragging
/// the dep into control.rs just to read five fields.
fn parse_oci_runtime(cfg_json: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(cfg_json).ok()?;
    let inner = v.get("config")?;
    let mut out = serde_json::Map::new();
    if let Some(env) = inner.get("Env") {
        out.insert("env".into(), env.clone());
    }
    if let Some(cwd) = inner.get("WorkingDir") {
        out.insert("cwd".into(), cwd.clone());
    }
    if let Some(cmd) = inner.get("Cmd") {
        out.insert("cmd".into(), cmd.clone());
    }
    if let Some(ep) = inner.get("Entrypoint") {
        out.insert("entrypoint".into(), ep.clone());
    }
    if let Some(u) = inner.get("User") {
        out.insert("user".into(), u.clone());
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

/// Equip a `-n` box's netns and start its smoltcp stack.
/// Stand up the in-engine smoltcp stack for a `tap` box on the TAP fd the
/// RUNNER created and handed us (SCM_RIGHTS on the register conn). The engine
/// creates no netns or device — it only polls the fd. Returns (dns_ip,
/// augmented_ca_bundle_path); empty strings for off/host. `None` on a real
/// failure (caller surfaces it — never a silent dead network).
fn prepare_net(
    state: &State,
    id: i64,
    msg: &Value,
    tap_fd: Option<std::os::fd::OwnedFd>,
) -> Option<(String, String)> {
    let net_mode = msg.get("net_mode").and_then(Value::as_str).unwrap_or("off");
    if net_mode != "tap" {
        drop(tap_fd); // off / host carry no TAP; close any fd the runner sent
        return Some((String::new(), String::new()));
    }
    // Tap REQUIRES the runner's TAP fd. Missing it is a protocol bug, not a
    // reason to silently hand the box a dead network — fail loud.
    let Some(tap_owned) = tap_fd else {
        eprintln!("sarun-engine: box {id} registered net=tap but sent no TAP fd");
        return None;
    };
    let net = match lock(state).net.clone() {
        Some(n) => n,
        None => {
            // tap_owned drops here → fd closed
            eprintln!("sarun-engine: net stack unavailable; -n refused for box {id}");
            return None;
        }
    };
    // Fixed addressing — every box's TAP is identical; we key by box id.
    let box_id_u16 = id as u16;
    let subnet = crate::net::tap::box_subnet();
    let gw_mac = crate::net::tap::gateway_mac();
    let box_dir = crate::paths::state_home().join(format!("flows/box{id}"));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let flows = match crate::net::flows::FlowsLog::create(&box_dir, ts, box_id_u16) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("sarun-engine: flows log: {e}");
            return None;
        }
    };
    let stack = crate::net::stack::StackRuntime::start(
        box_id_u16,
        subnet,
        gw_mac,
        crate::net::tap::BOX_MAC,
        tap_owned,
        flows.clone(),
    );

    // The augmented CA bundle (host bundle + engine CA) — sent as CONTENT; the
    // runner materializes + binds it in its own namespace (works for nested).
    let ca_pem = augmented_ca_bundle(&net.ca).unwrap_or_default();

    // Start the per-box dispatcher: it pulls AcceptedConn off the stack's
    // accept channel and routes each new connection to the right handler
    // (HTTP MITM / HTTPS MITM / L4 forward). The keylog is per-box (file
    // sits next to the box's pcapng) so a tshark with `-o
    // tls.keylog_file:<flows>.keys` decrypts every TLS connection in the
    // pcapng. The upstream rustls config is shared (the real internet's
    // trust roots don't vary by box).
    let keylog = crate::net::mitm::KeyLogFile::new(&flows.keylog_path).ok();
    let upstream_tls = crate::net::mitm::build_upstream_client_config();
    // Proxy hooks (DESIGN-web.md W2/W7): capture + filter, per-box opt-in.
    // The capture sink resolves live_box(id) at record time off the overlay,
    // exactly like the oaita proxy's api_log sink; the filter loads the
    // webfilter ruleset. All-None → the MITM proxy runs its pure pass-through.
    let want_webcap = msg
        .get("want_webcap")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let want_webfilter = msg
        .get("want_webfilter")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let capture = if want_webcap {
        lock(state)
            .overlay
            .clone()
            .map(|ov| crate::net::webcap::WebCapSink::new(ov, id))
    } else {
        None
    };
    let filter = if want_webfilter {
        Some(std::sync::Arc::new(crate::net::filter::Filter::load()))
    } else {
        None
    };
    // Replay (DESIGN-web.md W4.2): `replay_from` names the source box whose
    // captures answer this box's requests, with an optional `replay_asof`.
    let replay = msg
        .get("replay_from")
        .and_then(Value::as_i64)
        .map(|source_box| crate::net::ReplaySource {
            source_box,
            asof: msg.get("replay_asof").and_then(Value::as_f64),
        });
    let hooks = if capture.is_some() || filter.is_some() || replay.is_some() {
        Some(std::sync::Arc::new(crate::net::ProxyHooks {
            capture,
            filter,
            replay,
        }))
    } else {
        None
    };
    if let (Some(rt), Some(keylog)) = (lock(state).net_rt.clone(), keylog) {
        crate::net::dispatch::Dispatcher::start(
            stack.clone(),
            stack.dns.clone(),
            format!("box{id}"),
            net.ca.clone(),
            keylog,
            upstream_tls,
            net.prompts.clone(),
            hooks,
            rt,
        );
    }

    Some((ipv4_str(subnet.gateway_ip()), ca_pem))
}

fn ipv4_str(o: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3])
}

/// The augmented CA bundle CONTENT (host system bundle + the engine's MITM
/// root). Returned to the runner, which materializes it in its OWN namespace
/// and binds it — so it reaches a nested box too (a host path would not). Same
/// for every box (one engine root), so no per-box keying.
fn augmented_ca_bundle(ca: &crate::net::ca::Ca) -> Option<String> {
    // Append our root to whichever system bundle exists; if none does, fall
    // back to "just our root" (a self-contained mini-bundle is still trusted).
    let mut bundle = String::new();
    for p in &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
    ] {
        if let Ok(s) = std::fs::read_to_string(p) {
            bundle = s;
            break;
        }
    }
    if !bundle.ends_with('\n') {
        bundle.push('\n');
    }
    bundle.push_str(&ca.cert_pem);
    Some(bundle)
}

/// Engine-startup hook: pre-write the host-side files the FUSE overlay
/// shadows into every `--api` box. One write per engine lifetime; the
/// content is engine-wide (the MITM CA is engine-wide, the box-side
/// gateway IP is the fixed per-subnet value). Boxes never write here;
/// the path lives under runtime_home alongside `api-box-oaita.toml`.
pub fn write_api_box_net_shadows(net: &crate::net::Net) -> std::io::Result<()> {
    if let Some(bundle) = augmented_ca_bundle(&net.ca) {
        std::fs::write(crate::paths::api_box_ca_pem_path(), bundle)?;
    }
    let gw = ipv4_str(crate::net::tap::box_subnet().gateway_ip());
    std::fs::write(
        crate::paths::api_box_resolv_conf_path(),
        format!("nameserver {gw}\n"),
    )?;
    Ok(())
}

/// Implementation dispatch only. Names, schemas, descriptions, syntax, keys,
/// and menus belong to the Prolog relation. The context idents ride in the
/// list so the emitted function's binders share the bodies' hygiene context.
macro_rules! ui_verbs {
    ($emit:ident) => { $emit! { (state, verb, args, boxes)
        "session_dicts" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::SessionDicts,
            ));
        }
        "display_path" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::DisplayPath { sid },
            ));
        }
        "resolve_box" => {
            let Some(name_or_id) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "missing box name or id"});
            };
            let Ok(name_or_id) = crate::wire::BoundedText::new(name_or_id.to_owned()) else {
                return json!({"ok": false, "error": "box name or id exceeds relation bound"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ResolveBox { name_or_id },
            ));
        }
        "select" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Select { sid },
            ));
        }
        "processes" => {
            let Some(id) = arg_sid(args).and_then(|id| u64::try_from(id).ok()) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Processes { sid: id },
            ));
        }
        "outputs" => {
            let Some(id) = arg_sid(args).and_then(|id| u64::try_from(id).ok()) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Outputs { sid: id },
            ));
        }
        "api_log" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ApiLog { sid }));
        }
        "api_log_detail" => {
            let (Some(sid), Some(row)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid API log row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ApiLogDetail { sid, row }));
        }
        // Web capture (DESIGN-web.md W4): summary rows + full detail, mirroring
        // api_log. Feeds the UI's Captures pane and the oaita web tools.
        "webcap" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Webcap { sid }));
        }
        "webcap_detail" => {
            let (Some(sid), Some(row)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid web capture row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::WebcapDetail { sid, row }));
        }
        // Raw (base64) response body of one capture — for the image viewer,
        // which needs lossless binary the lossy-UTF8 detail can't carry.
        "webcap_body" => {
            let (Some(sid), Some(row)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid web capture row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::WebcapBody { sid, row }));
        }
        "brushprov" => {
            let Some(id) = arg_sid(args).and_then(|id| u64::try_from(id).ok()) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Brushprov { sid: id },
            ));
        }
        // Phase 1 embedded-ninja: the parsed build-graph edges (outs/ins/cmd),
        // including up-to-date targets that never executed.
        "build_edges" => {
            let Some(id) = arg_sid(args).and_then(|id| u64::try_from(id).ok()) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::BuildEdges { sid: id },
            ));
        }
        // D9 brush↔process linkage joins (both directions).
        "proc_pipeline" => {
            let (Some(sid), Some(row)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid process row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ProcPipeline { sid, row }));
        }
        "output_pipeline" => {
            let (Some(sid), Some(output)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid output row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OutputPipeline { sid, output }));
        }
        "pipeline_procs" => {
            let (Some(sid), Some(pipeline)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid pipeline row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::PipelineProcs { sid, pipeline }));
        }
        "output_detail" => {
            let (Some(sid), Some(output)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid output row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OutputDetail { sid, output }));
        }
        "processes_live" => {
            // For a box whose runner is still registered the engine returns
            // the captured process set as the "live" snapshot; the UI uses
            // null vs non-null to choose live-style vs finished-style
            // rendering. Without a separate exit-tracking pass the set
            // includes already-exited rows too — but the prototype's
            // strict-active behavior would need engine-level exit
            // detection (a separate ticket).
            let Some(id) = arg_sid(args).and_then(|id| u64::try_from(id).ok()) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ProcessesLive { sid: id },
            ));
        }
        "proc_info" => {
            let (Some(sid), Some(row)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid process row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ProcInfo { sid, row }));
        }
        "proc_prov" => {
            let (Some(sid), Some(row)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid process row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ProcProv { sid, row }));
        }
        "proc_roots" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ProcRoots { sid }));
        }
        "process_env" => {
            let (Some(sid), Some(row)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid process row"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ProcessEnv { sid, row }));
        }
        "writer_id" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid writer path"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::WriterId { sid, rel }));
        }
        "first_writer_id" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid writer path"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::FirstWriterId { sid, rel }));
        }
        "first_writer_prov" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"ok": false, "error": "missing or invalid writer path"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::FirstWriterProv { sid, rel }));
        }
        "stuck" => { match arg_sid(args) {
            // A wedged box is invisible from the outside — this answers
            // WHERE it is stuck without strace. Descend to THREADS, not
            // just processes: the engine's own workers (a wedged in-box
            // build is often a blocked tokio worker or coreutil thread,
            // e.g. sarun-uu_sort in unix_stream_data_wait) live as tasks
            // inside one pid, and the tgid leader alone shows "running"
            // for the idle main thread — useless. For each process whose
            // ancestry reaches the box's runner we walk /proc/<pid>/task/
            // and report every thread's comm / state / kernel wchan /
            // current syscall. Read straight from /proc; no box help.
            Some(id) => {
                let rp = lock(state).box_runpids.get(&id).copied();
                match rp {
                    None => json!({"ok": false, "error": "box not running"}),
                    Some(rp) => {
                        // stat "pid (comm) STATE …" — state char follows the
                        // LAST ')' (comm may contain parens).
                        let state_of = |path: &str| std::fs::read_to_string(path)
                            .ok()
                            .and_then(|s| s.rfind(')')
                                .and_then(|i| s[i + 1..].trim().chars().next())
                                .map(|c| c.to_string()))
                            .unwrap_or_default();
                        // The box's process set (ancestry reaches the runner).
                        let mut box_pids: Vec<i32> = vec![];
                        if let Ok(rd) = std::fs::read_dir("/proc") {
                            for ent in rd.flatten() {
                                let Some(pid) = ent.file_name().to_str()
                                    .and_then(|s| s.parse::<i32>().ok())
                                else { continue };
                                let mut cur = pid;
                                for _ in 0..64 {
                                    if cur == rp { box_pids.push(pid); break; }
                                    let pp = ppid_of(cur);
                                    if pp <= 1 { break; }
                                    cur = pp;
                                }
                            }
                        }
                        // Per-pid fd → target (readlink), plus a box-wide
                        // pipe/socket-inode → holders map so a thread blocked
                        // on a pipe/socket names WHO is on the other end —
                        // turning "unix_stream_data_wait" into an actual
                        // deadlock topology. Linux threads share the fd
                        // table, so fds are keyed per pid, not per tid.
                        let mut fd_tab: std::collections::HashMap<i32,
                            std::collections::HashMap<i32, String>> =
                            Default::default();
                        let mut holders: std::collections::HashMap<String,
                            Vec<i32>> = Default::default();
                        for &pid in &box_pids {
                            let mut t = std::collections::HashMap::new();
                            if let Ok(fds) = std::fs::read_dir(
                                format!("/proc/{pid}/fd")) {
                                for fe in fds.flatten() {
                                    let Some(n) = fe.file_name().to_str()
                                        .and_then(|s| s.parse::<i32>().ok())
                                    else { continue };
                                    if let Ok(tgt) = std::fs::read_link(fe.path()) {
                                        let tgt = tgt.to_string_lossy()
                                            .into_owned();
                                        if tgt.starts_with("pipe:")
                                            || tgt.starts_with("socket:") {
                                            holders.entry(tgt.clone())
                                                .or_default().push(pid);
                                        }
                                        t.insert(n, tgt);
                                    }
                                }
                            }
                            fd_tab.insert(pid, t);
                        }
                        // Symbolized backtraces, two sources merged. Under sud
                        // external gdb yields `?? ()` for the relocated engine,
                        // so the on-CPU spins — the ones /proc wchan/syscall
                        // can't localize — come from in-process self-unwind
                        // (each R thread dumps its own std::backtrace). gdb
                        // still covers the non-sud path and blocked threads.
                        let mut bt_map = thread_backtraces(&box_pids, 8);
                        // Drop gdb's unsymbolizable `?? ()` frames so the diag
                        // stops emitting misleading garbage.
                        for v in bt_map.values_mut() {
                            v.retain(|f| !f.starts_with("?? "));
                        }
                        bt_map.retain(|_, v| !v.is_empty());
                        // Self-unwind wins for any thread it localized.
                        for (tid, frames) in selfbt_backtraces(rp, &box_pids) {
                            if !frames.is_empty() { bt_map.insert(tid, frames); }
                        }
                        // Ptrace path wins over both: it works for ANY thread
                        // state and the sud-wrapper/foreign frames the in-box
                        // signal handler can't reach (sud masks signals in its
                        // dispatcher). This is what localizes a syscall-heavy
                        // spin — the common real wedge.
                        for (tid, frames) in ptrace_backtraces(&box_pids) {
                            if !frames.is_empty() { bt_map.insert(tid, frames); }
                        }
                        let mut threads = vec![];
                        for &pid in &box_pids {
                            let Ok(tasks) = std::fs::read_dir(
                                format!("/proc/{pid}/task")) else { continue };
                            for te in tasks.flatten() {
                                let Some(tid) = te.file_name().to_str()
                                    .and_then(|s| s.parse::<i32>().ok())
                                else { continue };
                                let base = format!("/proc/{pid}/task/{tid}");
                                let rd1 = |f: &str| std::fs::read_to_string(
                                    format!("{base}/{f}"))
                                    .unwrap_or_default().trim().to_string();
                                let comm = rd1("comm");
                                let wchan = rd1("wchan");
                                let state = state_of(&format!("{base}/stat"));
                                // /proc/.../syscall: "nr arg0 arg1 … sp pc"
                                // (nr decimal, args hex), or "running"/"-1".
                                let raw = rd1("syscall");
                                let toks: Vec<&str> = raw.split_whitespace()
                                    .collect();
                                let nr = toks.first().and_then(|t|
                                    t.parse::<i64>().ok());
                                // Decode the syscall + resolve its fd arg to
                                // the pipe/socket/file it names, then name the
                                // peer holding the other end. This is the join
                                // that makes wchan actionable.
                                let detail = match nr {
                                    Some(nr) if nr >= 0 => {
                                        let name = syscall_name(nr);
                                        let name = if name.is_empty() {
                                            format!("sys{nr}")
                                        } else { name.to_string() };
                                        if syscall_arg0_is_fd(nr) {
                                            let fd = toks.get(1)
                                                .and_then(|h| i64::from_str_radix(
                                                    h.trim_start_matches("0x"),
                                                    16).ok());
                                            match fd {
                                                Some(fd) => {
                                                    let tgt = fd_tab.get(&pid)
                                                        .and_then(|t| t.get(
                                                            &(fd as i32)))
                                                        .cloned()
                                                        .unwrap_or_else(||
                                                            "?".into());
                                                    let mut d = format!(
                                                        "{name}(fd {fd} → {tgt})");
                                                    if let Some(h) = holders
                                                        .get(&tgt) {
                                                        let peers: Vec<String> =
                                                            h.iter()
                                                             .filter(|p| **p != pid)
                                                             .map(|p| p.to_string())
                                                             .collect();
                                                        if !peers.is_empty() {
                                                            d.push_str(
                                                                &format!("  peer pid {}",
                                                                    peers.join(",")));
                                                        }
                                                    }
                                                    d
                                                }
                                                None => format!("{name}()"),
                                            }
                                        } else { format!("{name}()") }
                                    }
                                    // "running"/-1: on-CPU, no syscall — fall
                                    // back to the kernel wait channel (which is
                                    // "0"/empty when truly on-CPU).
                                    _ => if wchan.is_empty() || wchan == "0" {
                                        "running".into()
                                    } else { wchan.clone() },
                                };
                                let bt = bt_map.get(&tid)
                                    .map(|v| v.iter().take(6).cloned()
                                        .collect::<Vec<_>>())
                                    .unwrap_or_default();
                                threads.push(json!({
                                    "pid": pid, "tid": tid, "comm": comm,
                                    "state": state, "wchan": wchan,
                                    "syscall": toks.first().copied()
                                        .unwrap_or("").to_string(),
                                    "detail": detail, "bt": bt,
                                }));
                            }
                        }
                        // Blocked threads first (a wedge is a thread NOT
                        // running), then by pid/tid — so the smoking gun is
                        // at the top instead of buried under idle workers.
                        threads.sort_by_key(|t| {
                            let st = t.get("state").and_then(Value::as_str)
                                .unwrap_or("");
                            let running = matches!(st, "R");
                            (running,
                             t.get("pid").and_then(Value::as_i64).unwrap_or(0),
                             t.get("tid").and_then(Value::as_i64).unwrap_or(0))
                        });
                        json!({"ok": true, "runner": rp, "procs": threads})
                    }
                }
            }
            None => json!({"ok": false, "error": "no slopbox"}),
        }
        }
        "kill" => { match arg_sid(args) {
            Some(id) => {
                let fd = lock(state).box_pids.get(&id).copied();
                match fd {
                    Some(fd) => { pidfd_signal(fd, libc::SIGTERM); json!({"ok": true}) }
                    None => json!({"ok": false, "error": "box not running"}),
                }
            }
            None => json!({"ok": false, "error": "no slopbox"}),
        }
        }
        "dissolve" => { match arg_sid(args) {
            Some(id) => dissolve(state, id),
            None => json!({"ok": false, "error": "no slopbox"}),
        }
        }
        "apply_to_copy" => {
            match arg_sid(args) {
                Some(id) => apply_to_copy(state, boxes, id),
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        // RO attachments (DEPOT-DESIGN.md §8): reference another box's
        // layer read-only, between this box and its parent in the lookup
        // chain. args: [sid, row, ...] — each row an integer box id, or
        // an object {kind,store,ref,rev,prefix?,name} referencing a
        // mirror store directly (served through the readout, no import —
        // the generic attachment row the kind-specific verbs converge
        // on). The full ordered list replaces the current one (empty
        // list = detach all). A box row is hydrated from its at-rest
        // sqlar if not already live; an ext row opens lazily at first
        // use.
        "ro_attach" => {
            let ov = lock(state).overlay.clone();
            let (Some(ov), Some(sid)) = (ov, arg_sid(args)) else {
                return json!({"ok": false, "error": "no overlay / bad sid"});
            };
            // Hydrate an at-rest owner (same as the kind-specific attach
            // verbs): the list persists in meta, so attaching between
            // runs must work.
            let Some(b) = hydrate_box(&ov, sid) else {
                return json!({"ok": false, "error": "no such box"});
            };
            let mut ids = Vec::new();
            for v in args.iter().skip(1) {
                let Some(ro) = v.as_i64() else {
                    // Object row: an external reference. Parse strictly —
                    // a malformed row must fail the verb, not skip.
                    match serde_json::from_value::<crate::capture::ExtRef>(
                        v.clone())
                    {
                        Ok(e) => {
                            ids.push(crate::capture::RoAttachment::Ext(e));
                            continue;
                        }
                        Err(e) => return json!({"ok": false,
                            "error": format!("bad attachment row: {e}")}),
                    }
                };
                if ro == sid {
                    return json!({"ok": false, "error": "cannot attach self"});
                }
                if ov.box_of(ro).is_none() {
                    // Hydrate the at-rest box: open its sqlar, load the
                    // mirror, register it (never run — reference only).
                    let db = crate::paths::state_home()
                        .join(format!("{ro}.sqlar"));
                    if !db.exists() {
                        return json!({"ok": false,
                                      "error": format!("no box {ro}")});
                    }
                    match crate::capture::BoxState::create(ro) {
                        Ok(rb) => {
                            rb.load_mirror();
                            ov.add_box(std::sync::Arc::new(rb));
                        }
                        Err(e) => return json!({"ok": false,
                                                "error": e.to_string()}),
                    }
                }
                ids.push(crate::capture::RoAttachment::Box(ro));
            }
            b.set_ro_attachments(ids);
            ov.invalidate_ext(sid);
            json!({"ok": true})
        }
        // Check a commit out of a gitdepot mirror store INTO the box's
        // changes: resolve ref→sha from meta.sqlite, then STREAM the
        // commit's tree (or the subtree under SUBPATH) out of the store —
        // the union at that revision is folded once at the byte level
        // (geometric delta compose + one overlay) and walked as bytes, so
        // memory stays bounded; each entry lands as an ordinary change row
        // + pool blob, exactly as if the box had written it. args:
        // [sid, store_path, refname, dest?, subpath?] — dest prefixes the
        // written paths (checkout into a subdirectory of the box).
        "git_checkout" => {
            use crate::depot::BoxDepot as _;
            let ov = lock(state).overlay.clone();
            let (Some(ov), Some(sid)) = (ov, arg_sid(args)) else {
                return json!({"ok": false, "error": "no overlay / bad sid"});
            };
            let Some(b) = hydrate_box(&ov, sid) else {
                return json!({"ok": false, "error": "no such box"});
            };
            let (Some(store), Some(refname)) = (
                args.get(1).and_then(Value::as_str),
                args.get(2).and_then(Value::as_str),
            ) else {
                return json!({"ok": false, "error": "need store path + ref"});
            };
            let dest = args.get(3).and_then(Value::as_str).unwrap_or("")
                .trim_matches('/').to_string();
            let subpath = args.get(4).and_then(Value::as_str).unwrap_or("")
                .trim_matches('/').to_string();
            let store_path = std::path::Path::new(store);
            // REF may be a ref name ("main" matches "refs/heads/main") or a
            // unique commit-sha prefix — ANY commit in the chain, not just
            // the tips (gitdepot::resolve_ref owns the semantics).
            // A commit checks out its own tree (revision resolved by SHA —
            // tag-tree revisions interleave with commits, so the COMMITS
            // index is not the revision); a tag-at-tree ref pins the TAG
            // object's sha and checks out the tagged tree's revision.
            let resolved = match gitdepot::resolve_ref(store_path, refname) {
                Ok(Some(r)) => r,
                Ok(None) => return json!({"ok": false, "error":
                    format!("no ref or commit {refname} in store")}),
                Err(gitdepot::Error::Meta(msg)) =>
                    return json!({"ok": false, "error": msg}),
                Err(e) => return json!({"ok": false,
                                        "error": format!("store: {e}")}),
            };
            let ls = match gitdepot::store::Store::open(store_path)
                .and_then(|st| st.union())
            {
                Ok(ls) => ls,
                Err(e) => return json!({"ok": false,
                                        "error": format!("store: {e}")}),
            };
            let (sha, rev) = match resolved {
                gitdepot::Resolved::Commit { sha, .. } => match ls.rev_of(&sha) {
                    Some(rev) => (sha, rev),
                    None => return json!({"ok": false, "error":
                        format!("commit {sha} not in the union store")}),
                },
                gitdepot::Resolved::TreeTag { tag_sha, tree_idx } =>
                    (tag_sha, tree_idx),
            };
            let mut files = 0u64;
            let mut bytes = 0u64;
            // Ancestor DIRECTORY rows: the mount traverses real dir rows,
            // so each intermediate directory lands once, like mkdir -p.
            let mut dirs_done = std::collections::HashSet::new();
            let ensure_dirs = |b: &crate::capture::BoxState, path: &str,
                                   dirs_done: &mut std::collections::HashSet<String>| {
                let mut at = 0usize;
                while let Some(i) = path[at..].find('/') {
                    let d = &path[..at + i];
                    if dirs_done.insert(d.to_string()) {
                        b.set_dir(d, 0o040755, 0);
                    }
                    at += i + 1;
                }
            };
            let res = ls.checkout_entries_at(rev, subpath.as_bytes(), &mut |rel, mode, content| {
                let rel_s = String::from_utf8_lossy(rel);
                let path = if dest.is_empty() {
                    rel_s.into_owned()
                } else {
                    format!("{dest}/{rel_s}")
                };
                ensure_dirs(&b, &path, &mut dirs_done);
                use gitdepot::layer::Mode;
                match mode {
                    Mode::Symlink => {
                        let target = String::from_utf8_lossy(content).into_owned();
                        b.set_symlink(&path, std::path::Path::new(&target), 0);
                    }
                    Mode::Gitlink => {} // a submodule pointer has no content here
                    m => {
                        let full_mode = match m {
                            Mode::File => 0o100644,
                            Mode::Exec => 0o100755,
                            Mode::Other(x) => x,
                            _ => 0o100644,
                        };
                        let rid = b.ensure_file_row(&path, full_mode, 0);
                        let bp = crate::depot::blob_path(sid, rid);
                        if let Some(dir) = bp.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        std::fs::write(&bp, content).map_err(|e| gitdepot::Error::Chain(
                            format!("write {}: {e}", bp.display())))?;
                        b.finalize_file(&path, content.len() as i64, 0, 0);
                        files += 1;
                        bytes += content.len() as u64;
                    }
                }
                Ok(())
            });
            if let Err(e) = res {
                return json!({"ok": false, "error": format!("checkout: {e}")});
            }
            b.load_mirror();
            json!({"ok": true, "sha": sha, "files": files, "bytes": bytes})
        }
        // Attach a wikipedia mirror page (wikimak instance root) as an
        // external RO reference: args [sid, root, page_id, prefix?].
        // Bookkeeping only — title/id resolution here, and the head
        // rev read here is the PIN: the readout serves exactly that
        // revision as <title>.txt under prefix at first read
        // (attach.rs), even after later imports move the head.
        "wiki_attach" => {
            let ov = lock(state).overlay.clone();
            let (Some(ov), Some(sid)) = (ov, arg_sid(args)) else {
                return json!({"ok": false, "error": "no overlay / bad sid"});
            };
            let Some(b) = hydrate_box(&ov, sid) else {
                return json!({"ok": false, "error": "no such box"});
            };
            let Some(root) = args.get(1).and_then(Value::as_str) else {
                return json!({"ok": false, "error": "need root + page"});
            };
            let prefix = args.get(3).and_then(Value::as_str).unwrap_or("");
            // Read-side open (shared flock, dropped at scope end): the
            // pin must be attachable while another attachment is
            // hydrated — and never block an import running elsewhere.
            let inst = match open_wiki_instance(root) {
                Ok(i) => i,
                Err(e) => return json!({"ok": false, "error": e}),
            };
            // PAGE is a numeric id or a title (exact, else unique
            // case-insensitive substring) — titles are the UI, ids are
            // the plumbing.
            let (page, title) = match args.get(2) {
                Some(Value::Number(n)) if n.as_u64().is_some() => {
                    (n.as_u64().unwrap(), None)
                }
                Some(Value::String(s)) => match s.parse::<u64>() {
                    Ok(n) => (n, None),
                    Err(_) => match inst.page_by_title(s) {
                        Ok((Some(id), hits)) => {
                            let t = hits.into_iter()
                                .find(|(i, _)| *i == id).map(|(_, t)| t);
                            (id, t)
                        }
                        Ok((None, hits)) if hits.is_empty() =>
                            return json!({"ok": false,
                                "error": format!("no page titled {s:?}")}),
                        Ok((None, hits)) => {
                            let cands: Vec<String> = hits.into_iter()
                                .map(|(i, t)| format!("{t} ({i})")).collect();
                            return json!({"ok": false, "error": format!(
                                "title {s:?} is ambiguous: {}",
                                cands.join(", "))});
                        }
                        Err(e) => return json!({"ok": false,
                                                "error": e.to_string()}),
                    },
                },
                _ => return json!({"ok": false, "error": "need root + page"}),
            };
            let title = match title {
                Some(t) => t,
                // Attached by id: recover the title for the name via
                // the indexed dictionary hop (open interval →
                // title_id_to_page) — never a pool-wide listing.
                None => inst.page_current_title(page).ok().flatten()
                    .unwrap_or_else(|| format!("page-{page}")),
            };
            // page_head decodes ONE frame for the rev PIN — never a
            // full history walk; the pinned decode happens readout-side
            // at first read.
            let head = match inst.page_head(page) {
                Ok(Some(h)) => h,
                Ok(None) => return json!({"ok": false,
                                          "error": format!("no page {page}")}),
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            };
            // WHICH wiki: the instance root's directory name.
            let wiki = std::path::Path::new(root).file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "wiki".into());
            // attach.rs recovers the title from this exact shape
            // (strip "wiki:", drop the wiki label at the first '/',
            // rsplit the '@rN' pin) — keep the three in lockstep.
            let name = format!("wiki:{wiki}/{title}@r{}", head.rev_id);
            let mut rows = b.ro_attachment_list();
            rows.push(crate::capture::RoAttachment::Ext(
                crate::capture::ExtRef {
                    kind: "wiki".into(), store: root.to_string(),
                    refname: page.to_string(),
                    rev: head.rev_id.to_string(),
                    prefix: prefix.to_string(), name: name.clone(),
                }));
            b.set_ro_attachments(rows);
            ov.invalidate_ext(sid);
            json!({"ok": true, "name": name, "page": page,
                   "title": title, "rev": head.rev_id})
        }
        // Attach an IETF draft (ietf-mirror root) as an external RO
        // reference: args [sid, root, draft, prefix?]. Bookkeeping only
        // — the head rev read here is the PIN, and the readout serves
        // exactly that revision as <draft>-<rev>.txt under prefix at
        // first read (attach.rs), even after later updates move the
        // head.
        "ietf_attach" => {
            let ov = lock(state).overlay.clone();
            let (Some(ov), Some(sid)) = (ov, arg_sid(args)) else {
                return json!({"ok": false, "error": "no overlay / bad sid"});
            };
            let Some(b) = hydrate_box(&ov, sid) else {
                return json!({"ok": false, "error": "no such box"});
            };
            let (Some(root), Some(draft)) = (
                args.get(1).and_then(Value::as_str),
                args.get(2).and_then(Value::as_str),
            ) else {
                return json!({"ok": false, "error": "need root + draft"});
            };
            let prefix = args.get(3).and_then(Value::as_str).unwrap_or("");
            // Read-side open (shared flock, dropped at scope end): the
            // pin must be attachable while an update runs elsewhere.
            let m = match ietf_mirror::Mirror::open_read(
                ietf_mirror::MirrorConfig::new(root.into())) {
                Ok(m) => m,
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            };
            // head() decodes ONE layer for the rev pin — never a full
            // history walk; the pinned decode happens readout-side at
            // first read.
            let head_rev = match m.head(draft) {
                Ok(Some(h)) => h.rev,
                Ok(None) => return json!({"ok": false,
                                          "error": format!("no draft {draft}")}),
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            };
            let name = format!("ietf:{draft}@{head_rev}");
            let mut rows = b.ro_attachment_list();
            rows.push(crate::capture::RoAttachment::Ext(
                crate::capture::ExtRef {
                    kind: "ietf".into(), store: root.to_string(),
                    refname: draft.to_string(), rev: head_rev.clone(),
                    prefix: prefix.to_string(), name: name.clone(),
                }));
            b.set_ro_attachments(rows);
            ov.invalidate_ext(sid);
            json!({"ok": true, "name": name, "rev": head_rev})
        }
        // Mirror-update jobs (mirrors.rs): the schedule surface.
        "mirror_jobs" => { match crate::mirrors::jobs_list() {
            Ok(jobs) => serde_json::to_value(jobs).unwrap_or(Value::Null),
            Err(e) => return json!({"ok": false, "error": e}),
        }
        }
        // args: [kind, src, dest, interval_secs]
        "mirror_add" => {
            let (Some(kind), Some(src), Some(dest)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
                args.get(2).and_then(Value::as_str),
            ) else {
                return json!({"ok": false, "error": "need kind, src, dest"});
            };
            let interval = args.get(3).and_then(Value::as_i64).unwrap_or(24 * 3600);
            match crate::mirrors::job_add(kind, src, dest, interval) {
                Ok(id) => json!({"ok": true, "id": id}),
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        // args: [id] — force-run one job now (paused included).
        "mirror_run" => { match args.first().and_then(Value::as_i64) {
            Some(id) => match crate::mirrors::job_run(id) {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            None => return json!({"ok": false, "error": "need job id"}),
        }
        }
        // Start every due/stopped unpaused job.
        "mirror_run_pending" => { match crate::mirrors::run_pending() {
            Ok(ids) => json!({"ok": true, "started": ids}),
            Err(e) => json!({"ok": false, "error": e}),
        }
        }
        // args: [id, paused(bool)]
        "mirror_pause" => {
            let (Some(id), Some(p)) = (
                args.first().and_then(Value::as_i64),
                args.get(1).and_then(Value::as_bool),
            ) else {
                return json!({"ok": false, "error": "need id + bool"});
            };
            match crate::mirrors::job_set_paused(id, p) {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        // args: [id]
        "mirror_rm" => { match args.first().and_then(Value::as_i64) {
            Some(id) => match crate::mirrors::job_remove(id) {
                Ok(note) => json!({"ok": true, "note": note}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            None => return json!({"ok": false, "error": "need job id"}),
        }
        }
        // Rotation (DEPOT-DESIGN.md §6): promote child box over its
        // parent. args: [child_sid]. Encodings are rewritten — no
        // layer's occlusion changes: the child box ends up holding the
        // old stack's total occlusion (compose) and BECOMES the parent;
        // the old parent box holds the inverse (replicas + holes) and
        // becomes the child. Purely syntactic — no view, no host I/O;
        // ancestors are consulted only as recorded data.
        "rotate" => {
            let ov = lock(state).overlay.clone();
            let (Some(ov), Some(child_id)) = (ov, arg_sid(args)) else {
                return json!({"ok": false, "error": "no overlay / bad sid"});
            };
            let runpids = lock(state).box_runpids.clone();
            // Hydrate the at-rest pair (add_box pulls the parent chain in).
            if ov.box_of(child_id).is_none() {
                if !crate::paths::state_home()
                    .join(format!("{child_id}.sqlar")).exists()
                {
                    return json!({"ok": false, "error": "no such box"});
                }
                match crate::capture::BoxState::create(child_id) {
                    Ok(cb) => {
                        cb.load_mirror();
                        ov.add_box(std::sync::Arc::new(cb));
                    }
                    Err(e) => return json!({"ok": false, "error": e.to_string()}),
                }
            }
            let Some(child) = ov.box_of(child_id) else {
                return json!({"ok": false, "error": "no such box"});
            };
            let Some(parent_id) = child.parent() else {
                return json!({"ok": false, "error": "box has no parent"});
            };
            let Some(parent) = ov.box_of(parent_id) else {
                return json!({"ok": false, "error": "parent not hydrated"});
            };
            if runpids.contains_key(&child_id) || runpids.contains_key(&parent_id) {
                return json!({"ok": false,
                              "error": "rotate needs both boxes at rest"});
            }
            // Export the recorded chain: the parent's OWN ancestors
            // (lower-first), then a = parent's layer, b = child's layer.
            let mut anc_layers: Vec<depot_model::Layer> = Vec::new();
            {
                let mut cur = parent.parent();
                let mut hops = 0;
                while let Some(id) = cur {
                    hops += 1;
                    if hops > 64 { break; }
                    let Some(bx) = ov.box_of(id) else { break };
                    let conn = bx.conn.lock().unwrap();
                    match crate::depot::export_layer(&conn, id) {
                        Ok(l) => anc_layers.push(l),
                        Err(e) => return json!({"ok": false, "error": e}),
                    }
                    drop(conn);
                    cur = bx.parent();
                }
                anc_layers.reverse(); // walked upward; rotate wants lower-first
            }
            let a = {
                let conn = parent.conn.lock().unwrap();
                match crate::depot::export_layer(&conn, parent_id) {
                    Ok(l) => l,
                    Err(e) => return json!({"ok": false, "error": e}),
                }
            };
            let b = {
                let conn = child.conn.lock().unwrap();
                match crate::depot::export_layer(&conn, child_id) {
                    Ok(l) => l,
                    Err(e) => return json!({"ok": false, "error": e}),
                }
            };
            let anc_refs: Vec<&depot_model::Layer> = anc_layers.iter().collect();
            let (b_new, a_new) = depot_model::rotate(&anc_refs, &a, &b);
            // Write back: the CHILD box carries B' and becomes the
            // parent; the old parent carries A' and becomes its child.
            {
                let conn = child.conn.lock().unwrap();
                if let Err(e) = crate::depot::archive_clear(&conn, child_id)
                    .and_then(|_| crate::depot::import_layer(&conn, child_id, &b_new))
                {
                    return json!({"ok": false, "error": e});
                }
            }
            {
                let conn = parent.conn.lock().unwrap();
                if let Err(e) = crate::depot::archive_clear(&conn, parent_id)
                    .and_then(|_| crate::depot::import_layer(&conn, parent_id, &a_new))
                {
                    return json!({"ok": false,
                                  "error": format!("parent import: {e}                                                     (child already rewritten)")});
                }
            }
            // Flip the encoding parenthood (bookkeeping): child takes the
            // old parent's parent; the old parent hangs off the child.
            let grand = parent.parent();
            child.set_parent(grand);
            child.set_meta("parent_box_id",
                           &grand.map(|g| g.to_string()).unwrap_or_default());
            parent.set_parent(Some(child_id));
            parent.set_meta("parent_box_id", &child_id.to_string());
            // Refresh the in-RAM mirrors.
            child.load_mirror();
            parent.load_mirror();
            json!({"ok": true, "parent": child_id, "child": parent_id})
        }
        "reload_rules" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReloadRules,
            ));
        }
        "delete" => { match arg_sid(args) {
            // Free the box, KEEPING any boxes stacked on it: children have this
            // box's view copied down into them and are re-parented onto the
            // grandparent, so their merged view is unchanged. Same operation as
            // dissolve — the box's OWN writes never reach the parent or host.
            // A raw reap here would orphan the children — so delete never does
            // one when children exist, and never destroys a box not named.
            Some(id) => free_box(state, id),
            None => json!({"ok": false}),
        }
        }
        "review.session_changes" => { match arg_sid(args) {
            Some(id) => crate::review::session_changes(id),
            None => json!([]),
        }
        }
        "review.hunks" => {
            match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
                (Some(id), Some(rel)) => crate::review::hunks(id, rel),
                _ => json!({"is_text": false, "hunks": [],
                            "diff": {"kind": "error", "error": "bad args"}}),
            }
        }
        "review.file_bytes" => {
            match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
                (Some(id), Some(rel)) => crate::review::file_bytes(id, rel),
                _ => json!({"ok": false, "error": "bad args"}),
            }
        }
        "review.write_file" => {
            let ov = lock(state).overlay.clone();
            match (ov, arg_sid(args), args.get(1).and_then(Value::as_str),
                   args.get(2).and_then(Value::as_str)) {
                (Some(ov), Some(id), Some(rel), Some(b64)) => {
                    match base64::engine::general_purpose::STANDARD.decode(b64) {
                        Ok(bytes) => crate::review::write_file(id, rel, &bytes, &ov),
                        Err(e) => json!({"ok": false,
                                         "error": format!("bad base64: {e}")}),
                    }
                }
                (None, ..) => json!({"ok": false, "error": "overlay not available"}),
                _ => json!({"ok": false, "error": "bad args"}),
            }
        }
        "review.apply" => { match arg_sid(args) {
            // Audit H3: refuse a still-running box — its captured blobs may be
            // mid-write, so applying could stamp a torn blob onto the host.
            Some(id) if box_is_running(state, id) => json!({"applied": [],
                "errors": [{"path": "", "error": "box is running; stop it first"}]}),
            Some(id) => {
                let ctx = crate::review::NestCtx::new(
                    lock(state).overlay.clone());
                let r = crate::review::apply(id,
                    args.get(1).unwrap_or(&Value::Null), &ctx);
                drop_if_empty(state, id); r }
            None => json!({"applied": [], "errors": []}),
        }
        }
        "review.discard" => { match arg_sid(args) {
            // Audit H3: same running-box guard as apply (discard reads the same
            // blobs to copy them DOWN into children before dropping the row).
            Some(id) if box_is_running(state, id) => json!({"discarded": [],
                "errors": [{"path": "", "error": "box is running; stop it first"}]}),
            Some(id) => {
                let ctx = crate::review::NestCtx::new(
                    lock(state).overlay.clone());
                let r = crate::review::discard(id,
                    args.get(1).unwrap_or(&Value::Null), &ctx);
                drop_if_empty(state, id); r }
            None => json!({"discarded": [], "errors": []}),
        }
        }
        "review.file_groups" => {
            match arg_sid(args) {
                Some(id) => {
                    let paths = crate::review::changed_paths(id);
                    let groups: Vec<Value> = crate::overlay::file_groups().iter().map(|g| {
                        let matched: Vec<&String> =
                            paths.iter().filter(|p| g.matches(p)).collect();
                        json!({"name": g.name, "count": matched.len(), "paths": matched})
                    }).collect();
                    json!({"ok": true, "groups": groups})
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "review.patch_text" => { match arg_sid(args) {
            Some(id) => {
                let data = crate::review::patch_text(id);
                json!({"__b": base64::engine::general_purpose::STANDARD.encode(&data)})
            }
            None => json!({"__b": ""}),
        }
        }
        "review.change_mode" => { match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
            (Some(id), Some(rel)) => match crate::review::current_mode(id, rel) {
                Some(m) => json!(m), None => Value::Null,
            },
            _ => Value::Null,
        }
        }
        "review.decorate" => { match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
            (Some(id), Some(rel)) => crate::review::decorate(id, rel),
            _ => json!({"is_text": false, "stale": false, "kind": "changed"}),
        }
        }
        // Newest-first slice of the box's change set, for the boxes view's
        // "recently changed" panel on a live box. limit defaults to 200.
        "review.recent_changes" => {
            let id = arg_sid(args);
            let limit = args.get(1).and_then(Value::as_i64).unwrap_or(200);
            match id {
                Some(id) => crate::review::recent_changes(id, limit),
                None => Value::Array(vec![]),
            }
        }
        // Five-list bundle for the Sessions-view right pane: newest-first
        // outputs / changes / processes / pipelines / build-edges in one
        // round-trip, capped at `limit` per kind (default 20). Changes
        // includes xattr modifications inline as kind="xattr" rows.
        "review.box_summary" => {
            let id = arg_sid(args);
            let limit = args.get(1).and_then(Value::as_i64).unwrap_or(20);
            match id {
                Some(id) => {
                    let mut v = crate::review::box_summary(id, limit);
                    // Live in-flight builtin activity (recipes / $(shell) /
                    // parse) from the box's watchdog feed — engine memory,
                    // not the DB; a hung box shows WHAT it's chewing on.
                    let ov = lock(state).overlay.clone();
                    if let Some(b) = ov.as_ref().and_then(|ov| ov.live_box(id)) {
                        let items: Vec<Value> = b.activity.lock().unwrap()
                            .iter()
                            .map(|(d, age)| json!({"desc": d, "age": age}))
                            .collect();
                        if !items.is_empty() {
                            v["activity"] = json!(items);
                        }
                    }
                    v
                }
                None => json!({"outputs":[], "changes":[], "processes":[],
                               "pipelines":[], "edges":[]}),
            }
        }
        // The causal neighborhood of one pipeline: parent, children, owning
        // edge. args: [sid, brushprov_row_id].
        "review.pipeline_context" => {
            let id = arg_sid(args);
            let prov_id = args.get(1).and_then(Value::as_i64).unwrap_or(-1);
            match id {
                Some(id) => crate::review::pipeline_context(id, prov_id),
                None => json!({}),
            }
        }
        // Search the box's recorded makefile variable assignments. args:
        // [sid, name_pattern, value_pattern, limit]. Patterns are cmd_match
        // text globs (bare word = substring); empty = match all.
        "review.makevars" => {
            let id = arg_sid(args);
            let name_pat = args.get(1).and_then(Value::as_str).unwrap_or("");
            let value_pat = args.get(2).and_then(Value::as_str).unwrap_or("");
            let limit = args.get(3).and_then(Value::as_i64).unwrap_or(500);
            // 5th arg true = OR the two patterns (single-term "match name
            // OR value" queries from the UI).
            let any = args.get(4).and_then(Value::as_bool).unwrap_or(false);
            match id {
                Some(id) => crate::review::makevars(id, name_pat, value_pat,
                                                    limit, any),
                None => json!([]),
            }
        }
        // Map provenance row ids between the process / pipeline / edge
        // domains — the cross-pane generated filter's id translation.
        // args: [sid, from_kind, [ids...], to_kind] → [ids...].
        "review.map_ids" => {
            let id = arg_sid(args);
            let from = args.get(1).and_then(Value::as_str).unwrap_or("");
            let ids: Vec<i64> = args.get(2).and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            let to = args.get(3).and_then(Value::as_str).unwrap_or("");
            match id {
                Some(id) => crate::review::map_ids(id, from, &ids, to),
                None => json!([]),
            }
        }
        // Bulk decorate: one RPC for a whole window of changes-pane rows
        // (kind / stale / is_text per row) — the UI uses this to label the
        // changes list with +/~/- glyphs and the `!` stale marker without a
        // round-trip per row.
        "review.decorate_many" => {
            let id = arg_sid(args);
            let rels: Vec<&str> = args.get(1).and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match id {
                Some(id) => crate::review::decorate_many(id, &rels),
                None => Value::Array(vec![]),
            }
        }
        "review.apply_hunk" => { match (arg_sid(args), args.get(1).and_then(Value::as_str),
                                      args.get(2).and_then(Value::as_i64)) {
            (Some(id), Some(rel), Some(ix)) => {
                let r = crate::review::apply_hunk(id, rel, ix);
                drop_if_empty(state, id); r
            }
            _ => json!({"ok": false, "error": "bad args"}),
        }
        }
        "review.discard_hunk" => { match (arg_sid(args), args.get(1).and_then(Value::as_str),
                                        args.get(2).and_then(Value::as_i64)) {
            (Some(id), Some(rel), Some(ix)) => {
                let r = crate::review::discard_hunk(id, rel, ix);
                drop_if_empty(state, id); r
            }
            _ => json!({"ok": false, "error": "bad args"}),
        }
        }
        // ── server-side windowed views over per-box data ────────────────────
        // The UI lists are millions of rows in the limit; shipping the whole
        // set client-side made keystrokes multi-second. These verbs let the
        // client open a materialized view (filtered + sorted) and read it as
        // small windows — see views.rs.
        "view.open" => {
            let kind = match legacy_view_kind(args.first().and_then(Value::as_str)) {
                Ok(kind) => kind,
                Err(error) => return json!({"ok": false, "error": error}),
            };
            let Some(sid) = args.get(1).and_then(|value| value.as_u64().or_else(|| {
                value.as_str().and_then(|text| text.parse().ok())
            })) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            let filter = match legacy_filter_spec(args.get(2).unwrap_or(&Value::Null)) {
                Ok(filter) => filter,
                Err(error) => return json!({"ok": false, "error": error}),
            };
            let running_only = args.get(3).and_then(Value::as_bool).unwrap_or(true);
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::ViewOpen {
                    kind,
                    r#box: sid,
                    filter,
                    running_only,
                },
            ));
        }
        "view.window" => {
            let Some(view) = args.first().and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing or invalid view id"});
            };
            let Some(start) = args.get(1).and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing or invalid window start"});
            };
            let Some(size) = args.get(2).and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing or invalid window size"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::ViewWindow { view, start, size },
            ));
        }
        "view.filter" => {
            let Some(view) = args.first().and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing or invalid view id"});
            };
            let filter = match legacy_filter_spec(args.get(1).unwrap_or(&Value::Null)) {
                Ok(filter) => filter,
                Err(error) => return json!({"ok": false, "error": error}),
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::ViewFilter { view, filter },
            ));
        }
        "view.find" => {
            let Some(view) = args.first().and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing or invalid view id"});
            };
            let Some(row_id) = args.get(1).and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing or invalid row id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::ViewFind { view, row_id },
            ));
        }
        "view.close" => {
            let Some(view) = args.first().and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing or invalid view id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::ViewClose { view },
            ));
        }
        "ping" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Ping,
            ));
        }
        "box_new" => {
            // m3a: create a box and expose <mnt>/<id> — the overlay-core path
            // (the full runner register handshake is m3b).
            let ov = lock(state).overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not mounted"});
            };
            let id = boxes.keys().max().copied().unwrap_or(0) + 1;
            // optional parent arg (args[0]) — nests the new box for KIDS_DIR.
            let parent = args.first().and_then(Value::as_str)
                .and_then(|s| s.parse::<i64>().ok());
            match crate::capture::BoxState::create(id) {
                Ok(b) => {
                    b.set_parent(parent);
                    if let Some(p) = parent {
                        b.set_meta("parent_box_id", &p.to_string());
                    }
                    ov.add_box(std::sync::Arc::new(b));
                    // Same announce as register(): attached UIs need
                    // to know a new box exists. Without this the
                    // session list only updates on the next event of
                    // any kind (or a manual refresh).
                    broadcast(state, &json!({
                        "type": "session_added",
                        "sid": id.to_string(),
                        "parent": parent,
                    }));
                    json!({"sid": id.to_string(),
                           "root": crate::paths::mnt_point().join(id.to_string())
                                   .to_string_lossy()})
                }
                Err(e) => return json!({"ok": false,
                                        "error": format!("box_new: {e}")}),
            }
        }
        "struct_quick" => {
            match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
                (Some(id), Some(rel)) => crate::review::struct_quick(id, rel),
                _ => json!({"lines": [["err", "bad args"]], "job": Value::Null}),
            }
        }
        // ── flows pane: tshark-decoded HTTP/TLS rows for one box's pcapng ──
        // flows.list  [SID]              → {ok, flows: [row, ...]}
        // flows.detail [SID, FRAME]      → {ok, text: "..."}
        // SID may be omitted to mean the currently-selected box.
        "flows.list" => {
            match arg_sid(args).or_else(|| selected_sid(state)) {
                Some(id) => match flows_dir_for(id) {
                    Some(dir) => match crate::net::flows::tshark_list(&dir) {
                        Ok(rows) => json!({"ok": true,
                            "flows": rows.iter().map(|r| r.to_json())
                                .collect::<Vec<_>>()}),
                        Err(e) => json!({"ok": false, "error": e}),
                    },
                    None => json!({"ok": false, "error": "no flows dir for box"}),
                },
                None => json!({"ok": false, "error": "no box selected"}),
            }
        }
        "flows.detail" => {
            match flow_args(state, args)
                    .and_then(|(id, frame)| frame.as_u64().map(|frame| (id, frame))) {
                Some((id, frame)) if frame > 0 => match flows_dir_for(id) {
                    Some(dir) => match crate::net::flows::tshark_detail(&dir, frame) {
                        Ok(text) => json!({"ok": true, "text": text}),
                        Err(e) => json!({"ok": false, "error": e}),
                    },
                    None => json!({"ok": false, "error": "no flows dir for box"}),
                },
                _ => json!({"ok": false, "error": "bad args"}),
            }
        }
        // ── banner-prompt queue verbs (the TUI is the consumer) ────────
        // prompts.peek                          → {ok, ask: {...}|null}
        // prompts.answer [ID, "yes_once|no_once|allow_save|deny_save"]
        //                                       → {ok}
        // prompts.ui_active [bool]              → {ok}
        //   The TUI calls ui_active(true) on startup and ui_active(false)
        //   on shutdown; while inactive, dispatcher Ask short-circuits to
        //   deny so no connection wedges on an absent UI.
        "prompts.peek" => {
            match lock(state).net.clone() {
                Some(net) => match net.prompts.peek() {
                    Some(ask) => json!({"ok": true, "ask": {
                        "id": ask.id, "box": ask.box_name,
                        "host": ask.host, "port": ask.port,
                        "scheme": ask.scheme,
                    }}),
                    None => json!({"ok": true, "ask": Value::Null}),
                },
                None => json!({"ok": true, "ask": Value::Null}),
            }
        }
        "prompts.answer" => {
            let id = args.first().and_then(Value::as_u64).unwrap_or(0);
            let v = args.get(1).and_then(Value::as_str).unwrap_or("");
            let Some(verdict) = crate::net::prompt::Verdict::parse(v) else {
                return json!({"ok": false, "error": "bad verdict"});
            };
            match lock(state).net.clone() {
                Some(net) => {
                    let ok = net.prompts.answer(id, verdict);
                    // Net rules are reloaded from disk by the dispatcher on
                    // every connection (Rules::load() is cheap), so the
                    // newly-appended line takes effect immediately for
                    // future conns without touching the FUSE-side rule
                    // cache. (Doing the reload synchronously here was
                    // hanging on RwLock contention with the FUSE serve
                    // threads.)
                    json!({"ok": ok})
                }
                None => json!({"ok": false, "error": "no net registry"}),
            }
        }
        "prompts.ui_active" => {
            let on = args.first().and_then(Value::as_bool).unwrap_or(false);
            if let Some(net) = lock(state).net.clone() {
                net.prompts.mark_ui_active(on);
            }
            json!({"ok": true})
        }
        // flows.packets [SID, STREAM] → every frame in `tcp.stream == STREAM`
        // (i.e. the connection the user just drilled into from the flows
        // list pane). Powers the packet-list view inside Pane::Packets.
        "flows.packets" => {
            match flow_args(state, args)
                    .and_then(|(id, stream)| stream.as_i64().map(|stream| (id, stream))) {
                Some((id, stream)) if stream >= 0 => match flows_dir_for(id) {
                    Some(dir) => match crate::net::flows::tshark_packets(&dir, stream) {
                        Ok(rows) => json!({"ok": true,
                            "packets": rows.iter().map(|r| r.to_json())
                                .collect::<Vec<_>>()}),
                        Err(e) => json!({"ok": false, "error": e}),
                    },
                    None => json!({"ok": false, "error": "no flows dir for box"}),
                },
                _ => json!({"ok": false, "error": "bad args"}),
            }
        }
        "struct_finish" => { match args.first().and_then(Value::as_i64) {
            Some(job) => crate::review::struct_finish(job),
            None => json!({"lines": [["err", "bad job"]]}),
        }
        }
        "struct_cancel" => {
            if let Some(job) = args.first().and_then(Value::as_i64) {
                crate::review::struct_cancel(job);
            }
            return json!({"ok": true, "r": Value::Null});
        }
        "box_drop" => {
            let ov = lock(state).overlay.clone();
            if let (Some(ov), Some(id)) = (ov, arg_sid(args)) {
                ov.remove_box(id);
            }
            json!({"ok": true})
        }
        // ── box-rooted file ops — the engine-side half of oaita's read/write/
        //    inspect tools. Resolve name→id, hydrate the parent chain, then
        //    use the same overlay API nested boxes use. No FUSE mount needed,
        //    no subprocess. args: [name_or_id, path_rel_to_root, (write only)
        //    base64-bytes]. path must NOT start with '/'.
        "box_file_read" => {
            let ov = lock(state).overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_file_read: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            match ov.box_read_file(id, rel) {
                Ok(bytes) => {
                    use base64::{Engine, prelude::BASE64_STANDARD};
                    json!({"bytes": BASE64_STANDARD.encode(bytes)})
                }
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            }
        }
        "box_file_write" => {
            let ov = lock(state).overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_file_write: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            let b64 = args.get(2).and_then(Value::as_str).unwrap_or("");
            use base64::{Engine, prelude::BASE64_STANDARD};
            let bytes = match BASE64_STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => return json!({"ok": false,
                    "error": format!("bad base64: {e}")}),
            };
            // Shared guard+write core with review.write_file — the agent path
            // gains the tombstone/symlink/dir/binary refusals it lacked.
            // `return` (not fall-through) so a refusal reaches the executor
            // as an envelope error, not a silently-wrapped {ok:false}.
            // allow_create = true: the agent authors new files.
            return crate::review::write_file_checked(id, rel, &bytes, &ov, true);
        }
        "box_dir_list" => {
            let ov = lock(state).overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_dir_list: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            match ov.box_list_dir(id, rel) {
                Ok(entries) => Value::Array(entries.into_iter()
                    .map(|(n, k)| json!({"name": n, "kind": k.to_string()}))
                    .collect()),
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            }
        }
        "box_path_kind" => {
            let ov = lock(state).overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_path_kind: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            json!({"kind": ov.box_path_kind(id, rel).to_string()})
        }
        // OCI registry I/O runs HOST-SIDE in the engine: the CLI (host or
        // in-box) RPCs these so credentials never enter a box and an in-box
        // pull doesn't go through the box's netns/FUSE. Box EXECUTION stays in
        // the caller (runner::run) for foreground stdio. The pull blocks this
        // connection's own handler thread, not the main loop.
        "oci.load" => {
            let Some(reference) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "oci.load: missing reference"});
            };
            let name = args.get(1).and_then(Value::as_str).map(String::from);
            match crate::oci::load_blocking(reference, name) {
                Ok(o) => json!({
                    "base_id": o.base_id, "base_name": o.base_name,
                    "top_id": o.top_id, "top_name": o.top_name,
                    "n_layers": o.n_layers, "verified": o.verified,
                }),
                Err(e) => return json!({"ok": false, "error": format!("{e:#}")}),
            }
        }
        // Loaded images, for the UI's base-image picker: the TOP box of each
        // installed layer chain (the one carrying the image config), with the
        // reference it was pulled as. Cheap metadata scan, no registry I/O.
        "oci.images" => {
            let boxes = crate::discover::discover();
            Value::Array(boxes.iter()
                .filter(|(_, b)| b.meta.contains_key("oci_config"))
                .filter_map(|(id, b)| {
                    let reference = b.meta.get("oci_reference")?;
                    Some(json!({
                        "id": id,
                        "name": b.name,
                        "reference": reference,
                        "digest": b.meta.get("oci_manifest_digest")
                                        .cloned().unwrap_or_default(),
                    }))
                })
                .collect())
        }
        // Is a named svc.serve service live (≥1 parked accept slot)? Used by
        // `oaita local` to poll readiness / idempotency without racing.
        "svc.up" => {
            let name = args.first().and_then(Value::as_str).unwrap_or("");
            json!({"up": svc_has(name)})
        }
        "oci.resolve" => {
            let Some(reference) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "oci.resolve: missing reference"});
            };
            match crate::oci::resolve_image_top_local(reference) {
                Ok((top, note)) => json!({"top_id": top, "note": note}),
                Err(e) => return json!({"ok": false, "error": format!("{e:#}")}),
            }
        }
        // In-box `oci build`: the CLI ships its context + Dockerfile here so the
        // build runs host-side (its layer boxes land in engine state, not the
        // box's FUSE). Returns the worker's output + exit code + top box id.
        "oci.build" => {
            let Some(spec) = args.first() else {
                return json!({"ok": false, "error": "oci.build: missing spec"});
            };
            match crate::oci::build_in_engine(spec) {
                Ok(v) => v,
                Err(e) => return json!({"ok": false, "error": format!("{e:#}")}),
            }
        }
        // The local-model picker's catalog: currently-popular GGUF instruct
        // models resolved from a live HuggingFace query (config-file override
        // + offline fallback). Each entry is a ready-to-download Q4 URL. The
        // UI opens this when neither an external API nor a local model is set.
        "oaita.models" => {
            let (entries, source) = crate::oaita::models::catalog();
            json!({
                "source": source,
                "models": entries.iter().map(|e| json!({
                    "name": e.name, "url": e.url, "note": e.note,
                })).collect::<Vec<_>>(),
            })
        }
        // What the "Api" pane is wired to: external (host oaita.toml has a
        // model), local (an OAITA-LOCAL svc is declared), or none (offer the
        // picker). Lets the UI reflect real state instead of guessing.
        "oaita.status" => {
            let host_cfg = crate::oaita::config::Config::load();
            let external = host_cfg.model.as_deref()
                .map(|m| !m.trim().is_empty()).unwrap_or(false);
            let local = service_declared("oaita-local");
            let (kind, model, endpoint) = if external {
                ("external",
                 host_cfg.model.clone().unwrap_or_default(),
                 host_cfg.base_url.clone().unwrap_or_default())
            } else if local {
                ("local", "local".to_string(), "svc://oaita-local".to_string())
            } else {
                ("none", String::new(), String::new())
            };
            json!({
                "kind": kind, "model": model, "endpoint": endpoint,
                "serving": svc_has("oaita-local"),
            })
        }
        // Connection test for the external-API config editor: does a minimal
        // 1-token chat completion against the given endpoint and reports
        // reachability / auth / model validity as a single line. Runs on the
        // engine (which has the network), so the UI stays a thin client.
        "oaita.probe" => {
            let spec = args.first().cloned().unwrap_or(Value::Null);
            let base_url = spec.get("base_url").and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("https://api.openai.com/v1").to_string();
            let model = spec.get("model").and_then(Value::as_str)
                .unwrap_or("").to_string();
            let api_key = spec.get("api_key").and_then(Value::as_str)
                .unwrap_or("").to_string();
            if model.trim().is_empty() {
                return json!({"ok": false, "error": "set a model name first"});
            }
            let probe = crate::oaita::client::block_on(async {
                let client = crate::oaita::client::Client::from_resolved(
                    &base_url, &api_key)?;
                let body = json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1, "stream": false,
                });
                client.post("/chat/completions", body).await
            });
            match probe {
                Ok(_) => json!({"ok": true, "detail":
                    format!("connected · {model} @ {base_url}")}),
                Err(e) => return json!({"ok": false,
                    "error": format!("{base_url}: {e}")}),
            }
        }
        "verbs" => {
            let filter = match args.first() {
                None => None,
                Some(Value::String(value)) => {
                    let Ok(value) = crate::wire::BoundedText::new(value.clone()) else {
                        return json!({"ok": false, "error": "help filter exceeds relation bound"});
                    };
                    Some(value)
                }
                Some(_) => return json!({"ok": false, "error": "help filter must be text"}),
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Verbs { filter },
            ));
        }
    } };
}

macro_rules! emit_verb_dispatch {
    ( ($state:ident, $verb:ident, $args:ident, $boxes:ident)
      $($($name:literal)|+ => $body:block)* ) => {
        /// The `{"type":"ui"}` verb dispatch. An arm's `return` sends its
        /// value raw (errors); falling out wraps as {"ok":true,"r":...}.
        pub(crate) fn dispatch_ui_verb($state: &State, $verb: &str,
                                       $args: &[Value],
                                       $boxes: &std::collections::BTreeMap<i64, discover::Box_>)
                                       -> Value {
            let r: Value = match $verb {
                $( $($name)|+ => $body )*
                other => {
                    return json!({"ok": false, "error":
                        format!("unknown verb '{other}'; see 'verbs'")});
                }
            };
            json!({"ok": true, "r": r})
        }
    };
}

ui_verbs!(emit_verb_dispatch);

fn dispatch_ui(state: &State, msg: &Value) -> Value {
    let verb = msg.get("verb").and_then(Value::as_str).unwrap_or("");
    let empty = vec![];
    let args = msg.get("args").and_then(Value::as_array).unwrap_or(&empty);
    let boxes = discover::discover();
    dispatch_ui_verb(state, verb, args, &boxes)
}

/// One recvmsg on the box channel: read up to `buf` bytes and capture the first
/// SCM_RIGHTS fd if any (a MUTE frame attaches --inner's pidfd). Returns the
/// byte count (0 = EOF, <0 = error) and sets `*fd` to a received fd.
fn recv_frame_bytes(raw: i32, buf: &mut [u8], fd: &mut Option<i32>) -> isize {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    // socklen_t on glibc, size_t on musl — `as _` matches the field type.
    msg.msg_controllen = cmsg.len() as _;
    let n = unsafe { libc::recvmsg(raw, &mut msg, 0) };
    if n > 0 {
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let mut got = 0i32;
                    std::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(c),
                        (&mut got as *mut i32).cast(),
                        4,
                    );
                    if fd.is_none() {
                        *fd = Some(got);
                    } else {
                        libc::close(got);
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
    }
    n
}

/// A UnixStream with a few leading bytes (already pulled into the BufReader
/// before we noticed this was a PTY connection) replayed first on read. Writes
/// and clones go straight to the underlying stream. This lets `pty::serve_pty`
/// treat the whole connection — prebuffered frame bytes included — as one Read.
struct Prebuffered {
    pre: Vec<u8>,
    pos: usize,
    inner: UnixStream,
}

impl std::io::Read for Prebuffered {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.pre.len() {
            let n = (self.pre.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.pre[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}
impl std::io::Write for Prebuffered {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
impl crate::pty::CloneStream for Prebuffered {
    fn clone_stream(&self) -> Self {
        // The clone shares the socket but NOT the one-time prebuffer (only the
        // original replays it, so bytes are never delivered twice).
        Prebuffered {
            pre: vec![],
            pos: 0,
            inner: self.inner.try_clone().expect("UnixStream::try_clone"),
        }
    }
    fn shutdown_read(&self) {
        let _ = self.inner.shutdown(std::net::Shutdown::Read);
    }
}

/// Engine-held-PTY connection (D7/D9). `msg` is the `pty_spawn` request:
///   {"type":"pty_spawn","argv":[...],"rows":R,"cols":C}
/// We ack one JSON line, then the connection becomes a bidirectional FRAME_PTY_*
/// mux driven by `crate::pty::serve_pty` (master↔client + EOF). `prebuf` is any
/// bytes the BufReader already consumed past the request line.
///
/// HONEST SCOPE: the command is spawned DIRECTLY on the engine's PTY (no bwrap /
/// overlay box). The mux/render/input loop is the proven, reusable half; wrapping
/// the PTY child in an overlay-backed box reuses this exact frame mux and is the
/// documented follow-on (DESIGN.md D9 — PTY mode toggle over the box channel).
fn handle_pty_spawn(msg: &Value, writer: &mut UnixStream, prebuf: Vec<u8>) {
    let argv: Vec<String> = msg
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if argv.is_empty() {
        let _ = writer.write_all(b"{\"ok\":false,\"error\":\"pty_spawn: empty argv\"}\n");
        return;
    }
    let rows = msg.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
    let cols = msg.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
    // cwd: the UI's $PWD at the moment it sent the spawn — what the user
    // sees as "where I am". Without this the child inherits the engine
    // daemon's cwd (whatever it was when the daemon started, usually $HOME)
    // and `bash -i` opens in the wrong dir. Engine daemon is long-lived so
    // its own cwd is unreliable; the UI's is correct per-launch.
    let cwd: Option<std::path::PathBuf> = msg
        .get("cwd")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from);
    // env: portable_pty's CommandBuilder starts from a MINIMAL env by
    // default — SHELL/HOME/USER/PATH absent, so `bash -i` lands in a
    // broken shell. The UI ships its own envvars and we lay them on top
    // of the daemon's so the user gets a normal session.
    let env: Vec<(String, String)> = msg
        .get("env")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    // Ack BEFORE the frame mux begins so the client knows to switch to frames.
    if writer.write_all(b"{\"ok\":true,\"r\":\"pty\"}\n").is_err() {
        return;
    }
    let _ = writer.flush();
    let stream = match writer.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let chan = Prebuffered {
        pre: prebuf,
        pos: 0,
        inner: stream,
    };
    crate::pty::serve_pty(&argv, rows, cols, chan, None, cwd.as_deref(), &env);
}

/// Top-level handler for a control connection. `hint_box_id` is the box id
/// the conn is known to come from — broker-spawned conns set it (the broker
/// authenticated the dialer through the box channel); host conns leave it
/// None. Today only the `api.proxy` verb consumes it (the LLM-API
/// passthrough needs to know which box's apikey/admission to use); other
/// verbs derive identity via SCM_RIGHTS pidfd as before.
fn handle(state: State, conn: UnixStream) {
    handle_guarded(state, conn, None)
}

/// Panic-isolating wrapper around `handle_with_box` (audit M4). A panic in a
/// connection handler must not escape its thread (which would, combined with a
/// held `state` lock, poison the shared mutex and take down every other
/// connection). `catch_unwind` contains the panic here; paired with `lock()`'s
/// poison recovery, one bad handler can no longer wedge the control plane. We
/// assert unwind-safety because the recovery path (lock() via into_inner)
/// tolerates a possibly-half-updated `Shared`, and a dropped UnixStream is fine.
fn handle_guarded(state: State, conn: UnixStream, hint_box_id: Option<i64>) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_with_box(state, conn, hint_box_id)
    }));
    if r.is_err() {
        eprintln!(
            "sarun-engine: control connection handler panicked; \
                   connection dropped (engine continues)"
        );
    }
}

fn handle_with_box(state: State, conn: UnixStream, hint_box_id: Option<i64>) {
    // The register handshake carries the runner's pidfd as the connection's
    // first SCM_RIGHTS fd; keep it for host-pid derivation + kill. It belongs
    // to the FIRST message only (a register); close it if that never comes.
    let (mut peer_pidfd, peer_tapfd_raw, peer_thirdfd_raw) = recv_first_fd(&conn);
    // The TAP fd (tap boxes) and the sud trace-pipe fd (sud boxes) ride the
    // register message's SCM_RIGHTS after the pidfd. Own both so every
    // non-register path drops → closes them; the register call takes them and
    // sorts roles by want_sud + net_mode.
    let mut peer_tapfd: Option<std::os::fd::OwnedFd> = peer_tapfd_raw
        .map(|fd| unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    let mut peer_thirdfd: Option<std::os::fd::OwnedFd> = peer_thirdfd_raw
        .map(|fd| unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return;
        }
    });
    let mut writer = conn;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                if let Some(fd) = peer_pidfd.take() {
                    unsafe {
                        libc::close(fd);
                    }
                }
                return;
            }
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            if let Some(fd) = peer_pidfd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            return;
        };
        // Engine-held PTY (D7/D9): this connection becomes a bidirectional
        // FRAME_PTY_* mux. Handled in its own function, fully separate from the
        // newline-JSON verb dispatch below. Any register pidfd is irrelevant to a
        // PTY connection, so close it.
        if msg.get("type").and_then(Value::as_str) == Some("pty_spawn") {
            if let Some(fd) = peer_pidfd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            handle_pty_spawn(&msg, &mut writer, reader.buffer().to_vec());
            return;
        }
        // api.proxy: LLM-API passthrough conn from an in-box `oaita` client.
        // The conn was dialed via the FD broker (hint_box_id is set to the
        // box that hosts the dialer); the engine accepts HTTP/1.1 on this
        // conn, injects the configured upstream auth, forwards to the real
        // LLM API, and streams the response back on the same conn. Replaces
        // the FRAME_API_OPEN/DATA/CLOSE mux on the box-channel — one conn
        // per HTTP call, no stream-id demux needed.
        if msg.get("type").and_then(Value::as_str) == Some("api.proxy") {
            if let Some(fd) = peer_pidfd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            let Some(box_id) = hint_box_id else {
                return;
            };
            // No early budget gate here. We used to debit + write a 503
            // over the raw socket if the chain was exhausted, which closed
            // the conn while the client was still streaming its request
            // body — hyper on the client side then surfaced "send: error
            // writing a body to connection" instead of a parseable 503.
            // The debit + 503 now lives inside proxy::handle_inner, AFTER
            // hyper has drained the body, so the response goes back as
            // proper HTTP. State is passed through so handle_inner can
            // do the chain walk.
            // Each early-return path below LOGS into the box's api_log
            // so a dropped conn isn't silent. Without these, a stuck
            // broker or a missed runtime surfaced as the inscrutable
            // "send: error writing a body to connection" on the client
            // side with NO matching engine-side record of what
            // happened — the bug the post-mortem on mimo's fft4 ran
            // into. The status field uses the closest matching HTTP
            // value (502 = bad gateway, since "we couldn't even start
            // serving") and the body explains the specific cause.
            let (rt_opt, proxy_opt) = {
                let s = lock(&state);
                (s.net_rt.clone(), s.api_proxy.clone())
            };
            let (Some(rt), Some(proxy)) = (rt_opt, proxy_opt) else {
                if let Some(p) = lock(&state).api_proxy.clone() {
                    crate::oaita::proxy::log_call(
                        &p,
                        box_id,
                        "POST",
                        "/",
                        "",
                        502,
                        &[],
                        b"engine runtime or proxy unavailable",
                        false,
                    );
                }
                return;
            };
            let prebuffered = reader.buffer().to_vec();
            drop(reader);
            if writer.set_nonblocking(true).is_err() {
                crate::oaita::proxy::log_call(
                    &proxy,
                    box_id,
                    "POST",
                    "/",
                    "",
                    502,
                    &[],
                    b"set_nonblocking on api.proxy conn failed",
                    false,
                );
                return;
            }
            let std_conn = writer;
            let state_for_proxy = state.clone();
            rt.spawn(async move {
                let tokio_conn = match tokio::net::UnixStream::from_std(std_conn) {
                    Ok(c) => c,
                    Err(e) => {
                        crate::oaita::proxy::log_call(
                            &proxy,
                            box_id,
                            "POST",
                            "/",
                            "",
                            502,
                            &[],
                            format!("UnixStream::from_std: {e}").as_bytes(),
                            false,
                        );
                        return;
                    }
                };
                let io = PrebufferedIo::new(tokio_conn, prebuffered);
                let _ = crate::oaita::proxy::serve_one_conn_for_box(
                    proxy,
                    state_for_proxy,
                    box_id,
                    hyper_util::rt::TokioIo::new(io),
                )
                .await;
            });
            return;
        }
        // svc.serve: a box-side process PARKS this connection as one accept
        // slot of a named in-box service. The engine listens on a host UDS
        // ({runtime_home}/svc-<name>.sock); each host client that connects
        // is paired with a parked slot (the slot gets one "paired" line,
        // then both streams are spliced byte-for-byte). This is the inbound
        // path INTO a box that the tap stack deliberately doesn't have: an
        // in-box server (e.g. `oaita local`'s llama-server) becomes
        // host-reachable over sockets alone — no netns is ever shared.
        if msg.get("type").and_then(Value::as_str) == Some("svc.serve") {
            if let Some(fd) = peer_pidfd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            let Some(name) = msg.get("name").and_then(Value::as_str) else {
                let _ = writer.write_all(b"{\"ok\":false,\"error\":\"svc.serve: missing name\"}\n");
                return;
            };
            if !svc_name_ok(name) {
                let _ = writer.write_all(b"{\"ok\":false,\"error\":\"svc.serve: bad name\"}\n");
                return;
            }
            drop(reader);
            if writer
                .write_all(b"{\"ok\":true,\"r\":\"parked\"}\n")
                .is_err()
            {
                return;
            }
            SVC_PARKED
                .lock()
                .unwrap()
                .entry(name.to_string())
                .or_default()
                .push_back(writer);
            return;
        }
        // svc.declare: a box declares that IT provides an on-demand service.
        // Stamped onto the DIALING box's meta (hint_box_id): `svc_provide`
        // (name), `svc_argv` (JSON `--` payload to run in the serve sub-box),
        // `svc_net` (optional net mode). Later, when a box dials svc://<name>
        // and nothing is serving, the engine starts that payload as a
        // sub-box PARENTED on this box (see ensure_service). Generic: any box
        // — an oci image, a downloaded model box — can advertise a server
        // this way. See engine/DESIGN.md "On-demand box services".
        if msg.get("type").and_then(Value::as_str) == Some("svc.declare") {
            if let Some(fd) = peer_pidfd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            let name = msg.get("name").and_then(Value::as_str).unwrap_or("");
            let argv = msg.get("argv").cloned().unwrap_or(Value::Null);
            let net = msg.get("net").and_then(Value::as_str).unwrap_or("");
            let ok = svc_name_ok(name) && argv.is_array();
            if ok {
                if let (Some(bid), Some(ov)) = (hint_box_id, lock(&state).overlay.clone()) {
                    if let Some(b) = ov.box_of(bid) {
                        b.set_meta("svc_provide", name);
                        b.set_meta("svc_argv", &argv.to_string());
                        b.set_meta("svc_net", net);
                    }
                }
            }
            let reply = if ok {
                "{\"ok\":true}\n"
            } else {
                "{\"ok\":false,\"error\":\"svc.declare: bad name/argv\"}\n"
            };
            let _ = writer.write_all(reply.as_bytes());
            return;
        }
        // svc.dial: the host-side half — THIS connection becomes a raw
        // stream spliced onto a parked svc.serve slot of the named service.
        // Rides the ordinary control socket (like pty_spawn / api.proxy);
        // no extra listener or socket file exists for services.
        if msg.get("type").and_then(Value::as_str) == Some("svc.dial") {
            if let Some(fd) = peer_pidfd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            let Some(name) = msg.get("name").and_then(Value::as_str) else {
                let _ = writer.write_all(b"{\"ok\":false,\"error\":\"svc.dial: missing name\"}\n");
                return;
            };
            // Bytes the dialer pipelined right behind its dial line must
            // reach the service too.
            let prebuffered = reader.buffer().to_vec();
            drop(reader);
            let Some(mut slot) = svc_pair(name) else {
                let _ = writer.write_all(
                    format!(
                        "{{\"ok\":false,\"error\":\"svc.dial: no live \
                     '{name}' service (is its box running?)\"}}\n"
                    )
                    .as_bytes(),
                );
                return;
            };
            if writer.write_all(b"{\"ok\":true,\"r\":\"svc\"}\n").is_err() {
                return;
            }
            if !prebuffered.is_empty() && slot.write_all(&prebuffered).is_err() {
                return;
            }
            svc_splice(writer, slot);
            return;
        }
        // budget.grant: add `amount` to a box's pool. Additive — resume
        // extends it, doesn't reset. The box is identified either by
        // display name (`box: "OAITA-X..."`) when the caller knows the
        // name (top-level cli), OR implicitly as the conn's
        // hint_box_id (set by the FD broker handshake) when the caller
        // is in-box and doesn't need to spell out its own identity.
        if msg.get("type").and_then(Value::as_str) == Some("budget.grant") {
            if let Some(fd) = peer_pidfd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            let amount = msg.get("amount").and_then(Value::as_i64).unwrap_or(0);
            let name = msg.get("box").and_then(Value::as_str);
            let bid = match name {
                Some(n) if !n.is_empty() => {
                    let boxes = discover::discover();
                    resolve(&boxes, n)
                }
                _ => hint_box_id,
            };
            let resp = match bid {
                Some(id) => {
                    crate::oaita::budget::grant(&state, id, amount);
                    let rem = crate::oaita::budget::remaining(&state, id).unwrap_or(0);
                    format!("{{\"ok\":true,\"remaining\":{rem}}}\n")
                }
                None => "{\"ok\":false,\"error\":\"box not resolvable\"}\n".to_string(),
            };
            let _ = writer.write_all(resp.as_bytes());
            return;
        }
        // The oaita API proxy lives on the existing box-channel as new
        // FRAME_API_* frame types — not as a top-level connection type.
        // See the FRAME_API_* handling in the post-register frame loop
        // below and frames::FRAME_API_{OPEN,DATA,CLOSE}.
        let mut reply = if msg.get("type").and_then(Value::as_str) == Some("register") {
            register(
                &state,
                &msg,
                peer_pidfd.take(),
                peer_tapfd
                    .take()
                    .map(|f| <std::os::fd::OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(f)),
                peer_thirdfd
                    .take()
                    .map(|f| <std::os::fd::OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(f)),
            )
        } else if msg.get("type").and_then(Value::as_str) == Some("brush_prov_nested") {
            // D9 nested-shell provenance: a one-shot control message from the
            // brush-sh shim, carrying its OWN pidfd (like register) so we resolve
            // the enclosing box from /proc ancestry. NOT a box channel — record
            // and reply once, then the connection closes. The pidfd is consumed.
            brush_prov_nested(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("brush_prov_done") {
            // D9 pipeline completion: a one-shot control message emitted after a
            // pipeline's complete-command finishes, carrying its OWN pidfd (like
            // brush_prov_nested) so we resolve the box, then stamp done_ts +
            // exit_code on the matching brushprov rows (by uid).
            brush_prov_done(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("recipe_fixup") {
            recipe_fixup(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("build_edges") {
            // Phase 1 embedded-ninja: a one-shot control message from the
            // shadowed `ninja` (vendored n2) carrying its OWN pidfd, resolved to
            // the enclosing box by /proc ancestry exactly like brush_prov_nested.
            build_edges(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("make_vars") {
            make_vars(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("box_activity") {
            box_activity(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("build_edge_state") {
            // A single edge's run-state transition (started / finished), sent by
            // the in-process make/ninja executor around each recipe — stamps the
            // box's build_edges row so the targets pane shows live build progress.
            build_edge_state(&state, &msg, peer_pidfd.take())
        } else {
            dispatch(&state, &msg)
        };
        let subscribe = reply
            .get("_subscribe")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let box_sid = reply
            .as_object_mut()
            .and_then(|o| o.remove("_box_sid"))
            .and_then(|v| v.as_i64());
        if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
            return;
        }
        if let Some(id) = box_sid {
            // This connection IS the box's muxed channel now. Register it as the
            // echo writer (the sink-write handler frames captured bytes onto it),
            // then read MUTE/UNMUTE frames from --inner: MUTE carries --inner's
            // pidfd (SCM_RIGHTS) → resolve its host pid and mute it (its echo
            // readback is not re-recorded); UNMUTE / EOF unmutes. EOF = teardown.
            let ov = lock(&state).overlay.clone();
            if let Some(ov) = ov.as_ref() {
                if let Ok(w) = writer.try_clone() {
                    ov.set_echo(id, Arc::new(Mutex::new(w)));
                }
            }
            let raw = reader.get_ref().as_raw_fd();
            let mut fbuf = reader.buffer().to_vec();
            let mut pending_fd: Option<i32> = None;
            let mut muted_pid: Option<i32> = None;
            loop {
                let (frames, used) = crate::frames::decode(&fbuf);
                for (ft, _) in &frames {
                    match *ft {
                        crate::frames::FRAME_MUTE => {
                            if let Some(fd) = pending_fd.take() {
                                let hp = host_pid_from_pidfd(fd);
                                unsafe {
                                    libc::close(fd);
                                }
                                if hp > 0 {
                                    muted_pid = Some(hp);
                                    if let Some(ov) = ov.as_ref() {
                                        ov.mute_add(hp, id);
                                        // D9 brush↔process linkage: for a brush box
                                        // the muted pid IS the embedded brush shell's
                                        // --inner host tgid — the forest root every
                                        // pipeline process descends from. Record it so
                                        // brush-descendant rows can be attributed.
                                        if let Some(b) = ov.live_box(id) {
                                            if b.is_brush() {
                                                b.set_brush_host_tgid(hp as u32);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        crate::frames::FRAME_UNMUTE => {
                            if let (Some(hp), Some(ov)) = (muted_pid.take(), ov.as_ref()) {
                                ov.mute_remove(hp);
                            }
                        }
                        _ => {}
                    }
                }
                // D9 brush provenance (separate from MUTE/UNMUTE): a FRAME_PROV
                // carries a JSON object describing one shell command the box's
                // embedded brush shell ran. Record it into the box's sqlar and
                // broadcast a `brush_prov` event so live UIs see it.
                for (ft, payload) in &frames {
                    if *ft == crate::frames::FRAME_PROV {
                        record_brush_prov(&state, &ov, id, payload);
                    }
                }
                // FD broker — the inner asks the engine for a fresh
                // engine connection it can hand to a child via SCM_RIGHTS
                // (no bind-mount of ui.sock into the box). Engine creates
                // a socketpair, spawns its own handler on one side, and
                // sends the other side back over the box-channel as
                // FRAME_CONN + SCM_RIGHTS. Attribution is intrinsic to
                // the channel — every fresh conn is for THIS box.
                for (ft, _) in &frames {
                    if *ft != crate::frames::FRAME_OPEN_CONN {
                        continue;
                    }
                    let writer = ov.as_ref().and_then(|o| o.echo_writer(id));
                    let Some(writer) = writer else {
                        continue;
                    };
                    let (server_side, runner_side) = match std::os::unix::net::UnixStream::pair() {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let st_handler = state.clone();
                    let box_id_hint = id;
                    std::thread::spawn(move || {
                        handle_guarded(st_handler, server_side, Some(box_id_hint))
                    });
                    use std::os::fd::AsRawFd;
                    send_frame_with_fd(
                        &writer,
                        &crate::frames::encode(crate::frames::FRAME_CONN, &[]),
                        runner_side.as_raw_fd(),
                    );
                    drop(runner_side); // the receiver dup's it on recvmsg
                }
                fbuf.drain(..used);
                if frames.is_empty() {
                    let mut tmp = [0u8; 4096];
                    let mut fd = None;
                    let n = recv_frame_bytes(raw, &mut tmp, &mut fd);
                    if n <= 0 {
                        break;
                    }
                    if let Some(f) = fd {
                        if let Some(old) = pending_fd.replace(f) {
                            unsafe {
                                libc::close(old);
                            }
                        }
                    }
                    fbuf.extend_from_slice(&tmp[..n as usize]);
                }
            }
            if let Some(fd) = pending_fd {
                unsafe {
                    libc::close(fd);
                }
            }
            if let Some(ov) = ov.as_ref() {
                if let Some(hp) = muted_pid {
                    ov.mute_remove(hp);
                }
                // D9 brush↔process linkage: now that the box channel hit EOF the
                // brush shell has exited — ALL pipelines + process rows exist, so
                // attribute every brush-spawned process to its pipeline in one
                // race-free pass (no-op for non-brush boxes).
                if let Some(b) = ov.live_box(id) {
                    b.finalize_brush_links();
                }
                ov.clear_echo(id);
                ov.remove_box(id);
            }
            {
                let mut s = lock(&state);
                if let Some(fd) = s.box_pids.remove(&id) {
                    unsafe {
                        libc::close(fd);
                    }
                }
                s.box_runpids.remove(&id);
                if let Some(p) = s.api_proxy.clone() {
                    p.disable_box(id);
                }
            }
            broadcast(
                &state,
                &json!({"type": "session_removed",
                                      "session_id": id.to_string()}),
            );
            return;
        }
        if subscribe {
            // The connection becomes a one-way event feed: park it in the
            // subscriber list; broadcast() writes to it and prunes on error.
            lock(&state).subscribers.push(writer);
            return;
        }
    }
}

/// A leading CLI token that names a box: all-caps start, [A-Z0-9-.] body
/// (dots allow a nested display path A.B), no trailing '-'. Mirrors the Python
/// `valid_dotted_name` gate that turns `slopbox NAME …` into a box op.
pub fn is_box_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' || c == '.')
        && !s.ends_with('-')
}

/// CLI conveniences `sarun-engine NAME [op [arg]]` — connect to the running
/// engine's control socket and act on the named box (the verbs already exist
/// engine-side). `NAME` alone selects; `patch` prints the unified diff; `apply`
/// / `discard` act on the whole box; `rename NEW` renames. Mirrors the Python
/// `slopbox NAME patch|apply|discard|rename`.
// ── mirror attach plumbing (MIRRORS.md phase 4) ─────────────────────────────

/// A live handle for `sid`, hydrating the at-rest box (open sqlar, load
/// mirror, register) if it is not already live. The shared preamble of
/// the attach/rotate verbs.
fn hydrate_box(
    ov: &crate::overlay::Overlay,
    sid: i64,
) -> Option<std::sync::Arc<crate::capture::BoxState>> {
    if ov.box_of(sid).is_none() {
        if !crate::paths::state_home()
            .join(format!("{sid}.sqlar"))
            .exists()
        {
            return None;
        }
        let tb = crate::capture::BoxState::create(sid).ok()?;
        tb.load_mirror();
        ov.add_box(std::sync::Arc::new(tb));
    }
    ov.box_of(sid)
}

/// Open a wikimak instance READ-ONLY (shared flock, every write API
/// refuses): sizing defaults come from `wikimak_wikipedia::read_config`
/// (page-id bound derived from the existing depot's on-disk index).
/// Coexists with hydrated attachments and other readers; only a
/// writing `wikimak import`/`sync` briefly excludes it.
fn open_wiki_instance(root: &str) -> Result<wikimak_wikipedia::Instance, String> {
    wikimak_wikipedia::Instance::open_read(wikimak_wikipedia::read_config(
        std::path::PathBuf::from(root),
    ))
    .map_err(|e| e.to_string())
}

// ── svc: engine-spliced host↔box service streams ────────────────────────────
// State is engine-global (one namespace of service names). Parked slots are
// plain blocking UnixStreams; the splice is two copy threads per paired
// stream. A dead slot (box exited) is detected at pairing time — the
// "paired" line write fails — and the next slot is tried.

fn svc_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with('.')
}

static SVC_PARKED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<UnixStream>>>,
> = std::sync::LazyLock::new(Default::default);

/// (Re)write the safe-for-box `oaita.toml` from the CURRENT host config: the
/// model name, no api_key, and a marker base_url (the in-box client uses the
/// bind-mounted proxy, not this URL). Called at engine startup AND as each
/// --api box registers, so a config written after startup (e.g. `oaita
/// local`) reaches the box instead of the stale startup snapshot.
pub fn write_api_box_oaita_toml() {
    let host_cfg = crate::oaita::config::Config::load();
    // Model name: the host config's, or "local" when a sarun-local model is
    // available (so the box needs no config), else empty. The engine routes
    // regardless — this is just the name that rides in the request.
    let model = host_cfg
        .model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if service_declared("oaita-local") {
                "local".into()
            } else {
                String::new()
            }
        });
    let safe_toml = format!(
        "# Auto-generated by sarun for --api boxes. Read-only via FUSE.\n\
         model = {model:?}\n\
         base_url = \"http://oaita-proxy/v1\"\n"
    );
    let _ = std::fs::write(crate::paths::api_box_oaita_toml_path(), &safe_toml);
}

/// Whether a named svc.serve service currently has ≥1 parked accept slot
/// (i.e. its box is up and bridging). The serving box parks several slots
/// and re-parks after each pairing, so this reliably reflects "up" under
/// the sequential calls an agent makes.
pub fn svc_has(name: &str) -> bool {
    SVC_PARKED
        .lock()
        .unwrap()
        .get(name)
        .is_some_and(|q| !q.is_empty())
}

/// Whether any box has DECLARED an on-demand service `name` (meta
/// `svc_provide` == name), regardless of whether it's currently serving.
/// Lets the proxy default its upstream to a sarun-local model without any
/// oaita.toml — the declaration IS the "a local model exists" signal.
pub fn service_declared(name: &str) -> bool {
    discover::discover().values().any(|b| {
        b.meta
            .get("svc_provide")
            .map(|s| s == name)
            .unwrap_or(false)
    })
}

/// GENERIC on-demand service start. When a box dials `svc://<name>` and
/// nothing is serving (a fresh engine after restart, or the serve box was
/// discarded), find the box that DECLARED the service (meta `svc_provide`
/// == name, set via `svc.declare`) and start its declared payload
/// (`svc_argv`) as a sub-box PARENTED on it — so the sub-box reads the
/// declaring box's files without any apply. Serialized + idempotent:
/// concurrent callers coalesce onto one start. Returns once a slot parks
/// (server ready) or errors with an actionable reason. This is the reusable
/// mechanism behind `oaita local`; any box can advertise a server the same
/// way. See engine/DESIGN.md "On-demand box services".
pub async fn ensure_service(name: &str) -> Result<(), String> {
    if svc_has(name) {
        return Ok(());
    }
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    if svc_has(name) {
        return Ok(());
    } // another caller won the race
    // Find the declaring box.
    let boxes = discover::discover();
    let Some((&pid, decl)) = boxes.iter().find(|(_, b)| {
        b.meta
            .get("svc_provide")
            .map(|s| s == name)
            .unwrap_or(false)
    }) else {
        return Err(format!(
            "no box provides service '{name}' — nothing has \
            declared it (svc.declare)"
        ));
    };
    let argv: Vec<String> = decl
        .meta
        .get("svc_argv")
        .and_then(|s| serde_json::from_str(s).ok())
        .ok_or_else(|| format!("service '{name}': malformed svc_argv"))?;
    if argv.is_empty() {
        return Err(format!("service '{name}': empty svc_argv"));
    }
    // Net for the serve sub-box: declared, or default tap with a host
    // fallback where netns is unavailable (mirrors box networking).
    let net = match decl.meta.get("svc_net").map(String::as_str) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => if crate::net::tap::tap_available() {
            "tap"
        } else {
            "host"
        }
        .to_string(),
    };
    // Serve sub-box parented on the declaring box (numeric-id prefix — the
    // engine resolves it as the parent, same stacking `oci run` uses).
    let child = format!(
        "SVC-{}",
        name.to_ascii_uppercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            })
            .collect::<String>()
    );
    let serve_box = format!("{pid}.{child}");
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "sarun".into());
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["run", "--net", &net, &serve_box, "--"]);
    for a in &argv {
        cmd.arg(a);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // setsid: outlive whatever triggered the start.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| format!("start service '{name}': {e}"))?;
    for _ in 0..240 {
        // ~120s: cold server / model load
        if svc_has(name) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(format!(
        "service '{name}' did not become ready in time \
        (check box '{serve_box}')"
    ))
}

/// Pop the first LIVE parked slot for `name` (a dead slot — its box exited
/// — fails the "paired" write and the next one is tried).
fn svc_pair(name: &str) -> Option<UnixStream> {
    loop {
        let slot = SVC_PARKED
            .lock()
            .unwrap()
            .get_mut(name)
            .and_then(|q| q.pop_front())?;
        let mut slot = slot;
        if slot.write_all(b"{\"ok\":true,\"r\":\"paired\"}\n").is_ok() {
            return Some(slot);
        }
    }
}

/// Bidirectional byte splice between two blocking UnixStreams; each side's
/// EOF shuts down the peer's write half so HTTP keep-alive teardown works.
fn svc_splice(a: UnixStream, b: UnixStream) {
    let (Ok(mut ar), Ok(mut bw)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut ar, &mut bw);
        let _ = bw.shutdown(std::net::Shutdown::Write);
    });
    let (mut br, mut aw) = (b, a);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut br, &mut aw);
        let _ = aw.shutdown(std::net::Shutdown::Write);
    });
}

/// `sarun mirror …` — the mirror-jobs CLI (schedule surface of
/// mirrors.rs; the TUI's Mirrors pane shows the same rows).
pub fn cli_mirror(argv: &[String]) -> i32 {
    let sock = crate::paths::sock_path();
    let one = |verb: &str, args: Value| -> Result<Value, String> {
        let mut c = UnixStream::connect(&sock).map_err(|_| "no engine running".to_string())?;
        let msg = json!({"type": "ui", "verb": verb, "args": args});
        c.write_all(format!("{msg}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(&c)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        // Wrapped ok/r on success; bare early-return errors.
        Ok(v.get("r").cloned().unwrap_or(v))
    };
    let fail = |e: String| -> i32 {
        eprintln!("sarun-engine: {e}");
        1
    };
    let mut words = vec!["mirror"];
    if argv.is_empty() {
        words.push("ls");
    } else {
        words.extend(argv.iter().map(String::as_str));
    }
    let invocation = match crate::parser::parse_words(&words, &crate::parser::EmptyContext) {
        crate::parser::ParseResult::Invocation(invocation) => invocation,
        _ => {
            eprintln!(
                "usage: sarun mirror ls\n       sarun mirror add git|wiki|ietf|cmd SRC DEST [INTERVAL_SECS]  (cmd: SRC is a shell command, DEST = $1)\n       sarun mirror run [ID]        (no ID = run all pending)\n       sarun mirror pause|resume|rm ID"
            );
            return 2;
        }
    };
    let rpc_args = invocation.json_args();
    let protocol_verb = invocation.dispatch_name();
    match invocation.action.as_str() {
        "mirror_jobs" => match one(protocol_verb, rpc_args) {
            Ok(Value::Array(jobs)) => {
                for j in jobs {
                    let g = |k: &str| j.get(k).cloned().unwrap_or(Value::Null);
                    let due = g("next_due")
                        .as_i64()
                        .map(|d| {
                            let dt = d - std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|x| x.as_secs() as i64)
                                .unwrap_or(0);
                            if dt <= 0 {
                                "now".to_string()
                            } else {
                                format!("in {}m", dt / 60)
                            }
                        })
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{:>3}  {:<9} {:<5} {:<28} → {:<24} every {}m  due {}{}",
                        g("id").as_i64().unwrap_or(0),
                        g("state").as_str().unwrap_or("?"),
                        g("kind").as_str().unwrap_or("?"),
                        g("src").as_str().unwrap_or(""),
                        g("dest").as_str().unwrap_or(""),
                        g("interval_secs").as_i64().unwrap_or(0) / 60,
                        due,
                        match g("last_detail").as_str() {
                            Some(d) if !d.is_empty() && g("state").as_str() == Some("error") =>
                                format!("  [{}]", d.lines().last().unwrap_or("")),
                            _ => String::new(),
                        }
                    );
                }
                0
            }
            Ok(v) => fail(
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("bad reply")
                    .into(),
            ),
            Err(e) => fail(e),
        },
        "mirror_add" => match one(protocol_verb, rpc_args) {
            Ok(v) if v.get("ok") == Some(&Value::Bool(true)) => {
                println!(
                    "job {} added",
                    v.get("id").and_then(Value::as_i64).unwrap_or(-1)
                );
                0
            }
            Ok(v) => fail(
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("add failed")
                    .into(),
            ),
            Err(e) => fail(e),
        },
        "mirror_run_pending" => match one(protocol_verb, rpc_args) {
            Ok(v) if v.get("ok") == Some(&Value::Bool(true)) => {
                let n = v
                    .get("started")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                println!("{n} pending job(s) started");
                0
            }
            Ok(v) => fail(
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("run failed")
                    .into(),
            ),
            Err(e) => fail(e),
        },
        "mirror_run" | "mirror_pause" | "mirror_resume" | "mirror_rm" => {
            match one(protocol_verb, rpc_args) {
                Ok(v) if v.get("ok") == Some(&Value::Bool(true)) => {
                    match v.get("note").and_then(Value::as_str) {
                        Some(n) if !n.is_empty() => println!("ok — {n}"),
                        _ => println!("ok"),
                    }
                    0
                }
                Ok(v) => fail(
                    v.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .into(),
                ),
                Err(e) => fail(e),
            }
        }
        _ => 2,
    }
}

/// `sarun verbs [FILTER]` — print the running engine's projection of the
/// central action relation through the `verbs` action. Works from inside a
/// box too: the FD broker (SARUN_BROKER, same channel cli_box_op uses)
/// splices the connection to the engine's control socket.
pub fn cli_verbs(argv: &[String]) -> i32 {
    let filter = argv.first().cloned().unwrap_or_default();
    let sock = crate::paths::sock_path();
    let broker_name = std::env::var("SARUN_BROKER").ok().filter(|s| !s.is_empty());
    let dial = || -> std::io::Result<UnixStream> {
        match broker_name.as_ref() {
            Some(n) => crate::runner::broker_dial(n),
            None => UnixStream::connect(&sock),
        }
    };
    let mut c = match dial() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("sarun-engine: no engine running");
            return 1;
        }
    };
    let msg = json!({"type": "ui", "verb": "verbs", "args": [filter]});
    if let Err(e) = c.write_all(format!("{msg}\n").as_bytes()) {
        eprintln!("sarun-engine: {e}");
        return 1;
    }
    let mut line = String::new();
    if BufReader::new(&c).read_line(&mut line).is_err() {
        eprintln!("sarun-engine: read failed");
        return 1;
    }
    let v: Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sarun-engine: {e}");
            return 1;
        }
    };
    let Some(rows) = v.get("r").and_then(Value::as_array) else {
        eprintln!(
            "sarun-engine: {}",
            v.get("error")
                .and_then(Value::as_str)
                .unwrap_or("bad reply")
        );
        return 1;
    };
    let g = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let namew = rows.iter().map(|r| g(r, "verb").len()).max().unwrap_or(0);
    let argw = rows.iter().map(|r| g(r, "args").len()).max().unwrap_or(0);
    for r in rows {
        println!(
            "{:<namew$}  {:<argw$}  {}",
            g(r, "verb"),
            g(r, "args"),
            g(r, "help")
        );
    }
    if rows.is_empty() {
        eprintln!("no verbs match '{filter}'");
    }
    0
}

pub fn cli_box_op(argv: &[String]) -> i32 {
    let name = argv[0].as_str();
    let op = argv.get(1).map(String::as_str);
    // IN-BOX vs HOST socket selection. A `sarun OAITA-X discard` invoked
    // from INSIDE a box dials the FD broker (abstract UDS, name in
    // SARUN_BROKER — served by the parent inner). From HOST we use the
    // engine's filesystem control socket. No fallback chain.
    let sock = crate::paths::sock_path();
    let broker_name = std::env::var("SARUN_BROKER").ok().filter(|s| !s.is_empty());
    let dial = || -> std::io::Result<UnixStream> {
        if let Some(n) = broker_name.as_ref() {
            crate::runner::broker_dial(n)
        } else {
            UnixStream::connect(&sock)
        }
    };
    let one = |msg: Value| -> Result<Value, String> {
        let mut c = dial().map_err(|_| "no engine running".to_string())?;
        c.write_all(format!("{msg}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(&c)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    };
    let report = |r: Result<Value, String>| -> i32 {
        match r {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => 0,
            Ok(v) => {
                eprintln!(
                    "sarun-engine: {}",
                    v.get("error").and_then(Value::as_str).unwrap_or("failed")
                );
                1
            }
            Err(e) => {
                eprintln!("sarun-engine: {e}");
                1
            }
        }
    };
    match op {
        None => report(one(json!({"type": "select", "sid": name}))),
        // sarun NAME stuck — wedge diagnosis: every live process of the
        // box with its kernel wait channel + current syscall, so a silent
        // hang answers "where?" without strace.
        Some("stuck") => {
            // arg_sid takes a numeric SID; resolve the NAME first.
            let sid = match one(json!({"type": "ui", "verb": "resolve_box",
                                       "args": [name]}))
            {
                Ok(v) => match v.get("r").and_then(Value::as_str).map(String::from) {
                    Some(s) => s,
                    None => {
                        eprintln!("sarun-engine: no slopbox '{name}'");
                        return 1;
                    }
                },
                Err(e) => {
                    eprintln!("sarun-engine: {e}");
                    return 1;
                }
            };
            match one(json!({"type": "ui", "verb": "stuck",
                             "args": [sid]}))
            {
                Ok(v)
                    if v.get("ok").and_then(Value::as_bool) == Some(true)
                        && v.get("r")
                            .and_then(|r| r.get("ok"))
                            .and_then(Value::as_bool)
                            == Some(true) =>
                {
                    let r = v.get("r").cloned().unwrap_or(Value::Null);
                    let empty = vec![];
                    let procs = r.get("procs").and_then(Value::as_array).unwrap_or(&empty);
                    println!(
                        "{:>7} {:>7} {:>2} {:<16} {}",
                        "PID", "TID", "ST", "COMM", "BLOCKED-ON"
                    );
                    for p in procs {
                        let g =
                            |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        let n = |k: &str| p.get(k).and_then(Value::as_i64).unwrap_or(0);
                        println!(
                            "{:>7} {:>7} {:>2} {:<16} {}",
                            n("pid"),
                            n("tid"),
                            g("state"),
                            g("comm"),
                            g("detail")
                        );
                        // Backtrace under the thread — the only thing that
                        // localizes a "running" spin. Print for spinning /
                        // non-idle threads; skip the dozen idle futex workers.
                        let idle = g("detail").starts_with("futex")
                            || g("detail").starts_with("epoll")
                            || g("detail") == "wait4()";
                        if !idle {
                            if let Some(bt) = p.get("bt").and_then(Value::as_array) {
                                for f in bt.iter().filter_map(Value::as_str) {
                                    println!("{:>19}   {}", "", f);
                                }
                            }
                        }
                    }
                    if procs.is_empty() {
                        println!(
                            "(no live threads — the box's tree has \
                                  exited; the runner may be stuck in \
                                  teardown)"
                        );
                    }
                    0
                }
                Ok(v) => {
                    // The verb reply nests under "r" for ui verbs; surface
                    // either error shape.
                    let e = v
                        .get("r")
                        .and_then(|r| r.get("error"))
                        .and_then(Value::as_str)
                        .or_else(|| v.get("error").and_then(Value::as_str))
                        .unwrap_or("failed");
                    eprintln!("sarun-engine: {e}");
                    1
                }
                Err(e) => {
                    eprintln!("sarun-engine: {e}");
                    1
                }
            }
        }
        Some("apply") | Some("discard") => {
            let t = op.unwrap();
            match one(json!({"type": t, "sid": name})) {
                Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                    println!(
                        "{}: {} {}",
                        name,
                        v.get("count").and_then(Value::as_i64).unwrap_or(0),
                        t
                    );
                    0
                }
                other => report(other),
            }
        }
        // sarun NAME checkout <store> <ref> [DEST] [SUBPATH]
        // Stream a commit out of a gitdepot mirror store into NAME's
        // changes — ordinary files the box owns, bounded memory.
        Some("checkout") => {
            let (Some(src), Some(refname)) = (argv.get(2), argv.get(3)) else {
                eprintln!("usage: sarun NAME checkout <store> <ref> [DEST] [SUBPATH]");
                return 2;
            };
            let src = std::fs::canonicalize(src)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| src.clone());
            let sid = match one(json!({"type": "ui", "verb": "resolve_box",
                                       "args": [name]}))
            {
                Ok(v) => match v
                    .get("r")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    Some(id) => id,
                    None => {
                        eprintln!("sarun-engine: no box {name}");
                        return 1;
                    }
                },
                Err(e) => {
                    eprintln!("sarun-engine: {e}");
                    return 1;
                }
            };
            let mut vargs = vec![json!(sid), json!(src), json!(refname)];
            if let Some(d) = argv.get(4) {
                vargs.push(json!(d));
            }
            if let Some(sp) = argv.get(5) {
                vargs.push(json!(sp));
            }
            match one(json!({"type": "ui", "verb": "git_checkout",
                             "args": vargs}))
            {
                Ok(v) => {
                    let r = v.get("r").cloned().unwrap_or(v);
                    if r.get("ok").and_then(Value::as_bool) == Some(true) {
                        println!(
                            "checked out {} into {name} ({} files)",
                            r.get("sha").and_then(Value::as_str).unwrap_or("?"),
                            r.get("files").and_then(Value::as_u64).unwrap_or(0)
                        );
                        0
                    } else {
                        eprintln!(
                            "sarun-engine: {}",
                            r.get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("checkout failed")
                        );
                        1
                    }
                }
                Err(e) => {
                    eprintln!("sarun-engine: {e}");
                    1
                }
            }
        }
        // sarun NAME attach wiki <root> <page-id> [PREFIX]
        //                  attach ietf <root> <draft> [PREFIX]
        // Mirror serve path (ATTACH-CONVERGENCE.md) for BOUNDED
        // single-object stores: pin the mirror object as an external RO
        // reference on NAME — no import, the overlay serves it from the
        // store on first read. (git is NOT attachable: a whole tree is
        // checked out instead — see `checkout`.)
        Some("attach") => {
            let (Some(kind), Some(src), Some(key)) = (argv.get(2), argv.get(3), argv.get(4)) else {
                eprintln!("usage: sarun NAME attach wiki|ietf <src> <page|draft> [PREFIX]");
                return 2;
            };
            let verb = match kind.as_str() {
                "wiki" => "wiki_attach",
                "ietf" => "ietf_attach",
                "git" => {
                    eprintln!(
                        "sarun-engine: git attach was removed — use \
`sarun {name} checkout <store> <ref> [DEST]` to stream the commit into the box"
                    );
                    return 2;
                }
                other => {
                    eprintln!("sarun-engine: unknown attach kind {other:?} (wiki|ietf)");
                    return 2;
                }
            };
            // The engine resolves the source path from ITS cwd — send it
            // absolute.
            let src = std::fs::canonicalize(src)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| src.clone());
            let sid = match one(json!({"type": "ui", "verb": "resolve_box",
                                       "args": [name]}))
            {
                // resolve_box replies with the id as a STRING (or null).
                Ok(v) => match v
                    .get("r")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    Some(id) => id,
                    None => {
                        eprintln!("sarun-engine: no box {name}");
                        return 1;
                    }
                },
                Err(e) => {
                    eprintln!("sarun-engine: {e}");
                    return 1;
                }
            };
            // wiki pages go by title or id; the verb resolves either.
            let key_v: Value = json!(key);
            let mut vargs = vec![json!(sid), json!(src), key_v];
            if let Some(p) = argv.get(5) {
                vargs.push(json!(p));
            }
            match one(json!({"type": "ui", "verb": verb, "args": vargs})) {
                Ok(v) => {
                    // Success replies arrive wrapped ({"ok":true,"r":…});
                    // verb-side early-return errors arrive bare.
                    let r = v.get("r").cloned().unwrap_or(v);
                    if r.get("ok").and_then(Value::as_bool) == Some(true) {
                        // The attachment's display name carries the
                        // pinned rev ("…@<rev>") — the whole identity.
                        println!(
                            "attached {} to {name}",
                            r.get("name").and_then(Value::as_str).unwrap_or("?")
                        );
                        0
                    } else {
                        eprintln!(
                            "sarun-engine: {}",
                            r.get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("attach failed")
                        );
                        1
                    }
                }
                Err(e) => {
                    eprintln!("sarun-engine: {e}");
                    1
                }
            }
        }
        Some("rename") => {
            let new = argv.get(2).map(String::as_str).unwrap_or("");
            match one(json!({"type": "rename", "sid": name, "name": new})) {
                Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                    println!("renamed box {name} -> {new}");
                    0
                }
                other => report(other),
            }
        }
        Some("patch") => match one(json!({"type": "patch", "sid": name})) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                if let Some(b64) = v.get("patch").and_then(Value::as_str) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        use std::io::Write;
                        let _ = std::io::stdout().write_all(&bytes);
                    }
                }
                0
            }
            other => report(other),
        },
        Some(o) => {
            eprintln!("sarun-engine: unknown op '{o}'");
            2
        }
    }
}

/// Outcome of the single-instance lock attempt (audit M2).
pub enum InstanceLock {
    /// We hold the exclusive lock; KEEP this fd alive for the engine's whole
    /// lifetime (dropping it / exiting releases the kernel `flock`).
    Held(std::os::fd::OwnedFd),
    /// Another engine already holds the lock — this process must not start.
    AlreadyRunning,
}

/// Acquire the single-instance lock atomically (audit M2). The old guard was
/// TOCTOU: `connect(sock).is_ok()` probed for a live socket, then — much later,
/// after self-heal — `serve()` did `remove_file(sock)` + `bind(sock)`. Two
/// engines launched together could BOTH see no live socket (neither had bound
/// yet), both proceed, then both remove + bind, last-writer-wins, leaving one
/// engine serving on an unlinked socket nobody can reach.
///
/// Instead, take an exclusive `flock(LOCK_EX|LOCK_NB)` on a dedicated lock file
/// next to the socket BEFORE doing anything. The kernel grants it to exactly one
/// process; a second engine gets `EWOULDBLOCK` and bails. The lock is advisory
/// but every engine goes through this same path, and it is released
/// automatically by the kernel when the holding process exits (even on crash),
/// so no stale lock survives a dead daemon — strictly better than the
/// remove-then-bind dance for liveness detection.
pub fn acquire_instance_lock(sock: &std::path::Path) -> std::io::Result<InstanceLock> {
    use std::os::fd::AsRawFd;
    // The lock file lives beside the socket; ensure its dir exists (serve() runs
    // this before ensure_dirs()).
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock_path = sock.with_extension("lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    // SAFETY: f.as_raw_fd() is a valid open fd for the duration of the call.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        if matches!(e.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            return Ok(InstanceLock::AlreadyRunning);
        }
        return Err(e);
    }
    Ok(InstanceLock::Held(f.into()))
}

/// Bind the control socket. Done EARLY in serve startup (before the FUSE mount
/// and the rest of init) so the socket file only ever appears once it is a live,
/// listening socket — a client that connects during startup queues in the listen
/// backlog and is served once the accept loop runs, instead of racing a stale
/// socket file (the old code printed "listening" and ran init while the previous
/// daemon's dead socket still sat at `sock`, so a waiter could connect to it and
/// see ECONNREFUSED). The instance lock (acquire_instance_lock) is already held,
/// so removing a leftover socket from a dead daemon is safe here.
pub fn bind_listener(sock: &std::path::Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock)?;
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o600);
    std::fs::set_permissions(sock, mode)?;
    Ok(listener)
}

/// Run the accept loop on an already-bound listener (see [`bind_listener`]).
pub fn serve(state: State, listener: UnixListener) -> std::io::Result<()> {
    for conn in listener.incoming().flatten() {
        let st = state.clone();
        std::thread::spawn(move || handle(st, conn));
    }
    Ok(())
}

/// A tokio AsyncRead+AsyncWrite that serves a fixed prefix first, then
/// delegates to the wrapped UnixStream. Used by the `api.proxy` handoff:
/// the BufReader that read the verb header may have buffered some HTTP
/// request bytes the client wrote immediately after the header line, and
/// hyper needs to see those too.
struct PrebufferedIo {
    prefix: Vec<u8>,
    pos: usize,
    inner: tokio::net::UnixStream,
}

impl PrebufferedIo {
    fn new(inner: tokio::net::UnixStream, prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl tokio::io::AsyncRead for PrebufferedIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let avail = self.prefix.len() - self.pos;
            let n = avail.min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PrebufferedIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, b)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Send a frame over the box-channel WITH an attached fd via SCM_RIGHTS.
/// Engine-side analogue of runner::send_frame's pidfd path. Best-effort:
/// a closed/blocked channel silently fails (the receiver will EOF later
/// and we don't want one slow runner to deadlock the engine).
fn send_frame_with_fd(channel: &std::sync::Arc<Mutex<UnixStream>>, frame: &[u8], fd: i32) {
    use std::os::fd::AsRawFd;
    let c = channel.lock().unwrap();
    let conn_fd = c.as_raw_fd();
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
        let cm = libc::CMSG_FIRSTHDR(&msg);
        (*cm).cmsg_level = libc::SOL_SOCKET;
        (*cm).cmsg_type = libc::SCM_RIGHTS;
        (*cm).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping((&fd as *const i32).cast(), libc::CMSG_DATA(cm), 4);
        libc::sendmsg(conn_fd, &msg, 0);
    }
}

#[cfg(test)]
mod verb_tests {
    use super::*;

    // Calling every related verb with empty args is not safe in a unit test
    // (mirror_run_pending starts jobs, oaita.models does network I/O), so
    // we spot-check side-effect-free verbs instead.
    #[test]
    fn relation_help_is_complete_nonempty_and_unique() {
        let docs = crate::prolog::global().unwrap().ui_action_help().unwrap();
        assert_eq!(docs.len(), 91);
        let mut names: Vec<&str> = docs.iter().map(|d| d.verb.as_str()).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "duplicate UI actions in relation");
        for doc in docs {
            assert!(
                !doc.description.as_str().is_empty(),
                "action {} has no help",
                doc.verb.as_str()
            );
        }
    }

    #[test]
    fn unknown_verb_error_points_at_verbs() {
        let state: State = Default::default();
        let boxes = std::collections::BTreeMap::new();
        let r = dispatch_ui_verb(&state, "no_such_verb", &[], &boxes);
        let err = r.get("error").and_then(Value::as_str).unwrap();
        assert!(
            err.contains("unknown verb") && err.contains("see 'verbs'"),
            "got: {err}"
        );
    }

    #[test]
    fn typed_control_results_project_only_at_the_legacy_boundary() {
        use crate::generated_wire::{
            ActionMutationResult, ActionSuccess, PathError, SudEvent, SudEventKind, SudTraceView,
        };
        let path = crate::wire::BoundedBytes::new(b"src/main.rs".to_vec()).unwrap();
        let errors = crate::wire::BoundedVec::new(vec![PathError {
            path: Some(path),
            message: crate::wire::BoundedText::new("stale".into()).unwrap(),
        }])
        .unwrap();
        let reply = legacy_control_reply(ActionSuccess::Apply {
            value: ActionMutationResult {
                r#box: 7,
                count: 2,
                errors,
            },
        });
        assert_eq!(reply["sid"], "7");
        assert_eq!(reply["count"], 2);
        assert_eq!(reply["errors"][0]["path"], "src/main.rs");

        let trace = SudTraceView {
            events: crate::wire::BoundedVec::new(vec![SudEvent {
                time_ns: 4,
                kind: SudEventKind::Unknown { code: 42 },
                pid: 1,
                tgid: 1,
                ppid: 0,
                extras: crate::wire::BoundedVec::new(Vec::new()).unwrap(),
                text: crate::wire::BoundedText::new("future".into()).unwrap(),
            }])
            .unwrap(),
            truncated: false,
        };
        let reply = legacy_control_reply(ActionSuccess::Sudtrace { value: trace });
        assert_eq!(reply["events"][0]["kind"], "42");
        assert_eq!(reply["events"][0]["text"], "future");
    }

    #[test]
    fn typed_view_requests_drive_the_stateful_view_registry() {
        use crate::generated_wire::{
            ActionRequest, ActionSuccess, FilterClause, FilterJoin, FilterKind, PipelineRow,
        };
        let state: State = Default::default();
        let pipeline = |id, command: &str| PipelineRow {
            id,
            time: 0.0,
            command: crate::wire::BoundedText::new(command.into()).unwrap(),
            record: None,
            pipeline: None,
            spawned_at: None,
            done_at: None,
            nested: false,
            uid: None,
            parent_uid: None,
            exit_code: Some(0),
            processes: crate::wire::BoundedVec::new(Vec::new()).unwrap(),
        };
        lock(&state).views.insert(
            9,
            crate::views::View {
                sid: 1,
                source: crate::views::ViewRows::Pipelines(vec![
                    pipeline(11, "drop"),
                    pipeline(12, "keep this"),
                ]),
                idx: vec![0, 1],
                filter: None,
                aux: crate::views::ViewAux::Pipelines,
            },
        );

        let found = dispatch_action(
            &state,
            ActionRequest::ViewFind {
                view: 9,
                row_id: 12,
            },
        )
        .unwrap();
        assert_eq!(found, ActionSuccess::ViewFind { value: Some(1) });

        let filter = crate::wire::BoundedVec::new(vec![FilterClause {
            kind: FilterKind::Cmd,
            pattern: crate::wire::BoundedText::new("keep".into()).unwrap(),
            join: FilterJoin::And,
            negated: false,
            enabled: true,
        }])
        .unwrap();
        let filtered = dispatch_action(
            &state,
            ActionRequest::ViewFilter {
                view: 9,
                filter: Some(filter),
            },
        )
        .unwrap();
        assert_eq!(
            filtered,
            ActionSuccess::ViewFilter {
                value: crate::generated_wire::ViewFilterResult { total: 1 },
            }
        );

        let window = dispatch_action(
            &state,
            ActionRequest::ViewWindow {
                view: 9,
                start: 0,
                size: 10,
            },
        )
        .unwrap();
        let ActionSuccess::ViewWindow {
            value: crate::generated_wire::ViewWindow::Pipelines { start, total, rows },
        } = window else {
            panic!("wrong typed view window variant")
        };
        assert_eq!((start, total), (0, 1));
        assert_eq!(rows.as_slice()[0].id, 12);

        assert_eq!(
            dispatch_action(&state, ActionRequest::ViewClose { view: 9 }).unwrap(),
            ActionSuccess::ViewClose { value: () }
        );
        assert!(!lock(&state).views.contains_key(&9));
    }

    #[test]
    fn documented_verbs_spot_check_dispatch() {
        let state: State = Default::default();
        let boxes = std::collections::BTreeMap::new();
        // Two safe representatives: the self-list and an args-validating
        // mutator that errors before any side effect.
        for v in ["verbs", "mirror_pause"] {
            let r = dispatch_ui_verb(&state, v, &[], &boxes);
            let err = r.get("error").and_then(Value::as_str).unwrap_or("");
            assert!(!err.contains("unknown verb"), "{v} fell through: {err}");
        }
        // And the self-list actually lists itself + honors the filter arg.
        let r = dispatch_ui_verb(&state, "verbs", &[json!("mirror")], &boxes);
        let rows = r["r"].as_array().unwrap();
        assert!(
            rows.iter()
                .all(|x| x["verb"].as_str().unwrap().contains("mirror")
                    || x["help"].as_str().unwrap().contains("mirror"))
        );
        assert!(rows.len() >= 5);
        let all = dispatch_ui_verb(&state, "verbs", &[], &boxes);
        assert_eq!(all["r"].as_array().unwrap().len(), 91);
    }

    #[test]
    fn flow_args_accept_optional_string_sid_and_numeric_value() {
        let state: State = Default::default();
        lock(&state).selected = Some("42".into());

        let selected_args = [json!(34)];
        let explicit_args = [json!("12"), json!(34)];
        assert_eq!(flow_args(&state, &selected_args), Some((42, &json!(34))));
        assert_eq!(flow_args(&state, &explicit_args), Some((12, &json!(34))));

        for args in [
            vec![],
            vec![json!(12), json!(34)],
            vec![json!("bad"), json!(34)],
            vec![json!("12"), json!(34), json!(56)],
        ] {
            assert_eq!(flow_args(&state, &args), None, "accepted {args:?}");
        }

        let no_selection: State = Default::default();
        assert_eq!(flow_args(&no_selection, &selected_args), None);
    }

    #[test]
    fn flow_handlers_accept_explicit_or_selected_sid() {
        let state: State = Default::default();
        lock(&state).selected = Some("42".into());
        let boxes = std::collections::BTreeMap::new();

        for (verb, value) in [("flows.detail", 1), ("flows.packets", 0)] {
            for args in [vec![json!(value)], vec![json!("42"), json!(value)]] {
                let r = dispatch_ui_verb(&state, verb, &args, &boxes);
                assert_eq!(r["r"]["error"], "no flows dir for box", "{verb} {args:?}");
            }
            for args in [
                vec![json!("42")],
                vec![json!(42), json!(value)],
                vec![json!("42"), json!("not numeric")],
            ] {
                let r = dispatch_ui_verb(&state, verb, &args, &boxes);
                assert_eq!(r["r"]["error"], "bad args", "{verb} {args:?}");
            }
        }

        assert_eq!(
            dispatch_ui_verb(&state, "flows.detail", &[json!(0)], &boxes)["r"]["error"],
            "bad args"
        );
        assert_eq!(
            dispatch_ui_verb(&state, "flows.packets", &[json!(-1)], &boxes)["r"]["error"],
            "bad args"
        );
    }
}
