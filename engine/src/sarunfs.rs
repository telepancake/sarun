//! Transport-neutral state for sarun's single filesystem implementation.
//!
//! Protocol frontends do not own inode identity.  FUSE lookup/forget and the
//! virtio-fs guest protocol have identical lifetime rules, so the mapping and
//! reference counts live here and are shared by every frontend.

pub use crate::overlay::SarunFs;
pub(crate) mod backing;
pub(crate) mod layers;
pub(crate) mod mutation;
pub(crate) mod synthetic;

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(crate) type NodeKey = (i64, String);

/// Turn a decoder failure for a header-bearing raw FUSE message into the
/// protocol error reply expected by every byte transport.  Keeping this next
/// to the canonical protocol state prevents `/dev/fuse` and the SUD ring from
/// assigning different meanings to the same malformed message.
pub(crate) fn malformed_fuse_reply(request: &[u8], response: &mut [u8]) -> Option<usize> {
    use std::mem::size_of;
    use virtiofsd::fuse::{InHeader, OutHeader};

    if request.len() < size_of::<InHeader>() || response.len() < size_of::<OutHeader>() {
        return None;
    }
    // SAFETY: the length checks cover one possibly-unaligned protocol header;
    // ByteValued permits every initialized bit pattern.
    let input = unsafe { std::ptr::read_unaligned(request.as_ptr().cast::<InHeader>()) };
    let output = OutHeader {
        len: size_of::<OutHeader>() as u32,
        error: -libc::EIO,
        unique: input.unique,
    };
    // SAFETY: OutHeader is a plain repr(C) protocol value.
    let output_bytes = unsafe {
        std::slice::from_raw_parts(
            (&output as *const OutHeader).cast::<u8>(),
            size_of::<OutHeader>(),
        )
    };
    response[..size_of::<OutHeader>()].copy_from_slice(output_bytes);
    Some(size_of::<OutHeader>())
}

/// Transport-independent file kind. The shared protocol server translates it
/// when constructing a reply; overlay policy never handles a wire enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NodeKind {
    NamedPipe,
    CharDevice,
    BlockDevice,
    Directory,
    RegularFile,
    Symlink,
    Socket,
}

/// Canonical attributes produced by the shared filesystem core.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NodeAttr {
    pub inode: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub kind: NodeKind,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}

#[derive(Debug)]
struct Inodes {
    by_inode: HashMap<u64, NodeKey>,
    by_key: HashMap<NodeKey, u64>,
    lookups: HashMap<u64, u64>,
    next: u64,
}

/// Stable virtual inode identities plus kernel lookup reference counts.
///
/// A `FORGET` that releases the last kernel lookup reference also releases the
/// path identity. Open file and directory descriptions have independent
/// protocol handles and do not need the node id to remain interned; their
/// read/write/release paths resolve through `HandleTable`. Keeping forgotten
/// paths forever is not cheap for configure/build scans that visit hundreds of
/// thousands of names.
#[derive(Debug)]
pub(crate) struct InodeTable {
    state: RwLock<Inodes>,
}

/// One allocator and ownership table for every protocol-visible handle. The
/// stored value describes policy state; this type owns only identity and
/// lifetime, so file, directory and synthetic handles cannot collide or be
/// released through different transport-specific maps.
pub(crate) struct HandleTable<T> {
    next: AtomicU64,
    entries: RwLock<HashMap<u64, Arc<T>>>,
}

impl<T> HandleTable<T> {
    pub(crate) fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, value: T) -> u64 {
        let handle = self.next.fetch_add(1, Ordering::Relaxed);
        assert_ne!(handle, 0, "virtual handle exhausted");
        self.entries
            .write()
            .unwrap()
            .insert(handle, Arc::new(value));
        handle
    }

    pub(crate) fn get(&self, handle: u64) -> Option<Arc<T>> {
        self.entries.read().unwrap().get(&handle).cloned()
    }

    pub(crate) fn remove(&self, handle: u64) -> Option<Arc<T>> {
        self.entries.write().unwrap().remove(&handle)
    }

    pub(crate) fn values(&self) -> Vec<Arc<T>> {
        self.entries.read().unwrap().values().cloned().collect()
    }
}

impl InodeTable {
    pub(crate) fn new(root: NodeKey) -> Self {
        let mut by_inode = HashMap::new();
        by_inode.insert(1, root.clone());
        let mut by_key = HashMap::new();
        by_key.insert(root, 1);
        Self {
            state: RwLock::new(Inodes {
                by_inode,
                by_key,
                lookups: HashMap::new(),
                next: 2,
            }),
        }
    }

    pub(crate) fn key(&self, inode: u64) -> Option<NodeKey> {
        self.state.read().unwrap().by_inode.get(&inode).cloned()
    }

    pub(crate) fn inode(&self, key: &NodeKey) -> Option<u64> {
        self.state.read().unwrap().by_key.get(key).copied()
    }

