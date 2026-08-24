//! Optional GNU jobserver advertisement for embedded builds.
//!
//! A host may configure one FIFO-like endpoint. Bumba then advertises that
//! endpoint into `MAKEFLAGS`, so n2/rkati and tools forked by recipes draw from
//! the same pool. With no endpoint, local `-j` limits still apply and Bumba does
//! not claim ownership of a machine-wide scheduler.
//!
//! We advertise BOTH protocol forms at once so every client — old or new —
//! connects to the SAME pool:
//!   * `--jobserver-auth=fifo:PATH` — modern make/gcc open the path
//!     themselves; n2 opens its own non-blocking handle.
//!   * `--jobserver-fds=R,W` — a blocking handle we pre-open on that same path and
//!     leave for older tools to inherit.
//! Both forms refer to the same host-owned endpoint.

static JOBSERVER_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Configure the endpoint Bumba should advertise. This is once-per-process,
/// matching the process-global `MAKEFLAGS` inherited by external children.
pub fn set_path(path: impl Into<std::path::PathBuf>) -> Result<(), std::path::PathBuf> {
    JOBSERVER_PATH.set(path.into())
}

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
        let val_after =
            |i: usize| -> Option<usize> { argv.get(i + 1).and_then(|s| s.parse::<usize>().ok()) };
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

/// Advertise the host pool into this build's `MAKEFLAGS`. Idempotent: a
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
    let Some(path) = JOBSERVER_PATH.get() else {
        return;
    };
    // Pre-open a blocking handle on the pool for fd-form children to inherit.
    // open() never blocks (only read()=acquire does), so this is safe even when
    // the pool is momentarily empty. If the path can't be opened we're likely not
    // in a host with the pool mounted — leave MAKEFLAGS alone (serial).
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
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
    // host fds). The handles intentionally live for the host process's lifetime.
    clear_cloexec(r);
    clear_cloexec(w);

    let auth =
        format!(
            "-j{local_jobs} --jobserver-auth=fifo:{} --jobserver-fds={r},{w}",
            path.display()
        );
    let combined = match std::env::var("MAKEFLAGS") {
        Ok(prev) if !prev.trim().is_empty() => format!("{prev} {auth}"),
        _ => auth,
    };
    // SAFETY: runs once per process (idempotent guard above), before the build spawns
    // recipe threads or forks tools — no concurrent env reader yet.
    unsafe { std::env::set_var("MAKEFLAGS", combined) };
}
