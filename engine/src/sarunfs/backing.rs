//! Read-only access to SarunFs's ordinary host backing tree.
//!
//! This adapter deliberately speaks the upstream `virtiofsd::FileSystem`
//! interface instead of joining a host pathname and reopening it for every
//! protocol callback.  Upstream owns `O_PATH` traversal, inode references,
//! open-handle validation, directory cookies, metadata, and readlink
//! semantics.  Sarun keeps only the composition decision: whether a merged
//! name resolves to this backing tree at all.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use virtiofsd::filesystem::{Context, DirectoryIterator, FileSystem, FsOptions, ZeroCopyWriter};
use virtiofsd::passthrough::read_only::PassthroughFsRo;
use virtiofsd::soft_idmap::Id as _;

const ROOT: u64 = virtiofsd::filesystem::ROOT_ID;
const READ_CHUNK: usize = 1024 * 1024;

fn context() -> Context {
    Context {
        uid: unsafe { libc::geteuid() }.into(),
        gid: unsafe { libc::getegid() }.into(),
        pid: std::process::id() as i32,
    }
}

fn kind(mode: u32) -> crate::sarunfs::NodeKind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => crate::sarunfs::NodeKind::Directory,
        libc::S_IFLNK => crate::sarunfs::NodeKind::Symlink,
        libc::S_IFCHR => crate::sarunfs::NodeKind::CharDevice,
        libc::S_IFBLK => crate::sarunfs::NodeKind::BlockDevice,
        libc::S_IFIFO => crate::sarunfs::NodeKind::NamedPipe,
        libc::S_IFSOCK => crate::sarunfs::NodeKind::Socket,
        _ => crate::sarunfs::NodeKind::RegularFile,
    }
}

fn timestamp(seconds: u64, nanoseconds: u32) -> std::time::SystemTime {
    std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::new(seconds, nanoseconds))
        .unwrap_or(std::time::UNIX_EPOCH)
}

struct Inner {
    root: PathBuf,
    fs: PassthroughFsRo,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.fs.destroy();
    }
}

/// One initialized upstream passthrough tree.  Clones share its inode and
/// handle stores; there is exactly one such store per `SarunFs` core.
#[derive(Clone)]
pub(crate) struct BackingStore {
    inner: Arc<Inner>,
}

/// Primitive backing metadata, independent of either frontend's wire structs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BackingAttr {
    pub(crate) size: u64,
    pub(crate) blocks: u64,
    pub(crate) atime: std::time::SystemTime,
    pub(crate) mtime: std::time::SystemTime,
    pub(crate) ctime: std::time::SystemTime,
    pub(crate) kind: crate::sarunfs::NodeKind,
    pub(crate) mode: u32,
    pub(crate) nlink: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) rdev: u32,
    pub(crate) blksize: u32,
}

impl BackingAttr {
    pub(crate) fn node_attr(self, inode: u64) -> crate::sarunfs::NodeAttr {
        crate::sarunfs::NodeAttr {
            inode,
            size: self.size,
            blocks: self.blocks,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            kind: self.kind,
            perm: (self.mode & 0o7777) as u16,
            nlink: self.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: self.rdev,
            blksize: self.blksize,
            flags: 0,
        }
    }
}

/// A lookup-count reference in the upstream inode store.
pub(crate) struct BackingNode {
    store: BackingStore,
    inode: u64,
    attr: BackingAttr,
    forget: bool,
}

impl Drop for BackingNode {
    fn drop(&mut self) {
        if self.forget {
            self.store.inner.fs.forget(context(), self.inode, 1);
        }
    }
}

/// An upstream open handle with its inode reference.  Drop releases both in
/// protocol order, so a Sarun transport cannot leak backing handles.
pub(crate) struct BackingFile {
    store: BackingStore,
    inode: u64,
    handle: u64,
}

impl Drop for BackingFile {
    fn drop(&mut self) {
        let _ = self.store.inner.fs.release(
            context(),
            self.inode,
            libc::O_RDONLY as u32,
            self.handle,
            false,
            false,
            None,
        );
        self.store.inner.fs.forget(context(), self.inode, 1);
    }
}

struct SliceWriter<'a>(&'a mut [u8]);

