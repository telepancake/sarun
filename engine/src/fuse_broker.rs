//! Private mount-owner namespace for the FUSE execution backend.
//!
//! The engine serves the raw FUSE connection, but never retains the mount in
//! its host mount namespace. A small, single-threaded child enters a user
//! namespace containing the canonical Unix uid/gid range, creates a private
//! mount namespace, mounts FUSE there, and returns only the connection fd to
//! the engine. A top-level FUSE runner joins those namespaces before any
//! worker thread exists; bubblewrap consequently sees the private root and
//! ordinary ownership syscalls without an outside-in mount operation.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
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
const RUNNER_ENTERED_ENV: &str = "SARUN_FUSE_NAMESPACE_ENTERED";
const ID_MAP_KIND_ENV: &str = "SARUN_FUSE_ID_MAP";
const CANONICAL_ID_MAP: &str = "canonical";
const CALLER_ID_MAP: &str = "caller";
const REQUEST_NAMESPACES: u8 = b'N';
const REQUEST_SHUTDOWN: u8 = b'Q';
const REQUEST_ID_MAP: u8 = b'M';
const REPLY_ID_MAP: u8 = b'I';
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
        Self::create_private_mount(&mountpoint)
    }

    fn create_private_mount(mountpoint: &Path) -> io::Result<(Self, File)> {
        let (mut parent, child_socket) = UnixStream::pair()?;
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
        let (map_request, fds) = match receive_fds(&parent, 0) {
            Ok(reply) => reply,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if map_request != [REQUEST_ID_MAP] || !fds.is_empty() {
            let detail = startup_reply_error(&map_request, fds.len(), "ID-map request");
            drop(fds);
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!("FUSE mount broker: {detail}")));
        }
        let id_map = match configure_id_maps(child.id()) {
            Ok(id_map) => id_map,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other(format!("FUSE mount broker: {error}")));
            }
        };
        if let Err(error) = parent.write_all(&[id_map.reply()]) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let (reply, mut fds) = match receive_fds(&parent, 1) {
            Ok(reply) => reply,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if reply != [REPLY_READY] || fds.len() != 1 {
            let detail = startup_reply_error(&reply, fds.len(), "mount reply");
            drop(fds);
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!("FUSE mount broker: {detail}")));
        }
        let device = fds.pop().unwrap();
        Ok((Self {
            child,
            control: parent,
            socket_path,
        }, device))
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
    let (reply, fds) = receive_exact_fds(&broker, 2, 2)?;
    let id_map = decode_namespace_handoff(&reply, fds.len())?;
    unsafe { std::env::set_var(ID_MAP_KIND_ENV, id_map.env_value()) };
    let user = &fds[0];
    let mount = &fds[1];
    // Joining the owning user namespace first grants the capability needed to
    // join its mount namespace.  Reversing these calls is the outside-in
    // operation the kernel correctly rejects.
    nix::sched::setns(user.as_fd(), nix::sched::CloneFlags::CLONE_NEWUSER)
        .map_err(io::Error::from)?;
    nix::sched::setns(mount.as_fd(), nix::sched::CloneFlags::CLONE_NEWNS)
        .map_err(io::Error::from)?;
    if broker_uses_canonical_id_map() {
        // Establish the machine identity while this process is still
        // single-threaded. Asking rootless bubblewrap to select uid 0 later
        // would replace the complete subordinate map with a one-identity
        // namespace. newgidmap requires setgroups=deny, so the inherited
        // supplementary groups cannot be rewritten; their unmapped IDs confer
        // no authority here.
        if unsafe { libc::setresgid(0, 0, 0) } != 0
            || unsafe { libc::setresuid(0, 0, 0) } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
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

/// True after this runner joined the broker's complete canonical-ID namespace.
pub fn runner_has_canonical_id_map() -> bool {
    std::env::var(ID_MAP_KIND_ENV).is_ok_and(|value| value == CANONICAL_ID_MAP)
}

fn broker_uses_canonical_id_map() -> bool {
    std::env::var(ID_MAP_KIND_ENV).is_ok_and(|value| value == CANONICAL_ID_MAP)
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
    create_private_namespace(control)?;
    let mountpoint = std::env::var_os(MOUNTPOINT_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing broker mountpoint"))?;
    let device = mount_direct(&mountpoint)?;
    if !mountinfo_contains(&mountpoint)? {
        return Err(io::Error::other(
            "FUSE mount was not created in broker namespace",
        ));
    }
    let socket_path = crate::paths::fuse_broker_socket();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    send_fds(control, &[REPLY_READY], &[device.as_raw_fd()])?;
    drop(device);

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

fn create_private_namespace(control: &mut UnixStream) -> io::Result<()> {
    let tasks = std::fs::read_dir("/proc/self/task")?.count();
    if tasks != 1 {
        return Err(io::Error::other(format!(
            "broker started with {tasks} threads"
        )));
    }
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let _ = std::fs::write("/proc/self/setgroups", "deny");
    control.write_all(&[REQUEST_ID_MAP])?;
    let mut reply = [0u8; 1];
    control.read_exact(&mut reply)?;
    let id_map = IdMapKind::from_reply(reply[0])
        .ok_or_else(|| io::Error::other("parent rejected broker ID map"))?;
    unsafe { std::env::set_var(ID_MAP_KIND_ENV, id_map.env_value()) };
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

const CANONICAL_ID_COUNT: u32 = 65_536;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum IdMapKind {
    Canonical,
    Caller,
}

impl IdMapKind {
    fn reply(self) -> u8 {
        match self {
            Self::Canonical => REPLY_ID_MAP,
            Self::Caller => b'i',
        }
    }

    fn from_reply(reply: u8) -> Option<Self> {
        match reply {
            REPLY_ID_MAP => Some(Self::Canonical),
            b'i' => Some(Self::Caller),
            _ => None,
        }
    }

    fn env_value(self) -> &'static str {
        match self {
            Self::Canonical => CANONICAL_ID_MAP,
            Self::Caller => CALLER_ID_MAP,
        }
    }

    fn from_env_value(value: &str) -> Option<Self> {
        match value {
            CANONICAL_ID_MAP => Some(Self::Canonical),
            CALLER_ID_MAP => Some(Self::Caller),
            _ => None,
        }
    }
}

fn startup_reply_error(reply: &[u8], descriptors: usize, expected: &str) -> String {
    if reply.first() == Some(&REPLY_ERROR) {
        String::from_utf8_lossy(&reply[1..]).trim().to_owned()
    } else {
        format!("invalid {expected} {reply:?} with {descriptors} descriptors")
    }
}

fn configure_id_maps(pid: u32) -> io::Result<IdMapKind> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    if unsafe { libc::geteuid() } == 0 {
        std::fs::write(format!("/proc/{pid}/uid_map"),
                       format!("0 0 {CANONICAL_ID_COUNT}\n"))?;
        std::fs::write(format!("/proc/{pid}/gid_map"),
                       format!("0 0 {CANONICAL_ID_COUNT}\n"))?;
        return Ok(IdMapKind::Canonical);
    }
    let (uid_range, gid_range) = match (
        subordinate_range("/etc/subuid", uid),
        subordinate_range("/etc/subgid", gid),
    ) {
        (Ok(uid_range), Ok(gid_range)) => (uid_range, gid_range),
        (uid_result, gid_result) => {
            let detail = uid_result.err()
                .or_else(|| gid_result.err())
                .map(|error| error.to_string())
                .unwrap_or_else(|| "subordinate ID lookup failed".into());
            write_caller_id_map(pid, uid, gid)?;
            eprintln!(
                "sarun-engine: FUSE broker: using caller-only user namespace; \
                 canonical ownership disabled ({detail})"
            );
            return Ok(IdMapKind::Caller);
        }
    };
    run_id_mapper("newuidmap", pid, mapping_arguments(uid, uid_range))?;
    run_id_mapper("newgidmap", pid, mapping_arguments(gid, gid_range))?;
    Ok(IdMapKind::Canonical)
}

fn write_caller_id_map(pid: u32, uid: u32, gid: u32) -> io::Result<()> {
    std::fs::write(format!("/proc/{pid}/uid_map"), format!("0 {uid} 1\n"))?;
    std::fs::write(format!("/proc/{pid}/gid_map"), format!("0 {gid} 1\n"))
}

fn subordinate_range(path: &str, id: u32) -> io::Result<u32> {
    let numeric = id.to_string();
    let user = std::fs::read_to_string("/etc/passwd").ok()
        .and_then(|passwd| passwd.lines().find_map(|line| {
            let fields: Vec<_> = line.split(':').collect();
            (fields.len() > 2 && fields[2] == numeric).then(|| fields[0].to_owned())
        }))
        .unwrap_or_else(|| numeric.clone());
    let contents = std::fs::read_to_string(path).map_err(|error| {
        io::Error::new(error.kind(), format!(
            "cannot read {path}; install `uidmap` and provision at least \
             {CANONICAL_ID_COUNT} subordinate IDs for {user}: {error}"))
    })?;
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let owner = fields.next()?;
        let start = fields.next()?.parse::<u32>().ok()?;
        let count = fields.next()?.parse::<u32>().ok()?;
        ((owner == user || owner == numeric) && count >= CANONICAL_ID_COUNT)
            .then_some(start)
    }).ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, format!(
        "{path} has no {CANONICAL_ID_COUNT}-ID range for {user}; \
         canonical FUSE ownership requires subordinate IDs")))
}

