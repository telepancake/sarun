//! Thin Linux `/dev/fuse` transport for the shared virtiofsd server.
//!
//! This module owns only request/reply byte movement and raw-device workers.
//! Mount namespace ownership and teardown live in [`crate::fuse_broker`].
//! Opcode decoding and every filesystem semantic operation are handled by
//! [`virtiofsd::server::Server`] and its `FileSystem` implementation.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use virtiofsd::filesystem::FileSystem;
use virtiofsd::server::{FuseBacking, Server};

const MAX_REQUEST_SIZE: usize = (1 << 20) + 0x1000;
nix::ioctl_read!(fuse_dev_ioc_clone, 229, 0, u32);

#[repr(C)]
struct FuseBackingMap {
    fd: u32,
    flags: u32,
    padding: u64,
}

nix::ioctl_write_ptr!(fuse_dev_ioc_backing_open, 229, 1, FuseBackingMap);
nix::ioctl_write_ptr!(fuse_dev_ioc_backing_close, 229, 2, u32);

struct BackingRegistry {
    device: Arc<File>,
    by_handle: Mutex<std::collections::HashMap<u64, Vec<u32>>>,
    unavailable: AtomicBool,
    successes: AtomicU64,
    failures: AtomicU64,
}

impl BackingRegistry {
    fn close_ids(&self, ids: impl IntoIterator<Item = u32>) {
        for id in ids {
            // SAFETY: each id was returned by BACKING_OPEN on this connection.
            if let Err(error) = unsafe { fuse_dev_ioc_backing_close(self.device.as_raw_fd(), &id) }
            {
                eprintln!("sarun-engine: cannot close FUSE backing id {id}: {error}");
            }
        }
    }
}

impl FuseBacking for BackingRegistry {
    fn register(&self, handle: u64, file: &File) -> io::Result<u32> {
        if self.unavailable.load(Ordering::Relaxed) {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let map = FuseBackingMap {
            fd: file.as_raw_fd() as u32,
            flags: 0,
            padding: 0,
        };
        // SAFETY: both descriptors are live and map has the kernel ABI layout.
        let id = match unsafe { fuse_dev_ioc_backing_open(self.device.as_raw_fd(), &map) } {
            Ok(id) => id as u32,
            Err(error) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                if error == nix::errno::Errno::EPERM {
                    self.unavailable.store(true, Ordering::Relaxed);
                } else {
                    eprintln!("sarun-engine: cannot register FUSE backing file: {error}");
                }
                return Err(error.into());
            }
        };
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.by_handle
            .lock()
            .unwrap()
            .entry(handle)
            .or_default()
            .push(id);
        Ok(id)
    }

    fn release(&self, handle: u64) {
        if let Some(ids) = self.by_handle.lock().unwrap().remove(&handle) {
            self.close_ids(ids);
        }
    }
}

impl Drop for BackingRegistry {
    fn drop(&mut self) {
        let ids = self
            .by_handle
            .get_mut()
            .unwrap()
            .drain()
            .flat_map(|(_, ids)| ids)
            .collect::<Vec<_>>();
        self.close_ids(ids);
    }
}

/// Raw-FUSE request workers for a connection whose mount is broker-owned.
pub struct FuseSession {
    device: Option<Arc<File>>,
    stop: Arc<File>,
    backing: Option<Arc<BackingRegistry>>,
    workers: Vec<JoinHandle<io::Result<()>>>,
}

impl FuseSession {
    /// Serve a FUSE connection mounted by the private mount-owner broker.
    /// Teardown of the mount itself belongs to that broker; this object only
    /// owns the raw request fds and workers.
    pub(crate) fn serve_mounted<F>(filesystem: F, device: File, workers: usize) -> io::Result<Self>
    where
        F: FileSystem + Send + Sync + 'static,
    {
        if workers == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a FUSE session needs at least one worker",
            ));
        }
        let device = Arc::new(device);
        let stop = Arc::new(stop_event()?);
        let backing = Arc::new(BackingRegistry {
            device: device.clone(),
            by_handle: Mutex::new(std::collections::HashMap::new()),
            unavailable: AtomicBool::new(false),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        });
        let server = Arc::new(Server::new(filesystem));
        let mut session = Self {
            device: Some(device.clone()),
            stop: stop.clone(),
            backing: Some(backing.clone()),
            workers: Vec::with_capacity(workers),
        };

        for index in 0..workers {
            let worker_device = if index == 0 {
                device.clone()
            } else {
                Arc::new(clone_device(&device)?)
            };
            let server = server.clone();
            let backing = backing.clone();
            let stop = stop.clone();
            match thread::Builder::new()
                .name(format!("sarun-fuse-{index}"))
                .spawn(move || {
                    let result = serve_device(worker_device, stop, server, backing);
                    if let Err(error) = &result {
                        eprintln!("sarun-engine: raw FUSE worker failed: {error}");
                    }
                    result
                }) {
                Ok(worker) => session.workers.push(worker),
                Err(error) => return Err(error),
            }
        }
        Ok(session)
    }

    #[cfg(test)]
    pub(crate) fn backing_results(&self) -> (u64, u64) {
        let backing = self.backing.as_ref().unwrap();
        (
            backing.successes.load(Ordering::Relaxed),
            backing.failures.load(Ordering::Relaxed),
        )
    }

    fn detach(&mut self) {
        if self.device.is_none() {
            return;
        }
        let wake = 1u64.to_ne_bytes();
        unsafe {
            libc::write(self.stop.as_raw_fd(), wake.as_ptr().cast(), wake.len());
        }
        self.device.take();
    }

    pub fn unmount(mut self) -> io::Result<()> {
        self.detach();
        let result = join_workers(&mut self.workers);
        self.backing.take();
        result
    }
}

