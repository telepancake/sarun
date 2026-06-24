//! The engine-global slip pool — the authority side of the jobserver.
//!
//! sarun: one pool of N "slips" (jobserver tokens) for the WHOLE engine, sized to
//! the machine (CPU count) and living in the long-lived `serve` process. Every
//! parallel consumer in every box draws from it: the in-process n2/kati
//! schedulers, recursive makes, and forked tools like `gcc -flto=jobserver`. A
//! build's own `-jN` is just that build's local concurrency cap; the machine
//! pool is the system-wide bound, so three boxes each running `make -j32` on an
//! 8-CPU host still never exceed 8 concurrent jobs.
//!
//! Unlike a raw GNU-make pipe (where a token read into a crashed process's memory
//! is lost forever), the engine mediates every acquire/release — it is reached
//! through a synthetic FUSE file, so each operation carries the caller pid. That
//! lets the pool keep a per-pid LEDGER and, when the process tracing observes a
//! pid (or a whole box) exit, REAP the slips that pid never returned.
//!
//! This module is the pure pool logic, decoupled from FUSE via [`SlipReply`] so it
//! can be unit-tested without a mount. The FUSE handlers (overlay.rs) wrap their
//! deferred `ReplyData` in a `SlipReply` and call [`Pool::acquire`] / [`release`];
//! the teardown path (control.rs) and the pidfd reaper call [`reap_pid`].

// Foundation increment: the pool logic and its unit tests landed first; the FUSE
// handlers and pidfd reaper are now wired. `reap_box` and the cfg(test) helpers
// are the only not-yet-called items.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// The one engine-global pool, created on first use, sized to the machine.
static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();

/// Machine capacity = CPU count, or `SARUN_JOBS` if set (>0) — the latter lets
/// tests pin the pool to a known small size for deterministic bound checks.
pub fn slots() -> usize {
    std::env::var("SARUN_JOBS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()))
}

/// The engine-global slip pool (one per `serve` process → shared by all boxes).
pub fn global() -> &'static Mutex<Pool> {
    POOL.get_or_init(|| Mutex::new(Pool::new(slots())))
}

// ── Reaper ───────────────────────────────────────────────────────────────────
//
// A slip read into a process's memory is lost if that process dies without
// writing it back (the classic GNU-make pipe leak). Because every acquire is a
// FUSE op, the engine knows the holder pid; here we watch each holder with a
// pidfd and, when the kernel signals its exit, return its slips to the pool. This
// is event-driven (no /proc polling) and naturally covers whole-box teardown —
// every process exit reaps. Granted-to-waiters happen inside reap_pid.

struct Reaper {
    epfd: i32,
    /// holder pid -> its pidfd (so we can EPOLL_CTL_DEL + close on exit). Also
    /// the dedup set: a pid present here is already watched.
    watched: Mutex<HashMap<i32, i32>>,
}

static REAPER: OnceLock<Arc<Reaper>> = OnceLock::new();

fn pidfd_open(pid: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }
}

fn reaper() -> &'static Arc<Reaper> {
    REAPER.get_or_init(|| {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        let r = Arc::new(Reaper { epfd, watched: Mutex::new(HashMap::new()) });
        let r2 = r.clone();
        std::thread::Builder::new()
            .name("slip-reaper".into())
            .spawn(move || r2.run())
            .ok();
        r
    })
}

impl Reaper {
    /// epoll loop: a watched pidfd becomes readable when its process exits.
    fn run(&self) {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
        loop {
            let n = unsafe {
                libc::epoll_wait(self.epfd, events.as_mut_ptr(), events.len() as i32, -1)
            };
            if n < 0 {
                // EINTR or a torn-down epfd: brief pause, then retry.
                continue;
            }
            for ev in &events[..n as usize] {
                let pid = ev.u64 as i32;
                // Stop watching: DEL + close the pidfd before reaping.
                if let Some(pidfd) = self.watched.lock().unwrap().remove(&pid) {
                    unsafe {
                        libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, pidfd, std::ptr::null_mut());
                        libc::close(pidfd);
                    }
                }
                // Return the dead pid's slips to the pool (handing any to blocked
                // waiters). Those waiters were watched when they queued.
                let _granted = global().lock().unwrap().reap_pid(pid);
            }
        }
    }

    /// Begin watching `pid` for exit (idempotent). If the pid is already gone,
    /// reap it now.
    fn watch(&self, pid: i32) {
        {
            let w = self.watched.lock().unwrap();
            if w.contains_key(&pid) {
                return;
            }
        }
        let pidfd = pidfd_open(pid);
        if pidfd < 0 {
            // Already exited (ESRCH) between acquire and here — reap immediately.
            global().lock().unwrap().reap_pid(pid);
            return;
        }
        let mut w = self.watched.lock().unwrap();
        // Re-check under the lock (another thread may have just added it).
        if w.contains_key(&pid) {
            unsafe { libc::close(pidfd) };
            return;
        }
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: pid as u64,
        };
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_ADD, pidfd, &mut ev) };
        if rc < 0 {
            // Couldn't register (e.g. already-ready immediately on a fast exit):
            // close and reap now rather than leak the slip.
            unsafe { libc::close(pidfd) };
            drop(w);
            global().lock().unwrap().reap_pid(pid);
            return;
        }
        w.insert(pid, pidfd);
    }
}

