//! Direct, descriptor-free filesystem client for embedded Brush/rkati evaluation.
//!
//! Requests use the same raw-FUSE shared-memory ring and canonical `SarunFs`
//! decoder as SUD, but values and owned bytes are returned directly to Rust.
//! No protocol handle or host descriptor is exposed through [`BoxVfs`].

use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::mem::size_of;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brush_core::vfs::{BoxVfs, VfsAccess, VfsDirEntry, VfsFileType, VfsMetadata};
use virtiofsd::fuse::{
    AccessIn, Attr, AttrOut, Dirent, EntryOut, ForgetIn, GetattrIn, InHeader, InitInCompat,
    InitOut, KERNEL_MINOR_VERSION, KERNEL_VERSION, MIN_KERNEL_MINOR_VERSION, Opcode, OpenIn,
    OpenOut, OutHeader, ROOT_ID, ReadIn, ReleaseIn,
};
use virtiofsd::soft_idmap::Id as _;

use crate::sud_ring::{FD_LANE_FD, RING_FD, RingClient, RingMapping, SLOT_DATA};

const MAX_SYMLINKS: usize = 40;
const DIRENT_ALIGNMENT: usize = 8;
const READ_SIZE: usize = SLOT_DATA - size_of::<OutHeader>();

static CURRENT: OnceLock<Arc<DirectFsClient>> = OnceLock::new();
static ADOPT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

trait RequestTransport: Send + Sync + 'static {
    fn request(&self, request: &[u8]) -> io::Result<Vec<u8>>;
}

struct RingTransport(Arc<RingClient>);

impl RequestTransport for RingTransport {
    fn request(&self, request: &[u8]) -> io::Result<Vec<u8>> {
        self.0.request(request)
    }
}

/// Process-local direct filesystem provider. Its internal FUSE handles never
/// leave the client and are synchronously released before an operation returns.
pub(crate) struct DirectFsClient {
    transport: Arc<dyn RequestTransport>,
    next_unique: AtomicU64,
    uid: u32,
    gid: u32,
    supplementary_groups: Vec<u32>,
}

/// Adopt the fixed descriptors inherited by the trusted `inner` process.
///
/// This has no environment or path discovery fallback. Both descriptors are
/// marked close-on-exec immediately; external commands therefore remain on the
/// kernel-FUSE path and cannot acquire this capability accidentally.
pub(crate) fn adopt_inherited() -> io::Result<()> {
    let _guard = ADOPT_LOCK
        .lock()
        .map_err(|_| io::Error::other("direct filesystem adoption lock poisoned"))?;
    if CURRENT.get().is_some() {
        return Ok(());
    }

    set_cloexec(RING_FD)?;
    set_cloexec(FD_LANE_FD)?;
    // SAFETY: this function is the sole process-local adopter, serialized above,
    // and the fixed descriptors were deliberately inherited for this purpose.
    let ring_fd = unsafe { OwnedFd::from_raw_fd(RING_FD) };
    // SAFETY: as above, for the descriptor lane retained for future fd handoff.
    let lane_fd = unsafe { OwnedFd::from_raw_fd(FD_LANE_FD) };
    crate::sud_ring::identify_direct_caller(lane_fd)?;
    let mut mapping = RingMapping::from_fd(ring_fd)?;
    // The mapping outlives its memfd. Close both protocol descriptors before
    // Brush starts so in-process builtins cannot discover or operate on them.
    mapping.discard_descriptor();
    let mapping = Arc::new(mapping);
    let transport = Arc::new(RingTransport(Arc::new(RingClient::new(mapping))));
    let client = Arc::new(DirectFsClient::new(transport)?);
    CURRENT.set(client).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "direct filesystem already adopted",
        )
    })
}

/// Return the direct filesystem capability when inherited by this process.
pub(crate) fn current() -> Option<Arc<DirectFsClient>> {
    CURRENT.get().cloned()
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Clone)]
enum PathPart {
    Root,
    Current,
    Parent,
    Normal(OsString),
}

