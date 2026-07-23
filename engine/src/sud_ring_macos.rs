//! Compile-time Darwin boundary for the Linux-only SUD/direct-FUSE ring.
//!
//! A macOS host supports the QEMU backend. The shared ring is consumed by the
//! Linux SUD and namespace/FUSE launchers and must never be selected natively;
//! these types retain the common control/runner shape and fail explicitly if a
//! non-QEMU backend reaches them.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

pub(crate) const RING_FD: RawFd = 1021;
pub(crate) const FD_LANE_FD: RawFd = 1020;
pub(crate) const SLOT_DATA: usize = 32 * 1024;

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the SUD/direct-FUSE transport is Linux-only; use --qemu on macOS",
    )
}

pub(crate) struct RingMapping {
    fd: Option<OwnedFd>,
}

impl RingMapping {
    pub(crate) fn create() -> io::Result<Self> {
        Err(unsupported())
    }

    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Ok(Self { fd: Some(fd) })
    }

    pub(crate) fn duplicate_fd(&self) -> io::Result<OwnedFd> {
        let raw = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    }

    pub(crate) fn discard_descriptor(&mut self) {
        self.fd.take();
    }
}

impl AsRawFd for RingMapping {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1)
    }
}

pub(crate) struct RingClient;

impl RingClient {
    pub(crate) fn new(_mapping: Arc<RingMapping>) -> Self {
        Self
    }

    pub(crate) fn request(&self, _request: &[u8]) -> io::Result<Vec<u8>> {
        Err(unsupported())
    }
}

pub(crate) struct SudFsSession;

impl SudFsSession {
    pub(crate) fn start(
        _filesystem: crate::sarunfs::SarunFs,
        _fd: OwnedFd,
        _lane_fd: OwnedFd,
        _worker_count: usize,
    ) -> io::Result<Self> {
        Err(unsupported())
    }

    pub(crate) fn stop(self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct DirectFsSession;

impl DirectFsSession {
    pub(crate) fn start(
        _filesystem: crate::sarunfs::SarunFs,
        _fd: OwnedFd,
        _lane_fd: OwnedFd,
        _worker_count: usize,
    ) -> io::Result<Self> {
        Err(unsupported())
    }

    pub(crate) fn stop(self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn identify_direct_caller(_lane_fd: OwnedFd) -> io::Result<()> {
    Err(unsupported())
}
