//! Vhost-user transport for the shared [`SarunFs`](crate::sarunfs::SarunFs).
//!
//! This module deliberately knows no overlay rules.  It scopes protocol inode
//! 1 to a registered box, owns the ephemeral socket and runs virtiofsd's
//! transport loop.  FUSE, SUD and QEMU therefore cannot acquire divergent
//! path-resolution or capture semantics here.

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TAG: &str = "sarun-root";
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// One QEMU-facing virtio-fs server.  The server accepts one vhost-user
/// frontend and exits when that frontend disconnects.
#[derive(Debug)]
pub struct BoxExport {
    socket: PathBuf,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl BoxExport {
    /// Start serving `box_id` at the canonical per-box socket.
    pub fn start(fs: &crate::sarunfs::SarunFs, box_id: i64) -> io::Result<Self> {
        Self::start_at(fs, box_id, crate::paths::virtiofs_socket(box_id))
    }

    fn start_at(fs: &crate::sarunfs::SarunFs, box_id: i64, socket: PathBuf) -> io::Result<Self> {
        let scoped = fs.export_box(box_id)?;
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let socket_text = socket
            .to_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "virtio-fs socket is not UTF-8")
            })?
            .to_owned();
        let thread = std::thread::Builder::new()
            .name(format!("virtiofs-box-{box_id}"))
            .spawn(move || {
                virtiofsd::vhost_user::run_vhost_user_fs(
                    scoped,
                    &socket_text,
                    Some(TAG.to_owned()),
                    0,
                )
            })?;
        let export = Self {
            socket,
            thread: Some(thread),
        };
        export.wait_ready(READY_TIMEOUT)?;
        Ok(export)
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Wait for the frontend to disconnect and remove the stale socket name.
    pub fn wait(mut self) -> io::Result<()> {
        let result = self
            .thread
            .take()
            .expect("export thread present")
            .join()
            .map_err(|_| io::Error::other("virtio-fs transport thread panicked"))?;
        let _ = std::fs::remove_file(&self.socket);
        result
    }

    /// Stop a listener whose frontend never connected, or join one whose
    /// frontend has already disconnected.  Connecting and immediately closing
    /// is the vhost-user equivalent of cancelling accept; negotiation failure
    /// is expected on that forced path and is therefore not surfaced.
    pub fn stop(mut self) -> io::Result<()> {
        let thread = self.thread.take().expect("export thread present");
        let forced = !thread.is_finished();
        if forced {
            let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        }
        let result = thread
            .join()
            .map_err(|_| io::Error::other("virtio-fs transport thread panicked"))?;
        let _ = std::fs::remove_file(&self.socket);
        if forced {
            Ok(())
        } else {
            match result {
                // QEMU closes the vhost-user socket after the guest powers
                // down. virtiofsd reports that ordinary lifecycle EOF through
                // its generic request-error wrapper instead of `Ok(())`.
                Err(error) if error.to_string().contains("peer disconnected") => Ok(()),
                other => other,
            }
        }
    }

    fn wait_ready(&self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match std::fs::symlink_metadata(&self.socket) {
                Ok(meta) if meta.file_type().is_socket() => {
                    std::fs::set_permissions(&self.socket, std::fs::Permissions::from_mode(0o600))?;
                    return Ok(());
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} is not a socket", self.socket.display()),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                return Err(io::Error::other(
                    "virtio-fs transport exited before binding",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "virtio-fs socket did not appear at {}",
                        self.socket.display()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::BoxState;
    use std::sync::Arc;

    #[test]
    fn export_scopes_root_and_binds_private_socket() {
        let temp = tempfile::tempdir().unwrap();
        let fs = crate::sarunfs::SarunFs::new(temp.path().to_owned());
        let box_id = 9_000_000_071_i64;
        fs.add_box(Arc::new(BoxState::create(box_id).unwrap()));
        let socket = temp.path().join("vhost.sock");
        let export = BoxExport::start_at(&fs, box_id, socket.clone()).unwrap();
        let meta = std::fs::symlink_metadata(export.socket()).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        // A frontend that disappears during negotiation is still a completed
        // transport lifetime; the server must return instead of leaking a
        // listener thread.  A real QEMU connection follows the same teardown.
        drop(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let _ = export.wait();
        assert!(!socket.exists());
    }

    #[test]
    fn missing_box_never_creates_a_listener() {
        let temp = tempfile::tempdir().unwrap();
        let fs = crate::sarunfs::SarunFs::new(temp.path().to_owned());
        let socket = temp.path().join("missing.sock");
        let error = BoxExport::start_at(&fs, 404, socket.clone()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
        assert!(!socket.exists());
    }

    #[test]
    fn stop_cancels_a_listener_before_qemu_connects() {
        let temp = tempfile::tempdir().unwrap();
        let fs = crate::sarunfs::SarunFs::new(temp.path().to_owned());
        let box_id = 9_000_000_072_i64;
        fs.add_box(Arc::new(BoxState::create(box_id).unwrap()));
        let socket = temp.path().join("cancel.sock");
        let export = BoxExport::start_at(&fs, box_id, socket.clone()).unwrap();
        export.stop().unwrap();
        assert!(!socket.exists());
    }
}