impl ZeroCopyWriter for SliceWriter<'_> {
    fn read_from_file_at(
        &mut self,
        file: &File,
        count: usize,
        offset: u64,
        _flags: Option<virtiofsd::oslib::ReadvFlags>,
    ) -> io::Result<usize> {
        let count = count.min(self.0.len());
        file.read_at(&mut self.0[..count], offset)
    }
}

impl BackingStore {
    pub(crate) fn new(root: PathBuf) -> io::Result<Self> {
        let root_dir = root
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 backing root"))?
            .to_owned();
        let config = virtiofsd::passthrough::Config {
            root_dir,
            xattr: true,
            ..Default::default()
        };
        let fs = PassthroughFsRo::new(config)?;
        fs.init(FsOptions::empty())?;
        Ok(Self {
            inner: Arc::new(Inner { root, fs }),
        })
    }

    /// The absolute host path is exposed only for explicit Sarun policy that
    /// intentionally writes the host (`direct`/passthrough) or creates a new
    /// captured blob from a lower file. Ordinary lower reads must use `node`.
    pub(crate) fn direct_path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.inner.root.clone()
        } else {
            self.inner.root.join(rel)
        }
    }

    pub(crate) fn node(&self, rel: &str) -> io::Result<BackingNode> {
        let path = Path::new(rel);
        if path.is_absolute() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }

        let mut inode = ROOT;
        let mut forget = false;
        let mut final_attr = None;
        for component in path.components() {
            let Component::Normal(name) = component else {
                if matches!(component, Component::CurDir) {
                    continue;
                }
                if forget {
                    self.inner.fs.forget(context(), inode, 1);
                }
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            };
            let name = CString::new(name.as_encoded_bytes())
                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
            let entry = match self.inner.fs.lookup(context(), inode, &name) {
                Ok(entry) => entry,
                Err(error) => {
                    if forget {
                        self.inner.fs.forget(context(), inode, 1);
                    }
                    return Err(error);
                }
            };
            if forget {
                self.inner.fs.forget(context(), inode, 1);
            }
            inode = entry.inode;
            forget = true;
            final_attr = Some(entry.attr);
        }

        let raw = match final_attr {
            Some(attr) => attr,
            None => self.inner.fs.getattr(context(), ROOT, None)?.0,
        };
        let attr = BackingAttr {
            size: raw.size,
            blocks: raw.blocks,
            atime: timestamp(raw.atime, raw.atimensec),
            mtime: timestamp(raw.mtime, raw.mtimensec),
            ctime: timestamp(raw.ctime, raw.ctimensec),
            kind: kind(raw.mode),
            mode: raw.mode,
            nlink: raw.nlink,
            uid: raw.uid.into_inner(),
            gid: raw.gid.into_inner(),
            rdev: raw.rdev,
            blksize: raw.blksize,
        };
        Ok(BackingNode {
            store: self.clone(),
            inode,
            attr,
            forget,
        })
    }

    pub(crate) fn attr(&self, rel: &str) -> io::Result<BackingAttr> {
        self.node(rel).map(|node| node.attr)
    }

    pub(crate) fn exists(&self, rel: &str) -> bool {
        self.node(rel).is_ok()
    }

    pub(crate) fn read_all(&self, rel: &str) -> io::Result<Vec<u8>> {
        let file = self.node(rel)?.open()?;
        let mut result = Vec::new();
        let mut offset = 0u64;
        loop {
            let start = result.len();
            result.resize(start + READ_CHUNK, 0);
            let read = file.read_at(&mut result[start..], offset)?;
            result.truncate(start + read);
            if read < READ_CHUNK {
                return Ok(result);
            }
            offset = offset.saturating_add(read as u64);
        }
    }

    pub(crate) fn copy_to(&self, rel: &str, destination: &File) -> io::Result<()> {
        let source = self.node(rel)?.open()?;
        let mut buffer = vec![0; READ_CHUNK];
        let mut offset = 0u64;
        loop {
            let read = source.read_at(&mut buffer, offset)?;
            let mut written = 0usize;
            while written < read {
                let count = destination.write_at(
                    &buffer[written..read],
                    offset.saturating_add(written as u64),
                )?;
                if count == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "copy-up write"));
                }
                written += count;
            }
            if read < buffer.len() {
                destination.set_len(offset.saturating_add(read as u64))?;
                return Ok(());
            }
            offset = offset.saturating_add(read as u64);
        }
    }

    pub(crate) fn statfs(&self) -> io::Result<libc::statvfs64> {
        self.inner.fs.statfs(context(), ROOT)
    }
}