/// Watch `pid` so its slips are reaped when it exits. Call after an acquire that
/// attributed (or queued) a slip to the pid. Must NOT be called while holding the
/// pool lock (it may itself reap, taking that lock).
pub fn watch_pid(pid: i32) {
    reaper().watch(pid);
}

/// The single byte handed out per slip. Value is arbitrary (single-pool
/// protocol); a client writes back whatever it read.
pub const SLIP: u8 = b'+';

/// A pending acquirer's reply channel, decoupled from `fuser`. The FUSE read
/// handler implements this over its deferred `ReplyData`; tests use a mock.
/// Exactly one of `grant`/`deny_again` is called, consuming the reply.
pub trait SlipReply: Send {
    /// Hand the caller a slip (reply to the blocked/queued read with one byte).
    fn grant(self: Box<Self>);
    /// Tell a non-blocking (`O_NONBLOCK`) caller no slip is available right now.
    fn deny_again(self: Box<Self>);
}

/// What the caller must do after an [`Pool::acquire`] that took (or queued for) a
/// slip: start watching this pid for exit so its slips can be reaped. Returned so
/// the lock is dropped before the (syscall-bearing) pidfd registration runs.
#[must_use]
pub enum Watch {
    /// A slip is now (or will be) attributed to this pid — ensure it is watched.
    Pid(i32),
    /// Nothing to watch (denied a non-blocking caller, or pid already watched is
    /// the caller's concern via the ledger; the FUSE layer dedups watches).
    None,
}

pub struct Pool {
    /// Total slips (machine capacity). Fixed at creation.
    total: usize,
    /// Slips not currently held by anyone.
    available: usize,
    /// pid -> number of slips it holds. The reaping ledger.
    ledger: HashMap<i32, u32>,
    /// Blocking acquirers waiting for a slip, in FIFO order.
    waiters: VecDeque<(i32, Box<dyn SlipReply>)>,
}

impl Pool {
    pub fn new(total: usize) -> Pool {
        let total = total.max(1);
        Pool { total, available: total, ledger: HashMap::new(), waiters: VecDeque::new() }
    }

    #[cfg(test)]
    pub fn available(&self) -> usize {
        self.available
    }

    #[cfg(test)]
    pub fn held_by(&self, pid: i32) -> u32 {
        self.ledger.get(&pid).copied().unwrap_or(0)
    }