fn mapping_arguments(identity: u32, subordinate: u32) -> Vec<String> {
    let mut result = Vec::new();
    let canonical_before = identity.min(CANONICAL_ID_COUNT);
    if canonical_before != 0 {
        result.extend(["0".into(), subordinate.to_string(), canonical_before.to_string()]);
    }
    result.extend([identity.to_string(), identity.to_string(), "1".into()]);
    if identity < CANONICAL_ID_COUNT - 1 {
        let start = identity + 1;
        result.extend([
            start.to_string(),
            (subordinate + start).to_string(),
            (CANONICAL_ID_COUNT - start).to_string(),
        ]);
    } else if identity >= CANONICAL_ID_COUNT {
        result.extend([
            "0".into(), subordinate.to_string(), CANONICAL_ID_COUNT.to_string(),
        ]);
    }
    result
}

fn run_id_mapper(program: &str, pid: u32, arguments: Vec<String>) -> io::Result<()> {
    let output = Command::new(program).arg(pid.to_string()).args(arguments).output()
        .map_err(|error| io::Error::new(error.kind(), format!(
            "cannot run {program}; install the `uidmap` package: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim())))
    }
}

/// Create the superblock in the broker's complete canonical-ID namespace.
/// FUSE translates protocol uid/gid values through this creating namespace;
/// the parent therefore establishes its subordinate map before this call and
/// receives only the connection descriptor afterward.
fn mount_direct(mountpoint: &Path) -> io::Result<File> {
    let device = OpenOptions::new().read(true).write(true).open("/dev/fuse")?;
    let target = CString::new(mountpoint.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput,
                                    "FUSE mountpoint contains NUL"))?;
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let options = CString::new(format!(
        "fd={},rootmode=40000,user_id={uid},group_id={gid},allow_other",
        device.as_raw_fd(),
    )).unwrap();
    let result = unsafe {
        libc::mount(
            c"sarun-rs".as_ptr(),
            target.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(device)
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
            let id_map = std::env::var(ID_MAP_KIND_ENV)
                .ok()
                .as_deref()
                .and_then(IdMapKind::from_env_value)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "FUSE broker has no valid ID-map selection",
                    )
                })?;
            send_fds(
                &client,
                &[REPLY_READY, id_map.reply()],
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
    let (bytes, received) = receive_fd_chunk(socket, &mut body)?;
    body.truncate(bytes);
    if received.len() > expected {
        return Err(io::Error::other("descriptor handoff contained extra fds"));
    }
    Ok((body, received))
}

fn receive_fd_chunk(socket: &UnixStream, body: &mut [u8]) -> io::Result<(usize, Vec<File>)> {
    let mut iov = [IoSliceMut::new(body)];
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
    if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "descriptor handoff control data was truncated",
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
    Ok((bytes, received))
}

/// Receive one fixed-size descriptor-bearing frame from a stream socket.
///
/// `SCM_RIGHTS` is attached to a byte position, but a Unix stream is still a
/// stream: the body bytes can be returned by more than one `recvmsg`.  Keep
/// receiving until the complete typed frame is present instead of interpreting
/// a short first read as a different (or default) reply.
fn receive_exact_fds(
    socket: &UnixStream,
    body_len: usize,
    expected_fds: usize,
) -> io::Result<(Vec<u8>, Vec<File>)> {
    let mut body = vec![0u8; body_len];
    let mut offset = 0;
    let mut received = Vec::new();

    while offset < body_len {
        let (bytes, fds) = receive_fd_chunk(socket, &mut body[offset..])?;
        received.extend(fds);
        if received.len() > expected_fds {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "descriptor handoff contained extra fds",
            ));
        }
        offset += bytes;
    }

    if received.len() != expected_fds {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "descriptor handoff contained {} fds, expected {expected_fds}",
                received.len()
            ),
        ));
    }
    Ok((body, received))
}

