//! Process-wide politeness policy for Wikimedia HTTP requests.
//!
//! The engine owns mirror jobs and permits only one Wikipedia mirror job at a
//! time. Within a job, all fetch/decode workers share this process-wide gate;
//! no private socket, helper daemon, or environment protocol is needed.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::StatusCode;

use crate::types::{Error, Result};

// This is only a floor for resource classes whose policy says "at least one
// second".  It is not a claim that one second is safe or sufficient for dump
// downloads; the active-body limit and Retry-After cooldown are independent.
const DEFAULT_MIN_DELAY: Duration = Duration::from_secs(1);
// A 429 without Retry-After still needs a substantial quiet period. Sixty
// seconds is conservative enough that a missing server hint does not turn
// several helper processes into a steady stream of denials.
const DEFAULT_429_DELAY: Duration = Duration::from_secs(60);
const SERVER_ERROR_DELAY: Duration = Duration::from_secs(15 * 60);
// One mirror build may keep three dump streams in flight. The same limit is
// enforced across Kati recipe processes by destination-local slot locks.
const MAX_ACTIVE_REQUESTS: usize = 3;
/// A response-level retry is deliberately singular.  Transport failures are
/// a different class: they have no server refusal and use their own backoff.
pub(crate) const MAX_RESPONSE_RETRIES: u32 = 1;
const ROBOTS_CACHE_ENV: &str = "SARUN_WIKIMEDIA_ROBOTS_CACHE";

#[derive(Debug)]
struct State {
    active: usize,
    next_start: Instant,
    cooldown_until: Instant,
    min_delay: Duration,
}

#[derive(Debug)]
struct Gate {
    state: Mutex<State>,
    wake: Condvar,
}

/// The importer deliberately has several helper processes (Kati recipes),
/// while the policy must cover the whole engine-owned job. Destination-local
/// advisory slot locks enforce the shared concurrency limit without a daemon.
/// A separate short-held schedule lock serializes starts and cooldown updates.
/// `TMPDIR` is set to the destination scratch by the engine.
struct SharedLease {
    _slot: Option<std::fs::File>,
    schedule_path: PathBuf,
}

impl SharedLease {
    fn acquire(min_delay: Duration) -> std::io::Result<Self> {
        let root = std::env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&root)?;
        let schedule_path = root.join("wikimedia-request.schedule");
        loop {
            for index in 0..MAX_ACTIVE_REQUESTS {
                let slot_path = root.join(format!("wikimedia-request-{index}.slot"));
                let slot = std::fs::OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .open(slot_path)?;
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd;
                    if unsafe {
                        libc::flock(slot.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
                    } == 0
                    {
                        Self::wait_for_start(&schedule_path, min_delay)?;
                        return Ok(Self {
                            _slot: Some(slot),
                            schedule_path,
                        });
                    }
                    let error = std::io::Error::last_os_error();
                    let lock_is_busy = error
                        .raw_os_error()
                        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN);
                    if !lock_is_busy && error.kind() != std::io::ErrorKind::Interrupted
                    {
                        return Err(error);
                    }
                }
                #[cfg(not(unix))]
                {
                    Self::wait_for_start(&schedule_path, min_delay)?;
                    return Ok(Self {
                        _slot: Some(slot),
                        schedule_path,
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_for_start(path: &PathBuf, min_delay: Duration) -> std::io::Result<()> {
        loop {
            let mut file = Self::lock_schedule(path)?;
            let stored = Self::read_deadline(&mut file)?;
            let now = unix_micros();
            if stored <= now {
                return Self::write_deadline(
                    &mut file,
                    now.saturating_add(min_delay.as_micros() as u64),
                );
            }
            // Do not hold the schedule lock while waiting: an in-flight
            // response must be able to extend the shared deadline after a
            // 429, and this waiter must then observe that extension.
            drop(file);
            std::thread::sleep(Duration::from_micros(stored - now));
        }
    }

    fn set_delay(&mut self, delay: Duration) {
        let Ok(mut file) = Self::lock_schedule(&self.schedule_path) else {
            return;
        };
        let old = Self::read_deadline(&mut file).unwrap_or(0);
        let requested = unix_micros().saturating_add(delay.as_micros() as u64);
        let _ = Self::write_deadline(&mut file, old.max(requested));
    }

    fn lock_schedule(path: &PathBuf) -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        Ok(file)
    }

    fn read_deadline(file: &mut std::fs::File) -> std::io::Result<u64> {
        file.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text.trim().parse::<u64>().unwrap_or(0))
    }

    fn write_deadline(file: &mut std::fs::File, deadline: u64) -> std::io::Result<()> {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "{deadline}\n")?;
        file.sync_data()
    }
}

fn unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}

