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
struct CurlReader {
    child: Child,
    stdout: ChildStdout,
    finished: bool,
}

#[cfg(target_os = "macos")]
impl Read for CurlReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n != 0 || self.finished {
            return Ok(n);
        }
        self.finished = true;
        let status = self.child.wait()?;
        if status.success() {
            Ok(0)
        } else {
            Err(io::Error::other(format!(
                "curl download failed with {status}"
            )))
        }
    }
}

#[cfg(target_os = "macos")]
fn fetch_with_curl(part: &Part) -> Result<VerifyingReader<Box<dyn Read + Send>>> {
    let user_agent = curl_user_agent();
    let mut child = Command::new("/usr/bin/curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "30",
            "--max-time",
            "86400",
            "--user-agent",
            &user_agent,
            "--url",
            &part.url,
        ])
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("curl stdout unavailable"))?;
    let (hasher, expected) = match (&part.sha256, &part.sha1, &part.md5) {
        (Some(h), _, _) => (Some(Hasher::Sha256(Sha256::new())), h.to_lowercase()),
        (None, Some(h), _) => (Some(Hasher::Sha1(Sha1::new())), h.to_lowercase()),
        (None, None, Some(h)) => (Some(Hasher::Md5(Md5::new())), h.to_lowercase()),
        (None, None, None) => (None, String::new()),
    };
    Ok(VerifyingReader {
        inner: Box::new(CurlReader {
            child,
            stdout,
            finished: false,
        }),
        hasher,
        expected,
        filename: part.filename.clone(),
        finalized: false,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn curl_user_agent() -> String {
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
