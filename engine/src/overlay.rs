// The multi-box copy-on-write overlay (m3a). One FUSE mount; the synthetic
// root lists one <box_id> subdir per registered box; <mnt>/<box_id>/rel is a
// merged view of lower (the host) plus that box's captured upper. Reads fall
// through to the host; the box's writes are captured per DESIGN.md:
//   D3 — capture is LAZY: a writable open costs nothing and serves from the
//        lower file; the FIRST actual write triggers copy-up (+ row +
//        provenance) and from then on writes are ordinary pwrites to the blob.
//   D4 — every non-empty file's bytes live as a pool blob (data-NULL row);
//        a box is at rest the moment it stops — no consolidate phase.
// m3a scope: lookup/getattr/readdir(plus)/readlink/open/create/read/write/
// truncate/mkdir/unlink/rmdir/symlink. rename is ENOSYS for now (m3b).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use fuser::Errno;
use fuser::FileAttr;
use fuser::FileHandle;
use fuser::FileType;
use fuser::Filesystem;
use fuser::FopenFlags;
use fuser::Generation;
use fuser::INodeNo;
use fuser::LockOwner;
use fuser::OpenFlags;
use fuser::ReplyAttr;
use fuser::ReplyCreate;
use fuser::ReplyData;
use fuser::ReplyDirectory;
use fuser::ReplyDirectoryPlus;
use fuser::ReplyEmpty;
use fuser::ReplyEntry;
use fuser::ReplyOpen;
use fuser::ReplyWrite;
use fuser::Request;
use fuser::TimeOrNow;

use crate::capture::BoxState;
use crate::capture::Entry;
use crate::capture::blob_path;

const TTL: Duration = Duration::from_secs(1);

fn ts(secs: i64, nanos: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nanos as u32)
    } else {
        UNIX_EPOCH - Duration::new((-secs) as u64, 0)
    }
}

fn ns_ts(ns: i64) -> SystemTime {
    ts(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000))
}

fn kind_of_mode(mode: u32) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// (box_id, rel) — "" rel is the box root; box_id 0 is the synthetic mount root.
type Key = (i64, String);

/// Clone-able handle: fuser owns one clone as the mounted filesystem, the
/// control plane holds another to add/remove boxes. All state is behind the
/// shared Inner.
#[derive(Clone)]
pub struct Overlay {
    inner: Arc<Inner>,
}

struct Inner {
    lower: PathBuf,
    boxes: RwLock<BTreeMap<i64, Arc<BoxState>>>,
    ino_to_key: RwLock<HashMap<u64, Key>>,
    key_to_ino: RwLock<HashMap<Key, u64>>,
    next_ino: AtomicU64,
    fhs: RwLock<HashMap<u64, Mutex<Fh>>>,
    next_fh: AtomicU64,
}

struct Fh {
    inner: FhInner,
}

struct FhInner {
    box_id: i64,
    rel: String,
    file: Option<File>,
    upper: bool,
    dirty: bool,
    last_pid: u32,
}

enum Layer {
    Absent,
    UpperFile { rowid: i64, mode: u32 },
    UpperDir { mode: u32, mtime_ns: i64 },
    UpperSymlink { target: PathBuf },
    Lower,
}

impl Overlay {
    pub fn new(lower: PathBuf) -> Self {
        let mut i2k = HashMap::new();
        i2k.insert(1u64, (0i64, String::new()));
        let mut k2i = HashMap::new();
        k2i.insert((0i64, String::new()), 1u64);
        Overlay { inner: Arc::new(Inner {
            lower,
            boxes: RwLock::new(BTreeMap::new()),
            ino_to_key: RwLock::new(i2k),
            key_to_ino: RwLock::new(k2i),
            next_ino: AtomicU64::new(2),
            fhs: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }) }
    }

    pub fn add_box(&self, b: Arc<BoxState>) {
        self.inner.boxes.write().unwrap().insert(b.id, b);
    }

    pub fn remove_box(&self, id: i64) {
        self.inner.boxes.write().unwrap().remove(&id);
    }

    fn box_of(&self, id: i64) -> Option<Arc<BoxState>> {
        self.inner.boxes.read().unwrap().get(&id).cloned()
    }

    fn key_of(&self, ino: INodeNo) -> Option<Key> {
        self.inner.ino_to_key.read().unwrap().get(&u64::from(ino)).cloned()
    }