impl Gate {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            state: Mutex::new(State {
                active: 0,
                next_start: now,
                cooldown_until: now,
                min_delay: DEFAULT_MIN_DELAY,
            }),
            wake: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> std::io::Result<Permit> {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        loop {
            let now = Instant::now();
            let allowed_at = state.next_start.max(state.cooldown_until);
            if state.active < MAX_ACTIVE_REQUESTS && now >= allowed_at {
                state.active += 1;
                state.next_start = now + state.min_delay;
                let shared = match SharedLease::acquire(state.min_delay) {
                    Ok(shared) => shared,
                    Err(error) => {
                        state.active = state.active.saturating_sub(1);
                        self.wake.notify_all();
                        return Err(error);
                    }
                };
                return Ok(Permit {
                    gate: Some(Arc::clone(self)),
                    shared: Some(shared),
                    released: false,
                });
            }
            let wait_until = if state.active >= MAX_ACTIVE_REQUESTS {
                None
            } else {
                Some(allowed_at)
            };
            state = match wait_until {
                Some(deadline) => self
                    .wake
                    .wait_timeout(state, deadline.saturating_duration_since(now))
                    .expect("Wikimedia gate poisoned")
                    .0,
                None => self.wake.wait(state).expect("Wikimedia gate poisoned"),
            };
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        state.active = state.active.saturating_sub(1);
        self.wake.notify_all();
    }

    fn set_cooldown(&self, delay: Duration) {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        let until = Instant::now() + delay;
        state.cooldown_until = state.cooldown_until.max(until);
        self.wake.notify_all();
    }

    fn set_min_delay(&self, delay: Duration) {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        state.min_delay = state.min_delay.max(delay);
    }
}

static GATE: OnceLock<Arc<Gate>> = OnceLock::new();

fn gate() -> &'static Arc<Gate> {
    GATE.get_or_init(|| Arc::new(Gate::new()))
}

/// A lease for one HTTP request. It remains held until EOF/error, so the
/// active-body limit is real even when several parser workers are present.
pub(crate) struct Permit {
    gate: Option<Arc<Gate>>,
    shared: Option<SharedLease>,
    released: bool,
}

impl Permit {
    pub(crate) fn release_now(&mut self) {
        self.release();
    }

    fn release(&mut self) {
        if !self.released {
            self.released = true;
            // Release the cross-process lease first; only then wake another
            // worker in this process.
            self.shared.take();
            if let Some(gate) = &self.gate {
                gate.release();
            }
        }
    }

    /// Record a response that requires a retry.  The returned delay includes
    /// the conservative policy fallback when the server omitted Retry-After.
    pub(crate) fn retry_delay(&mut self, status: Option<u16>, retry_after: Option<Duration>) -> Duration {
        let delay = match status {
            Some(429) => retry_after.unwrap_or(DEFAULT_429_DELAY),
            Some(code) if (500..600).contains(&code) => {
                retry_after.map_or(SERVER_ERROR_DELAY, |value| value.max(SERVER_ERROR_DELAY))
            }
            _ => retry_after.unwrap_or(DEFAULT_429_DELAY),
        };
        if let Some(gate) = &self.gate {
            gate.set_cooldown(delay);
        }
        let effective_delay = delay.max(self.min_delay());
        if let Some(shared) = &mut self.shared {
            shared.set_delay(effective_delay);
        }
        effective_delay
    }

    /// Transport failures have no response header. Keep the host paused for
    /// the retry delay before the next request start.
    pub(crate) fn transport_delay(&mut self, delay: Duration) -> Duration {
        if let Some(gate) = &self.gate {
            gate.set_cooldown(delay);
        }
        let effective_delay = delay.max(self.min_delay());
        if let Some(shared) = &mut self.shared {
            shared.set_delay(effective_delay);
        }
        effective_delay
    }

    fn min_delay(&self) -> Duration {
        self.gate
            .as_ref()
            .map(|gate| gate.state.lock().expect("Wikimedia gate poisoned").min_delay)
            .unwrap_or_default()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.release();
    }
}

/// Reserve a Wikimedia request lease from the process-wide gate.
pub(crate) fn acquire(url: &str) -> std::io::Result<Permit> {
    if should_fetch_robots(url) {
        gate().acquire()
    } else {
        Ok(Permit {
            gate: None,
            shared: None,
            released: false,
        })
    }
}

