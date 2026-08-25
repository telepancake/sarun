//! Streaming HTTP fetch with on-EOF checksum verification.
//!
//! Per SPEC §API: the returned reader verifies the part's checksum on
//! EOF. Calling `into_inner()` or dropping mid-stream skips the check.
//! `sha256` takes precedence; if `None`, `sha1` is used.

use std::io::{self, Read};
#[cfg(target_os = "macos")]
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use md5::Md5;
use reqwest::blocking::{Client, Response};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};

use crate::politeness;
use crate::types::{Error, Part, Result};

/// Which digest the verifier is computing. `None` means no checksum was
/// advertised on the Part; reads pass through verbatim and EOF is silent.
enum Hasher {
    Sha256(Sha256),
    Sha1(Sha1),
    Md5(Md5),
}

/// Network-level counters for one source part. `bytes_received` counts bytes
/// delivered by the server, including bytes repeated after a range resume.
#[derive(Clone, Debug, Default)]
pub struct FetchStats {
    pub attempts: u64,
    pub bytes_received: u64,
    pub rate_limit_responses: u64,
    pub client_error_responses: u64,
    pub server_error_responses: u64,
    pub transport_errors: u64,
    /// Response retries grouped by the timing source that controlled the
    /// actual wait. These four counters are mutually exclusive per retry.
    pub server_timed_retries: u64,
    pub robots_timed_retries: u64,
    pub fallback_timed_retries: u64,
    pub local_spacing_timed_retries: u64,
}

pub type FetchStatsHandle = Arc<Mutex<FetchStats>>;

fn record_retry_timing(stats: &FetchStatsHandle, schedule: politeness::RetrySchedule) {
    if let Ok(mut stats) = stats.lock() {
        match schedule.source {
            politeness::RetryTimingSource::ServerRetryAfter => {
                stats.server_timed_retries = stats.server_timed_retries.saturating_add(1);
            }
            politeness::RetryTimingSource::RobotsPolicy => {
                stats.robots_timed_retries = stats.robots_timed_retries.saturating_add(1);
            }
            politeness::RetryTimingSource::ConservativeFallback => {
                stats.fallback_timed_retries = stats.fallback_timed_retries.saturating_add(1);
            }
            politeness::RetryTimingSource::LocalRequestSpacing => {
                stats.local_spacing_timed_retries =
                    stats.local_spacing_timed_retries.saturating_add(1);
            }
        }
    }
}

impl Hasher {
    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => sha2::Digest::update(h, data),
            Hasher::Sha1(h) => sha1::Digest::update(h, data),
            Hasher::Md5(h) => md5::Digest::update(h, data),
        }
    }
    fn finalize_hex(self) -> String {
        match self {
            Hasher::Sha256(h) => hex::encode(h.finalize()),
            Hasher::Sha1(h) => hex::encode(h.finalize()),
            Hasher::Md5(h) => hex::encode(h.finalize()),
        }
    }
}

fn checksum_for(part: &Part) -> (Option<Hasher>, String) {
    match (&part.sha256, &part.sha1, &part.md5) {
        (Some(h), _, _) => (Some(Hasher::Sha256(Sha256::new())), h.to_lowercase()),
        (None, Some(h), _) => (Some(Hasher::Sha1(Sha1::new())), h.to_lowercase()),
        (None, None, Some(h)) => (Some(Hasher::Md5(Md5::new())), h.to_lowercase()),
        (None, None, None) => (None, String::new()),
    }
}

fn checksum_error(part: &str, expected: &str, got: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        Error::ChecksumMismatch {
            part: part.to_owned(),
            expected: expected.to_owned(),
            got,
        },
    )
}

/// A `Read` wrapper that tracks the running hash and surfaces a
/// `ChecksumMismatch` error from `read` when the underlying reader hits
/// EOF if the digest does not match the part's advertised checksum.
///
/// Partial reads followed by `into_inner()` or drop skip the check.
pub struct VerifyingReader<R: Read> {
    pub(crate) inner: R,
    stats: FetchStatsHandle,
    hasher: Option<Hasher>,
    expected: String,
    filename: String,
    finalized: bool,
}

impl<R: Read> VerifyingReader<R> {
    /// Returns the inner reader, skipping the checksum check.
    pub fn into_inner(self) -> R {
        self.inner
    }

    pub fn stats_handle(&self) -> FetchStatsHandle {
        Arc::clone(&self.stats)
    }
}

impl<R: Read> Read for VerifyingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.finalized {
            return Ok(0);
        }
        let n = self.inner.read(buf)?;
        if n > 0 {
            if let Some(h) = self.hasher.as_mut() {
                h.update(&buf[..n]);
            }
            return Ok(n);
        }
        // EOF. Finalize once; if there's a hasher, compare.
        self.finalized = true;
        if let Some(h) = self.hasher.take() {
            let got = h.finalize_hex();
            if got != self.expected {
                return Err(checksum_error(&self.filename, &self.expected, got));
            }
        }
        Ok(0)
    }
}

struct ThrottledResponse {
    response: Response,
    stats: FetchStatsHandle,
    permit: Option<politeness::Permit>,
}

impl ThrottledResponse {
    fn new(response: Response, permit: politeness::Permit, stats: FetchStatsHandle) -> Self {
        Self {
            response,
            stats,
            permit: Some(permit),
        }
    }
}

impl Read for ThrottledResponse {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.response.read(buffer)?;
        if count != 0 {
            if let Ok(mut stats) = self.stats.lock() {
                stats.bytes_received = stats.bytes_received.saturating_add(count as u64);
            }
        } else {
            self.permit.take();
        }
        Ok(count)
    }
}

const MAX_SOURCE_BYTES_ENV: &str = "SARUN_WIKIMEDIA_MAX_SOURCE_BYTES";

fn source_byte_ceiling(part: &Part) -> io::Result<Option<u64>> {
    if part.size_bytes != 0 {
        // An advertised size is an exact protocol contract.  Do not turn a
        // separately configured safety ceiling into a smaller implicit size.
        return Ok(None);
    }
    let Some(value) = std::env::var_os(MAX_SOURCE_BYTES_ENV) else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let limit = value.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "invalid {MAX_SOURCE_BYTES_ENV}={value:?} for Wikimedia source {}: {error}",
                part.url
            ),
        )
    })?;
    if limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{MAX_SOURCE_BYTES_ENV} must be greater than zero for Wikimedia source {}",
                part.url
            ),
        ));
    }
    Ok(Some(limit))
}

fn source_limit_error(source: &str, limit: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "Wikimedia source {source} exceeded its per-source {MAX_SOURCE_BYTES_ENV} ceiling of {limit} bytes"
        ),
    )
}

/// Apply the unknown-size safety ceiling to transports that do not have the
/// curl reader's response-header parser.  The limit is per Part/fetch call;
/// it is not a shared aggregate budget across workers.
struct LimitedSourceReader {
    inner: Box<dyn Read + Send>,
    source: String,
    limit: u64,
    bytes: u64,
}

/// Enforce the exact size advertised by a known-size source.  The final
/// one-byte probe distinguishes an exact body from a body with an
/// unadvertised suffix; an EOF before the expected count is likewise an
/// invalid source, even when no checksum was advertised.
struct ExactSizeReader {
    inner: Box<dyn Read + Send>,
    source: String,
    expected: u64,
    bytes: u64,
    eof_checked: bool,
}

impl ExactSizeReader {
    fn new(inner: Box<dyn Read + Send>, source: String, expected: u64) -> Self {
        Self {
            inner,
            source,
            expected,
            bytes: 0,
            eof_checked: false,
        }
    }
}

