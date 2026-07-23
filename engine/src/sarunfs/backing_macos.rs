//! Descriptor-relative, read-only host backing for macOS.
//!
//! Linux uses virtiofsd's `O_PATH` passthrough implementation. Darwin has no
//! `O_PATH`, so this adapter keeps directory descriptors and resolves every
//! component with `openat`/`fstatat` plus `O_NOFOLLOW`. It preserves the same
//! no-symlink-escape boundary while exposing the small interface Sarun needs.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use virtiofsd::filesystem::ZeroCopyWriter;

const READ_CHUNK: usize = 1024 * 1024;

fn cvt(value: libc::c_int) -> io::Result<libc::c_int> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

fn duplicate(file: &File) -> io::Result<File> {
    let fd = cvt(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this call.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn fstat(file: &File) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    cvt(unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) })?;
    // SAFETY: successful fstat initialized the complete structure.
    Ok(unsafe { stat.assume_init() })
}

fn fstatat(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    cvt(unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    // SAFETY: successful fstatat initialized the complete structure.
    Ok(unsafe { stat.assume_init() })
}

fn openat(parent: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let fd = cvt(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    })?;
    // SAFETY: successful openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn timestamp(seconds: libc::time_t, nanoseconds: libc::c_long) -> std::time::SystemTime {
    let nanos = nanoseconds.max(0) as u32;
    if seconds >= 0 {
        std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::new(seconds as u64, nanos))
            .unwrap_or(std::time::UNIX_EPOCH)
    } else {
        std::time::UNIX_EPOCH
            .checked_sub(std::time::Duration::new(seconds.unsigned_abs(), nanos))
            .unwrap_or(std::time::UNIX_EPOCH)
    }
}

fn kind(mode: libc::mode_t) -> crate::sarunfs::NodeKind {
    let kind = mode & libc::S_IFMT;
    if kind == libc::S_IFDIR {
        crate::sarunfs::NodeKind::Directory
    } else if kind == libc::S_IFLNK {
        crate::sarunfs::NodeKind::Symlink
    } else if kind == libc::S_IFCHR {
        crate::sarunfs::NodeKind::CharDevice
    } else if kind == libc::S_IFBLK {
        crate::sarunfs::NodeKind::BlockDevice
    } else if kind == libc::S_IFIFO {
        crate::sarunfs::NodeKind::NamedPipe
    } else if kind == libc::S_IFSOCK {
        crate::sarunfs::NodeKind::Socket
    } else {
        crate::sarunfs::NodeKind::RegularFile
    }
}

#[derive(Clone)]
pub(crate) struct BackingStore {
    root_path: PathBuf,
    root: Arc<File>,
}

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

impl From<libc::stat> for BackingAttr {
    fn from(stat: libc::stat) -> Self {
        Self {
            size: stat.st_size.max(0) as u64,
            blocks: stat.st_blocks.max(0) as u64,
            atime: timestamp(stat.st_atime, stat.st_atime_nsec),
            mtime: timestamp(stat.st_mtime, stat.st_mtime_nsec),
            ctime: timestamp(stat.st_ctime, stat.st_ctime_nsec),
            kind: kind(stat.st_mode),
            mode: stat.st_mode.into(),
            nlink: stat.st_nlink as u32,
            uid: stat.st_uid,
            gid: stat.st_gid,
            rdev: stat.st_rdev as u32,
            blksize: stat.st_blksize.max(0) as u32,
        }
    }
}

pub(crate) struct BackingNode {
    parent: File,
    name: CString,
    stat: libc::stat,
}

pub(crate) struct BackingFile {
    file: File,
    inode: u64,
}

impl BackingStore {
    pub(crate) fn new(root_path: PathBuf) -> io::Result<Self> {
        let root = File::open(&root_path)?;
        let stat = fstat(&root)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        Ok(Self {
            root_path,
            root: Arc::new(root),
        })
    }