    pub(crate) fn intern(&self, key: &NodeKey) -> u64 {
        if let Some(inode) = self.state.read().unwrap().by_key.get(key) {
            return *inode;
        }
        let mut state = self.state.write().unwrap();
        if let Some(inode) = state.by_key.get(key) {
            return *inode;
        }
        let inode = state.next;
        state.next = state.next.checked_add(1).expect("virtual inode exhausted");
        state.by_key.insert(key.clone(), inode);
        state.by_inode.insert(inode, key.clone());
        inode
    }

    pub(crate) fn acquire(&self, inode: u64, count: u64) {
        if count == 0 {
            return;
        }
        let mut state = self.state.write().unwrap();
        if state.by_inode.contains_key(&inode) {
            let current = state.lookups.entry(inode).or_default();
            *current = current.saturating_add(count);
        }
    }

    pub(crate) fn forget(&self, inode: u64, count: u64) {
        let mut state = self.state.write().unwrap();
        if inode == 1 {
            return;
        }
        let remaining = state
            .lookups
            .get(&inode)
            .copied()
            .unwrap_or(0)
            .saturating_sub(count);
        if remaining != 0 {
            state.lookups.insert(inode, remaining);
            return;
        }
        state.lookups.remove(&inode);
        if let Some(key) = state.by_inode.remove(&inode) {
            // A detach followed by recreation can already have assigned this
            // key a newer inode. Never remove that newer identity when the
            // kernel forgets the old one.
            if state.by_key.get(&key) == Some(&inode) {
                state.by_key.remove(&key);
            }
        }
    }

    /// Remove a name from the intern table without invalidating the inode.
    /// Open or looked-up references may continue using the old inode, while a
    /// later recreation of the same path receives a fresh identity.
    pub(crate) fn detach(&self, key: &NodeKey) -> Option<u64> {
        self.state.write().unwrap().by_key.remove(key)
    }

    pub(crate) fn remap_subtree(&self, box_id: i64, old: &str, new: &str) {
        let prefix = format!("{old}/");
        let mut state = self.state.write().unwrap();
        let moves: Vec<(NodeKey, NodeKey, u64)> = state
            .by_key
            .iter()
            .filter(|((owner, _), _)| *owner == box_id)
            .filter_map(|((owner, path), inode)| {
                let replacement = if path == old {
                    Some(new.to_owned())
                } else {
                    path.strip_prefix(&prefix)
                        .map(|tail| format!("{new}/{tail}"))
                }?;
                Some(((*owner, path.clone()), (*owner, replacement), *inode))
            })
            .collect();
        for (old_key, new_key, inode) in moves {
            state.by_key.remove(&old_key);
            state.by_key.insert(new_key.clone(), inode);
            state.by_inode.insert(inode, new_key);
        }
    }

    #[cfg(test)]
    pub(crate) fn lookup_count(&self, inode: u64) -> u64 {
        self.state
            .read()
            .unwrap()
            .lookups
            .get(&inode)
            .copied()
            .unwrap_or(0)
    }
}

fn timestamp(value: SystemTime) -> (u64, u32) {
    value
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0))
}

pub(crate) fn virtio_attr(attr: NodeAttr) -> virtiofsd::fuse::Attr {
    let (atime, atimensec) = timestamp(attr.atime);
    let (mtime, mtimensec) = timestamp(attr.mtime);
    let (ctime, ctimensec) = timestamp(attr.ctime);
    let kind = match attr.kind {
        NodeKind::NamedPipe => libc::S_IFIFO,
        NodeKind::CharDevice => libc::S_IFCHR,
        NodeKind::BlockDevice => libc::S_IFBLK,
        NodeKind::Directory => libc::S_IFDIR,
        NodeKind::RegularFile => libc::S_IFREG,
        NodeKind::Symlink => libc::S_IFLNK,
        NodeKind::Socket => libc::S_IFSOCK,
    };
    virtiofsd::fuse::Attr {
        ino: attr.inode,
        size: attr.size,
        blocks: attr.blocks,
        atime,
        mtime,
        ctime,
        atimensec,
        mtimensec,
        ctimensec,
        mode: kind | u32::from(attr.perm),
        nlink: attr.nlink,
        uid: attr.uid.into(),
        gid: attr.gid.into(),
        rdev: attr.rdev,
        blksize: attr.blksize,
        flags: attr.flags,
    }
}

#[doc(hidden)]
pub struct DirIter {
    entries: Vec<OwnedDirEntry>,
    next: usize,
}

struct OwnedDirEntry {
    inode: u64,
    offset: u64,
    kind: u32,
    name: CString,
}

impl DirIter {
    pub(crate) fn new(entries: Vec<(u64, u64, NodeKind, String)>) -> Self {
        let entries = entries
            .into_iter()
            .filter_map(|(inode, offset, kind, name)| {
                let kind = match kind {
                    NodeKind::NamedPipe => libc::DT_FIFO,
                    NodeKind::CharDevice => libc::DT_CHR,
                    NodeKind::BlockDevice => libc::DT_BLK,
                    NodeKind::Directory => libc::DT_DIR,
                    NodeKind::RegularFile => libc::DT_REG,
                    NodeKind::Symlink => libc::DT_LNK,
                    NodeKind::Socket => libc::DT_SOCK,
                };
                Some(OwnedDirEntry {
                    inode,
                    offset,
                    kind: u32::from(kind),
                    name: CString::new(name).ok()?,
                })
            })
            .collect();
        Self { entries, next: 0 }
    }
}