/// Install a minimum spacing learned from robots.txt.  This only ever makes
/// the process more conservative than the built-in one-second default.
pub(crate) fn set_robot_delay(delay: Duration) {
    gate().set_min_delay(delay);
}

static ROBOTS_LOCK: Mutex<()> = Mutex::new(());
static ROBOTS_LOADED: OnceLock<RobotsPolicy> = OnceLock::new();

#[derive(Debug, Default)]
struct RobotsPolicy {
    rules: Vec<(bool, String)>,
    min_delay: Option<Duration>,
}

fn host_from_url(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split('/').next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    Some(host)
}

fn should_fetch_robots(url: &str) -> bool {
    host_from_url(url).is_some_and(|host| {
        host == "wikimedia.org"
            || host.ends_with(".wikimedia.org")
            || host == "wikipedia.org"
            || host.ends_with(".wikipedia.org")
    })
}

fn parse_robots_policy(body: &[u8]) -> RobotsPolicy {
    let Ok(text) = std::str::from_utf8(body) else {
        return RobotsPolicy::default();
    };
    let mut in_wildcard_group = false;
    let mut policy = RobotsPolicy::default();
    for raw_line in text.lines() {
        let Some(line) = raw_line.split('#').next().map(str::trim) else {
            continue;
        };
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "user-agent" => in_wildcard_group = value.trim() == "*",
            "crawl-delay" if in_wildcard_group => {
                if let Ok(seconds) = value.trim().parse::<f64>() {
                    if seconds.is_finite() && seconds >= 0.0 {
                        let Some(delay) = Duration::try_from_secs_f64(seconds).ok() else {
                            continue;
                        };
                        policy.min_delay = Some(
                            policy
                                .min_delay
                                .map_or(delay, |old: Duration| old.max(delay)),
                        );
                    }
                }
            }
            "request-rate" if in_wildcard_group => {
                let Some((requests, seconds)) = value.trim().split_once('/') else {
                    continue;
                };
                let Ok(requests) = requests.trim().parse::<f64>() else {
                    continue;
                };
                let Ok(seconds) = seconds.trim().parse::<f64>() else {
                    continue;
                };
                if requests.is_finite() && requests > 0.0 && seconds.is_finite() && seconds >= 0.0 {
                    let Some(delay) = Duration::try_from_secs_f64(seconds / requests).ok() else {
                        continue;
                    };
                    policy.min_delay = Some(
                        policy
                            .min_delay
                            .map_or(delay, |old: Duration| old.max(delay)),
                    );
                }
            }
            "allow" if in_wildcard_group => {
                policy.rules.push((true, value.trim().to_string()));
            }
            "disallow" if in_wildcard_group => {
                policy.rules.push((false, value.trim().to_string()));
            }
            _ => {}
        }
    }
    policy
}

fn url_path(url: &str) -> &str {
    url.split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|index| &rest[index..]))
        .unwrap_or("/")
}

fn robots_allows(policy: &RobotsPolicy, url: &str) -> bool {
    let path = url_path(url);
    policy
        .rules
        .iter()
        .filter(|(_, rule)| !rule.is_empty() && path.starts_with(rule))
        .max_by_key(|(_, rule)| rule.len())
        .map(|(allow, _)| *allow)
        .unwrap_or(true)
}

fn robots_cache_path(host: &str) -> Option<PathBuf> {
    let root = std::env::var_os(ROBOTS_CACHE_ENV)?;
    if root.is_empty() {
        return None;
    }
    // URL hosts are restricted to DNS names (or an IP literal), so this is a
    // stable, non-ambiguous filename and cannot escape the cache directory.
    Some(PathBuf::from(root).join(format!("{host}.robots")))
}

fn read_cached_robots(host: &str) -> Option<RobotsPolicy> {
    let path = robots_cache_path(host)?;
    let bytes = std::fs::read(path).ok()?;
    let separator = bytes.iter().position(|byte| *byte == b'\n')?;
    let (status, body) = bytes.split_at(separator);
    let body = &body[1..];
    let status = std::str::from_utf8(status).ok()?.parse::<u16>().ok()?;
    match status {
        200..=299 => Some(parse_robots_policy(body)),
        404 => Some(RobotsPolicy::default()),
        _ => None,
    }
}

