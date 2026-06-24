//! A minimal GNU-make jobserver client for kati's PARALLEL executor.
//!
//! sarun: a vendored addition. When a `-jN` (N>1) build advertises the engine's
//! slip pool in MAKEFLAGS (`--jobserver-auth=fifo:PATH`), kati's parallel
//! scheduler draws a slip per concurrent recipe beyond its one implicit job, so
//! its subshells share the one machine-wide pool with n2, sub-makes, and forked
//! tools. The serial executor never constructs this — the rkati↔make corpus runs
//! without `-j` and is unaffected.
//!
//! We open our OWN non-blocking handle to the fifo path so try_acquire never
//! stalls the scheduler; forked tools open their own (blocking) handles.

/// A non-blocking handle on the engine slip pool (the FUSE fifo).
pub struct Client {
    fd: i32,
}

fn fifo_path(makeflags: &str) -> Option<&str> {
    makeflags.split_whitespace().find_map(|tok| {
        tok.strip_prefix("--jobserver-auth=")
            .or_else(|| tok.strip_prefix("--jobserver-fds="))
            .and_then(|v| v.strip_prefix("fifo:"))
    })
}

impl Client {
    /// Connect to the fifo jobserver named in MAKEFLAGS, if any.
    pub fn from_env() -> Option<Client> {
        let mf = std::env::var("MAKEFLAGS").ok()?;
        let path = fifo_path(&mf)?;
        let c = std::ffi::CString::new(path).ok()?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
        if fd < 0 {
            return None;
        }
        Some(Client { fd })
    }

    /// Claim a slip without blocking. None ⇒ pool momentarily empty.
    pub fn try_acquire(&self) -> Option<u8> {
        let mut b = [0u8; 1];
        let n = unsafe { libc::read(self.fd, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 { Some(b[0]) } else { None }
    }

    /// Return a slip to the pool.
    pub fn release(&self, token: u8) {
        let b = [token];
        unsafe { libc::write(self.fd, b.as_ptr() as *const libc::c_void, 1) };
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}