    fn ino_for(&self, key: &Key) -> u64 {
        if let Some(i) = self.inner.key_to_ino.read().unwrap().get(key) {
            return *i;
        }
        let mut k2i = self.inner.key_to_ino.write().unwrap();
        if let Some(i) = k2i.get(key) {
            return *i;
        }
        let i = self.inner.next_ino.fetch_add(1, Ordering::Relaxed);
        k2i.insert(key.clone(), i);
        self.inner.ino_to_key.write().unwrap().insert(i, key.clone());
        i
    }

    fn host(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.inner.lower.clone() } else { self.inner.lower.join(rel) }
    }

    fn layer(&self, b: &BoxState, rel: &str) -> Layer {
        match b.entry(rel) {
            Some(Entry::Whiteout) => Layer::Absent,
            Some(Entry::File { rowid, mode }) => Layer::UpperFile { rowid, mode },
            Some(Entry::Dir { mode, mtime_ns }) => Layer::UpperDir { mode, mtime_ns },
            Some(Entry::Symlink { target }) => Layer::UpperSymlink { target },
            None => {
                if self.host(rel).symlink_metadata().is_ok() {
                    Layer::Lower
                } else {
                    Layer::Absent
                }
            }
        }
    }

    fn lower_attr(&self, ino: u64, rel: &str) -> Option<FileAttr> {
        let md = self.host(rel).symlink_metadata().ok()?;
        Some(self.attr_from_md(ino, &md))
    }

    fn attr_from_md(&self, ino: u64, md: &std::fs::Metadata) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: md.size(),
            blocks: md.blocks(),
            atime: ts(md.atime(), md.atime_nsec()),
            mtime: ts(md.mtime(), md.mtime_nsec()),
            ctime: ts(md.ctime(), md.ctime_nsec()),
            crtime: UNIX_EPOCH,
            kind: kind_of_mode(md.mode()),
            perm: (md.mode() & 0o7777) as u16,
            nlink: md.nlink() as u32,
            uid: md.uid(),
            gid: md.gid(),
            rdev: md.rdev() as u32,
            blksize: 512,
            flags: 0,
        }
    }

    fn synth_dir_attr(&self, ino: u64, mode: u32, mtime_ns: i64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino), size: 0, blocks: 0,
            atime: ns_ts(mtime_ns), mtime: ns_ts(mtime_ns), ctime: ns_ts(mtime_ns),
            crtime: UNIX_EPOCH, kind: FileType::Directory,
            perm: (mode & 0o7777) as u16, nlink: 2, uid: 0, gid: 0, rdev: 0,
            blksize: 512, flags: 0,
        }
    }

    fn synth_link_attr(&self, ino: u64, len: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino), size: len, blocks: 0,
            atime: UNIX_EPOCH, mtime: UNIX_EPOCH, ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH, kind: FileType::Symlink,
            perm: 0o777, nlink: 1, uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
        }
    }

    /// Attributes for (box, rel) through the merge, or None when absent.
    fn attr_of(&self, b: &BoxState, ino: u64, rel: &str) -> Option<FileAttr> {
        match self.layer(b, rel) {
            Layer::Absent => None,
            Layer::Lower => self.lower_attr(ino, rel),
            Layer::UpperFile { rowid, mode } => {
                let bp = blob_path(b.id, rowid);
                let md = bp.metadata().ok()?;
                let mut a = self.attr_from_md(ino, &md);
                a.perm = (mode & 0o7777) as u16;
                a.kind = FileType::RegularFile;
                Some(a)
            }
            Layer::UpperDir { mode, mtime_ns } =>
                Some(self.synth_dir_attr(ino, mode, mtime_ns)),
            Layer::UpperSymlink { target } =>
                Some(self.synth_link_attr(
                    ino, target.as_os_str().as_encoded_bytes().len() as u64)),
        }
    }

    /// D3: the first actual write to `rel` copies the lower bytes into a fresh
    /// pool blob (creating the row + provenance) and returns the RW blob file.
    fn copy_up(&self, b: &BoxState, rel: &str, pid: u32) -> std::io::Result<File> {
        let writer = b.writer_for(pid);
        let lower_md = self.host(rel).symlink_metadata().ok();
        let mode = lower_md.as_ref().map(|m| m.mode()).unwrap_or(0o100644);
        let rowid = b.ensure_file_row(rel, mode, writer);
        let bp = blob_path(b.id, rowid);
        if let Some(parent) = bp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !bp.exists() {
            if lower_md.is_some() {
                std::fs::copy(self.host(rel), &bp)?;
            } else {
                File::create(&bp)?;
            }
        }
        OpenOptions::new().read(true).write(true).open(&bp)
    }

    fn reg_fh(&self, fh: FhInner) -> u64 {
        let n = self.inner.next_fh.fetch_add(1, Ordering::Relaxed);
        self.inner.fhs.write().unwrap().insert(n, Mutex::new(Fh { inner: fh }));
        n
    }

    /// Merged listing of (box, rel): (name, kind, child-ino, Option<attr>).
    fn scan_dir(&self, b: &BoxState, rel: &str, plus: bool)
                -> Vec<(String, FileType, u64, Option<FileAttr>)> {
        let mut names: BTreeMap<String, ()> = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(self.host(rel)) {
            for ent in rd.flatten() {
                if let Some(n) = ent.file_name().to_str() {
                    names.insert(n.to_string(), ());
                }
            }
        }
        let (white, present) = b.children_of(rel);
        for w in &white {
            names.remove(w);
        }
        for p in present {
            names.insert(p, ());
        }
        let mut out = vec![];
        for name in names.keys() {
            let crel = if rel.is_empty() { name.clone() }
                       else { format!("{rel}/{name}") };
            let cino = self.ino_for(&(b.id, crel.clone()));
            let attr = self.attr_of(b, cino, &crel);
            let Some(attr) = attr else { continue };
            out.push((name.clone(), attr.kind, cino,
                      if plus { Some(attr) } else { None }));
        }
        out
    }
}

