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
use std::os::unix::ffi::OsStrExt;
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
    if let (Some(state), Ok(r#box)) = (STATE_HANDLE.read().clone(), u64::try_from(box_id)) {
        broadcast(
            &state,
            &crate::generated_wire::SubscriptionEvent::ApiLogAdded {
                r#box,
            },
        );
    }
}

/// Broadcast that box `box_id` has new webcap rows so the UI's Captures pane
/// refreshes. Best-effort, mirroring `broadcast_api_log` (DESIGN-web.md W1).
pub fn broadcast_webcap(box_id: i64) {
    if let (Some(state), Ok(r#box)) = (STATE_HANDLE.read().clone(), u64::try_from(box_id)) {
        broadcast(
            &state,
            &crate::generated_wire::SubscriptionEvent::WebCaptureAdded {
                r#box,
            },
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
    if let (Ok(r#box), Ok(row)) = (u64::try_from(id), u64::try_from(prov_id)) {
        broadcast(state,
            &crate::generated_wire::SubscriptionEvent::BrushProvenanceAdded { r#box, row });
    }
}

/// D9 nested-shell provenance verb. The brush-sh shim (a `sh -c RECIPE` the box
/// spawned, exec'd as the engine binary) sends one `brush_prov_nested` message
/// carrying ITS OWN pidfd as SCM_RIGHTS. We resolve the enclosing box from the
/// shim's /proc ancestry — the EXACT path `register` uses for a nested box — and
/// record each record as a NESTED brushprov row, broadcasting a `brush_prov`
/// event per row. Best-effort: an unresolvable box or malformed message is
/// dropped quietly (the recipe runs regardless; provenance is optional). This is
/// a one-shot control reply — it does NOT create a box channel.
fn brush_prov_nested(
    state: &State,
    records: &[crate::generated_wire::PipelineProvenance],
    peer_pidfd: Option<i32>,
) -> Result<u64, String> {
    let id = enclosing_box_from_pidfd(state, peer_pidfd)?;
    let records = records.iter().map(|record| {
        if !record.nested { return Err("nested provenance record is not marked nested".into()); }
        let sequence = i64::try_from(record.sequence)
            .map_err(|_| "pipeline sequence exceeds SQLite range")?;
        let uid = i64::try_from(record.uid)
            .map_err(|_| "pipeline uid exceeds SQLite range")?;
        let parent_uid = i64::try_from(record.parent_uid)
            .map_err(|_| "parent pipeline uid exceeds SQLite range")?;
        let targets = record.output_targets.as_slice().iter().map(|target|
            std::str::from_utf8(target.as_slice()).map(str::to_owned)
                .map_err(|_| "pipeline output target is not UTF-8".to_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((record.command.as_str().to_owned(),
            crate::discover::pipeline_provenance_json(record).to_string(),
            sequence, record.spawned_at, uid, parent_uid, targets))
    }).collect::<Result<Vec<_>, String>>()?;
    let ov = lock(state).overlay.clone();
    let mut count = 0u64;
    for (command, record_json, sequence, spawned_at, uid, parent_uid, targets) in records {
        let mut prov_id = 0i64;
        if let Some(ov) = ov.as_ref() {
            if let Some(b) = ov.live_box(id) {
                prov_id =
                    b.add_brushprov_nested(&command, &record_json, sequence, spawned_at, uid, parent_uid);
                b.on_brush_prov(prov_id, targets);
                // Set cur_brush_pipeline to the FIRST pipeline in this
                // complete-command — that is the one that executes first and
                // produces the initial output. Later pipelines in an and-or
                // list (cmd1 && cmd2) run after the first finishes; we cannot
                // track per-pipeline transitions within one complete-command,
                // but attributing to the first is correct for the common
                // single-pipeline case and for the first leg of a chain.
                if count == 0 {
                    b.set_cur_brush_pipeline(prov_id);
                }
            }
        }
        if let (Ok(r#box), Ok(row)) = (u64::try_from(id), u64::try_from(prov_id)) {
            broadcast(state,
                &crate::generated_wire::SubscriptionEvent::BrushProvenanceAdded { r#box, row });
        }
        count += 1;
    }
    Ok(count)
}

/// D9 pipeline completion. After a pipeline's complete-command finishes, the box
/// sends one message carrying the completed pipelines' `uids`, the `code`, and
/// the `done_ts` (wall clock), plus ITS OWN pidfd (resolve-the-box like
/// brush_prov_nested). We stamp done_ts + exit_code on those brushprov rows so a
/// reader can show per-pipeline wall time and tell running (done_ts==0) from
/// finished. Best-effort; one-shot reply.
fn enclosing_box_from_pidfd(state: &State, peer_pidfd: Option<i32>) -> Result<i64, String> {
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd {
        unsafe {
            libc::close(fd);
        }
    }
    derive_parent_box(state, host_pid).ok_or_else(|| "no enclosing box".into())
}

fn brush_prov_done(
    state: &State,
    pipelines: &[u64],
    code: i32,
    done_at: f64,
    peer_pidfd: Option<i32>,
) -> Result<(), String> {
    let id = enclosing_box_from_pidfd(state, peer_pidfd)?;
    let pipelines = pipelines.iter().map(|value| i64::try_from(*value)
        .map_err(|_| "pipeline id exceeds SQLite range"))
        .collect::<Result<Vec<_>, _>>()?;
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            b.mark_brushprov_done(&pipelines, i64::from(code), done_at);
        }
    }
    Ok(())
}

/// Recipe fixup: after a $(shell) recipe finishes, the box sends the pipeline
/// uids and the recipe's start timestamp. We retroactively fix the
/// brush_pipeline_id on stderr output rows that the FUSE handler captured
/// during the recipe with a wrong (racy) attribution. The stderr flowed
/// through fd 2 normally for live backread — this just fixes the DB linkage.
fn recipe_fixup(
    state: &State,
    pipelines: &[u64],
    started_at: f64,
    peer_pidfd: Option<i32>,
) -> Result<(), String> {
    let id = enclosing_box_from_pidfd(state, peer_pidfd)?;
    let pipelines = pipelines.iter().map(|value| i64::try_from(*value)
        .map_err(|_| "pipeline id exceeds SQLite range"))
        .collect::<Result<Vec<_>, _>>()?;
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            let pipeline_id = pipelines
                .iter()
                .map(|u| b.brushprov_id_for_uid(*u))
                .find(|id| *id > 0)
                .unwrap_or(0);
            if pipeline_id > 0 {
                b.fixup_output_attribution(started_at, pipeline_id);
            }
        }
    }
    Ok(())
}

/// Phase 1 embedded-ninja `build_edges` verb. The shadowed `ninja` (vendored n2,
/// in-process) sends ONE message carrying the FULL parsed build graph — every
/// edge {outs, ins, cmd}, INCLUDING up-to-date targets that never execute — plus
/// ITS OWN pidfd as SCM_RIGHTS. We resolve the enclosing box from /proc ancestry
/// (the same path register/brush_prov_nested use) and store each edge in the
/// box's `build_edges` table. One-shot control reply; not a box channel.
fn build_edges(
    state: &State,
    edges: &[crate::generated_wire::BuildEdge],
    peer_pidfd: Option<i32>,
) -> Result<u64, String> {
    let id = enclosing_box_from_pidfd(state, peer_pidfd)?;
    let path = |value: &crate::generated_wire::Path| std::str::from_utf8(value.as_slice())
        .map(str::to_owned).map_err(|_| "build graph path is not UTF-8".to_owned());
    let edges = edges.iter().map(|edge| {
        let outputs = edge.outputs.as_slice().iter().map(&path)
            .collect::<Result<Vec<_>, _>>()?;
        let inputs = edge.inputs.as_slice().iter().map(&path)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((serde_json::to_string(&outputs).map_err(|error| error.to_string())?,
            serde_json::to_string(&inputs).map_err(|error| error.to_string())?,
            edge.command.as_ref().map(|value| value.as_str().to_owned())))
    }).collect::<Result<Vec<_>, String>>()?;
    let ov = lock(state).overlay.clone();
    let mut count = 0u64;
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            for (outputs, inputs, command) in &edges {
                b.add_build_edge(outputs, inputs, command.as_deref());
                count += 1;
            }
        }
    }
    if let Ok(r#box) = u64::try_from(id) {
        broadcast(state, &crate::generated_wire::SubscriptionEvent::BuildGraphChanged {
            r#box, phase: crate::generated_wire::EdgePhase::Rebuilt,
        });
    }
    Ok(count)
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
fn make_vars(
    state: &State,
    rows: &[crate::generated_wire::MakeVariable],
    peer_pidfd: Option<i32>,
) -> Result<(), String> {
    let id = enclosing_box_from_pidfd(state, peer_pidfd)?;
    let string = |value: &[u8], field: &str| std::str::from_utf8(value)
        .map(str::to_owned).map_err(|_| format!("make variable {field} is not UTF-8"));
    let rows = rows.iter().map(|row| Ok(MakeVarRow {
        name: string(row.name.as_slice(), "name")?,
        loc: string(row.location.as_slice(), "location")?,
        value: string(row.value.as_slice(), "value")?,
        make_dir: string(row.make_directory.as_slice(), "directory")?,
        edge_out: row.edge_output.as_ref().map(|value|
            string(value.as_slice(), "edge output")).transpose()?,
        uid: i64::try_from(row.pipeline).map_err(|_| "make variable pipeline exceeds i64")?,
        rhs: string(row.rhs.as_slice(), "rhs")?,
        refs: string(row.references.as_slice(), "references")?,
        flags: row.flags.as_str().to_owned(),
    })).collect::<Result<Vec<_>, String>>()?;
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            b.add_makevars(&rows);
        }
    }
    Ok(())
}

/// `box_activity` frame: the box's live in-flight builtin work (kati
/// recipes / $(shell) / parse phases with ages) — stored ephemerally on the
/// BoxState for the UI's "what is it doing" feed.
fn box_activity(
    state: &State,
    items: &[crate::generated_wire::ActivityItem],
    peer_pidfd: Option<i32>,
) -> Result<(), String> {
    let id = enclosing_box_from_pidfd(state, peer_pidfd)?;
    let items = items.iter().map(|item|
        (item.description.as_str().to_owned(), item.age_seconds)).collect();
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            *b.activity.lock().map_err(|_| "box activity lock poisoned")? = items;
        }
    }
    Ok(())
}

fn build_edge_state(
    state: &State,
    transition: &crate::generated_wire::BuildEdgeTransition,
    peer_pidfd: Option<i32>,
) -> Result<(), String> {
    use crate::generated_wire::{BuildEdgeTransition, EdgePhase, SubscriptionEvent};
    fn text_path(value: &crate::generated_wire::Path) -> Result<&str, String> {
        std::str::from_utf8(value.as_slice())
            .map_err(|_| "build edge output is not UTF-8".into())
    }
    let id = enclosing_box_from_pidfd(state, peer_pidfd)?;
    let ov = lock(state).overlay.clone();
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            match transition {
                BuildEdgeTransition::Start { at, output, command } => {
                    let output = output.as_ref().map(text_path).transpose()?;
                    b.mark_build_edge_started(output, command.as_ref().map(|value| value.as_str()), *at);
                }
                BuildEdgeTransition::Done { at, output, command, code, excerpt } => {
                    let output = output.as_ref().map(text_path).transpose()?;
                    b.mark_build_edge_done(output, command.as_ref().map(|value| value.as_str()),
                        i64::from(*code), *at, excerpt.as_ref().map(|value| value.as_str()));
                }
            }
        }
    }
    let r#box = u64::try_from(id).map_err(|_| "negative enclosing box id")?;
    let phase = match transition {
        BuildEdgeTransition::Start { .. } => EdgePhase::Started,
        BuildEdgeTransition::Done { .. } => EdgePhase::Done,
    };
    broadcast(state, &SubscriptionEvent::BuildGraphChanged { r#box, phase });
    Ok(())
}

fn dispatch_reply_transport(
    state: &State,
    request: crate::generated_wire::TransportRequest,
    hint_box_id: Option<i64>,
    peer_pidfd: Option<i32>,
) -> Result<crate::generated_wire::TransportResponse, String> {
    use crate::generated_wire::{BoxTarget, TransportRequest, TransportResponse};
    match request {
        TransportRequest::BrushProvenance { records } => {
            brush_prov_nested(state, records.as_slice(), peer_pidfd)
                .map(|count| TransportResponse::Recorded { count })
        }
        TransportRequest::BrushDone { pipelines, code, done_at } => {
            brush_prov_done(state, pipelines.as_slice(), code, done_at, peer_pidfd)?;
            Ok(TransportResponse::Empty)
        }
        TransportRequest::RecipeStarted { pipelines, started_at } => {
            recipe_fixup(state, pipelines.as_slice(), started_at, peer_pidfd)?;
            Ok(TransportResponse::Empty)
        }
        TransportRequest::BuildGraph { edges } => {
            build_edges(state, edges.as_slice(), peer_pidfd)
                .map(|count| TransportResponse::Recorded { count })
        }
        TransportRequest::MakeVariables { rows } => {
            make_vars(state, rows.as_slice(), peer_pidfd)?;
            Ok(TransportResponse::Empty)
        }
        TransportRequest::BoxActivity { items } => {
            box_activity(state, items.as_slice(), peer_pidfd)?;
            Ok(TransportResponse::Empty)
        }
        TransportRequest::BuildEdgeState { transition } => {
            build_edge_state(state, &transition, peer_pidfd)?;
            Ok(TransportResponse::Empty)
        }
        TransportRequest::BudgetGrant { target, amount } => {
            if let Some(fd) = peer_pidfd {
                unsafe { libc::close(fd); }
            }
            let id = match target {
                BoxTarget::Broker => hint_box_id,
                BoxTarget::Selector { r#box } => {
                    resolve(&discover::discover(), r#box.as_str())
                }
            }.ok_or("box not resolvable")?;
            crate::oaita::budget::grant(state, id, amount);
            let remaining = crate::oaita::budget::remaining(state, id).unwrap_or(0);
            Ok(TransportResponse::Budget { remaining })
        }
        TransportRequest::SudIngest { r#box } => {
            if let Some(fd) = peer_pidfd {
                unsafe { libc::close(fd); }
            }
            let id = resolve(&discover::discover(), r#box.as_str())
                .ok_or("no slopbox")?;
            let live = lock(state).overlay.clone().and_then(|overlay| overlay.live_box(id))
                .ok_or("box is not live")?;
            let runpid = u32::try_from(
                lock(state).box_runpids.get(&id).copied().unwrap_or(0))
                .map_err(|_| "negative box runner pid")?;
            let report = crate::sud::sweep(&live, id, runpid);
            let count = u64::try_from(report.ingested)
                .map_err(|_| "sud ingest count exceeds u64")?;
            let errors = report.errors.into_iter().map(|error|
                crate::wire::BoundedText::new(error)
                    .map_err(|_| "sud ingest error exceeds relation bound"))
                .collect::<Result<Vec<_>, _>>()?;
            let errors = crate::wire::BoundedVec::new(errors)
                .map_err(|_| "sud ingest error count exceeds relation bound")?;
            Ok(TransportResponse::SudIngested { count, errors })
        }
        other => {
            if let Some(fd) = peer_pidfd {
                unsafe { libc::close(fd); }
            }
            Err(format!("transport opcode {} is not a reply-mode request", other.code()))
        }
    }
}

fn legacy_transport_response(
    result: Result<crate::generated_wire::TransportResponse, String>,
) -> Value {
    use crate::generated_wire::TransportResponse;
    match result {
        Ok(TransportResponse::Empty) => json!({"ok": true}),
        Ok(TransportResponse::Error { category, message }) => json!({
            "ok": false,
            "category": match category {
                crate::generated_wire::ErrorCategory::InvalidRequest => "invalid_request",
                crate::generated_wire::ErrorCategory::NotFound => "not_found",
                crate::generated_wire::ErrorCategory::Conflict => "conflict",
                crate::generated_wire::ErrorCategory::Unavailable => "unavailable",
                crate::generated_wire::ErrorCategory::Unauthorized => "unauthorized",
                crate::generated_wire::ErrorCategory::Internal => "internal",
            },
            "error": message.into_inner(),
        }),
        Ok(TransportResponse::Recorded { count }) => {
            json!({"ok": true, "recorded": count})
        }
        Ok(TransportResponse::SudIngested { count, errors }) => json!({
            "ok": true,
            "ingested": count,
            "errors": errors.into_inner().into_iter()
                .map(crate::wire::BoundedText::into_inner).collect::<Vec<_>>(),
        }),
        Ok(TransportResponse::Budget { remaining }) => {
            json!({"ok": true, "remaining": remaining})
        }
        Ok(TransportResponse::Action { value }) => legacy_ui_action_reply(Ok(value)),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

pub fn broadcast(state: &State, event: &crate::generated_wire::SubscriptionEvent) {
    use crate::generated_wire::{EdgePhase, SubscriptionEvent};
    // The newline listener is still the active outer projection until the
    // coordinated client/server cutover. Event meaning is already closed and
    // typed here; this projection is deleted with that listener.
    let value = match event {
        SubscriptionEvent::BoxAdded { r#box, name, parent } => json!({
            "type": "session_added", "sid": r#box.to_string(),
            "name": name.as_ref().map(|value| value.as_str()), "parent": parent,
        }),
        SubscriptionEvent::BoxRemoved { r#box } => json!({
            "type": "session_removed", "session_id": r#box.to_string(),
        }),
        SubscriptionEvent::BoxRenamed { r#box, name } => json!({
            "type": "session_renamed", "session_id": r#box.to_string(),
            "name": name.as_str(),
        }),
        SubscriptionEvent::OverlayChanged { r#box, count, latest_path } => json!({
            "type": "changes", "session_id": r#box.to_string(), "count": count,
            "path": latest_path.as_ref().map(|path|
                String::from_utf8_lossy(path.as_slice()).into_owned()),
        }),
        SubscriptionEvent::ProcessAdded { r#box, count } => json!({
            "type": "process_added", "session_id": r#box.to_string(), "count": count,
        }),
        SubscriptionEvent::BrushProvenanceAdded { r#box, row } => json!({
            "type": "brush_prov", "session_id": r#box.to_string(),
            "brushprov_id": row,
        }),
        SubscriptionEvent::BuildGraphChanged { r#box, phase } => json!({
            "type": "build_edges", "session_id": r#box.to_string(),
            "edge_state": match phase {
                EdgePhase::Rebuilt => "rebuilt", EdgePhase::Started => "start",
                EdgePhase::Done => "done",
            },
        }),
        SubscriptionEvent::ApiLogAdded { r#box } => json!({
            "type": "api_log_added", "sid": r#box.to_string(),
        }),
        SubscriptionEvent::WebCaptureAdded { r#box } => json!({
            "type": "webcap_added", "sid": r#box.to_string(),
        }),
        SubscriptionEvent::Pong => json!({"type": "pong"}),
    };
    let data = format!("{value}\n");
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
        "register" => match legacy_register_request(msg) {
            Ok(request) => legacy_register_reply(register(state, &request, None, None, None)),
            Err(error) => json!({"ok": false, "error": error}),
        },
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
            let result = msg.get("sid").and_then(Value::as_str)
                .ok_or_else(|| "sud_ingest: missing box selector".to_string())
                .and_then(|value| crate::wire::BoundedText::new(value.to_owned())
                    .map_err(|_| "sud_ingest: box selector exceeds relation bound".to_string()))
                .map(|r#box| crate::generated_wire::TransportRequest::SudIngest { r#box })
                .and_then(|request| dispatch_reply_transport(state, request, None, None));
            legacy_transport_response(result)
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

fn action_relative_path(path: &crate::generated_wire::Path) -> Result<&str, String> {
    let bytes = path.as_slice();
    if bytes.contains(&0) {
        return Err("box path contains NUL".into());
    }
    let path = std::str::from_utf8(bytes)
        .map_err(|_| "the current overlay index cannot address a non-UTF-8 box path")?;
    if path.starts_with('/') || std::path::Path::new(path).components().any(|component| {
        matches!(component, std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_))
    }) {
        return Err("box path must be relative and cannot contain '..'".into());
    }
    Ok(path)
}

