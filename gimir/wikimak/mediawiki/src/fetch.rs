//! Streaming HTTP fetch with on-EOF checksum verification.
//!
//! Per SPEC §API: the returned reader verifies the part's checksum on
//! EOF. Calling `into_inner()` or dropping mid-stream skips the check.
//! `sha256` takes precedence; if `None`, `sha1` is used.

use std::io::{self, Read};
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::process::{Child, ChildStdout, Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use md5::Md5;
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
}

pub type FetchStatsHandle = Arc<Mutex<FetchStats>>;

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
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    Error::ChecksumMismatch {
                        part: self.filename.clone(),
                        expected: self.expected.clone(),
                        got,
                    },
                ));
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

/// Fetch a Part: GET the URL, return a streaming reader.
pub fn fetch(client: &Client, part: &Part) -> Result<VerifyingReader<Box<dyn Read + Send>>> {
    politeness::ensure_robots(client, &part.url)?;
    #[cfg(target_os = "macos")]
    if part.url.starts_with("https://dumps.wikimedia.org/") {
        return fetch_with_curl(part);
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
                    let delay = permit.retry_delay(Some(status.as_u16()), retry_after);
                    drop(resp);
                    drop(permit);
                    std::thread::sleep(delay);
                    attempt += 1;
                    if let Ok(mut stats) = stats.lock() {
                        stats.attempts = stats.attempts.saturating_add(1);
                    }
                } else {
                    drop(resp);
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        // Do not retry a refusal without an explicit server
                        // window, but do publish a conservative shared
                        // cooldown before waking queued workers.
                        let _ = permit.retry_delay(Some(status.as_u16()), retry_after);
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
    let (hasher, expected) = match (&part.sha256, &part.sha1, &part.md5) {
        (Some(h), _, _) => (Some(Hasher::Sha256(Sha256::new())), h.to_lowercase()),
        (None, Some(h), _) => (Some(Hasher::Sha1(Sha1::new())), h.to_lowercase()),
        (None, None, Some(h)) => (Some(Hasher::Md5(Md5::new())), h.to_lowercase()),
        (None, None, None) => (None, String::new()),
    };
    Ok(VerifyingReader {
        inner: Box::new(resp) as Box<dyn Read + Send>,
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
impl CurlAttempt {
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
// Keep every failed dump stream to one resumption.  A 429 is retried only when
// the server supplied a Retry-After interval; otherwise it is surfaced
// immediately instead of inventing a cadence against a host asking us to stop.
const MAX_CURL_RESUMPTIONS: u32 = 1;

// A dump response may be very large, but an actually idle TCP connection is
// not useful.  curl's total timeout is deliberately a day; this independent
// low-speed timeout prevents a blackholed connection from making an import
// appear frozen for hours while preserving resumable byte offsets.
#[cfg(target_os = "macos")]
const CURL_LOW_SPEED_LIMIT: &str = "1024";
#[cfg(target_os = "macos")]
const CURL_LOW_SPEED_TIME: &str = "90";

#[cfg(target_os = "macos")]
struct CurlReader {
    url: String,
    user_agent: String,
    permit: Option<politeness::Permit>,
    attempt: Option<CurlAttempt>,
    headers_ready: bool,
    buffered: Vec<u8>,
    buffered_at: usize,
    offset: u64,
    retries: u32,
    retry_after: Option<Duration>,
    last_status: Option<u16>,
    finished: bool,
    last_failure: String,
    stats: FetchStatsHandle,
}

#[cfg(target_os = "macos")]
impl CurlReader {
    fn new(url: String, user_agent: String, stats: FetchStatsHandle) -> io::Result<Self> {
        let permit = politeness::acquire(&url)?;
        let attempt = Self::spawn(&url, &user_agent, 0)?;
        Ok(Self {
            url,
            user_agent,
            permit: Some(permit),
            attempt: Some(attempt),
            headers_ready: false,
            buffered: Vec::new(),
            buffered_at: 0,
            offset: 0,
            retries: 0,
            retry_after: None,
            last_status: None,
            finished: false,
            last_failure: "curl ended without a diagnostic".into(),
            stats,
        })
    }

    fn spawn(url: &str, user_agent: &str, offset: u64) -> io::Result<CurlAttempt> {
        let mut command = Command::new("/usr/bin/curl");
        command.args([
            "--location",
            "--include",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "30",
            "--max-time",
            "86400",
            "--speed-limit",
            CURL_LOW_SPEED_LIMIT,
            "--speed-time",
            CURL_LOW_SPEED_TIME,
            "--user-agent",
            user_agent,
        ]);
        if offset != 0 {
            command.args(["--range", &format!("{offset}-")]);
        }
        let mut child = command
            .args(["--url", url])
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

    fn prepare_body(&mut self) -> io::Result<()> {
        while !self.headers_ready {
            let header_end = loop {
                if let Some(end) = header_end(&self.buffered) {
                    break end;
                }
                if self.buffered.len() > 64 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "curl response headers exceed 64 KiB",
                    ));
                }
                let mut chunk = [0u8; 8192];
                let n = self
                    .attempt
                    .as_mut()
                    .expect("active curl attempt")
                    .stdout
                    .read(&mut chunk)?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "curl ended before response headers",
                    ));
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
            if self.offset == 0 && (status == 200 || status == 206) {
                self.last_status = None;
                self.headers_ready = true;
                continue;
            }
            if self.offset != 0 && status == 206 {
                let start = header_value(&header, b"content-range:")
                    .and_then(|value| value.strip_prefix("bytes "))
                    .and_then(|value| value.split('-').next())
                    .and_then(|value| value.parse::<u64>().ok());
                if start == Some(self.offset) {
                    self.last_status = None;
                    self.headers_ready = true;
                    continue;
                }
            }
            self.last_status = Some(status);
            self.retry_after = parse_retry_after(header_value(&header, b"retry-after:"));
            let retry_note = self.retry_after.map_or_else(
                || "; Retry-After absent".to_owned(),
                |delay| format!("; Retry-After {}s", delay.as_secs()),
            );
            return Err(io::Error::other(format!(
                "curl returned HTTP {status} while fetching {} at source offset {}{retry_note}",
                self.url, self.offset,
            )));
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
            "curl transfer failed for {} at source offset {} after {} resumptions: {}",
            self.url, self.offset, self.retries, self.last_failure
        ))
    }

    fn restart(&mut self) -> io::Result<()> {
        self.stop_attempt();
        if self.retries >= MAX_CURL_RESUMPTIONS {
            return Err(self.transfer_error());
        }
        let status = self.last_status;
        let fallback = Duration::from_secs(
            2u64
                .saturating_pow(self.retries.saturating_add(1))
                .min(30)
                .max(5),
        );
        let delay = if let Some(permit) = self.permit.as_mut() {
            if status.is_some() {
                permit.retry_delay(status, self.retry_after.take())
            } else {
                permit.transport_delay(fallback)
            }
        } else {
            self.retry_after.take().unwrap_or(fallback)
        };
        eprintln!(
            "wikimak curl retry {}/{} at source offset {} for {} after {}; waiting {}s before resuming",
            self.retries.saturating_add(1),
            MAX_CURL_RESUMPTIONS,
            self.offset,
            self.url,
            self.last_failure,
            delay.as_secs()
        );
        if let Some(mut permit) = self.permit.take() {
            permit.release_now();
        }
        std::thread::sleep(delay);
        self.retries += 1;
        if let Ok(mut stats) = self.stats.lock() {
            stats.attempts = stats.attempts.saturating_add(1);
        }
        // A resumption is a new HTTP request. Reacquire the central lease so
        // its start spacing and active-body limit also cover range retries.
        self.permit = Some(politeness::acquire(&self.url)?);
        self.attempt = Some(Self::spawn(&self.url, &self.user_agent, self.offset)?);
        self.headers_ready = false;
        self.buffered.clear();
        self.buffered_at = 0;
        self.last_status = None;
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
                if self.last_status.is_some_and(|status| status != 429 && !(500..600).contains(&status)) {
                    return Err(error);
                }
                if self.last_status == Some(429) && self.retry_after.is_none() {
                    if let Some(permit) = self.permit.as_mut() {
                        let _ = permit.retry_delay(Some(429), None);
                    }
                    return Err(error);
                }
                if self.last_status.is_none() {
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.transport_errors = stats.transport_errors.saturating_add(1);
                    }
                }
                self.last_failure = format!("response/read error: {error}");
                self.restart()?;
                continue;
            }
            if self.buffered_at < self.buffered.len() {
                let n = out
                    .len()
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
            match self
                .attempt
                .as_mut()
                .expect("active curl attempt")
                .stdout
                .read(out)
            {
                Ok(n) if n != 0 => {
                    self.offset += n as u64;
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.bytes_received = stats.bytes_received.saturating_add(n as u64);
                    }
                    return Ok(n);
                }
                Ok(_) => {
                    let (status, stderr) = self.finish_attempt()?;
                    if status.success() {
                        self.finished = true;
                        self.permit.take();
                        return Ok(0);
                    }
                    self.last_failure = if stderr.is_empty() {
                        format!("curl exited with {status}")
                    } else {
                        format!("curl exited with {status}: {stderr}")
                    };
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.transport_errors = stats.transport_errors.saturating_add(1);
                    }
                    self.restart()?;
                }
                Err(error) => {
                    self.last_failure = format!("stdout read error: {error}");
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.transport_errors = stats.transport_errors.saturating_add(1);
                    }
                    self.restart()?;
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
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let seconds = (date.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    Some(Duration::from_secs(seconds.max(0) as u64))
}

#[cfg(target_os = "macos")]
fn fetch_with_curl(part: &Part) -> Result<VerifyingReader<Box<dyn Read + Send>>> {
    let stats = Arc::new(Mutex::new(FetchStats {
        attempts: 1,
        ..FetchStats::default()
    }));
    let reader = CurlReader::new(part.url.clone(), curl_user_agent(), Arc::clone(&stats))?;
    let (hasher, expected) = match (&part.sha256, &part.sha1, &part.md5) {
        (Some(h), _, _) => (Some(Hasher::Sha256(Sha256::new())), h.to_lowercase()),
        (None, Some(h), _) => (Some(Hasher::Sha1(Sha1::new())), h.to_lowercase()),
        (None, None, Some(h)) => (Some(Hasher::Md5(Md5::new())), h.to_lowercase()),
        (None, None, None) => (None, String::new()),
    };
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
}