    pub(crate) fn direct_path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.root_path.clone()
        } else {
            self.root_path.join(rel)
        }
    }

    pub(crate) fn node(&self, rel: &str) -> io::Result<BackingNode> {
        let path = Path::new(rel);
        if path.is_absolute() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }

        let mut parent = duplicate(&self.root)?;
        let mut components = path.components().peekable();
        if components.peek().is_none() {
            return Ok(BackingNode {
                stat: fstat(&parent)?,
                parent,
                name: CString::new(".").unwrap(),
            });
        }

        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                if matches!(component, Component::CurDir) {
                    continue;
                }
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            };
            let name = CString::new(name.as_bytes())
                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
            let stat = fstatat(&parent, &name)?;
            if components.peek().is_none() {
                return Ok(BackingNode { parent, name, stat });
            }
            if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
            parent = openat(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        }
        Err(io::Error::from_raw_os_error(libc::EINVAL))
    }

    pub(crate) fn attr(&self, rel: &str) -> io::Result<BackingAttr> {
        self.node(rel).map(|node| node.stat.into())
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

    pub(crate) fn statfs(&self) -> io::Result<virtiofsd::Statvfs> {
        let mut stat = MaybeUninit::<virtiofsd::Statvfs>::zeroed();
        cvt(unsafe { libc::fstatvfs(self.root.as_raw_fd(), stat.as_mut_ptr()) })?;
        // SAFETY: successful fstatvfs initialized the complete structure.
        Ok(unsafe { stat.assume_init() })
    }
}

impl BackingNode {
    pub(crate) fn readlink(&self) -> io::Result<Vec<u8>> {
        let mut result = vec![0u8; libc::PATH_MAX as usize];
        let length = unsafe {
            libc::readlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                result.as_mut_ptr().cast(),
                result.len(),
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        result.truncate(length as usize);
        Ok(result)
    }

    pub(crate) fn open(self) -> io::Result<BackingFile> {
        let file = openat(&self.parent, &self.name, libc::O_RDONLY)?;
        Ok(BackingFile {
            file,
            inode: self.stat.st_ino,
        })
    }

    pub(crate) fn read_dir(&self) -> io::Result<Vec<String>> {
        let directory = openat(&self.parent, &self.name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let fd = directory.as_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        // fdopendir owns the descriptor after success.
        std::mem::forget(directory);
        let result = (|| {
            let mut names = Vec::new();
            loop {
                unsafe { *libc::__error() = 0 };
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    return if error.raw_os_error() == Some(0) {
                        Ok(names)
                    } else {
                        Err(error)
                    };
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                let bytes = name.to_bytes();
                if bytes != b"." && bytes != b".." {
                    if let Ok(name) = std::str::from_utf8(bytes) {
                        names.push(name.to_owned());
                    }
                }
            }
        })();
        let close_result = cvt(unsafe { libc::closedir(stream) }).map(|_| ());
        match (result, close_result) {
            (Ok(names), Ok(())) => Ok(names),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }
}

impl BackingFile {
    pub(crate) fn identity(&self) -> u64 {
        self.inode
    }

    pub(crate) fn lseek(&self, offset: u64, whence: u32) -> io::Result<u64> {
        let result = unsafe {
            libc::lseek(
                self.file.as_raw_fd(),
                offset.try_into().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "seek offset exceeds off_t")
                })?,
                whence as libc::c_int,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as u64)
        }
    }

    pub(crate) fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        self.file.read_at(buffer, offset)
    }

    pub(crate) fn read_to<W: ZeroCopyWriter>(
        &self,
        mut writer: W,
        size: u32,
        offset: u64,
    ) -> io::Result<usize> {
        writer.read_from_file_at(&self.file, size as usize, offset, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backing_adapter_uses_descriptor_relative_no_follow_resolution() {
        let root = std::env::temp_dir().join(format!(
            "sarun-backing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let outside = root.with_extension("outside");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("dir/file"), b"backing bytes").unwrap();
        std::fs::write(outside.join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink("dir/file", root.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let backing = BackingStore::new(root.clone()).unwrap();
        assert_eq!(backing.read_all("dir/file").unwrap(), b"backing bytes");
        assert_eq!(
            backing.node("link").unwrap().readlink().unwrap(),
            b"dir/file"
        );
        assert!(backing.read_all("escape/secret").is_err());
        let mut names = backing.node("dir").unwrap().read_dir().unwrap();
        names.sort();
        assert_eq!(names, ["file"]);
        assert!(backing.node("../escape").is_err());
        assert!(backing.statfs().is_ok());

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }
}