fn write_cached_robots(host: &str, status: u16, body: &[u8]) {
    let Some(path) = robots_cache_path(host) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temporary = path.with_extension(format!("robots.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        writeln!(file, "{status}")?;
        file.write_all(body)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
}

fn install_robots_policy(policy: RobotsPolicy) {
    if let Some(delay) = policy.min_delay {
        set_robot_delay(delay);
    }
    let _ = ROBOTS_LOADED.set(policy);
}

/// Fetch and cache the relevant robots.txt once for Wikimedia origins.
/// Local test servers and non-Wikimedia URLs deliberately do not get an
/// extra request.
pub(crate) fn ensure_robots(client: &Client, url: &str) -> Result<()> {
    if !should_fetch_robots(url) {
        return Ok(());
    }
    if let Some(policy) = ROBOTS_LOADED.get() {
        if !robots_allows(policy, url) {
            return Err(Error::Parse(format!("robots.txt disallows {url}")));
        }
        return Ok(());
    }
    let _guard = ROBOTS_LOCK.lock().expect("robots mutex poisoned");
    if let Some(policy) = ROBOTS_LOADED.get() {
        if !robots_allows(policy, url) {
            return Err(Error::Parse(format!("robots.txt disallows {url}")));
        }
        return Ok(());
    }
    let host = host_from_url(url).ok_or_else(|| Error::Parse(format!("invalid URL: {url}")))?;
    // Several build helpers are separate processes, so ROBOTS_LOCK only
    // serializes callers within this process.  A sibling may have populated
    // the destination-local cache while this process was waiting for its
    // local mutex.  Recheck it here before issuing another robots request;
    // otherwise every helper can independently decide that discovery is
    // needed and turn a one-time probe into a burst of requests.
    if let Some(policy) = read_cached_robots(host) {
        install_robots_policy(policy);
        if let Some(policy) = ROBOTS_LOADED.get() {
            if !robots_allows(policy, url) {
                return Err(Error::Parse(format!("robots.txt disallows {url}")));
            }
        }
        return Ok(());
    }
    let robots_url = format!("https://{host}/robots.txt");
    let mut permit = acquire(&robots_url)?;
    let response = client.get(&robots_url).send()?;
    let status = response.status();
    let retry_after = parse_retry_after_header(response.headers());
    let body = response.bytes()?.to_vec();
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        let delay = permit.retry_delay(Some(status.as_u16()), retry_after);
        std::thread::sleep(delay);
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            url: robots_url,
        });
    }
    if !status.is_success() && status != StatusCode::NOT_FOUND {
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            url: robots_url,
        });
    }
    if status.is_success() {
        let policy = parse_robots_policy(&body);
        write_cached_robots(host, status.as_u16(), &body);
        if let Some(delay) = policy.min_delay {
            set_robot_delay(delay);
        }
        if !robots_allows(&policy, url) {
            let _ = ROBOTS_LOADED.set(policy);
            permit.release_now();
            return Err(Error::Parse(format!("robots.txt disallows {url}")));
        }
        permit.release_now();
        let _ = ROBOTS_LOADED.set(policy);
        return Ok(());
    }
    permit.release_now();
    write_cached_robots(host, status.as_u16(), &body);
    let _ = ROBOTS_LOADED.set(RobotsPolicy::default());
    Ok(())
}

pub(crate) fn parse_retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let seconds = (date.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    Some(Duration::from_secs(seconds.max(0) as u64))
}

/// A 429 is retried only when the server actually supplied a retry window.
/// Repeatedly inventing our own retry interval for a rate-limit response is
/// indistinguishable from ignoring the server's signal.
pub(crate) fn should_retry_response(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
) -> bool {
    status.is_server_error()
        || (status == reqwest::StatusCode::TOO_MANY_REQUESTS && retry_after.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_robots_delays() {
        assert_eq!(parse_robots_policy(b"User-agent: *\nCrawl-delay: 2.5\n").min_delay, Some(Duration::from_secs_f64(2.5)));
        assert_eq!(parse_robots_policy(b"User-agent: *\nRequest-rate: 1/4\n").min_delay, Some(Duration::from_secs(4)));
        assert_eq!(parse_robots_policy(b"User-agent: sarun\nCrawl-delay: 99\n").min_delay, None);
        let policy = parse_robots_policy(b"User-agent: *\nDisallow: /private\nAllow: /private/public\n");
        assert!(!robots_allows(&policy, "https://dumps.wikimedia.org/private/file"));
        assert!(robots_allows(&policy, "https://dumps.wikimedia.org/private/public/file"));
    }

    #[test]
    fn rate_limit_requires_server_retry_window() {
        assert!(!should_retry_response(StatusCode::TOO_MANY_REQUESTS, None));
        assert!(should_retry_response(
            StatusCode::TOO_MANY_REQUESTS,
            Some(Duration::from_secs(1))
        ));
        assert!(should_retry_response(StatusCode::SERVICE_UNAVAILABLE, None));
    }
}
