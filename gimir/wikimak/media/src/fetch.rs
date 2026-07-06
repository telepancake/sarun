//! Network fetch (feature `fetch` only) — the Robot policy, hard-wired.
//!
//! wikitech's upload.wikimedia.org Robot policy is codified here, not
//! left to callers (plan §4):
//!   - ≤2 concurrent connections (a counting semaphore),
//!   - descriptive User-Agent with contact intent,
//!   - `Accept-Encoding: gzip` advertised,
//!   - honor `429 Retry-After`,
//!   - pause after `5xx` (a process-lifetime cooldown timestamp; the
//!     plan's 15-minute pause is the default).
//!
//! Bandwidth (≤25 Mbps) is NOT enforced here — it belongs to the
//! engine's per-host hostlimit budget the plan routes this through; see
//! crate `gaps`. `Accept-Encoding: gzip` is advertised for etiquette but
//! this crate does not enable reqwest's transparent gzip decode (the
//! workspace `reqwest` has no `gzip` feature and the manifest is frozen);
//! image blobs are already-compressed binary the CDN does not re-gzip,
//! so no decode is needed in practice — documented in crate `gaps`.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::StatusCode;

/// The mandatory descriptive User-Agent (plan §4).
pub const USER_AGENT: &str = "wikimak-media/0 (sarun mirror; local use)";
/// Robot policy: ≤2 concurrent connections.
pub const MAX_CONCURRENT: usize = 2;
/// Robot policy: pause 15 min after a 5xx.
pub const COOLDOWN_5XX: Duration = Duration::from_secs(15 * 60);

/// Why a single-URL fetch did not yield bytes.
pub enum FetchError {
    /// 404 — try the next repo in the chain, else negative-cache.
    NotFound,
    /// 429 or 5xx — the host asked us to back off; carries how long.
    Backoff(Duration),
    /// Transport/other error — treat as a soft miss, no negative cache.
    Network(String),
}

/// A counting semaphore (std has none) enforcing max concurrency.
struct Semaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Semaphore {
        Semaphore {
            permits: Mutex::new(n),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let mut p = self.permits.lock().unwrap();
        while *p == 0 {
            p = self.cv.wait(p).unwrap();
        }
        *p -= 1;
        Permit { sem: self }
    }

    fn release(&self) {
        let mut p = self.permits.lock().unwrap();
        *p += 1;
        self.cv.notify_one();
    }
}

struct Permit<'a> {
    sem: &'a Semaphore,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.sem.release();
    }
}

/// Robot-policy-enforcing fetcher. One per [`crate::MediaStore`].
pub struct Robot {
    client: Client,
    gate: Semaphore,
    /// Set when a 429/5xx told us to pause; checked before every fetch.
    cooldown_until: Mutex<Option<Instant>>,
    cooldown_5xx: Duration,
}

impl Default for Robot {
    fn default() -> Robot {
        Robot::new()
    }
}

impl Robot {
    pub fn new() -> Robot {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest blocking client");
        Robot {
            client,
            gate: Semaphore::new(MAX_CONCURRENT),
            cooldown_until: Mutex::new(None),
            cooldown_5xx: COOLDOWN_5XX,
        }
    }

    /// Remaining cooldown, or None if we may fetch now.
    pub fn cooldown_remaining(&self) -> Option<Duration> {
        let guard = self.cooldown_until.lock().unwrap();
        match *guard {
            Some(until) => until.checked_duration_since(Instant::now()),
            None => None,
        }
    }

    fn arm_cooldown(&self, d: Duration) {
        let mut guard = self.cooldown_until.lock().unwrap();
        let until = Instant::now() + d;
        // Extend, never shorten, an existing cooldown.
        *guard = Some(match *guard {
            Some(prev) if prev > until => prev,
            _ => until,
        });
    }

    /// GET `url`, enforcing concurrency, headers, and back-off policy.
    /// A successful 200 returns the raw body bytes.
    pub fn fetch(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        if let Some(rem) = self.cooldown_remaining() {
            return Err(FetchError::Backoff(rem));
        }
        let _permit = self.gate.acquire();
        let resp = self
            .client
            // Advertise gzip for etiquette (see module note on decode).
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "gzip")
            .send();
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Err(FetchError::Network(e.to_string())),
        };
        let status = resp.status();
        if status.is_success() {
            return resp
                .bytes()
                .map(|b| b.to_vec())
                .map_err(|e| FetchError::Network(e.to_string()));
        }
        if status == StatusCode::NOT_FOUND {
            return Err(FetchError::NotFound);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let wait = retry_after(&resp).unwrap_or(self.cooldown_5xx);
            self.arm_cooldown(wait);
            return Err(FetchError::Backoff(wait));
        }
        if status.is_server_error() {
            self.arm_cooldown(self.cooldown_5xx);
            return Err(FetchError::Backoff(self.cooldown_5xx));
        }
        Err(FetchError::Network(format!("unexpected status {status}")))
    }
}

/// Parse `Retry-After` as integer seconds. HTTP-date form is not parsed
/// (falls back to the default cooldown) — documented in crate `gaps`.
fn retry_after(resp: &reqwest::blocking::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}
