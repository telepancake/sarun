//! Private mount-owner namespace for the FUSE execution backend.
//!
//! The engine serves the raw FUSE connection, but never retains the mount in
//! its host mount namespace.  `fusermount3` creates the superblock there so
//! every canonical uid/gid remains representable.  A small, single-threaded
//! child immediately clones that mount into an identity-mapped user namespace
//! plus its private mount namespace; a normal (propagating) unmount removes
//! every outer startup copy before FUSE workers or the accept loop start.  A
//! top-level FUSE runner joins those namespaces before any worker thread
//! exists; bubblewrap consequently sees the private root without an
//! outside-in mount operation.

use std::fs::File;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use nix::sys::socket::{
    ControlMessage, ControlMessageOwned, MsgFlags, SockaddrStorage, recvmsg, sendmsg,
};
use virtiofsd::filesystem::FileSystem;

const CHILD_COMMAND: &str = "fuse-mount-broker";
const CONTROL_FD_ENV: &str = "SARUN_FUSE_BROKER_CONTROL_FD";
const MOUNTPOINT_ENV: &str = "SARUN_FUSE_BROKER_MOUNTPOINT";
const FUSERMOUNT_COMM_FD: &str = "_FUSE_COMMFD";
const RUNNER_ENTERED_ENV: &str = "SARUN_FUSE_NAMESPACE_ENTERED";
const REQUEST_NAMESPACES: u8 = b'N';
const REQUEST_SHUTDOWN: u8 = b'Q';
const REPLY_READY: u8 = b'R';
const REPLY_DONE: u8 = b'D';
const REPLY_ERROR: u8 = b'E';

/// The complete private FUSE lifetime.  Field teardown is deliberately
/// explicit: live runners stop, the broker releases its mount namespace, and
/// then the engine stops and joins the raw-FUSE workers.
pub struct BrokeredFuseSession {
    broker: Option<FuseBroker>,
    fuse: Option<crate::fuse_transport::FuseSession>,
}

impl BrokeredFuseSession {
    pub fn mount<F>(filesystem: F, mountpoint: &Path, workers: usize) -> io::Result<Self>
    where
        F: FileSystem + Send + Sync + 'static,
    {
        let (broker, device) = FuseBroker::start(mountpoint)?;
        let fuse =
            match crate::fuse_transport::FuseSession::serve_mounted(filesystem, device, workers) {
                Ok(fuse) => fuse,
                Err(error) => {
                    let _ = broker.shutdown();
                    return Err(error);
                }
            };
        Ok(Self {
            broker: Some(broker),
            fuse: Some(fuse),
        })
    }

    pub fn unmount(mut self) -> io::Result<()> {
        self.stop()
    }

    #[cfg(test)]
    pub(crate) fn backing_results(&self) -> (u64, u64) {
        self.fuse.as_ref().unwrap().backing_results()
    }

    #[cfg(test)]
    pub(crate) fn broker_pid(&self) -> u32 {
        self.broker.as_ref().unwrap().child.id()
    }

    fn stop(&mut self) -> io::Result<()> {
        let broker_result = self.broker.take().map_or(Ok(()), FuseBroker::shutdown);
        let fuse_result = self.fuse.take().map_or(Ok(()), |fuse| fuse.unmount());
        broker_result.and(fuse_result)
    }
}

impl Drop for BrokeredFuseSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct FuseBroker {
    child: Child,
    control: UnixStream,
    socket_path: PathBuf,
}

impl FuseBroker {
    fn start(mountpoint: &Path) -> io::Result<(Self, File)> {
        let mountpoint = mountpoint.canonicalize()?;
        let device = mount_with_helper(&mountpoint)?;
        let broker = match Self::clone_private_mount(&mountpoint) {
            Ok(broker) => broker,
            Err(error) => {
                let _ = unmount_with_helper(&mountpoint);
                return Err(error);
            }
        };
        if let Err(error) = unmount_with_helper(&mountpoint) {
            let _ = broker.shutdown();
            let _ = unmount_with_helper(&mountpoint);
            return Err(io::Error::other(format!(
                "cannot detach outer FUSE startup mount: {error}"
            )));
        }
        match mountinfo_contains(&mountpoint) {
            Ok(false) => {}
            Ok(true) => {
                let _ = broker.shutdown();
                return Err(io::Error::other(
                    "outer FUSE startup mount remained visible after handoff",
                ));
            }
            Err(error) => {
                let _ = broker.shutdown();
                return Err(error);
            }
        }
        Ok((broker, device))
    }

