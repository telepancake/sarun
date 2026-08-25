//! Process-wide politeness policy for Wikimedia HTTP requests.
//!
//! The engine owns mirror jobs and permits only one Wikipedia mirror job at a
//! time. Within a job, all fetch/decode workers share this process-wide gate;
//! no private socket, helper daemon, or environment protocol is needed.

use std::collections::HashMap;
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
// These response fallbacks apply only when neither a valid server
// Retry-After nor applicable robots timing exists. They are deliberately
// distinct from the ordinary one-second request spacing and curl's transport
// resumption backoff.
//
// A 429 without usable upstream timing still needs a substantial quiet
// period, so several helper processes do not become a steady stream of
// denials.
const DEFAULT_429_DELAY: Duration = Duration::from_secs(60);
// A transient server failure without usable upstream timing gets a longer
// quiet period because immediately retrying a distressed dump host is both
// impolite and unlikely to make progress.
const SERVER_ERROR_DELAY: Duration = Duration::from_secs(15 * 60);
const MAX_ROBOTS_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const ROBOTS_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
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
    origins: HashMap<String, OriginTiming>,
}

#[derive(Debug)]
struct OriginTiming {
    next_start: Instant,
    cooldown_until: Instant,
    robots_delay: Option<Duration>,
}

impl OriginTiming {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            next_start: now,
            cooldown_until: now,
            robots_delay: None,
        }
    }
}

#[derive(Debug)]
struct Gate {
    state: Mutex<State>,
    wake: Condvar,
}

/// The importer deliberately has several helper processes (Kati recipes),
/// while the policy must cover the whole Chupa job. Destination-local
/// advisory slot locks enforce the shared concurrency limit without a daemon.
/// A separate short-held schedule lock serializes starts and cooldown updates.
/// `TMPDIR` is set to destination-local scratch by the Chupa driver.
struct SharedLease {
    _slot: Option<std::fs::File>,
    schedule_path: PathBuf,
}

fn shared_lease_root(configured: Option<PathBuf>) -> std::io::Result<PathBuf> {
    configured
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "TMPDIR must be set to a destination-local Wikimedia scratch directory",
            )
        })
}

impl SharedLease {
    fn reserve(origin: &str) -> std::io::Result<Self> {
        let root = shared_lease_root(std::env::var_os("TMPDIR").map(PathBuf::from))?;
        Self::reserve_at(root, origin)
    }

