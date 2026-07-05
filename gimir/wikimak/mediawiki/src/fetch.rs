//! Streaming HTTP fetch with on-EOF checksum verification.
//!
//! Per SPEC §API: the returned reader verifies the part's checksum on
//! EOF. Calling `into_inner()` or dropping mid-stream skips the check.
//! `sha256` takes precedence; if `None`, `sha1` is used.

use std::io::{self, Read};

use reqwest::blocking::Client;
use sha1::Sha1;
use sha2::{Digest as _, Sha256};

use crate::types::{Error, Part, Result};

/// Which digest the verifier is computing. `None` means no checksum was
/// advertised on the Part; reads pass through verbatim and EOF is silent.
enum Hasher {
    Sha256(Sha256),
    Sha1(Sha1),
}

impl Hasher {
    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => sha2::Digest::update(h, data),
            Hasher::Sha1(h) => sha1::Digest::update(h, data),
        }
    }
    fn finalize_hex(self) -> String {
        match self {
            Hasher::Sha256(h) => hex::encode(h.finalize()),
            Hasher::Sha1(h) => hex::encode(h.finalize()),
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
    let resp = client.get(&part.url).send()?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            url: part.url.clone(),
        });
    }
    let (hasher, expected) = match (&part.sha256, &part.sha1) {
        (Some(h), _) => (Some(Hasher::Sha256(Sha256::new())), h.to_lowercase()),
        (None, Some(h)) => (Some(Hasher::Sha1(Sha1::new())), h.to_lowercase()),
        (None, None) => (None, String::new()),
    };
    Ok(VerifyingReader {
        inner: Box::new(resp) as Box<dyn Read + Send>,
        hasher,
        expected,
        filename: part.filename.clone(),
        finalized: false,
    })
}