impl Read for ExactSizeReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.eof_checked {
            return Ok(0);
        }
        if self.bytes == self.expected {
            let mut probe = [0u8; 1];
            let count = self.inner.read(&mut probe)?;
            if count != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "known-size Wikimedia source {} returned more bytes than advertised ({})",
                        self.source, self.expected
                    ),
                ));
            }
            self.eof_checked = true;
            return Ok(0);
        }
        let remaining = self.expected - self.bytes;
        let count = output.len().min(remaining.min(usize::MAX as u64) as usize);
        let read = self.inner.read(&mut output[..count])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "known-size Wikimedia source {} ended at {} bytes, expected {}",
                    self.source, self.bytes, self.expected
                ),
            ));
        }
        self.bytes = self.bytes.saturating_add(read as u64);
        Ok(read)
    }
}

impl LimitedSourceReader {
    fn new(inner: Box<dyn Read + Send>, source: String, limit: u64) -> Self {
        Self {
            inner,
            source,
            limit,
            bytes: 0,
        }
    }
}

impl Read for LimitedSourceReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.bytes >= self.limit {
            let mut probe = [0u8; 1];
            let count = self.inner.read(&mut probe)?;
            if count != 0 {
                return Err(source_limit_error(&self.source, self.limit));
            }
            return Ok(0);
        }
        let remaining = self.limit - self.bytes;
        let count = output.len().min(remaining.min(usize::MAX as u64) as usize);
        let read = self.inner.read(&mut output[..count])?;
        self.bytes = self.bytes.saturating_add(read as u64);
        Ok(read)
    }
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let Some(value) = value.strip_prefix("bytes ") else {
        return None;
    };
    let Some((range, advertised_total)) = value.split_once('/') else {
        return None;
    };
    let Some((start, end)) = range.split_once('-') else {
        return None;
    };
    let (Ok(start), Ok(end), Ok(advertised_total)) = (
        start.parse::<u64>(),
        end.parse::<u64>(),
        advertised_total.parse::<u64>(),
    ) else {
        return None;
    };
    Some((start, end, advertised_total))
}

/// Fetch a Part: GET the URL, return a streaming reader.
pub fn fetch(client: &Client, part: &Part) -> Result<VerifyingReader<Box<dyn Read + Send>>> {
    let source_limit = source_byte_ceiling(part)?;
    politeness::ensure_robots(client, &part.url)?;
    #[cfg(target_os = "macos")]
    if uses_curl_payload(&part.url) {
        return fetch_with_curl(part, source_limit);
    }

    let stats = Arc::new(Mutex::new(FetchStats {
        attempts: 1,
        ..FetchStats::default()
    }));
    let mut attempt = 0u32;
    let resp = loop {
        let mut permit = politeness::acquire(&part.url)?;
        match client.get(&part.url).send() {
            Ok(resp) if resp.status().is_success() => {
                break ThrottledResponse::new(resp, permit, Arc::clone(&stats));
            }
            Ok(resp) => {
                let status = resp.status();
                let retry_after = politeness::parse_retry_after_header(resp.headers());
                if let Ok(mut stats) = stats.lock() {
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        stats.rate_limit_responses = stats.rate_limit_responses.saturating_add(1);
                    } else if status.is_client_error() {
                        stats.client_error_responses = stats.client_error_responses.saturating_add(1);
                    } else if status.is_server_error() {
                        stats.server_error_responses = stats.server_error_responses.saturating_add(1);
                    }
                }
                if attempt < politeness::MAX_RESPONSE_RETRIES
                    && politeness::should_retry_response(status, retry_after)
                {
                    let schedule = permit.retry_schedule(Some(status.as_u16()), retry_after);
                    record_retry_timing(&stats, schedule);
                    eprintln!(
                        "wikimak HTTP response retry {}/{} for {} after status {}; timing source: {}; waiting {}s",
                        attempt.saturating_add(1),
                        politeness::MAX_RESPONSE_RETRIES,
                        part.url,
                        status.as_u16(),
                        schedule.source.description(),
                        schedule.delay.as_secs()
                    );
                    drop(resp);
                    drop(permit);
                    std::thread::sleep(schedule.delay);
                    attempt += 1;
                    if let Ok(mut stats) = stats.lock() {
                        stats.attempts = stats.attempts.saturating_add(1);
                    }
                } else {
                    drop(resp);
                    if politeness::should_retry_response(status, retry_after) {
                        // The finite response retry budget is exhausted, but
                        // queued requests must still obey this response's
                        // server/robots/fallback timing.
                        let schedule =
                            permit.retry_schedule(Some(status.as_u16()), retry_after);
                        eprintln!(
                            "wikimak HTTP status {} for {} exhausted response retries; delaying queued starts {}s using {}",
                            status.as_u16(),
                            part.url,
                            schedule.delay.as_secs(),
                            schedule.source.description()
                        );
                    }
                    permit.release_now();
                    return Err(Error::HttpStatus {
                        status: status.as_u16(),
                        url: part.url.clone(),
                    });
                }
            }
            Err(error) if attempt < 3 && (error.is_connect() || error.is_timeout()) => {
                let delay = permit.transport_delay(std::time::Duration::from_secs(
                    2u64.saturating_pow(attempt + 1).max(5),
                ));
                drop(permit);
                std::thread::sleep(delay);
                attempt += 1;
                if let Ok(mut stats) = stats.lock() {
                    stats.attempts = stats.attempts.saturating_add(1);
                    stats.transport_errors = stats.transport_errors.saturating_add(1);
                }
            }
            Err(error) => {
                if let Ok(mut stats) = stats.lock() {
                    stats.transport_errors = stats.transport_errors.saturating_add(1);
                }
                permit.release_now();
                return Err(error.into());
            }
        }
    };
    let (hasher, expected) = checksum_for(part);
    let inner: Box<dyn Read + Send> = if part.size_bytes != 0 {
        Box::new(ExactSizeReader::new(
            Box::new(resp),
            part.url.clone(),
            part.size_bytes,
        ))
    } else if let Some(limit) = source_limit {
        Box::new(LimitedSourceReader::new(
            Box::new(resp),
            part.url.clone(),
            limit,
        ))
    } else {
        Box::new(resp)
    };
    Ok(VerifyingReader {
        inner,
        stats,
        hasher,
        expected,
        filename: part.filename.clone(),
        finalized: false,
    })
}

#[cfg(target_os = "macos")]
struct CurlAttempt {
    child: Child,
    stdout: ChildStdout,
    stderr: Option<std::thread::JoinHandle<io::Result<Vec<u8>>>>,
}

#[cfg(target_os = "macos")]
enum CurlPrepareError {
    /// The response is a protocol-level range mismatch.  This is the only
    /// curl condition allowed to switch a resumed transfer to byte zero.
    RangeMismatch(io::Error),
    /// The response is deterministically invalid for this source or policy;
    /// retrying the same headers cannot make it valid.
    Rejected(io::Error),
    Other(io::Error),
}

#[cfg(target_os = "macos")]
impl From<io::Error> for CurlPrepareError {
    fn from(error: io::Error) -> Self {
        Self::Other(error)
    }
}