impl Filesystem for Overlay {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some((bid, prel)) = self.key_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(name) = name.to_str() else { return reply.error(Errno::ENOENT) };
        if bid == 0 {
            // synthetic root: entries are box ids
            let Ok(id) = name.parse::<i64>() else { return reply.error(Errno::ENOENT) };
            if self.box_of(id).is_none() {
                return reply.error(Errno::ENOENT);
            }
            let ino = self.ino_for(&(id, String::new()));
            return reply.entry(&TTL, &self.synth_dir_attr(ino, 0o40755, 0),
                               Generation(0));
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        let ino = self.ino_for(&(bid, rel.clone()));
        match self.attr_of(&b, ino, &rel) {
            Some(a) => reply.entry(&TTL, &a, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>,
               reply: ReplyAttr) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        if bid == 0 || rel.is_empty() {
            return reply.attr(&TTL, &self.synth_dir_attr(u64::from(ino), 0o40755, 0));
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        match self.attr_of(&b, u64::from(ino), &rel) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        match self.layer(&b, &rel) {
            Layer::UpperSymlink { target } =>
                reply.data(target.as_os_str().as_encoded_bytes()),
            Layer::Lower => match std::fs::read_link(self.host(&rel)) {
                Ok(t) => reply.data(t.as_os_str().as_encoded_bytes()),
                Err(_) => reply.error(Errno::EINVAL),
            },
            _ => reply.error(Errno::EINVAL),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let want_write = !matches!(flags.acc_mode(),
                                   fuser::OpenAccMode::O_RDONLY);
        let (file, upper) = match self.layer(&b, &rel) {
            Layer::UpperFile { rowid, .. } => {
                let bp = blob_path(bid, rowid);
                match OpenOptions::new().read(true).write(want_write).open(&bp) {
                    Ok(f) => (Some(f), true),
                    Err(_) => return reply.error(Errno::EIO),
                }
            }
            Layer::Lower => match File::open(self.host(&rel)) {
                // D3: open-for-write stays on the LOWER file (read-only) until
                // the first write op arrives — opens are free.
                Ok(f) => (Some(f), false),
                Err(_) => return reply.error(Errno::EACCES),
            },
            _ => return reply.error(Errno::ENOENT),
        };
        let n = self.reg_fh(FhInner {
            box_id: bid, rel, file, upper, dirty: false, last_pid: req.pid(),
        });
        reply.opened(FileHandle(n), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn create(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32,
              _umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some((bid, prel)) = self.key_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        if bid == 0 {
            return reply.error(Errno::EPERM);
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let Some(name) = name.to_str() else { return reply.error(Errno::EINVAL) };
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        let writer = b.writer_for(req.pid());
        let rowid = b.ensure_file_row(&rel, mode | 0o100000, writer);
        let bp = blob_path(bid, rowid);
        if let Some(p) = bp.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let f = match OpenOptions::new().read(true).write(true).create(true)
            .truncate(true).open(&bp) {
            Ok(f) => f,
            Err(_) => return reply.error(Errno::EIO),
        };
        let ino = self.ino_for(&(bid, rel.clone()));
        let md = f.metadata().ok();
        let mut attr = md.map(|m| self.attr_from_md(ino, &m))
            .unwrap_or_else(|| self.synth_dir_attr(ino, mode, 0));
        attr.kind = FileType::RegularFile;
        attr.perm = (mode & 0o7777) as u16;
        let n = self.reg_fh(FhInner {
            box_id: bid, rel, file: Some(f), upper: true,
            dirty: true, last_pid: req.pid(),
        });
        reply.created(&TTL, &attr, Generation(0), FileHandle(n),
                      FopenFlags::empty());
    }

    fn read(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64,
            size: u32, _flags: OpenFlags, _lo: Option<LockOwner>, reply: ReplyData) {
        let fhs = self.inner.fhs.read().unwrap();
        let Some(h) = fhs.get(&u64::from(fh)) else {
            return reply.error(Errno::EBADF);
        };
        let h = h.lock().unwrap();
        let Some(f) = h.inner.file.as_ref() else {
            return reply.error(Errno::EBADF);
        };
        let mut buf = vec![0u8; size as usize];
        match f.read_at(&mut buf, offset) {
            Ok(n) => reply.data(&buf[..n]),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn write(&self, req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64,
             data: &[u8], _wf: fuser::WriteFlags, _flags: OpenFlags,
             _lo: Option<LockOwner>, reply: ReplyWrite) {
        let fhs = self.inner.fhs.read().unwrap();
        let Some(h) = fhs.get(&u64::from(fh)) else {
            return reply.error(Errno::EBADF);
        };
        let mut h = h.lock().unwrap();
        if !h.inner.upper {
            // D3: the FIRST write triggers copy-up + row + provenance.
            let Some(b) = self.box_of(h.inner.box_id) else {
                return reply.error(Errno::EIO);
            };
            match self.copy_up(&b, &h.inner.rel.clone(), req.pid()) {
                Ok(f) => {
                    h.inner.file = Some(f);
                    h.inner.upper = true;
                }
                Err(_) => return reply.error(Errno::EIO),
            }
        }
        h.inner.dirty = true;
        h.inner.last_pid = req.pid();
        let Some(f) = h.inner.file.as_ref() else {
            return reply.error(Errno::EBADF);
        };
        match f.write_at(data, offset) {
            Ok(n) => reply.written(n as u32),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
               _flags: OpenFlags, _lo: Option<LockOwner>, _flush: bool,
               reply: ReplyEmpty) {
        let h = self.inner.fhs.write().unwrap().remove(&u64::from(fh));
        if let Some(h) = h {
            let h = h.into_inner().unwrap();
            if h.inner.dirty {
                if let Some(b) = self.box_of(h.inner.box_id) {
                    let writer = b.writer_for(h.inner.last_pid);
                    if let Some(md) = h.inner.file.as_ref()
                        .and_then(|f| f.metadata().ok()) {
                        b.finalize_file(&h.inner.rel, md.size() as i64,
                                        md.mtime() * 1_000_000_000
                                        + md.mtime_nsec(), writer);
                    }
                }
            }
        }
        reply.ok();
    }

    fn setattr(&self, req: &Request, ino: INodeNo, mode: Option<u32>,
               _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>,
               _atime: Option<TimeOrNow>, _mtime: Option<TimeOrNow>,
               _ctime: Option<SystemTime>, _fh: Option<FileHandle>,
               _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>,
               _bkuptime: Option<SystemTime>, _flags: Option<fuser::BsdFileFlags>,
               reply: ReplyAttr) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        if let Some(sz) = size {
            // truncate: a write — copy-up if still lower, then set_len.
            let f = match self.layer(&b, &rel) {
                Layer::UpperFile { rowid, .. } => OpenOptions::new().write(true)
                    .open(blob_path(bid, rowid)).ok(),
                Layer::Lower => self.copy_up(&b, &rel, req.pid()).ok(),
                _ => None,
            };
            let Some(f) = f else { return reply.error(Errno::EIO) };
            if f.set_len(sz).is_err() {
                return reply.error(Errno::EIO);
            }
        }
        if let (Some(m), Some(Entry::File { rowid, mode: _ })) =
            (mode, b.entry(&rel)) {
            // chmod on a captured file: update the row's mode (blob perms are
            // an artifact, the row is the truth).
            let writer = b.writer_for(req.pid());
            b.ensure_file_row(&rel, m, writer);
            let _ = rowid;
        }
        match self.attr_of(&b, u64::from(ino), &rel) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32,
             _umask: u32, reply: ReplyEntry) {
        let Some((bid, prel)) = self.key_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        if bid == 0 {
            return reply.error(Errno::EPERM);
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let Some(name) = name.to_str() else { return reply.error(Errno::EINVAL) };
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        if !matches!(self.layer(&b, &rel), Layer::Absent) {
            return reply.error(Errno::EEXIST);
        }
        b.set_dir(&rel, mode, b.writer_for(req.pid()));
        let ino = self.ino_for(&(bid, rel));
        reply.entry(&TTL, &self.synth_dir_attr(ino, mode | 0o40000, 0),
                    Generation(0));
    }

    fn symlink(&self, req: &Request, parent: INodeNo, link_name: &OsStr,
               target: &Path, reply: ReplyEntry) {
        let Some((bid, prel)) = self.key_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        if bid == 0 {
            return reply.error(Errno::EPERM);
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let Some(name) = link_name.to_str() else {
            return reply.error(Errno::EINVAL);
        };
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        b.set_symlink(&rel, target, b.writer_for(req.pid()));
        let ino = self.ino_for(&(bid, rel));
        reply.entry(&TTL, &self.synth_link_attr(
            ino, target.as_os_str().as_encoded_bytes().len() as u64),
            Generation(0));
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr,
              reply: ReplyEmpty) {
        let Some((bid, prel)) = self.key_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let Some(name) = name.to_str() else { return reply.error(Errno::EINVAL) };
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        let writer = b.writer_for(req.pid());
        let lower_exists = self.host(&rel).symlink_metadata().is_ok();
        match b.entry(&rel) {
            Some(Entry::Whiteout) | None if !lower_exists =>
                return reply.error(Errno::ENOENT),
            Some(Entry::File { .. }) | Some(Entry::Symlink { .. }) => {
                b.drop_row(&rel);
                if lower_exists {
                    b.set_whiteout(&rel, writer);
                }
            }
            _ => b.set_whiteout(&rel, writer),
        }
        reply.ok();
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr,
             reply: ReplyEmpty) {
        let Some((bid, prel)) = self.key_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let Some(name) = name.to_str() else { return reply.error(Errno::EINVAL) };
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        if !self.scan_dir(&b, &rel, false).is_empty() {
            return reply.error(Errno::ENOTEMPTY);
        }
        let writer = b.writer_for(req.pid());
        if matches!(b.entry(&rel), Some(Entry::Dir { .. })) {
            b.drop_row(&rel);
        }
        if self.host(&rel).is_dir() {
            b.set_whiteout(&rel, writer);
        }
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64,
               mut reply: ReplyDirectory) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        if bid == 0 {
            for (i, id) in self.inner.boxes.read().unwrap().keys().enumerate() {
                if (i as u64) < offset { continue; }
                let cino = self.ino_for(&(*id, String::new()));
                if reply.add(INodeNo(cino), (i + 1) as u64, FileType::Directory,
                             id.to_string()) {
                    break;
                }
            }
            return reply.ok();
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        for (i, (name, kind, cino, _)) in
            self.scan_dir(&b, &rel, false).into_iter().enumerate() {
            if (i as u64) < offset { continue; }
            if reply.add(INodeNo(cino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, _fh: FileHandle,
                   offset: u64, mut reply: ReplyDirectoryPlus) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        if bid == 0 {
            for (i, id) in self.inner.boxes.read().unwrap().keys().enumerate() {
                if (i as u64) < offset { continue; }
                let cino = self.ino_for(&(*id, String::new()));
                let a = self.synth_dir_attr(cino, 0o40755, 0);
                if reply.add(INodeNo(cino), (i + 1) as u64, id.to_string(),
                             &TTL, &a, Generation(0)) {
                    break;
                }
            }
            return reply.ok();
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        for (i, (name, _k, cino, attr)) in
            self.scan_dir(&b, &rel, true).into_iter().enumerate() {
            if (i as u64) < offset { continue; }
            let Some(a) = attr else { continue };
            if reply.add(INodeNo(cino), (i + 1) as u64, name, &TTL, &a,
                         Generation(0)) {
                break;
            }
        }
        reply.ok();
    }
}