    fn clone_private_mount(mountpoint: &Path) -> io::Result<Self> {
        let (parent, child_socket) = UnixStream::pair()?;
        let fd = child_socket.as_raw_fd();
        let executable = broker_executable()?;
        let socket_path = crate::paths::fuse_broker_socket();
        let _ = std::fs::remove_file(&socket_path);
        let mut command = Command::new(executable);
        command
            .arg(CHILD_COMMAND)
            .env(CONTROL_FD_ENV, fd.to_string())
            .env(MOUNTPOINT_ENV, mountpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // SAFETY: the closure only changes one descriptor flag between fork
        // and exec.  No allocation or lock-taking occurs there.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(child_socket);
        let (reply, fds) = match receive_fds(&parent, 0) {
            Ok(reply) => reply,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if reply != [REPLY_READY] || !fds.is_empty() {
            let detail = if reply.first() == Some(&REPLY_ERROR) {
                String::from_utf8_lossy(&reply[1..]).trim().to_owned()
            } else {
                format!(
                    "invalid startup reply {:?} with {} descriptors",
                    reply,
                    fds.len()
                )
            };
            drop(fds);
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!("FUSE mount broker: {detail}")));
        }
        Ok(Self {
            child,
            control: parent,
            socket_path,
        })
    }

    fn shutdown(mut self) -> io::Result<()> {
        let request_result = self.control.write_all(&[REQUEST_SHUTDOWN]);
        let mut reply = [0u8; 1];
        let reply_result = request_result.and_then(|_| self.control.read_exact(&mut reply));
        drop(self.control);
        let wait_result = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
        reply_result?;
        let status = wait_result?;
        if reply != [REPLY_DONE] {
            return Err(io::Error::other("FUSE mount broker rejected shutdown"));
        }
        if !status.success() {
            return Err(io::Error::other(format!(
                "FUSE mount broker exited {status}"
            )));
        }
        Ok(())
    }
}

fn broker_executable() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    #[cfg(test)]
    if executable.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("deps")) {
        let product = executable.parent().unwrap().parent().unwrap().join("sarun");
        if product.is_file() {
            return Ok(product);
        }
    }
    Ok(executable)
}

/// Called before Prolog initialization for a top-level `run` whose final
/// backend selection is FUSE.  The process remains the user's foreground
/// runner; only its user and mount namespace membership changes.
pub fn enter_runner_namespace() -> io::Result<()> {
    let mut broker = UnixStream::connect(crate::paths::fuse_broker_socket())?;
    broker.write_all(&[REQUEST_NAMESPACES])?;
    let (reply, fds) = receive_fds(&broker, 2)?;
    if reply != [REPLY_READY] || fds.len() != 2 {
        return Err(io::Error::other("invalid FUSE namespace handoff"));
    }
    let user = &fds[0];
    let mount = &fds[1];
    // Joining the owning user namespace first grants the capability needed to
    // join its mount namespace.  Reversing these calls is the outside-in
    // operation the kernel correctly rejects.
    nix::sched::setns(user.as_fd(), nix::sched::CloneFlags::CLONE_NEWUSER)
        .map_err(io::Error::from)?;
    nix::sched::setns(mount.as_fd(), nix::sched::CloneFlags::CLONE_NEWNS)
        .map_err(io::Error::from)?;
    // A rootless Tap setup can deliberately self-exec after parser startup.
    // Namespace membership survives exec; this marker prevents that second
    // image from trying to setns from a child user namespace back to its
    // ancestor broker namespace.
    unsafe { std::env::set_var(RUNNER_ENTERED_ENV, "1") };
    Ok(())
}

/// Raw-argv decision used before the parser runtime can create threads.
pub fn is_top_level_fuse_run(argv: &[String]) -> bool {
    if argv.first().map(String::as_str) != Some("run")
        || std::env::var("SARUN_BROKER").is_ok_and(|value| !value.is_empty())
        || std::env::var_os("SARUN_ENGINE_PARENT").is_some()
        || std::env::var_os(RUNNER_ENTERED_ENV).is_some()
    {
        return false;
    }
    enum Backend {
        Fuse,
        Sud,
        Qemu,
    }
    let mut backend = Backend::Fuse;
    for argument in &argv[1..] {
        if argument == "--" {
            break;
        }
        match argument.as_str() {
            "--fuse" => backend = Backend::Fuse,
            "--sud" => backend = Backend::Sud,
            "--qemu" => backend = Backend::Qemu,
            _ => {}
        }
    }
    matches!(backend, Backend::Fuse)
}