#[cfg(target_os = "macos")]
fn read_stdout_with_inactivity<R>(
    reader: &mut R,
    buffer: &mut [u8],
    inactivity: Duration,
) -> io::Result<usize>
where
    R: Read + std::os::fd::AsRawFd,
{
    if buffer.is_empty() {
        return Ok(0);
    }
    let deadline = Instant::now() + inactivity;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for curl stdout",
            ));
        }
        // poll takes milliseconds and truncating here could make a nearly
        // expired read time out before its full inactivity interval. Round
        // up, while retaining the absolute deadline across EINTR/EAGAIN.
        let timeout_millis = remaining
            .as_nanos()
            .saturating_add(999_999)
            .checked_div(1_000_000)
            .unwrap_or(u128::MAX)
            .min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the descriptor is borrowed from the live reader for the
        // duration of this call, and poll only writes the local pollfd.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_millis) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for curl stdout",
            ));
        }
        let revents = descriptor.revents;
        if revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "curl stdout descriptor became invalid",
            ));
        }
        // HUP and ERR are deliberately passed to read: a pipe/socket can
        // still have bytes queued with either flag, and read then supplies
        // those bytes or the actual EOF/error. POLLIN is the normal case.
        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match reader.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl CurlAttempt {
    fn read_stdout(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        read_stdout_with_inactivity(&mut self.stdout, buffer, CURL_STDOUT_INACTIVITY)
    }

    fn stderr_text(&mut self) -> String {
        self.stderr
            .take()
            .and_then(|reader| reader.join().ok())
            .and_then(|result| result.ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
            .unwrap_or_default()
    }
}

#[cfg(target_os = "macos")]
// A multi-hour dump needs more than one chance after an ordinary transport
// drop, but a broken endpoint must still produce a bounded failure.  Eight
// resumptions means at most nine HTTP attempts for one source stream.  With
// the transport backoff below, consecutive failures wait at most
// 5+10+20+40+60+60+60+60 = 315 seconds before the final error.  HTTP response
// failures remain subject to the server-directed Retry-After policy below.
const MAX_CURL_RESUMPTIONS: u32 = 8;

#[cfg(target_os = "macos")]
const CURL_RESUMPTION_BACKOFF_BASE_SECS: u64 = 5;
#[cfg(target_os = "macos")]
const CURL_RESUMPTION_BACKOFF_CAP_SECS: u64 = 60;

#[cfg(target_os = "macos")]
fn curl_resumption_backoff(resumptions: u32) -> Duration {
    let multiplier = 1u64.checked_shl(resumptions.min(63)).unwrap_or(u64::MAX);
    Duration::from_secs(
        CURL_RESUMPTION_BACKOFF_BASE_SECS
            .saturating_mul(multiplier)
            .min(CURL_RESUMPTION_BACKOFF_CAP_SECS),
    )
}

#[cfg(target_os = "macos")]
fn wait_for_curl_resumption(delay: Duration) {
    #[cfg(not(test))]
    std::thread::sleep(delay);
    #[cfg(test)]
    let _ = delay;
}

// The inactivity bound applies only while CurlReader is actively trying to
// satisfy one Read::read call. A downstream parser may stop calling read
// while it is doing local work without starting this deadline.
#[cfg(target_os = "macos")]
const CURL_STDOUT_INACTIVITY: Duration = Duration::from_secs(90);

#[cfg(target_os = "macos")]
const CURL_MAX_REDIRECTS: &str = "5";

#[cfg(target_os = "macos")]
fn curl_payload_redirect_policy(url: &str) -> Option<[&'static str; 6]> {
    uses_curl_payload(url).then_some([
        "--max-redirs",
        CURL_MAX_REDIRECTS,
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
    ])
}

#[cfg(target_os = "macos")]
fn uses_curl_payload(url: &str) -> bool {
    url.starts_with("https://dumps.wikimedia.org/")
}

#[cfg(target_os = "macos")]
fn curl_arguments(url: &str, user_agent: &str, offset: u64) -> Vec<String> {
    let mut arguments = vec![
        "--disable".to_owned(),
        "--location".to_owned(),
        "--include".to_owned(),
        "--silent".to_owned(),
        "--show-error".to_owned(),
        "--connect-timeout".to_owned(),
        "30".to_owned(),
        "--user-agent".to_owned(),
        user_agent.to_owned(),
    ];
    if let Some(policy) = curl_payload_redirect_policy(url) {
        arguments.extend(policy.into_iter().map(str::to_owned));
    }
    if offset != 0 {
        arguments.extend(["--range".to_owned(), format!("{offset}-")]);
    }
    arguments.extend(["--url".to_owned(), url.to_owned()]);
    arguments
}

#[cfg(target_os = "macos")]
struct CurlReader {
    url: String,
    user_agent: String,
    total_size: Option<u64>,
    max_source_bytes: Option<u64>,
    permit: Option<politeness::Permit>,
    attempt: Option<CurlAttempt>,
    headers_ready: bool,
    buffered: Vec<u8>,
    buffered_at: usize,
    offset: u64,
    request_offset: u64,
    discard_from_body: u64,
    transport_resumptions: u32,
    response_retries: u32,
    retry_after: Option<Duration>,
    last_status: Option<u16>,
    finished: bool,
    last_failure: String,
    stats: FetchStatsHandle,
}

#[cfg(target_os = "macos")]
impl CurlReader {
    fn new(
        url: String,
        user_agent: String,
        total_size: u64,
        stats: FetchStatsHandle,
    ) -> io::Result<Self> {
        Self::new_with_offset_checked(url, user_agent, 0, Some(total_size), None, stats)
    }

    fn new_unknown(
        url: String,
        user_agent: String,
        max_source_bytes: Option<u64>,
        stats: FetchStatsHandle,
    ) -> io::Result<Self> {
        Self::new_with_offset_checked(url, user_agent, 0, None, max_source_bytes, stats)
    }

    fn new_with_offset(
        url: String,
        user_agent: String,
        offset: u64,
        total_size: u64,
        stats: FetchStatsHandle,
    ) -> io::Result<Self> {
        Self::new_with_offset_checked(url, user_agent, offset, Some(total_size), None, stats)
    }

    fn new_with_offset_checked(
        url: String,
        user_agent: String,
        offset: u64,
        total_size: Option<u64>,
        max_source_bytes: Option<u64>,
        stats: FetchStatsHandle,
    ) -> io::Result<Self> {
        let permit = politeness::acquire(&url)?;
        Self::new_with_offset_checked_with_permit(
            url,
            user_agent,
            offset,
            total_size,
            max_source_bytes,
            permit,
            stats,
        )
    }

    fn new_with_offset_checked_with_permit(
        url: String,
        user_agent: String,
        offset: u64,
        total_size: Option<u64>,
        max_source_bytes: Option<u64>,
        permit: politeness::Permit,
        stats: FetchStatsHandle,
    ) -> io::Result<Self> {
        if total_size.is_some_and(|total| offset > total) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "curl source offset {offset} exceeds advertised size for {url}"
                ),
            ));
        }
        if max_source_bytes.is_some_and(|limit| offset > limit) {
            return Err(source_limit_error(&url, max_source_bytes.unwrap_or(0)));
        }
        let attempt = Self::spawn(&url, &user_agent, offset)?;
        Ok(Self {
            url,
            user_agent,
            total_size,
            max_source_bytes,
            permit: Some(permit),
            attempt: Some(attempt),
            headers_ready: false,
            buffered: Vec::new(),
            buffered_at: 0,
            offset,
            request_offset: offset,
            discard_from_body: 0,
            transport_resumptions: 0,
            response_retries: 0,
            retry_after: None,
            last_status: None,
            finished: false,
            last_failure: "curl ended without a diagnostic".into(),
            stats,
        })
    }

    fn set_total_size(&mut self, total: u64) -> io::Result<()> {
        if let Some(expected) = self.total_size {
            if expected != total {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "inconsistent total for Wikimedia source {}: expected {expected}, got {total}",
                        self.url
                    ),
                ));
            }
        }
        if self.request_offset != 0 && total == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "resumed Wikimedia source {} disclosed an empty total at offset {}",
                    self.url, self.request_offset
                ),
            ));
        }
        if self.max_source_bytes.is_some_and(|limit| total > limit) {
            return Err(source_limit_error(
                &self.url,
                self.max_source_bytes.unwrap_or(0),
            ));
        }
        self.total_size = Some(total);
        Ok(())
    }

    fn output_limit(&self) -> Option<u64> {
        match (self.total_size, self.max_source_bytes) {
            (Some(total), Some(limit)) => Some(total.min(limit)),
            (Some(total), None) => Some(total),
            (None, Some(limit)) => Some(limit),
            (None, None) => None,
        }
    }

    fn content_length(header: &[u8]) -> io::Result<Option<u64>> {
        header_value(header, b"content-length:")
            .map(|value| {
                value.parse::<u64>().map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid curl Content-Length {value:?}: {error}"),
                    )
                })
            })
            .transpose()
    }

    fn successful_eof(&mut self) -> io::Result<()> {
        if let Some(total) = self.total_size {
            if self.offset != total {
                let error = io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "Wikimedia source {} ended at {} bytes, expected {total}",
                        self.url, self.offset
                    ),
                );
                self.permit.take();
                self.finished = true;
                return Err(error);
            }
        }
        if self.max_source_bytes.is_some_and(|limit| self.offset > limit) {
            let error = source_limit_error(
                &self.url,
                self.max_source_bytes.unwrap_or(0),
            );
            self.permit.take();
            self.finished = true;
            return Err(error);
        }
        self.finished = true;
        self.permit.take();
        Ok(())
    }

    fn spawn(url: &str, user_agent: &str, offset: u64) -> io::Result<CurlAttempt> {
        let mut command = Command::new("/usr/bin/curl");
        let mut child = command
            .args(curl_arguments(url, user_agent, offset))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("curl stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("curl stderr unavailable"))?;
        let stderr = std::thread::spawn(move || {
            let mut stderr = stderr;
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let count = stderr.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() > 16 * 1024 {
                    let excess = bytes.len() - 16 * 1024;
                    bytes.drain(..excess);
                }
            }
            Ok(bytes)
        });
        Ok(CurlAttempt {
            child,
            stdout,
            stderr: Some(stderr),
        })
    }

    fn prepare_body(&mut self) -> std::result::Result<(), CurlPrepareError> {
        while !self.headers_ready {
            let header_end = loop {
                if let Some(end) = header_end(&self.buffered) {
                    break end;
                }
                if self.buffered.len() > 64 * 1024 {
                    return Err(CurlPrepareError::Other(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "curl response headers exceed 64 KiB",
                    )));
                }
                let mut chunk = [0u8; 8192];
                let n = self
                    .attempt
                    .as_mut()
                    .expect("active curl attempt")
                    .read_stdout(&mut chunk)?;
                if n == 0 {
                    return Err(CurlPrepareError::Other(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "curl ended before response headers",
                    )));
                }
                self.buffered.extend_from_slice(&chunk[..n]);
            };
            let header = self.buffered[..header_end].to_vec();
            self.buffered.drain(..header_end);
            let (status, reason) = response_status(&header).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid curl HTTP response")
            })?;
            if let Ok(mut stats) = self.stats.lock() {
                if status == 429 {
                    stats.rate_limit_responses = stats.rate_limit_responses.saturating_add(1);
                } else if (400..500).contains(&status) {
                    stats.client_error_responses = stats.client_error_responses.saturating_add(1);
                } else if (500..600).contains(&status) {
                    stats.server_error_responses = stats.server_error_responses.saturating_add(1);
                }
            }
            if status < 200
                || (300..400).contains(&status)
                || reason.eq_ignore_ascii_case("connection established")
            {
                self.last_status = None;
                continue;
            }
            let content_length = Self::content_length(&header)
                .map_err(CurlPrepareError::Rejected)?;
            if self.request_offset == 0 && status == 200 {
                if let Some(length) = content_length {
                    self.set_total_size(length)
                        .map_err(CurlPrepareError::Rejected)?;
                }
                self.last_status = None;
                self.headers_ready = true;
                continue;
            }
            if self.request_offset == 0 && status == 206 {
                let Some(content_range) = header_value(&header, b"content-range:") else {
                    return Err(CurlPrepareError::Rejected(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "curl returned 206 without Content-Range for Wikimedia source {}",
                            self.url
                        ),
                    )));
                };
                let Some((start, end, total)) = parse_content_range(content_range) else {
                    return Err(CurlPrepareError::Rejected(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "curl returned malformed Content-Range for Wikimedia source {}",
                            self.url
                        ),
                    )));
                };
                if start != 0
                    || end < start
                    || end.checked_add(1) != Some(total)
                    || content_length.is_some_and(|length| length != total)
                {
                    return Err(CurlPrepareError::Rejected(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "curl returned inconsistent initial Content-Range for Wikimedia source {}",
                            self.url
                        ),
                    )));
                }
                self.set_total_size(total)
                    .map_err(CurlPrepareError::Rejected)?;
                self.last_status = None;
                self.headers_ready = true;
                continue;
            }
            if self.request_offset != 0 && status == 200 {
                // Range was ignored.  Keep this already-open full response
                // and discard the received prefix as it is consumed.  This is
                // deliberately distinct from a transport or HTTP failure.
                if let Some(length) = content_length {
                    self.set_total_size(length)
                        .map_err(CurlPrepareError::Rejected)?;
                    if length < self.request_offset {
                        return Err(CurlPrepareError::Rejected(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "curl full response for Wikimedia source {} is shorter than resumed offset {}",
                                self.url, self.request_offset
                            ),
                        )));
                    }
                }
                self.discard_from_body = self.offset;
                self.last_status = None;
                self.headers_ready = true;
                continue;
            }
            if self.request_offset != 0 && status == 206 {
                let content_range = header_value(&header, b"content-range:");
                let Some((start, end, total)) = content_range.and_then(parse_content_range) else {
                    return Err(CurlPrepareError::RangeMismatch(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "curl response did not honor range bytes={}- for {}",
                            self.request_offset, self.url
                        ),
                    )));
                };
                if self.total_size.is_some_and(|expected| expected != total) {
                    return Err(CurlPrepareError::Rejected(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "inconsistent resumed total for Wikimedia source {}: expected {}, got {total}",
                            self.url,
                            self.total_size.unwrap_or(0)
                        ),
                    )));
                }
                if start == self.request_offset
                    && end >= start
                    && end.checked_add(1) == Some(total)
                    && content_length
                        .is_none_or(|length| length == total - self.request_offset)
                {
                    self.set_total_size(total)
                        .map_err(CurlPrepareError::Rejected)?;
                    self.last_status = None;
                    self.headers_ready = true;
                    continue;
                }
                return Err(CurlPrepareError::RangeMismatch(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "curl response did not honor range bytes={}- for {}",
                        self.request_offset, self.url
                    ),
                )));
            }
            self.last_status = Some(status);
            self.retry_after = parse_retry_after(header_value(&header, b"retry-after:"));
            let retry_note = self.retry_after.map_or_else(
                || "; Retry-After absent or invalid".to_owned(),
                |delay| format!("; Retry-After {}s", delay.as_secs()),
            );
            return Err(CurlPrepareError::Other(io::Error::other(format!(
                "curl returned HTTP {status} while fetching {} at source offset {}{retry_note}",
                self.url, self.request_offset,
            ))));
        }
        Ok(())
    }

    fn stop_attempt(&mut self) {
        if let Some(mut attempt) = self.attempt.take() {
            let _ = attempt.child.kill();
            let _ = attempt.child.wait();
            let _ = attempt.stderr_text();
        }
    }

    fn finish_attempt(&mut self) -> io::Result<(std::process::ExitStatus, String)> {
        let mut attempt = self
            .attempt
            .take()
            .ok_or_else(|| io::Error::other("curl attempt unavailable"))?;
        let status = attempt.child.wait();
        let stderr = attempt.stderr_text();
        status.map(|status| (status, stderr))
    }

    fn transfer_error(&self) -> io::Error {
        io::Error::other(format!(
            "curl transfer failed for {} at source offset {} after {} transport resumptions and {} response retries: {}",
            self.url,
            self.offset,
            self.transport_resumptions,
            self.response_retries,
            self.last_failure
        ))
    }

    fn fail<T>(&mut self, error: io::Error) -> io::Result<T> {
        self.stop_attempt();
        self.permit.take();
        self.finished = true;
        Err(error)
    }

    fn restart(&mut self) -> io::Result<()> {
        self.stop_attempt();
        let status = self.last_status;
        let mut permit = self
            .permit
            .take()
            .ok_or_else(|| io::Error::other("curl permit unavailable"))?;
        let (retry_kind, retry_number, retry_limit) = if status.is_some() {
            if self.response_retries >= politeness::MAX_RESPONSE_RETRIES {
                let schedule = permit.retry_schedule(status, self.retry_after.take());
                eprintln!(
                    "wikimak curl response retries exhausted at source offset {} for {}; delaying queued starts {}s using {}",
                    self.offset,
                    self.url,
                    schedule.delay.as_secs(),
                    schedule.source.description()
                );
                return Err(self.transfer_error());
            }
            (
                "response",
                self.response_retries.saturating_add(1),
                politeness::MAX_RESPONSE_RETRIES,
            )
        } else {
            if self.transport_resumptions >= MAX_CURL_RESUMPTIONS {
                self.permit.take();
                return Err(self.transfer_error());
            }
            (
                "transport",
                self.transport_resumptions.saturating_add(1),
                MAX_CURL_RESUMPTIONS,
            )
        };
        let fallback = curl_resumption_backoff(self.transport_resumptions);
        let (delay, timing_source) = if status.is_some() {
            let schedule = permit.retry_schedule(status, self.retry_after.take());
            record_retry_timing(&self.stats, schedule);
            (schedule.delay, schedule.source.description())
        } else {
            (
                permit.transport_delay(fallback),
                "local curl transport-failure fallback",
            )
        };
        eprintln!(
            "wikimak curl {retry_kind} retry {retry_number}/{retry_limit} at source offset {} for {} after {}; timing source: {timing_source}; waiting {}s before resuming",
            self.offset,
            self.url,
            self.last_failure,
            delay.as_secs()
        );
        wait_for_curl_resumption(delay);
        if status.is_some() {
            self.response_retries += 1;
        } else {
            self.transport_resumptions += 1;
        }
        if let Ok(mut stats) = self.stats.lock() {
            stats.attempts = stats.attempts.saturating_add(1);
        }
        // A resumption is a new HTTP request. Retain the central lease while
        // its ordinary start spacing and shared destination schedule advance;
        // another source cannot steal this restart reservation.
        permit.wait_for_next_start()?;
        self.request_offset = self.offset;
        self.discard_from_body = 0;
        let attempt = Self::spawn(
            &self.url,
            &self.user_agent,
            self.request_offset,
        )?;
        self.permit = Some(permit);
        self.attempt = Some(attempt);
        self.headers_ready = false;
        self.buffered.clear();
        self.buffered_at = 0;
        self.last_status = None;
        Ok(())
    }

    fn restart_from_zero(&mut self) -> io::Result<()> {
        self.stop_attempt();
        let mut permit = self
            .permit
            .take()
            .ok_or_else(|| io::Error::other("curl permit unavailable"))?;
        // A zero-origin range fallback is still a new request. Retain the
        // reservation while advancing normal process and destination-local
        // start spacing.
        permit.wait_for_next_start()?;
        self.request_offset = 0;
        self.discard_from_body = self.offset;
        let attempt = Self::spawn(&self.url, &self.user_agent, 0)?;
        self.permit = Some(permit);
        self.attempt = Some(attempt);
        self.headers_ready = false;
        self.buffered.clear();
        self.buffered_at = 0;
        self.last_status = None;
        if let Ok(mut stats) = self.stats.lock() {
            stats.attempts = stats.attempts.saturating_add(1);
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for CurlReader {
    fn drop(&mut self) {
        // A downstream parser can finish or fail before curl has observed the
        // closed stdout pipe. Stop and reap it while the pipe is still owned,
        // avoiding curl's misleading secondary "failure writing output"
        // diagnostic on the parent's stderr.
        self.stop_attempt();
    }
}

#[cfg(target_os = "macos")]
impl Read for CurlReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.finished || out.is_empty() {
            return Ok(0);
        }
        loop {
            if let Err(error) = self.prepare_body() {
                let error = match error {
                    CurlPrepareError::RangeMismatch(error) => {
                        // Only the explicitly classified protocol mismatch
                        // may start a zero-origin transfer.  HTTP statuses,
                        // 429s, and transport failures stay on their normal
                        // error/resumption paths below.
                        self.last_failure = format!("range response error: {error}");
                        match self.restart_from_zero() {
                            Ok(()) => continue,
                            Err(error) => return self.fail(error),
                        }
                    }
                    CurlPrepareError::Rejected(error) => return self.fail(error),
                    CurlPrepareError::Other(error) => error,
                };
                if self
                    .last_status
                    .is_some_and(|status| status != 429 && !(500..600).contains(&status))
                {
                    return self.fail(error);
                }
                if self.last_status.is_none() {
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.transport_errors = stats.transport_errors.saturating_add(1);
                    }
                }
                self.last_failure = format!("response/read error: {error}");
                match self.restart() {
                    Ok(()) => continue,
                    Err(error) => return self.fail(error),
                }
            }
            if self.discard_from_body != 0 {
                if self.buffered_at < self.buffered.len() {
                    let count = (self.discard_from_body as usize)
                        .min(self.buffered.len().saturating_sub(self.buffered_at));
                    self.buffered_at += count;
                    self.discard_from_body -= count as u64;
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.bytes_received = stats.bytes_received.saturating_add(count as u64);
                    }
                    if self.buffered_at == self.buffered.len() {
                        self.buffered.clear();
                        self.buffered_at = 0;
                    }
                    continue;
                }
                let count = (self.discard_from_body as usize).min(128 * 1024);
                let mut discarded = vec![0u8; count];
                match self
                    .attempt
                    .as_mut()
                    .expect("active curl attempt")
                    .read_stdout(&mut discarded)
                {
                    Ok(n) if n != 0 => {
                        self.discard_from_body -= n as u64;
                        if let Ok(mut stats) = self.stats.lock() {
                            stats.bytes_received = stats.bytes_received.saturating_add(n as u64);
                        }
                        continue;
                    }
                    Ok(_) => {
                        return self.fail(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "curl response for Wikimedia source {} ended before resumed prefix of {} bytes",
                                self.url, self.offset
                            ),
                        ));
                    }
                    Err(error) => {
                        self.last_failure = format!("stdout read error: {error}");
                        if let Ok(mut stats) = self.stats.lock() {
                            stats.transport_errors = stats.transport_errors.saturating_add(1);
                        }
                        match self.restart() {
                            Ok(()) => continue,
                            Err(error) => return self.fail(error),
                        }
                    }
                }
            }
            if self.output_limit().is_some_and(|limit| self.offset >= limit) {
                if self.buffered_at < self.buffered.len() {
                    return self.fail(if self
                        .max_source_bytes
                        .is_some_and(|limit| self.offset >= limit)
                    {
                        source_limit_error(
                            &self.url,
                            self.max_source_bytes.unwrap_or(0),
                        )
                    } else {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "Wikimedia source {} returned more bytes than advertised",
                                self.url
                            ),
                        )
                    });
                }
                let mut probe = [0u8; 1];
                match self
                    .attempt
                    .as_mut()
                    .expect("active curl attempt")
                    .read_stdout(&mut probe)
                {
                    Ok(n) if n != 0 => {
                        return self.fail(if self
                            .max_source_bytes
                            .is_some_and(|limit| self.offset >= limit)
                        {
                            source_limit_error(
                                &self.url,
                                self.max_source_bytes.unwrap_or(0),
                            )
                        } else {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "Wikimedia source {} returned more bytes than advertised",
                                    self.url
                                ),
                            )
                        });
                    }
                    Ok(_) => {
                        let (status, stderr) = match self.finish_attempt() {
                            Ok(result) => result,
                            Err(error) => return self.fail(error),
                        };
                        if status.success() {
                            return match self.successful_eof() {
                                Ok(()) => Ok(0),
                                Err(error) => self.fail(error),
                            };
                        }
                        self.last_failure = if stderr.is_empty() {
                            format!("curl exited with {status}")
                        } else {
                            format!("curl exited with {status}: {stderr}")
                        };
                        if let Ok(mut stats) = self.stats.lock() {
                            stats.transport_errors = stats.transport_errors.saturating_add(1);
                        }
                        match self.restart() {
                            Ok(()) => continue,
                            Err(error) => return self.fail(error),
                        }
                    }
                    Err(error) => {
                        self.last_failure = format!("stdout read error: {error}");
                        if let Ok(mut stats) = self.stats.lock() {
                            stats.transport_errors = stats.transport_errors.saturating_add(1);
                        }
                        match self.restart() {
                            Ok(()) => continue,
                            Err(error) => return self.fail(error),
                        }
                    }
                }
            }
            if self.buffered_at < self.buffered.len() {
                let n = out
                    .len()
                    .min(
                        self.output_limit()
                            .map(|limit| limit.saturating_sub(self.offset))
                            .unwrap_or(u64::MAX)
                            .min(usize::MAX as u64) as usize,
                    )
                    .min(self.buffered.len().saturating_sub(self.buffered_at));
                out[..n].copy_from_slice(
                    &self.buffered[self.buffered_at..self.buffered_at.saturating_add(n)],
                );
                self.buffered_at += n;
                self.offset += n as u64;
                if self.buffered_at == self.buffered.len() {
                    self.buffered.clear();
                    self.buffered_at = 0;
                }
                if let Ok(mut stats) = self.stats.lock() {
                    stats.bytes_received = stats.bytes_received.saturating_add(n as u64);
                }
                return Ok(n);
            }
            let read_len = out
                .len()
                .min(
                    self.output_limit()
                        .map(|limit| limit.saturating_sub(self.offset))
                        .unwrap_or(u64::MAX)
                        .min(usize::MAX as u64) as usize,
                );
            match self
                .attempt
                .as_mut()
                .expect("active curl attempt")
                .read_stdout(&mut out[..read_len])
            {
                Ok(n) if n != 0 => {
                    self.offset += n as u64;
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.bytes_received = stats.bytes_received.saturating_add(n as u64);
                    }
                    return Ok(n);
                }
                Ok(_) => {
                    let (status, stderr) = match self.finish_attempt() {
                        Ok(result) => result,
                        Err(error) => return self.fail(error),
                    };
                    if status.success() {
                        return match self.successful_eof() {
                            Ok(()) => Ok(0),
                            Err(error) => self.fail(error),
                        };
                    }
                    self.last_failure = if stderr.is_empty() {
                        format!("curl exited with {status}")
                    } else {
                        format!("curl exited with {status}: {stderr}")
                    };
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.transport_errors = stats.transport_errors.saturating_add(1);
                    }
                    if let Err(error) = self.restart() {
                        return self.fail(error);
                    }
                }
                Err(error) => {
                    self.last_failure = format!("stdout read error: {error}");
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.transport_errors = stats.transport_errors.saturating_add(1);
                    }
                    if let Err(error) = self.restart() {
                        return self.fail(error);
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
        .or_else(|| {
            bytes
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| position + 2)
        })
}