impl BackingNode {
    pub(crate) fn readlink(&self) -> io::Result<Vec<u8>> {
        self.store.inner.fs.readlink(context(), self.inode)
    }

    pub(crate) fn open(mut self) -> io::Result<BackingFile> {
        let (handle, _) = self.store.inner.fs.open(
            context(),
            self.inode,
            false,
            (libc::O_RDONLY | libc::O_CLOEXEC) as u32,
        )?;
        let handle = handle.ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))?;
        let file = BackingFile {
            store: self.store.clone(),
            inode: self.inode,
            handle,
        };
        // The open handle now owns this lookup reference.
        self.forget = false;
        Ok(file)
    }

    pub(crate) fn read_dir(&self) -> io::Result<Vec<String>> {
        let (handle, _) = self.store.inner.fs.opendir(
            context(),
            self.inode,
            (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u32,
        )?;
        let handle = handle.ok_or_else(|| io::Error::from_raw_os_error(libc::EIO))?;
        let result = (|| {
            let mut names = Vec::new();
            let mut offset = 0u64;
            loop {
                let mut entries = self.store.inner.fs.readdir(
                    context(),
                    self.inode,
                    handle,
                    64 * 1024,
                    offset,
                )?;
                let mut next = None;
                while let Some(entry) = DirectoryIterator::next(&mut entries) {
                    next = Some(entry.offset);
                    let bytes = entry.name.to_bytes();
                    if bytes != b"." && bytes != b".." {
                        if let Ok(name) = std::str::from_utf8(bytes) {
                            names.push(name.to_owned());
                        }
                    }
                }
                let Some(next) = next else { break };
                if next == offset {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                offset = next;
            }
            Ok(names)
        })();
        let release = self
            .store
            .inner
            .fs
            .releasedir(context(), self.inode, 0, handle);
        match (result, release) {
            (Ok(names), Ok(())) => Ok(names),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }
}

impl BackingFile {
    pub(crate) fn lseek(&self, offset: u64, whence: u32) -> io::Result<u64> {
        self.store.inner.fs.lseek(
            context(),
            self.inode,
            self.handle,
            offset,
            whence,
        )
    }

    pub(crate) fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        let size = buffer.len().try_into().unwrap_or(u32::MAX);
        self.store.inner.fs.read(
            context(),
            self.inode,
            self.handle,
            SliceWriter(buffer),
            size,
            offset,
            None,
            0,
        )
    }

    pub(crate) fn read_to<W: ZeroCopyWriter>(
        &self,
        writer: W,
        size: u32,
        offset: u64,
    ) -> io::Result<usize> {
        self.store.inner.fs.read(
            context(),
            self.inode,
            self.handle,
            writer,
            size,
            offset,
            None,
            0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_backing_adapter_owns_lookup_open_and_directory_lifetimes() {
        let root = std::env::temp_dir().join(format!(
            "sarun-backing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir/file"), b"backing bytes").unwrap();
        std::os::unix::fs::symlink("dir/file", root.join("link")).unwrap();

        let backing = BackingStore::new(root.clone()).unwrap();
        let attr = backing.attr("dir/file").unwrap();
        assert_eq!(attr.kind, crate::sarunfs::NodeKind::RegularFile);
        assert_eq!(attr.size, 13);
        assert_eq!(backing.read_all("dir/file").unwrap(), b"backing bytes");
        assert_eq!(
            backing.node("link").unwrap().readlink().unwrap(),
            b"dir/file"
        );
        let mut names = backing.node("dir").unwrap().read_dir().unwrap();
        names.sort();
        assert_eq!(names, ["file"]);
        assert!(backing.node("../escape").is_err());
        assert!(backing.statfs().is_ok());

        drop(backing);
        let _ = std::fs::remove_dir_all(root);
    }
}