pub fn is_child_command(argv: &[String]) -> bool {
    argv.first().map(String::as_str) == Some(CHILD_COMMAND)
}

pub fn child_main() -> i32 {
    // The broker shares the engine's inherited process group, but its lifetime
    // is the control socket, not terminal signal delivery.  Let the engine's
    // normal signal path request an ordered namespace release; engine death is
    // observed as control EOF even for SIGKILL.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let fd = match control_fd() {
        Ok(fd) => fd,
        Err(error) => {
            eprintln!("sarun-engine: FUSE mount broker: {error}");
            return 1;
        }
    };
    // SAFETY: this hidden child is the sole owner of the inherited endpoint.
    let mut control = unsafe { UnixStream::from_raw_fd(fd) };
    match child_run(&mut control) {
        Ok(()) => 0,
        Err(error) => {
            let _ =
                control.write_all(&[vec![REPLY_ERROR], error.to_string().into_bytes()].concat());
            eprintln!("sarun-engine: FUSE mount broker: {error}");
            1
        }
    }
}

fn child_run(control: &mut UnixStream) -> io::Result<()> {
    create_private_namespace()?;
    let mountpoint = std::env::var_os(MOUNTPOINT_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing broker mountpoint"))?;
    if !mountinfo_contains(&mountpoint)? {
        return Err(io::Error::other(
            "FUSE mount was not inherited into broker namespace",
        ));
    }
    let socket_path = crate::paths::fuse_broker_socket();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    send_fds(control, &[REPLY_READY], &[])?;

    let result = broker_loop(control, &listener);
    let _ = std::fs::remove_file(&socket_path);
    result
}

fn control_fd() -> io::Result<RawFd> {
    let value = std::env::var(CONTROL_FD_ENV)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "missing broker control fd"))?;
    value
        .parse::<RawFd>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid broker control fd"))
}

