// Banner-style approval queue. Connections needing a human decision are
// pushed onto this queue with a `oneshot`-style completer; the UI claims the
// next pending entry, shows the banner ("[box NAME] curl https://x.com:443
// — [y]es once / [n]o once / [a]llow+save / [d]eny+save"), waits for a key,
// and resolves the entry's verdict. If the TUI is not up, the queue's
// `prompt(...)` short-circuits to Deny — connect-stage timeouts are seconds,
// so blocking on a non-running UI would be a pure footgun.
//
// One global queue. Multiple boxes share it; the UI handles them serially
// in arrival order (later boxes wait their turn).

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

#[derive(Clone, Debug)]
pub struct Ask {
    pub box_name: String,
    pub exe: String,
    pub argv: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    YesOnce, NoOnce,
    AllowSave, DenySave,
}

struct Pending {
    ask: Ask,
    tx: tokio::sync::oneshot::Sender<Verdict>,
}

#[derive(Default)]
pub struct PromptQueue {
    inner: Mutex<Inner>,
    pub ui_active: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Inner {
    queue: std::collections::VecDeque<Pending>,
}

impl PromptQueue {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    /// Enqueue an Ask; resolves to a Verdict (or Deny on UI-down).
    pub async fn ask(self: Arc<Self>, ask: Ask) -> Verdict {
        if !self.ui_active.load(std::sync::atomic::Ordering::Acquire) {
            return Verdict::NoOnce;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner.lock().queue.push_back(Pending { ask, tx });
        // 60s ceiling: if a user wanders off, don't block forever.
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(v)) => v,
            _ => Verdict::NoOnce,
        }
    }

    /// UI side: pop one pending ask and return it + the resolver. UI calls
    /// `verdict.send(v)` after the keystroke.
    pub fn next(&self) -> Option<(Ask, tokio::sync::oneshot::Sender<Verdict>)> {
        let p = self.inner.lock().queue.pop_front()?;
        Some((p.ask, p.tx))
    }
}