struct Resolved {
    nodeid: u64,
    attr: Attr,
    lookups: Vec<u64>,
    canonical_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DirectFsMetadata {
    pub file_type: VfsFileType,
    pub mode: u32,
    pub len: u64,
    pub modified: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectFsDirEntry {
    pub path: PathBuf,
    pub file_name: OsString,
    pub file_type: u32,
}

impl DirectFsClient {
    fn new(transport: Arc<dyn RequestTransport>) -> io::Result<Self> {
        let client = Self {
            transport,
            next_unique: AtomicU64::new(1),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            supplementary_groups: supplementary_groups()?,
        };
        client.init()?;
        Ok(client)
    }

    fn init(&self) -> io::Result<()> {
        let input = InitInCompat {
            major: KERNEL_VERSION,
            minor: KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
        };
        let payload = self.call(Opcode::Init, 0, pod_bytes(&input))?;
        let output: InitOut = read_exact_pod(&payload, "INIT")?;
        if output.major != KERNEL_VERSION
            || output.minor < MIN_KERNEL_MINOR_VERSION
            || output.max_write == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported direct FUSE negotiation {}.{} max_write={}",
                    output.major, output.minor, output.max_write
                ),
            ));
        }
        Ok(())
    }

    fn call(&self, opcode: Opcode, nodeid: u64, payload: &[u8]) -> io::Result<Vec<u8>> {
        let length = size_of::<InHeader>()
            .checked_add(payload.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        if length > SLOT_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct FUSE request exceeds ring slot",
            ));
        }
        let unique = self.next_unique.fetch_add(1, Ordering::Relaxed);
        let header = InHeader {
            len: length as u32,
            opcode: opcode as u32,
            unique,
            nodeid,
            uid: self.uid.into(),
            gid: self.gid.into(),
            pid: std::process::id(),
            ..Default::default()
        };
        let mut request = Vec::with_capacity(length);
        request.extend_from_slice(pod_bytes(&header));
        request.extend_from_slice(payload);
        let response = self.transport.request(&request)?;

        if matches!(opcode, Opcode::Forget) {
            if response.is_empty() {
                return Ok(Vec::new());
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FORGET unexpectedly received a reply",
            ));
        }
        if response.len() < size_of::<OutHeader>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short direct FUSE response header",
            ));
        }
        let output: OutHeader = read_pod(&response)?;
        if output.len as usize != response.len() || output.unique != unique {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct FUSE response header mismatch",
            ));
        }
        if output.error > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct FUSE response has a positive errno",
            ));
        }
        if output.error < 0 {
            let errno = output.error.checked_neg().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid direct FUSE errno")
            })?;
            return Err(io::Error::from_raw_os_error(errno));
        }
        Ok(response[size_of::<OutHeader>()..].to_vec())
    }

    fn lookup(&self, parent: u64, name: &OsStr) -> io::Result<EntryOut> {
        let name = name.as_bytes();
        if name.is_empty() || name.len() > 255 || name.contains(&0) {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        let mut payload = Vec::with_capacity(name.len() + 1);
        payload.extend_from_slice(name);
        payload.push(0);
        read_exact_pod(&self.call(Opcode::Lookup, parent, &payload)?, "LOOKUP")
    }

    fn getattr(&self, nodeid: u64) -> io::Result<Attr> {
        let output: AttrOut = read_exact_pod(
            &self.call(Opcode::Getattr, nodeid, pod_bytes(&GetattrIn::default()))?,
            "GETATTR",
        )?;
        Ok(output.attr)
    }

    fn readlink_node(&self, nodeid: u64) -> io::Result<PathBuf> {
        let payload = self.call(Opcode::Readlink, nodeid, &[])?;
        if payload.is_empty() || payload.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid READLINK payload",
            ));
        }
        Ok(PathBuf::from(OsString::from_vec(payload)))
    }

    fn forget(&self, nodeid: u64) -> io::Result<()> {
        self.call(Opcode::Forget, nodeid, pod_bytes(&ForgetIn { nlookup: 1 }))?;
        Ok(())
    }

    fn forget_all(&self, lookups: &[u64]) -> io::Result<()> {
        let mut first_error = None;
        for &nodeid in lookups.iter().rev() {
            if let Err(error) = self.forget(nodeid)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn resolve(&self, path: &Path, follow_final: bool) -> io::Result<Resolved> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct BoxVfs paths must be absolute",
            ));
        }
        let mut pending = path_parts(path)?;
        let root_attr = self.getattr(ROOT_ID)?;
        let mut stack = vec![(ROOT_ID, root_attr)];
        let mut names = Vec::<OsString>::new();
        let mut lookups = Vec::new();
        let mut symlinks = 0;

        let result = (|| {
            while let Some(part) = pending.pop_front() {
                match part {
                    PathPart::Root => {
                        stack.truncate(1);
                        names.clear();
                    }
                    PathPart::Current => {
                        ensure_searchable(
                            stack.last().expect("path stack always contains root").1,
                            self.uid,
                            self.gid,
                            &self.supplementary_groups,
                        )?;
                    }
                    PathPart::Parent => {
                        ensure_searchable(
                            stack.last().expect("path stack always contains root").1,
                            self.uid,
                            self.gid,
                            &self.supplementary_groups,
                        )?;
                        if stack.len() > 1 {
                            stack.pop();
                            names.pop();
                        }
                    }
                    PathPart::Normal(name) => {
                        let &(parent, parent_attr) =
                            stack.last().expect("path stack always contains root");
                        ensure_searchable(
                            parent_attr,
                            self.uid,
                            self.gid,
                            &self.supplementary_groups,
                        )?;
                        let entry = self.lookup(parent, &name)?;
                        lookups.push(entry.nodeid);
                        let is_last = pending.is_empty();
                        if mode_type(entry.attr.mode) == VfsFileType::Symlink
                            && (follow_final || !is_last)
                        {
                            symlinks += 1;
                            if symlinks > MAX_SYMLINKS {
                                return Err(io::Error::from_raw_os_error(libc::ELOOP));
                            }
                            let target = self.readlink_node(entry.nodeid)?;
                            let mut target_parts = path_parts(&target)?;
                            if target.is_absolute() {
                                stack.truncate(1);
                                names.clear();
                            }
                            while let Some(target_part) = target_parts.pop_back() {
                                pending.push_front(target_part);
                            }
                        } else {
                            stack.push((entry.nodeid, entry.attr));
                            names.push(name);
                        }
                    }
                }
            }
            let nodeid = stack.last().expect("path stack always contains root").0;
            let attr = self.getattr(nodeid)?;
            Ok(Resolved {
                nodeid,
                attr,
                lookups: std::mem::take(&mut lookups),
                canonical_path: names
                    .iter()
                    .fold(PathBuf::from("/"), |path, name| path.join(name)),
            })
        })();

        match result {
            Ok(resolved) => Ok(resolved),
            Err(error) => {
                let _ = self.forget_all(&lookups);
                Err(error)
            }
        }
    }

    fn with_resolved<T>(
        &self,
        path: &Path,
        follow_final: bool,
        operation: impl FnOnce(&Resolved) -> io::Result<T>,
    ) -> io::Result<T> {
        let resolved = self.resolve(path, follow_final)?;
        let result = operation(&resolved);
        let forgotten = self.forget_all(&resolved.lookups);
        match (result, forgotten) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    fn open(&self, nodeid: u64, directory: bool) -> io::Result<u64> {
        let opcode = if directory {
            Opcode::Opendir
        } else {
            Opcode::Open
        };
        let flags = if directory {
            (libc::O_RDONLY | libc::O_DIRECTORY) as u32
        } else {
            libc::O_RDONLY as u32
        };
        let output: OpenOut = read_exact_pod(
            &self.call(
                opcode,
                nodeid,
                pod_bytes(&OpenIn {
                    flags,
                    open_flags: 0,
                }),
            )?,
            if directory { "OPENDIR" } else { "OPEN" },
        )?;
        Ok(output.fh)
    }

    fn release(&self, nodeid: u64, handle: u64, directory: bool) -> io::Result<()> {
        let opcode = if directory {
            Opcode::Releasedir
        } else {
            Opcode::Release
        };
        let payload = self.call(
            opcode,
            nodeid,
            pod_bytes(&ReleaseIn {
                fh: handle,
                flags: libc::O_RDONLY as u32,
                release_flags: 0,
                lock_owner: 0,
            }),
        )?;
        if payload.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RELEASE returned an unexpected payload",
            ))
        }
    }

    fn read_all(&self, resolved: &Resolved) -> io::Result<Vec<u8>> {
        if mode_type(resolved.attr.mode) == VfsFileType::Directory {
            return Err(io::Error::from_raw_os_error(libc::EISDIR));
        }
        evaluate_access(
            resolved.attr,
            self.uid,
            self.gid,
            &self.supplementary_groups,
            VfsAccess::Read,
        )?;
        let handle = self.open(resolved.nodeid, false)?;
        let result = (|| {
            let initial_capacity = usize::try_from(resolved.attr.size)
                .unwrap_or(READ_SIZE)
                .min(READ_SIZE);
            let mut bytes = Vec::with_capacity(initial_capacity);
            let mut offset = 0u64;
            loop {
                let payload = self.call(
                    Opcode::Read,
                    resolved.nodeid,
                    pod_bytes(&ReadIn {
                        fh: handle,
                        offset,
                        size: READ_SIZE as u32,
                        flags: libc::O_RDONLY as u32,
                        ..Default::default()
                    }),
                )?;
                if payload.len() > READ_SIZE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "READ exceeded requested size",
                    ));
                }
                if payload.is_empty() {
                    break;
                }
                offset = offset
                    .checked_add(payload.len() as u64)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::FileTooLarge))?;
                bytes.extend_from_slice(&payload);
            }
            Ok(bytes)
        })();
        let released = self.release(resolved.nodeid, handle, false);
        match (result, released) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(bytes), Ok(())) => Ok(bytes),
        }
    }

    fn read_directory(
        &self,
        path: &Path,
        resolved: &Resolved,
    ) -> io::Result<Vec<DirectFsDirEntry>> {
        if mode_type(resolved.attr.mode) != VfsFileType::Directory {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        evaluate_access(
            resolved.attr,
            self.uid,
            self.gid,
            &self.supplementary_groups,
            VfsAccess::Read,
        )?;
        let handle = self.open(resolved.nodeid, true)?;
        let result = (|| {
            let mut entries = Vec::new();
            let mut offset = 0u64;
            loop {
                let payload = self.call(
                    Opcode::Readdir,
                    resolved.nodeid,
                    pod_bytes(&ReadIn {
                        fh: handle,
                        offset,
                        size: READ_SIZE as u32,
                        ..Default::default()
                    }),
                )?;
                if payload.is_empty() {
                    break;
                }
                let mut cursor = 0;
                while cursor < payload.len() {
                    if payload.len() - cursor < size_of::<Dirent>() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "truncated READDIR entry",
                        ));
                    }
                    let dirent: Dirent = read_pod(&payload[cursor..])?;
                    let name_start = cursor + size_of::<Dirent>();
                    let name_end = name_start
                        .checked_add(dirent.namelen as usize)
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
                    if name_end > payload.len() || dirent.off <= offset {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid READDIR entry",
                        ));
                    }
                    let name = &payload[name_start..name_end];
                    if name.is_empty() || name.contains(&0) || name.contains(&b'/') {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid READDIR name",
                        ));
                    }
                    if name != b"." && name != b".." {
                        let file_name = OsString::from_vec(name.to_vec());
                        entries.push(DirectFsDirEntry {
                            path: path.join(&file_name),
                            file_name,
                            file_type: dirent.type_,
                        });
                    }
                    offset = dirent.off;
                    let record_len = align_up(size_of::<Dirent>() + dirent.namelen as usize);
                    cursor = cursor
                        .checked_add(record_len)
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
                    if cursor > payload.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "READDIR alignment exceeds response",
                        ));
                    }
                }
            }
            Ok(entries)
        })();
        let released = self.release(resolved.nodeid, handle, true);
        match (result, released) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(entries), Ok(())) => Ok(entries),
        }
    }

    pub(crate) fn direct_metadata(&self, path: &Path) -> io::Result<DirectFsMetadata> {
        self.with_resolved(path, true, |resolved| direct_metadata(resolved.attr))
    }

    pub(crate) fn direct_symlink_metadata(&self, path: &Path) -> io::Result<DirectFsMetadata> {
        self.with_resolved(path, false, |resolved| direct_metadata(resolved.attr))
    }

    pub(crate) fn direct_read_dir(&self, path: &Path) -> io::Result<Vec<DirectFsDirEntry>> {
        self.with_resolved(path, true, |resolved| self.read_directory(path, resolved))
    }

    pub(crate) fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.with_resolved(path, true, |resolved| Ok(resolved.canonical_path.clone()))
    }

    /// Expand an absolute byte pattern entirely through direct FUSE requests.
    /// libc is used only for component matching; libc glob/stat never sees the
    /// box path, so there is no host-filesystem fallback.
    pub(crate) fn glob(&self, pattern: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        if !pattern.starts_with(b"/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct filesystem glob patterns must be absolute",
            ));
        }
        if pattern.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in glob pattern",
            ));
        }
        let components: Vec<&[u8]> = pattern[1..].split(|byte| *byte == b'/').collect();
        let mut matches = Vec::new();
        self.glob_components(b"/".to_vec(), &components, &mut matches)?;
        matches.sort_unstable();
        matches.dedup();
        Ok(matches)
    }

    fn glob_components(
        &self,
        prefix: Vec<u8>,
        components: &[&[u8]],
        matches: &mut Vec<Vec<u8>>,
    ) -> io::Result<()> {
        let Some((&component, rest)) = components.split_first() else {
            match self.direct_metadata(Path::new(OsStr::from_bytes(&prefix))) {
                Ok(_) => matches.push(prefix),
                Err(error) if ignorable_glob_error(&error) => {}
                Err(error) => return Err(error),
            }
            return Ok(());
        };

        if has_glob_magic(component) {
            let directory = Path::new(OsStr::from_bytes(&prefix));
            let mut entries = match self.direct_read_dir(directory) {
                Ok(entries) => entries,
                Err(error) if ignorable_glob_error(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            entries.sort_by(|a, b| a.file_name.as_bytes().cmp(b.file_name.as_bytes()));
            let component = CString::new(component).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "NUL in glob component")
            })?;
            for entry in entries {
                let name = entry.file_name.as_bytes();
                let name_c = CString::new(name).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "NUL in directory entry")
                })?;
                // SAFETY: both operands are valid C strings for the duration of
                // this call. FNM_PERIOD preserves libc glob's dotfile rule.
                let matched =
                    unsafe { libc::fnmatch(component.as_ptr(), name_c.as_ptr(), libc::FNM_PERIOD) }
                        == 0;
                if matched {
                    self.glob_components(join_glob_component(&prefix, name), rest, matches)?;
                }
            }
        } else {
            self.glob_components(join_glob_component(&prefix, component), rest, matches)?;
        }
        Ok(())
    }
}

