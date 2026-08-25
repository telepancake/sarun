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
static JOBSERVER_FDS: std::sync::OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
static LOCAL_JOBSERVER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct LocalJobserver {
    path: std::path::PathBuf,
    _server: std::os::fd::OwnedFd,
    _legacy_read: std::os::fd::OwnedFd,
    _legacy_write: std::os::fd::OwnedFd,
}

impl Drop for LocalJobserver {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A jobserver advertisement and, for a standalone top-level build, ownership
/// of the FIFO behind it. Keeping this value alive keeps recursive consumers on
/// the same bounded pool; dropping it removes the private FIFO.
pub struct Advertisement {
    flags: String,
    _local: Option<LocalJobserver>,
}

impl Advertisement {
    pub fn flags(&self) -> &str {
        &self.flags
    }
}

/// Configure the host-owned endpoint Bumba should advertise. The endpoint is
/// process-wide, but every build composes its own logical `MAKEFLAGS` value.
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

fn host_advertisement(local_jobs: usize) -> Option<String> {
    let path = JOBSERVER_PATH.get()?;
    let &(r, w) = JOBSERVER_FDS
        .get_or_init(|| {
            use std::os::unix::ffi::OsStrExt as _;
            let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
            let r = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
            if r < 0 {
                return None;
            }
            let w = unsafe { libc::dup(r) };
            if w < 0 {
                unsafe { libc::close(r) };
                return None;
            }
            clear_cloexec(r);
            clear_cloexec(w);
            Some((r, w))
        })
        .as_ref()?;
    Some(format!(
        "-j{local_jobs} --jobserver-auth=fifo:{} --jobserver-fds={r},{w}",
        path.display()
    ))
}

fn local_advertisement(local_jobs: usize) -> Option<Advertisement> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::sync::atomic::Ordering;

    if local_jobs <= 1 {
        return None;
    }
    // A FIFO is finite. Real CPU counts are far below this; rejecting an
    // absurd request avoids an unbounded allocation or blocking while seeding.
    let token_count = local_jobs.checked_sub(1)?;
    if token_count > 32_768 {
        return None;
    }
    let id = LOCAL_JOBSERVER_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "bumba-jobserver-{}-{id}.fifo",
        std::process::id()
    ));
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    if unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) } < 0 {
        return None;
    }
    let fail = || {
        let _ = std::fs::remove_file(&path);
        None
    };
    let server_fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if server_fd < 0 {
        return fail();
    }
    let server = unsafe { std::os::fd::OwnedFd::from_raw_fd(server_fd) };
    let legacy_read_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
    if legacy_read_fd < 0 {
        return fail();
    }
    let legacy_read = unsafe { std::os::fd::OwnedFd::from_raw_fd(legacy_read_fd) };
    let legacy_write_fd = unsafe { libc::dup(legacy_read_fd) };
    if legacy_write_fd < 0 {
        return fail();
    }
    let legacy_write = unsafe { std::os::fd::OwnedFd::from_raw_fd(legacy_write_fd) };
    clear_cloexec(legacy_read_fd);
    clear_cloexec(legacy_write_fd);

    let tokens = vec![b'+'; token_count];
    let written = unsafe {
        libc::write(
            server_fd,
            tokens.as_ptr().cast::<libc::c_void>(),
            tokens.len(),
        )
    };
    if written != token_count as isize {
        return fail();
    }
    let flags = format!(
        "-j{local_jobs} --jobserver-auth=fifo:{} --jobserver-fds={legacy_read_fd},{legacy_write_fd}",
        path.display()
    );
    Some(Advertisement {
        flags,
        _local: Some(LocalJobserver {
            path,
            _server: server,
            _legacy_read: legacy_read,
            _legacy_write: legacy_write,
        }),
    })
}

/// Return the jobserver words to add to one invocation's logical `MAKEFLAGS`.
///
/// This deliberately does not mutate `std::env`: Bumba hosts unrelated makes
/// in one process, and the first build's `-jN` must not leak into every later
/// make/ninja invocation. A configured host pool is reused; otherwise Bumba
/// creates a private, scoped FIFO for this top-level standalone build.
pub fn advertisement(local_jobs: usize) -> Advertisement {
    if let Some(flags) = host_advertisement(local_jobs.max(1)) {
        return Advertisement {
            flags,
            _local: None,
        };
    }
    local_advertisement(local_jobs.max(1)).unwrap_or_else(|| Advertisement {
        flags: format!("-j{}", local_jobs.max(1)),
        _local: None,
    })
}