fn decode_namespace_handoff(reply: &[u8], descriptors: usize) -> io::Result<IdMapKind> {
    if reply.len() != 2 || reply[0] != REPLY_READY || descriptors != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "invalid FUSE namespace handoff {reply:?} with {descriptors} descriptors"
            ),
        ));
    }
    IdMapKind::from_reply(reply[1]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown FUSE namespace ID-map reply byte {:#04x}", reply[1]),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subordinate_map_preserves_canonical_ids_and_the_calling_identity() {
        assert_eq!(mapping_arguments(501, 100_000), vec![
            "0", "100000", "501",
            "501", "501", "1",
            "502", "100502", "65034",
        ]);
    }

    #[test]
    fn identity_outside_canonical_range_uses_a_separate_extent() {
        assert_eq!(mapping_arguments(70_000, 100_000), vec![
            "70000", "70000", "1",
            "0", "100000", "65536",
        ]);
    }

    #[test]
    fn id_map_kind_round_trips_over_broker_protocol_and_env() {
        assert_eq!(
            IdMapKind::from_reply(REPLY_ID_MAP),
            Some(IdMapKind::Canonical)
        );
        assert_eq!(IdMapKind::from_reply(b'i'), Some(IdMapKind::Caller));
        assert_eq!(IdMapKind::Canonical.reply(), REPLY_ID_MAP);
        assert_eq!(IdMapKind::Caller.reply(), b'i');
        assert_eq!(
            IdMapKind::from_env_value(CANONICAL_ID_MAP),
            Some(IdMapKind::Canonical)
        );
        assert_eq!(
            IdMapKind::from_env_value(CALLER_ID_MAP),
            Some(IdMapKind::Caller)
        );
    }

    #[test]
    fn namespace_handoff_requires_the_map_byte_and_exact_descriptor_count() {
        assert!(decode_namespace_handoff(&[REPLY_READY], 2).is_err());
        assert!(decode_namespace_handoff(&[REPLY_READY, REPLY_ID_MAP], 1).is_err());
        assert!(decode_namespace_handoff(&[REPLY_READY, REPLY_ID_MAP], 3).is_err());
    }

    #[test]
    fn namespace_handoff_rejects_unknown_map_kind_instead_of_defaulting() {
        let error = decode_namespace_handoff(&[REPLY_READY, b'?'], 2).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unknown FUSE namespace ID-map"));
    }

    #[test]
    fn namespace_handoff_assembles_a_split_stream_frame() {
        let (receiver, mut sender) = UnixStream::pair().unwrap();
        let first = File::open("/dev/null").unwrap();
        let second = File::open("/dev/null").unwrap();
        send_fds(
            &sender,
            &[REPLY_READY],
            &[first.as_raw_fd(), second.as_raw_fd()],
        )
        .unwrap();
        let writer = std::thread::spawn(move || sender.write_all(&[IdMapKind::Caller.reply()]));

        let (reply, fds) = receive_exact_fds(&receiver, 2, 2).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(
            decode_namespace_handoff(&reply, fds.len()).unwrap(),
            IdMapKind::Caller
        );
    }

    #[test]
    fn runner_reports_canonical_map_only_when_broker_selected_it() {
        let old = std::env::var_os(ID_MAP_KIND_ENV);
        unsafe { std::env::set_var(ID_MAP_KIND_ENV, CANONICAL_ID_MAP) };
        assert!(runner_has_canonical_id_map());
        unsafe { std::env::set_var(ID_MAP_KIND_ENV, CALLER_ID_MAP) };
        assert!(!runner_has_canonical_id_map());
        unsafe {
            match old {
                Some(value) => std::env::set_var(ID_MAP_KIND_ENV, value),
                None => std::env::remove_var(ID_MAP_KIND_ENV),
            }
        }
    }

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
