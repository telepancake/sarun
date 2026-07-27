//! Streaming HTTP fetch with on-EOF checksum verification.
//!
//! Per SPEC §API: the returned reader verifies the part's checksum on
//! EOF. Calling `into_inner()` or dropping mid-stream skips the check.
//! `sha256` takes precedence; if `None`, `sha1` is used.

use std::io::{self, Read};
#[cfg(target_os = "macos")]
use std::process::{Child, ChildStdout, Command, Stdio};

use reqwest::blocking::Client;
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest as _, Sha256};

use crate::types::{Error, Part, Result};

/// Which digest the verifier is computing. `None` means no checksum was
/// advertised on the Part; reads pass through verbatim and EOF is silent.
enum Hasher {
    Sha256(Sha256),
    Sha1(Sha1),
    Md5(Md5),
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

/// A `Read` wrapper that tracks the running hash and surfaces a
/// `ChecksumMismatch` error from `read` when the underlying reader hits
/// EOF if the digest does not match the part's advertised checksum.
///
/// Partial reads followed by `into_inner()` or drop skip the check.
pub struct VerifyingReader<R: Read> {
    pub(crate) inner: R,
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

/// Fetch a Part: GET the URL, return a streaming reader.
pub fn fetch(client: &Client, part: &Part) -> Result<VerifyingReader<Box<dyn Read + Send>>> {
    #[cfg(target_os = "macos")]
    if part.url.starts_with("https://dumps.wikimedia.org/") {
        return fetch_with_curl(part);
    }

    let mut attempt = 0u32;
    let resp = loop {
        match client.get(&part.url).send() {
            Ok(resp) if resp.status().is_success() => break resp,
            Ok(resp)
                if attempt < 3
                    && (resp.status().as_u16() == 429 || resp.status().is_server_error()) =>
            {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok());
                let delay = retry_after
                    .unwrap_or(2u64.saturating_pow(attempt + 1))
                    .min(120);
                std::thread::sleep(std::time::Duration::from_secs(delay));
                attempt += 1;
            }
            Ok(resp) => {
                return Err(Error::HttpStatus {
                    status: resp.status().as_u16(),
                    url: part.url.clone(),
                });
            }
            Err(error) if attempt < 3 && (error.is_connect() || error.is_timeout()) => {
                std::thread::sleep(std::time::Duration::from_secs(
                    2u64.saturating_pow(attempt + 1),
                ));
                attempt += 1;
            }
            Err(error) => return Err(error.into()),
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
}

#[cfg(target_os = "macos")]
const MAX_CURL_RESUMPTIONS: u32 = 16;

#[cfg(target_os = "macos")]
struct CurlReader {
    url: String,
    user_agent: String,
    attempt: CurlAttempt,
    headers_ready: bool,
    buffered: Vec<u8>,
    buffered_at: usize,
    offset: u64,
    retries: u32,
    finished: bool,
}

#[cfg(target_os = "macos")]
impl CurlReader {
    fn new(url: String, user_agent: String) -> io::Result<Self> {
        let attempt = Self::spawn(&url, &user_agent, 0)?;
        Ok(Self {
            url,
            user_agent,
            attempt,
            headers_ready: false,
            buffered: Vec::new(),
            buffered_at: 0,
            offset: 0,
            retries: 0,
            finished: false,
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
            "--user-agent",
            user_agent,
        ]);
        if offset != 0 {
            command.args(["--range", &format!("{offset}-")]);
        }
        let mut child = command
            .args(["--url", url])
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("curl stdout unavailable"))?;
        Ok(CurlAttempt { child, stdout })
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
                let n = self.attempt.stdout.read(&mut chunk)?;
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
            if status < 200
                || (300..400).contains(&status)
                || reason.eq_ignore_ascii_case("connection established")
            {
                continue;
            }
            if self.offset == 0 && (status == 200 || status == 206) {
                self.headers_ready = true;
                continue;
            }
            if self.offset != 0 && status == 206 {
                let start = header_value(&header, b"content-range:")
                    .and_then(|value| value.strip_prefix("bytes "))
                    .and_then(|value| value.split('-').next())
                    .and_then(|value| value.parse::<u64>().ok());
                if start == Some(self.offset) {
                    self.headers_ready = true;
                    continue;
                }
            }
            return Err(io::Error::other(format!(
                "curl returned HTTP {status} while fetching {} at byte {}",
                self.url, self.offset
            )));
        }
        Ok(())
    }

    fn restart(&mut self) -> io::Result<()> {
        if self.retries >= MAX_CURL_RESUMPTIONS {
            return Err(io::Error::other(format!(
                "curl transfer failed after {} resumptions",
                self.retries
            )));
        }
        let _ = self.attempt.child.kill();
        let _ = self.attempt.child.wait();
        let delay = 2u64
            .saturating_pow(self.retries.saturating_add(1))
            .min(30);
        std::thread::sleep(std::time::Duration::from_secs(delay));
        self.retries += 1;
        self.attempt = Self::spawn(&self.url, &self.user_agent, self.offset)?;
        self.headers_ready = false;
        self.buffered.clear();
        self.buffered_at = 0;
        Ok(())
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
                if self.retries >= MAX_CURL_RESUMPTIONS {
                    return Err(error);
                }
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
                return Ok(n);
            }
            match self.attempt.stdout.read(out) {
                Ok(n) if n != 0 => {
                    self.offset += n as u64;
                    return Ok(n);
                }
                Ok(_) => {
                    let status = self.attempt.child.wait()?;
                    if status.success() {
                        self.finished = true;
                        return Ok(0);
                    }
                    self.restart()?;
                }
                Err(error) => {
                    if self.retries >= MAX_CURL_RESUMPTIONS {
                        return Err(error);
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
fn fetch_with_curl(part: &Part) -> Result<VerifyingReader<Box<dyn Read + Send>>> {
    let reader = CurlReader::new(part.url.clone(), curl_user_agent())?;
    let (hasher, expected) = match (&part.sha256, &part.sha1, &part.md5) {
        (Some(h), _, _) => (Some(Hasher::Sha256(Sha256::new())), h.to_lowercase()),
        (None, Some(h), _) => (Some(Hasher::Sha1(Sha1::new())), h.to_lowercase()),
        (None, None, Some(h)) => (Some(Hasher::Md5(Md5::new())), h.to_lowercase()),
        (None, None, None) => (None, String::new()),
    };
    Ok(VerifyingReader {
        inner: Box::new(reader),
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
        let mut reader =
            CurlReader::new(format!("http://{address}/part"), "sarun-test".to_string()).unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        server.join().unwrap();
        assert_eq!(body, b"abcdefghij");
    }
}
