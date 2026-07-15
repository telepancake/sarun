// Banner-style approval queue. Net rules with `Action::Ask` enqueue an
// Ask here; the UI (TUI) polls `peek()` on each tick and renders the next
// pending prompt as a one-line banner. The user's keystroke goes back as
// a Verdict via `answer(id, verdict)`.
//
// When the TUI is not up (no peeker has called `mark_ui_active()`), the
// dispatcher's `ask()` short-circuits to Deny — connect-stage timeouts
// are seconds; we don't want connections wedged on a non-running UI.
//
// One global queue is enough: only one banner can be visible at a time,
// so we don't need per-box parallelism — concurrent asks queue, the user
// answers them in arrival order. Each Ask has a monotonically-increasing
// id so the UI can match the answer to the open prompt unambiguously.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::oneshot;

#[derive(Clone, Debug)]
pub struct Ask {
    pub id: u64,
    pub box_name: String,
    pub host: String,
    pub port: u16,
    pub scheme: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    YesOnce, NoOnce,
    AllowSave, DenySave,
}

impl Verdict {
    pub fn is_allow(self) -> bool {
        matches!(self, Verdict::YesOnce | Verdict::AllowSave)
    }
    pub fn is_persistent(self) -> bool {
        matches!(self, Verdict::AllowSave | Verdict::DenySave)
    }
}

struct Pending {
    ask: Ask,
    tx: oneshot::Sender<Verdict>,
    deadline: Instant,
}

#[derive(Default)]
pub struct PromptQueue {
    inner: Mutex<Inner>,
    pub ui_active: AtomicBool,
    next_id: AtomicU64,
}

#[derive(Default)]
struct Inner {
    queue: std::collections::VecDeque<Pending>,
}

impl PromptQueue {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    /// UI signals it's up. While this is true, asks block on a oneshot
    /// (up to 60 s); while false, asks deny-out immediately.
    pub fn mark_ui_active(&self, on: bool) {
        self.ui_active.store(on, Ordering::Release);
        if !on {
            // The UI went away — resolve every pending ask as Deny so the
            // dispatcher's tokio task wakes up and tears the conn down.
            let mut g = self.inner.lock();
            while let Some(p) = g.queue.pop_front() {
                // Fire-and-forget by design: the dispatcher waiter may have
                // already hit its 60s timeout and dropped the receiver, in
                // which case the conn is being torn down anyway. Safe to ignore.
                let _ = p.tx.send(Verdict::NoOnce);
            }
        }
    }

    /// Dispatcher side: enqueue an Ask, await a verdict (max 60 s).
    /// Returns NoOnce if the TUI isn't up or the user took too long.
    pub async fn ask(self: &Arc<Self>, box_name: String, host: String,
                     port: u16, scheme: String) -> Verdict {
        if !self.ui_active.load(Ordering::Acquire) { return Verdict::NoOnce; }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let ask = Ask { id, box_name, host, port, scheme };
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.inner.lock();
            g.queue.push_back(Pending {
                ask, tx, deadline: Instant::now() + Duration::from_secs(60),
            });
        }
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(v)) => v,
            _ => Verdict::NoOnce,
        }
    }

    /// UI side: peek the next pending Ask without claiming it. Returns
    /// None when the queue is empty. Idempotent.
    pub fn peek(&self) -> Option<Ask> {
        // Garbage-collect any prompts whose deadline expired so the UI
        // doesn't get stuck rendering a banner for an answer no one's
        // waiting on. (The dispatcher's tokio::time::timeout already
        // delivered NoOnce; the entry just lingers here.)
        let now = Instant::now();
        let mut g = self.inner.lock();
        while g.queue.front().map(|p| p.deadline < now).unwrap_or(false) {
            g.queue.pop_front();
        }
        g.queue.front().map(|p| p.ask.clone())
    }

    /// UI side: answer the pending Ask with the given id. Returns true
    /// if the id was at the head of the queue and the verdict was
    /// delivered to the dispatcher waiter; false on stale id.
    pub fn answer(&self, id: u64, verdict: Verdict) -> bool {
        let mut g = self.inner.lock();
        if g.queue.front().map(|p| p.ask.id) != Some(id) { return false; }
        let p = g.queue.pop_front().unwrap();
        // Fire-and-forget by design: if the dispatcher waiter already timed out
        // and dropped its receiver, the answer is simply moot. Safe to ignore.
        let _ = p.tx.send(verdict);
        true
    }
}
