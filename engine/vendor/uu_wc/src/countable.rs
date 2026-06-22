// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.
//! Traits and implementations for iterating over lines in a file-like object.
//!
//! This module provides a [`WordCountable`] trait and implementations
//! for some common file-like objects. Use the [`WordCountable::buffered`]
//! method to get an iterator over lines of a file-like object.
use std::fs::File;
use std::io::{BufRead, BufReader, Read, StdinLock};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

#[cfg(unix)]
pub trait WordCountable: AsFd + AsRawFd + Read {
    type Buffered: BufRead;
    fn buffered(self) -> Self::Buffered;
    fn inner_file(&mut self) -> Option<&mut File>;
}

#[cfg(all(not(unix), not(target_os = "wasi")))]
pub trait WordCountable: Read {
    type Buffered: BufRead;
    fn buffered(self) -> Self::Buffered;
    fn inner_file(&mut self) -> Option<&mut File>;
}

#[cfg(target_os = "wasi")]
pub trait WordCountable: Read {
    type Buffered: BufRead;
    fn buffered(self) -> Self::Buffered;
}

#[cfg(not(target_os = "wasi"))]
impl WordCountable for StdinLock<'_> {
    type Buffered = Self;

    fn buffered(self) -> Self::Buffered {
        self
    }
    fn inner_file(&mut self) -> Option<&mut File> {
        None
    }
}

#[cfg(target_os = "wasi")]
impl WordCountable for StdinLock<'_> {
    type Buffered = Self;

    fn buffered(self) -> Self::Buffered {
        self
    }
}

#[cfg(not(target_os = "wasi"))]
impl WordCountable for File {
    type Buffered = BufReader<Self>;

    fn buffered(self) -> Self::Buffered {
        BufReader::new(self)
    }

    fn inner_file(&mut self) -> Option<&mut File> {
        Some(self)
    }
}

#[cfg(target_os = "wasi")]
impl WordCountable for File {
    type Buffered = BufReader<Self>;

    fn buffered(self) -> Self::Buffered {
        BufReader::new(self)
    }
}

/// The shell's logical standard input, as handed to the in-process `wc`
/// builtin: an arbitrary [`Read`] sink (`reader`) plus the raw descriptor
/// backing it (`in_fd`) when one exists.
///
/// Upstream `wc` reads the process's real stdin (`io::stdin().lock()`). The
/// logical entry point [`crate::wc`] never touches process-global stdio; it
/// drives counting from this handle instead. `in_fd` is `None` for an
/// in-memory stream with no descriptor — in which case the `-c` `fstat`/splice
/// fast paths in [`crate::count_fast`] are skipped (they `fstat` an invalid fd,
/// which fails, and fall through to the plain read loop, so byte counts are
/// still correct).
#[cfg(unix)]
pub struct LogicalStdin<'a> {
    reader: &'a mut dyn Read,
    in_fd: Option<RawFd>,
}

#[cfg(unix)]
impl<'a> LogicalStdin<'a> {
    pub fn new(reader: &'a mut dyn Read, in_fd: Option<RawFd>) -> Self {
        Self { reader, in_fd }
    }
}

#[cfg(unix)]
impl Read for LogicalStdin<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

#[cfg(unix)]
impl AsRawFd for LogicalStdin<'_> {
    fn as_raw_fd(&self) -> RawFd {
        // -1 when there is no backing descriptor: `fstat(-1)` fails, so the
        // byte-count fast path in `count_fast` falls back to the read loop.
        self.in_fd.unwrap_or(-1)
    }
}

#[cfg(unix)]
impl AsFd for LogicalStdin<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: when `in_fd` is `Some`, it is the descriptor of the OpenFile
        // that backs `reader`, which outlives this handle. When `None`, we
        // borrow the invalid fd -1; the only consumer (`rustix::fs::fstat`)
        // treats it as an error and falls through to the read loop.
        unsafe { BorrowedFd::borrow_raw(self.in_fd.unwrap_or(-1)) }
    }
}

#[cfg(unix)]
impl WordCountable for LogicalStdin<'_> {
    type Buffered = BufReader<Self>;

    fn buffered(self) -> Self::Buffered {
        BufReader::new(self)
    }

    fn inner_file(&mut self) -> Option<&mut File> {
        // Not a real `File`, so the seek-based `-c` optimization is skipped;
        // the read loop in `count_bytes_fast` handles it correctly.
        None
    }
}