#[cfg(target_os = "macos")]
fn response_status(header: &[u8]) -> Option<(u16, &str)> {
    let line = std::str::from_utf8(header.split(|byte| *byte == b'\n').next()?)
        .ok()?
        .trim();
    let mut fields = line.splitn(3, ' ');
    fields.next()?.starts_with("HTTP/").then_some(())?;
    let status = fields.next()?.parse().ok()?;
    Some((status, fields.next().unwrap_or("").trim()))
}

#[cfg(target_os = "macos")]
fn header_value<'a>(header: &'a [u8], name: &[u8]) -> Option<&'a str> {
    header.split(|byte| *byte == b'\n').find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        (line.len() >= name.len() && line[..name.len()].eq_ignore_ascii_case(name))
            .then(|| std::str::from_utf8(line[name.len()..].trim_ascii()).ok())
            .flatten()
    })
}

#[cfg(target_os = "macos")]
fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    politeness::parse_retry_after_at(value, chrono::Utc::now())
}

#[cfg(target_os = "macos")]
fn fetch_with_curl(
    part: &Part,
    max_source_bytes: Option<u64>,
) -> Result<VerifyingReader<Box<dyn Read + Send>>> {
    let stats = Arc::new(Mutex::new(FetchStats {
        attempts: 1,
        ..FetchStats::default()
    }));
    let reader = if part.size_bytes == 0 {
        CurlReader::new_unknown(
            part.url.clone(),
            curl_user_agent(),
            max_source_bytes,
            Arc::clone(&stats),
        )?
    } else {
        CurlReader::new(
            part.url.clone(),
            curl_user_agent(),
            part.size_bytes,
            Arc::clone(&stats),
        )?
    };
    let (hasher, expected) = checksum_for(part);
    Ok(VerifyingReader {
        inner: Box::new(reader),
        stats,
        hasher,
        expected,
        filename: part.filename.clone(),
        finalized: false,
    })
}