impl Drop for FuseSession {
    fn drop(&mut self) {
        self.detach();
        let _ = join_workers(&mut self.workers);
        self.backing.take();
    }
}

fn join_workers(workers: &mut Vec<JoinHandle<io::Result<()>>>) -> io::Result<()> {
    let mut result = Ok(());
    for worker in workers.drain(..) {
        let joined = worker
            .join()
            .map_err(|_| io::Error::other("raw FUSE worker panicked"))
            .and_then(|value| value);
        if result.is_ok() && joined.is_err() {
            result = joined;
        }
    }
    result
}

fn clone_device(source: &File) -> io::Result<File> {
    let clone = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")?;
    let mut source_fd = source.as_raw_fd() as u32;
    // SAFETY: `clone` is a fresh /dev/fuse fd and source_fd names the mounted
    // connection returned by fusermount.
    unsafe { fuse_dev_ioc_clone(clone.as_raw_fd(), &mut source_fd)? };
    Ok(clone)
}

fn stop_event() -> io::Result<File> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: eventfd returned a fresh owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn serve_device<F>(
    device: Arc<File>,
    stop: Arc<File>,
    server: Arc<Server<F>>,
    backing: Arc<BackingRegistry>,
) -> io::Result<()>
where
    F: FileSystem + Send + Sync + 'static,
{
    set_nonblocking(&device)?;
    let mut request = vec![0u8; MAX_REQUEST_SIZE];
    let mut response = vec![0u8; MAX_REQUEST_SIZE];
    loop {
        let count = loop {
            let mut descriptors = [
                libc::pollfd {
                    fd: device.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if descriptors[1].revents != 0 {
                return Ok(());
            }
            match nix::unistd::read(device.as_fd(), &mut request) {
                Ok(count) => break count,
                Err(
                    nix::errno::Errno::ENOENT
                    | nix::errno::Errno::EINTR
                    | nix::errno::Errno::EAGAIN,
                ) => {}
                Err(nix::errno::Errno::ENODEV) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        };
        if count == 0 {
            return Ok(());
        }
        let reply_count = match server.handle_fuse_message_with_backing(
            &request[..count],
            &mut response,
            &*backing,
        ) {
            Ok(reply_count) => reply_count,
            Err(error) => crate::sarunfs::malformed_fuse_reply(&request[..count], &mut response)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?,
        };
        if reply_count == 0 {
            continue;
        }
        match nix::unistd::write(device.as_fd(), &response[..reply_count]) {
            Ok(written) if written == reply_count => {}
            Ok(written) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short /dev/fuse reply: {written}/{reply_count}"),
                ));
            }
            Err(nix::errno::Errno::ENOENT) => {}
            Err(nix::errno::Errno::ENODEV) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::io::Write;
    use std::mem::size_of;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use virtiofsd::filesystem::{
        Context, DirEntry, DirectoryIterator, Entry, OpenOptions, ROOT_ID, ZeroCopyWriter,
    };
    use virtiofsd::fuse::{Attr, InHeader, OutHeader};

    struct EmptyDirectory;

    impl DirectoryIterator for EmptyDirectory {
        fn next(&mut self) -> Option<DirEntry<'_>> {
            None
        }
    }

    struct StaticFile {
        backing: File,
        daemon_reads: Arc<AtomicUsize>,
    }

    impl FileSystem for StaticFile {
        type Inode = u64;
        type Handle = u64;
        type DirIter = EmptyDirectory;

        fn init(
            &self,
            capable: virtiofsd::filesystem::FsOptions,
        ) -> io::Result<virtiofsd::filesystem::FsOptions> {
            Ok(capable & virtiofsd::filesystem::FsOptions::PASSTHROUGH)
        }

        fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
            if parent != ROOT_ID || name.to_bytes() != b"hello" {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            Ok(Entry {
                inode: 2,
                generation: 0,
                attr: file_attr(),
                attr_timeout: Duration::ZERO,
                entry_timeout: Duration::ZERO,
            })
        }

        fn getattr(
            &self,
            _ctx: Context,
            inode: u64,
            _handle: Option<u64>,
        ) -> io::Result<(Attr, Duration)> {
            if inode == ROOT_ID {
                Ok((
                    Attr {
                        ino: ROOT_ID,
                        mode: libc::S_IFDIR | 0o755,
                        nlink: 2,
                        ..Default::default()
                    },
                    Duration::ZERO,
                ))
            } else if inode == 2 {
                Ok((file_attr(), Duration::ZERO))
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            }
        }

        fn open(
            &self,
            _ctx: Context,
            inode: u64,
            _kill_priv: bool,
            _flags: u32,
        ) -> io::Result<(Option<u64>, OpenOptions)> {
            if inode == 2 {
                Ok((Some(7), OpenOptions::PASSTHROUGH))
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            }
        }

        fn backing_file(&self, handle: u64) -> io::Result<Option<File>> {
            if handle == 7 {
                self.backing.try_clone().map(Some)
            } else {
                Ok(None)
            }
        }

        fn read<W: ZeroCopyWriter>(
            &self,
            _ctx: Context,
            inode: u64,
            handle: u64,
            mut writer: W,
            size: u32,
            offset: u64,
            _lock_owner: Option<u64>,
            _flags: u32,
        ) -> io::Result<usize> {
            if inode != 2 || handle != 7 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            self.daemon_reads.fetch_add(1, Ordering::Relaxed);
            let contents = b"shared decoder\n";
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(contents.len());
            let end = start.saturating_add(size as usize).min(contents.len());
            writer.read_from_file_at(&self.backing, end - start, start as u64, None)
        }
    }

    fn file_attr() -> Attr {
        Attr {
            ino: 2,
            size: b"shared decoder\n".len() as u64,
            blocks: 1,
            mode: libc::S_IFREG | 0o444,
            nlink: 1,
            ..Default::default()
        }
    }

    #[test]
    fn kernel_mount_reads_through_the_shared_decoder() {
        if !Path::new("/dev/fuse").exists() {
            eprintln!("SKIP: /dev/fuse is unavailable");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        let old_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        let old_namespace = std::env::var_os("SLOPBOX_NS");
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &runtime);
            std::env::set_var("SLOPBOX_NS", "FUSETRANSPORTTEST");
        }
        crate::paths::ensure_dirs().unwrap();
        let mountpoint = crate::paths::mnt_point();
        let daemon_reads = Arc::new(AtomicUsize::new(0));
        let mut backing = tempfile::tempfile().unwrap();
        backing.write_all(b"shared decoder\n").unwrap();
        let session = crate::fuse_broker::BrokeredFuseSession::mount(
            StaticFile {
                backing,
                daemon_reads: daemon_reads.clone(),
            },
            &mountpoint,
            2,
        )
        .unwrap();
        assert!(
            !std::fs::read_to_string("/proc/self/mountinfo")
                .unwrap()
                .contains(mountpoint.to_string_lossy().as_ref())
        );
        let read = std::process::Command::new("nsenter")
            .args(["-t", &session.broker_pid().to_string(), "-U", "-m"])
            .arg("--preserve-credentials")
            .args(["--", "cat"])
            .arg(mountpoint.join("hello"))
            .output()
            .unwrap();
        assert!(
            read.status.success(),
            "{}",
            String::from_utf8_lossy(&read.stderr)
        );
        assert_eq!(read.stdout, b"shared decoder\n");
        let (backing_successes, backing_failures) = session.backing_results();
        assert!(
            (backing_successes == 1 && daemon_reads.load(Ordering::Relaxed) == 0)
                || (backing_failures == 1 && daemon_reads.load(Ordering::Relaxed) == 1)
        );
        session.unmount().unwrap();
        unsafe {
            match old_runtime {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match old_namespace {
                Some(value) => std::env::set_var("SLOPBOX_NS", value),
                None => std::env::remove_var("SLOPBOX_NS"),
            }
        }
    }

    #[test]
    fn malformed_request_keeps_its_unique_id() {
        let input = InHeader {
            unique: 98,
            ..Default::default()
        };
        let mut response = [0; 64];
        let input_bytes = unsafe {
            std::slice::from_raw_parts(
                (&input as *const InHeader).cast::<u8>(),
                size_of::<InHeader>(),
            )
        };
        let count = crate::sarunfs::malformed_fuse_reply(input_bytes, &mut response).unwrap();
        assert_eq!(count, size_of::<OutHeader>());
        let output = unsafe { std::ptr::read_unaligned(response.as_ptr().cast::<OutHeader>()) };
        assert_eq!((output.unique, output.error), (98, -libc::EIO));
    }
}