    /// The number of acquirers currently blocked waiting for a slip.
    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.waiters.len()
    }

    /// Acquire one slip for `pid`. If one is free it is granted immediately;
    /// otherwise, a blocking caller is queued (its reply is fulfilled later by a
    /// `release`/`reap`) and a non-blocking caller is denied at once. Returns
    /// whether the pid now needs exit-watching.
    pub fn acquire(&mut self, pid: i32, reply: Box<dyn SlipReply>, nonblocking: bool) -> Watch {
        if self.available > 0 {
            self.available -= 1;
            *self.ledger.entry(pid).or_insert(0) += 1;
            reply.grant();
            Watch::Pid(pid)
        } else if nonblocking {
            reply.deny_again();
            Watch::None
        } else {
            self.waiters.push_back((pid, reply));
            // Watch even while only queued: if the pid dies before being granted,
            // reap_pid must drop its waiter (below) — watching makes that fire.
            Watch::Pid(pid)
        }
    }

    /// Move one free slip to the head waiter if any, else back to `available`.
    /// Returns the pid that just received a slip (so the caller can watch it).
    fn hand_off_one(&mut self) -> Option<i32> {
        if let Some((wpid, reply)) = self.waiters.pop_front() {
            *self.ledger.entry(wpid).or_insert(0) += 1;
            reply.grant();
            Some(wpid)
        } else {
            self.available += 1;
            None
        }
    }

    /// `pid` returns one slip it holds. The freed slip goes to the next waiter,
    /// or back to the pool. Returns the pid newly granted a slip, if any.
    pub fn release(&mut self, pid: i32) -> Option<i32> {
        match self.ledger.get_mut(&pid) {
            Some(c) if *c > 0 => {
                *c -= 1;
                if *c == 0 {
                    self.ledger.remove(&pid);
                }
            }
            // A release with no recorded holding (double-release, or a slip
            // already reaped): treat as a no-op so we never inflate the pool.
            _ => return None,
        }
        self.hand_off_one()
    }

    /// Reclaim every slip attributed to `pid` (it exited). Its held slips return
    /// to the pool (handed to waiters or made available), and if `pid` itself was
    /// blocked waiting, that waiter is dropped (denied). Returns the pids newly
    /// granted slips, so the caller can watch them.
    pub fn reap_pid(&mut self, pid: i32) -> Vec<i32> {
        // Drop any queued acquire from this dead pid (deny it — nobody will read).
        let mut kept: VecDeque<(i32, Box<dyn SlipReply>)> = VecDeque::new();
        while let Some((wpid, reply)) = self.waiters.pop_front() {
            if wpid == pid {
                reply.deny_again();
            } else {
                kept.push_back((wpid, reply));
            }
        }
        self.waiters = kept;

        let held = self.ledger.remove(&pid).unwrap_or(0);
        let mut granted = Vec::new();
        for _ in 0..held {
            if let Some(p) = self.hand_off_one() {
                granted.push(p);
            }
        }
        granted
    }

    /// Reap every pid in `pids` (whole-box teardown). Returns pids newly granted.
    pub fn reap_box(&mut self, pids: &[i32]) -> Vec<i32> {
        let mut granted = Vec::new();
        for &pid in pids {
            granted.extend(self.reap_pid(pid));
        }
        granted
    }

    /// Total slips (machine capacity).
    #[cfg(test)]
    pub fn total(&self) -> usize {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // A mock reply that records whether it was granted or denied.
    struct MockReply {
        granted: Arc<AtomicUsize>,
        denied: Arc<AtomicUsize>,
    }
    impl SlipReply for MockReply {
        fn grant(self: Box<Self>) {
            self.granted.fetch_add(1, Ordering::SeqCst);
        }
        fn deny_again(self: Box<Self>) {
            self.denied.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn reply(g: &Arc<AtomicUsize>, d: &Arc<AtomicUsize>) -> Box<MockReply> {
        Box::new(MockReply { granted: g.clone(), denied: d.clone() })
    }

    #[test]
    fn acquire_grants_until_empty_then_queues() {
        let g = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let mut p = Pool::new(2);
        let _ = p.acquire(10, reply(&g, &d), false);
        let _ = p.acquire(11, reply(&g, &d), false);
        assert_eq!(g.load(Ordering::SeqCst), 2);
        assert_eq!(p.available(), 0);
        // Pool empty: a blocking acquire queues, a non-blocking one is denied.
        let _ = p.acquire(12, reply(&g, &d), false);
        assert_eq!(p.waiting(), 1);
        let _ = p.acquire(13, reply(&g, &d), true);
        assert_eq!(d.load(Ordering::SeqCst), 1);
        assert_eq!(g.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn release_hands_off_to_waiter_not_pool() {
        let g = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let mut p = Pool::new(1);
        let _ = p.acquire(10, reply(&g, &d), false); // grants, available now 0
        let _ = p.acquire(11, reply(&g, &d), false); // queues
        assert_eq!(g.load(Ordering::SeqCst), 1);
        let granted = p.release(10);
        assert_eq!(granted, Some(11));
        assert_eq!(g.load(Ordering::SeqCst), 2); // waiter 11 got the freed slip
        assert_eq!(p.available(), 0); // went to the waiter, not the pool
        assert_eq!(p.held_by(11), 1);
        assert_eq!(p.held_by(10), 0);
    }

    #[test]
    fn reap_returns_leaked_slips_and_never_exceeds_total() {
        let g = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let mut p = Pool::new(3);
        let _ = p.acquire(10, reply(&g, &d), false);
        let _ = p.acquire(10, reply(&g, &d), false); // pid 10 holds 2
        let _ = p.acquire(11, reply(&g, &d), false); // pid 11 holds 1, pool now empty
        assert_eq!(p.available(), 0);
        // pid 10 dies holding 2 slips it never returned.
        let granted = p.reap_pid(10);
        assert!(granted.is_empty()); // no waiters, so slips go back to the pool
        assert_eq!(p.available(), 2);
        assert_eq!(p.held_by(10), 0);
        // Releasing 11 restores the pool to full; never above total.
        p.release(11);
        assert_eq!(p.available(), 3);
        assert_eq!(p.available(), p.total());
    }

    #[test]
    fn reap_drops_a_dead_pids_pending_waiter() {
        let g = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let mut p = Pool::new(1);
        let _ = p.acquire(10, reply(&g, &d), false); // grant
        let _ = p.acquire(11, reply(&g, &d), false); // 11 queues, waiting for a slip
        assert_eq!(p.waiting(), 1);
        // 11 dies while blocked: its waiter is dropped (denied), not leaked.
        let granted = p.reap_pid(11);
        assert!(granted.is_empty());
        assert_eq!(p.waiting(), 0);
        assert_eq!(d.load(Ordering::SeqCst), 1);
        // Now 10 releasing returns the slip to the pool (no waiters left).
        p.release(10);
        assert_eq!(p.available(), 1);
    }

    #[test]
    fn double_release_does_not_inflate_pool() {
        let g = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let mut p = Pool::new(2);
        let _ = p.acquire(10, reply(&g, &d), false);
        p.release(10);
        p.release(10); // stray second release must be ignored
        assert_eq!(p.available(), 2);
        assert_eq!(p.available(), p.total());
    }
}