impl BoxVfs for DirectFsClient {
    fn metadata(&self, path: &Path) -> io::Result<VfsMetadata> {
        self.direct_metadata(path).map(|metadata| metadata.into())
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<VfsMetadata> {
        self.direct_symlink_metadata(path)
            .map(|metadata| metadata.into())
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<VfsDirEntry>> {
        self.direct_read_dir(path).map(|entries| {
            entries
                .into_iter()
                .map(|entry| VfsDirEntry {
                    path: entry.path,
                    file_name: entry.file_name,
                    file_type: dirent_type(entry.file_type),
                })
                .collect()
        })
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.with_resolved(path, false, |resolved| {
            if mode_type(resolved.attr.mode) != VfsFileType::Symlink {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            self.readlink_node(resolved.nodeid)
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.with_resolved(path, true, |resolved| self.read_all(resolved))
    }

    fn access(&self, path: &Path, access: VfsAccess) -> io::Result<()> {
        self.with_resolved(path, true, |resolved| {
            evaluate_access(
                resolved.attr,
                self.uid,
                self.gid,
                &self.supplementary_groups,
                access,
            )?;
            let mask = match access {
                VfsAccess::Read => libc::R_OK,
                VfsAccess::Write => libc::W_OK,
                VfsAccess::Execute => libc::X_OK,
            } as u32;
            let payload = self.call(
                Opcode::Access,
                resolved.nodeid,
                pod_bytes(&AccessIn { mask, padding: 0 }),
            )?;
            if payload.is_empty() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ACCESS returned an unexpected payload",
                ))
            }
        })
    }
}

fn supplementary_groups() -> io::Result<Vec<u32>> {
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut groups = vec![0 as libc::gid_t; count as usize];
    if count != 0 {
        let read = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        groups.truncate(read as usize);
    }
    Ok(groups.into_iter().map(|group| group as u32).collect())
}

fn evaluate_access(
    attr: Attr,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
    access: VfsAccess,
) -> io::Result<()> {
    let requested = match access {
        VfsAccess::Read => 0o4,
        VfsAccess::Write => 0o2,
        VfsAccess::Execute => 0o1,
    };
    if uid == 0 {
        if access != VfsAccess::Execute
            || mode_type(attr.mode) == VfsFileType::Directory
            || attr.mode & 0o111 != 0
        {
            return Ok(());
        }
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }

    let attr_uid = attr.uid.into_inner();
    let attr_gid = attr.gid.into_inner();
    let granted = if uid == attr_uid {
        (attr.mode >> 6) & 0o7
    } else if gid == attr_gid || supplementary_groups.contains(&attr_gid) {
        (attr.mode >> 3) & 0o7
    } else {
        attr.mode & 0o7
    };
    if granted & requested == requested {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EACCES))
    }
}

