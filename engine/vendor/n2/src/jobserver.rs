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
//!   of inherited pipe fds) or `--jobserver-auth=fifo:PATH`. A client reads one
//!   byte to claim a slot and writes it back to release. Every client also has
//!   one implicit token: it may always run a single job without acquiring, which
//!   is what makes the protocol deadlock-free under recursion.
//!
//! Absent any jobserver in the environment, [`Client::from_env`] returns None and
//! n2 falls back to its own `--parallelism`, unchanged.

/// A connection to an inherited jobserver pipe (or fifo).
pub struct Client {
    read_fd: i32,
    write_fd: i32,
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

impl Client {
    /// Connect to the jobserver named in `MAKEFLAGS`, if any.
    pub fn from_env() -> Option<Client> {
        let mf = std::env::var("MAKEFLAGS").ok()?;
        // Prefer --jobserver-auth (current), fall back to --jobserver-fds (legacy).
        let val = flag_value(&mf, "--jobserver-auth=")
            .or_else(|| flag_value(&mf, "--jobserver-fds="))?;

        if let Some(path) = val.strip_prefix("fifo:") {
            // Open the fifo read-write so neither end ever sees EOF/EPIPE while
            // the build runs (the standard client trick).
            let c = std::ffi::CString::new(path).ok()?;
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR) };
            if fd < 0 {
                return None;
            }
            return Some(Client { read_fd: fd, write_fd: fd });
        }

        let (read_fd, write_fd) = parse_fd_pair(val)?;
        // Validate the fds are actually open (a stale MAKEFLAGS from a parent
        // that already closed them would otherwise wedge us).
        if unsafe { libc::fcntl(read_fd, libc::F_GETFD) } < 0
            || unsafe { libc::fcntl(write_fd, libc::F_GETFD) } < 0
        {
            return None;
        }
        Some(Client { read_fd, write_fd })
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
    /// The pipe stays blocking (forked tools require that), so poll first and
    /// only read when a byte is waiting.
    pub fn try_acquire(&self) -> Option<u8> {
        let mut pfd = libc::pollfd { fd: self.read_fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r <= 0 || (pfd.revents & libc::POLLIN) == 0 {
            return None;
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
