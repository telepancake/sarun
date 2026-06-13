// sarun-engine — milestone 1: a multithreaded read-only passthrough FUSE
// filesystem over a lower root. The point of this milestone is NOT features —
// it exists to measure the serving-loop scaling that the Python engine's
// single GIL thread cannot deliver (see bench/FINDINGS.md "parallel builds").
//
//   sarun-engine <mountpoint> [--lower /] [--threads N]
//
// Serves lookup/getattr/readdir(plus)/readlink/open/read, nothing else; every
// answer comes straight from the lower tree (no overlay, no capture yet).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use fuser::Config;
use fuser::Errno;
use fuser::FileAttr;
use fuser::FileHandle;
use fuser::FileType;
use fuser::Filesystem;
use fuser::FopenFlags;
use fuser::Generation;
use fuser::INodeNo;
use fuser::LockOwner;
use fuser::MountOption;
use fuser::OpenFlags;
use fuser::ReplyAttr;
use fuser::ReplyData;
use fuser::ReplyDirectory;
use fuser::ReplyDirectoryPlus;
use fuser::ReplyEmpty;
use fuser::ReplyEntry;
use fuser::ReplyOpen;
use fuser::Request;

mod capture;
mod control;
mod discover;
mod overlay;
mod paths;
mod review;
mod runner;

const TTL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct InoTable {
    by_ino: HashMap<u64, PathBuf>,
    by_path: HashMap<PathBuf, u64>,
    next: u64,
}

struct Passthrough {
    lower: PathBuf,
    inos: RwLock<InoTable>,
    files: RwLock<HashMap<u64, File>>,
    next_fh: AtomicU64,
}

fn ts(secs: i64, nanos: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nanos as u32)
    } else {
        UNIX_EPOCH - Duration::new((-secs) as u64, 0)
    }
}

