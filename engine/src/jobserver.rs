//! A per-build GNU-make-style jobserver, shared by every parallel consumer in a
//! single build tree: the in-process n2 (ninja) scheduler, recursive in-process
//! makes, and any external tool a recipe forks that speaks the protocol
//! (`gcc -flto=jobserver`, a real sub-`make`, …). One pool of N permission
//! tokens bounds the total number of jobs running at once to N, so one build
//! cannot oversubscribe the machine no matter how many tools inside it each
//! think they may run `-jN`.
//!
//! Transport is the classic POSIX pipe protocol GNU make has used since the
//! beginning: a pipe pre-filled with N-1 one-byte tokens (the Nth is the
//! holder's implicit token — every make/scheduler may run one job for free).
//! A client reads one byte to claim a slot and writes the same byte back to
//! release it. The read/write fd numbers are advertised to children through
//! `MAKEFLAGS` (`--jobserver-auth=R,W`, plus the legacy `--jobserver-fds=R,W`).
//! Recipes run in this same box process and inherit the fds (we clear CLOEXEC,
//! the same way runner.rs does for the fds that must survive bwrap exec), so a
//! forked `gcc -flto=jobserver` draws tokens from the very same pool as the
//! in-process scheduler.
//!
//! Scope is per-build: the singleton is created the first time a make/ninja
//! builtin runs in this box process (one top-level build per box), and every
//! recursive sub-instance reuses it. Two independent `sarun run` boxes are two
//! processes and therefore two pools — exactly like running `make -j` in two
//! separate terminals, where the OS, not make, arbitrates between them.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The token byte. The value is arbitrary for a single-pool protocol; we write
/// back whatever we read, and seed with this. GNU make traditionally uses a mix
/// of bytes for accounting it doesn't need here.
const TOKEN: u8 = b'+';

/// Requested parallelism (from a `-jN` on the top-level make/ninja). 0 ⇒ use the
/// CPU count. Only the first build to call [`ensure`] fixes the pool size; later
/// (recursive) requests are ignored, matching make — a sub-make inherits the
/// jobserver, it does not resize it.
static REQUESTED: AtomicUsize = AtomicUsize::new(0);

static JOBSERVER: OnceLock<JobServer> = OnceLock::new();

pub struct JobServer {
    read_fd: i32,
    write_fd: i32,
    /// Total slots N (1 implicit + N-1 in the pipe).
    jobs: usize,
}

fn clear_cloexec(fd: i32) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        if f >= 0 {
            libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC);
        }
    }
}

/// Record the parallelism a top-level build asked for, before the pool exists.
/// No effect once the pool has been created (the top-level `-j` wins).
pub fn request_jobs(n: usize) {
    if JOBSERVER.get().is_none() {
        REQUESTED.store(n, Ordering::SeqCst);
    }
}

/// The CPU count, the default pool size when parallelism is wanted but no count
/// was given (a bare `-j`, or ninja's parallel-by-default).
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

fn effective_jobs() -> usize {
    match REQUESTED.load(Ordering::SeqCst) {
        0 => std::thread::available_parallelism().map_or(1, |p| p.get()),
        n => n,
    }
}

/// Materialize (once) and return the build's jobserver. The first call fixes the
/// pool size from [`request_jobs`] (or the CPU count) and advertises the fds in
/// `MAKEFLAGS`; subsequent calls return the same pool.
pub fn ensure() -> &'static JobServer {
    JOBSERVER.get_or_init(|| JobServer::create(effective_jobs()))
}

/// The jobserver if it has been created, else None (no build has started one).
pub fn get() -> Option<&'static JobServer> {
    JOBSERVER.get()
}

impl JobServer {
    fn create(jobs: usize) -> JobServer {
        let jobs = jobs.max(1);
        let mut fds = [0i32; 2];
        // SAFETY: standard pipe(2). On failure we fall back to a serial pool
        // (jobs=1, fds=-1) so a build still runs, just without parallelism.
        let ok = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0;
        if !ok {
            return JobServer { read_fd: -1, write_fd: -1, jobs: 1 };
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // The recipe-forked tools (gcc, sub-make) must inherit these across
        // exec, so clear CLOEXEC — same requirement as runner.rs's box fds.
        clear_cloexec(read_fd);
        clear_cloexec(write_fd);

        // Seed N-1 tokens; the holder keeps the implicit Nth.
        let buf = [TOKEN];
        for _ in 0..jobs.saturating_sub(1) {
            // A 1-byte write to a fresh pipe never blocks (capacity ≫ N).
            unsafe { libc::write(write_fd, buf.as_ptr() as *const libc::c_void, 1) };
        }

        let server = JobServer { read_fd, write_fd, jobs };
        // Advertise to children exactly once, before any recipe runs. Only a
        // real (jobs>1) pool is advertised: with -j1 there is nothing to share,
        // and GNU make likewise omits the jobserver, so `gcc -flto=jobserver`
        // sees none and stays serial.
        if jobs > 1 {
            server.export_makeflags();
        }
        server
    }

    /// Total slots (N). 1 ⇒ serial.
    pub fn jobs(&self) -> usize {
        self.jobs
    }

    pub fn read_fd(&self) -> i32 {
        self.read_fd
    }

    pub fn write_fd(&self) -> i32 {
        self.write_fd
    }

    /// Append the jobserver auth (and `-jN`) to `MAKEFLAGS` so every recipe —
    /// brush builtins and forked binaries alike — inherits it. Done once, inside
    /// the OnceLock init, before any recipe thread spawns.
    fn export_makeflags(&self) {
        let auth = format!(
            "-j{} --jobserver-auth={},{} --jobserver-fds={},{}",
            self.jobs, self.read_fd, self.write_fd, self.read_fd, self.write_fd,
        );
        let combined = match std::env::var("MAKEFLAGS") {
            Ok(prev) if !prev.trim().is_empty() => format!("{prev} {auth}"),
            _ => auth,
        };
        // SAFETY: runs once, in the OnceLock initializer, before the build
        // spawns recipe threads or forks tools — no concurrent env reader yet.
        unsafe { std::env::set_var("MAKEFLAGS", combined) };
    }

    /// Try to claim a token without blocking. Returns the token byte on success,
    /// None if the pool is currently empty (all slots in use). Callers must
    /// always allow one implicit job WITHOUT a token so the build can never
    /// deadlock waiting on a pool it has fully drained itself.
    ///
    /// The pipe stays in blocking mode (forked `gcc` requires that), so we
    /// `poll()` with a zero timeout to test readability and only then read. A
    /// concurrent external client could, in a tight race, take the byte between
    /// the poll and the read and briefly block us; in practice external grabbers
    /// are rare and also reading, so the byte is there. Correctness (the ≤N
    /// bound and deadlock-freedom) does not depend on the race never happening.
    pub fn try_acquire(&self) -> Option<u8> {
        if self.read_fd < 0 || self.jobs <= 1 {
            return None;
        }
        let mut pfd = libc::pollfd { fd: self.read_fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r <= 0 || (pfd.revents & libc::POLLIN) == 0 {
            return None;
        }
        let mut b = [0u8; 1];
        let n = unsafe { libc::read(self.read_fd, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 { Some(b[0]) } else { None }
    }

    /// Return a previously-acquired token to the pool.
    pub fn release(&self, token: u8) {
        if self.write_fd < 0 {
            return;
        }
        let b = [token];
        // Writing back a token we hold can never overflow the pipe (we only
        // ever return bytes we took out), so this does not block.
        unsafe { libc::write(self.write_fd, b.as_ptr() as *const libc::c_void, 1) };
    }
}