impl virtiofsd::filesystem::DirectoryIterator for DirIter {
    fn next(&mut self) -> Option<virtiofsd::filesystem::DirEntry<'_>> {
        let index = self.next;
        self.next = self.next.saturating_add(1);
        let entry = self.entries.get(index)?;
        Some(virtiofsd::filesystem::DirEntry {
            ino: entry.inode,
            offset: entry.offset,
            type_: entry.kind,
            name: &entry.name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtiofsd::soft_idmap::Id as _;

    fn attr(kind: NodeKind) -> NodeAttr {
        NodeAttr {
            inode: 42,
            size: 123,
            blocks: 2,
            atime: UNIX_EPOCH + std::time::Duration::new(1, 2),
            mtime: UNIX_EPOCH + std::time::Duration::new(3, 4),
            ctime: UNIX_EPOCH + std::time::Duration::new(5, 6),
            kind,
            perm: 0o654,
            nlink: 3,
            uid: 1000,
            gid: 1001,
            rdev: 7,
            blksize: 4096,
            flags: 9,
        }
    }

    #[test]
    fn protocol_attributes_are_a_projection_of_one_canonical_value() {
        let canonical = attr(NodeKind::RegularFile);
        let virtio = virtio_attr(canonical);
        assert_eq!(virtio.ino, canonical.inode);
        assert_eq!(canonical.size, virtio.size);
        assert_eq!(canonical.blocks, virtio.blocks);
        assert_eq!(canonical.perm, (virtio.mode & 0o7777) as u16);
        assert_eq!(canonical.uid, virtio.uid.into_inner());
        assert_eq!(canonical.gid, virtio.gid.into_inner());
        assert_eq!(virtio.mode & libc::S_IFMT, libc::S_IFREG);
    }

    #[test]
    fn handles_share_one_identity_and_lifetime_table() {
        let table = HandleTable::new();
        let file = table.insert("file");
        let directory = table.insert("directory");
        assert_ne!(file, directory);
        assert_eq!(table.get(file).as_deref(), Some(&"file"));
        assert_eq!(table.remove(file).as_deref(), Some(&"file"));
        assert!(table.get(file).is_none());
        assert_eq!(table.get(directory).as_deref(), Some(&"directory"));
    }

    #[test]
    fn every_canonical_kind_has_a_protocol_encoding() {
        let cases = [
            (NodeKind::NamedPipe, libc::S_IFIFO),
            (NodeKind::CharDevice, libc::S_IFCHR),
            (NodeKind::BlockDevice, libc::S_IFBLK),
            (NodeKind::Directory, libc::S_IFDIR),
            (NodeKind::RegularFile, libc::S_IFREG),
            (NodeKind::Symlink, libc::S_IFLNK),
            (NodeKind::Socket, libc::S_IFSOCK),
        ];
        for (kind, mode) in cases {
            assert_eq!(virtio_attr(attr(kind)).mode & libc::S_IFMT, mode);
        }
    }

    #[test]
    fn identity_is_stable_until_the_last_lookup_is_forgotten() {
        let table = InodeTable::new((0, String::new()));
        let key = (7, "a/b".to_owned());
        let inode = table.intern(&key);
        assert_eq!(inode, table.intern(&key));
        assert_eq!(table.key(inode), Some(key));
        table.acquire(inode, 3);
        table.forget(inode, 2);
        assert_eq!(table.lookup_count(inode), 1);
        table.forget(inode, 20);
        assert_eq!(table.lookup_count(inode), 0);
        assert_eq!(table.key(inode), None);
        assert_eq!(table.inode(&key), None);
        assert_ne!(table.intern(&key), inode);
    }

    #[test]
    fn rename_preserves_inode_identity_for_whole_subtree() {
        let table = InodeTable::new((0, String::new()));
        let parent = table.intern(&(4, "old".to_owned()));
        let child = table.intern(&(4, "old/dir/file".to_owned()));
        let other = table.intern(&(5, "old/dir/file".to_owned()));
        table.remap_subtree(4, "old", "new");
        assert_eq!(table.key(parent), Some((4, "new".to_owned())));
        assert_eq!(table.key(child), Some((4, "new/dir/file".to_owned())));
        assert_eq!(table.key(other), Some((5, "old/dir/file".to_owned())));
    }

    #[test]
    fn detached_name_gets_fresh_identity_when_recreated() {
        let table = InodeTable::new((0, String::new()));
        let key = (3, "file".to_owned());
        let old = table.intern(&key);
        table.acquire(old, 1);
        assert_eq!(table.detach(&key), Some(old));
        assert_eq!(table.key(old), Some(key.clone()));
        let new = table.intern(&key);
        assert_ne!(new, old);
        assert_eq!(table.key(new), Some(key));
    }
}