fn kind_of(mode: u32) -> FileType {
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

impl Passthrough {
    fn new(lower: PathBuf) -> Self {
        let mut t = InoTable::default();
        t.next = 2;
        t.by_ino.insert(1, PathBuf::new()); // root: rel ""
        t.by_path.insert(PathBuf::new(), 1);
        Passthrough {
            lower,
            inos: RwLock::new(t),
            files: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn host(&self, rel: &Path) -> PathBuf {
        self.lower.join(rel)
    }

    fn rel_of(&self, ino: INodeNo) -> Option<PathBuf> {
        self.inos.read().unwrap().by_ino.get(&u64::from(ino)).cloned()
    }

    fn ino_for(&self, rel: &Path) -> u64 {
        if let Some(i) = self.inos.read().unwrap().by_path.get(rel) {
            return *i;
        }
        let mut t = self.inos.write().unwrap();
        if let Some(i) = t.by_path.get(rel) {
            return *i;
        }
        let i = t.next;
        t.next += 1;
        t.by_ino.insert(i, rel.to_path_buf());
        t.by_path.insert(rel.to_path_buf(), i);
        i
    }

    fn attr_of(&self, ino: u64, host: &Path) -> Option<FileAttr> {
        let md = std::fs::symlink_metadata(host).ok()?;
        Some(FileAttr {
            ino: INodeNo(ino),
            size: md.size(),
            blocks: md.blocks(),
            atime: ts(md.atime(), md.atime_nsec()),
            mtime: ts(md.mtime(), md.mtime_nsec()),
            ctime: ts(md.ctime(), md.ctime_nsec()),
            crtime: UNIX_EPOCH,
            kind: kind_of(md.mode()),
            perm: (md.mode() & 0o7777) as u16,
            nlink: md.nlink() as u32,
            uid: md.uid(),
            gid: md.gid(),
            rdev: md.rdev() as u32,
            blksize: 512,
            flags: 0,
        })
    }
}

impl Filesystem for Passthrough {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(prel) = self.rel_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let rel = prel.join(name);
        let ino = self.ino_for(&rel);
        match self.attr_of(ino, &self.host(&rel)) {
            Some(a) => reply.entry(&TTL, &a, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(rel) = self.rel_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.attr_of(u64::from(ino), &self.host(&rel)) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(rel) = self.rel_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match std::fs::read_link(self.host(&rel)) {
            Ok(t) => reply.data(t.as_os_str().as_encoded_bytes()),
            Err(_) => reply.error(Errno::EINVAL),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some(rel) = self.rel_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match File::open(self.host(&rel)) {
            Ok(f) => {
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.files.write().unwrap().insert(fh, f);
                reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
            }
            Err(_) => reply.error(Errno::EACCES),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let files = self.files.read().unwrap();
        let Some(f) = files.get(&u64::from(fh)) else {
            return reply.error(Errno::EBADF);
        };
        let mut buf = vec![0u8; size as usize];
        match f.read_at(&mut buf, offset) {
            Ok(n) => reply.data(&buf[..n]),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.files.write().unwrap().remove(&u64::from(fh));
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(rel) = self.rel_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Ok(rd) = std::fs::read_dir(self.host(&rel)) else {
            return reply.error(Errno::ENOENT);
        };
        for (i, ent) in rd.flatten().enumerate() {
            if (i as u64) < offset {
                continue;
            }
            let name = ent.file_name();
            let crel = rel.join(&name);
            let cino = self.ino_for(&crel);
            let kind = ent
                .file_type()
                .map(|t| {
                    if t.is_dir() {
                        FileType::Directory
                    } else if t.is_symlink() {
                        FileType::Symlink
                    } else {
                        FileType::RegularFile
                    }
                })
                .unwrap_or(FileType::RegularFile);
            if reply.add(INodeNo(cino), (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let Some(rel) = self.rel_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Ok(rd) = std::fs::read_dir(self.host(&rel)) else {
            return reply.error(Errno::ENOENT);
        };
        for (i, ent) in rd.flatten().enumerate() {
            if (i as u64) < offset {
                continue;
            }
            let name = ent.file_name();
            let crel = rel.join(&name);
            let cino = self.ino_for(&crel);
            let Some(attr) = self.attr_of(cino, &self.host(&crel)) else {
                continue;
            };
            if reply.add(INodeNo(cino), (i + 1) as u64, &name, &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }
}

// m2 `serve` mode: the control socket at the instance's namespaced path,
// speaking the Python ChannelServer's protocol (single-instance guard, ui
// verbs over on-disk box discovery, subscribe event feed). No boxes yet —
// register is refused politely; the overlay arrives at m3.
static SOCK_FOR_SIGNAL: std::sync::OnceLock<std::ffi::CString> =
    std::sync::OnceLock::new();

extern "C" fn on_term(_sig: i32) {
    // async-signal-safe teardown: drop the socket, exit clean.
    if let Some(p) = SOCK_FOR_SIGNAL.get() {
        unsafe { libc::unlink(p.as_ptr()) };
    }
    unsafe { libc::_exit(0) };
}

fn serve() -> i32 {
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("sarun-engine: cannot create instance dirs: {e}");
        return 1;
    }
    let sock = paths::sock_path();
    // Single-instance guard, same semantics as the Python engine/UI: a live
    // socket means an instance is running; a dead file is stale and replaced.
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        eprintln!("sarun-engine: an engine/UI is already running \
                   (control socket {}).", sock.display());
        return 4;
    }
    let c = std::ffi::CString::new(sock.as_os_str().as_encoded_bytes()).unwrap();
    let _ = SOCK_FOR_SIGNAL.set(c);
    unsafe {
        libc::signal(libc::SIGTERM, on_term as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as libc::sighandler_t);
    }
    // Mount the multi-box overlay at the instance mountpoint (threads = cores).
    let mnt = paths::mnt_point();
    let ov = overlay::Overlay::new(PathBuf::from("/"));
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::FSName("sarun-rs".into())];
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cfg.n_threads = Some(n);
    cfg.clone_fd = n > 1;
    let session = match fuser::spawn_mount2(ov.clone(),
                                            &mnt, &cfg) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("sarun-engine: overlay mount FAILED: {e} — boxes cannot run");
            None
        }
    };
    let state: control::State = Default::default();
    state.lock().unwrap().overlay = Some(ov.clone());
    println!("sarun-engine: listening · {}  ·  overlay {}",
             sock.display(), mnt.display());
    let rc = match control::serve(state, &sock) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sarun-engine: serve failed: {e}");
            1
        }
    };
    drop(session); // unmount
    rc
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("serve") => std::process::exit(serve()),
        Some("run") => {
            // run [NAME] -- CMD...
            let rest = &argv[1..];
            let sep = rest.iter().position(|a| a == "--");
            let (pre, cmd) = match sep {
                Some(i) => (&rest[..i], rest[i + 1..].to_vec()),
                None => (rest, vec![]),
            };
            let name = pre.first().cloned();
            std::process::exit(runner::run(name, cmd));
        }
        Some("inner") => {
            // inner --conn-fd N -- CMD...
            let rest = &argv[1..];
            let mut conn_fd = -1;
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "--conn-fd" && i + 1 < rest.len() {
                    conn_fd = rest[i + 1].parse().unwrap_or(-1); i += 2;
                } else if rest[i] == "--" { i += 1; break; }
                else { i += 1; }
            }
            std::process::exit(runner::inner(conn_fd, rest[i..].to_vec()));
        }
        _ => {}
    }
    let mut args = std::env::args().skip(1);
    let mut mountpoint = None;
    let mut lower = PathBuf::from("/");
    let mut threads = 1usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--lower" => lower = PathBuf::from(args.next().expect("--lower PATH")),
            "--threads" => threads = args.next().expect("--threads N").parse().unwrap(),
            other => mountpoint = Some(PathBuf::from(other)),
        }
    }
    let mountpoint = mountpoint.expect("usage: sarun-engine MOUNTPOINT [--lower /] [--threads N]");
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::FSName("sarun-rs".into())];
    cfg.n_threads = Some(threads);
    cfg.clone_fd = threads > 1;
    eprintln!("sarun-engine m1: lower={} threads={} at {}", lower.display(), threads,
              mountpoint.display());
    fuser::mount2(Passthrough::new(lower), &mountpoint, &cfg).expect("mount failed");
}
