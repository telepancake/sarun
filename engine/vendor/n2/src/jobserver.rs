//! A minimal GNU-make jobserver CLIENT.
//!
//! sarun: this is a vendored addition. n2 reads the jobserver advertised by its
//! parent in `MAKEFLAGS` and draws tokens from it, so n2's parallelism is bounded
//! by the same shared pool every other consumer in the build tree draws from —
//! the engine, recursive in-process makes, and forked tools like
//! `gcc -flto=jobserver`. Total jobs running at once across the whole tree stay
//! within the pool's N, instead of each tool independently fanning out to -jN.
//!
//! Protocol (the classic POSIX form, plus the 4.4 fifo form):
//!   MAKEFLAGS contains `--jobserver-auth=R,W` or `--jobserver-fds=R,W` (a pair
//!   of inherited fds) or `--jobserver-auth=fifo:PATH`. A client reads one byte
//!   to claim a slot and writes it back to release. Every client also has one
//!   implicit token: it may always run a single job without acquiring, which is
//!   what makes the protocol deadlock-free under recursion.
//!
//! sarun: when a fifo path is advertised (the engine's FUSE-served pool), n2 opens
//! its OWN handle to it with O_NONBLOCK, so `try_acquire` is a clean non-blocking
//! read that never stalls the scheduler — independent of the blocking handles
//! that forked tools inherit. All handles are opens of the same pool.
//!
//! Absent any jobserver in the environment, [`Client::from_env`] returns None and
//! n2 falls back to its own `--parallelism`, unchanged.

/// A connection to the jobserver: either our own opened fifo handle (`owned`,
/// non-blocking) or a pair of inherited fds (blocking).
pub struct Client {
    read_fd: i32,
    write_fd: i32,
    /// We opened the fd (a fifo path) and must close it; it is O_NONBLOCK.
    owned: bool,
}

/// Find `name=` in a MAKEFLAGS string and return its value token.
fn flag_value<'a>(makeflags: &'a str, name: &str) -> Option<&'a str> {
    makeflags
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix(name))
}

/// Parse `R,W` into a pair of fds.
fn parse_fd_pair(v: &str) -> Option<(i32, i32)> {
    let (r, w) = v.split_once(',')?;
    Some((r.trim().parse().ok()?, w.trim().parse().ok()?))
}

/// Extract the fifo path from either auth key, if present.
fn fifo_path(makeflags: &str) -> Option<&str> {
    flag_value(makeflags, "--jobserver-auth=")
        .or_else(|| flag_value(makeflags, "--jobserver-fds="))
        .and_then(|v| v.strip_prefix("fifo:"))
}

impl Client {
    /// Connect to the jobserver named in `MAKEFLAGS`, if any.
    pub fn from_env() -> Option<Client> {
        let mf = std::env::var("MAKEFLAGS").ok()?;
        // Prefer the fifo form: open our OWN non-blocking handle so try_acquire
        // never blocks the scheduler (forked tools open their own blocking ones).
        if let Some(path) = fifo_path(&mf) {
            let c = std::ffi::CString::new(path).ok()?;
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
            if fd < 0 {
                return None;
            }
            return Some(Client { read_fd: fd, write_fd: fd, owned: true });
        }
        // Legacy fd form: a pair of inherited (blocking) fds we must not close.
        let val = flag_value(&mf, "--jobserver-auth=")
            .or_else(|| flag_value(&mf, "--jobserver-fds="))?;
        let (read_fd, write_fd) = parse_fd_pair(val)?;
        if unsafe { libc::fcntl(read_fd, libc::F_GETFD) } < 0
            || unsafe { libc::fcntl(write_fd, libc::F_GETFD) } < 0
        {
            return None;
        }
        Some(Client { read_fd, write_fd, owned: false })
    }

    /// Whether a jobserver is advertised at all (a string check — no fd opened).
    pub fn present() -> bool {
        std::env::var("MAKEFLAGS")
            .map(|m| m.contains("--jobserver-auth=") || m.contains("--jobserver-fds="))
            .unwrap_or(false)
    }

    /// The `-jN` count advertised in MAKEFLAGS, if present — used to size n2's
    /// runner cap so it can hold up to N concurrent tasks (the pool still does
    /// the real bounding).
    pub fn jobs_hint() -> Option<usize> {
        let mf = std::env::var("MAKEFLAGS").ok()?;
        for tok in mf.split_whitespace() {
            if let Some(rest) = tok.strip_prefix("-j") {
                if let Ok(n) = rest.parse::<usize>() {
                    return Some(n);
                }
            }
            if let Some(rest) = tok.strip_prefix("--jobs=") {
                if let Ok(n) = rest.parse::<usize>() {
                    return Some(n);
                }
            }
        }
        None
    }

    /// Claim a token without blocking. None ⇒ the pool is momentarily empty.
    /// Our own fifo handle is O_NONBLOCK, so a bare read returns at once (EAGAIN
    /// ⇒ None). For inherited blocking fds we poll first to avoid stalling.
    pub fn try_acquire(&self) -> Option<u8> {
        if !self.owned {
            let mut pfd = libc::pollfd { fd: self.read_fd, events: libc::POLLIN, revents: 0 };
            let r = unsafe { libc::poll(&mut pfd, 1, 0) };
            if r <= 0 || (pfd.revents & libc::POLLIN) == 0 {
                return None;
            }
        }
        let mut b = [0u8; 1];
        let n = unsafe { libc::read(self.read_fd, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 { Some(b[0]) } else { None }
    }

    /// Return a token to the pool.
    pub fn release(&self, token: u8) {
        let b = [token];
        unsafe { libc::write(self.write_fd, b.as_ptr() as *const libc::c_void, 1) };
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.owned {
            unsafe { libc::close(self.read_fd) };
        }
    }
}