fn bounded_review_limit(limit: Option<u64>, default: u64) -> Result<u64, String> {
    let limit = limit.unwrap_or(default);
    (limit <= crate::generated_wire::LIMIT_COLLECTION_ITEMS as u64)
        .then_some(limit)
        .ok_or_else(|| "review limit exceeds relation collection bound".into())
}

fn relation_path_kind(kind: char) -> Result<crate::generated_wire::PathKind, String> {
    use crate::generated_wire::PathKind;
    match kind {
        '?' => Ok(PathKind::Missing),
        'f' => Ok(PathKind::File),
        'd' => Ok(PathKind::Directory),
        'l' => Ok(PathKind::Symlink),
        's' => Ok(PathKind::Special),
        other => Err(format!("unknown overlay path kind {other:?}")),
    }
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
                let result = crate::review::apply_typed(id, &[], &context)?;
                crate::generated_wire::ActionMutationResult {
                    r#box: sid,
                    count: result.applied.as_slice().len() as u64,
                    errors: result.errors,
                }
            } else {
                let result = crate::review::discard_typed(id, &[], &context)?;
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
                &crate::generated_wire::SubscriptionEvent::BoxRenamed {
                    r#box: sid, name: new.clone(),
                },
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
            broadcast(state, &crate::generated_wire::SubscriptionEvent::Pong);
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
        ActionRequest::BoxNew { parent_sid } => {
            let overlay = lock(state).overlay.clone()
                .ok_or("overlay not mounted")?;
            let parent = parent_sid.map(existing_box_id).transpose()?;
            let boxes = discover::discover();
            let id = boxes.keys().max().copied().unwrap_or(0)
                .max(overlay.box_ids().into_iter().max().unwrap_or(0)) + 1;
            let capture = crate::capture::BoxState::create(id)
                .map_err(|error| format!("box_new: {error}"))?;
            capture.set_parent(parent);
            if let Some(parent) = parent {
                capture.set_meta("parent_box_id", &parent.to_string());
            }
            overlay.add_box(std::sync::Arc::new(capture));
            broadcast(state, &crate::generated_wire::SubscriptionEvent::BoxAdded {
                r#box: u64::try_from(id).map_err(|_| "negative allocated box id")?,
                name: None,
                parent: parent.map(|value| u64::try_from(value)
                    .map_err(|_| "negative parent box id")).transpose()?,
            });
            let root = crate::paths::mnt_point().join(id.to_string());
            let root = crate::wire::BoundedBytes::new(root.as_os_str().as_bytes().to_vec())
                .map_err(|error| format!("box root path exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::BoxNew {
                value: crate::generated_wire::BoxCreated {
                    r#box: u64::try_from(id).map_err(|_| "negative allocated box id")?,
                    root,
                },
            })
        }
        ActionRequest::BoxDrop { sid } => {
            let id = existing_box_id(sid)?;
            let overlay = lock(state).overlay.clone()
                .ok_or("overlay not mounted")?;
            overlay.remove_box(id);
            Ok(ActionSuccess::BoxDrop { value: () })
        }
        ActionRequest::BoxFileRead { r#box, path } => {
            let id = existing_box_id(r#box)?;
            let overlay = lock(state).overlay.clone()
                .ok_or("overlay not available")?;
            let bytes = overlay.box_read_file(id, action_relative_path(&path)?)
                .map_err(|error| error.to_string())?;
            let value = crate::wire::BoundedBytes::new(bytes)
                .map_err(|error| format!("box file exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::BoxFileRead { value })
        }
        ActionRequest::BoxFileWrite { r#box, path, b64 } => {
            let id = existing_box_id(r#box)?;
            let overlay = lock(state).overlay.clone()
                .ok_or("overlay not available")?;
            let value = crate::review::write_file_checked_typed(
                id,
                action_relative_path(&path)?,
                b64.as_slice(),
                &overlay,
                true,
            )?;
            Ok(ActionSuccess::BoxFileWrite { value })
        }
        ActionRequest::BoxDirList { r#box, path } => {
            let id = existing_box_id(r#box)?;
            let overlay = lock(state).overlay.clone()
                .ok_or("overlay not available")?;
            let rows = overlay.box_list_dir(id, action_relative_path(&path)?)
                .map_err(|error| error.to_string())?.into_iter()
                .map(|(name, kind)| Ok(crate::generated_wire::DirectoryEntry {
                    name: crate::wire::BoundedBytes::new(name.into_bytes())
                        .map_err(|error| format!("directory name exceeds relation bound: {error:?}"))?,
                    kind: relation_path_kind(kind)?,
                }))
                .collect::<Result<Vec<_>, String>>()?;
            let value = crate::wire::BoundedVec::new(rows)
                .map_err(|error| format!("directory listing exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::BoxDirList { value })
        }
        ActionRequest::BoxPathKind { r#box, path } => {
            let id = existing_box_id(r#box)?;
            let overlay = lock(state).overlay.clone()
                .ok_or("overlay not available")?;
            let value = relation_path_kind(
                overlay.box_path_kind(id, action_relative_path(&path)?))?;
            Ok(ActionSuccess::BoxPathKind { value })
        }
        ActionRequest::ReviewFileBytes { sid, rel } => {
            let id = existing_box_id(sid)?;
            let bytes = crate::review::file_bytes_typed(id, action_relative_path(&rel)?)?;
            let value = crate::wire::BoundedBytes::new(bytes)
                .map_err(|error| format!("review file exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewFileBytes { value })
        }
        ActionRequest::ReviewWriteFile { sid, rel, b64 } => {
            let id = existing_box_id(sid)?;
            let overlay = lock(state).overlay.clone().ok_or("overlay not available")?;
            let value = crate::review::write_file_checked_typed(
                id, action_relative_path(&rel)?, b64.as_slice(), &overlay, false)?;
            Ok(ActionSuccess::ReviewWriteFile { value })
        }
        ActionRequest::ReviewPatchText { sid } => {
            let id = existing_box_id(sid)?;
            let value = crate::wire::BoundedBytes::new(crate::review::patch_text(id))
                .map_err(|error| format!("review patch exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewPatchText { value })
        }
        ActionRequest::ReviewChangeMode { sid, rel } => {
            let id = existing_box_id(sid)?;
            let value = crate::review::current_mode(id, action_relative_path(&rel)?);
            Ok(ActionSuccess::ReviewChangeMode { value })
        }
        ActionRequest::ReviewSessionChanges { sid } => {
            let id = existing_box_id(sid)?;
            let value = crate::wire::BoundedVec::new(
                crate::review::session_changes_typed(id)?)
                .map_err(|error| format!("change list exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewSessionChanges { value })
        }
        ActionRequest::ReviewFileGroups { sid } => {
            let id = existing_box_id(sid)?;
            let changes = crate::review::session_changes_typed(id)?;
            let groups = crate::overlay::file_groups().into_iter().map(|group| {
                let paths = changes.iter().filter(|change| {
                    std::str::from_utf8(change.path.as_slice()).ok()
                        .is_some_and(|path| group.matches(path))
                }).map(|change| change.path.clone()).collect::<Vec<_>>();
                Ok(crate::generated_wire::FileGroup {
                    name: crate::wire::BoundedText::new(group.name)
                        .map_err(|error| format!(
                            "file group name exceeds relation bound: {error:?}"))?,
                    count: u64::try_from(paths.len())
                        .map_err(|_| "file group count exceeds u64")?,
                    paths: crate::wire::BoundedVec::new(paths)
                        .map_err(|error| format!(
                            "file group paths exceed relation bound: {error:?}"))?,
                })
            }).collect::<Result<Vec<_>, String>>()?;
            let value = crate::wire::BoundedVec::new(groups)
                .map_err(|error| format!("file groups exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewFileGroups { value })
        }
        ActionRequest::ReviewHunks { sid, rel } => {
            let id = existing_box_id(sid)?;
            let value = crate::review::hunks_typed(id, action_relative_path(&rel)?)?;
            Ok(ActionSuccess::ReviewHunks { value })
        }
        ActionRequest::ReviewDecorate { sid, rel } => {
            let id = existing_box_id(sid)?;
            let value = crate::review::decorate_typed(id, action_relative_path(&rel)?)?;
            Ok(ActionSuccess::ReviewDecorate { value })
        }
        ActionRequest::ReviewDecorateMany { sid, rels } => {
            let id = existing_box_id(sid)?;
            let paths = rels.as_slice().iter().map(action_relative_path)
                .collect::<Result<Vec<_>, String>>()?;
            let value = crate::wire::BoundedVec::new(
                crate::review::decorate_many_typed(id, &paths)?)
                .map_err(|error| format!("decorations exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewDecorateMany { value })
        }
        ActionRequest::ReviewRecentChanges { sid, limit } => {
            let id = existing_box_id(sid)?;
            let limit = bounded_review_limit(limit, 200)?;
            let value = crate::wire::BoundedVec::new(
                crate::review::recent_changes_typed(id, limit)?)
                .map_err(|error| format!("recent changes exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewRecentChanges { value })
        }
        ActionRequest::ReviewBoxSummary { sid, limit } => {
            let id = existing_box_id(sid)?;
            let limit = bounded_review_limit(limit, 20)?;
            let mut value = crate::review::box_summary_typed(id, limit)?;
            let overlay = lock(state).overlay.clone();
            if let Some(r#box) = overlay.as_ref().and_then(|overlay| overlay.live_box(id)) {
                let activity = r#box.activity.lock()
                    .map_err(|_| "box activity lock poisoned")?.iter().map(
                        |(description, age_seconds)| Ok(crate::generated_wire::ActivityItem {
                            description: crate::wire::BoundedText::new(description.clone())
                                .map_err(|error| format!(
                                    "activity description exceeds relation bound: {error:?}"))?,
                            age_seconds: *age_seconds,
                        }),
                    ).collect::<Result<Vec<_>, String>>()?;
                value.activity = crate::wire::BoundedVec::new(activity)
                    .map_err(|error| format!("activity exceeds relation bound: {error:?}"))?;
            }
            Ok(ActionSuccess::ReviewBoxSummary { value })
        }
        ActionRequest::ReviewPipelineContext { sid, prov_id } => {
            let id = existing_box_id(sid)?;
            let value = crate::review::pipeline_context_typed(id, prov_id)?;
            Ok(ActionSuccess::ReviewPipelineContext { value })
        }
        ActionRequest::ReviewMakevars {
            sid, name_pat, value_pat, limit, any,
        } => {
            let id = existing_box_id(sid)?;
            let name_pat = name_pat.as_ref().map(|value| value.as_str()).unwrap_or("");
            let value_pat = value_pat.as_ref().map(|value| value.as_str()).unwrap_or("");
            let limit = bounded_review_limit(limit, 500)?;
            let value = crate::wire::BoundedVec::new(crate::review::makevars_typed(
                id, name_pat, value_pat, limit, any.unwrap_or(false))?)
                .map_err(|error| format!("make variables exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewMakevars { value })
        }
        ActionRequest::ReviewMapIds { sid, from, ids, to } => {
            let id = existing_box_id(sid)?;
            let value = crate::wire::BoundedVec::new(crate::review::map_ids_typed(
                id, from, ids.as_slice(), to)?)
                .map_err(|error| format!("mapped ids exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::ReviewMapIds { value })
        }
        request @ (ActionRequest::ReviewApply { sid, .. }
            | ActionRequest::ReviewDiscard { sid, .. }) => {
            let applying = matches!(&request, ActionRequest::ReviewApply { .. });
            let paths = match request {
                ActionRequest::ReviewApply { paths, .. }
                | ActionRequest::ReviewDiscard { paths, .. } => paths,
                _ => unreachable!(),
            };
            let id = existing_box_id(sid)?;
            let selected = paths.as_slice().iter().map(action_relative_path)
                .collect::<Result<Vec<_>, String>>()?;
            if box_is_running(state, id) {
                let message = crate::wire::BoundedText::new(
                    "box is running; stop it first".to_owned())
                    .map_err(|error| format!("review error exceeds relation bound: {error:?}"))?;
                let errors = crate::wire::BoundedVec::new(vec![
                    crate::generated_wire::PathError { path: None, message }])
                    .map_err(|error| format!("review errors exceed relation bound: {error:?}"))?;
                return Ok(if applying {
                    ActionSuccess::ReviewApply {
                        value: crate::generated_wire::ApplyResult {
                            applied: crate::wire::BoundedVec::new(Vec::new())
                                .map_err(|error| format!("review paths exceed bound: {error:?}"))?,
                            errors,
                        },
                    }
                } else {
                    ActionSuccess::ReviewDiscard {
                        value: crate::generated_wire::DiscardResult {
                            discarded: crate::wire::BoundedVec::new(Vec::new())
                                .map_err(|error| format!("review paths exceed bound: {error:?}"))?,
                            errors,
                        },
                    }
                });
            }
            let context = crate::review::NestCtx::new(lock(state).overlay.clone());
            let result = if applying {
                ActionSuccess::ReviewApply {
                    value: crate::review::apply_typed(id, &selected, &context)?,
                }
            } else {
                ActionSuccess::ReviewDiscard {
                    value: crate::review::discard_typed(id, &selected, &context)?,
                }
            };
            drop_if_empty(state, id);
            Ok(result)
        }
        ActionRequest::ReviewApplyHunk { sid, rel, hunk_ix } => {
            let id = existing_box_id(sid)?;
            if box_is_running(state, id) {
                return Err("box is running; stop it first".into());
            }
            crate::review::apply_hunk_typed(id, action_relative_path(&rel)?, hunk_ix)?;
            drop_if_empty(state, id);
            Ok(ActionSuccess::ReviewApplyHunk { value: () })
        }
        ActionRequest::ReviewDiscardHunk { sid, rel, hunk_ix } => {
            let id = existing_box_id(sid)?;
            if box_is_running(state, id) {
                return Err("box is running; stop it first".into());
            }
            crate::review::discard_hunk_typed(id, action_relative_path(&rel)?, hunk_ix)?;
            drop_if_empty(state, id);
            Ok(ActionSuccess::ReviewDiscardHunk { value: () })
        }
        request @ (ActionRequest::Delete { sid } | ActionRequest::Dissolve { sid }) => {
            let deleting = matches!(request, ActionRequest::Delete { .. });
            let id = existing_box_id(sid)?;
            let value = free_box_typed(state, id)?;
            Ok(if deleting {
                ActionSuccess::Delete { value }
            } else {
                ActionSuccess::Dissolve { value }
            })
        }
        ActionRequest::ApplyToCopy { sid } => {
            let id = existing_box_id(sid)?;
            let boxes = discover::discover();
            let value = apply_to_copy_typed(state, &boxes, id)?;
            Ok(ActionSuccess::ApplyToCopy { value })
        }
        ActionRequest::Kill { sid } => {
            let id = existing_box_id(sid)?;
            let pidfd = lock(state).box_pids.get(&id).copied()
                .ok_or("box not running")?;
            pidfd_signal(pidfd, libc::SIGTERM);
            Ok(ActionSuccess::Kill { value: () })
        }
        ActionRequest::Rotate { sid } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::Rotate {
                value: rotate_typed(state, id)?,
            })
        }
        ActionRequest::Stuck { sid } => {
            let id = existing_box_id(sid)?;
            Ok(ActionSuccess::Stuck {
                value: stuck_typed(state, id)?,
            })
        }
        ActionRequest::PromptsPeek => {
            let prompt = lock(state).net.clone()
                .and_then(|net| net.prompts.peek());
            let value = prompt.map(|prompt| -> Result<_, String> {
                Ok(crate::generated_wire::NetworkPrompt {
                    id: prompt.id,
                    r#box: crate::wire::BoundedText::new(prompt.box_name)
                        .map_err(|error| format!(
                            "prompt box name exceeds relation bound: {error:?}"))?,
                    host: crate::wire::BoundedText::new(prompt.host)
                        .map_err(|error| format!(
                            "prompt host exceeds relation bound: {error:?}"))?,
                    port: prompt.port,
                    scheme: crate::wire::BoundedText::new(prompt.scheme)
                        .map_err(|error| format!(
                            "prompt scheme exceeds relation bound: {error:?}"))?,
                })
            }).transpose()?;
            Ok(ActionSuccess::PromptsPeek { value })
        }
        ActionRequest::PromptsAnswer { id, verdict } => {
            use crate::generated_wire::PromptVerdict;
            let verdict = match verdict {
                PromptVerdict::YesOnce => crate::net::prompt::Verdict::YesOnce,
                PromptVerdict::NoOnce => crate::net::prompt::Verdict::NoOnce,
                PromptVerdict::AllowSave => crate::net::prompt::Verdict::AllowSave,
                PromptVerdict::DenySave => crate::net::prompt::Verdict::DenySave,
            };
            let net = lock(state).net.clone().ok_or("no net registry")?;
            Ok(ActionSuccess::PromptsAnswer {
                value: net.prompts.answer(id, verdict),
            })
        }
        ActionRequest::PromptsUiActive { bool: active } => {
            if let Some(net) = lock(state).net.clone() {
                net.prompts.mark_ui_active(active);
            }
            Ok(ActionSuccess::PromptsUiActive { value: () })
        }
        ActionRequest::FlowsList { sid } => {
            let id = action_flow_box(state, sid)?;
            let directory = flows_dir_for(id).ok_or("no flows dir for box")?;
            let value = crate::wire::BoundedVec::new(
                crate::net::flows::tshark_list(&directory)?)
                .map_err(|error| format!("flow rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::FlowsList { value })
        }
        ActionRequest::FlowsDetail { sid, frame } => {
            if frame == 0 {
                return Err("flow frame must be positive".into());
            }
            let id = action_flow_box(state, sid)?;
            let directory = flows_dir_for(id).ok_or("no flows dir for box")?;
            let value = crate::wire::BoundedText::new(
                crate::net::flows::tshark_detail(&directory, frame)?)
                .map_err(|error| format!("flow detail exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::FlowsDetail { value })
        }
        ActionRequest::FlowsPackets { sid, stream } => {
            let id = action_flow_box(state, sid)?;
            let stream = i64::try_from(stream).map_err(|_| "flow stream exceeds i64")?;
            let directory = flows_dir_for(id).ok_or("no flows dir for box")?;
            let value = crate::wire::BoundedVec::new(
                crate::net::flows::tshark_packets(&directory, stream)?)
                .map_err(|error| format!("packet rows exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::FlowsPackets { value })
        }
        ActionRequest::OaitaModels => {
            use crate::generated_wire::{ModelCatalog, ModelEntry};
            let (entries, source) = crate::oaita::models::catalog();
            let models = entries.into_iter().map(|entry| Ok(ModelEntry {
                name: crate::wire::BoundedText::new(entry.name)
                    .map_err(|error| format!("model name exceeds relation bound: {error:?}"))?,
                url: crate::wire::BoundedText::new(entry.url)
                    .map_err(|error| format!("model URL exceeds relation bound: {error:?}"))?,
                note: crate::wire::BoundedText::new(entry.note)
                    .map_err(|error| format!("model note exceeds relation bound: {error:?}"))?,
            })).collect::<Result<Vec<_>, String>>()?;
            let value = ModelCatalog {
                source: crate::wire::BoundedText::new(source)
                    .map_err(|error| format!("model source exceeds relation bound: {error:?}"))?,
                models: crate::wire::BoundedVec::new(models)
                    .map_err(|error| format!("model catalog exceeds relation bound: {error:?}"))?,
            };
            Ok(ActionSuccess::OaitaModels { value })
        }
        ActionRequest::OaitaStatus => {
            use crate::generated_wire::{OaitaStatus, OaitaStatusKind};
            let host_cfg = crate::oaita::config::Config::load();
            let external = host_cfg.model.as_deref()
                .is_some_and(|model| !model.trim().is_empty());
            let local = service_declared("oaita-local");
            let (kind, model, endpoint) = if external {
                (OaitaStatusKind::External,
                 host_cfg.model.unwrap_or_default(),
                 host_cfg.base_url.unwrap_or_default())
            } else if local {
                (OaitaStatusKind::Local, "local".to_owned(), "svc://oaita-local".to_owned())
            } else {
                (OaitaStatusKind::None, String::new(), String::new())
            };
            let value = OaitaStatus {
                kind,
                model: crate::wire::BoundedText::new(model)
                    .map_err(|error| format!("OAITA model exceeds relation bound: {error:?}"))?,
                endpoint: crate::wire::BoundedText::new(endpoint)
                    .map_err(|error| format!("OAITA endpoint exceeds relation bound: {error:?}"))?,
                serving: svc_has("oaita-local"),
            };
            Ok(ActionSuccess::OaitaStatus { value })
        }
        ActionRequest::OaitaProbe { spec } => {
            let base_url = if spec.base_url.as_str().is_empty() {
                "https://api.openai.com/v1"
            } else {
                spec.base_url.as_str()
            };
            if spec.model.as_str().trim().is_empty() {
                return Err("set a model name first".into());
            }
            crate::oaita::client::block_on(async {
                let client = crate::oaita::client::Client::from_resolved(
                    base_url, spec.api_key.as_str())?;
                let body = json!({
                    "model": spec.model.as_str(),
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1,
                    "stream": false,
                });
                client.post("/chat/completions", body).await
            }).map_err(|error| format!("{base_url}: {error}"))?;
            let detail = format!("connected · {} @ {base_url}", spec.model.as_str());
            let value = crate::wire::BoundedText::new(detail)
                .map_err(|error| format!("probe result exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::OaitaProbe { value })
        }
        ActionRequest::SvcUp { name } => {
            Ok(ActionSuccess::SvcUp { value: svc_has(name.as_str()) })
        }
        ActionRequest::OciLoad { reference, name } => {
            let outcome = crate::oci::load_blocking(
                reference.as_str(), name.map(|value| value.into_inner()))
                .map_err(|error| format!("{error:#}"))?;
            let value = crate::generated_wire::OciLoadResult {
                base: u64::try_from(outcome.base_id).map_err(|_| "negative OCI base box id")?,
                base_name: crate::wire::BoundedText::new(outcome.base_name)
                    .map_err(|error| format!("OCI base name exceeds relation bound: {error:?}"))?,
                top: u64::try_from(outcome.top_id).map_err(|_| "negative OCI top box id")?,
                top_name: crate::wire::BoundedText::new(outcome.top_name)
                    .map_err(|error| format!("OCI top name exceeds relation bound: {error:?}"))?,
                layer_count: u32::try_from(outcome.n_layers)
                    .map_err(|_| "OCI layer count exceeds u32")?,
                verified: outcome.verified,
            };
            Ok(ActionSuccess::OciLoad { value })
        }
        ActionRequest::OciImages => {
            let rows = crate::discover::discover().into_iter()
                .filter(|(_, image)| image.meta.contains_key("oci_config"))
                .map(|(id, image)| {
                    let reference = image.meta.get("oci_reference")
                        .ok_or_else(|| format!("OCI image box {id} has no reference"))?;
                    Ok(crate::generated_wire::OciImage {
                        top: u64::try_from(id).map_err(|_| "negative OCI image box id")?,
                        name: crate::wire::BoundedText::new(image.name)
                            .map_err(|error| format!(
                                "OCI image name exceeds relation bound: {error:?}"))?,
                        reference: crate::wire::BoundedText::new(reference.clone())
                            .map_err(|error| format!(
                                "OCI reference exceeds relation bound: {error:?}"))?,
                        digest: crate::wire::BoundedText::new(image.meta
                            .get("oci_manifest_digest").cloned().unwrap_or_default())
                            .map_err(|error| format!(
                                "OCI digest exceeds relation bound: {error:?}"))?,
                    })
                }).collect::<Result<Vec<_>, String>>()?;
            let value = crate::wire::BoundedVec::new(rows)
                .map_err(|error| format!("OCI image list exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::OciImages { value })
        }
        ActionRequest::OciResolve { reference } => {
            let (top, note) = crate::oci::resolve_image_top_local(reference.as_str())
                .map_err(|error| format!("{error:#}"))?;
            let value = crate::generated_wire::OciResolveResult {
                top: u64::try_from(top).map_err(|_| "negative OCI top box id")?,
                note: crate::wire::BoundedText::new(note)
                    .map_err(|error| format!("OCI resolution note exceeds relation bound: {error:?}"))?,
            };
            Ok(ActionSuccess::OciResolve { value })
        }
        ActionRequest::OciBuild { spec } => {
            let value = crate::oci::build_in_engine(&spec)
                .map_err(|error| format!("{error:#}"))?;
            Ok(ActionSuccess::OciBuild { value })
        }
        ActionRequest::RoAttach { r#box, attachments } => {
            use crate::generated_wire::ReadonlyAttachment;
            let overlay = lock(state).overlay.clone().ok_or("no overlay")?;
            let sid = i64::try_from(r#box).map_err(|_| "box id exceeds engine range")?;
            let owner = hydrate_box(&overlay, sid).ok_or("no such box")?;
            let mut rows = Vec::with_capacity(attachments.as_slice().len());
            for attachment in attachments.into_inner() {
                match attachment {
                    ReadonlyAttachment::Box { r#box } => {
                        let attached = i64::try_from(r#box)
                            .map_err(|_| "attached box id exceeds engine range")?;
                        if attached == sid {
                            return Err("cannot attach self".into());
                        }
                        if overlay.box_of(attached).is_none() {
                            let db = crate::paths::state_home().join(format!("{attached}.sqlar"));
                            if !db.exists() {
                                return Err(format!("no box {attached}"));
                            }
                            let state = crate::capture::BoxState::create(attached)
                                .map_err(|error| error.to_string())?;
                            state.load_mirror();
                            overlay.add_box(std::sync::Arc::new(state));
                        }
                        rows.push(crate::capture::RoAttachment::Box(attached));
                    }
                    ReadonlyAttachment::External { reference } => {
                        let store = std::str::from_utf8(reference.store.as_slice())
                            .map_err(|_| "the attachment store path is not UTF-8")?;
                        let prefix = action_relative_path(&reference.prefix)?;
                        rows.push(crate::capture::RoAttachment::Ext(crate::capture::ExtRef {
                            kind: reference.kind.into_inner(),
                            store: store.to_owned(),
                            refname: reference.reference.into_inner(),
                            rev: reference.revision.into_inner(),
                            prefix: prefix.to_owned(),
                            name: reference.name.into_inner(),
                        }));
                    }
                }
            }
            owner.set_ro_attachments(rows);
            overlay.invalidate_ext(sid);
            Ok(ActionSuccess::RoAttach { value: () })
        }
        ActionRequest::WikiAttach { sid, root, page, prefix } => {
            let overlay = lock(state).overlay.clone().ok_or("no overlay")?;
            let sid = i64::try_from(sid).map_err(|_| "box id exceeds engine range")?;
            let owner = hydrate_box(&overlay, sid).ok_or("no such box")?;
            let root = std::str::from_utf8(root.as_slice())
                .map_err(|_| "the wiki root path is not UTF-8")?;
            let prefix = prefix.as_ref().map(action_relative_path)
                .transpose()?.unwrap_or("");
            let instance = open_wiki_instance(root)?;
            let (page_id, resolved_title) = match page.as_str().parse::<u64>() {
                Ok(id) => (id, None),
                Err(_) => match instance.page_by_title(page.as_str()).map_err(|e| e.to_string())? {
                    (Some(id), hits) => {
                        let title = hits.into_iter().find(|(candidate, _)| *candidate == id)
                            .map(|(_, title)| title);
                        (id, title)
                    }
                    (None, hits) if hits.is_empty() => {
                        return Err(format!("no page titled {:?}", page.as_str()));
                    }
                    (None, hits) => {
                        let candidates = hits.into_iter().map(|(id, title)| format!(
                            "{title} ({id})")).collect::<Vec<_>>();
                        return Err(format!("title {:?} is ambiguous: {}",
                            page.as_str(), candidates.join(", ")));
                    }
                },
            };
            let title = match resolved_title {
                Some(title) => title,
                None => instance.page_current_title(page_id).map_err(|e| e.to_string())?
                    .unwrap_or_else(|| format!("page-{page_id}")),
            };
            let head = instance.page_head(page_id).map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no page {page_id}"))?;
            let wiki = std::path::Path::new(root).file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "wiki".into());
            let name = format!("wiki:{wiki}/{title}@r{}", head.rev_id);
            let value = crate::generated_wire::WikiAttachmentResult {
                name: crate::wire::BoundedText::new(name.clone())
                    .map_err(|error| format!("wiki attachment name exceeds relation bound: {error:?}"))?,
                page: page_id,
                title: crate::wire::BoundedText::new(title.clone())
                    .map_err(|error| format!("wiki title exceeds relation bound: {error:?}"))?,
                revision: head.rev_id,
            };
            let mut rows = owner.ro_attachment_list();
            rows.push(crate::capture::RoAttachment::Ext(crate::capture::ExtRef {
                kind: "wiki".into(),
                store: root.to_owned(),
                refname: page_id.to_string(),
                rev: head.rev_id.to_string(),
                prefix: prefix.to_owned(),
                name: name.clone(),
            }));
            owner.set_ro_attachments(rows);
            overlay.invalidate_ext(sid);
            Ok(ActionSuccess::WikiAttach { value })
        }
        ActionRequest::IetfAttach { sid, root, draft, prefix } => {
            let overlay = lock(state).overlay.clone().ok_or("no overlay")?;
            let sid = i64::try_from(sid).map_err(|_| "box id exceeds engine range")?;
            let owner = hydrate_box(&overlay, sid).ok_or("no such box")?;
            let root = std::str::from_utf8(root.as_slice())
                .map_err(|_| "the IETF root path is not UTF-8")?;
            let prefix = prefix.as_ref().map(action_relative_path)
                .transpose()?.unwrap_or("");
            let mirror = ietf_mirror::Mirror::open_read(
                ietf_mirror::MirrorConfig::new(root.into()))
                .map_err(|error| error.to_string())?;
            let head_revision = mirror.head(draft.as_str())
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("no draft {}", draft.as_str()))?.rev;
            let name = format!("ietf:{}@{head_revision}", draft.as_str());
            let value = crate::generated_wire::IetfAttachmentResult {
                name: crate::wire::BoundedText::new(name.clone())
                    .map_err(|error| format!("IETF attachment name exceeds relation bound: {error:?}"))?,
                revision: crate::wire::BoundedText::new(head_revision.clone())
                    .map_err(|error| format!("IETF revision exceeds relation bound: {error:?}"))?,
            };
            let mut rows = owner.ro_attachment_list();
            rows.push(crate::capture::RoAttachment::Ext(crate::capture::ExtRef {
                kind: "ietf".into(),
                store: root.to_owned(),
                refname: draft.as_str().to_owned(),
                rev: head_revision.clone(),
                prefix: prefix.to_owned(),
                name: name.clone(),
            }));
            owner.set_ro_attachments(rows);
            overlay.invalidate_ext(sid);
            Ok(ActionSuccess::IetfAttach { value })
        }
        ActionRequest::GitCheckout { sid, store, r#ref, dest, subpath } => {
            let value = git_checkout_typed(state, sid, store, r#ref, dest, subpath)?;
            Ok(ActionSuccess::GitCheckout { value })
        }
        ActionRequest::MirrorJobs => {
            let value = crate::wire::BoundedVec::new(crate::mirrors::jobs_list_typed()?)
                .map_err(|error| format!("mirror jobs exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::MirrorJobs { value })
        }
        ActionRequest::MirrorAdd { kind, src, dest, interval_secs } => {
            let destination = std::str::from_utf8(dest.as_slice())
                .map_err(|_| "the mirror scheduler cannot address a non-UTF-8 destination")?;
            let interval = i64::try_from(interval_secs.unwrap_or(24 * 3600))
                .map_err(|_| "mirror interval exceeds i64")?;
            let id = crate::mirrors::job_add(kind.as_str(), src.as_str(), destination, interval)?;
            Ok(ActionSuccess::MirrorAdd {
                value: u64::try_from(id).map_err(|_| "negative allocated mirror job id")?,
            })
        }
        ActionRequest::MirrorRun { id } => {
            crate::mirrors::job_run(
                i64::try_from(id).map_err(|_| "mirror job id exceeds i64")?)?;
            Ok(ActionSuccess::MirrorRun { value: () })
        }
        ActionRequest::MirrorRunPending => {
            let ids = crate::mirrors::run_pending()?.into_iter()
                .map(|id| u64::try_from(id).map_err(|_| "negative mirror job id"))
                .collect::<Result<Vec<_>, _>>()?;
            let value = crate::wire::BoundedVec::new(ids)
                .map_err(|error| format!("started mirror jobs exceed relation bound: {error:?}"))?;
            Ok(ActionSuccess::MirrorRunPending { value })
        }
        ActionRequest::MirrorPause { id, paused } => {
            crate::mirrors::job_set_paused(
                i64::try_from(id).map_err(|_| "mirror job id exceeds i64")?, paused)?;
            Ok(ActionSuccess::MirrorPause { value: () })
        }
        ActionRequest::MirrorRm { id } => {
            let note = crate::mirrors::job_remove(
                i64::try_from(id).map_err(|_| "mirror job id exceeds i64")?)?;
            let value = crate::wire::BoundedText::new(note)
                .map_err(|error| format!("mirror removal note exceeds relation bound: {error:?}"))?;
            Ok(ActionSuccess::MirrorRm { value })
        }
        ActionRequest::StructQuick { sid, rel } => {
            let id = existing_box_id(sid)?;
            let value = crate::review::struct_quick(id, action_relative_path(&rel)?)?;
            Ok(ActionSuccess::StructQuick { value })
        }
        ActionRequest::StructFinish { job } => {
            let value = crate::review::struct_finish(job)?;
            Ok(ActionSuccess::StructFinish { value })
        }
        ActionRequest::StructCancel { job } => {
            crate::review::struct_cancel(job);
            Ok(ActionSuccess::StructCancel { value: () })
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

fn legacy_bounded_text<const MAXIMUM: usize>(
    value: String,
    field: &str,
) -> Result<crate::wire::BoundedText<MAXIMUM>, String> {
    crate::wire::BoundedText::new(value)
        .map_err(|_| format!("{field} exceeds relation bound"))
}

fn legacy_pipeline_provenance(record: crate::generated_wire::PipelineProvenance) -> Value {
    crate::discover::pipeline_provenance_json(&record)
}

fn legacy_change_kind(kind: crate::generated_wire::ChangeKind) -> &'static str {
    use crate::generated_wire::ChangeKind;
    match kind {
        ChangeKind::Changed => "changed",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Symlink => "symlink",
        ChangeKind::Created => "created",
        ChangeKind::Modified => "modified",
        ChangeKind::Xattr => "xattr",
        ChangeKind::Directory => "dir",
        ChangeKind::XattrOnly => "xattr-only",
    }
}

fn legacy_change_decoration(value: crate::generated_wire::ChangeDecoration) -> Value {
    json!({
        "is_text": value.is_text,
        "stale": value.stale,
        "kind": legacy_change_kind(value.kind),
    })
}

fn legacy_file_diff(value: crate::generated_wire::FileDiff) -> Value {
    use crate::generated_wire::FileDiff;
    match value {
        FileDiff::Text { hunks } => json!({
            "is_text": true,
            "hunks": hunks.into_inner().into_iter().map(|hunk| json!({
                "index": hunk.index,
                "lines": hunk.lines.into_inner().into_iter().map(|line| json!([
                    line.style.into_inner(), line.text.into_inner(),
                ])).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }),
        FileDiff::Deleted => json!({
            "is_text": false,
            "hunks": [],
            "diff": {"kind": "deleted"},
        }),
        FileDiff::Symlink { kind, target } => json!({
            "is_text": false,
            "hunks": [],
            "diff": {
                "kind": legacy_change_kind(kind),
                "diff": format!("symlink → {}", legacy_bytes(target)),
            },
        }),
        FileDiff::Binary { kind, content, content_before } => {
            let mut diff = json!({
                "kind": legacy_change_kind(kind),
                "content": base64::engine::general_purpose::STANDARD.encode(content.as_slice()),
            });
            if let Some(before) = content_before {
                diff["content_before"] = json!(
                    base64::engine::general_purpose::STANDARD.encode(before.as_slice()));
            }
            json!({"is_text": false, "hunks": [], "diff": diff})
        }
        FileDiff::Unavailable { message } => json!({
            "is_text": false,
            "hunks": [],
            "diff": {"kind": "error", "error": message.into_inner()},
        }),
    }
}

fn legacy_review_errors(
    errors: crate::wire::BoundedVec<
        crate::generated_wire::PathError,
        0,
        { crate::generated_wire::LIMIT_COLLECTION_ITEMS },
    >,
) -> Vec<Value> {
    errors.into_inner().into_iter().map(|error| json!({
        "path": error.path.map(legacy_bytes).unwrap_or_default(),
        "error": error.message.into_inner(),
    })).collect()
}

fn legacy_view_window(value: crate::generated_wire::ViewWindow) -> Value {
    use crate::generated_wire::{EchoStream, ViewWindow};
    match value {
        ViewWindow::Changes { start, total, rows } => json!({
            "start": start,
            "total": total,
            "rows": rows.into_inner().into_iter().map(|row| {
                let mut value = json!({
                    "path": legacy_bytes(row.path),
                    "name": legacy_bytes(row.name),
                    "kind": legacy_change_kind(row.kind),
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

fn legacy_path_kind(value: crate::generated_wire::PathKind) -> &'static str {
    use crate::generated_wire::PathKind;
    match value {
        PathKind::Missing => "?",
        PathKind::File => "f",
        PathKind::Directory => "d",
        PathKind::Symlink => "l",
        PathKind::Special => "s",
    }
}

fn legacy_mirror_state(value: crate::generated_wire::MirrorState) -> &'static str {
    use crate::generated_wire::MirrorState;
    match value {
        MirrorState::Running => "running",
        MirrorState::Paused => "paused",
        MirrorState::Pending => "pending",
        MirrorState::Stopped => "stopped",
        MirrorState::Error => "error",
        MirrorState::Completed => "completed",
        MirrorState::Scheduled => "scheduled",
    }
}

fn legacy_oaita_status_kind(value: crate::generated_wire::OaitaStatusKind) -> &'static str {
    use crate::generated_wire::OaitaStatusKind;
    match value {
        OaitaStatusKind::None => "none",
        OaitaStatusKind::External => "external",
        OaitaStatusKind::Local => "local",
    }
}

fn legacy_structural_lines(
    lines: crate::wire::BoundedVec<
        crate::generated_wire::StructuralLine,
        0,
        { crate::generated_wire::LIMIT_COLLECTION_ITEMS },
    >,
) -> Vec<Value> {
    lines.into_inner().into_iter().map(|line| json!([
        line.style.into_inner(),
        line.text.into_inner(),
    ])).collect()
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
        Ok(ActionSuccess::BoxNew { value }) => json!({
            "sid": value.r#box.to_string(),
            "root": legacy_bytes(value.root),
        }),
        Ok(ActionSuccess::BoxDrop { .. }) => json!({"ok": true}),
        Ok(ActionSuccess::BoxFileRead { value }) => {
            use base64::{Engine, prelude::BASE64_STANDARD};
            json!({"bytes": BASE64_STANDARD.encode(value.as_slice())})
        }
        Ok(ActionSuccess::BoxFileWrite { value }) => json!({"len": value}),
        Ok(ActionSuccess::BoxDirList { value }) => Value::Array(value.into_inner()
            .into_iter().map(|row| json!({
                "name": legacy_bytes(row.name),
                "kind": legacy_path_kind(row.kind),
            })).collect()),
        Ok(ActionSuccess::BoxPathKind { value }) => {
            json!({"kind": legacy_path_kind(value)})
        }
        Ok(ActionSuccess::ReviewFileBytes { value }) => json!({
            "ok": true,
            "b64": base64::engine::general_purpose::STANDARD.encode(value.as_slice()),
        }),
        Ok(ActionSuccess::ReviewWriteFile { value }) => json!({"ok": true, "len": value}),
        Ok(ActionSuccess::ReviewPatchText { value }) => json!({
            "__b": base64::engine::general_purpose::STANDARD.encode(value.as_slice()),
        }),
        Ok(ActionSuccess::ReviewChangeMode { value }) => value
            .map(|mode| json!(mode)).unwrap_or(Value::Null),
        Ok(ActionSuccess::ReviewSessionChanges { value }) => Value::Array(value.into_inner()
            .into_iter().map(|change| json!({
                "path": legacy_bytes(change.path),
                "kind": legacy_change_kind(change.kind),
                "size": change.size,
            })).collect()),
        Ok(ActionSuccess::ReviewFileGroups { value }) => json!({
            "ok": true,
            "groups": value.into_inner().into_iter().map(|group| json!({
                "name": group.name.into_inner(),
                "count": group.count,
                "paths": group.paths.into_inner().into_iter()
                    .map(legacy_bytes).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }),
        Ok(ActionSuccess::ReviewHunks { value }) => legacy_file_diff(value),
        Ok(ActionSuccess::ReviewDecorate { value }) => legacy_change_decoration(value),
        Ok(ActionSuccess::ReviewDecorateMany { value }) => Value::Array(value.into_inner()
            .into_iter().map(legacy_change_decoration).collect()),
        Ok(ActionSuccess::ReviewRecentChanges { value }) => Value::Array(value.into_inner()
            .into_iter().map(|change| json!({
                "path": legacy_bytes(change.path),
                "kind": legacy_change_kind(change.kind),
                "size": change.size,
            })).collect()),
        Ok(ActionSuccess::ReviewBoxSummary { value }) => json!({
            "outputs": value.outputs.into_inner().into_iter().map(|row| json!({
                "id": row.id,
                "ts": row.time,
                "stream": match row.stream {
                    crate::generated_wire::EchoStream::Stdout => 0,
                    crate::generated_wire::EchoStream::Stderr => 1,
                },
                "len": row.length,
                "preview": row.preview.into_inner(),
            })).collect::<Vec<_>>(),
            "changes": value.changes.into_inner().into_iter().map(|row| {
                let mut result = json!({
                    "path": legacy_bytes(row.path),
                    "kind": legacy_change_kind(row.kind),
                    "size": row.size,
                    "mtime": row.modified_at,
                });
                if let Some(key) = row.xattr_key {
                    result["xattr_key"] = json!(legacy_bytes(key));
                }
                if let Some(length) = row.xattr_length {
                    result["xattr_len"] = json!(length);
                }
                result
            }).collect::<Vec<_>>(),
            "processes": value.processes.into_inner().into_iter().map(|row| json!({
                "id": row.id,
                "tgid": row.tgid,
                "exe": legacy_bytes(row.executable),
                "argv0": legacy_bytes(row.argv0),
            })).collect::<Vec<_>>(),
            "pipelines": value.pipelines.into_inner().into_iter().map(|row| json!({
                "id": row.id,
                "cmd": row.command.into_inner(),
                "nested": row.nested,
            })).collect::<Vec<_>>(),
            "edges": value.edges.into_inner().into_iter().map(|row| json!({
                "id": row.id,
                "out": row.output.map(legacy_bytes).unwrap_or_default(),
                "n_outs": row.output_count,
                "cmd": row.command.map(|value| value.into_inner()).unwrap_or_default(),
            })).collect::<Vec<_>>(),
            "failures": value.failures.into_inner().into_iter().map(|row| json!({
                "kind": match row.kind {
                    crate::generated_wire::FailureKind::Edge => "edge",
                    crate::generated_wire::FailureKind::Pipeline => "pipeline",
                },
                "label": row.label.into_inner(),
                "code": row.code,
                "excerpt": row.excerpt.into_inner(),
            })).collect::<Vec<_>>(),
            "makevar": if value.has_make_variables { vec![1] } else { vec![] },
            "sudtrace": if value.has_sud_trace { vec![1] } else { vec![] },
            "activity": value.activity.into_inner().into_iter().map(|row| json!({
                "desc": row.description.into_inner(),
                "age": row.age_seconds,
            })).collect::<Vec<_>>(),
        }),
        Ok(ActionSuccess::ReviewPipelineContext { value }) => {
            let item = |item: crate::generated_wire::PipelineContextItem| json!({
                "id": item.id,
                "cmd": item.command.into_inner(),
                "exit_code": item.exit_code,
            });
            json!({
                "parent": value.parent.map(&item),
                "children": value.children.into_inner().into_iter().map(item)
                    .collect::<Vec<_>>(),
                "edge_out": value.edge_output.map(legacy_bytes).unwrap_or_default(),
            })
        }
        Ok(ActionSuccess::ReviewMakevars { value }) => Value::Array(value.into_inner()
            .into_iter().map(|row| json!({
                "id": row.id,
                "name": legacy_bytes(row.name),
                "loc": legacy_bytes(row.location),
                "value": legacy_bytes(row.value),
                "make": legacy_bytes(row.make_directory),
                "rhs": legacy_bytes(row.rhs),
                "refs": legacy_bytes(row.references),
                "flags": row.flags.into_inner(),
                "edge_out": row.edge_output.map(legacy_bytes),
                "uid": row.pipeline_uid,
                "edge_id": row.edge,
                "pipeline_id": row.pipeline,
            })).collect()),
        Ok(ActionSuccess::ReviewMapIds { value }) => json!(value.into_inner()),
        Ok(ActionSuccess::ReviewApply { value }) => json!({
            "applied": value.applied.into_inner().into_iter()
                .map(legacy_bytes).collect::<Vec<_>>(),
            "errors": legacy_review_errors(value.errors),
        }),
        Ok(ActionSuccess::ReviewDiscard { value }) => json!({
            "discarded": value.discarded.into_inner().into_iter()
                .map(legacy_bytes).collect::<Vec<_>>(),
            "errors": legacy_review_errors(value.errors),
        }),
        Ok(ActionSuccess::ReviewApplyHunk { .. })
        | Ok(ActionSuccess::ReviewDiscardHunk { .. }) => json!({"ok": true}),
        Ok(ActionSuccess::Delete { value }) | Ok(ActionSuccess::Dissolve { value }) => json!({
            "ok": true,
            "reparented": value.reparented.into_inner(),
        }),
        Ok(ActionSuccess::ApplyToCopy { value }) => json!({
            "ok": true,
            "new_sid": value.r#box.to_string(),
            "name": value.name.into_inner(),
            "applied": value.applied,
        }),
        Ok(ActionSuccess::Kill { .. }) => json!({"ok": true}),
        Ok(ActionSuccess::Rotate { value }) => json!({
            "ok": true,
            "parent": value.parent,
            "child": value.child,
        }),
        Ok(ActionSuccess::Stuck { value }) => json!({
            "ok": true,
            "runner": value.runner,
            "procs": value.threads.into_inner().into_iter().map(|thread| json!({
                "pid": thread.pid,
                "tid": thread.tid,
                "comm": thread.command.into_inner(),
                "state": thread.state.into_inner(),
                "wchan": thread.wait_channel.into_inner(),
                "syscall": thread.syscall.into_inner(),
                "detail": thread.detail.into_inner(),
                "bt": thread.backtrace.into_inner().into_iter()
                    .map(|frame| frame.into_inner()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }),
        Ok(ActionSuccess::PromptsPeek { value }) => json!({
            "ok": true,
            "ask": value.map(|prompt| json!({
                "id": prompt.id,
                "box": prompt.r#box.into_inner(),
                "host": prompt.host.into_inner(),
                "port": prompt.port,
                "scheme": prompt.scheme.into_inner(),
            })),
        }),
        Ok(ActionSuccess::PromptsAnswer { value }) => json!({"ok": value}),
        Ok(ActionSuccess::PromptsUiActive { .. }) => json!({"ok": true}),
        Ok(ActionSuccess::FlowsList { value }) => json!({
            "ok": true,
            "flows": value.into_inner().into_iter().map(|row| json!({
                "frame": row.frame,
                "t": row.time,
                "src": row.source.into_inner(),
                "dst": row.destination.into_inner(),
                "sni": row.sni.into_inner(),
                "host": row.host.into_inner(),
                "method": row.method.into_inner(),
                "uri": row.uri.into_inner(),
                "status": row.status.into_inner(),
                "stream": row.stream.map(|stream| stream as i64).unwrap_or(-1),
            })).collect::<Vec<_>>(),
        }),
        Ok(ActionSuccess::FlowsDetail { value }) => json!({
            "ok": true,
            "text": value.into_inner(),
        }),
        Ok(ActionSuccess::FlowsPackets { value }) => json!({
            "ok": true,
            "packets": value.into_inner().into_iter().map(|row| json!({
                "frame": row.frame,
                "t": row.time,
                "src": row.source.into_inner(),
                "dst": row.destination.into_inner(),
                "proto": row.protocol.into_inner(),
                "len": row.length,
                "info": row.summary.into_inner(),
            })).collect::<Vec<_>>(),
        }),
        Ok(ActionSuccess::OaitaModels { value }) => json!({
            "source": value.source.into_inner(),
            "models": value.models.into_inner().into_iter().map(|model| json!({
                "name": model.name.into_inner(),
                "url": model.url.into_inner(),
                "note": model.note.into_inner(),
            })).collect::<Vec<_>>(),
        }),
        Ok(ActionSuccess::OaitaStatus { value }) => json!({
            "kind": legacy_oaita_status_kind(value.kind),
            "model": value.model.into_inner(),
            "endpoint": value.endpoint.into_inner(),
            "serving": value.serving,
        }),
        Ok(ActionSuccess::OaitaProbe { value }) => json!({
            "ok": true,
            "detail": value.into_inner(),
        }),
        Ok(ActionSuccess::SvcUp { value }) => json!({"up": value}),
        Ok(ActionSuccess::OciLoad { value }) => json!({
            "base_id": value.base,
            "base_name": value.base_name.into_inner(),
            "top_id": value.top,
            "top_name": value.top_name.into_inner(),
            "n_layers": value.layer_count,
            "verified": value.verified,
        }),
        Ok(ActionSuccess::OciImages { value }) => Value::Array(value.into_inner()
            .into_iter().map(|image| json!({
                "id": image.top,
                "name": image.name.into_inner(),
                "reference": image.reference.into_inner(),
                "digest": image.digest.into_inner(),
            })).collect()),
        Ok(ActionSuccess::OciResolve { value }) => json!({
            "top_id": value.top,
            "note": value.note.into_inner(),
        }),
        Ok(ActionSuccess::OciBuild { value }) => json!({
            "code": value.code,
            "log": value.log.into_inner(),
            "top_id": value.top,
        }),
        Ok(ActionSuccess::RoAttach { .. }) => json!({"ok": true}),
        Ok(ActionSuccess::WikiAttach { value }) => json!({
            "ok": true,
            "name": value.name.into_inner(),
            "page": value.page,
            "title": value.title.into_inner(),
            "rev": value.revision,
        }),
        Ok(ActionSuccess::IetfAttach { value }) => json!({
            "ok": true,
            "name": value.name.into_inner(),
            "rev": value.revision.into_inner(),
        }),
        Ok(ActionSuccess::GitCheckout { value }) => json!({
            "ok": true,
            "sha": value.revision.into_inner(),
            "files": value.files,
            "bytes": value.bytes,
        }),
        Ok(ActionSuccess::MirrorJobs { value }) => Value::Array(value.into_inner()
            .into_iter().map(|job| json!({
                "id": job.id,
                "kind": job.kind.into_inner(),
                "src": job.source.into_inner(),
                "dest": legacy_bytes(job.destination),
                "interval_secs": job.interval_seconds,
                "paused": job.paused,
                "last_start": job.last_start,
                "last_end": job.last_end,
                "last_exit": job.last_exit,
                "last_detail": job.last_detail.into_inner(),
                "state": legacy_mirror_state(job.state),
                "next_due": job.next_due,
            })).collect()),
        Ok(ActionSuccess::MirrorAdd { value }) => json!({"ok": true, "id": value}),
        Ok(ActionSuccess::MirrorRun { .. }) | Ok(ActionSuccess::MirrorPause { .. }) => {
            json!({"ok": true})
        }
        Ok(ActionSuccess::MirrorRunPending { value }) => json!({
            "ok": true,
            "started": value.into_inner(),
        }),
        Ok(ActionSuccess::MirrorRm { value }) => json!({
            "ok": true,
            "note": value.into_inner(),
        }),
        Ok(ActionSuccess::StructQuick { value }) => json!({
            "lines": legacy_structural_lines(value.lines),
            "job": value.job,
        }),
        Ok(ActionSuccess::StructFinish { value }) => json!({
            "lines": legacy_structural_lines(value.lines),
        }),
        Ok(ActionSuccess::StructCancel { .. }) => json!({"ok": true, "r": Value::Null}),
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

fn action_flow_box(state: &State, requested: Option<u64>) -> Result<i64, String> {
    let sid = match requested {
        Some(sid) => sid,
        None => lock(state).selected.as_ref()
            .and_then(|sid| sid.parse::<u64>().ok())
            .ok_or("no box selected")?,
    };
    i64::try_from(sid).map_err(|_| "box id exceeds engine range".into())
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

fn legacy_provenance_domain(value: &str) -> Option<crate::generated_wire::ProvenanceDomain> {
    use crate::generated_wire::ProvenanceDomain;
    match value {
        "process" => Some(ProvenanceDomain::Process),
        "pipeline" => Some(ProvenanceDomain::Pipeline),
        "edge" => Some(ProvenanceDomain::Edge),
        _ => None,
    }
}

fn legacy_path_arg(args: &[Value], index: usize) -> Option<crate::generated_wire::Path> {
    crate::wire::BoundedBytes::new(args.get(index)?.as_str()?.as_bytes().to_vec()).ok()
}

fn legacy_path_list(value: Option<&Value>) -> Result<crate::wire::BoundedVec<
    crate::generated_wire::Path,
    0,
    { crate::generated_wire::LIMIT_COLLECTION_ITEMS },
>, String> {
    let values = match value {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(values)) => values.as_slice(),
        Some(_) => return Err("paths must be an array".into()),
    };
    let paths = values.iter().map(|value| {
        let path = value.as_str().ok_or("path must be text")?;
        crate::wire::BoundedBytes::new(path.as_bytes().to_vec())
            .map_err(|error| format!("path exceeds relation bound: {error:?}"))
    }).collect::<Result<Vec<_>, String>>()?;
    crate::wire::BoundedVec::new(paths)
        .map_err(|error| format!("path list exceeds relation bound: {error:?}"))
}

fn legacy_transport_paths<const MINIMUM: usize>(
    value: Option<&Value>,
) -> Result<crate::wire::BoundedVec<
    crate::generated_wire::Path,
    MINIMUM,
    { crate::generated_wire::LIMIT_COLLECTION_ITEMS },
>, String> {
    let values = value.and_then(Value::as_array)
        .ok_or("build edge paths must be an array")?;
    let paths = values.iter().map(|value| {
        let path = value.as_str().ok_or("build edge path must be text")?;
        crate::wire::BoundedBytes::new(path.as_bytes().to_vec())
            .map_err(|_| "build edge path exceeds relation bound")
    }).collect::<Result<Vec<_>, _>>()?;
    crate::wire::BoundedVec::new(paths)
        .map_err(|error| format!("build edge paths exceed relation bound: {error:?}"))
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
    if let Ok(r#box) = u64::try_from(id) {
        broadcast(state, &crate::generated_wire::SubscriptionEvent::BoxRemoved { r#box });
    }
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
fn free_box_typed(
    state: &State,
    id: i64,
) -> Result<crate::generated_wire::FreeResult, String> {
    let boxes = discover::discover();
    let me = boxes.get(&id).ok_or("no slopbox")?;
    let grandparent = me.parent;
    let children: Vec<i64> = boxes
        .values()
        .filter(|b| b.parent == Some(id))
        .map(|b| b.box_id)
        .collect();
    if lock(state).box_pids.contains_key(&id) && !children.is_empty() {
        return Err("box is running; stop it first".into());
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
                    return Err(format!("copy-down to box {child} failed at {rel}: {e}"));
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
    let children = children.into_iter()
        .map(|child| u64::try_from(child).map_err(|_| "negative child box id"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(crate::generated_wire::FreeResult {
        reparented: crate::wire::BoundedVec::new(children)
            .map_err(|error| format!("reparented boxes exceed relation bound: {error:?}"))?,
    })
}

/// Apply box `id`'s changes onto a fresh COPY of its parent, leaving the real
/// parent (and its other children) untouched. Composes existing primitives:
/// create a new box beside the parent (child of the grandparent), copy the
/// parent's own changes into it (so it starts as a snapshot of the parent),
/// then promote `id`'s changes on top. The result is a new sibling box holding
/// "parent + id's changes"; nothing else in the tree moves.
fn apply_to_copy_typed(
    state: &State,
    boxes: &std::collections::BTreeMap<i64, discover::Box_>,
    id: i64,
) -> Result<crate::generated_wire::ApplyCopyResult, String> {
    let me = boxes.get(&id).ok_or("no slopbox")?;
    let parent = me.parent.ok_or(
        "box has no parent box to copy (a top-level box applies to the host)")?;
    if box_is_running(state, id) || box_is_running(state, parent) {
        return Err("box or its parent is running; stop it first".into());
    }
    let grandparent = boxes.get(&parent).and_then(|b| b.parent);
    let ov = lock(state).overlay.clone().ok_or("overlay not mounted")?;
    let new_id = boxes.keys().max().copied().unwrap_or(0)
        .max(ov.box_ids().into_iter().max().unwrap_or(0)) + 1;
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
    let created = crate::capture::BoxState::create(new_id)
        .map_err(|error| format!("create copy box: {error}"))?;
    created.set_parent(grandparent);
    if let Some(gp) = grandparent {
        created.set_meta("parent_box_id", &gp.to_string());
    }
    created.set_meta("name", &new_name);
    ov.add_box(std::sync::Arc::new(created));
    // 2. Copy the parent's OWN changes into the copy (snapshot of the parent).
    for rel in crate::review::changed_paths(parent) {
        crate::review::copy_down_entry(parent, new_id, &rel, None)
            .map_err(|error| format!("copy parent '{rel}': {error}"))?;
    }
    // 3. Promote this box's changes onto the copy.
    let mut applied = 0usize;
    for rel in crate::review::changed_paths(id) {
        crate::review::promote_into_parent(id, new_id, None, &rel)
            .map_err(|error| format!("apply '{rel}' onto copy: {error}"))?;
        applied += 1;
    }
    if let (Ok(r#box), Ok(name)) = (u64::try_from(new_id),
        crate::wire::BoundedText::new(new_name.clone()))
    {
        broadcast(state, &crate::generated_wire::SubscriptionEvent::BoxAdded {
            r#box, name: Some(name),
            parent: grandparent.and_then(|value| u64::try_from(value).ok()),
        });
    }
    Ok(crate::generated_wire::ApplyCopyResult {
        r#box: u64::try_from(new_id).map_err(|_| "negative allocated box id")?,
        name: crate::wire::BoundedText::new(new_name)
            .map_err(|error| format!("copy name exceeds relation bound: {error:?}"))?,
        applied: u64::try_from(applied).map_err(|_| "applied path count exceeds u64")?,
    })
}

/// Promote a child above its parent by rewriting the two recorded layer
/// encodings. The merged filesystem view remains unchanged.
fn rotate_typed(
    state: &State,
    child_id: i64,
) -> Result<crate::generated_wire::RotateResult, String> {
    let (overlay, run_pids) = {
        let shared = lock(state);
        (shared.overlay.clone().ok_or("overlay not mounted")?,
         shared.box_runpids.clone())
    };
    if overlay.box_of(child_id).is_none() {
        if !crate::paths::state_home().join(format!("{child_id}.sqlar")).exists() {
            return Err("no such box".into());
        }
        let child = crate::capture::BoxState::create(child_id)
            .map_err(|error| error.to_string())?;
        child.load_mirror();
        overlay.add_box(std::sync::Arc::new(child));
    }
    let child = overlay.box_of(child_id).ok_or("no such box")?;
    let parent_id = child.parent().ok_or("box has no parent")?;
    let parent = overlay.box_of(parent_id).ok_or("parent not hydrated")?;
    if run_pids.contains_key(&child_id) || run_pids.contains_key(&parent_id) {
        return Err("rotate needs both boxes at rest".into());
    }

    let mut ancestor_layers = Vec::new();
    let mut current = parent.parent();
    let mut hops = 0;
    while let Some(id) = current {
        hops += 1;
        if hops > 64 {
            break;
        }
        let Some(row) = overlay.box_of(id) else {
            break;
        };
        let connection = row.conn.lock().unwrap();
        ancestor_layers.push(crate::depot::export_layer(&connection, id)?);
        drop(connection);
        current = row.parent();
    }
    ancestor_layers.reverse();
    let parent_layer = {
        let connection = parent.conn.lock().unwrap();
        crate::depot::export_layer(&connection, parent_id)?
    };
    let child_layer = {
        let connection = child.conn.lock().unwrap();
        crate::depot::export_layer(&connection, child_id)?
    };
    let ancestor_refs = ancestor_layers.iter().collect::<Vec<_>>();
    let (new_child_layer, new_parent_layer) =
        depot_model::rotate(&ancestor_refs, &parent_layer, &child_layer);
    {
        let connection = child.conn.lock().unwrap();
        crate::depot::archive_clear(&connection, child_id)
            .and_then(|_| crate::depot::import_layer(
                &connection, child_id, &new_child_layer))?;
    }
    {
        let connection = parent.conn.lock().unwrap();
        crate::depot::archive_clear(&connection, parent_id)
            .and_then(|_| crate::depot::import_layer(
                &connection, parent_id, &new_parent_layer))
            .map_err(|error| format!(
                "parent import after child rewrite: {error}"))?;
    }

    let grandparent = parent.parent();
    child.set_parent(grandparent);
    child.set_meta(
        "parent_box_id",
        &grandparent.map(|id| id.to_string()).unwrap_or_default(),
    );
    parent.set_parent(Some(child_id));
    parent.set_meta("parent_box_id", &child_id.to_string());
    child.load_mirror();
    parent.load_mirror();
    Ok(crate::generated_wire::RotateResult {
        parent: u64::try_from(child_id).map_err(|_| "negative child box id")?,
        child: u64::try_from(parent_id).map_err(|_| "negative parent box id")?,
    })
}

fn stuck_typed(
    state: &State,
    id: i64,
) -> Result<crate::generated_wire::StuckReport, String> {
    use crate::generated_wire::{StuckReport, StuckThread};
    let runner = lock(state).box_runpids.get(&id).copied()
        .ok_or("box not running")?;
    let state_of = |path: &str| std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.rfind(')')
            .and_then(|index| text[index + 1..].trim().chars().next())
            .map(|state| state.to_string()))
        .unwrap_or_default();

    let mut box_pids = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str()
                .and_then(|text| text.parse::<i32>().ok()) else { continue };
            let mut current = pid;
            for _ in 0..64 {
                if current == runner {
                    box_pids.push(pid);
                    break;
                }
                let parent = ppid_of(current);
                if parent <= 1 {
                    break;
                }
                current = parent;
            }
        }
    }

    let mut file_descriptors: std::collections::HashMap<
        i32, std::collections::HashMap<i32, String>> = Default::default();
    let mut holders: std::collections::HashMap<String, Vec<i32>> = Default::default();
    for &pid in &box_pids {
        let mut table = std::collections::HashMap::new();
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for entry in entries.flatten() {
                let Some(number) = entry.file_name().to_str()
                    .and_then(|text| text.parse::<i32>().ok()) else { continue };
                if let Ok(target) = std::fs::read_link(entry.path()) {
                    let target = target.to_string_lossy().into_owned();
                    if target.starts_with("pipe:") || target.starts_with("socket:") {
                        holders.entry(target.clone()).or_default().push(pid);
                    }
                    table.insert(number, target);
                }
            }
        }
        file_descriptors.insert(pid, table);
    }

    let mut backtraces = thread_backtraces(&box_pids, 8);
    for frames in backtraces.values_mut() {
        frames.retain(|frame| !frame.starts_with("?? "));
    }
    backtraces.retain(|_, frames| !frames.is_empty());
    for (tid, frames) in selfbt_backtraces(runner, &box_pids) {
        if !frames.is_empty() {
            backtraces.insert(tid, frames);
        }
    }
    for (tid, frames) in ptrace_backtraces(&box_pids) {
        if !frames.is_empty() {
            backtraces.insert(tid, frames);
        }
    }

    let short = |value: String| -> Result<
        crate::wire::BoundedText<{ crate::generated_wire::LIMIT_SHORT_BYTES }>, String> {
        crate::wire::BoundedText::new(value)
            .map_err(|error| format!("stuck short text exceeds relation bound: {error:?}"))
    };
    let text = |value: String| -> Result<
        crate::wire::BoundedText<{ crate::generated_wire::LIMIT_TEXT_BYTES }>, String> {
        crate::wire::BoundedText::new(value)
            .map_err(|error| format!("stuck text exceeds relation bound: {error:?}"))
    };
    let mut threads = Vec::new();
    for &pid in &box_pids {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for task in tasks.flatten() {
            let Some(tid) = task.file_name().to_str()
                .and_then(|value| value.parse::<i32>().ok()) else { continue };
            let base = format!("/proc/{pid}/task/{tid}");
            let read = |name: &str| std::fs::read_to_string(format!("{base}/{name}"))
                .unwrap_or_default().trim().to_string();
            let command = read("comm");
            let wait_channel = read("wchan");
            let thread_state = state_of(&format!("{base}/stat"));
            let syscall_raw = read("syscall");
            let syscall_fields = syscall_raw.split_whitespace().collect::<Vec<_>>();
            let number = syscall_fields.first().and_then(|field| field.parse::<i64>().ok());
            let detail = match number {
                Some(number) if number >= 0 => {
                    let name = syscall_name(number);
                    let name = if name.is_empty() {
                        format!("sys{number}")
                    } else {
                        name.to_string()
                    };
                    if syscall_arg0_is_fd(number) {
                        let descriptor = syscall_fields.get(1).and_then(|field|
                            i64::from_str_radix(field.trim_start_matches("0x"), 16).ok());
                        match descriptor {
                            Some(descriptor) => {
                                let target = file_descriptors.get(&pid)
                                    .and_then(|table| table.get(&(descriptor as i32)))
                                    .cloned().unwrap_or_else(|| "?".into());
                                let mut detail = format!(
                                    "{name}(fd {descriptor} → {target})");
                                if let Some(processes) = holders.get(&target) {
                                    let peers = processes.iter().filter(|peer| **peer != pid)
                                        .map(ToString::to_string).collect::<Vec<_>>();
                                    if !peers.is_empty() {
                                        detail.push_str(&format!(
                                            "  peer pid {}", peers.join(",")));
                                    }
                                }
                                detail
                            }
                            None => format!("{name}()"),
                        }
                    } else {
                        format!("{name}()")
                    }
                }
                _ if wait_channel.is_empty() || wait_channel == "0" => "running".into(),
                _ => wait_channel.clone(),
            };
            let frames = backtraces.get(&tid).into_iter().flatten().take(6)
                .cloned().map(text).collect::<Result<Vec<_>, _>>()?;
            threads.push(StuckThread {
                pid: u32::try_from(pid).map_err(|_| "invalid process id in stuck report")?,
                tid: u32::try_from(tid).map_err(|_| "invalid thread id in stuck report")?,
                command: short(command)?,
                state: short(thread_state)?,
                wait_channel: text(wait_channel)?,
                syscall: short(syscall_fields.first().copied().unwrap_or("").to_string())?,
                detail: text(detail)?,
                backtrace: crate::wire::BoundedVec::new(frames)
                    .map_err(|error| format!(
                        "stuck backtrace exceeds relation bound: {error:?}"))?,
            });
        }
    }
    threads.sort_by_key(|thread| (
        thread.state.as_str() == "R",
        thread.pid,
        thread.tid,
    ));
    Ok(StuckReport {
        runner: u32::try_from(runner).map_err(|_| "invalid runner process id")?,
        threads: crate::wire::BoundedVec::new(threads)
            .map_err(|error| format!("stuck thread list exceeds relation bound: {error:?}"))?,
    })
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
    if crate::review::session_changes_typed(id)
        .is_ok_and(|changes| changes.is_empty()) {
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

fn legacy_process_provenance(value: &Value)
    -> Result<crate::generated_wire::ProcessProvenance, String>
{
    let bytes = |name: &str| {
        let value = value.get(name).and_then(Value::as_str)
            .ok_or_else(|| format!("process provenance has no {name}"))?;
        crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
            .map_err(|error| format!("process {name} exceeds relation bound: {error:?}"))
    };
    let argv = value.get("argv").and_then(Value::as_array)
        .ok_or("process provenance has no argv")?.iter().map(|value| {
            let value = value.as_str().ok_or("process argument is not text")?;
            crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                .map_err(|error| format!("process argument exceeds relation bound: {error:?}"))
        }).collect::<Result<Vec<_>, String>>()?;
    let environment = value.get("env").map(|environment| {
        let entries = environment.as_object().ok_or("process environment is not an object")?
            .iter().map(|(key, value)| {
                let value = value.as_str().ok_or("process environment value is not text")?;
                Ok((
                    crate::wire::BoundedBytes::new(key.as_bytes().to_vec())
                        .map_err(|_| "process environment key exceeds relation bound")?,
                    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                        .map_err(|_| "process environment value exceeds relation bound")?,
                ))
            }).collect::<Result<std::collections::BTreeMap<_, _>, String>>()?;
        crate::wire::BoundedMap::new(entries)
            .map_err(|error| format!("process environment exceeds relation bound: {error:?}"))
    }).transpose()?;
    Ok(crate::generated_wire::ProcessProvenance {
        tgid: value.get("tgid").or_else(|| value.get("pid"))
            .and_then(Value::as_u64).ok_or("process provenance has no tgid")?
            .try_into().map_err(|_| "process tgid exceeds u32")?,
        ppid: value.get("ppid").and_then(Value::as_i64).unwrap_or(0)
            .try_into().map_err(|_| "process ppid exceeds i32")?,
        executable: bytes("exe")?,
        cwd: bytes("cwd")?,
        argv: crate::wire::BoundedVec::new(argv)
            .map_err(|error| format!("process argv exceeds relation bound: {error:?}"))?,
        environment,
    })
}

fn legacy_register_request(msg: &Value)
    -> Result<crate::generated_wire::TransportRequest, String>
{
    use crate::generated_wire::{NetMode, RegistrationName, RunBackend, TransportRequest};
    let command = msg.get("cmd").and_then(Value::as_array)
        .ok_or("register has no command")?.iter().map(|value| {
            let value = value.as_str().ok_or("register command argument is not text")?;
            crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                .map_err(|error| format!("register command exceeds relation bound: {error:?}"))
        }).collect::<Result<Vec<_>, String>>()?;
    let name = if let Some(name) = msg.get("relname").and_then(Value::as_str) {
        RegistrationName::Nested {
            name: crate::wire::BoundedText::new(name.to_owned())
                .map_err(|_| "nested registration name exceeds relation bound")?,
        }
    } else if let Some(selector) = msg.get("session_id").and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        RegistrationName::Host {
            selector: crate::wire::BoundedText::new(selector.to_owned())
                .map_err(|_| "registration selector exceeds relation bound")?,
        }
    } else {
        RegistrationName::Automatic
    };
    let net_mode = match msg.get("net_mode").and_then(Value::as_str).unwrap_or("off") {
        "off" => NetMode::Off,
        "host" => NetMode::Host,
        "tap" => NetMode::Tap,
        _ => return Err("invalid registration network mode".into()),
    };
    Ok(TransportRequest::Register {
        command: crate::wire::BoundedVec::new(command)
            .map_err(|error| format!("register command exceeds relation bound: {error:?}"))?,
        provenance: legacy_process_provenance(msg.get("prov")
            .ok_or("register has no process provenance")?)?,
        name,
        backend: if msg.get("want_sud").and_then(Value::as_bool).unwrap_or(false) {
            RunBackend::Sud
        } else { RunBackend::Fuse },
        net_mode,
        capture: msg.get("want_capture").and_then(Value::as_bool).unwrap_or(true),
        direct: msg.get("want_direct").and_then(Value::as_bool).unwrap_or(false),
        capture_environment: msg.get("want_env").and_then(Value::as_bool).unwrap_or(false),
        brush: msg.get("want_brush").and_then(Value::as_bool).unwrap_or(false),
        api: msg.get("want_api").and_then(Value::as_bool).unwrap_or(false),
        web_capture: msg.get("want_webcap").and_then(Value::as_bool).unwrap_or(false),
        web_filter: msg.get("want_webfilter").and_then(Value::as_bool).unwrap_or(false),
        replay_from: msg.get("replay_from").and_then(Value::as_u64),
        no_parent: msg.get("want_no_parent").and_then(Value::as_bool).unwrap_or(false),
        readonly_parent: msg.get("want_readonly_parent").and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn legacy_register_reply(
    result: Result<crate::generated_wire::RegisterReply, String>,
) -> Value {
    use crate::generated_wire::RegisterReply;
    let RegisterReply {
        mount, shared_memory, dns, ca_bundle, owner, r#box, name,
        capture, api, no_host, oci, sud,
    } = match result {
        Ok(reply) => reply,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    let bytes = |value: crate::generated_wire::OsString| legacy_bytes(value);
    let oci = oci.map(|runtime| json!({
        "env": runtime.environment.map(|values|
            values.into_inner().into_iter().map(&bytes).collect::<Vec<_>>()),
        "cwd": runtime.cwd.map(legacy_bytes),
        "cmd": runtime.command.map(|values|
            values.into_inner().into_iter().map(&bytes).collect::<Vec<_>>()),
        "entrypoint": runtime.entrypoint.map(|values|
            values.into_inner().into_iter().map(&bytes).collect::<Vec<_>>()),
        "user": runtime.user.map(legacy_bytes),
    }));
    let mut reply = json!({
        "ok": true,
        "mount": legacy_bytes(mount),
        "shm_dir": legacy_bytes(shared_memory),
        "dns_ip": dns.map(|address|
            std::net::Ipv4Addr::from(address.0).to_string()).unwrap_or_default(),
        "ca_pem": ca_bundle.map(legacy_bytes).unwrap_or_default(),
        "owner_token": owner.0.iter().map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "box_id": r#box,
        "session_id": r#box.to_string(),
        "name": name.into_inner(),
        "capture": capture,
        "api": api,
        "no_host": no_host,
        "_box_sid": r#box,
    });
    if let Some(oci) = oci {
        reply["oci"] = oci;
    }
    if let Some(runtime) = sud {
        reply["sud_upper"] = json!(legacy_bytes(runtime.upper));
        reply["sud_lowers"] = json!(runtime.lowers.into_inner().into_iter()
            .map(legacy_bytes).collect::<Vec<_>>());
        reply["sud_ir_key"] = json!(runtime.inramfs_key.into_inner());
    }
    reply
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
    request: &crate::generated_wire::TransportRequest,
    peer_pidfd: Option<i32>,
    fd2_raw: Option<i32>,
    fd3_raw: Option<i32>,
) -> Result<crate::generated_wire::RegisterReply, String> {
    use crate::generated_wire::{NetMode, RegistrationName, RunBackend, TransportRequest};
    let TransportRequest::Register {
        command: _, provenance, name: registration_name, backend, net_mode,
        capture, direct, capture_environment, brush, api, web_capture,
        web_filter, replay_from, no_parent, readonly_parent,
    } = request else {
        return Err("expected register request".into());
    };
    // Assign the post-pidfd SCM_RIGHTS fds to roles from the message. The
    // runner sends them in a fixed order: [tap (if net_mode==tap)] then
    // [sud trace pipe (if want_sud)]. So:
    //   fuse+tap : fd2=tap
    //   sud+tap  : fd2=tap,   fd3=trace
    //   sud+!tap : fd2=trace
    // Own each as an OwnedFd so every early-return path closes it; the tap
    // fd moves into prepare_net and the trace fd into stream_events only on
    // the success path.
    let want_sud_fd = *backend == RunBackend::Sud;
    let is_tap_fd = *net_mode == NetMode::Tap;
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
        return Err("overlay mount is not available".into());
    };
    // Runner host pid: from the pidfd if sent (correct for nested runners whose
    // own getpid() is a parent-namespace pid); else the claimed tgid (top-level).
    let host_pid = peer_pidfd
        .map(host_pid_from_pidfd)
        .filter(|p| *p > 0)
        .or_else(|| {
            i32::try_from(provenance.tgid).ok()
        })
        .unwrap_or(0);
    let boxes = discover::discover();
    // ── PARENT + NAME RESOLUTION ───────────────────────────────────────────
    // IN-BOX (relname present): parent = kernel-derived enclosing box; the box
    //   supplies only a single-segment relative NAME (or "" → auto A<n>).
    // HOST (no relname): top-level by default; a supplied session_id may be a
    //   single NAME or a dotted display path (A.B) whose prefix names the parent.
    let mut parent: Option<i64> = None;
    let mut name: Option<String> = None;
    if let RegistrationName::Nested { name: nested_name } = registration_name {
        let rel = nested_name.as_str();
        if !rel.is_empty() && (!valid_name(rel) || rel.contains('.') || rel.contains('/')) {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return Err("invalid relname: must be a single NAME segment".into());
        }
        match derive_parent_box(state, host_pid) {
            Some(p) => parent = Some(p),
            None => {
                if let Some(fd) = peer_pidfd {
                    unsafe {
                        libc::close(fd);
                    }
                }
                return Err("relname supplied but no enclosing box found".into());
            }
        }
        if !rel.is_empty() {
            name = Some(rel.to_string());
        }
    } else if let RegistrationName::Host { selector } = registration_name {
        let want = selector.as_str();
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
                    return Err(format!("parent box '{prefix}' does not exist"));
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
        return Err("slopbox is already running".into());
    }
    let live_max = ov.box_ids().into_iter().max().unwrap_or(0);
    let id =
        existing_id.unwrap_or_else(|| boxes.keys().max().copied().unwrap_or(0).max(live_max) + 1);
    let name = name.unwrap_or_else(|| format!("A{id}"));
    let env_capture = *capture_environment;
    let direct = *direct;
    // D-parent flags. `want_no_parent` is the runner's explicit "this box has
    // NO parent and the lower chain does NOT bottom at the host /": the box's
    // own contents are its entire filesystem (the bottom of an OCI image
    // stack). It overrides the kernel-derived parent walk, so even a runner
    // nested under another box can declare itself a rootfs.
    let want_no_parent = *no_parent;
    let want_readonly_parent = *readonly_parent;
    let want_capture = *capture && !direct;
    let backing = crate::paths::live_home().join(id.to_string());
    if let Err(e) = std::fs::create_dir_all(backing.join("up")) {
        if let Some(fd) = peer_pidfd {
            unsafe {
                libc::close(fd);
            }
        }
        return Err(format!("backing: {e}"));
    }
    let b = match crate::capture::BoxState::create(id) {
        Ok(b) => b,
        Err(e) => {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return Err(format!("sqlar: {e}"));
        }
    };
    // RERUN: reopen the existing box's recorded state so prior writes show
    // through and prior process rows keep their ids (the new root is additive).
    if rerun {
        b.load_mirror();
    }
    b.set_env_capture(env_capture);
    b.set_direct(direct);
    b.set_is_brush(*brush);
    b.set_is_api(*api);
    // Tap boxes reach the network through the engine's MITM proxy + synthetic
    // DNS, so they need the engine's CA appended to their trust store and their
    // resolver pointed at the gateway. The overlay serves both as shadows gated
    // on this flag (see overlay.rs).
    b.set_is_tap(*net_mode == NetMode::Tap);
    b.set_meta("name", &name);
    // --sud (WIP, see engine/DESIGN-sud.md): the box runs under the sud64
    // wrapper with a directory upper instead of on the FUSE mount. Create
    // the upper here so the ack can hand its path to the runner; the
    // post-exit `sud_ingest` verb sweeps it into this BoxState. The trace
    // pipe (fd-1023 read end) came in as its own SCM_RIGHTS fd
    // (sud_trace_owned), separate from the tap fd — so a sud box can be a
    // TAP box too (tap fd → prepare_net, trace fd → stream_events).
    let want_sud = *backend == RunBackend::Sud;
    let mut sud_trace_fd: Option<std::os::fd::OwnedFd> = sud_trace_owned;
    if want_sud {
        let up = backing.join("sud-up");
        if let Err(e) = std::fs::create_dir_all(&up) {
            if let Some(fd) = peer_pidfd {
                unsafe {
                    libc::close(fd);
                }
            }
            return Err(format!("sud upper: {e}"));
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
                return Err(format!(
                    "sud nesting is same-in-same: enclosing box {enc} is \
                     not a sud box (see engine/DESIGN-sud.md)"));
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
                    return Err(format!("sud lower export: {e}"));
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
                return Err(format!(
                    "sud nesting is same-in-same: ancestor box {aid} is \
                     not a sud box (see engine/DESIGN-sud.md)"));
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
                        return Err(format!(
                            "running sud box {aid} has no recorded layer \
                             list (engine restarted under it?)"));
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
                    return Err(format!("sud lower export: {e}"));
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
            return Err(format!("sud upper: {e}"));
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
    match b.root_process(provenance, i64::from(host_pid)) {
        Ok(()) => (),
        Err(error) => {
            if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
            return Err(error);
        }
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
    let want_api = *api;
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
    if let (Ok(r#box), Ok(name)) = (u64::try_from(id),
        crate::wire::BoundedText::new(name.clone()))
    {
        broadcast(state, &crate::generated_wire::SubscriptionEvent::BoxAdded {
            r#box,
            name: Some(name),
            parent: parent.and_then(|value| u64::try_from(value).ok()),
        });
    }
    let root = crate::paths::mnt_point().join(id.to_string());

    // ── Networking (-n boxes only) ────────────────────────────────────────
    // Tap mode: the RUNNER already created the netns + TAP and handed us its fd
    // (SCM_RIGHTS on this register conn). Build the StackRuntime + flows log
    // around that fd and return dns_ip + the CA bundle CONTENT so the runner can
    // wire bwrap up (it materializes the CA in its own namespace). The engine
    // creates no netns/device, so there is no netns_path.
    let replay_from = match *replay_from {
        Some(value) if i64::try_from(value).is_err() => {
            return Err("replay box id exceeds engine range".into());
        }
        value => value,
    };
    let (dns_ip, ca_pem) = prepare_net(
        state,
        id,
        *net_mode,
        *web_capture,
        *web_filter,
        replay_from,
        tap_fd,
    ).unwrap_or_default();

    // D-oci: if any ancestor in the parent chain has an oci_config meta key
    // (stamped by `sarun oci load` on the top layer of an image), surface
    // env / cwd / cmd / entrypoint / user in the ack so the runner can
    // bwrap with the image's PATH set, in the image's WorkingDir, with the
    // image's User — without which `sarun img -- /bin/sh` would inherit
    // the HOST's PATH (pointing at host bins that don't exist in a closed
    // box) and the HOST's cwd (likely a path outside the image).
    let oci = oci_runtime_from_chain(&boxes, parent)?;
    let bounded_path = |value: &std::path::Path, field: &str| {
        crate::wire::BoundedBytes::new(value.as_os_str().as_bytes().to_vec())
            .map_err(|error| format!("{field} exceeds relation bound: {error:?}"))
    };
    let dns = if dns_ip.is_empty() {
        None
    } else {
        let address = dns_ip.parse::<std::net::Ipv4Addr>()
            .map_err(|error| format!("invalid generated DNS address {dns_ip:?}: {error}"))?;
        Some(crate::wire::FixedBytes(address.octets()))
    };
    let ca_bundle = if ca_pem.is_empty() {
        None
    } else {
        Some(crate::wire::BoundedBytes::new(ca_pem.into_bytes())
            .map_err(|error| format!("CA bundle exceeds relation bound: {error:?}"))?)
    };
    let owner = std::process::id() as u128
        ^ (id as u128) << 64
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
    let sud = if want_sud {
        let lowers = sud_lowers.into_iter().map(|value| {
            crate::wire::BoundedBytes::new(
                std::ffi::OsStr::new(&value).as_bytes().to_vec())
                .map_err(|error| format!("sud lower exceeds relation bound: {error:?}"))
        }).collect::<Result<Vec<_>, String>>()?;
        Some(crate::generated_wire::SudRuntime {
            upper: bounded_path(&backing.join("sud-up"), "sud upper")?,
            lowers: crate::wire::BoundedVec::new(lowers)
                .map_err(|error| format!("sud lower count exceeds relation bound: {error:?}"))?,
            inramfs_key: crate::wire::BoundedText::new(
                lock(state)
                    .overlay
                    .clone()
                    .and_then(|overlay| overlay.live_box(id))
                    .and_then(|r#box| r#box.get_meta("sud_ir_key"))
                    .unwrap_or_default())
                .map_err(|error| format!("sud inramfs key exceeds relation bound: {error:?}"))?,
        })
    } else {
        None
    };
    Ok(crate::generated_wire::RegisterReply {
        mount: bounded_path(&root, "mount path")?,
        shared_memory: bounded_path(&backing, "shared-memory path")?,
        dns,
        ca_bundle,
        owner: crate::wire::FixedBytes(owner.to_be_bytes()),
        r#box: u64::try_from(id).map_err(|_| "negative box id".to_string())?,
        name: crate::wire::BoundedText::new(name)
            .map_err(|error| format!("box name exceeds relation bound: {error:?}"))?,
        capture: want_capture,
        api: want_api,
        no_host,
        oci,
        sud,
    })
}

/// Walk the parent chain looking for an `oci_config` meta entry (stamped by
/// `sarun oci load` on the image's TOP layer). Returns the parsed runtime
/// view {env, cwd, cmd, entrypoint, user} the runner uses, or None when the
/// chain has no OCI ancestor (a non-OCI box). Reads from the discover()
/// snapshot's `Box_.meta` — no per-hop sqlar opens.
fn oci_runtime_from_chain(
    boxes: &std::collections::BTreeMap<i64, discover::Box_>,
    parent: Option<i64>,
) -> Result<Option<crate::generated_wire::OciRuntime>, String> {
    let mut cur = parent;
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cur {
        if !seen.insert(id) {
            return Ok(None);
        }
        let Some(b) = boxes.get(&id) else { return Ok(None) };
        if let Some(cfg_json) = b.meta.get("oci_config") {
            return parse_oci_runtime(cfg_json);
        }
        cur = b.parent;
    }
    Ok(None)
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
fn parse_oci_runtime(cfg_json: &str)
    -> Result<Option<crate::generated_wire::OciRuntime>, String>
{
    use crate::generated_wire::OciRuntime;
    let value: Value = serde_json::from_str(cfg_json)
        .map_err(|error| format!("invalid stored OCI config: {error}"))?;
    let Some(config) = value.get("config") else { return Ok(None) };
    let strings = |name: &str| -> Result<Option<Vec<crate::generated_wire::OsString>>, String> {
        config.get(name).map(|value| {
            let values = value.as_array().ok_or_else(|| format!("OCI {name} is not an array"))?
                .iter().map(|value| {
                    let value = value.as_str().ok_or_else(|| format!("OCI {name} item is not text"))?;
                    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                        .map_err(|error| format!("OCI {name} item exceeds relation bound: {error:?}"))
                }).collect::<Result<Vec<_>, String>>()?;
            Ok(values)
        }).transpose()
    };
    let bytes = |name: &str| -> Result<Option<_>, String> {
        config.get(name).map(|value| {
            let value = value.as_str().ok_or_else(|| format!("OCI {name} is not text"))?;
            crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                .map_err(|error| format!("OCI {name} exceeds relation bound: {error:?}"))
        }).transpose()
    };
    let runtime = OciRuntime {
        environment: strings("Env")?.map(|values|
            crate::wire::BoundedVec::<_, 0,
                { crate::generated_wire::LIMIT_ENVIRONMENT_ENTRIES }>::new(values)
                .map_err(|error| format!("OCI Env exceeds relation bound: {error:?}")))
            .transpose()?,
        cwd: bytes("WorkingDir")?,
        command: strings("Cmd")?.map(|values|
            crate::wire::BoundedVec::<_, 0,
                { crate::generated_wire::LIMIT_COMMAND_ITEMS }>::new(values)
                .map_err(|error| format!("OCI Cmd exceeds relation bound: {error:?}")))
            .transpose()?,
        entrypoint: strings("Entrypoint")?.map(|values|
            crate::wire::BoundedVec::<_, 0,
                { crate::generated_wire::LIMIT_COMMAND_ITEMS }>::new(values)
                .map_err(|error| format!("OCI Entrypoint exceeds relation bound: {error:?}")))
            .transpose()?,
        user: bytes("User")?,
    };
    let empty = runtime.environment.is_none() && runtime.cwd.is_none()
        && runtime.command.is_none() && runtime.entrypoint.is_none() && runtime.user.is_none();
    Ok((!empty).then_some(runtime))
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
    net_mode: crate::generated_wire::NetMode,
    web_capture: bool,
    web_filter: bool,
    replay_from: Option<u64>,
    tap_fd: Option<std::os::fd::OwnedFd>,
) -> Option<(String, String)> {
    if net_mode != crate::generated_wire::NetMode::Tap {
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
    let capture = if web_capture {
        lock(state)
            .overlay
            .clone()
            .map(|ov| crate::net::webcap::WebCapSink::new(ov, id))
    } else {
        None
    };
    let filter = if web_filter {
        Some(std::sync::Arc::new(crate::net::filter::Filter::load()))
    } else {
        None
    };
    // Replay (DESIGN-web.md W4.2): `replay_from` names the source box whose
    // captures answer this box's requests, with an optional `replay_asof`.
    let replay = replay_from.map(|source_box| crate::net::ReplaySource {
        source_box: i64::try_from(source_box).expect("validated replay box id"),
        asof: None,
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
        "stuck" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Stuck { sid },
            ));
        }
        "kill" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Kill { sid },
            ));
        }
        "dissolve" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Dissolve { sid },
            ));
        }
        "apply_to_copy" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ApplyToCopy { sid },
            ));
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
            let Some(r#box) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "bad box id"});
            };
            let mut attachments = Vec::new();
            for v in args.iter().skip(1) {
                let Some(attached) = v.as_u64() else {
                    // Object row: an external reference. Parse strictly —
                    // a malformed row must fail the verb, not skip.
                    let external = match serde_json::from_value::<crate::capture::ExtRef>(v.clone()) {
                        Ok(value) => value,
                        Err(error) => return json!({"ok": false,
                            "error": format!("bad attachment row: {error}")}),
                    };
                    let reference: Result<crate::generated_wire::ExternalReference, String> =
                        (|| Ok(crate::generated_wire::ExternalReference {
                        kind: legacy_bounded_text(external.kind, "attachment kind")?,
                        store: crate::wire::BoundedBytes::new(external.store.into_bytes())
                            .map_err(|_| "attachment store exceeds relation bound".to_owned())?,
                        reference: legacy_bounded_text(
                            external.refname, "attachment reference")?,
                        revision: legacy_bounded_text(
                            external.rev, "attachment revision")?,
                        prefix: crate::wire::BoundedBytes::new(external.prefix.into_bytes())
                            .map_err(|_| "attachment prefix exceeds relation bound".to_owned())?,
                        name: legacy_bounded_text(external.name, "attachment name")?,
                        }))();
                    let reference = match reference {
                        Ok(value) => value,
                        Err(error) => return json!({"ok": false, "error": error}),
                    };
                    attachments.push(crate::generated_wire::ReadonlyAttachment::External {
                        reference,
                    });
                    continue;
                };
                attachments.push(crate::generated_wire::ReadonlyAttachment::Box {
                    r#box: attached,
                });
            }
            let Ok(attachments) = crate::wire::BoundedVec::new(attachments) else {
                return json!({"ok": false, "error": "too many attachments"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::RoAttach {
                    r#box, attachments,
                },
            ));
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
            let (Some(sid), Some(store), Some(reference)) = (
                legacy_u64(args, 0),
                args.get(1).and_then(Value::as_str),
                args.get(2).and_then(Value::as_str),
            ) else {
                return json!({"ok": false, "error": "need store path + ref"});
            };
            let Ok(store) = crate::wire::BoundedBytes::new(store.as_bytes().to_vec()) else {
                return json!({"ok": false, "error": "git store exceeds relation bound"});
            };
            let Ok(reference) = crate::wire::BoundedText::new(reference.to_owned()) else {
                return json!({"ok": false, "error": "git reference exceeds relation bound"});
            };
            let bounded_path = |index: usize, field: &str| -> Result<_, Value> {
                let Some(path) = args.get(index).and_then(Value::as_str) else {
                    return Ok(None);
                };
                crate::wire::BoundedBytes::new(path.as_bytes().to_vec())
                    .map(Some).map_err(|_| json!({"ok": false,
                        "error": format!("git {field} exceeds relation bound")}))
            };
            let dest = match bounded_path(3, "destination") {
                Ok(value) => value,
                Err(error) => return error,
            };
            let subpath = match bounded_path(4, "subpath") {
                Ok(value) => value,
                Err(error) => return error,
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::GitCheckout {
                    sid, store, r#ref: reference, dest, subpath,
                },
            ));
        }
        // Attach a wikipedia mirror page (wikimak instance root) as an
        // external RO reference: args [sid, root, page_id, prefix?].
        // Bookkeeping only — title/id resolution here, and the head
        // rev read here is the PIN: the readout serves exactly that
        // revision as <title>.txt under prefix at first read
        // (attach.rs), even after later imports move the head.
        "wiki_attach" => {
            let (Some(sid), Some(root)) = (
                legacy_u64(args, 0),
                args.get(1).and_then(Value::as_str),
            ) else {
                return json!({"ok": false, "error": "need root + page"});
            };
            let page = match args.get(2) {
                Some(Value::Number(value)) if value.as_u64().is_some() => {
                    value.as_u64().unwrap().to_string()
                }
                Some(Value::String(value)) => value.clone(),
                _ => return json!({"ok": false, "error": "need root + page"}),
            };
            let Ok(root) = crate::wire::BoundedBytes::new(root.as_bytes().to_vec()) else {
                return json!({"ok": false, "error": "wiki root exceeds relation bound"});
            };
            let Ok(page) = crate::wire::BoundedText::new(page) else {
                return json!({"ok": false, "error": "wiki page exceeds relation bound"});
            };
            let prefix = match args.get(3).and_then(Value::as_str) {
                None => None,
                Some(prefix) => match crate::wire::BoundedBytes::new(
                    prefix.as_bytes().to_vec()) {
                    Ok(prefix) => Some(prefix),
                    Err(_) => return json!({"ok": false,
                        "error": "wiki prefix exceeds relation bound"}),
                },
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::WikiAttach {
                    sid, root, page, prefix,
                },
            ));
        }
        // Attach an IETF draft (ietf-mirror root) as an external RO
        // reference: args [sid, root, draft, prefix?]. Bookkeeping only
        // — the head rev read here is the PIN, and the readout serves
        // exactly that revision as <draft>-<rev>.txt under prefix at
        // first read (attach.rs), even after later updates move the
        // head.
        "ietf_attach" => {
            let (Some(sid), Some(root), Some(draft)) = (
                legacy_u64(args, 0),
                args.get(1).and_then(Value::as_str),
                args.get(2).and_then(Value::as_str),
            ) else {
                return json!({"ok": false, "error": "need root + draft"});
            };
            let Ok(root) = crate::wire::BoundedBytes::new(root.as_bytes().to_vec()) else {
                return json!({"ok": false, "error": "IETF root exceeds relation bound"});
            };
            let Ok(draft) = crate::wire::BoundedText::new(draft.to_owned()) else {
                return json!({"ok": false, "error": "IETF draft exceeds relation bound"});
            };
            let prefix = match args.get(3).and_then(Value::as_str) {
                None => None,
                Some(prefix) => match crate::wire::BoundedBytes::new(
                    prefix.as_bytes().to_vec()) {
                    Ok(prefix) => Some(prefix),
                    Err(_) => return json!({"ok": false,
                        "error": "IETF prefix exceeds relation bound"}),
                },
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::IetfAttach {
                    sid, root, draft, prefix,
                },
            ));
        }
        // Mirror-update jobs (mirrors.rs): the schedule surface.
        "mirror_jobs" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::MirrorJobs,
            ));
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
            let Ok(kind) = crate::wire::BoundedText::new(kind.to_owned()) else {
                return json!({"ok": false, "error": "mirror kind exceeds relation bound"});
            };
            let Ok(src) = crate::wire::BoundedText::new(src.to_owned()) else {
                return json!({"ok": false, "error": "mirror source exceeds relation bound"});
            };
            let Ok(dest) = crate::wire::BoundedBytes::new(dest.as_bytes().to_vec()) else {
                return json!({"ok": false, "error": "mirror destination exceeds relation bound"});
            };
            let interval_secs = match args.get(3) {
                None | Some(Value::Null) => None,
                Some(value) => match value.as_u64() {
                    Some(value) => Some(value),
                    None => return json!({"ok": false, "error": "invalid mirror interval"}),
                },
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::MirrorAdd {
                    kind, src, dest, interval_secs,
                },
            ));
        }
        // args: [id] — force-run one job now (paused included).
        "mirror_run" => {
            let Some(id) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "need job id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::MirrorRun { id },
            ));
        }
        // Start every due/stopped unpaused job.
        "mirror_run_pending" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::MirrorRunPending,
            ));
        }
        // args: [id, paused(bool)]
        "mirror_pause" => {
            let (Some(id), Some(p)) = (
                legacy_u64(args, 0),
                args.get(1).and_then(Value::as_bool),
            ) else {
                return json!({"ok": false, "error": "need id + bool"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::MirrorPause {
                    id, paused: p,
                },
            ));
        }
        // args: [id]
        "mirror_rm" => {
            let Some(id) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "need job id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::MirrorRm { id },
            ));
        }
        "rotate" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Rotate { sid },
            ));
        }
        "reload_rules" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReloadRules,
            ));
        }
        "delete" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::Delete { sid },
            ));
        }
        "review.session_changes" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": []});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewSessionChanges { sid },
            ));
        }
        "review.hunks" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"ok": true, "r": {"is_text": false, "hunks": [],
                    "diff": {"kind": "error", "error": "bad args"}}});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewHunks { sid, rel },
            ));
        }
        "review.file_bytes" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"ok": false, "error": "bad args"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewFileBytes { sid, rel },
            ));
        }
        "review.write_file" => {
            let (Some(sid), Some(rel), Some(encoded)) = (
                legacy_u64(args, 0),
                legacy_path_arg(args, 1),
                args.get(2).and_then(Value::as_str),
            ) else {
                return json!({"ok": false, "error": "bad args"});
            };
            let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(bytes) => bytes,
                Err(error) => return json!({"ok": false,
                    "error": format!("bad base64: {error}")}),
            };
            let Ok(b64) = crate::wire::BoundedBytes::new(bytes) else {
                return json!({"ok": false, "error": "review file exceeds relation bound"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewWriteFile { sid, rel, b64 },
            ));
        }
        "review.apply" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": {"applied": [], "errors": []}});
            };
            let paths = match legacy_path_list(args.get(1)) {
                Ok(paths) => paths,
                Err(error) => return json!({"ok": false, "error": error}),
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewApply { sid, paths },
            ));
        }
        "review.discard" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": {"discarded": [], "errors": []}});
            };
            let paths = match legacy_path_list(args.get(1)) {
                Ok(paths) => paths,
                Err(error) => return json!({"ok": false, "error": error}),
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewDiscard { sid, paths },
            ));
        }
        "review.file_groups" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "no slopbox"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewFileGroups { sid },
            ));
        }
        "review.patch_text" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": {"__b": ""}});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewPatchText { sid },
            ));
        }
        "review.change_mode" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"ok": true, "r": Value::Null});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewChangeMode { sid, rel },
            ));
        }
        "review.decorate" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"ok": true, "r": {
                    "is_text": false, "stale": false, "kind": "changed"}});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewDecorate { sid, rel },
            ));
        }
        // Newest-first slice of the box's change set, for the boxes view's
        // "recently changed" panel on a live box. limit defaults to 200.
        "review.recent_changes" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": []});
            };
            let limit = args.get(1).and_then(Value::as_u64);
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewRecentChanges { sid, limit },
            ));
        }
        // Five-list bundle for the Sessions-view right pane: newest-first
        // outputs / changes / processes / pipelines / build-edges in one
        // round-trip, capped at `limit` per kind (default 20). Changes
        // includes xattr modifications inline as kind="xattr" rows.
        "review.box_summary" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": {"outputs":[], "changes":[],
                    "processes":[], "pipelines":[], "edges":[]}});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewBoxSummary {
                    sid, limit: args.get(1).and_then(Value::as_u64),
                },
            ));
        }
        // The causal neighborhood of one pipeline: parent, children, owning
        // edge. args: [sid, brushprov_row_id].
        "review.pipeline_context" => {
            let (Some(sid), Some(prov_id)) = (legacy_u64(args, 0), legacy_u64(args, 1)) else {
                return json!({"ok": true, "r": {}});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewPipelineContext {
                    sid, prov_id,
                },
            ));
        }
        // Search the box's recorded makefile variable assignments. args:
        // [sid, name_pattern, value_pattern, limit]. Patterns are cmd_match
        // text globs (bare word = substring); empty = match all.
        "review.makevars" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": []});
            };
            let text = |index| args.get(index).and_then(Value::as_str)
                .map(|value| crate::wire::BoundedText::new(value.to_owned())
                    .map_err(|error| format!("make variable pattern exceeds relation bound: {error:?}")))
                .transpose();
            let (name_pat, value_pat) = match (text(1), text(2)) {
                (Ok(name), Ok(value)) => (name, value),
                (Err(error), _) | (_, Err(error)) => return json!({"ok": false, "error": error}),
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewMakevars {
                    sid, name_pat, value_pat,
                    limit: args.get(3).and_then(Value::as_u64),
                    any: args.get(4).and_then(Value::as_bool),
                },
            ));
        }
        // Map provenance row ids between the process / pipeline / edge
        // domains — the cross-pane generated filter's id translation.
        // args: [sid, from_kind, [ids...], to_kind] → [ids...].
        "review.map_ids" => {
            let (Some(sid), Some(from), Some(values), Some(to)) = (
                legacy_u64(args, 0), args.get(1).and_then(Value::as_str)
                    .and_then(legacy_provenance_domain),
                args.get(2).and_then(Value::as_array),
                args.get(3).and_then(Value::as_str).and_then(legacy_provenance_domain),
            ) else {
                return json!({"ok": false, "error": "bad provenance mapping arguments"});
            };
            let Some(ids) = values.iter().map(Value::as_u64).collect::<Option<Vec<_>>>() else {
                return json!({"ok": false, "error": "provenance ids must be unsigned integers"});
            };
            let ids = match crate::wire::BoundedVec::new(ids) {
                Ok(ids) => ids,
                Err(error) => return json!({"ok": false,
                    "error": format!("provenance ids exceed relation bound: {error:?}")}),
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewMapIds {
                    sid, from, ids, to,
                },
            ));
        }
        // Bulk decorate: one RPC for a whole window of changes-pane rows
        // (kind / stale / is_text per row) — the UI uses this to label the
        // changes list with +/~/- glyphs and the `!` stale marker without a
        // round-trip per row.
        "review.decorate_many" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": []});
            };
            let Some(values) = args.get(1).and_then(Value::as_array) else {
                return json!({"ok": false, "error": "bad decoration paths"});
            };
            let mut rels = Vec::with_capacity(values.len());
            for value in values {
                let Some(path) = value.as_str() else {
                    return json!({"ok": false, "error": "bad decoration path"});
                };
                let Ok(path) = crate::wire::BoundedBytes::new(path.as_bytes().to_vec()) else {
                    return json!({"ok": false,
                        "error": "decoration path exceeds relation bound"});
                };
                rels.push(path);
            }
            let Ok(rels) = crate::wire::BoundedVec::new(rels) else {
                return json!({"ok": false, "error": "too many decoration paths"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewDecorateMany { sid, rels },
            ));
        }
        "review.apply_hunk" => {
            let (Some(sid), Some(rel), Some(hunk_ix)) = (
                legacy_u64(args, 0), legacy_path_arg(args, 1),
                args.get(2).and_then(Value::as_u64)
                    .and_then(|index| u32::try_from(index).ok()),
            ) else {
                return json!({"ok": false, "error": "bad args"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewApplyHunk {
                    sid, rel, hunk_ix,
                },
            ));
        }
        "review.discard_hunk" => {
            let (Some(sid), Some(rel), Some(hunk_ix)) = (
                legacy_u64(args, 0), legacy_path_arg(args, 1),
                args.get(2).and_then(Value::as_u64)
                    .and_then(|index| u32::try_from(index).ok()),
            ) else {
                return json!({"ok": false, "error": "bad args"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::ReviewDiscardHunk {
                    sid, rel, hunk_ix,
                },
            ));
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
            let parent_sid = match args.first() {
                None | Some(Value::Null) => None,
                Some(_) => match legacy_u64(args, 0) {
                    Some(parent) => Some(parent),
                    None => return json!({"ok": false, "error": "invalid parent box id"}),
                },
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::BoxNew { parent_sid },
            ));
        }
        "struct_quick" => {
            let (Some(sid), Some(rel)) = (legacy_u64(args, 0), legacy_path_arg(args, 1)) else {
                return json!({"lines": [["err", "bad args"]], "job": Value::Null});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::StructQuick { sid, rel },
            ));
        }
        "flows.list" => {
            let sid = match args.first() {
                None => None,
                Some(_) => match legacy_u64(args, 0) {
                    Some(sid) => Some(sid),
                    None => return json!({"ok": false, "error": "invalid box id"}),
                },
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::FlowsList { sid },
            ));
        }
        "flows.detail" => {
            let (sid, frame) = match args {
                [frame] => (None, frame.as_u64()),
                [sid, frame] => {
                    let Some(sid) = sid.as_str().and_then(|sid| sid.parse().ok()) else {
                        return json!({"ok": false, "error": "invalid box id"});
                    };
                    (Some(sid), frame.as_u64())
                }
                _ => (None, None),
            };
            let Some(frame) = frame else {
                return json!({"ok": false, "error": "bad args"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::FlowsDetail { sid, frame },
            ));
        }
        "prompts.peek" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::PromptsPeek,
            ));
        }
        "prompts.answer" => {
            let Some(id) = args.first().and_then(Value::as_u64) else {
                return json!({"ok": false, "error": "missing prompt id"});
            };
            let verdict = match args.get(1).and_then(Value::as_str) {
                Some("yes_once") => crate::generated_wire::PromptVerdict::YesOnce,
                Some("no_once") => crate::generated_wire::PromptVerdict::NoOnce,
                Some("allow_save") => crate::generated_wire::PromptVerdict::AllowSave,
                Some("deny_save") => crate::generated_wire::PromptVerdict::DenySave,
                _ => return json!({"ok": false, "error": "bad verdict"}),
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::PromptsAnswer { id, verdict },
            ));
        }
        "prompts.ui_active" => {
            let Some(active) = args.first().and_then(Value::as_bool) else {
                return json!({"ok": false, "error": "missing active flag"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::PromptsUiActive { bool: active },
            ));
        }
        "flows.packets" => {
            let (sid, stream) = match args {
                [stream] => (None, stream.as_u64()),
                [sid, stream] => {
                    let Some(sid) = sid.as_str().and_then(|sid| sid.parse().ok()) else {
                        return json!({"ok": false, "error": "invalid box id"});
                    };
                    (Some(sid), stream.as_u64())
                }
                _ => (None, None),
            };
            let Some(stream) = stream else {
                return json!({"ok": false, "error": "bad args"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::FlowsPackets { sid, stream },
            ));
        }
        "struct_finish" => {
            let Some(job) = legacy_u64(args, 0) else {
                return json!({"lines": [["err", "bad job"]]});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::StructFinish { job },
            ));
        }
        "struct_cancel" => {
            let Some(job) = legacy_u64(args, 0) else {
                return json!({"ok": true, "r": Value::Null});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::StructCancel { job },
            ));
        }
        "box_drop" => {
            let Some(sid) = legacy_u64(args, 0) else {
                return json!({"ok": false, "error": "missing or invalid box id"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::BoxDrop { sid },
            ));
        }
        // ── box-rooted file ops — the engine-side half of oaita's read/write/
        //    inspect tools. Resolve name→id, hydrate the parent chain, then
        //    use the same overlay API nested boxes use. No FUSE mount needed,
        //    no subprocess. args: [name_or_id, path_rel_to_root, (write only)
        //    base64-bytes]. path must NOT start with '/'.
        "box_file_read" => {
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_file_read: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let Some(path) = legacy_path_arg(args, 1) else {
                return json!({"ok": false, "error": "box_file_read: missing path"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::BoxFileRead {
                    r#box: id as u64,
                    path,
                },
            ));
        }
        "box_file_write" => {
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_file_write: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let Some(path) = legacy_path_arg(args, 1) else {
                return json!({"ok": false, "error": "box_file_write: missing path"});
            };
            let b64 = args.get(2).and_then(Value::as_str).unwrap_or("");
            use base64::{Engine, prelude::BASE64_STANDARD};
            let bytes = match BASE64_STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => return json!({"ok": false,
                    "error": format!("bad base64: {e}")}),
            };
            let Ok(b64) = crate::wire::BoundedBytes::new(bytes) else {
                return json!({"ok": false, "error": "box file content exceeds relation bound"});
            };
            return match dispatch_action(
                state,
                crate::generated_wire::ActionRequest::BoxFileWrite {
                    r#box: id as u64,
                    path,
                    b64,
                },
            ) {
                Ok(crate::generated_wire::ActionSuccess::BoxFileWrite { value }) => {
                    json!({"ok": true, "len": value})
                }
                Ok(other) => json!({"ok": false, "error": format!(
                    "wrong typed box_file_write result opcode: {}", other.code())}),
                Err(error) => json!({"ok": false, "error": error}),
            };
        }
        "box_dir_list" => {
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_dir_list: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let Some(path) = legacy_path_arg(args, 1) else {
                return json!({"ok": false, "error": "box_dir_list: missing path"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::BoxDirList {
                    r#box: id as u64,
                    path,
                },
            ));
        }
        "box_path_kind" => {
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_path_kind: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let Some(path) = legacy_path_arg(args, 1) else {
                return json!({"ok": false, "error": "box_path_kind: missing path"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state,
                crate::generated_wire::ActionRequest::BoxPathKind {
                    r#box: id as u64,
                    path,
                },
            ));
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
            let Ok(reference) = crate::wire::BoundedText::new(reference.to_owned()) else {
                return json!({"ok": false, "error": "OCI reference exceeds relation bound"});
            };
            let name = match args.get(1).and_then(Value::as_str) {
                None => None,
                Some(name) => match crate::wire::BoundedText::new(name.to_owned()) {
                    Ok(name) => Some(name),
                    Err(_) => return json!({"ok": false,
                        "error": "OCI image name exceeds relation bound"}),
                },
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OciLoad { reference, name },
            ));
        }
        // Loaded images, for the UI's base-image picker: the TOP box of each
        // installed layer chain (the one carrying the image config), with the
        // reference it was pulled as. Cheap metadata scan, no registry I/O.
        "oci.images" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OciImages,
            ));
        }
        // Is a named svc.serve service live (≥1 parked accept slot)? Used by
        // `oaita local` to poll readiness / idempotency without racing.
        "svc.up" => {
            let name = args.first().and_then(Value::as_str).unwrap_or("");
            let Ok(name) = crate::wire::BoundedText::new(name.to_owned()) else {
                return json!({"ok": false, "error": "service name exceeds relation bound"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::SvcUp { name },
            ));
        }
        "oci.resolve" => {
            let Some(reference) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "oci.resolve: missing reference"});
            };
            let Ok(reference) = crate::wire::BoundedText::new(reference.to_owned()) else {
                return json!({"ok": false, "error": "OCI reference exceeds relation bound"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OciResolve { reference },
            ));
        }
        // In-box `oci build`: the CLI ships its context + Dockerfile here so the
        // build runs host-side (its layer boxes land in engine state, not the
        // box's FUSE). Returns the worker's output + exit code + top box id.
        "oci.build" => {
            let Some(spec) = args.first() else {
                return json!({"ok": false, "error": "oci.build: missing spec"});
            };
            use base64::{Engine as _, prelude::BASE64_STANDARD};
            let Some(context) = spec.get("context_tar_gz").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "oci.build: missing context_tar_gz"});
            };
            let context = match BASE64_STANDARD.decode(context.trim()) {
                Ok(value) => value,
                Err(error) => return json!({"ok": false,
                    "error": format!("oci.build: invalid context: {error}")}),
            };
            let Ok(context_tar_gz) = crate::wire::BoundedBytes::new(context) else {
                return json!({"ok": false, "error": "OCI build context exceeds relation bound"});
            };
            let dockerfile = spec.get("dockerfile").and_then(Value::as_str).unwrap_or("");
            let Ok(dockerfile) = crate::wire::BoundedBytes::new(dockerfile.as_bytes().to_vec()) else {
                return json!({"ok": false, "error": "OCI Dockerfile exceeds relation bound"});
            };
            let tag = match spec.get("tag").and_then(Value::as_str) {
                None => None,
                Some(tag) => match crate::wire::BoundedText::new(tag.to_owned()) {
                    Ok(tag) => Some(tag),
                    Err(_) => return json!({"ok": false,
                        "error": "OCI build tag exceeds relation bound"}),
                },
            };
            let net_mode = match spec.get("net").and_then(Value::as_str).unwrap_or("tap") {
                "off" => crate::generated_wire::NetMode::Off,
                "host" => crate::generated_wire::NetMode::Host,
                "tap" => crate::generated_wire::NetMode::Tap,
                mode => return json!({"ok": false,
                    "error": format!("unknown OCI build network mode {mode:?}")}),
            };
            let mut build_arguments = std::collections::BTreeMap::new();
            if let Some(arguments) = spec.get("build_args").and_then(Value::as_array) {
                for argument in arguments {
                    let Some(pair) = argument.as_array().filter(|pair| pair.len() == 2) else {
                        return json!({"ok": false, "error": "invalid OCI build argument"});
                    };
                    let (Some(key), Some(value)) = (pair[0].as_str(), pair[1].as_str()) else {
                        return json!({"ok": false, "error": "invalid OCI build argument"});
                    };
                    let (Ok(key), Ok(value)) = (
                        crate::wire::BoundedText::new(key.to_owned()),
                        crate::wire::BoundedText::new(value.to_owned()),
                    ) else {
                        return json!({"ok": false,
                            "error": "OCI build argument exceeds relation bound"});
                    };
                    if build_arguments.insert(key, value).is_some() {
                        return json!({"ok": false, "error": "duplicate OCI build argument"});
                    }
                }
            }
            let Ok(build_arguments) = crate::wire::BoundedMap::new(build_arguments) else {
                return json!({"ok": false, "error": "too many OCI build arguments"});
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OciBuild {
                    spec: crate::generated_wire::OciBuildSpec {
                        context_tar_gz,
                        dockerfile,
                        tag,
                        net_mode,
                        build_arguments,
                    },
                },
            ));
        }
        // The local-model picker's catalog: currently-popular GGUF instruct
        // models resolved from a live HuggingFace query (config-file override
        // + offline fallback). Each entry is a ready-to-download Q4 URL. The
        // UI opens this when neither an external API nor a local model is set.
        "oaita.models" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OaitaModels,
            ));
        }
        // What the "Api" pane is wired to: external (host oaita.toml has a
        // model), local (an OAITA-LOCAL svc is declared), or none (offer the
        // picker). Lets the UI reflect real state instead of guessing.
        "oaita.status" => {
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OaitaStatus,
            ));
        }
        // Connection test for the external-API config editor: does a minimal
        // 1-token chat completion against the given endpoint and reports
        // reachability / auth / model validity as a single line. Runs on the
        // engine (which has the network), so the UI stays a thin client.
        "oaita.probe" => {
            let spec = args.first().unwrap_or(&Value::Null);
            let text = |field: &str, default: &str| -> Result<_, Value> {
                let value = spec.get(field).and_then(Value::as_str).unwrap_or(default);
                crate::wire::BoundedText::new(value.to_owned()).map_err(|_| json!({
                    "ok": false,
                    "error": format!("OAITA {field} exceeds relation bound"),
                }))
            };
            let base_url = match text("base_url", "https://api.openai.com/v1") {
                Ok(value) => value,
                Err(error) => return error,
            };
            let model = match text("model", "") {
                Ok(value) => value,
                Err(error) => return error,
            };
            let api_key = match text("api_key", "") {
                Ok(value) => value,
                Err(error) => return error,
            };
            return legacy_ui_action_reply(dispatch_action(
                state, crate::generated_wire::ActionRequest::OaitaProbe {
                    spec: crate::generated_wire::ApiProbeSpec {
                        base_url, model, api_key,
                    },
                },
            ));
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
        /// Temporary newline-JSON projection into the generated action path.
        /// Every catalogued arm returns after constructing an `ActionRequest`.
        pub(crate) fn dispatch_ui_verb($state: &State, $verb: &str,
                                       $args: &[Value],
                                       $boxes: &std::collections::BTreeMap<i64, discover::Box_>)
                                       -> Value {
            match $verb {
                $( $($name)|+ => $body )*
                other => json!({"ok": false, "error":
                    format!("unknown verb '{other}'; see 'verbs'")}),
            }
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
            let request: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let amount = msg.get("amount").and_then(Value::as_i64)
                    .ok_or_else(|| "budget.grant: missing integer amount".to_string())?;
                let target = match msg.get("box").and_then(Value::as_str) {
                    Some(name) if !name.is_empty() => {
                        crate::generated_wire::BoxTarget::Selector {
                            r#box: crate::wire::BoundedText::new(name.to_owned())
                                .map_err(|_| "budget.grant: box selector exceeds relation bound".to_string())?,
                        }
                    }
                    _ => crate::generated_wire::BoxTarget::Broker,
                };
                Ok(crate::generated_wire::TransportRequest::BudgetGrant { target, amount })
            })();
            let response = request.and_then(|request|
                dispatch_reply_transport(&state, request, hint_box_id, None));
            let reply = legacy_transport_response(response);
            let _ = writer.write_all(format!("{reply}\n").as_bytes());
            return;
        }
        // The oaita API proxy lives on the existing box-channel as new
        // FRAME_API_* frame types — not as a top-level connection type.
        // See the FRAME_API_* handling in the post-register frame loop
        // below and frames::FRAME_API_{OPEN,DATA,CLOSE}.
        let mut reply = if msg.get("type").and_then(Value::as_str) == Some("register") {
            match legacy_register_request(&msg) {
                Ok(request) => legacy_register_reply(register(
                    &state,
                    &request,
                    peer_pidfd.take(),
                    peer_tapfd.take().map(|fd|
                        <std::os::fd::OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(fd)),
                    peer_thirdfd.take().map(|fd|
                        <std::os::fd::OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(fd)),
                )),
                Err(error) => json!({"ok": false, "error": error}),
            }
        } else if msg.get("type").and_then(Value::as_str) == Some("brush_prov_nested") {
            // D9 nested-shell provenance: a one-shot control message from the
            // brush-sh shim, carrying its OWN pidfd (like register) so we resolve
            // the enclosing box from /proc ancestry. NOT a box channel — record
            // and reply once, then the connection closes. The pidfd is consumed.
            let result: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let values = msg.get("records").and_then(Value::as_array)
                    .ok_or("brush_prov_nested: missing records")?;
                let records = values.iter().map(crate::discover::relation_pipeline_provenance)
                    .collect::<Result<Vec<_>, _>>()?;
                let records = crate::wire::BoundedVec::<_, 1,
                    { crate::generated_wire::LIMIT_COLLECTION_ITEMS }>::new(records)
                    .map_err(|error| format!("pipeline records exceed relation bound: {error:?}"))?;
                Ok(crate::generated_wire::TransportRequest::BrushProvenance { records })
            })();
            match result {
                Ok(request) => legacy_transport_response(dispatch_reply_transport(
                    &state, request, hint_box_id, peer_pidfd.take())),
                Err(error) => json!({"ok": false, "error": error}),
            }
        } else if msg.get("type").and_then(Value::as_str) == Some("brush_prov_done") {
            // D9 pipeline completion: a one-shot control message emitted after a
            // pipeline's complete-command finishes, carrying its OWN pidfd (like
            // brush_prov_nested) so we resolve the box, then stamp done_ts +
            // exit_code on the matching brushprov rows (by uid).
            let result: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let values = msg.get("uids").and_then(Value::as_array)
                    .ok_or("brush_prov_done: missing uids")?;
                let pipelines = values.iter().map(|value| value.as_u64()
                    .ok_or("brush_prov_done: pipeline id must be unsigned"))
                    .collect::<Result<Vec<_>, _>>()?;
                let code = i32::try_from(msg.get("code").and_then(Value::as_i64).unwrap_or(0))
                    .map_err(|_| "brush_prov_done: code exceeds i32")?;
                let done_at = msg.get("done_ts").and_then(Value::as_f64)
                    .ok_or("brush_prov_done: missing done_ts")?;
                let pipelines = crate::wire::BoundedVec::<_, 1,
                    { crate::generated_wire::LIMIT_COLLECTION_ITEMS }>::new(pipelines)
                    .map_err(|error| format!("pipeline ids exceed relation bound: {error:?}"))?;
                Ok(crate::generated_wire::TransportRequest::BrushDone {
                    pipelines, code, done_at,
                })
            })();
            match result {
                Ok(request) => legacy_transport_response(dispatch_reply_transport(
                    &state, request, hint_box_id, peer_pidfd.take())),
                Err(error) => json!({"ok": false, "error": error}),
            }
        } else if msg.get("type").and_then(Value::as_str) == Some("recipe_fixup") {
            let result: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let values = msg.get("uids").and_then(Value::as_array)
                    .ok_or("recipe_fixup: missing uids")?;
                let pipelines = values.iter().map(|value| value.as_u64()
                    .ok_or("recipe_fixup: pipeline id must be unsigned"))
                    .collect::<Result<Vec<_>, _>>()?;
                let started_at = msg.get("start_ts").and_then(Value::as_f64)
                    .ok_or("recipe_fixup: missing start_ts")?;
                let pipelines = crate::wire::BoundedVec::<_, 1,
                    { crate::generated_wire::LIMIT_COLLECTION_ITEMS }>::new(pipelines)
                    .map_err(|error| format!("pipeline ids exceed relation bound: {error:?}"))?;
                Ok(crate::generated_wire::TransportRequest::RecipeStarted {
                    pipelines, started_at,
                })
            })();
            match result {
                Ok(request) => legacy_transport_response(dispatch_reply_transport(
                    &state, request, hint_box_id, peer_pidfd.take())),
                Err(error) => json!({"ok": false, "error": error}),
            }
        } else if msg.get("type").and_then(Value::as_str) == Some("build_edges") {
            // Phase 1 embedded-ninja: a one-shot control message from the
            // shadowed `ninja` (vendored n2) carrying its OWN pidfd, resolved to
            // the enclosing box by /proc ancestry exactly like brush_prov_nested.
            let result: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let values = msg.get("edges").and_then(Value::as_array)
                    .ok_or("build_edges: missing edges")?;
                let edges = values.iter().map(|value| Ok(crate::generated_wire::BuildEdge {
                    outputs: legacy_transport_paths::<1>(value.get("outs"))?,
                    inputs: legacy_transport_paths::<0>(value.get("ins"))?,
                    command: value.get("cmd").and_then(Value::as_str).map(|command|
                        crate::wire::BoundedText::new(command.to_owned())
                            .map_err(|_| "build edge command exceeds relation bound"))
                        .transpose()?,
                })).collect::<Result<Vec<_>, String>>()?;
                let edges = crate::wire::BoundedVec::<_, 1,
                    { crate::generated_wire::LIMIT_COLLECTION_ITEMS }>::new(edges)
                    .map_err(|error| format!("build graph exceeds relation bound: {error:?}"))?;
                Ok(crate::generated_wire::TransportRequest::BuildGraph { edges })
            })();
            match result {
                Ok(request) => legacy_transport_response(dispatch_reply_transport(
                    &state, request, hint_box_id, peer_pidfd.take())),
                Err(error) => json!({"ok": false, "error": error}),
            }
        } else if msg.get("type").and_then(Value::as_str) == Some("make_vars") {
            let result: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let values = msg.get("rows").and_then(Value::as_array)
                    .ok_or("make_vars: missing rows")?;
                let rows = values.iter().map(|row| {
                    let bytes = |name: &str, default: bool| {
                        let value = row.get(name).and_then(Value::as_str);
                        let value = if default { value.unwrap_or("") }
                            else { value.ok_or("make variable name is required")? };
                        crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                            .map_err(|_| "make variable field exceeds relation bound")
                    };
                    Ok(crate::generated_wire::MakeVariable {
                        name: bytes("name", false)?,
                        location: bytes("loc", true)?,
                        value: bytes("value", true)?,
                        make_directory: bytes("make", true)?,
                        rhs: bytes("rhs", true)?,
                        references: bytes("refs", true)?,
                        edge_output: row.get("edge").and_then(Value::as_str).map(|value|
                            crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                                .map_err(|_| "make variable edge exceeds relation bound"))
                            .transpose()?,
                        pipeline: row.get("uid").and_then(Value::as_u64).unwrap_or(0),
                        flags: crate::wire::BoundedText::new(row.get("flags")
                            .and_then(Value::as_str).unwrap_or("").to_owned())
                            .map_err(|_| "make variable flags exceed relation bound")?,
                    })
                }).collect::<Result<Vec<_>, String>>()?;
                let rows = crate::wire::BoundedVec::<_, 1,
                    { crate::generated_wire::LIMIT_COLLECTION_ITEMS }>::new(rows)
                    .map_err(|error| format!("make variables exceed relation bound: {error:?}"))?;
                Ok(crate::generated_wire::TransportRequest::MakeVariables { rows })
            })();
            match result {
                Ok(request) => legacy_transport_response(dispatch_reply_transport(
                    &state, request, hint_box_id, peer_pidfd.take())),
                Err(error) => json!({"ok": false, "error": error}),
            }
        } else if msg.get("type").and_then(Value::as_str) == Some("box_activity") {
            let result: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let values = msg.get("items").and_then(Value::as_array)
                    .ok_or("box_activity: missing items")?;
                let items = values.iter().map(|value| {
                    let fields = value.as_array().ok_or("box activity item must be an array")?;
                    let description = fields.first().and_then(Value::as_str)
                        .ok_or("box activity description must be text")?;
                    let age_seconds = fields.get(1).and_then(Value::as_u64)
                        .ok_or("box activity age must be unsigned")?;
                    Ok(crate::generated_wire::ActivityItem {
                        description: crate::wire::BoundedText::new(description.to_owned())
                            .map_err(|_| "box activity description exceeds relation bound")?,
                        age_seconds,
                    })
                }).collect::<Result<Vec<_>, String>>()?;
                let items = crate::wire::BoundedVec::<_, 1,
                    { crate::generated_wire::LIMIT_COLLECTION_ITEMS }>::new(items)
                    .map_err(|error| format!("box activity exceeds relation bound: {error:?}"))?;
                Ok(crate::generated_wire::TransportRequest::BoxActivity { items })
            })();
            match result {
                Ok(request) => legacy_transport_response(dispatch_reply_transport(
                    &state, request, hint_box_id, peer_pidfd.take())),
                Err(error) => json!({"ok": false, "error": error}),
            }
        } else if msg.get("type").and_then(Value::as_str) == Some("build_edge_state") {
            // A single edge's run-state transition (started / finished), sent by
            // the in-process make/ninja executor around each recipe — stamps the
            // box's build_edges row so the targets pane shows live build progress.
            let result: Result<crate::generated_wire::TransportRequest, String> = (|| {
                let path = |name: &str| msg.get(name).and_then(Value::as_str)
                    .map(|value| crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
                        .map_err(|_| "build edge path exceeds relation bound"))
                    .transpose();
                let text = |name: &str| msg.get(name).and_then(Value::as_str)
                    .map(|value| crate::wire::BoundedText::new(value.to_owned())
                        .map_err(|_| "build edge text exceeds relation bound"))
                    .transpose();
                let at = msg.get("ts").and_then(Value::as_f64)
                    .ok_or("build_edge_state: missing timestamp")?;
                let transition = match msg.get("state").and_then(Value::as_str) {
                    Some("start") => crate::generated_wire::BuildEdgeTransition::Start {
                        at, output: path("out")?, command: text("cmd")?,
                    },
                    Some("done") => crate::generated_wire::BuildEdgeTransition::Done {
                        at, output: path("out")?, command: text("cmd")?,
                        code: i32::try_from(msg.get("code").and_then(Value::as_i64)
                            .unwrap_or(0)).map_err(|_| "build edge code exceeds i32")?,
                        excerpt: text("excerpt")?,
                    },
                    _ => return Err("build_edge_state: invalid state".into()),
                };
                Ok(crate::generated_wire::TransportRequest::BuildEdgeState { transition })
            })();
            match result {
                Ok(request) => legacy_transport_response(dispatch_reply_transport(
                    &state, request, hint_box_id, peer_pidfd.take())),
                Err(error) => json!({"ok": false, "error": error}),
            }
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
            if let Ok(r#box) = u64::try_from(id) {
                broadcast(&state,
                    &crate::generated_wire::SubscriptionEvent::BoxRemoved { r#box });
            }
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

fn git_checkout_typed(
    state: &State,
    sid: u64,
    store: crate::generated_wire::Path,
    reference: crate::wire::BoundedText<{ crate::generated_wire::LIMIT_TEXT_BYTES }>,
    dest: Option<crate::generated_wire::Path>,
    subpath: Option<crate::generated_wire::Path>,
) -> Result<crate::generated_wire::CheckoutResult, String> {
    use crate::depot::BoxDepot as _;
    let overlay = lock(state).overlay.clone().ok_or("no overlay")?;
    let sid = i64::try_from(sid).map_err(|_| "box id exceeds engine range")?;
    let owner = hydrate_box(&overlay, sid).ok_or("no such box")?;
    if store.as_slice().contains(&0) {
        return Err("git store path contains NUL".into());
    }
    let store = std::str::from_utf8(store.as_slice())
        .map_err(|_| "the git store path is not UTF-8")?;
    let dest = dest.as_ref().map(action_relative_path).transpose()?.unwrap_or("")
        .trim_end_matches('/');
    let subpath = subpath.as_ref().map(action_relative_path).transpose()?.unwrap_or("")
        .trim_end_matches('/');
    let store_path = std::path::Path::new(store);
    let resolved = match gitdepot::resolve_ref(store_path, reference.as_str()) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return Err(format!("no ref or commit {} in store", reference.as_str())),
        Err(gitdepot::Error::Meta(message)) => return Err(message),
        Err(error) => return Err(format!("store: {error}")),
    };
    let layers = gitdepot::store::Store::open(store_path)
        .and_then(|store| store.union()).map_err(|error| format!("store: {error}"))?;
    let (sha, revision) = match resolved {
        gitdepot::Resolved::Commit { sha, .. } => {
            let revision = layers.rev_of(&sha)
                .ok_or_else(|| format!("commit {sha} not in the union store"))?;
            (sha, revision)
        }
        gitdepot::Resolved::TreeTag { tag_sha, tree_idx } => (tag_sha, tree_idx),
    };
    let revision_name = crate::wire::BoundedText::new(sha)
        .map_err(|error| format!("git revision exceeds relation bound: {error:?}"))?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut directories = std::collections::HashSet::new();
    let ensure_directories = |path: &str,
                              directories: &mut std::collections::HashSet<String>| {
        let mut at = 0usize;
        while let Some(index) = path[at..].find('/') {
            let directory = &path[..at + index];
            if directories.insert(directory.to_owned()) {
                owner.set_dir(directory, 0o040755, 0);
            }
            at += index + 1;
        }
    };
    layers.checkout_entries_at(revision, subpath.as_bytes(), &mut |rel, mode, content| {
        let rel = std::str::from_utf8(rel).map_err(|_| gitdepot::Error::Chain(
            "checkout path cannot be represented by the current UTF-8 overlay index".into()))?;
        if rel.contains('\0') || rel.starts_with('/') || std::path::Path::new(rel)
            .components().any(|component| matches!(component,
                std::path::Component::ParentDir | std::path::Component::RootDir
                | std::path::Component::Prefix(_)))
        {
            return Err(gitdepot::Error::Chain(format!("unsafe checkout path {rel:?}")));
        }
        let path = if dest.is_empty() { rel.to_owned() } else { format!("{dest}/{rel}") };
        ensure_directories(&path, &mut directories);
        use gitdepot::layer::Mode;
        match mode {
            Mode::Symlink => {
                let target = std::str::from_utf8(content).map_err(|_| gitdepot::Error::Chain(
                    format!("symlink target for {path:?} is not UTF-8")))?;
                owner.set_symlink(&path, std::path::Path::new(target), 0);
            }
            Mode::Gitlink => {}
            mode => {
                let full_mode = match mode {
                    Mode::File => 0o100644,
                    Mode::Exec => 0o100755,
                    Mode::Other(value) => value,
                    _ => 0o100644,
                };
                let row = owner.ensure_file_row(&path, full_mode, 0);
                let blob = crate::depot::blob_path(sid, row);
                if let Some(directory) = blob.parent() {
                    std::fs::create_dir_all(directory).map_err(|error| gitdepot::Error::Chain(
                        format!("create {}: {error}", directory.display())))?;
                }
                std::fs::write(&blob, content).map_err(|error| gitdepot::Error::Chain(
                    format!("write {}: {error}", blob.display())))?;
                let size = i64::try_from(content.len()).map_err(|_| gitdepot::Error::Chain(
                    format!("checkout content for {path:?} exceeds i64")))?;
                owner.finalize_file(&path, size, 0, 0);
                files = files.checked_add(1).ok_or_else(|| gitdepot::Error::Chain(
                    "checkout file count overflow".into()))?;
                bytes = bytes.checked_add(content.len() as u64).ok_or_else(||
                    gitdepot::Error::Chain("checkout byte count overflow".into()))?;
            }
        }
        Ok(())
    }).map_err(|error| format!("checkout: {error}"))?;
    owner.load_mirror();
    Ok(crate::generated_wire::CheckoutResult {
        revision: revision_name,
        files,
        bytes,
    })
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

    #[test]
    fn typed_box_paths_reject_unsafe_or_unaddressable_overlay_keys() {
        let path = |bytes| crate::wire::BoundedBytes::new(bytes).unwrap();
        assert_eq!(action_relative_path(&path(b"src/main.rs".to_vec())).unwrap(),
                   "src/main.rs");
        assert_eq!(action_relative_path(&path(Vec::new())).unwrap(), "");
        assert!(action_relative_path(&path(b"../host".to_vec())).is_err());
        assert!(action_relative_path(&path(b"/etc/passwd".to_vec())).is_err());
        assert!(action_relative_path(&path(vec![b'a', 0, b'b'])).is_err());
        assert!(action_relative_path(&path(vec![0xff])).is_err());
    }

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
    fn typed_registration_reply_projects_only_at_the_legacy_boundary() {
        use crate::generated_wire::{OciRuntime, RegisterReply, SudRuntime};
        let raw = |value: &[u8]| crate::wire::BoundedBytes::new(value.to_vec()).unwrap();
        let reply = legacy_register_reply(Ok(RegisterReply {
            mount: raw(b"/mnt/7"),
            shared_memory: raw(b"/run/sarun/7"),
            dns: Some(crate::wire::FixedBytes([240, 0, 0, 1])),
            ca_bundle: Some(crate::wire::BoundedBytes::new(b"certificate".to_vec()).unwrap()),
            owner: crate::wire::FixedBytes([0xab; 16]),
            r#box: 7,
            name: crate::wire::BoundedText::new("build".into()).unwrap(),
            capture: true,
            api: false,
            no_host: true,
            oci: Some(OciRuntime {
                environment: Some(crate::wire::BoundedVec::new(vec![raw(b"PATH=/bin")]).unwrap()),
                cwd: Some(raw(b"/work")),
                command: Some(crate::wire::BoundedVec::new(vec![raw(b"make")]).unwrap()),
                entrypoint: None,
                user: Some(raw(b"1000:1000")),
            }),
            sud: Some(SudRuntime {
                upper: raw(b"/upper"),
                lowers: crate::wire::BoundedVec::new(vec![raw(b"/lower")]).unwrap(),
                inramfs_key: crate::wire::BoundedText::new("key".into()).unwrap(),
            }),
        }));
        assert_eq!(reply["mount"], "/mnt/7");
        assert_eq!(reply["dns_ip"], "240.0.0.1");
        assert_eq!(reply["owner_token"], "ab".repeat(16));
        assert_eq!(reply["oci"]["env"][0], "PATH=/bin");
        assert_eq!(reply["sud_lowers"][0], "/lower");
        assert_eq!(reply["_box_sid"], 7);
    }

    #[test]
    fn typed_transport_responses_project_only_at_the_legacy_boundary() {
        use crate::generated_wire::{ErrorCategory, TransportResponse};
        let recorded = legacy_transport_response(Ok(TransportResponse::Recorded { count: 3 }));
        assert_eq!(recorded, json!({"ok": true, "recorded": 3}));
        let error = legacy_transport_response(Ok(TransportResponse::Error {
            category: ErrorCategory::InvalidRequest,
            message: crate::wire::BoundedText::new("bad frame".into()).unwrap(),
        }));
        assert_eq!(error["category"], "invalid_request");
        assert_eq!(error["error"], "bad frame");
        let swept = legacy_transport_response(Ok(TransportResponse::SudIngested {
            count: 2,
            errors: crate::wire::BoundedVec::new(vec![
                crate::wire::BoundedText::new("stale entry".into()).unwrap(),
            ]).unwrap(),
        }));
        assert_eq!(swept["ingested"], 2);
        assert_eq!(swept["errors"][0], "stale entry");
    }

    #[test]
    fn stored_oci_runtime_is_materialized_as_a_closed_type() {
        let runtime = parse_oci_runtime(
            r#"{"config":{"Env":["A=B"],"WorkingDir":"/work","Cmd":["make"]}}"#,
        ).unwrap().unwrap();
        assert_eq!(legacy_bytes(runtime.cwd.unwrap()), "/work");
        assert_eq!(legacy_bytes(runtime.command.unwrap().into_inner().remove(0)), "make");
        assert!(parse_oci_runtime(r#"{"config":{"Cmd":"make"}}"#).is_err());
        assert!(parse_oci_runtime("not json").is_err());
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
    fn flow_handlers_accept_explicit_or_selected_sid() {
        let state: State = Default::default();
        lock(&state).selected = Some("42".into());
        let boxes = std::collections::BTreeMap::new();

        for (verb, value) in [("flows.detail", 1), ("flows.packets", 0)] {
            for args in [vec![json!(value)], vec![json!("42"), json!(value)]] {
                let r = dispatch_ui_verb(&state, verb, &args, &boxes);
                assert_eq!(r["error"], "no flows dir for box", "{verb} {args:?}");
            }
            for args in [
                vec![json!("42")],
                vec![json!(42), json!(value)],
                vec![json!("42"), json!("not numeric")],
            ] {
                let r = dispatch_ui_verb(&state, verb, &args, &boxes);
                assert!(r.get("error").is_some(), "{verb} accepted {args:?}: {r}");
            }
        }

        assert_eq!(
            dispatch_ui_verb(&state, "flows.detail", &[json!(0)], &boxes)["error"],
            "flow frame must be positive"
        );
        assert_eq!(
            dispatch_ui_verb(&state, "flows.packets", &[json!(-1)], &boxes)["error"],
            "bad args"
        );
    }
}
