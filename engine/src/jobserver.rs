//! Box-side jobserver advertisement.
//!
//! The slip pool itself lives in the engine (slippool.rs) and is served to every
//! box as the FUSE file `/.slopbox-jobserver` (overlay.rs). This module's only job
//! is to advertise that one engine-global pool into a build's `MAKEFLAGS`, so the
//! in-process n2/kati schedulers and any tool a recipe forks (`gcc -flto`, a
//! sub-`make`) all draw slips from it.
//!
//! We advertise BOTH protocol forms at once so every client — old or new —
//! connects to the SAME pool:
//!   * `--jobserver-auth=fifo:/.slopbox-jobserver` — modern make/gcc open the path
//!     themselves; n2 opens its own non-blocking handle.
//!   * `--jobserver-fds=R,W` — a blocking handle we pre-open on that same path and
//!     leave for older tools to inherit.
//! Both are handles onto the one FUSE-backed pool. Because the backing is FUSE
//! (not a kernel pipe), even the shared inherited fd-form handle is per-pid
//! mediated — the engine sees the reader's pid on every acquire, so the pool's
//! ledger and reaping cover fd-form clients too.

/// The box-visible path of the engine's slip pool (overlay synthetic file).
pub const JOBSERVER_PATH: &str = "/.slopbox-jobserver";

/// The CPU count — ninja's parallel-by-default fallback and the bare-`-j` value.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map_or(1, |p| p.get())
}

/// Parse an explicit jobs request from a make/ninja argv: `-jN`, `-j N`,
/// `--jobs=N`, `--jobs N`, or a bare `-j`/`--jobs` (⇒ CPU count). Returns None
/// when no jobs flag is present — the caller decides the default (serial for
/// make, CPU count for ninja). argv[0] is the program name and is skipped.
pub fn explicit_jobs(argv: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        let val_after = |i: usize| -> Option<usize> {
            argv.get(i + 1).and_then(|s| s.parse::<usize>().ok())
        };
        if let Some(rest) = a.strip_prefix("-j") {
            if rest.is_empty() {
                return Some(val_after(i).unwrap_or_else(cpu_count));
            }
            return Some(rest.parse::<usize>().unwrap_or_else(|_| cpu_count()));
        }
        if a == "--jobs" {
            return Some(val_after(i).unwrap_or_else(cpu_count));
        }
        if let Some(rest) = a.strip_prefix("--jobs=") {
            return Some(rest.parse::<usize>().unwrap_or_else(|_| cpu_count()));
        }
        i += 1;
    }
    None
}

fn clear_cloexec(fd: i32) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        if f >= 0 {
            libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC);
        }
    }
}

/// Advertise the engine pool into this build's `MAKEFLAGS`. Idempotent: a
/// recursive sub-make (or a ninja under a parallel make) inherits the already-set
/// `MAKEFLAGS` and returns at once — it joins the same pool rather than opening a
/// second advertisement. `local_jobs` is this build's `-j` cap (n2 uses it as its
/// runner cap; the pool does the system-wide bounding).
pub fn advertise(local_jobs: usize) {
    if std::env::var("MAKEFLAGS")
        .map(|m| m.contains("--jobserver-auth=") || m.contains("--jobserver-fds="))
        .unwrap_or(false)
    {
        return; // inherited from a parent build — same pool, nothing to do
    }
    // Pre-open a blocking handle on the pool for fd-form children to inherit.
    // open() never blocks (only read()=acquire does), so this is safe even when
    // the pool is momentarily empty. If the path can't be opened we're likely not
    // in a box with the pool mounted — leave MAKEFLAGS alone (serial).
    let Ok(cpath) = std::ffi::CString::new(JOBSERVER_PATH) else { return };
    let r = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
    if r < 0 {
        return;
    }
    let w = unsafe { libc::dup(r) };
    if w < 0 {
        unsafe { libc::close(r) };
        return;
    }
    // Children must inherit these across exec (same requirement as runner.rs's
    // box fds). The handles intentionally live for the box process's lifetime.
    clear_cloexec(r);
    clear_cloexec(w);

    let auth = format!(
        "-j{local_jobs} --jobserver-auth=fifo:{JOBSERVER_PATH} --jobserver-fds={r},{w}",
    );
    let combined = match std::env::var("MAKEFLAGS") {
        Ok(prev) if !prev.trim().is_empty() => format!("{prev} {auth}"),
        _ => auth,
    };
    // SAFETY: runs once per box (idempotent guard above), before the build spawns
    // recipe threads or forks tools — no concurrent env reader yet.
    unsafe { std::env::set_var("MAKEFLAGS", combined) };
}
