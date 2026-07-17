//! In-process self-unwind for the `stuck` diagnostic.
//!
//! Under `--sud` the box's syscalls are dispatched through a userland loader
//! that maps the engine binary at an address an *external* gdb doesn't expect,
//! so `gdb -p` resolves the engine's own Rust frames to `?? ()`. The only way
//! to get a real, symbolized stack out of a spinning sud thread is to have the
//! thread unwind *itself* — it knows exactly where it's loaded and still has
//! the retained line-table debug info (`[profile.release] debug`).
//!
//! Mechanism: at box startup (`brush_sh`) we install a handler for a realtime
//! signal on an fd the runner pre-opened to a host file (`SARUN_STUCK_FD`,
//! inherited across the sud execs — writes to an already-open host fd do not
//! perform any pathname operation and therefore bypass SarunFs). On
//! `stuck`, the engine truncates that file, `tgkill`s each on-CPU thread with
//! the same signal, and reads back the blocks. `std::backtrace` does the
//! symbolization in-process via gimli reading `/proc/self/exe`.

use std::sync::atomic::{AtomicI32, Ordering};

/// Host file fd the handler writes backtraces to (inherited from the runner
/// via `SARUN_STUCK_FD`). -1 until `install()` picks it up.
static DUMP_FD: AtomicI32 = AtomicI32::new(-1);

/// The realtime signal every party agrees on. Computed the same way in the
/// runner (which never sends it, only sizes the disposition), the box (which
/// handles it), and the engine (which sends it) — same musl binary, so
/// `SIGRTMIN` is identical everywhere. Chosen high in the RT range to avoid
/// tokio/runtime signals.
pub fn dump_signal() -> i32 {
    libc::SIGRTMIN() + 5
}

/// Called by the runner: open the host sink file, hand its fd to the box via
/// the environment (NOT close-on-exec, so it survives the sud wrapper execs),
/// and return the path the engine will read. `key` is a value the `stuck`
/// verb can recompute (the runner's host pid). Returns None on failure — the
/// diagnostic then falls back to the /proc-only view.
pub fn sink_path(key: i32) -> String {
    format!("/tmp/sarun-stuck-{key}.bt")
}

/// Runner side: create the sink file and expose its fd to the box. Best-effort.
pub fn runner_setup(cmd: &mut std::process::Command, key: i32) {
    use std::os::unix::io::IntoRawFd;
    let path = sink_path(key);
    let f = match std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(true)
        .mode(0o600).open(&path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let fd = f.into_raw_fd();
    // Clear FD_CLOEXEC so the fd survives the launcher → wrapper → binary exec
    // chain and reaches the in-box brush.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    cmd.env("SARUN_STUCK_FD", fd.to_string());
    // Leak the fd deliberately: it must stay open in this runner (and be
    // inherited) for the box's lifetime; the runner exits when the box does.
}

use std::os::unix::fs::OpenOptionsExt;

/// Box side: if the runner handed us a sink fd, install the self-unwind
/// handler on `dump_signal()`. Idempotent-ish; call once at box startup.
pub fn install() {
    let fd: i32 = match std::env::var("SARUN_STUCK_FD")
        .ok().and_then(|s| s.parse().ok()) {
        Some(fd) => fd,
        None => return,
    };
    DUMP_FD.store(fd, Ordering::SeqCst);
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        // SA_RESTART so a signalled blocking syscall in an unrelated thread
        // resumes instead of failing with EINTR (we only aim at R-state
        // threads, but be defensive).
        sa.sa_flags = libc::SA_RESTART | libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(dump_signal(), &sa, std::ptr::null_mut());
    }
}

// x86_64 `gregs` indices (musl == glibc enum ordering).
const REG_RBP: usize = 10;
const REG_RIP: usize = 16;
// Byte offset of `uc_mcontext.gregs` within `ucontext_t` on x86_64 musl:
// uc_flags(8) + uc_link(8) + uc_stack{ss_sp(8)+ss_flags(4)+pad(4)+ss_size(8)}
// = 40. gregs[i] then lives at OFF_GREGS + i*8.
const OFF_GREGS: usize = 40;

/// Signal handler: seed a frame-pointer walk from the INTERRUPTED registers in
/// the signal `ucontext` (not the handler's own stack — that's why the earlier
/// `std::backtrace` attempt yielded a single frame: it can't cross the signal
/// trampoline). Writes the raw return addresses, framed by a `=== tid N ===`
/// header, to the sink fd; the engine symbolizes them offline with addr2line
/// against the retained debug info (sarun is a fixed-address ET_EXEC, so a
/// runtime address IS its ELF vaddr — no relocation to undo). CRITICAL: only
/// memory reads + one write(2), NO other syscalls. Under sud any path-based
/// syscall (e.g. reading /proc/self/maps) would re-enter the SIGSYS dispatcher
/// from inside this handler and can wedge the thread — so we do none.
extern "C" fn handler(_sig: i32, _info: *mut libc::siginfo_t,
                      ctx: *mut libc::c_void) {
    let fd = DUMP_FD.load(Ordering::SeqCst);
    if fd < 0 || ctx.is_null() { return; }
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let greg = |i: usize| -> u64 {
        unsafe { *((ctx as *const u8).add(OFF_GREGS + i * 8) as *const u64) }
    };
    let pc = greg(REG_RIP);
    let mut fp = greg(REG_RBP);

    let mut out = format!("=== tid {tid} ===\n0x{pc:x}\n");
    // Frame-pointer chain: at each frame, [fp] = saved rbp, [fp+8] = return
    // address. Forced frame pointers (see .cargo/config.toml) make this valid.
    let mut prev = fp;
    for _ in 0..64 {
        if fp == 0 || (fp & 7) != 0 { break; }
        // Guard: don't dereference a frame pointer that isn't strictly above
        // the previous one (stack grows down; a good chain is monotonic).
        if fp < prev { break; }
        let saved = unsafe { *(fp as *const u64) };
        let ret = unsafe { *((fp + 8) as *const u64) };
        if ret == 0 { break; }
        out.push_str(&format!("0x{ret:x}\n"));
        if saved <= fp { break; }
        prev = fp;
        fp = saved;
    }

    let bytes = out.as_bytes();
    let mut off = 0usize;
    while off < bytes.len() {
        let n = unsafe {
            libc::write(fd, bytes[off..].as_ptr().cast(), bytes.len() - off)
        };
        if n <= 0 { break; }
        off += n as usize;
    }
}