fn ensure_searchable(
    attr: Attr,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> io::Result<()> {
    if mode_type(attr.mode) != VfsFileType::Directory {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    evaluate_access(attr, uid, gid, supplementary_groups, VfsAccess::Execute)
}

/// Split a Unix path without erasing a final slash or explicit `.`/`..`.
/// `Path::components` normalizes those away, which would incorrectly allow
/// `/regular/.`, `/regular/..`, and traversal through non-searchable dirs.
fn path_parts(path: &Path) -> io::Result<VecDeque<PathPart>> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    if bytes.len() > libc::PATH_MAX as usize {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }

    let mut parts = VecDeque::new();
    let absolute = bytes.first() == Some(&b'/');
    if absolute {
        parts.push_back(PathPart::Root);
    }
    let rest = if absolute { &bytes[1..] } else { bytes };
    let segments: Vec<&[u8]> = rest.split(|byte| *byte == b'/').collect();
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.into_iter().enumerate() {
        match segment {
            b"" if index == last && !bytes.is_empty() && bytes != b"/" => {
                parts.push_back(PathPart::Current);
            }
            b"" => {}
            b"." => parts.push_back(PathPart::Current),
            b".." => parts.push_back(PathPart::Parent),
            name => parts.push_back(PathPart::Normal(OsString::from_vec(name.to_vec()))),
        }
    }
    Ok(parts)
}

fn mode_type(mode: u32) -> VfsFileType {
    match mode & libc::S_IFMT {
        libc::S_IFREG => VfsFileType::File,
        libc::S_IFDIR => VfsFileType::Directory,
        libc::S_IFLNK => VfsFileType::Symlink,
        _ => VfsFileType::Other,
    }
}

fn direct_metadata(attr: Attr) -> io::Result<DirectFsMetadata> {
    if attr.mtimensec >= 1_000_000_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "direct FUSE mtime has invalid nanoseconds",
        ));
    }
    let modified = UNIX_EPOCH
        .checked_add(Duration::new(attr.mtime, attr.mtimensec))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "direct FUSE mtime overflow"))?;
    Ok(DirectFsMetadata {
        file_type: mode_type(attr.mode),
        mode: attr.mode,
        len: attr.size,
        modified,
    })
}