fn create_private_namespace() -> io::Result<()> {
    let tasks = std::fs::read_dir("/proc/self/task")?.count();
    if tasks != 1 {
        return Err(io::Error::other(format!(
            "broker started with {tasks} threads"
        )));
    }
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let _ = std::fs::write("/proc/self/setgroups", "deny");
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Do not let mounts propagate back through a shared host root.
    let root = c"/";
    if unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fusermount_binary() -> String {
    if let Some(path) = std::env::var_os("FUSERMOUNT_PATH") {
        return path.to_string_lossy().into_owned();
    }
    if Command::new("fusermount3").arg("-h").output().is_ok() {
        "fusermount3".into()
    } else {
        "fusermount".into()
    }
}

/// Create the superblock while the caller still belongs to the initial user
/// namespace.  This is not merely a privilege workaround: FUSE translates
/// every protocol uid/gid through the creating user namespace, so creating it
/// in the broker's one-id namespace would make all other canonical owners
/// invalid and Linux would reject writes before they reached the daemon.
fn mount_with_helper(mountpoint: &Path) -> io::Result<File> {
    let (child_socket, receive_socket) = UnixStream::pair()?;
    let fd = child_socket.as_raw_fd();
    let mut command = Command::new(fusermount_binary());
    command
        .arg("-o")
        .arg("fsname=sarun-rs")
        .arg("--")
        .arg(mountpoint)
        .env(FUSERMOUNT_COMM_FD, fd.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: this closure performs only fcntl on one inherited descriptor.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop(child_socket);
    let device = match receive_fds(&receive_socket, 1) {
        Ok((_, mut fds)) if fds.len() == 1 => fds.pop().unwrap(),
        Ok((_, fds)) => {
            drop(fds);
            drop(receive_socket);
            let output = child.wait_with_output()?;
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(io::Error::other(if detail.is_empty() {
                "fusermount did not return a /dev/fuse descriptor".to_owned()
            } else {
                detail
            }));
        }
        Err(receive_error) => {
            drop(receive_socket);
            let output = child.wait_with_output()?;
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if detail.is_empty() {
                receive_error
            } else {
                io::Error::other(detail)
            });
        }
    };
    drop(receive_socket);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let flags = unsafe { libc::fcntl(device.as_raw_fd(), libc::F_GETFD) };
    if flags >= 0 {
        unsafe { libc::fcntl(device.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    }
    Ok(device)
}

fn unmount_with_helper(mountpoint: &Path) -> io::Result<()> {
    let output = Command::new(fusermount_binary())
        // This must be a propagating, non-lazy unmount. A lazy detach removes
        // only this namespace's attachment and can strand propagated copies
        // when the mountpoint lives below a shared host mount.
        .args(["-u", "-q", "--"])
        .arg(mountpoint)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(io::Error::other(if detail.is_empty() {
            format!("fusermount exited {}", output.status)
        } else {
            detail
        }))
    }
}

fn mountinfo_contains(mountpoint: &Path) -> io::Result<bool> {
    let mut encoded = String::new();
    for &byte in mountpoint.as_os_str().as_bytes() {
        match byte {
            b' ' => encoded.push_str("\\040"),
            b'\t' => encoded.push_str("\\011"),
            b'\n' => encoded.push_str("\\012"),
            b'\\' => encoded.push_str("\\134"),
            value => encoded.push(value as char),
        }
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(mountinfo.lines().any(|line| {
        let Some((mount, filesystem)) = line.split_once(" - ") else {
            return false;
        };
        mount.split_whitespace().nth(4) == Some(encoded.as_str())
            && filesystem
                .split_whitespace()
                .next()
                .is_some_and(|kind| kind.starts_with("fuse"))
    }))
}

fn broker_loop(control: &mut UnixStream, listener: &UnixListener) -> io::Result<()> {
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let polled = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if descriptors[0].revents != 0 {
            let mut request = [0u8; 1];
            match control.read(&mut request) {
                Ok(1) if request == [REQUEST_SHUTDOWN] => {
                    control.write_all(&[REPLY_DONE])?;
                    return Ok(());
                }
                Ok(0) => return Ok(()),
                Ok(_) => return Err(io::Error::other("invalid broker control request")),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            let (mut client, _) = listener.accept()?;
            if peer_uid(&client)? != unsafe { libc::getuid() } {
                continue;
            }
            let mut request = [0u8; 1];
            if client.read_exact(&mut request).is_err() || request != [REQUEST_NAMESPACES] {
                continue;
            }
            let user = File::open("/proc/self/ns/user")?;
            let mount = File::open("/proc/self/ns/mnt")?;
            send_fds(
                &client,
                &[REPLY_READY],
                &[user.as_raw_fd(), mount.as_raw_fd()],
            )?;
        }
    }
}

fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials.uid)
}

fn send_fds(socket: &UnixStream, body: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let iov = [IoSlice::new(body)];
    let ancillary = [ControlMessage::ScmRights(fds)];
    let written = sendmsg::<()>(
        socket.as_raw_fd(),
        &iov,
        &ancillary,
        MsgFlags::MSG_NOSIGNAL,
        None,
    )?;
    if written != body.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short descriptor handoff",
        ));
    }
    Ok(())
}

fn receive_fds(socket: &UnixStream, expected: usize) -> io::Result<(Vec<u8>, Vec<File>)> {
    let mut body = vec![0u8; 4096];
    let mut iov = [IoSliceMut::new(&mut body)];
    let mut ancillary = nix::cmsg_space!([RawFd; 2]);
    let message = recvmsg::<SockaddrStorage>(
        socket.as_raw_fd(),
        &mut iov,
        Some(&mut ancillary),
        MsgFlags::empty(),
    )?;
    if message.bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "descriptor handoff EOF",
        ));
    }
    let bytes = message.bytes;
    let mut received = Vec::new();
    for control in message.cmsgs().map_err(io::Error::other)? {
        if let ControlMessageOwned::ScmRights(fds) = control {
            for fd in fds {
                // SAFETY: SCM_RIGHTS installed a new owned descriptor.
                received.push(unsafe { File::from_raw_fd(fd) });
            }
        }
    }
    body.truncate(bytes);
    if received.len() > expected {
        return Err(io::Error::other("descriptor handoff contained extra fds"));
    }
    Ok((body, received))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_backend_selection_respects_last_explicit_selector() {
        unsafe { std::env::remove_var("SARUN_BROKER") };
        unsafe { std::env::remove_var("SARUN_ENGINE_PARENT") };
        unsafe { std::env::remove_var(RUNNER_ENTERED_ENV) };
        assert!(is_top_level_fuse_run(&[
            "run".into(),
            "--".into(),
            "true".into()
        ]));
        assert!(!is_top_level_fuse_run(&[
            "run".into(),
            "--qemu".into(),
            "aarch64".into(),
            "--".into(),
            "true".into(),
        ]));
        assert!(is_top_level_fuse_run(&[
            "run".into(),
            "--sud".into(),
            "--fuse".into(),
            "--".into(),
            "true".into(),
        ]));
    }
}