    fn reserve_at(root: PathBuf, origin: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let schedule_name = format!("wikimedia-request-{origin}.schedule");
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
                        return Ok(Self {
                            _slot: Some(slot),
                            schedule_path: root.join(&schedule_name),
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
                    return Ok(Self {
                        _slot: Some(slot),
                        schedule_path: root.join(&schedule_name),
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
                    now.saturating_add(
                        u64::try_from(min_delay.as_micros()).unwrap_or(u64::MAX),
                    ),
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
        let requested = unix_micros().saturating_add(
            u64::try_from(delay.as_micros()).unwrap_or(u64::MAX),
        );
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
        Self {
            state: Mutex::new(State {
                active: 0,
                origins: HashMap::new(),
            }),
            wake: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>, origin: &str) -> std::io::Result<Permit> {
        let mut permit = self.reserve(origin)?;
        if let Err(error) = permit.wait_for_next_start() {
            drop(permit);
            return Err(error);
        }
        Ok(permit)
    }

    fn reserve(self: &Arc<Self>, origin: &str) -> std::io::Result<Permit> {
        loop {
            let mut state = self.state.lock().expect("Wikimedia gate poisoned");
            while state.active >= MAX_ACTIVE_REQUESTS {
                state = self.wake.wait(state).expect("Wikimedia gate poisoned");
            }
            // Claim the local token, then drop the gate mutex before entering
            // the cross-process flock loop. An existing same-process permit
            // must always be able to release its token while this reservation
            // waits for an external slot.
            state.active += 1;
            drop(state);

            let shared = match SharedLease::reserve(origin) {
                Ok(shared) => shared,
                Err(error) => {
                    self.release();
                    return Err(error);
                }
            };
            return Ok(Permit {
                gate: Some(Arc::clone(self)),
                shared: Some(shared),
                origin: Some(origin.to_owned()),
                released: false,
            });
        }
    }

    #[cfg(test)]
    fn reserve_at(self: &Arc<Self>, root: PathBuf, origin: &str) -> std::io::Result<Permit> {
        loop {
            let mut state = self.state.lock().expect("Wikimedia gate poisoned");
            while state.active >= MAX_ACTIVE_REQUESTS {
                state = self.wake.wait(state).expect("Wikimedia gate poisoned");
            }
            state.active += 1;
            drop(state);

            let shared = match SharedLease::reserve_at(root.clone(), origin) {
                Ok(shared) => shared,
                Err(error) => {
                    self.release();
                    return Err(error);
                }
            };
            return Ok(Permit {
                gate: Some(Arc::clone(self)),
                shared: Some(shared),
                origin: Some(origin.to_owned()),
                released: false,
            });
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        state.active = state.active.saturating_sub(1);
        self.wake.notify_all();
    }

    fn set_cooldown(&self, origin: &str, delay: Duration) {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        let Some(until) = Instant::now().checked_add(delay) else {
            return;
        };
        let timing = state
            .origins
            .entry(origin.to_owned())
            .or_insert_with(OriginTiming::new);
        timing.cooldown_until = timing.cooldown_until.max(until);
        self.wake.notify_all();
    }

    fn set_robot_delay(&self, origin: &str, delay: Duration) {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        let timing = state
            .origins
            .entry(origin.to_owned())
            .or_insert_with(OriginTiming::new);
        timing.robots_delay = Some(timing.robots_delay.map_or(delay, |old| old.max(delay)));
    }

    /// Wait for the next start permitted by this already-held reservation.
    ///
    /// A `Permit` is deliberately not released while a request is backing off.
    /// This method advances the ordinary in-process start schedule without
    /// changing `active`, so the reservation cannot be stolen by another
    /// source between two attempts.
    fn wait_for_reserved_start(&self, origin: &str) {
        let mut state = self.state.lock().expect("Wikimedia gate poisoned");
        loop {
            let now = Instant::now();
            let timing = state
                .origins
                .entry(origin.to_owned())
                .or_insert_with(OriginTiming::new);
            let allowed_at = timing.next_start.max(timing.cooldown_until);
            if now >= allowed_at {
                let min_delay = DEFAULT_MIN_DELAY.max(timing.robots_delay.unwrap_or_default());
                timing.next_start = now.checked_add(min_delay).unwrap_or(now);
                return;
            }
            state = self
                .wake
                .wait_timeout(state, allowed_at.saturating_duration_since(now))
                .expect("Wikimedia gate poisoned")
                .0;
        }
    }

    fn robots_delay(&self, origin: &str) -> Option<Duration> {
        self.state
            .lock()
            .expect("Wikimedia gate poisoned")
            .origins
            .get(origin)
            .and_then(|timing| timing.robots_delay)
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
    origin: Option<String>,
    released: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryTimingSource {
    ServerRetryAfter,
    RobotsPolicy,
    ConservativeFallback,
    LocalRequestSpacing,
}

impl RetryTimingSource {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::ServerRetryAfter => "server Retry-After",
            Self::RobotsPolicy => "robots.txt timing",
            Self::ConservativeFallback => {
                "conservative local fallback (no valid Retry-After or robots timing)"
            }
            Self::LocalRequestSpacing => "local minimum request spacing",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetrySchedule {
    pub(crate) delay: Duration,
    pub(crate) source: RetryTimingSource,
}

fn response_retry_schedule(
    status: Option<u16>,
    retry_after: Option<Duration>,
    robots_delay: Option<Duration>,
    local_min_delay: Duration,
) -> RetrySchedule {
    let (mut delay, mut source) = if let Some(delay) = retry_after {
        (delay, RetryTimingSource::ServerRetryAfter)
    } else if let Some(delay) = robots_delay {
        (delay, RetryTimingSource::RobotsPolicy)
    } else {
        let delay = match status {
            Some(429) => DEFAULT_429_DELAY,
            Some(code) if (500..600).contains(&code) => SERVER_ERROR_DELAY,
            _ => DEFAULT_429_DELAY,
        };
        (delay, RetryTimingSource::ConservativeFallback)
    };
    if let Some(robots_delay) = robots_delay {
        if robots_delay > delay {
            delay = robots_delay;
            source = RetryTimingSource::RobotsPolicy;
        }
    }
    if local_min_delay > delay {
        delay = local_min_delay;
        source = RetryTimingSource::LocalRequestSpacing;
    }
    RetrySchedule { delay, source }
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

    /// Record a response that requires a retry and retain the timing source.
    /// A valid server delay is never capped or compared with the local
    /// response fallback. Applicable robots timing and ordinary request
    /// spacing may only make the next request later.
    pub(crate) fn retry_schedule(
        &mut self,
        status: Option<u16>,
        retry_after: Option<Duration>,
    ) -> RetrySchedule {
        let schedule = response_retry_schedule(
            status,
            retry_after,
            self.robots_delay(),
            self.local_min_delay(),
        );
        if let (Some(gate), Some(origin)) = (&self.gate, &self.origin) {
            gate.set_cooldown(origin, schedule.delay);
        }
        if let Some(shared) = &mut self.shared {
            shared.set_delay(schedule.delay);
        }
        schedule
    }

    pub(crate) fn retry_delay(
        &mut self,
        status: Option<u16>,
        retry_after: Option<Duration>,
    ) -> Duration {
        self.retry_schedule(status, retry_after).delay
    }

    /// Transport failures have no response header. Keep the host paused for
    /// the retry delay before the next request start.
    pub(crate) fn transport_delay(&mut self, delay: Duration) -> Duration {
        if let (Some(gate), Some(origin)) = (&self.gate, &self.origin) {
            gate.set_cooldown(origin, delay);
        }
        let effective_delay = delay.max(self.local_min_delay());
        if let Some(shared) = &mut self.shared {
            shared.set_delay(effective_delay);
        }
        effective_delay
    }

    /// Prepare the next HTTP start while retaining this permit.
    ///
    /// The initial acquisition already performs this operation.  Cached
    /// readers call it after their local prefix has been replayed, and curl
    /// calls it before every retained-permit restart.  Neither the process
    /// active count nor the destination-local slot lock is released here.
    pub(crate) fn wait_for_next_start(&mut self) -> std::io::Result<()> {
        let min_delay = self.local_min_delay();
        if let (Some(gate), Some(origin)) = (&self.gate, &self.origin) {
            gate.wait_for_reserved_start(origin);
        }
        if let Some(shared) = &mut self.shared {
            SharedLease::wait_for_start(&shared.schedule_path, min_delay)?;
        }
        Ok(())
    }

    fn robots_delay(&self) -> Option<Duration> {
        match (&self.gate, &self.origin) {
            (Some(gate), Some(origin)) => gate.robots_delay(origin),
            _ => None,
        }
    }

    fn local_min_delay(&self) -> Duration {
        if self.gate.is_some() {
            DEFAULT_MIN_DELAY.max(self.robots_delay().unwrap_or_default())
        } else {
            Duration::ZERO
        }
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
        let origin = host_from_url(url).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid Wikimedia URL")
        })?;
        gate().acquire(origin)
    } else {
        Ok(Permit {
            gate: None,
            shared: None,
            origin: None,
            released: false,
        })
    }
}

/// Install a minimum spacing learned from robots.txt.  This only ever makes
/// the process more conservative than the built-in one-second default.
pub(crate) fn set_robot_delay(origin: &str, delay: Duration) {
    gate().set_robot_delay(origin, delay);
}

static ROBOTS_LOCK: Mutex<()> = Mutex::new(());
static ROBOTS_LOADED: OnceLock<Mutex<HashMap<String, RobotsPolicy>>> = OnceLock::new();

#[derive(Clone, Debug, Default)]
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

fn loaded_robots_policy(host: &str) -> Option<RobotsPolicy> {
    ROBOTS_LOADED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("robots policy mutex poisoned")
        .get(host)
        .cloned()
}

fn install_robots_policy(host: &str, policy: RobotsPolicy) {
    if let Some(delay) = policy.min_delay {
        set_robot_delay(host, delay);
    }
    ROBOTS_LOADED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("robots policy mutex poisoned")
        .insert(host.to_owned(), policy);
}

fn bounded_robots_response(
    mut response: reqwest::blocking::Response,
    url: &str,
) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
    let status = response.status();
    let headers = response.headers().clone();
    if headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|length| length > MAX_ROBOTS_RESPONSE_BYTES)
    {
        return Err(Error::Parse(format!(
            "robots.txt response exceeded the 64 MiB bound for {url}"
        )));
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(MAX_ROBOTS_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_ROBOTS_RESPONSE_BYTES {
        return Err(Error::Parse(format!(
            "robots.txt response exceeded the 64 MiB bound for {url}"
        )));
    }
    Ok((status, headers, body))
}

/// Fetch and cache the relevant robots.txt once for Wikimedia origins.
/// Local test servers and non-Wikimedia URLs deliberately do not get an
/// extra request.
pub(crate) fn ensure_robots(client: &Client, url: &str) -> Result<()> {
    if !should_fetch_robots(url) {
        return Ok(());
    }
    let host = host_from_url(url).ok_or_else(|| Error::Parse(format!("invalid URL: {url}")))?;
    if let Some(policy) = loaded_robots_policy(host) {
        if !robots_allows(&policy, url) {
            return Err(Error::Parse(format!("robots.txt disallows {url}")));
        }
        return Ok(());
    }
    let _guard = ROBOTS_LOCK.lock().expect("robots mutex poisoned");
    if let Some(policy) = loaded_robots_policy(host) {
        if !robots_allows(&policy, url) {
            return Err(Error::Parse(format!("robots.txt disallows {url}")));
        }
        return Ok(());
    }
    // Several build helpers are separate processes, so ROBOTS_LOCK only
    // serializes callers within this process.  A sibling may have populated
    // the destination-local cache while this process was waiting for its
    // local mutex.  Recheck it here before issuing another robots request;
    // otherwise every helper can independently decide that discovery is
    // needed and turn a one-time probe into a burst of requests.
    if let Some(policy) = read_cached_robots(host) {
        install_robots_policy(host, policy.clone());
        if !robots_allows(&policy, url) {
            return Err(Error::Parse(format!("robots.txt disallows {url}")));
        }
        return Ok(());
    }
    let robots_url = format!("https://{host}/robots.txt");
    let mut permit = acquire(&robots_url)?;
    #[cfg(target_os = "macos")]
    let (status, headers, body) = if crate::curl_http::handles(&robots_url) {
        let response = crate::curl_http::request(
            &robots_url,
            crate::curl_http::RequestKind::Get,
        )?;
        (response.status, response.headers, response.body)
    } else {
        let response = client
            .get(&robots_url)
            .timeout(ROBOTS_REQUEST_TIMEOUT)
            .send()?;
        bounded_robots_response(response, &robots_url)?
    };
    #[cfg(not(target_os = "macos"))]
    let (status, headers, body) = {
        let response = client
            .get(&robots_url)
            .timeout(ROBOTS_REQUEST_TIMEOUT)
            .send()?;
        bounded_robots_response(response, &robots_url)?
    };
    let retry_after = parse_retry_after_header(&headers);
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
        if !robots_allows(&policy, url) {
            install_robots_policy(host, policy);
            permit.release_now();
            return Err(Error::Parse(format!("robots.txt disallows {url}")));
        }
        permit.release_now();
        install_robots_policy(host, policy);
        return Ok(());
    }
    permit.release_now();
    write_cached_robots(host, status.as_u16(), &body);
    install_robots_policy(host, RobotsPolicy::default());
    Ok(())
}

pub(crate) fn parse_retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    parse_retry_after_at(value, chrono::Utc::now())
}

pub(crate) fn parse_retry_after_at(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let seconds = (date.with_timezone(&chrono::Utc) - now).num_seconds();
    Some(Duration::from_secs(seconds.max(0) as u64))
}

/// Response retries are finite regardless of timing source. A missing or
/// malformed Retry-After changes timing attribution to robots or the explicit
/// local fallback; it does not create an unbounded retry loop.
pub(crate) fn should_retry_response(
    status: reqwest::StatusCode,
    _retry_after: Option<Duration>,
) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn test_slot_root(name: &str) -> PathBuf {
        let root = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "politeness-{name}-{}-{}",
                std::process::id(),
                unix_micros()
            ));
        std::fs::create_dir_all(&root).expect("create test slot root");
        root
    }

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
    fn retry_after_seconds_date_and_malformed_are_deterministic() {
        let now = chrono::DateTime::parse_from_rfc2822("Fri, 14 Aug 2026 10:00:00 GMT")
            .expect("fixed clock")
            .with_timezone(&chrono::Utc);
        assert_eq!(
            parse_retry_after_at("17", now),
            Some(Duration::from_secs(17))
        );
        assert_eq!(
            parse_retry_after_at("Fri, 14 Aug 2026 10:02:03 GMT", now),
            Some(Duration::from_secs(123))
        );
        assert_eq!(
            parse_retry_after_at("Fri, 14 Aug 2026 09:59:59 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after_at("not-a-delay", now), None);
    }

    #[test]
    fn retry_schedule_attributes_server_robots_and_fallback_precedence() {
        let fixed_now = chrono::DateTime::parse_from_rfc2822(
            "Fri, 14 Aug 2026 10:00:00 GMT",
        )
        .expect("fixed clock")
        .with_timezone(&chrono::Utc);
        let server_longer = response_retry_schedule(
            Some(503),
            Some(Duration::from_secs(120)),
            Some(Duration::from_secs(30)),
            DEFAULT_MIN_DELAY,
        );
        assert_eq!(server_longer.delay, Duration::from_secs(120));
        assert_eq!(server_longer.source, RetryTimingSource::ServerRetryAfter);

        let robots_longer = response_retry_schedule(
            Some(429),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(120)),
            DEFAULT_MIN_DELAY,
        );
        assert_eq!(robots_longer.delay, Duration::from_secs(120));
        assert_eq!(robots_longer.source, RetryTimingSource::RobotsPolicy);

        let robots_only = response_retry_schedule(
            Some(429),
            None,
            Some(Duration::from_secs(7)),
            DEFAULT_MIN_DELAY,
        );
        assert_eq!(robots_only.delay, Duration::from_secs(7));
        assert_eq!(robots_only.source, RetryTimingSource::RobotsPolicy);

        let malformed_fallback = response_retry_schedule(
            Some(429),
            parse_retry_after_at("malformed", fixed_now),
            None,
            DEFAULT_MIN_DELAY,
        );
        assert_eq!(malformed_fallback.delay, DEFAULT_429_DELAY);
        assert_eq!(
            malformed_fallback.source,
            RetryTimingSource::ConservativeFallback
        );

        let server_shorter_than_old_fallback = response_retry_schedule(
            Some(503),
            Some(Duration::from_secs(5)),
            None,
            DEFAULT_MIN_DELAY,
        );
        assert_eq!(server_shorter_than_old_fallback.delay, Duration::from_secs(5));
        assert_eq!(
            server_shorter_than_old_fallback.source,
            RetryTimingSource::ServerRetryAfter
        );
        let server_error_fallback = response_retry_schedule(
            Some(503),
            None,
            None,
            DEFAULT_MIN_DELAY,
        );
        assert_eq!(server_error_fallback.delay, SERVER_ERROR_DELAY);
        assert_eq!(
            server_error_fallback.source,
            RetryTimingSource::ConservativeFallback
        );
    }

    #[test]
    fn rate_limit_retry_is_finite_with_or_without_server_timing() {
        assert!(should_retry_response(StatusCode::TOO_MANY_REQUESTS, None));
        assert!(should_retry_response(
            StatusCode::TOO_MANY_REQUESTS,
            Some(Duration::from_secs(1))
        ));
        assert!(should_retry_response(StatusCode::SERVICE_UNAVAILABLE, None));
    }

    #[test]
    fn robots_policy_loaded_for_one_host_does_not_apply_to_another() {
        let dumps_host = "test-dumps.wikimedia.org";
        let wikipedia_host = "test-en.wikipedia.org";
        let policy = parse_robots_policy(
            b"User-agent: *\nCrawl-delay: 9\nDisallow: /private\n",
        );

        install_robots_policy(dumps_host, policy.clone());

        let dumps_policy = loaded_robots_policy(dumps_host).expect("dumps policy is installed");
        assert!(!robots_allows(
            &dumps_policy,
            "https://test-dumps.wikimedia.org/private/file"
        ));
        assert!(
            loaded_robots_policy(wikipedia_host).is_none(),
            "a dumps policy must not be reused for a different Wikimedia host"
        );
        assert!(robots_allows(
            &RobotsPolicy::default(),
            "https://test-en.wikipedia.org/private/file"
        ));
        assert_eq!(gate().robots_delay(dumps_host), Some(Duration::from_secs(9)));
        assert_eq!(gate().robots_delay(wikipedia_host), None);
    }

    #[test]
    fn shared_lease_requires_nonempty_tmpdir() {
        // Pure path-selection test: no directory is created or opened.
        let destination_tmp = PathBuf::from("destination-owned-request-tmp");
        assert_eq!(
            shared_lease_root(Some(destination_tmp.clone())).expect("configured TMPDIR"),
            destination_tmp
        );
        assert_eq!(
            shared_lease_root(None).expect_err("missing TMPDIR must fail closed").kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            shared_lease_root(Some(PathBuf::new()))
                .expect_err("empty TMPDIR must fail closed")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn shared_deadline_saturates_instead_of_shortening_large_server_wait() {
        let root = test_slot_root("large-server-wait");
        let mut lease = SharedLease::reserve_at(
            root,
            "large-server-wait.wikimedia.org",
        )
        .expect("reserve shared lease");
        lease.set_delay(Duration::from_secs(u64::MAX));
        let mut schedule = SharedLease::lock_schedule(&lease.schedule_path)
            .expect("open shared schedule");
        assert_eq!(
            SharedLease::read_deadline(&mut schedule).expect("read shared deadline"),
            u64::MAX
        );
    }

    #[test]
    fn reservation_does_not_advance_start_schedule_until_http_start() {
        let origin = "no-phantom-start.wikimedia.org";
        let gate = Arc::new(Gate::new());
        let mut permit = gate
            .reserve_at(test_slot_root("no-phantom-start"), origin)
            .expect("reserve slot");
        assert!(
            gate.state.lock().expect("gate state").origins.get(origin).is_none(),
            "reservation alone must not create or advance an origin schedule"
        );

        permit
            .wait_for_next_start()
            .expect("schedule one actual HTTP start");
        let after_start = gate
            .state
            .lock()
            .expect("gate state")
            .origins
            .get(origin)
            .expect("origin timing after start")
            .next_start;
        assert_eq!(gate.state.lock().expect("gate state").active, 1);
        assert!(
            after_start >= Instant::now() + DEFAULT_MIN_DELAY / 2,
            "one retained permit start must advance the ordinary spacing schedule"
        );
    }

    #[test]
    fn blocked_shared_reservation_does_not_hold_same_gate_mutex() {
        let origin = "blocked-shared-release.wikimedia.org";
        let root = test_slot_root("blocked-shared-release");
        let gate = Arc::new(Gate::new());
        let existing = gate.reserve_at(root.clone(), origin).expect("existing permit");
        let external_one = SharedLease::reserve_at(root.clone(), origin).expect("external slot one");
        let external_two = SharedLease::reserve_at(root.clone(), origin).expect("external slot two");

        let (ready, completed) = mpsc::channel();
        let waiter_gate = Arc::clone(&gate);
        let waiter_root = root.clone();
        let waiter = std::thread::spawn(move || {
            let permit = waiter_gate
                .reserve_at(waiter_root, origin)
                .expect("blocked reservation eventually acquires a slot");
            ready.send(()).expect("signal reservation completion");
            permit
        });

        let mut observed_waiter_token = false;
        for _ in 0..2000 {
            if gate.state.lock().expect("gate state").active == 2 {
                observed_waiter_token = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            observed_waiter_token,
            "waiter must claim a local token before blocking on the shared slot"
        );

        // This must not block behind the waiter's cross-process flock loop.
        drop(existing);
        assert_eq!(gate.state.lock().expect("gate state").active, 1);

        drop(external_one);
        drop(external_two);
        completed
            .recv_timeout(Duration::from_secs(2))
            .expect("shared waiter must make progress after slots are released");
        drop(waiter.join().expect("reservation waiter thread"));
    }

    #[test]
    fn three_reservations_block_a_fourth_without_slot_steal() {
        let origin = "three-reservations.wikimedia.org";
        let root = test_slot_root("three-reservations");
        let gate = Arc::new(Gate::new());
        let mut first = gate.reserve_at(root.clone(), origin).expect("reservation one");
        let second = gate.reserve_at(root.clone(), origin).expect("reservation two");
        let third = gate.reserve_at(root.clone(), origin).expect("reservation three");

        first
            .wait_for_next_start()
            .expect("retained reservation can schedule its start");
        assert_eq!(gate.state.lock().expect("gate state").active, 3);

        let (ready, completed) = mpsc::channel();
        let waiter_gate = Arc::clone(&gate);
        let waiter_root = root.clone();
        let waiter = std::thread::spawn(move || {
            let permit = waiter_gate
                .reserve_at(waiter_root, origin)
                .expect("fourth reservation after one release");
            ready.send(()).expect("signal fourth reservation");
            permit
        });
        assert!(
            completed.recv_timeout(Duration::from_millis(50)).is_err(),
            "a fourth source cannot steal a retained restart reservation"
        );

        first.release_now();
        assert_eq!(gate.state.lock().expect("gate state").active, 2);
        drop(first);
        completed
            .recv_timeout(Duration::from_secs(2))
            .expect("fourth reservation proceeds only after a slot is released");
        drop(second);
        drop(third);
        drop(waiter.join().expect("fourth reservation thread"));
    }
}