impl From<DirectFsMetadata> for VfsMetadata {
    fn from(metadata: DirectFsMetadata) -> Self {
        Self {
            file_type: metadata.file_type,
            len: metadata.len,
        }
    }
}

fn has_glob_magic(component: &[u8]) -> bool {
    component
        .iter()
        .any(|byte| matches!(byte, b'?' | b'*' | b'[' | b'\\'))
}

fn join_glob_component(prefix: &[u8], component: &[u8]) -> Vec<u8> {
    let mut joined =
        Vec::with_capacity(prefix.len() + usize::from(prefix != b"/") + component.len());
    joined.extend_from_slice(prefix);
    if prefix != b"/" && !prefix.ends_with(b"/") {
        joined.push(b'/');
    }
    joined.extend_from_slice(component);
    joined
}

fn ignorable_glob_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::EACCES) | Some(libc::ELOOP)
    )
}

fn dirent_type(file_type: u32) -> VfsFileType {
    match file_type {
        value if value == libc::DT_REG as u32 => VfsFileType::File,
        value if value == libc::DT_DIR as u32 => VfsFileType::Directory,
        value if value == libc::DT_LNK as u32 => VfsFileType::Symlink,
        _ => VfsFileType::Other,
    }
}

const fn align_up(value: usize) -> usize {
    (value + DIRENT_ALIGNMENT - 1) & !(DIRENT_ALIGNMENT - 1)
}