#[cfg(target_os = "macos")]
fn curl_user_agent() -> String {
    let operator = std::env::var("SARUN_WIKIMEDIA_CONTACT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("; operator: {value}"))
        .unwrap_or_default();
    format!(
        "sarun-wikimak/{} (+https://github.com/telepancake/sarun{operator})",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::io::{BufRead, BufReader, Cursor, Write};
    use std::net::TcpListener;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    fn set_source_limit(value: Option<&str>) -> EnvRestore {
        let name = MAX_SOURCE_BYTES_ENV;
        let previous = std::env::var_os(name);
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        EnvRestore { name, previous }
    }

    fn unknown_size_part() -> Part {
        Part {
            url: "http://127.0.0.1/source".to_owned(),
            filename: "source.bin".to_owned(),
            size_bytes: 0,
            sha256: None,
            sha1: None,
            md5: None,
        }
    }

    #[test]
    fn unknown_size_without_limit_env_is_unbounded() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = set_source_limit(None);
        assert_eq!(source_byte_ceiling(&unknown_size_part()).unwrap(), None);
    }

    #[test]
    fn unknown_size_with_empty_limit_env_is_unbounded() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = set_source_limit(Some(""));
        assert_eq!(source_byte_ceiling(&unknown_size_part()).unwrap(), None);
    }

    #[test]
    fn explicit_limit_bounds_unknown_size_reader() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = set_source_limit(Some("5"));
        let part = unknown_size_part();
        let limit = source_byte_ceiling(&part).unwrap().unwrap();
        let mut reader = LimitedSourceReader::new(
            Box::new(Cursor::new(b"abcdef".to_vec())),
            part.url,
            limit,
        );
        let mut body = Vec::new();
        let error = reader
            .read_to_end(&mut body)
            .expect_err("explicit source limit must reject an oversized body");
        assert_eq!(body, b"abcde");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(MAX_SOURCE_BYTES_ENV));
    }

    #[test]
    fn reqwest_retry_after_is_counted_as_server_timing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
        });
        let part = Part {
            url: format!("http://{address}/server-timed"),
            filename: "server-timed.bin".to_owned(),
            size_bytes: 2,
            sha256: None,
            sha1: None,
            md5: None,
        };
        let client = Client::builder().build().unwrap();
        let mut reader = fetch(&client, &part).expect("fetch after one server-timed retry");
        let stats = reader.stats_handle();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"ok");
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 2);
        assert_eq!(stats.server_error_responses, 1);
        assert_eq!(stats.server_timed_retries, 1);
        assert_eq!(stats.fallback_timed_retries, 0);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod curl_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn response_header_parsing_handles_redirects_and_ranges() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: /next\r\n\r\n";
        assert_eq!(header_end(redirect), Some(redirect.len()));
        assert_eq!(response_status(redirect), Some((302, "Found")));

        let range = b"HTTP/2 206 \r\nContent-Range: bytes 123-999/1000\r\n\r\n";
        assert_eq!(response_status(range), Some((206, "")));
        assert_eq!(header_value(range, b"content-range:"), Some("bytes 123-999/1000"));
    }

    #[test]
    fn bulk_curl_allows_only_bounded_https_redirects_and_keeps_local_origins_on_reqwest() {
        assert_eq!(
            curl_payload_redirect_policy("https://dumps.wikimedia.org/enwiki/test.xml.bz2"),
            Some([
                "--max-redirs",
                "5",
                "--proto",
                "=https",
                "--proto-redir",
                "=https"
            ])
        );
        assert!(uses_curl_payload(
            "https://dumps.wikimedia.org/enwiki/test.xml.bz2"
        ));
        assert!(curl_payload_redirect_policy("http://127.0.0.1:43123/test.xml.bz2").is_none());
        assert!(!uses_curl_payload("http://127.0.0.1:43123/test.xml.bz2"));
        assert!(curl_payload_redirect_policy("https://en.wikipedia.org/test.xml.bz2").is_none());
        assert!(!uses_curl_payload("https://en.wikipedia.org/test.xml.bz2"));
    }

    #[test]
    fn retry_after_accepts_delta_seconds_and_http_date() {
        assert_eq!(parse_retry_after(Some("17")), Some(Duration::from_secs(17)));
        assert!(parse_retry_after(Some("not-a-retry-delay")).is_none());
        let future = (chrono::Utc::now() + chrono::Duration::seconds(20))
            .to_rfc2822();
        let parsed = parse_retry_after(Some(&future)).unwrap();
        assert!(parsed <= Duration::from_secs(20));
        assert!(parsed >= Duration::from_secs(15));
    }

    #[test]
    fn curl_server_retry_after_is_uncapped_and_counted_as_server_timing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 86400\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new(
            format!("http://{address}/server-timed"),
            "sarun-test".to_owned(),
            2,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"ok");
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 2);
        assert_eq!(stats.server_error_responses, 1);
        assert_eq!(stats.server_timed_retries, 1);
        assert_eq!(stats.fallback_timed_retries, 0);
    }

    #[test]
    fn curl_command_disables_user_configuration_before_policy_options() {
        let arguments = curl_arguments("http://127.0.0.1:1/source", "sarun-test", 7);
        assert_eq!(arguments.first().map(String::as_str), Some("--disable"));
        assert!(arguments.iter().any(|argument| argument == "--connect-timeout"));
        assert!(!arguments.iter().any(|argument| argument == "--max-time"));
        assert!(!arguments.iter().any(|argument| argument == "--speed-limit"));
        assert!(!arguments.iter().any(|argument| argument == "--speed-time"));
        assert!(arguments.iter().any(|argument| argument == "--range"));
        assert!(arguments.iter().any(|argument| argument == "7-"));
    }

    #[test]
    fn delayed_consumer_call_does_not_use_an_expired_stdout_deadline() {
        use std::os::unix::net::UnixStream;

        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(b"ready-before-consumer-pause").unwrap();
        std::thread::sleep(Duration::from_millis(30));

        let mut output = [0u8; 5];
        let read = read_stdout_with_inactivity(
            &mut reader,
            &mut output,
            Duration::from_millis(5),
        )
        .expect("a delayed call must start a fresh active-read deadline");
        assert_eq!(&output[..read], b"ready");
    }

    #[test]
    fn active_stdout_read_wait_returns_a_timeout() {
        use std::os::unix::net::UnixStream;

        let (_writer, mut reader) = UnixStream::pair().unwrap();
        let mut output = [0u8; 1];
        let started = Instant::now();
        let error = read_stdout_with_inactivity(
            &mut reader,
            &mut output,
            Duration::from_millis(20),
        )
        .expect_err("an active read with no stdout must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn curl_resumption_backoff_is_exponential_and_capped() {
        assert_eq!(curl_resumption_backoff(0), Duration::from_secs(5));
        assert_eq!(curl_resumption_backoff(1), Duration::from_secs(10));
        assert_eq!(curl_resumption_backoff(2), Duration::from_secs(20));
        assert_eq!(curl_resumption_backoff(3), Duration::from_secs(40));
        assert_eq!(curl_resumption_backoff(4), Duration::from_secs(60));
        assert_eq!(curl_resumption_backoff(MAX_CURL_RESUMPTIONS), Duration::from_secs(60));
    }

    #[test]
    fn curl_unknown_size_discovers_total_and_resumes_only_the_suffix() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = {
                    let mut reader = BufReader::new(&stream);
                    let mut request = String::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        request.push_str(&line);
                    }
                    request
                };
                if attempt == 0 {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc")
                        .unwrap();
                    stream.shutdown(std::net::Shutdown::Write).unwrap();
                } else {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("range: bytes=3-"),
                        "{request}"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nContent-Range: bytes 3-9/10\r\nConnection: close\r\n\r\ndefghij",
                        )
                        .unwrap();
                }
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new_unknown(
            format!("http://{address}/unknown"),
            "sarun-test".to_owned(),
            None,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"abcdefghij");
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 2);
        assert_eq!(stats.bytes_received, 10);
    }

    #[test]
    fn curl_unknown_size_rejects_an_inconsistent_resumed_total() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = {
                    let mut reader = BufReader::new(&stream);
                    let mut request = String::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        request.push_str(&line);
                    }
                    request
                };
                if attempt == 0 {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc")
                        .unwrap();
                    stream.shutdown(std::net::Shutdown::Write).unwrap();
                } else {
                    assert!(request.to_ascii_lowercase().contains("range: bytes=3-"));
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nContent-Range: bytes 3-9/11\r\nConnection: close\r\n\r\ndefghij",
                        )
                        .unwrap();
                }
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new_unknown(
            format!("http://{address}/inconsistent"),
            "sarun-test".to_owned(),
            None,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        let error = reader
            .read_to_end(&mut body)
            .expect_err("inconsistent resumed total must be rejected");
        assert!(error.to_string().contains("inconsistent resumed total"));
        assert_eq!(
            stats.lock().unwrap().attempts,
            2,
            "the deterministic mismatch must not trigger another request"
        );
        server.join().unwrap();
    }

    #[test]
    fn curl_unknown_size_enforces_the_ceiling_during_body_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            {
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcdef")
                .unwrap();
        });
        let stats = Arc::new(Mutex::new(FetchStats::default()));
        let mut reader = CurlReader::new_unknown(
            format!("http://{address}/ceiling"),
            "sarun-test".to_owned(),
            Some(5),
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        let error = reader
            .read_to_end(&mut body)
            .expect_err("curl source must stop at its per-source ceiling");
        assert!(error.to_string().contains("ceiling"));
        assert_eq!(body, b"abcde");
        server.join().unwrap();
    }

    #[test]
    fn curl_known_size_rejects_a_successful_truncated_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            {
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabc")
                .unwrap();
        });
        let stats = Arc::new(Mutex::new(FetchStats::default()));
        let mut reader = CurlReader::new(
            format!("http://{address}/truncated"),
            "sarun-test".to_owned(),
            10,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        let error = reader
            .read_to_end(&mut body)
            .expect_err("known-size truncated body must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        server.join().unwrap();
    }

    #[test]
    fn curl_reader_resumes_without_duplicating_partial_output() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                {
                    let mut reader = BufReader::new(&stream);
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        request.push_str(&line);
                    }
                }
                if attempt == 0 {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
                        .unwrap();
                } else {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("range: bytes=3-"),
                        "{request}"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nContent-Range: bytes 3-9/10\r\n\r\ndefghij",
                        )
                        .unwrap();
                }
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new(
            format!("http://{address}/part"),
            "sarun-test".to_string(),
            10,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"abcdefghij");
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 2);
        assert_eq!(stats.bytes_received, 10);
    }

    #[test]
    fn curl_reader_supports_multiple_range_resumptions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let suffixes = [(0, b"ab".as_slice()), (2, b"cde".as_slice()), (5, b"fgh".as_slice()), (8, b"ij".as_slice())];
            for (attempt, (offset, body)) in suffixes.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let request = {
                    let mut reader = BufReader::new(&stream);
                    let mut request = String::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        request.push_str(&line);
                    }
                    request
                };
                if attempt == 0 {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n")
                        .unwrap();
                } else {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains(&format!("range: bytes={offset}-")),
                        "{request}"
                    );
                    let remaining = 10 - offset;
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {remaining}\r\nContent-Range: bytes {offset}-9/10\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
                stream.write_all(body).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new(
            format!("http://{address}/multiple-resumes"),
            "sarun-test".to_owned(),
            10,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"abcdefghij");
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 4, "one initial request plus three resumptions");
        assert_eq!(stats.bytes_received, 10, "only suffix bytes are delivered");
    }

    #[test]
    fn curl_reader_stops_after_the_bounded_resumption_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let total = 100u64;
            for attempt in 0..=MAX_CURL_RESUMPTIONS {
                let (mut stream, _) = listener.accept().unwrap();
                let offset = attempt as u64;
                let request = {
                    let mut reader = BufReader::new(&stream);
                    let mut request = String::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        request.push_str(&line);
                    }
                    request
                };
                if offset == 0 {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                } else {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains(&format!("range: bytes={offset}-")),
                        "{request}"
                    );
                    let remaining = total - offset;
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {remaining}\r\nContent-Range: bytes {offset}-99/100\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
                stream.write_all(&[b'a' + attempt as u8]).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new(
            format!("http://{address}/bounded-resumes"),
            "sarun-test".to_owned(),
            100,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        let error = reader
            .read_to_end(&mut body)
            .expect_err("repeated transport failures must eventually fail");
        server.join().unwrap();
        assert!(error
            .to_string()
            .contains(&format!(
                "after {MAX_CURL_RESUMPTIONS} transport resumptions"
            )));
        assert_eq!(body.len(), MAX_CURL_RESUMPTIONS as usize + 1);
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, MAX_CURL_RESUMPTIONS as u64 + 1);
    }

    #[test]
    fn curl_transport_budget_does_not_expand_server_error_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..=politeness::MAX_RESPONSE_RETRIES {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: malformed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new(
            format!("http://{address}/server-error"),
            "sarun-test".to_owned(),
            10,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        let error = reader
            .read_to_end(&mut body)
            .expect_err("server errors must retain their singular retry budget");
        server.join().unwrap();
        assert!(error.to_string().contains("HTTP 503"));
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 2);
        assert_eq!(stats.server_error_responses, 2);
        assert_eq!(stats.transport_errors, 0);
        assert_eq!(stats.fallback_timed_retries, 1);
        assert_eq!(stats.server_timed_retries, 0);
    }

    #[test]
    fn curl_reader_does_not_retry_deterministic_http_refusal() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let stats = Arc::new(Mutex::new(FetchStats {
            attempts: 1,
            ..FetchStats::default()
        }));
        let mut reader = CurlReader::new(
            format!("http://{address}/refused"),
            "sarun-test".to_owned(),
            10,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        let error = reader
            .read_to_end(&mut body)
            .expect_err("deterministic client refusal must be surfaced");
        server.join().unwrap();
        assert!(error.to_string().contains("HTTP 403"));
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 1);
        assert_eq!(stats.client_error_responses, 1);
        assert_eq!(stats.transport_errors, 0);
    }

    #[test]
    fn curl_reader_reuses_ignored_range_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            {
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    request.push_str(&line);
                }
            }
            assert!(request.to_ascii_lowercase().contains("range: bytes=3-"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcdefghij",
                )
                .unwrap();
        });
        let stats = Arc::new(Mutex::new(FetchStats::default()));
        let mut reader = CurlReader::new_with_offset(
            format!("http://{address}/ignored"),
            "sarun-test".to_string(),
            3,
            10,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"defghij");
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 0);
        assert_eq!(stats.bytes_received, 10);
    }

    #[test]
    fn curl_reader_malformed_range_uses_one_zero_origin_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                {
                    let mut reader = BufReader::new(&stream);
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        request.push_str(&line);
                    }
                }
                if attempt == 0 {
                    assert!(request.to_ascii_lowercase().contains("range: bytes=3-"));
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nContent-Range: bytes 3-8/10\r\nConnection: close\r\n\r\ndefghij",
                        )
                        .unwrap();
                } else {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcdefghij",
                        )
                        .unwrap();
                }
            }
        });
        let stats = Arc::new(Mutex::new(FetchStats::default()));
        let mut reader = CurlReader::new_with_offset(
            format!("http://{address}/malformed"),
            "sarun-test".to_string(),
            3,
            10,
            Arc::clone(&stats),
        )
        .unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"defghij");
        let stats = stats.lock().unwrap().clone();
        assert_eq!(stats.attempts, 1);
        assert_eq!(stats.bytes_received, 10);
    }
}