fn pod_bytes<T>(value: &T) -> &[u8] {
    // SAFETY: callers only pass repr(C), Copy FUSE wire values with fully
    // initialized padding (all are built from Default or explicit fields).
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast(), size_of::<T>()) }
}

fn read_pod<T: Copy>(bytes: &[u8]) -> io::Result<T> {
    if bytes.len() < size_of::<T>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short direct FUSE structure",
        ));
    }
    // SAFETY: the destination is copied out unaligned from a complete wire value.
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

fn read_exact_pod<T: Copy>(bytes: &[u8], operation: &str) -> io::Result<T> {
    if bytes.len() != size_of::<T>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{operation} returned {} bytes, expected {}",
                bytes.len(),
                size_of::<T>()
            ),
        ));
    }
    read_pod(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Handler = Box<dyn Fn(&InHeader, &[u8]) -> io::Result<Vec<u8>> + Send>;

    struct StrictTransport {
        handlers: Mutex<VecDeque<Handler>>,
    }

    impl StrictTransport {
        fn new(handlers: Vec<Handler>) -> Arc<Self> {
            Arc::new(Self {
                handlers: Mutex::new(handlers.into()),
            })
        }

        fn assert_drained(&self) {
            assert!(self.handlers.lock().unwrap().is_empty());
        }
    }

    impl RequestTransport for StrictTransport {
        fn request(&self, request: &[u8]) -> io::Result<Vec<u8>> {
            let header: InHeader = read_pod(request)?;
            assert_eq!(header.len as usize, request.len());
            let handler = self
                .handlers
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected FUSE request");
            handler(&header, &request[size_of::<InHeader>()..])
        }
    }

    fn expect(
        opcode: Opcode,
        nodeid: u64,
        reply: impl Fn(&InHeader, &[u8]) -> Vec<u8> + Send + 'static,
    ) -> Handler {
        Box::new(move |header, payload| {
            assert_eq!(header.opcode, opcode as u32);
            assert_eq!(header.nodeid, nodeid);
            Ok(reply(header, payload))
        })
    }

    fn ok<T>(header: &InHeader, payload: &T) -> Vec<u8> {
        ok_bytes(header, pod_bytes(payload))
    }

    fn ok_bytes(header: &InHeader, payload: &[u8]) -> Vec<u8> {
        let output = OutHeader {
            len: (size_of::<OutHeader>() + payload.len()) as u32,
            error: 0,
            unique: header.unique,
        };
        let mut bytes = pod_bytes(&output).to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn init_handler() -> Handler {
        expect(Opcode::Init, 0, |header, payload| {
            let input: InitInCompat = read_exact_pod(payload, "test INIT").unwrap();
            assert_eq!(
                (input.major, input.minor),
                (KERNEL_VERSION, KERNEL_MINOR_VERSION)
            );
            ok(
                header,
                &InitOut {
                    major: KERNEL_VERSION,
                    minor: KERNEL_MINOR_VERSION,
                    max_write: 128 * 1024,
                    ..Default::default()
                },
            )
        })
    }

    fn entry(header: &InHeader, inode: u64, mode: u32, size: u64) -> Vec<u8> {
        ok(
            header,
            &EntryOut {
                nodeid: inode,
                attr: Attr {
                    ino: inode,
                    mode,
                    size,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
    }

    fn attr(header: &InHeader, inode: u64, mode: u32, size: u64) -> Vec<u8> {
        ok(
            header,
            &AttrOut {
                attr: Attr {
                    ino: inode,
                    mode,
                    size,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
    }

    fn forget_handler(inode: u64) -> Handler {
        expect(Opcode::Forget, inode, |_header, payload| {
            let forget: ForgetIn = read_exact_pod(payload, "test FORGET").unwrap();
            assert_eq!(forget.nlookup, 1);
            Vec::new()
        })
    }

    fn root_attr_handler() -> Handler {
        expect(Opcode::Getattr, ROOT_ID, |header, payload| {
            let _: GetattrIn = read_exact_pod(payload, "test root GETATTR").unwrap();
            attr(header, ROOT_ID, libc::S_IFDIR | 0o755, 0)
        })
    }

    fn dirents(entries: &[(u64, u64, u32, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(ino, off, type_, name) in entries {
            let record_len = align_up(size_of::<Dirent>() + name.len());
            bytes.extend_from_slice(pod_bytes(&Dirent {
                ino,
                off,
                namelen: name.len() as u32,
                type_,
            }));
            bytes.extend_from_slice(name);
            bytes.resize(
                bytes.len() + record_len - size_of::<Dirent>() - name.len(),
                0,
            );
        }
        bytes
    }

    #[test]
    fn symlink_resolution_is_absolute_and_balances_every_lookup() {
        let transport = StrictTransport::new(vec![
            init_handler(),
            root_attr_handler(),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"link\0");
                entry(header, 2, libc::S_IFLNK | 0o777, 4)
            }),
            expect(Opcode::Readlink, 2, |header, payload| {
                assert!(payload.is_empty());
                ok_bytes(header, b"real")
            }),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"real\0");
                entry(header, 3, libc::S_IFDIR | 0o755, 0)
            }),
            expect(Opcode::Lookup, 3, |header, payload| {
                assert_eq!(payload, b"item\0");
                entry(header, 4, libc::S_IFREG | 0o644, 19)
            }),
            expect(Opcode::Getattr, 4, |header, payload| {
                let _: GetattrIn = read_exact_pod(payload, "test GETATTR").unwrap();
                attr(header, 4, libc::S_IFREG | 0o644, 19)
            }),
            forget_handler(4),
            forget_handler(3),
            forget_handler(2),
        ]);
        let client = DirectFsClient::new(transport.clone()).unwrap();

        assert_eq!(
            client.canonicalize(Path::new("/link/item")).unwrap(),
            Path::new("/real/item")
        );
        transport.assert_drained();
    }

    #[test]
    fn lstat_returns_fuse_kind_size_and_nanosecond_mtime() {
        let transport = StrictTransport::new(vec![
            init_handler(),
            root_attr_handler(),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"link\0");
                entry(header, 12, libc::S_IFLNK | 0o777, 6)
            }),
            expect(Opcode::Getattr, 12, |header, _| {
                ok(
                    header,
                    &AttrOut {
                        attr: Attr {
                            ino: 12,
                            mode: libc::S_IFLNK | 0o777,
                            size: 6,
                            mtime: 123,
                            mtimensec: 456,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
            }),
            forget_handler(12),
        ]);
        let client = DirectFsClient::new(transport.clone()).unwrap();

        let metadata = client.direct_symlink_metadata(Path::new("/link")).unwrap();
        assert_eq!(metadata.file_type, VfsFileType::Symlink);
        assert_eq!(metadata.len, 6);
        assert_eq!(metadata.modified, UNIX_EPOCH + Duration::new(123, 456));
        transport.assert_drained();
    }

    #[test]
    fn glob_expands_via_readdir_and_never_uses_native_paths() {
        let transport = StrictTransport::new(vec![
            init_handler(),
            root_attr_handler(),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"dir\0");
                entry(header, 20, libc::S_IFDIR | 0o755, 0)
            }),
            expect(Opcode::Getattr, 20, |header, _| {
                attr(header, 20, libc::S_IFDIR | 0o755, 0)
            }),
            expect(Opcode::Opendir, 20, |header, _| {
                ok(
                    header,
                    &OpenOut {
                        fh: 77,
                        ..Default::default()
                    },
                )
            }),
            expect(Opcode::Readdir, 20, |header, payload| {
                let input: ReadIn = read_exact_pod(payload, "test READDIR").unwrap();
                assert_eq!((input.fh, input.offset), (77, 0));
                ok_bytes(
                    header,
                    &dirents(&[
                        (21, 1, libc::DT_REG as u32, b"a.mk"),
                        (22, 2, libc::DT_REG as u32, b"b.txt"),
                        (23, 3, libc::DT_REG as u32, b".hidden.mk"),
                    ]),
                )
            }),
            expect(Opcode::Readdir, 20, |header, payload| {
                let input: ReadIn = read_exact_pod(payload, "test READDIR EOF").unwrap();
                assert_eq!((input.fh, input.offset), (77, 3));
                ok_bytes(header, &[])
            }),
            expect(Opcode::Releasedir, 20, |header, _| ok_bytes(header, &[])),
            forget_handler(20),
            root_attr_handler(),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"dir\0");
                entry(header, 24, libc::S_IFDIR | 0o755, 0)
            }),
            expect(Opcode::Lookup, 24, |header, payload| {
                assert_eq!(payload, b"a.mk\0");
                entry(header, 25, libc::S_IFREG | 0o644, 1)
            }),
            expect(Opcode::Getattr, 25, |header, _| {
                attr(header, 25, libc::S_IFREG | 0o644, 1)
            }),
            forget_handler(25),
            forget_handler(24),
        ]);
        let client = DirectFsClient::new(transport.clone()).unwrap();

        assert_eq!(
            client.glob(b"/dir/*.mk").unwrap(),
            vec![b"/dir/a.mk".to_vec()]
        );
        transport.assert_drained();
    }

    #[test]
    fn whole_file_read_keeps_handle_internal_and_releases_before_forget() {
        let transport = StrictTransport::new(vec![
            init_handler(),
            root_attr_handler(),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"file\0");
                entry(header, 8, libc::S_IFREG | 0o644, 3)
            }),
            expect(Opcode::Getattr, 8, |header, _| {
                attr(header, 8, libc::S_IFREG | 0o644, 3)
            }),
            expect(Opcode::Open, 8, |header, payload| {
                let input: OpenIn = read_exact_pod(payload, "test OPEN").unwrap();
                assert_eq!(input.flags & libc::O_ACCMODE as u32, libc::O_RDONLY as u32);
                ok(
                    header,
                    &OpenOut {
                        fh: 91,
                        ..Default::default()
                    },
                )
            }),
            expect(Opcode::Read, 8, |header, payload| {
                let input: ReadIn = read_exact_pod(payload, "test READ").unwrap();
                assert_eq!((input.fh, input.offset), (91, 0));
                ok_bytes(header, b"abc")
            }),
            expect(Opcode::Read, 8, |header, payload| {
                let input: ReadIn = read_exact_pod(payload, "test READ at EOF").unwrap();
                assert_eq!((input.fh, input.offset), (91, 3));
                ok_bytes(header, b"def")
            }),
            expect(Opcode::Read, 8, |header, payload| {
                let input: ReadIn = read_exact_pod(payload, "test READ EOF").unwrap();
                assert_eq!((input.fh, input.offset), (91, 6));
                ok_bytes(header, &[])
            }),
            expect(Opcode::Release, 8, |header, payload| {
                let input: ReleaseIn = read_exact_pod(payload, "test RELEASE").unwrap();
                assert_eq!(input.fh, 91);
                ok_bytes(header, &[])
            }),
            forget_handler(8),
        ]);
        let client = DirectFsClient::new(transport.clone()).unwrap();

        // GETATTR size is only a capacity hint. A concurrent append must still
        // be observed by reading until the server's empty EOF response.
        assert_eq!(client.read(Path::new("/file")).unwrap(), b"abcdef");
        transport.assert_drained();
    }

    #[test]
    fn dotdot_cannot_traverse_through_a_regular_file() {
        let transport = StrictTransport::new(vec![
            init_handler(),
            root_attr_handler(),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"file\0");
                entry(header, 12, libc::S_IFREG | 0o755, 0)
            }),
            forget_handler(12),
        ]);
        let client = DirectFsClient::new(transport.clone()).unwrap();

        assert_eq!(
            client
                .metadata(Path::new("/file/.."))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOTDIR)
        );
        transport.assert_drained();
    }

    #[test]
    fn every_intermediate_directory_requires_search_permission() {
        let transport = StrictTransport::new(vec![
            init_handler(),
            root_attr_handler(),
            expect(Opcode::Lookup, ROOT_ID, |header, payload| {
                assert_eq!(payload, b"private\0");
                ok(
                    header,
                    &EntryOut {
                        nodeid: 13,
                        attr: Attr {
                            ino: 13,
                            mode: libc::S_IFDIR | 0o700,
                            uid: 123.into(),
                            gid: 123.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
            }),
            forget_handler(13),
        ]);
        let client = DirectFsClient::new(transport.clone()).unwrap();

        assert_eq!(
            client
                .metadata(Path::new("/private/hidden"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EACCES)
        );
        transport.assert_drained();
    }

    #[test]
    fn mismatched_reply_identity_is_rejected() {
        let transport = StrictTransport::new(vec![Box::new(|header, _payload| {
            let output = OutHeader {
                len: size_of::<OutHeader>() as u32,
                error: 0,
                unique: header.unique + 1,
            };
            Ok(pod_bytes(&output).to_vec())
        })]);
        let error = DirectFsClient::new(transport.clone())
            .err()
            .expect("invalid INIT reply must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        transport.assert_drained();
    }

    #[test]
    fn access_evaluation_uses_owner_group_other_and_root_execute_rules() {
        let attr = Attr {
            mode: libc::S_IFREG | 0o640,
            uid: 1000.into(),
            gid: 2000.into(),
            ..Default::default()
        };
        assert!(evaluate_access(attr, 1000, 99, &[], VfsAccess::Write).is_ok());
        assert!(evaluate_access(attr, 99, 2000, &[], VfsAccess::Read).is_ok());
        assert!(evaluate_access(attr, 99, 99, &[2000], VfsAccess::Read).is_ok());
        assert_eq!(
            evaluate_access(attr, 99, 99, &[], VfsAccess::Read)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EACCES)
        );
        assert_eq!(
            evaluate_access(attr, 0, 0, &[], VfsAccess::Execute)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EACCES)
        );
        let executable = Attr {
            mode: libc::S_IFREG | 0o100,
            ..attr
        };
        assert!(evaluate_access(executable, 0, 0, &[], VfsAccess::Execute).is_ok());
    }
}
