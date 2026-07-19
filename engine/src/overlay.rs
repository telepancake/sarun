// The multi-box copy-on-write overlay (m3a). One FUSE mount; the synthetic
// root lists one <box_id> subdir per registered box; <mnt>/<box_id>/rel is a
// merged view of lower (the host) plus that box's captured upper. Reads fall
// through to the host; the box's writes are captured per DESIGN.md:
//   D3 — capture is LAZY: a writable open costs nothing and serves from the
//        lower file; the FIRST actual write triggers copy-up (+ row +
//        provenance) and from then on writes are ordinary pwrites to the blob.
//   D4 — every non-empty file's bytes live as a pool blob (data-NULL row);
//        a box is at rest the moment it stops — no consolidate phase.
// Implemented: lookup/getattr/readdir(plus)/readlink/open/create/read/write/
// truncate/mkdir/unlink/rmdir/symlink/rename.

use crate::depot::BoxDepot;
use virtiofsd::soft_idmap::Id as _;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::ffi::OsStrExt;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;


use crate::capture::BoxState;
use crate::capture::Entry;
use crate::depot::blob_path;
use crate::sarunfs::layers::{ChainLink, Layer};
use crate::sarunfs::synthetic::{SyntheticNode, SyntheticRuntime};
use crate::sarunfs::{HandleTable, NodeAttr, NodeKind};

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

fn kind_of_mode(mode: u32) -> NodeKind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => NodeKind::Directory,
        libc::S_IFLNK => NodeKind::Symlink,
        libc::S_IFCHR => NodeKind::CharDevice,
        libc::S_IFBLK => NodeKind::BlockDevice,
        libc::S_IFIFO => NodeKind::NamedPipe,
        libc::S_IFSOCK => NodeKind::Socket,
        _ => NodeKind::RegularFile,
    }
}

/// (box_id, rel) — "" rel is the box root; box_id 0 is the synthetic mount root.
type Key = crate::sarunfs::NodeKey;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Errno(i32);

impl Errno {
    const EACCES: Self = Self(libc::EACCES);
    const EAGAIN: Self = Self(libc::EAGAIN);
    const EBADF: Self = Self(libc::EBADF);
    const EEXIST: Self = Self(libc::EEXIST);
    const EFBIG: Self = Self(libc::EFBIG);
    const EINVAL: Self = Self(libc::EINVAL);
    const EIO: Self = Self(libc::EIO);
    const EISDIR: Self = Self(libc::EISDIR);
    const ENODATA: Self = Self(libc::ENODATA);
    const ENOENT: Self = Self(libc::ENOENT);
    const ENOSYS: Self = Self(libc::ENOSYS);
    const ENOTDIR: Self = Self(libc::ENOTDIR);
    const ENOTEMPTY: Self = Self(libc::ENOTEMPTY);
    const EPERM: Self = Self(libc::EPERM);
    const EROFS: Self = Self(libc::EROFS);
    const ESPIPE: Self = Self(libc::ESPIPE);
    const EOVERFLOW: Self = Self(libc::EOVERFLOW);
    const EXDEV: Self = Self(libc::EXDEV);
}

impl From<std::io::Error> for Errno {
    fn from(error: std::io::Error) -> Self {
        Self(error.raw_os_error().unwrap_or(libc::EIO))
    }
}

impl From<Errno> for i32 {
    fn from(error: Errno) -> Self {
        error.0
    }
}

/// Clone-able handle: each transport owns one clone as its filesystem, while
/// the control plane holds another to add/remove boxes. All state is shared.
#[derive(Clone)]
pub struct SarunFs {
    inner: Arc<Inner>,
    root: Key,
    kernel_passthrough: Arc<std::sync::atomic::AtomicBool>,
    host_request_pids: bool,
}

/// Transitional source-compatible name for control-plane callers.  Filesystem
/// policy lives in `SarunFs`; transports must not grow another implementation.
pub type Overlay = SarunFs;

struct Inner {
    backing: crate::sarunfs::backing::BackingStore,
    boxes: RwLock<BTreeMap<i64, Arc<BoxState>>>,
    synthetic: SyntheticRuntime,
    inodes: crate::sarunfs::InodeTable,
    detached_attrs: RwLock<HashMap<u64, NodeAttr>>,
    handles: HandleTable<Handle>,
    // Live ExtAttachment objects per owning box (RoAttachment::Ext rows,
    // in list order). Constructed lazily and WITHOUT I/O (attach.rs
    // opens the store on first entry/blob use), so hydration cost stays
    // O(bookkeeping) no matter the store size. Invalidated when a box's
    // attachment list is rewritten.
    ext: RwLock<HashMap<i64, Arc<Vec<Arc<crate::attach::ExtAttachment>>>>>,
    // The §7 materialization cache: real pool files for attachment blobs
    // a readout hands back as Bytes (open/mmap/exec need an fd). Root is
    // state_home()/cache — is_engine_path already hides the whole
    // state_home subtree from boxes. None = open failed (served EIO).
    cache: std::sync::OnceLock<Option<depot_cache::Cache>>,
    rules: RwLock<crate::rules::Rules>,  // passthrough decisions (reload verb)
    /// Lazy shadowing for -b boxes: at lookup/open time, if the
    /// box-relative path matches one of the compiled glob patterns,
    /// the FUSE layer serves `self_exe` (the engine binary) instead
    /// of the host file. NO pre-enumeration of the host filesystem —
    /// matching is per-lookup, the way the user asked for it.
    shadows: RwLock<Shadows>,
    // D5 (rule-gated): true iff the kernel negotiated FUSE_PASSTHROUGH at init.
    // ONLY read-only opens of `readonly`-RULED paths register backing fds; never
    // a blind per-open guess (see DESIGN.md D5). daemon_reads counts read() ops
    // the daemon served (test observability: stays ~0 for passthrough'd reads).
    passthrough_ok: std::sync::atomic::AtomicBool,
    daemon_reads: AtomicU64,
    mutations: crate::sarunfs::mutation::MutationJournal,
    locks: LockTable,
}

struct Fh {
    inner: FhInner,
    lock_identity: LockIdentity,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LockIdentity {
    Native(u64, u64),
    Lower(u64),
    Synthetic(u64),
}

#[derive(Clone, Copy)]
struct RecordLock {
    owner: u64,
    start: u64,
    end: u64,
    type_: u32,
    pid: u32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct LockKey {
    file: LockIdentity,
    flock: bool,
}

struct LockTable {
    state: Mutex<HashMap<LockKey, Vec<RecordLock>>>,
    changed: Condvar,
}

impl LockTable {
    fn new() -> Self {
        Self { state: Mutex::new(HashMap::new()), changed: Condvar::new() }
    }

    fn conflict(records: &[RecordLock], owner: u64,
                requested: &virtiofsd::fuse::FileLock) -> Option<RecordLock> {
        records.iter().copied().find(|record| {
            record.owner != owner
                && record.start <= requested.end
                && requested.start <= record.end
                && (record.type_ == libc::F_WRLCK as u32
                    || requested.type_ == libc::F_WRLCK as u32)
        })
    }

    fn get(&self, key: LockKey, owner: u64,
           requested: virtiofsd::fuse::FileLock) -> virtiofsd::fuse::FileLock {
        let state = self.state.lock().unwrap();
        Self::conflict(state.get(&key).map(Vec::as_slice).unwrap_or(&[]),
                       owner, &requested)
            .map(|record| virtiofsd::fuse::FileLock {
                start: record.start,
                end: record.end,
                type_: record.type_,
                pid: record.pid,
            })
            .unwrap_or(virtiofsd::fuse::FileLock {
                type_: libc::F_UNLCK as u32,
                ..requested
            })
    }

    fn set(&self, key: LockKey, owner: u64,
           requested: virtiofsd::fuse::FileLock, blocking: bool)
           -> Result<(), Errno> {
        if requested.start > requested.end
            || !matches!(requested.type_, x if x == libc::F_RDLCK as u32
                                      || x == libc::F_WRLCK as u32
                                      || x == libc::F_UNLCK as u32)
        {
            return Err(Errno::EINVAL);
        }
        let mut state = self.state.lock().unwrap();
        while requested.type_ != libc::F_UNLCK as u32
            && Self::conflict(state.get(&key).map(Vec::as_slice).unwrap_or(&[]),
                              owner, &requested).is_some()
        {
            if !blocking { return Err(Errno::EAGAIN); }
            state = self.changed.wait(state).unwrap();
        }
        let records = state.entry(key).or_default();
        let mut replacement = Vec::with_capacity(records.len() + 2);
        for record in records.drain(..) {
            if record.owner != owner
                || record.end < requested.start || requested.end < record.start
            {
                replacement.push(record);
                continue;
            }
            if record.start < requested.start {
                replacement.push(RecordLock {
                    end: requested.start - 1,
                    ..record
                });
            }
            if record.end > requested.end {
                replacement.push(RecordLock {
                    start: requested.end + 1,
                    ..record
                });
            }
        }
        if requested.type_ != libc::F_UNLCK as u32 {
            replacement.push(RecordLock {
                owner,
                start: requested.start,
                end: requested.end,
                type_: requested.type_,
                pid: requested.pid,
            });
        }
        *records = replacement;
        if records.is_empty() { state.remove(&key); }
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    fn release(&self, key: LockKey, owner: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(records) = state.get_mut(&key) {
            records.retain(|record| record.owner != owner);
            if records.is_empty() { state.remove(&key); }
        }
        drop(state);
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn record_locks_conflict_split_and_release_by_owner() {
        let table = LockTable::new();
        let key = LockKey { file: LockIdentity::Synthetic(1), flock: false };
        let write = virtiofsd::fuse::FileLock {
            start: 10, end: 19, type_: libc::F_WRLCK as u32, pid: 123,
        };
        table.set(key, 100, write, false).unwrap();
        let conflict = table.get(key, 200, write);
        assert_eq!(conflict.type_, libc::F_WRLCK as u32);
        assert_eq!((conflict.start, conflict.end, conflict.pid), (10, 19, 123));
        assert_eq!(table.set(key, 200, write, false), Err(Errno::EAGAIN));

        table.set(key, 100, virtiofsd::fuse::FileLock {
            start: 13, end: 16, type_: libc::F_UNLCK as u32, pid: 123,
        }, false).unwrap();
        let middle = table.get(key, 200, virtiofsd::fuse::FileLock {
            start: 13, end: 16, type_: libc::F_WRLCK as u32, pid: 200,
        });
        assert_eq!(middle.type_, libc::F_UNLCK as u32);
        assert_eq!(table.get(key, 200, write).start, 10);

        table.release(key, 100);
        assert_eq!(table.get(key, 200, write).type_, libc::F_UNLCK as u32);
    }
}

enum FileData {
    Native(File),
    Lower(crate::sarunfs::backing::BackingFile),
    Sink(i32),
}

enum Handle {
    File(Mutex<Fh>),
    Directory(Arc<Vec<DirNode>>),
    Jobserver { box_id: i64, nonblock: bool },
}

struct FhInner {
    box_id: i64,
    rel: String,
    /// Whether `rel` still names this open file description. Unlink and
    /// rename-over detach the description while its kernel-style handle keeps
    /// the backing inode alive. Detached writes must never recreate or
    /// finalize the vanished pathname.
    linked: bool,
    data: FileData,
    upper: bool,
    dirty: bool,
    last_pid: u32,
    // TGID of the last data writer, resolved AT WRITE TIME (while the writer is
    // still alive). In-process pipeline stages (brush builtins / bundled
    // coreutils) run on a transient worker thread that has already exited by
    // release(); resolving the writer lazily from `last_pid` there would read a
    // vanished /proc/<tid> and mint a phantom, parentless process row. Capturing
    // the long-lived process TGID here attributes the write to the owning
    // process (the brush --inner), which the process forest and the
    // brush↔pipeline linkage (finalize_brush_links) both rely on. 0 until the
    // first data write; release() only consults it when `dirty`, so 0 is safe.
    last_tgid: u32,
    passthrough: bool, // writes go straight to the real host file (uncaptured)
    backing_candidate: bool,
}

#[derive(Clone)]
struct DirNode {
    inode: u64,
    kind: NodeKind,
    name: String,
}

struct OpenedNode {
    handle: u64,
    direct_io: bool,
    nonseekable: bool,
    keep_cache: bool,
    backing_candidate: bool,
}

enum WriteTarget {
    Jobserver { box_id: i64 },
    Sink { box_id: i64, stream: i32 },
    File { file: File, box_id: i64, rel: Option<String> },
}

struct NodeSetattr {
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<SystemTime>,
    mtime: Option<SystemTime>,
}

/// Thread-group id of `pid` from /proc/<pid>/status (so a thread's write is
/// matched against the muted set by its process). Falls back to `pid`.
fn tgid_of(pid: u32) -> u32 {
    if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("Tgid:") {
                if let Ok(v) = rest.trim().parse() { return v; }
            }
        }
    }
    pid
}

/// exe / cwd / argv of `pid` from /proc — the writer provenance a process-
/// scoped file rule (exe:/cwd:/arg:) matches against. Empty fields on any read
/// failure (a never-matching facet, mirroring the Python empty-Subject default).
fn proc_facets(pid: u32) -> (String, String, Vec<String>) {
    let rl = |which: &str| std::fs::read_link(format!("/proc/{pid}/{which}"))
        .ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let exe = rl("exe");
    let cwd = rl("cwd");
    let argv = std::fs::read(format!("/proc/{pid}/cmdline")).ok()
        .map(|b| b.split(|&c| c == 0).filter(|s| !s.is_empty())
                  .map(|s| String::from_utf8_lossy(s).into_owned()).collect())
        .unwrap_or_default();
    (exe, cwd, argv)
}

/// Compiled shadow-pattern globs, loaded from the user's
/// shadow_sh.glob / shadow_make.glob / shadow_ninja.glob in
/// config_home. Empty file → empty patterns → no shadowing.
/// Missing file → historical defaults (matches the old hardcoded
/// shadow set so existing users see no behavior change).
#[derive(Clone, Debug, Default)]
struct Shadows {
    sh: Vec<glob::Pattern>,
    make: Vec<glob::Pattern>,
    ninja: Vec<glob::Pattern>,
    /// The engine binary path on the host. None if current_exe()
    /// failed (very unusual; we can't shadow without it).
    self_exe: Option<PathBuf>,
}

impl Shadows {
    fn load() -> Self {
        let (sh, make, ninja) = shadow_glob_strings();
        Self {
            sh: compile_globs(&sh),
            make: compile_globs(&make),
            ninja: compile_globs(&ninja),
            self_exe: std::env::current_exe().ok(),
        }
    }
}

/// The raw shadow glob pattern strings (sh, make, ninja). They feed the single
/// SarunFs shadow decision, so FUSE, SUD, and QEMU see the same projected
/// executable without any runner-side remap rules.
fn shadow_glob_strings() -> (Vec<String>, Vec<String>, Vec<String>) {
    (
        load_glob_strings(
            &crate::paths::shadow_sh_glob_path(),
            &["/bin/sh", "/usr/bin/sh",
              "/bin/bash", "/usr/bin/bash",
              "/bin/dash", "/usr/bin/dash"]),
        load_glob_strings(
            &crate::paths::shadow_make_glob_path(),
            &["/bin/make", "/usr/bin/make",
              "/bin/gmake", "/usr/bin/gmake"]),
        load_glob_strings(
            &crate::paths::shadow_ninja_glob_path(),
            &["/bin/ninja", "/usr/bin/ninja"]),
    )
}

fn load_glob_strings(file: &std::path::Path, defaults: &[&str])
    -> Vec<String>
{
    match std::fs::read_to_string(file) {
        Ok(s) => s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => defaults.iter().map(|s| (*s).to_string()).collect(),
    }
}

fn compile_globs(raw: &[String]) -> Vec<glob::Pattern> {
    raw.iter().filter_map(|p| {
        // Patterns SHOULD be absolute (the FUSE matcher prepends '/'
        // to the box-relative path). Relative patterns would silently
        // not match anything; loudly skip with a hint instead.
        if !p.starts_with('/') {
            eprintln!("sarun-engine: shadow glob {p:?} ignored \
                       (must be an absolute path; e.g. /bin/sh, \
                       /opt/**/bin/make)");
            return None;
        }
        match glob::Pattern::new(p) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("sarun-engine: shadow glob {p:?} is not a \
                           valid pattern: {e}");
                None
            }
        }
    }).collect()
}

/// The cellulose browser box writes its Chromium profile here (a fixed
/// `--user-data-dir` in the box), so "browser session" is one gloctable dir.
const BROWSER_SESSION_GLOBS: &[&str] = &["/cellulose-profile/**"];

/// A named set of path globs for scoping apply/discard to a subset of a box's
/// changes — "save/discard just the browser session", say, instead of fishing
/// individual files. Same absolute per-line glob format as `shadow_*.glob`.
pub struct FileGroup {
    pub name: String,
    pub patterns: Vec<glob::Pattern>,
}

impl FileGroup {
    /// True when this group selects the (box-relative or absolute) path `rel`.
    pub fn matches(&self, rel: &str) -> bool {
        let abs = if rel.starts_with('/') {
            rel.to_string()
        } else {
            format!("/{rel}")
        };
        self.patterns.iter().any(|p| p.matches(&abs))
    }
}

/// Named file-selection groups: one per `{config_home}/files_<name>.glob` (the
/// `<name>` is the group's display label, underscores → spaces), each a list of
/// absolute path globs like the `shadow_*.glob` files. A built-in "browser
/// session" group (the cellulose profile dir) is appended unless the user
/// defined their own `files_browser_session.glob`.
pub fn file_groups() -> Vec<FileGroup> {
    let mut groups: Vec<FileGroup> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(crate::paths::config_home()) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if let Some(stub) = fname
                .strip_prefix("files_")
                .and_then(|s| s.strip_suffix(".glob"))
            {
                let patterns = compile_globs(&load_glob_strings(&e.path(), &[]));
                if !patterns.is_empty() {
                    groups.push(FileGroup { name: stub.replace('_', " "), patterns });
                }
            }
        }
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    if !groups.iter().any(|g| g.name == "browser session") {
        let raw: Vec<String> = BROWSER_SESSION_GLOBS.iter().map(|s| s.to_string()).collect();
        groups.push(FileGroup { name: "browser session".into(), patterns: compile_globs(&raw) });
    }
    groups
}

#[cfg(test)]
mod file_group_tests {
    use super::*;

    #[test]
    fn browser_session_matches_profile_paths() {
        let g = FileGroup {
            name: "browser session".into(),
            patterns: compile_globs(&["/cellulose-profile/**".to_string()]),
        };
        assert!(g.matches("cellulose-profile/Default/Cookies")); // box-relative
        assert!(g.matches("/cellulose-profile/Local Storage/leveldb/000003.log"));
        assert!(!g.matches("home/user/notes.txt"));
    }

    #[test]
    fn builtin_browser_session_always_present() {
        // With no user files_*.glob in a scratch config home, the built-in
        // group is still offered.
        let groups = file_groups();
        assert!(groups.iter().any(|g| g.name == "browser session"),
                "built-in 'browser session' group must always be available");
    }
}

impl SarunFs {
    /// Drain queued (box_id, rel, op) events out of the overlay — the
    /// control loop calls this on a tick and broadcasts each one as a
    /// type=overlay event to subscribers. The mutation journal bounds itself
    /// under a write storm before this consumer drains it.
    pub fn drain_events(&self) -> Vec<(i64, String, &'static str)> {
        self.inner.mutations.drain()
    }

    pub fn new(lower: PathBuf) -> Self {
        let backing = crate::sarunfs::backing::BackingStore::new(lower.clone())
            .unwrap_or_else(|error| {
                panic!("cannot initialize upstream backing {}: {error}", lower.display())
            });
        let ov = SarunFs {
            inner: Arc::new(Inner {
                backing,
                boxes: RwLock::new(BTreeMap::new()),
                synthetic: SyntheticRuntime::new(),
                inodes: crate::sarunfs::InodeTable::new((0, String::new())),
                detached_attrs: RwLock::new(HashMap::new()),
                handles: HandleTable::new(),
                ext: RwLock::new(HashMap::new()),
                cache: std::sync::OnceLock::new(),
                rules: RwLock::new(crate::rules::Rules::load()),
                passthrough_ok: std::sync::atomic::AtomicBool::new(false),
                daemon_reads: AtomicU64::new(0),
                mutations: crate::sarunfs::mutation::MutationJournal::new(),
                locks: LockTable::new(),
                shadows: RwLock::new(Shadows::load()),
            }),
            root: (0, String::new()),
            kernel_passthrough: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            host_request_pids: true,
        };
        // Reclaim unreferenced cache pool files once per engine start —
        // cheap (one readdir sweep), and NEVER on the FUSE path: eviction
        // under a live open() race would yank a pool file an mmap still
        // needs; at startup nothing holds cache fds yet.
        if let Some(c) = ov.cache() {
            let _ = c.evict_unreferenced();
        }
        // Test observability: if SARUN_STATS_FILE is set, a thread writes
        // "passthrough=<0|1> daemon_reads=<n>" to it (survives the SIGTERM
        // _exit teardown, which skips destroy()). No-op when unset.
        if let Ok(path) = std::env::var("SARUN_STATS_FILE") {
            let inner = ov.inner.clone();
            std::thread::spawn(move || loop {
                let line = format!("passthrough={} daemon_reads={}\n",
                    inner.passthrough_ok.load(Ordering::Relaxed) as u8,
                    inner.daemon_reads.load(Ordering::Relaxed));
                let _ = std::fs::write(&path, line);
                std::thread::sleep(Duration::from_millis(100));
            });
        }
        ov
    }

    /// A transport view whose protocol inode 1 is one live box's merged root.
    /// The view shares all filesystem policy, handles, capture state, and inode
    /// allocation with the host FUSE view; only the protocol root is scoped.
    pub fn export_box(&self, box_id: i64) -> std::io::Result<Self> {
        self.box_of(box_id)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok(Self {
            inner: self.inner.clone(),
            root: (box_id, String::new()),
            kernel_passthrough: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            host_request_pids: false,
        })
    }

    /// Export an already-open protocol handle as a kernel descriptor. This is
    /// deliberately narrower than a second filesystem API: path resolution,
    /// open policy, and capture have already happened through canonical FUSE.
    /// Linux consumers such as exec and mmap receive only the resulting
    /// backing object.
    pub(crate) fn export_handle(
        &self,
        handle: u64,
        writable: bool,
        caller_pid: u32,
    ) -> std::io::Result<File> {
        if writable {
            return match self.prepare_write_node(caller_pid, handle).map_err(virtio_error)? {
                WriteTarget::File { file, .. } => Ok(file),
                WriteTarget::Jobserver { .. } | WriteTarget::Sink { .. } =>
                    Err(std::io::Error::from_raw_os_error(libc::ENODEV)),
            };
        }

        let handle = self.inner.handles.get(handle)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?;
        let Handle::File(handle) = &*handle else {
            return Err(std::io::Error::from_raw_os_error(libc::EBADF));
        };
        let handle = handle.lock().unwrap();
        match &handle.inner.data {
            FileData::Native(file) => file.try_clone(),
            FileData::Lower(lower) => {
                let staging = staging_file()?;
                let mut offset = 0u64;
                let mut buffer = vec![0u8; 1024 * 1024];
                loop {
                    let read = lower.read_at(&mut buffer, offset)?;
                    if read == 0 { break; }
                    let mut written = 0;
                    while written < read {
                        let count = staging.write_at(&buffer[written..read],
                                                     offset + written as u64)?;
                        if count == 0 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "cannot materialize SUD backing object"));
                        }
                        written += count;
                    }
                    offset += read as u64;
                }
                Ok(staging)
            }
            FileData::Sink(_) => Err(std::io::Error::from_raw_os_error(libc::ENODEV)),
        }
    }


    /// Reload the shadow_sh / shadow_make / shadow_ninja globs from
    /// disk. Called by the `reload_rules` control verb (so the user
    /// edits all three families together with one RPC) AND on first
    /// box-attach so a fresh edit lands without an engine restart.
    pub fn reload_shadows(&self) {
        *self.inner.shadows.write().unwrap() = Shadows::load();
    }

    /// True if `rel` (a box-relative path, no leading '/') matches
    /// any compiled shadow pattern. Caller gates on b.is_brush().
    /// Cheap (pattern.matches_str over a small Vec) — no filesystem
    /// access. Used by both lookup/getattr and open to decide whether
    /// to serve the engine binary in place of the host file.
    fn shadow_matches(&self, rel: &str) -> bool {
        let full = if rel.starts_with('/') { rel.to_string() }
                   else { format!("/{rel}") };
        let p = std::path::Path::new(&full);
        let s = self.inner.shadows.read().unwrap();
        s.sh.iter().any(|pat| pat.matches_path(p))
            || s.make.iter().any(|pat| pat.matches_path(p))
            || s.ninja.iter().any(|pat| pat.matches_path(p))
    }

    /// The host path of the architecture-matching engine binary served as the
    /// shadow target. Native FUSE/SUD use this process's executable; a QEMU box
    /// uses its cached target `/init`, so a cross-architecture guest never sees
    /// a host-architecture make/shell/ninja projection.
    fn shadow_target_path(&self, b: &BoxState) -> Option<PathBuf> {
        if let Some(architecture) = b.get_meta("qemu_architecture") {
            let architecture = match architecture.as_str() {
                "aarch64" => crate::generated_wire::QemuArchitecture::Aarch64,
                "x86_64" => crate::generated_wire::QemuArchitecture::X8664,
                _ => return None,
            };
            return Some(crate::appliance::target_init(architecture));
        }
        let s = self.inner.shadows.read().unwrap();
        s.self_exe.clone()
    }

    /// True when `rel` is the box's view of the HOST oaita config file
    /// (the path computed by `paths::oaita_config_path()` with its leading
    /// `/` stripped). For `--api` boxes the overlay substitutes the safe
    /// pre-generated toml at this path: the box never sees the host's
    /// api_key or its real upstream URL.
    fn matches_host_oaita_config(rel: &str) -> bool {
        let host = crate::paths::oaita_config_path();
        let s = host.to_string_lossy();
        let stripped = s.strip_prefix('/').unwrap_or(&s);
        rel == stripped
    }

    /// True when `rel` is one of the engine's MITM-CA shadow targets —
    /// the canonical system CA bundle paths the runner USED to bwrap-
    /// bind into the box. For `--api` boxes the overlay now serves
    /// these from the engine's pre-written augmented bundle instead.
    fn matches_api_box_ca_target(rel: &str) -> bool {
        for tgt in crate::runner::CA_BUNDLE_TARGETS {
            let s = tgt.strip_prefix('/').unwrap_or(tgt);
            if rel == s { return true; }
        }
        false
    }

    /// True when `rel` is the substituted oaita.toml OR one of its ancestor
    /// directories — so that the self-hide gate keeps the path TO the file
    /// reachable even when the parent dirs (config_home) are otherwise
    /// hidden. Used by lookup/readdir exemption for `--api` boxes.
    fn oaita_config_ancestor_or_self(rel: &str) -> bool {
        let host = crate::paths::oaita_config_path();
        let s = host.to_string_lossy();
        let stripped = s.strip_prefix('/').unwrap_or(&s);
        if rel == stripped { return true; }
        // rel is a strict directory ancestor of the safe toml path.
        // Compare component-wise: `stripped` starts with `rel/`.
        stripped.starts_with(&format!("{rel}/"))
    }

    /// True when `rel` is the in-box oaita state dir, an ancestor of it,
    /// or WITHIN it — the path tree the in-box oaita CLI needs writable
    /// to persist its own session folders. `--api` boxes use this to
    /// punch through the otherwise-blanket self-hide of state_home so the
    /// in-box `oaita add` / `oaita run` can write to the natural XDG
    /// location without colliding with host sessions (each box gets its
    /// own copy via the overlay — host sessions remain hidden through
    /// the rest of state_home).
    fn oaita_state_ancestor_self_or_within(rel: &str) -> bool {
        let host = crate::paths::oaita_state_home();
        let s = host.to_string_lossy();
        let stripped = s.strip_prefix('/').unwrap_or(&s);
        if rel == stripped { return true; }
        // ancestor of the dir
        if stripped.starts_with(&format!("{rel}/")) { return true; }
        // descendant of the dir
        rel.starts_with(&format!("{stripped}/"))
    }

    /// True when `rel` falls inside one of sarun's OWN host directories —
    /// data_home / config_home / state_home / runtime_home. Those hold the
    /// engine's runtime artifacts (sqlar files, sockets, blobs, the CA
    /// bundle, the per-box pool, oaita.toml) and have no business being
    /// visible to a sandboxed box: visibility there lets the box re-enter
    /// its own overlay, read other boxes' sqlars, lift the api_key, or get
    /// its own writes captured as "changes" when something accidentally
    /// touches a path it has via passthrough. Hiding is component-wise:
    /// any rel inside or equal to one of the four roots is treated as
    /// absent. Mirrors the Python prototype's behaviour (commit 9e9138b).
    ///
    /// Note: --api `oaita.toml` substitution is checked BEFORE this hide,
    /// so the substituted safe toml is still served to api boxes even
    /// though config_home is otherwise hidden.
    fn is_engine_path(rel: &str) -> bool {
        // Cache the (already-stripped) roots so the per-lookup cost is
        // just a handful of string compares against `rel`. The runner
        // doesn't bind anything in-box anymore (the inner shim exec's
        // from /proc/self/fd/N) so there's no /run/sarun path to
        // consider here.
        use std::sync::OnceLock;
        static ROOTS: OnceLock<Vec<String>> = OnceLock::new();
        let roots = ROOTS.get_or_init(|| {
            [crate::paths::data_home(),
             crate::paths::config_home(),
             crate::paths::state_home(),
             crate::paths::runtime_home()]
                .iter()
                .map(|p| {
                    let s = p.to_string_lossy().into_owned();
                    s.strip_prefix('/').unwrap_or(&s).to_string()
                })
                .filter(|s| !s.is_empty())
                .collect()
        });
        for r in roots {
            if rel == r { return true; }
            // subtree match — `rel` starts with `r/`
            if rel.len() > r.len() + 1
                && rel.starts_with(r)
                && rel.as_bytes()[r.len()] == b'/'
            {
                return true;
            }
        }
        false
    }

    pub fn reload_rules(&self) {
        // One reload covers both rules.txt AND the three shadow_*.glob
        // files — they're all "things on disk the user can edit while
        // the engine is running". One control verb, two reloads.
        self.reload_shadows();
        *self.inner.rules.write().unwrap() = crate::rules::Rules::load();
    }

    // ── echo mux + mute (called from control's box-channel thread) ──
    /// Attach the box's muxed connection as its echo writer (the sink-write
    /// handler frames ECHO onto it). Replaces any prior writer for the box.
    pub fn set_echo(&self, id: i64,
                    conn: std::sync::Arc<Mutex<std::os::unix::net::UnixStream>>) {
        self.inner.synthetic.set_echo(id, conn);
    }
    /// Drop the box's echo writer (box channel closing / teardown).
    pub fn clear_echo(&self, id: i64) {
        self.inner.synthetic.clear_echo(id);
    }
    /// The box-channel writer stored under id (set by control::handle as the
    /// echo conn). Reused by the oaita API mux to frame FRAME_API_DATA
    /// responses back over the same channel — no second control conn.
    pub fn echo_writer(&self, id: i64)
        -> Option<std::sync::Arc<Mutex<std::os::unix::net::UnixStream>>>
    {
        self.inner.synthetic.echo_writer(id)
    }
    pub fn mute_add(&self, host_pid: i32, box_id: i64) {
        self.inner.synthetic.mute_add(host_pid, box_id);
    }
    pub fn mute_remove(&self, host_pid: i32) {
        self.inner.synthetic.mute_remove(host_pid);
    }

    /// HOST-DIRECT WRITE routing decision: the FULL-grammar passthrough rule,
    /// scoped by the box display name and the writing process's provenance
    /// (exe/cwd/argv). A rule like `passthrough *.key and exe:gpg` therefore
    /// only routes gpg's writes host-direct. The box/proc facets are resolved
    /// only when some rule actually uses them (the common path-only case stays
    /// fast — empty Subject, no /proc or discover work).
    fn is_passthrough(&self, rel: &str, bid: i64, pid: u32) -> bool {
        let rules = self.inner.rules.read().unwrap();
        if !rules.needs_box() && !rules.needs_proc() {
            // common case: no box/proc clauses → path-only Subject suffices.
            return matches!(rules.decide(rel, &crate::rules::Subject::default()),
                            Some(crate::rules::Action::Passthrough));
        }
        let subject = self.writer_subject(bid, pid, rules.needs_box(),
                                          rules.needs_proc());
        matches!(rules.decide(rel, &subject),
                 Some(crate::rules::Action::Passthrough))
    }

    /// D5 kernel-READ-passthrough gate: PATH-ONLY passthrough match only, so a
    /// box-/proc-scoped passthrough never enables the captured-here-but-
    /// passthrough-there read divergence (see DESIGN.md D5 / rules.rs).
    fn is_passthrough_read(&self, rel: &str) -> bool {
        self.inner.rules.read().unwrap().passthrough_path_only(rel)
    }

    /// Build the rule Subject for the WRITING process `pid` in box `bid`: the
    /// box display name (when a rule needs it) and the live process's exe/cwd/
    /// argv read from /proc (when a rule needs them). Self-contained — no box
    /// state required beyond the id.
    fn writer_subject(&self, bid: i64, pid: u32, want_box: bool, want_proc: bool)
        -> crate::rules::Subject
    {
        let mut s = crate::rules::Subject::default();
        if want_box {
            s.box_name = crate::discover::display_path(&crate::discover::discover(), bid);
        }
        if want_proc {
            let (exe, cwd, argv) = proc_facets(pid);
            s.exe = exe; s.cwd = cwd; s.argv = argv;
        }
        s
    }

    pub fn add_box(&self, b: Arc<BoxState>) {
        self.inner.mutations.attach_box(&b);
        let parent = b.parent();
        let bid_of_added = b.id;
        self.inner.boxes.write().unwrap().insert(b.id, b);
        // Hydrate any at-rest parent chain into the overlay's live box map so
        // resolve()/scan_dir() can WALK INTO the ancestors during the child's
        // FUSE ops. Without this, a child whose parent is an at-rest box (e.g.
        // an OCI image layer created by `sarun oci load`) would see the chain
        // truncate at its own contents — every read past its own entries would
        // fall through to host (or Absent under no_host_fallback), missing
        // every layer below. Idempotent — already-loaded ancestors are kept.
        self.hydrate_chain(parent);
        for ro in self.box_of(bid_of_added).map(|b| b.ro_attachment_box_ids())
            .unwrap_or_default()
        {
            self.hydrate_chain(Some(ro));
        }
    }

    /// Open + load-mirror each at-rest box up the parent chain rooted at
    /// `start`, adding to `self.boxes` (under the same lock discipline as
    /// add_box). Stops on missing sqlar, on a cycle, or after 64 hops.
    fn hydrate_chain(&self, start: Option<i64>) {
        let mut work: Vec<i64> = start.into_iter().collect();
        let mut seen = std::collections::HashSet::new();
        let mut hops = 0;
        while let Some(id) = work.pop() {
            hops += 1;
            if hops > 64 || !seen.insert(id) { continue; }
            if let Some(b) = self.inner.boxes.read().unwrap().get(&id) {
                // already live — but its parents/attachments may need work.
                work.extend(b.parent());
                work.extend(b.ro_attachment_box_ids());
                continue;
            }
            // Open the at-rest sqlar; `BoxState::create` is a CREATE-IF-NOT-
            // EXISTS open + schema upsert (additive), so on an existing
            // sqlar it just rebinds. load_mirror() then populates `kinds`
            // and restores the parent-stack mode flags + RO attachments
            // from meta (an attachment is itself a chain root to hydrate).
            match BoxState::create(id) {
                Ok(pb) => {
                    pb.load_mirror();
                    work.extend(pb.parent());
                    work.extend(pb.ro_attachment_box_ids());
                    self.inner.boxes.write().unwrap()
                        .insert(pb.id, Arc::new(pb));
                }
                Err(_) => continue,
            }
        }
    }

    /// The live ExtAttachment list for `owner`'s RoAttachment::Ext rows,
    /// constructed on first ask (no store I/O — see attach.rs). Shared
    /// Arc so repeated chain walks reuse the same memos/opens.
    pub(crate) fn ext_attachments(&self, owner: i64)
        -> Arc<Vec<Arc<crate::attach::ExtAttachment>>>
    {
        if let Some(v) = self.inner.ext.read().unwrap().get(&owner) {
            return v.clone();
        }
        let built: Vec<Arc<crate::attach::ExtAttachment>> =
            self.box_of(owner).map(|b| b.ro_attachment_list()).unwrap_or_default()
                .into_iter()
                .filter_map(|r| match r {
                    crate::capture::RoAttachment::Ext(e) =>
                        Some(Arc::new(crate::attach::ExtAttachment::new(e))),
                    crate::capture::RoAttachment::Box(_) => None,
                })
                .collect();
        let built = Arc::new(built);
        self.inner.ext.write().unwrap().insert(owner, built.clone());
        built
    }

    /// The materialization cache, opened once per engine. None (with a
    /// one-time log line) when the root can't be created; blob-backed
    /// attachment opens then fail with EIO rather than bricking the box.
    fn cache(&self) -> Option<&depot_cache::Cache> {
        self.inner.cache.get_or_init(|| {
            let root = crate::paths::state_home().join("cache");
            match depot_cache::Cache::open(root) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("sarun-engine: depot cache unavailable: {e}");
                    None
                }
            }
        }).as_ref()
    }

    /// Open errors of `owner`'s LIVE ext attachments, keyed by name.
    /// Reads only what is already built/opened — never triggers a
    /// build or a store open (the session list must stay lazy).
    pub(crate) fn ext_errors(&self, owner: i64)
        -> HashMap<String, String>
    {
        self.inner.ext.read().unwrap().get(&owner)
            .map(|v| v.iter()
                .filter_map(|a| a.error().map(|e| (a.ext.name.clone(), e)))
                .collect())
            .unwrap_or_default()
    }

    /// Drop `owner`'s cached ExtAttachments — call after rewriting its
    /// attachment list so the next walk rebuilds from bookkeeping.
    pub(crate) fn invalidate_ext(&self, owner: i64) {
        self.inner.ext.write().unwrap().remove(&owner);
    }

    pub fn remove_box(&self, id: i64) {
        self.inner.boxes.write().unwrap().remove(&id);
        self.inner.synthetic.remove_box(id);
    }

    /// Project a host file read-only at `rel` in one live box. Projections are
    /// filesystem presentation state, not overlay mutations, and disappear
    /// with the box.
    pub fn project_file(&self, id: i64, rel: &str, source: PathBuf)
        -> std::io::Result<()>
    {
        if !source.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("projected file {} does not exist", source.display()),
            ));
        }
        if !self.inner.boxes.read().unwrap().contains_key(&id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("box {id} not registered"),
            ));
        }
        self.inner.synthetic.project(id, rel, source);
        Ok(())
    }

    fn projected_file(&self, id: i64, rel: &str) -> Option<PathBuf> {
        self.inner.synthetic.projected(id, rel)
    }

    pub fn box_ids(&self) -> Vec<i64> {
        self.inner.boxes.read().unwrap().keys().copied().collect()
    }

    // ── in-engine file ops, served via control verbs ────────────────────────
    //
    // These wrap the same resolve/copy_up machinery FUSE uses, so a tool that
    // calls them sees and writes the same merged box view a shell-inside-the-
    // box would see. `hydrate_chain` makes them work for AT-REST boxes too —
    // the box can be sleeping (no runner holding it) and its sqlar upper +
    // host-fall-through lower stay reachable. Same pattern nested-box
    // construction uses; the FUSE mount is just one consumer of this layer.

    /// Read the file at `rel` in box `bid`'s merged view as raw bytes.
    /// Hydrates the box + its parent chain on demand.
    pub fn box_read_file(&self, bid: i64, rel: &str) -> std::io::Result<Vec<u8>> {
        self.hydrate_chain(Some(bid));
        if let Some(path) = self.projected_file(bid, rel) {
            return std::fs::read(path);
        }
        match self.resolve(bid, rel) {
            Layer::UpperFile { owner, rowid, .. } => {
                // Restore an inline (discard-reverted) row to its blob so this
                // host-side read sees the reverted bytes, exactly as the FUSE
                // read path does. See ensure_upper_blob.
                self.ensure_upper_blob(owner, rowid, rel);
                std::fs::read(crate::depot::blob_path(owner, rowid))
            }
            Layer::Lower => self.inner.backing.read_all(rel),
            Layer::ExtFile { att, rel, .. } => match att.blob(&rel) {
                Some(depot_model::variant::Blob::Bytes(b)) => Ok(b),
                Some(depot_model::variant::Blob::File(p)) => std::fs::read(p),
                // Resolved entry but no blob: the store went away between
                // getattr and read (§8 failure mode) — EIO, never a panic.
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::Other, "attachment blob unavailable")),
            },
            Layer::UpperSymlink { target } =>
                Ok(target.to_string_lossy().into_owned().into_bytes()),
            Layer::UpperDir { .. } => Err(std::io::Error::new(
                std::io::ErrorKind::Other, "is a directory")),
            Layer::UpperSpecial { .. } => Err(std::io::Error::new(
                std::io::ErrorKind::Other, "special file")),
            Layer::Absent => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound, "not found")),
        }
    }

    /// The permission bits of the file at `rel` in box `bid`'s merged view, or
    /// None if it isn't a regular file. Lets a host-side reader (oci build's
    /// `COPY --from`) preserve a source file's exec bits. Hydrates on demand.
    pub fn box_file_mode(&self, bid: i64, rel: &str) -> Option<u32> {
        use std::os::unix::fs::PermissionsExt;
        self.hydrate_chain(Some(bid));
        if let Some(path) = self.projected_file(bid, rel) {
            return std::fs::metadata(path).ok()
                .map(|metadata| metadata.permissions().mode() & 0o7777);
        }
        match self.resolve(bid, rel) {
            Layer::UpperFile { mode, .. } | Layer::ExtFile { mode, .. } =>
                Some(mode & 0o7777),
            Layer::Lower => self.inner.backing.attr(rel).ok()
                .map(|attr| attr.mode & 0o7777),
            _ => None,
        }
    }

    /// Replace the contents of `rel` in box `bid`'s upper with `bytes`. Stages
    /// the write exactly as a FUSE write would (copy_up → truncate → write →
    /// finalize_file). If the file doesn't exist anywhere, this creates it
    /// in the box's upper; the host is never touched.
    pub fn box_write_file(&self, bid: i64, rel: &str, bytes: &[u8])
        -> std::io::Result<()>
    {
        use std::io::{Seek, SeekFrom, Write};
        self.hydrate_chain(Some(bid));
        if self.ro_denied(bid, rel) {
            return Err(std::io::Error::from_raw_os_error(libc::EROFS));
        }
        let Some(b) = self.box_of(bid) else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound,
                                           format!("box {bid} not registered")));
        };
        // pid=0 — control-RPC writes have no host writer; provenance just
        // records the synthetic 0.
        let mut f = self.copy_up(&b, rel, 0)?;
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(bytes)?;
        f.flush()?;
        let sz = bytes.len() as i64;
        let mtime_ns = SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64).unwrap_or(0);
        self.inner
            .mutations
            .writer(&b, 0, self.host_request_pids)
            .finalize_file(rel, sz, mtime_ns);
        Ok(())
    }

    /// Merged listing of `rel` in box `bid`. Returns (name, kind_char)
    /// where kind_char ∈ 'f' (file), 'd' (dir), 'l' (symlink), 's' (special),
    /// '?' (unknown).
    pub fn box_list_dir(&self, bid: i64, rel: &str)
        -> std::io::Result<Vec<(String, char)>>
    {
        self.hydrate_chain(Some(bid));
        let Some(b) = self.box_of(bid) else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound,
                                           format!("box {bid} not registered")));
        };
        let entries = self.scan_dir(&b, rel, /*plus=*/false);
        Ok(entries.into_iter().map(|(name, kind, _, _)| {
            let c = match kind {
                NodeKind::RegularFile => 'f',
                NodeKind::Directory => 'd',
                NodeKind::Symlink => 'l',
                _ => 's',
            };
            (name, c)
        }).collect())
    }

    /// Kind of `rel` in box `bid` (or '?' when absent). Like a single stat.
    pub fn box_path_kind(&self, bid: i64, rel: &str) -> char {
        self.hydrate_chain(Some(bid));
        match self.resolve(bid, rel) {
            Layer::UpperFile { .. } | Layer::ExtFile { .. } => 'f',
            Layer::UpperDir { .. } => 'd',
            Layer::UpperSymlink { .. } => 'l',
            Layer::UpperSpecial { .. } => 's',
            Layer::Lower => match self.inner.backing.attr(rel).map(|attr| attr.kind) {
                Ok(NodeKind::Symlink) => 'l',
                Ok(NodeKind::Directory) => 'd',
                Ok(NodeKind::RegularFile) => 'f',
                Ok(_) => 's',
                Err(_) => '?',
            },
            Layer::Absent => '?',
        }
    }

    /// Re-parent a live box (dissolve copy-down): point its resolve()/KIDS_DIR
    /// chain at the grandparent (None = top-level). No-op if the box isn't live.
    pub fn set_box_parent(&self, id: i64, parent: Option<i64>) {
        if let Some(b) = self.inner.boxes.read().unwrap().get(&id) {
            b.set_parent(parent);
        }
    }

    /// The live BoxState for `id`, if the box is currently mounted (running).
    /// Used to route writes (dissolve copy-down, meta) through the live
    /// connection + RAM mirror instead of a rival on-disk handle.
    pub fn live_box(&self, id: i64) -> Option<Arc<BoxState>> {
        self.inner.boxes.read().unwrap().get(&id).cloned()
    }

    /// On rename, the kernel keeps the cached inode and moves its dentry; our
    /// ino->key map must follow, for `rel_o` and the whole subtree under it,
    /// or a getattr on the cached inode resolves the stale (now-absent) path.
    fn remap_inode_subtree(&self, bid: i64, rel_o: &str, rel_n: &str) {
        self.inner.inodes.remap_subtree(bid, rel_o, rel_n);
    }

    pub(crate) fn box_of(&self, id: i64) -> Option<Arc<BoxState>> {
        self.inner.boxes.read().unwrap().get(&id).cloned()
    }

    fn key_of(&self, inode: u64) -> Option<Key> {
        if inode == 1 {
            Some(self.root.clone())
        } else {
            self.inner.inodes.key(inode)
        }
    }

    fn ino_for(&self, key: &Key) -> u64 {
        if key == &self.root {
            1
        } else {
            self.inner.inodes.intern(key)
        }
    }

    fn host(&self, rel: &str) -> PathBuf {
        self.inner.backing.direct_path(rel)
    }

    /// A box's OWN layer for `rel` (single level, no parent walk) — used by the
    /// WRITE paths, which operate on the box's own overlay. UpperFile.owner is
    /// the box itself.
    fn layer(&self, b: &BoxState, rel: &str) -> Layer {
        crate::sarunfs::layers::own_layer(
            b,
            rel,
            self.inner.backing.exists(rel),
        )
    }

    /// `b`'s RO attachments as chain links, in LIST order — Box and Ext
    /// rows interleaved exactly as ro_attachments records them (the
    /// per-kind accessors each preserve relative order; the running Ext
    /// index rejoins them). Box rows not currently registered are
    /// skipped, matching the old id-walk's box_of miss behavior.
    fn attachment_links(&self, b: &BoxState, out: &mut Vec<ChainLink>) {
        let exts = self.ext_attachments(b.id);
        if exts.is_empty() {
            // Common case: no Ext rows — skip the (cloning) full-list
            // accessor on the per-op resolve path.
            for ro in b.ro_attachment_box_ids() {
                if let Some(rb) = self.box_of(ro) {
                    out.push(ChainLink::Box(rb));
                }
            }
            return;
        }
        let mut ei = 0;
        for row in b.ro_attachment_list() {
            match row {
                crate::capture::RoAttachment::Box(ro) => {
                    if let Some(rb) = self.box_of(ro) {
                        out.push(ChainLink::Box(rb));
                    }
                }
                crate::capture::RoAttachment::Ext(_) => {
                    if let Some(a) = exts.get(ei) {
                        out.push(ChainLink::Ext(a.clone()));
                    }
                    ei += 1;
                }
            }
        }
    }

    /// The full lookup chain for `bid`: each box followed by its RO
    /// attachments (DEPOT-DESIGN.md §8 — read-only layers conceptually
    /// between a box and its parent), then the parent, recursively.
    /// Capped like the old parent walk.
    fn chain_of(&self, bid: i64) -> Vec<ChainLink> {
        let mut out = Vec::new();
        let mut cur = Some(bid);
        while let Some(id) = cur {
            if out.len() >= 64 { break; }
            let Some(b) = self.box_of(id) else { break };
            cur = b.parent();
            out.push(ChainLink::Box(b.clone()));
            self.attachment_links(&b, &mut out);
            out.truncate(64);
        }
        out
    }

    /// Does any RO attachment in `bid`'s chain match `rel`? Matched keys
    /// are immutable for the running box (EROFS at the mutating call,
    /// checked BEFORE any capture side effect) — the invariant that
    /// keeps the captured layer independent of what was attached.
    pub(crate) fn ro_denied(&self, bid: i64, rel: &str) -> bool {
        if self.projected_file(bid, rel).is_some() {
            return true;
        }
        let mut cur = Some(bid);
        let mut seen = 0;
        while let Some(id) = cur {
            seen += 1;
            if seen > 64 { break; }
            let Some(b) = self.box_of(id) else { break };
            let mut links = Vec::new();
            self.attachment_links(&b, &mut links);
            for l in links {
                let hit = match l {
                    ChainLink::Box(rb) => rb.entry(rel).is_some(),
                    ChainLink::Ext(att) => att.entry(rel).is_some(),
                };
                if hit { return true; }
            }
            cur = b.parent();
        }
        false
    }

    /// The MERGED resolution for `rel` as seen by box `bid`: the box's own entry
    /// if any, else its parent box's overlay (recursively), the root box
    /// bottoming out at the host. A whiteout at any level hides everything
    /// below (parent boxes AND the host). This is the nested read-through-parent
    /// semantic — used by every READ and existence check. UpperFile.owner names
    /// whichever box in the chain actually holds the bytes (for blob_path).
    ///
    /// D-opaque: a box may carry an OPAQUE marker on a directory (OCI/AUFS
    /// `.wh..wh..opq` convention), meaning that directory's lower-layer
    /// contents are wiped when this box appears in the chain. Used by
    /// resolve() (a lookup past this box for `dir/X` returns Absent if `dir`
    /// is opaque here and we have no own entry for it) and by scan_dir()
    /// (the merged listing clears accumulated names at the opaque-marker box
    /// before applying its own present/whiteout contributions).
    fn resolve(&self, bid: i64, rel: &str) -> Layer {
        self.resolve_with_lower_presence(bid, rel, self.inner.backing.exists(rel))
    }

    /// Resolve against an already-probed lower path. Attribute lookup needs
    /// both the merge decision and the lower metadata; passing the result of
    /// that single metadata probe here avoids walking every path component
    /// once for `exists()` and then a second time for `attr()`.
    fn resolve_with_lower_presence(
        &self, bid: i64, rel: &str, lower_exists: bool,
    ) -> Layer {
        let chain = self.chain_of(bid);
        crate::sarunfs::layers::resolve(
            bid,
            rel,
            self.box_of(bid).is_some_and(|origin| origin.is_api()),
            &chain,
            lower_exists,
        )
    }

    fn attr_from_md(&self, ino: u64, md: &std::fs::Metadata) -> NodeAttr {
        NodeAttr {
            inode: ino,
            size: md.size(),
            blocks: md.blocks(),
            atime: ts(md.atime(), md.atime_nsec()),
            mtime: ts(md.mtime(), md.mtime_nsec()),
            ctime: ts(md.ctime(), md.ctime_nsec()),
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

    fn synth_dir_attr(&self, ino: u64, mode: u32, mtime_ns: i64) -> NodeAttr {
        NodeAttr {
            inode: ino, size: 0, blocks: 0,
            atime: ns_ts(mtime_ns), mtime: ns_ts(mtime_ns), ctime: ns_ts(mtime_ns),
            kind: NodeKind::Directory,
            perm: (mode & 0o7777) as u16, nlink: 2, uid: 0, gid: 0, rdev: 0,
            blksize: 512, flags: 0,
        }
    }

    fn synth_file_attr(&self, ino: u64) -> NodeAttr {
        NodeAttr {
            inode: ino, size: 0, blocks: 0,
            atime: UNIX_EPOCH, mtime: UNIX_EPOCH, ctime: UNIX_EPOCH,
            kind: NodeKind::RegularFile,
            perm: 0o666, nlink: 1, uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
        }
    }

    fn synth_link_attr(&self, ino: u64, len: u64) -> NodeAttr {
        NodeAttr {
            inode: ino, size: len, blocks: 0,
            atime: UNIX_EPOCH, mtime: UNIX_EPOCH, ctime: UNIX_EPOCH,
            kind: NodeKind::Symlink,
            perm: 0o777, nlink: 1, uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
        }
    }

    /// Attributes for (box, rel) through the FULL merge (own → parent chain →
    /// host), or None when absent.
    fn attr_of(&self, b: &BoxState, ino: u64, rel: &str) -> Option<NodeAttr> {
        if let Some(path) = self.projected_file(b.id, rel) {
            let mut attr = self.attr_from_md(ino, &std::fs::metadata(path).ok()?);
            attr.kind = NodeKind::RegularFile;
            return Some(attr);
        }
        // The lower probe is required even when an upper layer wins because
        // lower presence participates in the merge decision. Keep its attrs so
        // the overwhelmingly common lower case does not repeat a root-to-leaf
        // PassthroughFsRo traversal after resolving the layer.
        let lower_attr = self.inner.backing.attr(rel).ok();
        let layer = self.resolve_with_lower_presence(b.id, rel, lower_attr.is_some());
        // --api substitute: same FUSE-shadow trick as brush, but the target
        // is the safe-for-box oaita.toml the engine pre-wrote at startup.
        // Only when the box was launched with --api AND the rel is the host
        // oaita config's path AND the lower (host) is what would otherwise
        // serve — a box that wrote into this path keeps its own write.
        if matches!(layer, Layer::Lower) && b.is_api()
            && Self::matches_host_oaita_config(rel) {
            let safe = crate::paths::api_box_oaita_toml_path();
            if let Ok(md) = std::fs::metadata(&safe) {
                let mut a = self.attr_from_md(ino, &md);
                a.kind = NodeKind::RegularFile;
                return Some(a);
            }
        }
        // --api MITM CA bundle: when the box reads any of the canonical
        // CA bundle paths and the lower (host) would otherwise serve, we
        // shadow it with the engine's pre-written augmented bundle
        // (host system + engine MITM root). The runner USED to bwrap-
        // bind a memfd/tempfile here, but a NESTED runner's `/tmp` is
        // overlay-captured — those binds left `sarun-ca-{pid}.pem`
        // noise in the parent box's overlay. Same shape as the
        // oaita.toml shadow above; the box reads canonical paths and
        // gets engine-controlled content, no on-disk write or bind.
        // Gated on the box's OWN upper having no entry (not on Layer::Lower):
        // an OCI image layer baking its own bundle/resolv.conf must not
        // bypass the shadow — only this box's own write (or delete) wins.
        if b.entry(rel).is_none() && b.is_tap()
            && Self::matches_api_box_ca_target(rel) {
            let safe = crate::paths::api_box_ca_pem_path();
            if let Ok(md) = std::fs::metadata(&safe) {
                let mut a = self.attr_from_md(ino, &md);
                a.kind = NodeKind::RegularFile;
                return Some(a);
            }
        }
        // --api resolv.conf: synthetic `nameserver <engine-gateway>\n`
        // so the box's stub resolver dials the engine's per-box DNS.
        // Same shadow pattern, same reasons.
        if b.entry(rel).is_none() && b.is_tap() && rel == "etc/resolv.conf" {
            let safe = crate::paths::api_box_resolv_conf_path();
            if let Ok(md) = std::fs::metadata(&safe) {
                let mut a = self.attr_from_md(ino, &md);
                a.kind = NodeKind::RegularFile;
                return Some(a);
            }
        }
        // Brush-mode shadow: if the box is -b AND this lookup falls
        // through to the lower (host) AND the rel matches one of the
        // compiled shadow patterns, serve the engine binary's attrs.
        // If the box wrote to this path (UpperFile / UpperSymlink /
        // ...) the upper wins — the shadow only kicks in for the
        // host-passthrough case.
        if matches!(layer, Layer::Lower) && b.is_brush() && self.shadow_matches(rel) {
            if let Some(exe) = self.shadow_target_path(b) {
                if let Ok(md) = std::fs::metadata(&exe) {
                    let mut a = self.attr_from_md(ino, &md);
                    a.kind = NodeKind::RegularFile;
                    // Keep exec bits — most shadow targets are
                    // /bin/sh-shaped things the box wants to exec.
                    return Some(a);
                }
            }
        }
        let mut attr = match layer {
            Layer::Absent => None,
            Layer::Lower => lower_attr.map(|attr| attr.node_attr(ino)),
            Layer::UpperFile { owner, rowid, mode } => {
                let bp = blob_path(owner, rowid);
                let md = bp.metadata().ok()?;
                let mut a = self.attr_from_md(ino, &md);
                a.perm = (mode & 0o7777) as u16;
                a.kind = NodeKind::RegularFile;
                Some(a)
            }
            // Carried size/mode only — getattr must NEVER call blob()
            // (an `ls -lR` over a big attachment must not decode it).
            Layer::ExtFile { size, mode, .. } => {
                let mut a = self.synth_file_attr(ino);
                a.size = size;
                a.blocks = size.div_ceil(512);
                a.perm = (mode & 0o7777) as u16;
                Some(a)
            }
            Layer::UpperDir { mode, mtime_ns } =>
                Some(self.synth_dir_attr(ino, mode, mtime_ns)),
            Layer::UpperSymlink { target } =>
                Some(self.synth_link_attr(
                    ino, target.as_os_str().as_encoded_bytes().len() as u64)),
            Layer::UpperSpecial { mode, rdev } => {
                let mut a = self.synth_file_attr(ino);
                a.kind = kind_of_mode(mode);
                a.perm = (mode & 0o7777) as u16;
                a.rdev = rdev as u32;
                Some(a)
            }
        }?;
        if let Some(atime_ns) = b.atime_of(rel) {
            attr.atime = ns_ts(atime_ns);
        }
        if let Some((uid, gid)) = b.owner_of(rel) {
            attr.uid = uid;
            attr.gid = gid;
        }
        Some(attr)
    }

    /// Transport-neutral lookup.  Both the host `/dev/fuse` adapter and the
    /// virtio-fs server enter policy here, including synthetic nodes and inode
    /// lookup lifetime accounting.
    fn lookup_node(&self, parent: u64, name: &OsStr) -> Result<NodeAttr, Errno> {
        let (bid, prel) = self.key_of(parent).ok_or(Errno::ENOENT)?;
        let name = name.to_str().ok_or(Errno::ENOENT)?;
        let attr = if bid == 0 {
            let id = name.parse::<i64>().map_err(|_| Errno::ENOENT)?;
            if self.box_of(id).is_none() {
                return Err(Errno::ENOENT);
            }
            let ino = self.ino_for(&(id, String::new()));
            self.synth_dir_attr(ino, 0o40755, 0)
        } else {
            let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
            if prel.is_empty() && name == SyntheticNode::Children.name() {
                let ino = self.ino_for(&(bid, SyntheticNode::Children.name().to_string()));
                SyntheticNode::Children.attr(ino)
            } else if prel == SyntheticNode::Children.name() {
                let cid = name.parse::<i64>().map_err(|_| Errno::ENOENT)?;
                if !self.inner.synthetic.is_child(
                    &self.inner.boxes.read().unwrap(),
                    bid,
                    cid,
                ) {
                    return Err(Errno::ENOENT);
                }
                let ino = self.ino_for(&(cid, String::new()));
                self.synth_dir_attr(ino, 0o40755, 0)
            } else {
                let rel = if prel.is_empty() {
                    name.to_string()
                } else {
                    format!("{prel}/{name}")
                };
                let ino = self.ino_for(&(bid, rel.clone()));
                if prel.is_empty()
                    && SyntheticNode::at(&rel).is_some_and(SyntheticNode::is_file)
                {
                    SyntheticNode::at(&rel).unwrap().attr(ino)
                } else {
                    if !(b.is_api()
                        && (Self::oaita_config_ancestor_or_self(&rel)
                            || Self::oaita_state_ancestor_self_or_within(&rel)))
                        && Self::is_engine_path(&rel)
                    {
                        return Err(Errno::ENOENT);
                    }
                    self.attr_of(&b, ino, &rel).ok_or(Errno::ENOENT)?
                }
            }
        };
        self.inner.inodes.acquire(attr.inode, 1);
        Ok(attr)
    }

    fn getattr_node(&self, inode: u64) -> Result<NodeAttr, Errno> {
        // A detached inode may intentionally retain the same historical key
        // as a replacement now occupying that pathname. Its saved identity
        // must win; consulting the live namespace first would make an open
        // overwritten fd appear to become the replacement file.
        if let Some(attr) = self.inner.detached_attrs.read().unwrap().get(&inode).copied() {
            return Ok(attr);
        }
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        if bid == 0 || rel.is_empty() {
            return Ok(self.synth_dir_attr(inode, 0o40755, 0));
        }
        if let Some(node) = SyntheticNode::at(&rel) {
            return Ok(node.attr(inode));
        }
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        self.attr_of(&b, inode, &rel).ok_or(Errno::ENOENT)
    }

    fn readlink_node(&self, inode: u64) -> Result<Vec<u8>, Errno> {
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        self.box_of(bid).ok_or(Errno::ENOENT)?;
        match self.resolve(bid, &rel) {
            Layer::UpperSymlink { target } =>
                Ok(target.as_os_str().as_encoded_bytes().to_vec()),
            Layer::Lower => self.inner.backing.node(&rel)
                .and_then(|node| node.readlink())
                .map_err(|_| Errno::EINVAL),
            _ => Err(Errno::EINVAL),
        }
    }

    fn set_xattr_node(
        &self,
        inode: u64,
        name: &OsStr,
        value: &[u8],
        flags: u32,
    ) -> Result<(), Errno> {
        self.getattr_node(inode)?;
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        if self.ro_denied(bid, &rel) {
            return Err(Errno::EROFS);
        }
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let exists = b.get_xattr(&rel, name).is_some();
        let create = libc::XATTR_CREATE as u32;
        let replace = libc::XATTR_REPLACE as u32;
        if flags & !(create | replace) != 0
            || flags & create != 0 && flags & replace != 0
        {
            return Err(Errno::EINVAL);
        }
        if flags & create != 0 && exists {
            return Err(Errno::EEXIST);
        }
        if flags & replace != 0 && !exists {
            return Err(Errno::ENODATA);
        }
        self.inner.mutations.set_xattr(&b, &rel, name, value);
        Ok(())
    }

    fn get_xattr_node(&self, inode: u64, name: &OsStr) -> Result<Vec<u8>, Errno> {
        self.getattr_node(inode)?;
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        b.get_xattr(&rel, name).ok_or(Errno::ENODATA)
    }

    fn list_xattr_node(&self, inode: u64) -> Result<Vec<u8>, Errno> {
        self.getattr_node(inode)?;
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        let mut buffer = Vec::new();
        for name in b.list_xattr(&rel) {
            buffer.extend_from_slice(name.as_bytes());
            buffer.push(0);
        }
        Ok(buffer)
    }

    fn remove_xattr_node(&self, inode: u64, name: &OsStr) -> Result<(), Errno> {
        self.getattr_node(inode)?;
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        if self.ro_denied(bid, &rel) {
            return Err(Errno::EROFS);
        }
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        self.inner
            .mutations
            .remove_xattr(&b, &rel, name)
            .then_some(())
            .ok_or(Errno::ENODATA)
    }

    fn open_node(
        &self,
        pid: u32,
        inode: u64,
        flags: u32,
        allow_backing: bool,
    ) -> Result<OpenedNode, Errno> {
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        let want_write = flags & libc::O_ACCMODE as u32 != libc::O_RDONLY as u32;
        if let Some(path) = self.projected_file(bid, &rel) {
            if want_write { return Err(Errno::EROFS); }
            let file = File::open(path).map_err(Errno::from)?;
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                linked: true,
                data: FileData::Native(file),
                upper: false,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                passthrough: false,
                backing_candidate: allow_backing,
            });
            return Ok(OpenedNode {
                handle,
                direct_io: false,
                nonseekable: false,
                keep_cache: true,
                backing_candidate: allow_backing,
            });
        }
        if want_write && self.ro_denied(bid, &rel) {
            return Err(Errno::EROFS);
        }
        if SyntheticNode::at(&rel) == Some(SyntheticNode::Jobserver) {
            let nonblock = flags & libc::O_NONBLOCK as u32 != 0;
            let handle = self
                .inner
                .handles
                .insert(Handle::Jobserver { box_id: bid, nonblock });
            return Ok(OpenedNode {
                handle,
                direct_io: true,
                nonseekable: true,
                keep_cache: false,
                backing_candidate: false,
            });
        }
        if let Some(stream) = SyntheticNode::at(&rel).and_then(SyntheticNode::stream) {
            self.inner.synthetic.sink_opened(bid);
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                linked: true,
                data: FileData::Sink(stream),
                upper: false,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                passthrough: false,
                backing_candidate: false,
            });
            return Ok(OpenedNode {
                handle,
                direct_io: false,
                nonseekable: false,
                keep_cache: false,
                backing_candidate: false,
            });
        }
        if want_write && (b.direct() || self.is_passthrough(&rel, bid, pid)) {
            let host = self.host(&rel);
            if let Some(parent) = host.parent() {
                std::fs::create_dir_all(parent).map_err(Errno::from)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(flags & libc::O_TRUNC as u32 != 0)
                .open(&host)
                .map_err(Errno::from)?;
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                linked: true,
                data: FileData::Native(file),
                upper: true,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                passthrough: true,
                backing_candidate: false,
            });
            return Ok(OpenedNode {
                handle,
                direct_io: false,
                nonseekable: false,
                keep_cache: false,
                backing_candidate: false,
            });
        }
        let tap_shadow = if b.is_tap() && b.entry(&rel).is_none() {
            if Self::matches_api_box_ca_target(&rel) {
                Some(crate::paths::api_box_ca_pem_path())
            } else if rel == "etc/resolv.conf" {
                Some(crate::paths::api_box_resolv_conf_path())
            } else {
                None
            }
        } else {
            None
        };
        if let Some(safe) = tap_shadow {
            let file = File::open(safe).map_err(|_| Errno::EACCES)?;
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                linked: true,
                data: FileData::Native(file),
                upper: false,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                passthrough: false,
                backing_candidate: false,
            });
            return Ok(OpenedNode {
                handle,
                direct_io: false,
                nonseekable: false,
                keep_cache: false,
                backing_candidate: false,
            });
        }
        const FMODE_EXEC: u32 = 0x20;
        let backing_candidate = allow_backing
            && !want_write
            && flags & FMODE_EXEC == 0
            && (b.direct() || self.is_passthrough_read(&rel));
        let (mut data, mut upper) = match self.resolve(bid, &rel) {
            Layer::UpperFile { owner, rowid, .. } => {
                self.ensure_upper_blob(owner, rowid, &rel);
                let own = owner == bid;
                let file = OpenOptions::new()
                    .read(true)
                    .write(want_write && own)
                    .open(blob_path(owner, rowid))
                    .map_err(|_| Errno::EIO)?;
                (FileData::Native(file), own)
            }
            Layer::Lower => {
                let projected = if b.is_api() && Self::matches_host_oaita_config(&rel) {
                    Some(crate::paths::api_box_oaita_toml_path())
                } else if b.is_brush() && self.shadow_matches(&rel) {
                    self.shadow_target_path(&b)
                } else {
                    None
                };
                if let Some(path) = projected {
                    (FileData::Native(File::open(path).map_err(|_| Errno::EACCES)?), false)
                } else if backing_candidate {
                    (
                        FileData::Native(
                            File::open(self.host(&rel)).map_err(|_| Errno::EACCES)?
                        ),
                        false,
                    )
                } else {
                    let lower = self
                        .inner
                        .backing
                        .node(&rel)
                        .and_then(|node| node.open())
                        .map_err(|_| Errno::EACCES)?;
                    (FileData::Lower(lower), false)
                }
            }
            Layer::ExtFile { att, rel: erel, .. } => {
                let file = match att.blob(&erel) {
                    Some(depot_model::variant::Blob::File(path)) => File::open(path).ok(),
                    Some(depot_model::variant::Blob::Bytes(bytes)) => self
                        .cache()
                        .and_then(|cache| cache.file_for(&bytes).ok())
                        .and_then(|path| File::open(path).ok()),
                    None => None,
                }
                .ok_or(Errno::EIO)?;
                (FileData::Native(file), false)
            }
            _ => return Err(Errno::ENOENT),
        };
        let truncated = want_write && flags & libc::O_TRUNC as u32 != 0;
        if truncated {
            if !upper {
                data = FileData::Native(
                    self.copy_up(&b, &rel, pid).map_err(Errno::from)?,
                );
                upper = true;
            }
            let FileData::Native(file) = &data else {
                return Err(Errno::EBADF);
            };
            file.set_len(0).map_err(Errno::from)?;
        }
        let handle = self.reg_fh(FhInner {
            box_id: bid,
            rel,
            linked: true,
            data,
            upper,
            dirty: truncated,
            last_pid: pid,
            last_tgid: 0,
            passthrough: false,
            backing_candidate,
        });
        Ok(OpenedNode {
            handle,
            direct_io: false,
            nonseekable: false,
            keep_cache: !backing_candidate,
            backing_candidate,
        })
    }

    fn create_node(
        &self,
        pid: u32,
        uid: u32,
        gid: u32,
        parent: u64,
        name: &OsStr,
        mode: u32,
    ) -> Result<(NodeAttr, OpenedNode), Errno> {
        let (bid, parent_rel) = self.key_of(parent).ok_or(Errno::ENOENT)?;
        if bid == 0 {
            return Err(Errno::EPERM);
        }
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let rel = if parent_rel.is_empty() {
            name.to_owned()
        } else {
            format!("{parent_rel}/{name}")
        };
        if self.ro_denied(bid, &rel) {
            return Err(Errno::EROFS);
        }
        let (attr, handle) = if b.direct() || self.is_passthrough(&rel, bid, pid) {
            let host = self.host(&rel);
            if let Some(parent) = host.parent() {
                std::fs::create_dir_all(parent).map_err(Errno::from)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&host)
                .map_err(Errno::from)?;
            let inode = self.ino_for(&(bid, rel.clone()));
            let mut attr = file
                .metadata()
                .ok()
                .map(|metadata| self.attr_from_md(inode, &metadata))
                .unwrap_or_else(|| self.synth_file_attr(inode));
            attr.kind = NodeKind::RegularFile;
            attr.perm = (mode & 0o7777) as u16;
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                linked: true,
                data: FileData::Native(file),
                upper: true,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                passthrough: true,
                backing_candidate: false,
            });
            (attr, handle)
        } else {
            let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
            let rowid = capture.ensure_file(&rel, mode | libc::S_IFREG);
            capture.set_owner(&rel, uid, gid);
            let path = blob_path(bid, rowid);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(Errno::from)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .map_err(Errno::from)?;
            let inode = self.ino_for(&(bid, rel.clone()));
            let mut attr = file
                .metadata()
                .ok()
                .map(|metadata| self.attr_from_md(inode, &metadata))
                .unwrap_or_else(|| self.synth_file_attr(inode));
            attr.kind = NodeKind::RegularFile;
            attr.perm = (mode & 0o7777) as u16;
            attr.uid = uid;
            attr.gid = gid;
            self.inner.mutations.record(bid, rel.clone(), "create");
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                linked: true,
                data: FileData::Native(file),
                upper: true,
                dirty: true,
                last_pid: pid,
                last_tgid: 0,
                passthrough: false,
                backing_candidate: false,
            });
            (attr, handle)
        };
        self.inner.inodes.acquire(attr.inode, 1);
        Ok((
            attr,
            OpenedNode {
                handle,
                direct_io: false,
                nonseekable: false,
                keep_cache: false,
                backing_candidate: false,
            },
        ))
    }

    fn child_path(&self, parent: u64, name: &OsStr) -> Result<(i64, String), Errno> {
        let (box_id, parent_rel) = self.key_of(parent).ok_or(Errno::ENOENT)?;
        if box_id == 0 {
            return Err(Errno::EPERM);
        }
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let rel = if parent_rel.is_empty() {
            name.to_owned()
        } else {
            format!("{parent_rel}/{name}")
        };
        Ok((box_id, rel))
    }

    fn mkdir_node(&self, pid: u32, uid: u32, gid: u32,
                  parent: u64, name: &OsStr, mode: u32)
        -> Result<NodeAttr, Errno>
    {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        if !matches!(self.resolve(box_id, &rel), Layer::Absent) {
            return Err(Errno::EEXIST);
        }
        let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
        capture.set_dir(&rel, mode);
        capture.set_owner(&rel, uid, gid);
        let inode = self.ino_for(&(box_id, rel.clone()));
        self.inner.mutations.record(box_id, rel, "mkdir");
        let mut attr = self.synth_dir_attr(inode, mode | libc::S_IFDIR, 0);
        attr.uid = uid;
        attr.gid = gid;
        self.inner.inodes.acquire(inode, 1);
        Ok(attr)
    }

    fn symlink_node(
        &self,
        pid: u32,
        uid: u32,
        gid: u32,
        parent: u64,
        name: &OsStr,
        target: &Path,
    ) -> Result<NodeAttr, Errno> {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
        capture.set_symlink(&rel, target);
        capture.set_owner(&rel, uid, gid);
        let inode = self.ino_for(&(box_id, rel.clone()));
        self.inner.mutations.record(box_id, rel, "symlink");
        let mut attr = self.synth_link_attr(
            inode,
            target.as_os_str().as_encoded_bytes().len() as u64,
        );
        attr.uid = uid;
        attr.gid = gid;
        self.inner.inodes.acquire(inode, 1);
        Ok(attr)
    }

    /// Preserve POSIX open-file-description lifetime before a namespace entry
    /// disappears. A lower handle must first acquire a private copy: after the
    /// following unlink/rename-over removes the captured row, the open file
    /// descriptor still owns that now-anonymous blob and can be read/written
    /// without touching the host or recreating the pathname.
    fn detach_open_handles(&self, box_id: i64, rel: &str, pid: u32) -> Result<(), Errno> {
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        for handle in self.inner.handles.values() {
            let Handle::File(handle) = &*handle else { continue };
            let mut handle = handle.lock().unwrap();
            if handle.inner.box_id != box_id
                || handle.inner.rel != rel
                || !handle.inner.linked
            {
                continue;
            }
            if matches!(handle.inner.data, FileData::Lower(_)) {
                handle.inner.data = FileData::Native(
                    self.copy_up(&b, rel, pid).map_err(Errno::from)?,
                );
                handle.inner.upper = true;
                handle.inner.backing_candidate = false;
            }
            handle.inner.linked = false;
        }
        Ok(())
    }

    fn remap_open_handle_subtree(&self, box_id: i64, old: &str, new: &str) {
        let prefix = format!("{old}/");
        for handle in self.inner.handles.values() {
            let Handle::File(handle) = &*handle else { continue };
            let mut handle = handle.lock().unwrap();
            if handle.inner.box_id != box_id || !handle.inner.linked {
                continue;
            }
            let replacement = if handle.inner.rel == old {
                Some(new.to_owned())
            } else {
                handle
                    .inner
                    .rel
                    .strip_prefix(&prefix)
                    .map(|tail| format!("{new}/{tail}"))
            };
            if let Some(replacement) = replacement {
                handle.inner.rel = replacement;
            }
        }
    }

    fn detach_inode_name(&self, b: &BoxState, rel: &str) {
        let key = (b.id, rel.to_owned());
        let Some(inode) = self.inner.inodes.inode(&key) else { return };
        if let Some(attr) = self.attr_of(b, inode, rel) {
            self.inner.detached_attrs.write().unwrap().insert(inode, attr);
        }
        self.inner.inodes.detach(&key);
    }

    fn unlink_node(&self, pid: u32, parent: u64, name: &OsStr) -> Result<(), Errno> {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        let inode = self.ino_for(&(box_id, rel.clone()));
        let attr = self.attr_of(&b, inode, &rel).ok_or(Errno::ENOENT)?;
        if attr.kind == NodeKind::Directory {
            return Err(Errno::EISDIR);
        }
        self.detach_open_handles(box_id, &rel, pid)?;
        self.inner.inodes.detach(&(box_id, rel.clone()));
        self.inner.detached_attrs.write().unwrap().insert(inode, attr);
        self.inner.mutations.writer(&b, pid, self.host_request_pids).delete(&rel);
        self.inner.mutations.record(box_id, rel, "unlink");
        Ok(())
    }

    fn rmdir_node(&self, pid: u32, parent: u64, name: &OsStr) -> Result<(), Errno> {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        let inode = self.ino_for(&(box_id, rel.clone()));
        let attr = self.attr_of(&b, inode, &rel).ok_or(Errno::ENOENT)?;
        if attr.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        if !self.scan_dir(&b, &rel, false).is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        self.inner.inodes.detach(&(box_id, rel.clone()));
        self.inner.detached_attrs.write().unwrap().insert(inode, attr);
        self.inner.mutations.writer(&b, pid, self.host_request_pids).delete(&rel);
        self.inner.mutations.record(box_id, rel, "rmdir");
        Ok(())
    }

    fn rename_node(
        &self,
        pid: u32,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        flags: u32,
    ) -> Result<(), Errno> {
        let (box_id, old_parent) = self.key_of(parent).ok_or(Errno::EACCES)?;
        let (new_box_id, new_parent) = self
            .key_of(new_parent)
            .ok_or(Errno::EACCES)?;
        if box_id == 0 || new_box_id == 0 || box_id != new_box_id {
            return Err(Errno::EXDEV);
        }
        if flags & libc::RENAME_EXCHANGE != 0 {
            return Err(Errno::ENOSYS);
        }
        if flags & !(libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE) != 0 {
            return Err(Errno::EINVAL);
        }
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let new_name = new_name.to_str().ok_or(Errno::EINVAL)?;
        let join = |parent: &str, name: &str| {
            if parent.is_empty() { name.to_owned() } else { format!("{parent}/{name}") }
        };
        let old_rel = join(&old_parent, name);
        let new_rel = join(&new_parent, new_name);
        if old_rel == new_rel {
            return Ok(());
        }
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &old_rel) || self.ro_denied(box_id, &new_rel) {
            return Err(Errno::EROFS);
        }
        if flags & libc::RENAME_NOREPLACE != 0
            && !matches!(self.resolve(box_id, &new_rel), Layer::Absent)
        {
            return Err(Errno::EEXIST);
        }
        if !matches!(self.resolve(box_id, &new_rel), Layer::Absent) {
            self.detach_open_handles(box_id, &new_rel, pid)?;
            self.detach_inode_name(&b, &new_rel);
        }
        let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
        let lower_attr = self.inner.backing.attr(&old_rel).ok();
        let lower_old = lower_attr.is_some();
        match self.layer(&b, &old_rel) {
            Layer::Absent => return Err(Errno::ENOENT),
            Layer::UpperDir { .. } => {
                capture.reparent(&old_rel, &new_rel);
                if lower_attr.is_some_and(|attr| attr.kind == NodeKind::Directory) {
                    capture.whiteout(&old_rel);
                }
            }
            Layer::Lower => {
                self.copy_up(&b, &old_rel, pid).map_err(|_| Errno::EIO)?;
                capture.rename(&old_rel, &new_rel);
                capture.whiteout(&old_rel);
            }
            Layer::UpperFile { .. }
            | Layer::UpperSymlink { .. }
            | Layer::UpperSpecial { .. } => {
                capture.rename(&old_rel, &new_rel);
                if lower_old {
                    capture.whiteout(&old_rel);
                }
            }
            Layer::ExtFile { .. } => return Err(Errno::EROFS),
        }
        self.remap_inode_subtree(box_id, &old_rel, &new_rel);
        self.remap_open_handle_subtree(box_id, &old_rel, &new_rel);
        self.inner.mutations.record(box_id, old_rel, "rename_src");
        self.inner.mutations.record(box_id, new_rel, "rename_dst");
        Ok(())
    }

    fn mknod_node(
        &self,
        pid: u32,
        uid: u32,
        gid: u32,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
    ) -> Result<NodeAttr, Errno> {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        match mode & libc::S_IFMT {
            libc::S_IFREG => {
                let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
                let rowid = capture.ensure_file(&rel, mode);
                capture.set_owner(&rel, uid, gid);
                let path = blob_path(box_id, rowid);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(Errno::from)?;
                }
                File::create(path).map_err(Errno::from)?;
            }
            libc::S_IFIFO | libc::S_IFCHR | libc::S_IFBLK | libc::S_IFSOCK => {
                let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
                capture.set_special(&rel, mode, rdev as u64);
                capture.set_owner(&rel, uid, gid);
            }
            _ => return Err(Errno::EINVAL),
        }
        self.inner.mutations.record(box_id, rel.clone(), "mknod");
        let inode = self.ino_for(&(box_id, rel.clone()));
        let attr = self.attr_of(&b, inode, &rel).ok_or(Errno::EIO)?;
        self.inner.inodes.acquire(inode, 1);
        Ok(attr)
    }

    fn link_node(
        &self,
        pid: u32,
        inode: u64,
        new_parent: u64,
        new_name: &OsStr,
    ) -> Result<NodeAttr, Errno> {
        let (source_box, source_rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        let (box_id, new_rel) = self.child_path(new_parent, new_name)?;
        if source_box != box_id {
            return Err(Errno::EXDEV);
        }
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &new_rel) {
            return Err(Errno::EROFS);
        }
        if !matches!(self.resolve(box_id, &new_rel), Layer::Absent) {
            return Err(Errno::EEXIST);
        }
        if self.copy_up(&b, &source_rel, pid).is_err() {
            return Err(Errno::EIO);
        }
        let (source_rowid, source_mode) = match self.layer(&b, &source_rel) {
            Layer::UpperFile { rowid, mode, .. } => (rowid, mode),
            _ => return Err(Errno::EPERM),
        };
        let new_rowid = self
            .inner
            .mutations
            .writer(&b, pid, self.host_request_pids)
            .ensure_file(&new_rel, source_mode);
        let destination = blob_path(box_id, new_rowid);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(Errno::from)?;
        }
        std::fs::hard_link(blob_path(box_id, source_rowid), &destination)
            .map_err(Errno::from)?;
        let new_inode = self.ino_for(&(box_id, new_rel.clone()));
        let attr = self.attr_of(&b, new_inode, &new_rel).ok_or(Errno::EIO)?;
        self.inner.inodes.acquire(new_inode, 1);
        self.inner.mutations.record(box_id, new_rel, "link");
        Ok(attr)
    }

    fn fallocate_node(
        &self,
        pid: u32,
        handle: u64,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> Result<(), Errno> {
        let WriteTarget::File { file, box_id, rel } = self.prepare_write_node(pid, handle)? else {
            return Err(Errno::EINVAL);
        };
        let offset = i64::try_from(offset).map_err(|_| Errno::EFBIG)?;
        let length = i64::try_from(length).map_err(|_| Errno::EFBIG)?;
        let result = unsafe { libc::fallocate64(file.as_raw_fd(), mode as i32, offset, length) };
        if result != 0 {
            return Err(Errno::from(std::io::Error::last_os_error()));
        }
        self.finish_file_write(box_id, rel);
        Ok(())
    }

    fn setattr_node(
        &self,
        pid: u32,
        inode: u64,
        request: NodeSetattr,
    ) -> Result<NodeAttr, Errno> {
        let (box_id, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        if b.direct() || self.is_passthrough(&rel, box_id, pid) {
            let host = self.host(&rel);
            let path = CString::new(host.as_os_str().as_encoded_bytes())
                .map_err(|_| Errno::EINVAL)?;
            if let Some(size) = request.size {
                OpenOptions::new()
                    .write(true)
                    .open(&host)
                    .map_err(Errno::from)?
                    .set_len(size)
                    .map_err(Errno::from)?;
            }
            if let Some(mode) = request.mode {
                if unsafe { libc::chmod(path.as_ptr(), mode & 0o7777) } != 0 {
                    return Err(Errno::from(std::io::Error::last_os_error()));
                }
            }
            if request.uid.is_some() || request.gid.is_some() {
                if unsafe {
                    libc::lchown(
                        path.as_ptr(),
                        request.uid.unwrap_or(u32::MAX),
                        request.gid.unwrap_or(u32::MAX),
                    )
                } != 0
                {
                    return Err(Errno::from(std::io::Error::last_os_error()));
                }
            }
            if request.atime.is_some() || request.mtime.is_some() {
                let to_timespec = |time: SystemTime| {
                    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
                    libc::timespec {
                        tv_sec: duration.as_secs() as _,
                        tv_nsec: duration.subsec_nanos() as _,
                    }
                };
                let omit = libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT };
                let times = [
                    request.atime.map_or(omit, &to_timespec),
                    request.mtime.map_or(omit, &to_timespec),
                ];
                if unsafe {
                    libc::utimensat(
                        libc::AT_FDCWD,
                        path.as_ptr(),
                        times.as_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } != 0
                {
                    return Err(Errno::from(std::io::Error::last_os_error()));
                }
            }
            return self.attr_of(&b, inode, &rel).ok_or(Errno::ENOENT);
        }
        if let Some(size) = request.size {
            let file = match self.layer(&b, &rel) {
                Layer::UpperFile { rowid, .. } => {
                    self.ensure_upper_blob(box_id, rowid, &rel);
                    OpenOptions::new().write(true).open(blob_path(box_id, rowid)).ok()
                }
                Layer::Lower => self.copy_up(&b, &rel, pid).ok(),
                _ => None,
            }
            .ok_or(Errno::EIO)?;
            file.set_len(size).map_err(Errno::from)?;
            let metadata = file.metadata().map_err(Errno::from)?;
            self.inner
                .mutations
                .writer(&b, pid, self.host_request_pids)
                .finalize_file(
                    &rel,
                    metadata.size() as i64,
                    metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
                );
        }
        if let Some(mode) = request.mode {
            let permissions = mode & 0o7777;
            let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
            match self.layer(&b, &rel) {
                Layer::UpperFile { .. } => capture.set_mode(&rel, libc::S_IFREG | permissions),
                Layer::UpperDir { .. } => capture.set_mode(&rel, libc::S_IFDIR | permissions),
                Layer::UpperSymlink { .. } => {}
                Layer::Lower if self.inner.backing.attr(&rel).is_ok_and(|attr| {
                    attr.kind == NodeKind::Directory
                }) => capture.set_dir(&rel, permissions),
                Layer::Lower => {
                    self.copy_up(&b, &rel, pid).map_err(|_| Errno::EIO)?;
                    capture.set_mode(&rel, libc::S_IFREG | permissions);
                }
                Layer::Absent => return Err(Errno::ENOENT),
                Layer::UpperSpecial { mode, .. } => {
                    capture.set_mode(&rel, (mode & libc::S_IFMT) | permissions)
                }
                Layer::ExtFile { .. } => return Err(Errno::EROFS),
            }
        }
        if request.uid.is_some() || request.gid.is_some() {
            let current = b.owner_of(&rel).unwrap_or((0, 0));
            let uid = request.uid.unwrap_or(current.0);
            let gid = request.gid.unwrap_or(current.1);
            if matches!(self.layer(&b, &rel), Layer::Lower)
                && !self.inner.backing.attr(&rel).is_ok_and(|attr| {
                    attr.kind == NodeKind::Directory
                })
            {
                self.copy_up(&b, &rel, pid).map_err(|_| Errno::EIO)?;
            }
            // Captured ownership is protocol metadata, not backing-store
            // ownership. The blob lives on a host superblock where namespace
            // root has no authority to chown it, and no guest accesses that
            // blob except through this projection. Persist the canonical IDs
            // in the ownership side table below.
            if matches!(self.layer(&b, &rel), Layer::Absent) {
                return Err(Errno::ENOENT);
            }
            self.inner
                .mutations
                .writer(&b, pid, self.host_request_pids)
                .set_owner(&rel, uid, gid);
        }
        if request.atime.is_some() || request.mtime.is_some() {
            if matches!(self.layer(&b, &rel), Layer::Lower)
            {
                if self.inner.backing.attr(&rel).is_ok_and(|attr| {
                    attr.kind == NodeKind::Directory
                }) {
                    self.inner
                        .mutations
                        .writer(&b, pid, self.host_request_pids)
                        .set_dir(&rel, self.inner.backing.attr(&rel)
                            .map(|attr| attr.mode)
                            .unwrap_or(libc::S_IFDIR | 0o755));
                } else {
                    self.copy_up(&b, &rel, pid).map_err(|_| Errno::EIO)?;
                }
            }
            let capture = self.inner.mutations.writer(&b, pid, self.host_request_pids);
            if let Some(atime) = request.atime {
                let nanos = atime.duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_nanos() as i64)
                    .unwrap_or(0);
                capture.set_atime(&rel, nanos);
            }
            if let Some(mtime) = request.mtime {
                let nanos = mtime.duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_nanos() as i64)
                    .unwrap_or(0);
                capture.set_mtime(&rel, nanos);
            }
            if let Layer::UpperFile { rowid, .. } = self.layer(&b, &rel) {
                self.ensure_upper_blob(box_id, rowid, &rel);
                let path = CString::new(blob_path(box_id, rowid)
                    .as_os_str().as_encoded_bytes()).map_err(|_| Errno::EINVAL)?;
                let to_timespec = |time: SystemTime| {
                    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
                    libc::timespec {
                        tv_sec: duration.as_secs() as _,
                        tv_nsec: duration.subsec_nanos() as _,
                    }
                };
                let omit = libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT };
                let times = [
                    request.atime.map_or(omit, &to_timespec),
                    request.mtime.map_or(omit, &to_timespec),
                ];
                if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(),
                                             times.as_ptr(), 0) } != 0 {
                    return Err(Errno::from(std::io::Error::last_os_error()));
                }
            }
        }
        self.attr_of(&b, inode, &rel).ok_or(Errno::ENOENT)
    }

    fn jobserver_handle(&self, handle: u64) -> Option<(i64, bool)> {
        self.inner
            .handles
            .get(handle)
            .as_deref()
            .and_then(|handle| match handle {
                Handle::Jobserver { box_id, nonblock } => Some((*box_id, *nonblock)),
                _ => None,
            })
    }

    fn read_file_to<W: virtiofsd::filesystem::ZeroCopyWriter>(
        &self,
        handle: u64,
        mut writer: W,
        size: u32,
        offset: u64,
    ) -> std::io::Result<usize> {
        self.inner.daemon_reads.fetch_add(1, Ordering::Relaxed);
        let handle = self
            .inner
            .handles
            .get(handle)
            .ok_or_else(|| virtio_error(Errno::EBADF))?;
        let Handle::File(handle) = &*handle else {
            return Err(virtio_error(Errno::EBADF));
        };
        let handle = handle.lock().unwrap();
        match &handle.inner.data {
            FileData::Native(file) => {
                writer.read_from_file_at(file, size as usize, offset, None)
            }
            FileData::Lower(lower) => lower.read_to(writer, size, offset),
            FileData::Sink(_) => Err(virtio_error(Errno::EBADF)),
        }
    }

    fn prepare_write_node(&self, pid: u32, handle: u64) -> Result<WriteTarget, Errno> {
        let handle = self.inner.handles.get(handle).ok_or(Errno::EBADF)?;
        if let Handle::Jobserver { box_id, .. } = &*handle {
            return Ok(WriteTarget::Jobserver { box_id: *box_id });
        }
        let Handle::File(handle) = &*handle else {
            return Err(Errno::EBADF);
        };
        let mut handle = handle.lock().unwrap();
        if let FileData::Sink(stream) = &handle.inner.data {
            return Ok(WriteTarget::Sink {
                box_id: handle.inner.box_id,
                stream: *stream,
            });
        }
        if !handle.inner.upper {
            if !handle.inner.linked {
                // Read-only native projections cannot be promoted after their
                // name vanished. Ordinary lower handles were converted to an
                // anonymous native copy by detach_open_handles().
                return Err(Errno::EBADF);
            }
            let b = self.box_of(handle.inner.box_id).ok_or(Errno::EIO)?;
            handle.inner.data = FileData::Native(
                self.copy_up(&b, &handle.inner.rel.clone(), pid)
                    .map_err(|_| Errno::EIO)?,
            );
            handle.inner.upper = true;
        }
        handle.inner.dirty = true;
        if pid != handle.inner.last_pid || handle.inner.last_tgid == 0 {
            handle.inner.last_tgid = if self.host_request_pids { tgid_of(pid) } else { pid };
            if let Some(b) = self.box_of(handle.inner.box_id) {
                self.inner
                    .mutations
                    .observe_writer(&b, handle.inner.last_tgid, self.host_request_pids);
            }
        }
        handle.inner.last_pid = pid;
        let FileData::Native(file) = &handle.inner.data else {
            return Err(Errno::EBADF);
        };
        let file = file.try_clone().map_err(Errno::from)?;
        Ok(WriteTarget::File {
            file,
            box_id: handle.inner.box_id,
            rel: handle.inner.linked.then(|| handle.inner.rel.clone()),
        })
    }

    fn finish_file_write(&self, box_id: i64, rel: Option<String>) {
        if let Some(rel) = rel {
            self.inner.mutations.record(box_id, rel, "write");
        }
    }

    fn write_sink_node(&self, pid: u32, box_id: i64, stream: i32, data: &[u8]) {
        let box_state = self.box_of(box_id);
        self.inner.synthetic.write_sink(
            pid,
            if self.host_request_pids { tgid_of(pid) as i32 } else { pid as i32 },
            self.host_request_pids,
            box_state.as_deref(),
            box_id,
            stream,
            data,
        );
    }

    fn release_node(&self, handle: u64) -> Result<(), Errno> {
        let handle = self
            .inner
            .handles
            .remove(handle)
            .ok_or(Errno::EBADF)?;
        if matches!(&*handle, Handle::Jobserver { .. }) {
            return Ok(());
        }
        let Handle::File(handle) = &*handle else {
            return Err(Errno::EBADF);
        };
        let handle = handle.lock().unwrap();
        if matches!(&handle.inner.data, FileData::Sink(_)) {
            self.inner.synthetic.sink_released(handle.inner.box_id);
        } else if handle.inner.dirty && handle.inner.linked && !handle.inner.passthrough {
            if let Some(b) = self.box_of(handle.inner.box_id) {
                let writer_id = if handle.inner.last_tgid != 0 {
                    handle.inner.last_tgid
                } else {
                    handle.inner.last_pid
                };
                if let FileData::Native(file) = &handle.inner.data {
                    let metadata = file.metadata().ok();
                    if let Some(metadata) = metadata {
                        self.inner
                            .mutations
                            .writer(&b, writer_id, self.host_request_pids)
                            .finalize_file(
                                &handle.inner.rel,
                                metadata.size() as i64,
                                metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
                            );
                    }
                }
            }
        }
        Ok(())
    }

    fn sync_file_node(&self, handle: u64, data_only: bool) -> Result<(), Errno> {
        let handle = self.inner.handles.get(handle).ok_or(Errno::EBADF)?;
        if matches!(&*handle, Handle::Jobserver { .. }) {
            return Ok(());
        }
        let Handle::File(handle) = &*handle else {
            return Err(Errno::EBADF);
        };
        let handle = handle.lock().unwrap();
        match &handle.inner.data {
            FileData::Native(file) if data_only => file.sync_data().map_err(Errno::from),
            FileData::Native(file) => file.sync_all().map_err(Errno::from),
            FileData::Lower(_) | FileData::Sink(_) => Ok(()),
        }
    }

    /// FUSE FLUSH is issued for every close of a duplicated guest descriptor;
    /// it is not fsync. Mirror close(2) so delayed close errors are reported,
    /// while explicit FSYNC remains the only operation that forces durability.
    fn flush_file_node(&self, handle: u64) -> Result<(), Errno> {
        let handle = self.inner.handles.get(handle).ok_or(Errno::EBADF)?;
        if matches!(&*handle, Handle::Jobserver { .. }) {
            return Ok(());
        }
        let Handle::File(handle) = &*handle else {
            return Err(Errno::EBADF);
        };
        let handle = handle.lock().unwrap();
        let FileData::Native(file) = &handle.inner.data else {
            return Ok(());
        };
        let duplicate = unsafe { libc::dup(file.as_raw_fd()) };
        if duplicate < 0 {
            return Err(Errno::from(std::io::Error::last_os_error()));
        }
        if unsafe { libc::close(duplicate) } < 0 {
            Err(Errno::from(std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    fn lseek_node(&self, handle: u64, offset: u64, whence: u32) -> Result<u64, Errno> {
        if whence != libc::SEEK_DATA as u32 && whence != libc::SEEK_HOLE as u32 {
            return Err(Errno::EINVAL);
        }
        let handle = self.inner.handles.get(handle).ok_or(Errno::EBADF)?;
        let Handle::File(handle) = &*handle else {
            return Err(Errno::EBADF);
        };
        let handle = handle.lock().unwrap();
        match &handle.inner.data {
            FileData::Native(file) => {
                let offset = i64::try_from(offset).map_err(|_| Errno::EOVERFLOW)?;
                let result = unsafe { libc::lseek64(file.as_raw_fd(), offset, whence as i32) };
                if result < 0 {
                    Err(Errno::from(std::io::Error::last_os_error()))
                } else {
                    Ok(result as u64)
                }
            }
            FileData::Lower(lower) => lower.lseek(offset, whence).map_err(Errno::from),
            FileData::Sink(_) => Err(Errno::ESPIPE),
        }
    }

    fn lock_key(&self, handle: u64, flags: u32) -> Result<LockKey, Errno> {
        let handle = self.inner.handles.get(handle).ok_or(Errno::EBADF)?;
        let Handle::File(handle) = &*handle else {
            return Err(Errno::EBADF);
        };
        let handle = handle.lock().unwrap();
        if matches!(handle.inner.data, FileData::Sink(_)) {
            return Err(Errno::EBADF);
        }
        Ok(LockKey {
            file: handle.lock_identity,
            flock: flags & virtiofsd::fuse::LK_FLOCK != 0,
        })
    }

    fn get_lock_node(&self, handle: u64, owner: u64,
                     lock: virtiofsd::fuse::FileLock, flags: u32)
                     -> Result<virtiofsd::fuse::FileLock, Errno> {
        let key = self.lock_key(handle, flags)?;
        Ok(self.inner.locks.get(key, owner, lock))
    }

    fn set_lock_node(&self, handle: u64, owner: u64,
                     lock: virtiofsd::fuse::FileLock, flags: u32,
                     blocking: bool) -> Result<(), Errno> {
        let key = self.lock_key(handle, flags)?;
        self.inner.locks.set(key, owner, lock, blocking)
    }

    fn release_lock_owner(&self, handle: u64, owner: u64, flock: bool) {
        if let Ok(key) = self.lock_key(handle, if flock {
            virtiofsd::fuse::LK_FLOCK
        } else { 0 }) {
            self.inner.locks.release(key, owner);
        }
    }

    fn statfs_node(&self) -> Result<libc::statvfs64, Errno> {
        self.inner.backing.statfs().map_err(Errno::from)
    }

    /// D3: the first actual write to `rel` copies the RESOLVED lower bytes
    /// (the parent box's version if nested, else the host file, else empty)
    /// into a fresh pool blob in THIS box (creating the row + provenance) and
    /// returns the RW blob file.
    /// Ensure the pool blob backing an UpperFile row EXISTS on disk. A
    /// discard-hunk-reverted row keeps its bytes INLINE in the sqlar `data`
    /// column with NO pool blob (`review::write_current`); every path that
    /// opens an upper blob — copy_up, the FUSE open/setattr/fallocate
    /// handlers — funnels through here first, so such a row is transparently
    /// re-materialized to a standard blob-backed capture row before any read
    /// or write touches it. Without this the box's OWN write path fails
    /// (EIO/ENOENT) against a file whose hunks it had discarded. No-op for
    /// already-blob-backed rows.
    fn ensure_upper_blob(&self, owner: i64, rowid: i64, rel: &str) {
        if !blob_path(owner, rowid).exists() {
            if let Some(ob) = self.box_of(owner) {
                let _ = ob.outline_inline_row(rel);
            }
        }
    }

    fn copy_up(&self, b: &BoxState, rel: &str, pid: u32) -> std::io::Result<File> {
        if self.ro_denied(b.id, rel) {
            return Err(std::io::Error::from_raw_os_error(libc::EROFS));
        }
        let capture = self.inner.mutations.writer(b, pid, self.host_request_pids);
        // Source the lower bytes + mode from the parent-chain resolution.
        let (src, mode, lower_source): (Option<PathBuf>, u32, bool) =
            match self.resolve(b.id, rel) {
            Layer::UpperFile { owner, rowid, mode } => {
                // Re-materialize an inline (discard-reverted) row to its blob
                // so the copy below — and the box's own re-run write — has a
                // source. See ensure_upper_blob.
                self.ensure_upper_blob(owner, rowid, rel);
                (Some(blob_path(owner, rowid)), mode, false)
            }
            Layer::Lower => {
                let mode = self.inner.backing.attr(rel)
                    .map(|attr| attr.mode)
                    .unwrap_or(0o100644);
                (None, mode, true)
            }
            // Unreachable: every mutation path EROFS'd at ro_denied
            // before copy_up could see an attachment-resolved key.
            Layer::ExtFile { .. } =>
                return Err(std::io::Error::from_raw_os_error(libc::EROFS)),
            _ => (None, 0o100644, false),
        };
        let rowid = capture.ensure_file(rel, mode);
        let bp = blob_path(b.id, rowid);
        if let Some(parent) = bp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !bp.exists() {
            if lower_source {
                let destination = File::create(&bp)?;
                self.inner.backing.copy_to(rel, &destination)?;
            } else {
                match src {
                    Some(s) => { std::fs::copy(&s, &bp)?; }
                    None => { File::create(&bp)?; }
                }
            }
        }
        OpenOptions::new().read(true).write(true).open(&bp)
    }

    fn reg_fh(&self, fh: FhInner) -> u64 {
        let lock_identity = match &fh.data {
            FileData::Native(file) => file.metadata().ok()
                .map(|metadata| LockIdentity::Native(metadata.dev(), metadata.ino()))
                .unwrap_or(LockIdentity::Synthetic(u64::MAX)),
            FileData::Lower(lower) => LockIdentity::Lower(lower.identity()),
            FileData::Sink(stream) => LockIdentity::Synthetic(*stream as u64),
        };
        self.inner
            .handles
            .insert(Handle::File(Mutex::new(Fh { inner: fh, lock_identity })))
    }

    /// Merged listing of (box, rel) through the FULL chain: host entries, then
    /// each box from ROOT down to the child applied in order (so a deeper box's
    /// whiteouts hide and its entries override shallower layers). (name, kind,
    /// child-ino, Option<attr>).
    fn scan_dir(&self, b: &BoxState, rel: &str, plus: bool)
                -> Vec<(String, NodeKind, u64, Option<NodeAttr>)> {
        let mut names: BTreeMap<String, ()> = BTreeMap::new();
        // chain of links, root-first (incl. RO attachments).
        let mut chain = self.chain_of(b.id);
        chain.reverse();
        // D-parent: skip host seeding when any box in the chain disables it
        // (matches resolve()'s no_host_fallback semantics — the box stack is
        // closed at the bottom, no /etc-from-host bleed-through).
        let no_host = chain.iter().any(|l| matches!(l,
            ChainLink::Box(bx) if bx.no_host_fallback()));
        if !no_host {
            if let Ok(backing_names) = self.inner.backing.node(rel)
                .and_then(|node| node.read_dir())
            {
                for name in backing_names { names.insert(name, ()); }
            }
        }
        for link in chain {
            let bx = match link {
                ChainLink::Box(bx) => bx,
                // Attachment entries are plain present names — no
                // whiteouts/holes/opacity; kinds come from attr_of below.
                ChainLink::Ext(att) => {
                    for n in att.children(rel) { names.insert(n, ()); }
                    continue;
                }
            };
            {
                // D-opaque: clear lower contributions if THIS box marks `rel`
                // opaque, OR if ANY ANCESTOR of `rel` is opaque here. The
                // OCI/AUFS spec says `.wh..wh..opq` hides every lower entry
                // in that directory's WHOLE subtree, not just the immediate
                // children — so scan_dir of `etc/replace` past a layer with
                // an opaque `etc` must also drop earlier-layer `replace/*`
                // entries. Without the ancestor check, `etc/replace/old`
                // would survive a layer that opacified `etc`.
                if bx.is_opaque(rel)
                    || crate::sarunfs::layers::has_opaque_ancestor(&bx, rel)
                {
                    names.clear();
                }
                // REBASED here (this dir, or an ancestor): everything the
                // chain recorded so far is erased for this subtree; the
                // backdrop (host) still shows through — re-seed it.
                if matches!(bx.entry(rel),
                            Some(Entry::Dir { rebased: true, .. }))
                    || crate::sarunfs::layers::has_rebased_ancestor(&bx, rel)
                {
                    names.clear();
                    if !no_host {
                        if let Ok(backing_names) = self.inner.backing.node(rel)
                            .and_then(|node| node.read_dir())
                        {
                            for name in backing_names { names.insert(name, ()); }
                        }
                    }
                }
                let (white, present, holes) = bx.children_of(rel);
                for w in &white { names.remove(w); }
                for h in &holes {
                    // A hole un-occludes the name: recorded contributions
                    // vanish; the LIVE backdrop decides.
                    names.remove(h);
                    if !no_host {
                        let hp = if rel.is_empty() { h.clone() }
                                 else { format!("{rel}/{h}") };
                        if self.inner.backing.exists(&hp) {
                            names.insert(h.clone(), ());
                        }
                    }
                }
                for p in present { names.insert(p, ()); }
            }
        }
        for name in self.inner.synthetic.projected_children(b.id, rel) {
            names.insert(name, ());
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

    fn directory_snapshot(&self, inode: u64) -> Result<Vec<DirNode>, Errno> {
        if self.getattr_node(inode)?.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        let (bid, rel) = self.key_of(inode).ok_or(Errno::ENOENT)?;
        if bid == 0 {
            return Ok(self
                .inner
                .boxes
                .read()
                .unwrap()
                .keys()
                .map(|id| DirNode {
                    inode: self.ino_for(&(*id, String::new())),
                    kind: NodeKind::Directory,
                    name: id.to_string(),
                })
                .collect());
        }
        if rel == SyntheticNode::Children.name() {
            return Ok(self
                .inner
                .synthetic
                .child_ids(&self.inner.boxes.read().unwrap(), bid)
                .into_iter()
                .map(|id| DirNode {
                    inode: self.ino_for(&(id, String::new())),
                    kind: NodeKind::Directory,
                    name: id.to_string(),
                })
                .collect());
        }
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        Ok(self
            .scan_dir(&b, &rel, false)
            .into_iter()
            .filter(|(name, _, _, _)| {
                let child_rel = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                (b.is_api()
                    && (Self::oaita_config_ancestor_or_self(&child_rel)
                        || Self::oaita_state_ancestor_self_or_within(&child_rel)))
                    || !Self::is_engine_path(&child_rel)
            })
            .map(|(name, kind, inode, _)| DirNode { inode, kind, name })
            .collect())
    }

    fn open_directory(&self, inode: u64) -> Result<u64, Errno> {
        let snapshot = Arc::new(self.directory_snapshot(inode)?);
        Ok(self.inner.handles.insert(Handle::Directory(snapshot)))
    }

    fn read_directory(&self, handle: u64, offset: u64) -> Result<Vec<DirNode>, Errno> {
        let handle = self.inner.handles.get(handle).ok_or(Errno::EBADF)?;
        let Handle::Directory(snapshot) = &*handle else {
            return Err(Errno::EBADF);
        };
        let start = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        Ok(snapshot.iter().skip(start).cloned().collect())
    }

    fn close_directory(&self, handle: u64) -> Result<(), Errno> {
        match self.inner.handles.remove(handle).as_deref() {
            Some(Handle::Directory(_)) => Ok(()),
            _ => Err(Errno::EBADF),
        }
    }
}

fn virtio_error(error: Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(i32::from(error))
}

fn staging_file() -> std::io::Result<File> {
    let name = CStr::from_bytes_with_nul(b"sarun-virtio-write\0").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: memfd_create returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

/// Canonical filesystem protocol implementation. Raw kernel FUSE,
/// vhost-user-fs, and the SUD ring all enter through this one implementation.
impl virtiofsd::filesystem::FileSystem for SarunFs {
    type Inode = u64;
    type Handle = u64;
    type DirIter = crate::sarunfs::DirIter;

    fn init(
        &self,
        capable: virtiofsd::filesystem::FsOptions,
    ) -> std::io::Result<virtiofsd::filesystem::FsOptions> {
        let passthrough = capable.contains(virtiofsd::filesystem::FsOptions::PASSTHROUGH);
        self.kernel_passthrough.store(passthrough, Ordering::Relaxed);
        if self.root.0 == 0 {
            self.inner.passthrough_ok.store(passthrough, Ordering::Relaxed);
        }
        let mut wanted = virtiofsd::filesystem::FsOptions::POSIX_LOCKS
            | virtiofsd::filesystem::FsOptions::FLOCK_LOCKS;
        if passthrough {
            wanted |= virtiofsd::filesystem::FsOptions::PASSTHROUGH;
        }
        Ok(wanted)
    }

    fn lookup(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        parent: u64,
        name: &CStr,
    ) -> std::io::Result<virtiofsd::filesystem::Entry> {
        let attr = self
            .lookup_node(parent, OsStr::from_bytes(name.to_bytes()))
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: attr.inode,
            generation: 0,
            attr: crate::sarunfs::virtio_attr(attr),
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn forget(&self, _ctx: virtiofsd::filesystem::Context, inode: u64, count: u64) {
        self.inner.inodes.forget(inode, count);
    }

    fn getattr(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
        _handle: Option<u64>,
    ) -> std::io::Result<(virtiofsd::fuse::Attr, Duration)> {
        let attr = self.getattr_node(inode).map_err(virtio_error)?;
        Ok((crate::sarunfs::virtio_attr(attr), TTL))
    }

    fn setattr(
        &self,
        ctx: virtiofsd::filesystem::Context,
        inode: u64,
        attr: virtiofsd::fuse::SetattrIn,
        _handle: Option<u64>,
        valid: virtiofsd::filesystem::SetattrValid,
    ) -> std::io::Result<(virtiofsd::fuse::Attr, Duration)> {
        let mtime = if valid.contains(virtiofsd::filesystem::SetattrValid::MTIME_NOW) {
            Some(SystemTime::now())
        } else if valid.contains(virtiofsd::filesystem::SetattrValid::MTIME) {
            Some(UNIX_EPOCH + Duration::new(attr.mtime, attr.mtimensec))
        } else {
            None
        };
        let atime = if valid.contains(virtiofsd::filesystem::SetattrValid::ATIME_NOW) {
            Some(SystemTime::now())
        } else if valid.contains(virtiofsd::filesystem::SetattrValid::ATIME) {
            Some(UNIX_EPOCH + Duration::new(attr.atime, attr.atimensec))
        } else {
            None
        };
        let result = self
            .setattr_node(
                ctx.pid as u32,
                inode,
                NodeSetattr {
                    mode: valid
                        .contains(virtiofsd::filesystem::SetattrValid::MODE)
                        .then_some(attr.mode),
                    uid: valid
                        .contains(virtiofsd::filesystem::SetattrValid::UID)
                        .then_some(attr.uid.into_inner()),
                    gid: valid
                        .contains(virtiofsd::filesystem::SetattrValid::GID)
                        .then_some(attr.gid.into_inner()),
                    size: valid
                        .contains(virtiofsd::filesystem::SetattrValid::SIZE)
                        .then_some(attr.size),
                    atime,
                    mtime,
                },
            )
            .map_err(virtio_error)?;
        Ok((crate::sarunfs::virtio_attr(result), TTL))
    }

    fn readlink(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
    ) -> std::io::Result<Vec<u8>> {
        self.readlink_node(inode).map_err(virtio_error)
    }

    fn open(
        &self,
        ctx: virtiofsd::filesystem::Context,
        inode: u64,
        _kill_priv: bool,
        flags: u32,
    ) -> std::io::Result<(Option<u64>, virtiofsd::filesystem::OpenOptions)> {
        let opened = self
            .open_node(
                ctx.pid as u32,
                inode,
                flags,
                self.kernel_passthrough.load(Ordering::Relaxed),
            )
            .map_err(virtio_error)?;
        let mut options = virtiofsd::filesystem::OpenOptions::empty();
        if opened.direct_io {
            options |= virtiofsd::filesystem::OpenOptions::DIRECT_IO;
        }
        if opened.nonseekable {
            options |= virtiofsd::filesystem::OpenOptions::NONSEEKABLE;
        }
        if opened.keep_cache {
            options |= virtiofsd::filesystem::OpenOptions::KEEP_CACHE;
        }
        if opened.backing_candidate {
            options |= virtiofsd::filesystem::OpenOptions::PASSTHROUGH;
        }
        Ok((Some(opened.handle), options))
    }

    fn backing_file(&self, handle: u64) -> std::io::Result<Option<File>> {
        let handle = self
            .inner
            .handles
            .get(handle)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?;
        let Handle::File(handle) = &*handle else {
            return Ok(None);
        };
        let handle = handle.lock().unwrap();
        if !handle.inner.backing_candidate {
            return Ok(None);
        }
        match &handle.inner.data {
            FileData::Native(file) => file.try_clone().map(Some),
            _ => Ok(None),
        }
    }

    fn create(
        &self,
        ctx: virtiofsd::filesystem::Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _kill_priv: bool,
        _flags: u32,
        _umask: u32,
        _extensions: virtiofsd::filesystem::Extensions,
    ) -> std::io::Result<(
        virtiofsd::filesystem::Entry,
        Option<u64>,
        virtiofsd::filesystem::OpenOptions,
    )> {
        let (attr, opened) = self
            .create_node(
                ctx.pid as u32,
                ctx.uid.into_inner(),
                ctx.gid.into_inner(),
                parent,
                OsStr::from_bytes(name.to_bytes()),
                mode,
            )
            .map_err(virtio_error)?;
        Ok((
            virtiofsd::filesystem::Entry {
                inode: attr.inode,
                generation: 0,
                attr: crate::sarunfs::virtio_attr(attr),
                attr_timeout: TTL,
                entry_timeout: TTL,
            },
            Some(opened.handle),
            virtiofsd::filesystem::OpenOptions::empty(),
        ))
    }

    fn mkdir(
        &self,
        ctx: virtiofsd::filesystem::Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        _umask: u32,
        _extensions: virtiofsd::filesystem::Extensions,
    ) -> std::io::Result<virtiofsd::filesystem::Entry> {
        let attr = self
            .mkdir_node(
                ctx.pid as u32,
                ctx.uid.into_inner(),
                ctx.gid.into_inner(),
                parent,
                OsStr::from_bytes(name.to_bytes()),
                mode,
            )
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: attr.inode,
            generation: 0,
            attr: crate::sarunfs::virtio_attr(attr),
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn mknod(
        &self,
        ctx: virtiofsd::filesystem::Context,
        parent: u64,
        name: &CStr,
        mode: u32,
        rdev: u32,
        _umask: u32,
        _extensions: virtiofsd::filesystem::Extensions,
    ) -> std::io::Result<virtiofsd::filesystem::Entry> {
        let attr = self
            .mknod_node(
                ctx.pid as u32,
                ctx.uid.into_inner(),
                ctx.gid.into_inner(),
                parent,
                OsStr::from_bytes(name.to_bytes()),
                mode,
                rdev,
            )
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: attr.inode,
            generation: 0,
            attr: crate::sarunfs::virtio_attr(attr),
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn symlink(
        &self,
        ctx: virtiofsd::filesystem::Context,
        linkname: &CStr,
        parent: u64,
        name: &CStr,
        _extensions: virtiofsd::filesystem::Extensions,
    ) -> std::io::Result<virtiofsd::filesystem::Entry> {
        let attr = self
            .symlink_node(
                ctx.pid as u32,
                ctx.uid.into_inner(),
                ctx.gid.into_inner(),
                parent,
                OsStr::from_bytes(name.to_bytes()),
                Path::new(OsStr::from_bytes(linkname.to_bytes())),
            )
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: attr.inode,
            generation: 0,
            attr: crate::sarunfs::virtio_attr(attr),
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn unlink(
        &self,
        ctx: virtiofsd::filesystem::Context,
        parent: u64,
        name: &CStr,
    ) -> std::io::Result<()> {
        self.unlink_node(
            ctx.pid as u32,
            parent,
            OsStr::from_bytes(name.to_bytes()),
        )
        .map_err(virtio_error)
    }

    fn rmdir(
        &self,
        ctx: virtiofsd::filesystem::Context,
        parent: u64,
        name: &CStr,
    ) -> std::io::Result<()> {
        self.rmdir_node(
            ctx.pid as u32,
            parent,
            OsStr::from_bytes(name.to_bytes()),
        )
        .map_err(virtio_error)
    }

    fn rename(
        &self,
        ctx: virtiofsd::filesystem::Context,
        olddir: u64,
        oldname: &CStr,
        newdir: u64,
        newname: &CStr,
        flags: u32,
    ) -> std::io::Result<()> {
        self.rename_node(
            ctx.pid as u32,
            olddir,
            OsStr::from_bytes(oldname.to_bytes()),
            newdir,
            OsStr::from_bytes(newname.to_bytes()),
            flags,
        )
        .map_err(virtio_error)
    }

    fn link(
        &self,
        ctx: virtiofsd::filesystem::Context,
        inode: u64,
        newparent: u64,
        newname: &CStr,
    ) -> std::io::Result<virtiofsd::filesystem::Entry> {
        let attr = self
            .link_node(
                ctx.pid as u32,
                inode,
                newparent,
                OsStr::from_bytes(newname.to_bytes()),
            )
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: attr.inode,
            generation: 0,
            attr: crate::sarunfs::virtio_attr(attr),
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn read<W: virtiofsd::filesystem::ZeroCopyWriter>(
        &self,
        ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        mut writer: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> std::io::Result<usize> {
        if size == 0 {
            return Ok(0);
        }
        if let Some((box_id, nonblocking)) = self.jobserver_handle(handle) {
            let slip = if self.host_request_pids {
                self.inner.synthetic.acquire_host_jobserver_blocking(
                    tgid_of(ctx.pid as u32) as i32,
                    nonblocking,
                )?
            } else {
                self.inner.synthetic.acquire_guest_jobserver_blocking(
                    box_id,
                    ctx.pid as u32,
                    nonblocking,
                )?
            };
            let staging = staging_file()?;
            staging.write_at(&[slip], 0)?;
            return writer.read_from_file_at(&staging, 1, 0, None);
        }
        self.read_file_to(handle, writer, size, offset)
    }

    fn write<R: virtiofsd::filesystem::ZeroCopyReader>(
        &self,
        ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        mut reader: R,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _kill_priv: bool,
        _flags: u32,
    ) -> std::io::Result<usize> {
        match self
            .prepare_write_node(ctx.pid as u32, handle)
            .map_err(virtio_error)?
        {
            WriteTarget::Jobserver { box_id } => {
                if self.host_request_pids {
                    self.inner
                        .synthetic
                        .release_host_jobserver(tgid_of(ctx.pid as u32) as i32);
                } else {
                    self.inner
                        .synthetic
                        .release_guest_jobserver(box_id, ctx.pid as u32);
                }
                Ok(size as usize)
            }
            WriteTarget::Sink { box_id, stream } => {
                let staging = staging_file()?;
                let written = reader.write_to_file_at(
                    &staging,
                    size as usize,
                    0,
                    None,
                )?;
                let mut data = vec![0; written];
                let read = staging.read_at(&mut data, 0)?;
                data.truncate(read);
                self.write_sink_node(ctx.pid as u32, box_id, stream, &data);
                Ok(read)
            }
            WriteTarget::File { file, box_id, rel } => {
                let written = reader.write_to_file_at(
                    &file,
                    size as usize,
                    offset,
                    None,
                )?;
                self.finish_file_write(box_id, rel);
                Ok(written)
            }
        }
    }

    fn fallocate(
        &self,
        ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> std::io::Result<()> {
        self.fallocate_node(ctx.pid as u32, handle, mode, offset, length)
            .map_err(virtio_error)
    }

    fn release(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        _flags: u32,
        handle: u64,
        _flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> std::io::Result<()> {
        self.release_lock_owner(handle, (1u64 << 63) | handle, false);
        if flock_release {
            if let Some(owner) = lock_owner {
                self.release_lock_owner(handle, owner, true);
            }
        }
        self.release_node(handle).map_err(virtio_error)
    }

    fn flush(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        lock_owner: u64,
    ) -> std::io::Result<()> {
        self.flush_file_node(handle).map_err(virtio_error)?;
        self.release_lock_owner(handle, lock_owner, false);
        Ok(())
    }

    fn fsync(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        datasync: bool,
        handle: u64,
    ) -> std::io::Result<()> {
        self.sync_file_node(handle, datasync).map_err(virtio_error)
    }

    fn statfs(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
    ) -> std::io::Result<libc::statvfs64> {
        self.statfs_node().map_err(virtio_error)
    }

    fn lseek(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        offset: u64,
        whence: u32,
    ) -> std::io::Result<u64> {
        self.lseek_node(handle, offset, whence).map_err(virtio_error)
    }

    fn getlk(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        owner: u64,
        lock: virtiofsd::fuse::FileLock,
        flags: u32,
    ) -> std::io::Result<virtiofsd::fuse::FileLock> {
        self.get_lock_node(handle, owner, lock, flags).map_err(virtio_error)
    }

    fn setlk(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        owner: u64,
        lock: virtiofsd::fuse::FileLock,
        flags: u32,
    ) -> std::io::Result<()> {
        self.set_lock_node(handle, owner, lock, flags, false).map_err(virtio_error)
    }

    fn setlkw(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        owner: u64,
        lock: virtiofsd::fuse::FileLock,
        flags: u32,
    ) -> std::io::Result<()> {
        self.set_lock_node(handle, owner, lock, flags, true).map_err(virtio_error)
    }

    fn access(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
        _mask: u32,
    ) -> std::io::Result<()> {
        self.getattr_node(inode).map(|_| ()).map_err(virtio_error)
    }

    fn setxattr(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
        name: &CStr,
        value: &[u8],
        flags: u32,
        _extra_flags: virtiofsd::filesystem::SetxattrFlags,
    ) -> std::io::Result<()> {
        self.set_xattr_node(
            inode,
            OsStr::from_bytes(name.to_bytes()),
            value,
            flags,
        )
        .map_err(virtio_error)
    }

    fn getxattr(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
        name: &CStr,
        size: u32,
    ) -> std::io::Result<virtiofsd::filesystem::GetxattrReply> {
        let value = self
            .get_xattr_node(inode, OsStr::from_bytes(name.to_bytes()))
            .map_err(virtio_error)?;
        if size == 0 {
            Ok(virtiofsd::filesystem::GetxattrReply::Count(value.len() as u32))
        } else if (size as usize) < value.len() {
            Err(std::io::Error::from_raw_os_error(libc::ERANGE))
        } else {
            Ok(virtiofsd::filesystem::GetxattrReply::Value(value))
        }
    }

    fn listxattr(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
        size: u32,
    ) -> std::io::Result<virtiofsd::filesystem::ListxattrReply> {
        let names = self.list_xattr_node(inode).map_err(virtio_error)?;
        if size == 0 {
            Ok(virtiofsd::filesystem::ListxattrReply::Count(names.len() as u32))
        } else if (size as usize) < names.len() {
            Err(std::io::Error::from_raw_os_error(libc::ERANGE))
        } else {
            Ok(virtiofsd::filesystem::ListxattrReply::Names(names))
        }
    }

    fn removexattr(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
        name: &CStr,
    ) -> std::io::Result<()> {
        self.remove_xattr_node(inode, OsStr::from_bytes(name.to_bytes()))
            .map_err(virtio_error)
    }

    fn opendir(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        inode: u64,
        _flags: u32,
    ) -> std::io::Result<(Option<u64>, virtiofsd::filesystem::OpenOptions)> {
        let handle = self.open_directory(inode).map_err(virtio_error)?;
        Ok((Some(handle), virtiofsd::filesystem::OpenOptions::empty()))
    }

    fn readdir(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        _size: u32,
        offset: u64,
    ) -> std::io::Result<Self::DirIter> {
        let entries = self
            .read_directory(handle, offset)
            .map_err(virtio_error)?
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                (
                    entry.inode,
                    offset.saturating_add(index as u64).saturating_add(1),
                    entry.kind,
                    entry.name,
                )
            })
            .collect();
        Ok(crate::sarunfs::DirIter::new(entries))
    }

    fn releasedir(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        _flags: u32,
        handle: u64,
    ) -> std::io::Result<()> {
        self.close_directory(handle).map_err(virtio_error)
    }

    fn fsyncdir(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        _datasync: bool,
        _handle: u64,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

/// Live migration is intentionally unsupported in the first appliance
/// generation.  The upstream trait's default methods return `Unsupported`.
impl virtiofsd::filesystem::SerializableFileSystem for SarunFs {}

#[cfg(test)]
mod chain_tests {
    use super::*;
    use crate::capture::{BoxState, ExtRef, RoAttachment};
    use std::ffi::CString;

    struct CollectWriter(Arc<Mutex<Vec<u8>>>);

    struct BytesReader(&'static [u8]);

    impl virtiofsd::filesystem::ZeroCopyWriter for CollectWriter {
        fn read_from_file_at(
            &mut self,
            file: &File,
            count: usize,
            offset: u64,
            _flags: Option<virtiofsd::oslib::ReadvFlags>,
        ) -> std::io::Result<usize> {
            let mut bytes = vec![0; count];
            let read = file.read_at(&mut bytes, offset)?;
            self.0.lock().unwrap().extend_from_slice(&bytes[..read]);
            Ok(read)
        }
    }

    impl virtiofsd::filesystem::ZeroCopyReader for BytesReader {
        fn write_to_file_at(
            &mut self,
            file: &File,
            count: usize,
            offset: u64,
            _flags: Option<virtiofsd::oslib::WritevFlags>,
        ) -> std::io::Result<usize> {
            file.write_at(&self.0[..count.min(self.0.len())], offset)
        }
    }

    fn ext_ref(prefix: &str) -> ExtRef {
        ExtRef { kind: "git".into(), store: "/nonexistent".into(),
                 refname: "main".into(), rev: "abc".into(),
                 prefix: prefix.into(), name: "t".into() }
    }

    // chain_of must honor the FULL interleaved ro_attachments order —
    // Box and Ext rows at their list positions, never grouped by kind
    // (resolve precedence is the list order the attach verbs recorded).
    // The Ext link's synthesized prefix chain must then show through
    // resolve/ro_denied/chain_has_children WITHOUT the store (here
    // nonexistent) ever opening.
    #[test]
    fn chain_interleaves_box_and_ext_rows_in_list_order() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "sarun-chain-{}-{:?}", std::process::id(),
            std::time::SystemTime::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: same discipline as review.rs's promote test — the lock
        // serializes every state_home-reading test in this binary.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let ov = Overlay::new(tmp.clone());
        let (owner, a, c) = (9101i64, 9102i64, 9103i64);
        for id in [a, c] {
            ov.add_box(Arc::new(BoxState::create(id).unwrap()));
        }
        let ob = BoxState::create(owner).unwrap();
        ob.set_ro_attachments(vec![
            RoAttachment::Box(a),
            RoAttachment::Ext(ext_ref("deep/sdk")),
            RoAttachment::Box(c),
        ]);
        ov.add_box(Arc::new(ob));

        let tags: Vec<String> = ov.chain_of(owner).iter().map(|l| match l {
            ChainLink::Box(b) => format!("box:{}", b.id),
            ChainLink::Ext(att) => format!("ext:{}", att.ext.prefix),
        }).collect();
        assert_eq!(tags, ["box:9101", "box:9102", "ext:deep/sdk",
                          "box:9103"]);

        assert!(matches!(ov.resolve(owner, "deep"), Layer::UpperDir { .. }));
        assert!(ov.ro_denied(owner, "deep"));
        assert!(crate::sarunfs::layers::chain_has_children(
            &ov.chain_of(owner),
            "",
        ));
        assert_eq!(ov.box_path_kind(owner, "deep"), 'd');
        // Off-prefix rels miss the attachment and stay writable.
        assert!(!ov.ro_denied(owner, "elsewhere"));
    }

    #[test]
    fn canonical_virtio_lookup_uses_the_shared_policy_and_lifetime() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "sarun-canonical-fs-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("hello"), b"canonical bytes").unwrap();
        std::fs::write(tmp.join("truncate-open"), b"tail must disappear").unwrap();
        // SAFETY: TEST_STATE_HOME_LOCK serializes state-home tests.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let fs = SarunFs::new(tmp.clone());
        let id = 9201;
        fs.add_box(Arc::new(BoxState::create(id).unwrap()));
        let ctx = virtiofsd::filesystem::Context {
            uid: 1234.into(),
            gid: 2345.into(),
            pid: 1,
        };
        let export = fs.export_box(id).unwrap();
        let (export_root, _) =
            <SarunFs as virtiofsd::filesystem::FileSystem>::getattr(
                &export, ctx, 1, None,
            )
            .unwrap();
        assert_eq!(export_root.ino, 1);
        let export_filename = CString::new("hello").unwrap();
        let export_file = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &export,
            ctx,
            1,
            &export_filename,
        )
        .unwrap();
        assert_ne!(export_file.inode, 1);
        let name = CString::new(id.to_string()).unwrap();
        let entry = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs,
            ctx,
            1,
            &name,
        )
        .unwrap();
        assert_eq!(entry.attr.mode & libc::S_IFMT, libc::S_IFDIR);
        assert_eq!(fs.inner.inodes.lookup_count(entry.inode), 1);

        let xattr = CString::new("user.sarun-test").unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::setxattr(
            &fs,
            ctx,
            entry.inode,
            &xattr,
            b"value",
            libc::XATTR_CREATE as u32,
            virtiofsd::filesystem::SetxattrFlags::empty(),
        )
        .unwrap();
        let duplicate = <SarunFs as virtiofsd::filesystem::FileSystem>::setxattr(
            &fs,
            ctx,
            entry.inode,
            &xattr,
            b"other",
            libc::XATTR_CREATE as u32,
            virtiofsd::filesystem::SetxattrFlags::empty(),
        )
        .err()
        .unwrap();
        assert_eq!(duplicate.raw_os_error(), Some(libc::EEXIST));
        match <SarunFs as virtiofsd::filesystem::FileSystem>::getxattr(
            &fs,
            ctx,
            entry.inode,
            &xattr,
            32,
        )
        .unwrap()
        {
            virtiofsd::filesystem::GetxattrReply::Value(value) =>
                assert_eq!(value, b"value"),
            virtiofsd::filesystem::GetxattrReply::Count(_) => panic!("expected value"),
        }
        assert!(matches!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::getxattr(
                &fs, ctx, entry.inode, &xattr, 0,
            )
            .unwrap(),
            virtiofsd::filesystem::GetxattrReply::Count(5),
        ));
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::getxattr(
                &fs, ctx, entry.inode, &xattr, 1,
            )
            .err()
            .unwrap()
            .raw_os_error(),
            Some(libc::ERANGE),
        );
        let xattr_bytes = xattr.as_bytes_with_nul();
        assert!(matches!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::listxattr(
                &fs, ctx, entry.inode, 0,
            )
            .unwrap(),
            virtiofsd::filesystem::ListxattrReply::Count(count)
                if count as usize == xattr_bytes.len(),
        ));
        match <SarunFs as virtiofsd::filesystem::FileSystem>::listxattr(
            &fs,
            ctx,
            entry.inode,
            xattr_bytes.len() as u32,
        )
        .unwrap()
        {
            virtiofsd::filesystem::ListxattrReply::Names(names) =>
                assert_eq!(names, xattr_bytes),
            virtiofsd::filesystem::ListxattrReply::Count(_) => panic!("expected names"),
        }
        <SarunFs as virtiofsd::filesystem::FileSystem>::removexattr(
            &fs, ctx, entry.inode, &xattr,
        )
        .unwrap();
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::getxattr(
                &fs, ctx, entry.inode, &xattr, 32,
            )
            .err()
            .unwrap()
            .raw_os_error(),
            Some(libc::ENODATA),
        );

        // Some clients express shell `>` solely as OPEN(O_TRUNC), while a
        // Linux FUSE client may additionally emit SETATTR(size=0). The shared
        // core must implement the open flag itself so transports agree.
        let truncate_name = CString::new("truncate-open").unwrap();
        let truncate_entry = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs, ctx, entry.inode, &truncate_name,
        )
        .unwrap();
        let (truncate_handle, _) = <SarunFs as virtiofsd::filesystem::FileSystem>::open(
            &fs,
            ctx,
            truncate_entry.inode,
            false,
            (libc::O_WRONLY | libc::O_TRUNC) as u32,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs,
            ctx,
            truncate_entry.inode,
            0,
            truncate_handle.unwrap(),
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::getattr(
                &fs, ctx, truncate_entry.inode, None,
            )
            .unwrap()
            .0
            .size,
            0,
        );
        assert_eq!(
            std::fs::read(tmp.join("truncate-open")).unwrap(),
            b"tail must disappear",
        );
        let Layer::UpperFile { owner, rowid, .. } = fs.resolve(id, "truncate-open") else {
            panic!("O_TRUNC did not copy up the lower file");
        };
        assert_eq!(blob_path(owner, rowid).metadata().unwrap().len(), 0);

        let filename = CString::new("hello").unwrap();
        let file_entry = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs,
            ctx,
            entry.inode,
            &filename,
        )
        .unwrap();
        let (file_handle, _) = <SarunFs as virtiofsd::filesystem::FileSystem>::open(
            &fs,
            ctx,
            file_entry.inode,
            false,
            libc::O_RDONLY as u32,
        )
        .unwrap();
        let file_handle = file_handle.unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let read = <SarunFs as virtiofsd::filesystem::FileSystem>::read(
            &fs,
            ctx,
            file_entry.inode,
            file_handle,
            CollectWriter(captured.clone()),
            64,
            0,
            None,
            0,
        )
        .unwrap();
        assert_eq!(read, b"canonical bytes".len());
        assert_eq!(&*captured.lock().unwrap(), b"canonical bytes");
        let exported = fs.export_handle(file_handle, false, ctx.pid as u32).unwrap();
        let mut exported_bytes = [0u8; 32];
        let exported_len = exported.read_at(&mut exported_bytes, 0).unwrap();
        assert_eq!(&exported_bytes[..exported_len], b"canonical bytes");
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs,
            ctx,
            file_entry.inode,
            0,
            file_handle,
            false,
            false,
            None,
        )
        .unwrap();

        let (write_handle, _) = <SarunFs as virtiofsd::filesystem::FileSystem>::open(
            &fs,
            ctx,
            file_entry.inode,
            false,
            libc::O_RDWR as u32,
        )
        .unwrap();
        let write_handle = write_handle.unwrap();
        let mapped = fs.export_handle(write_handle, true, ctx.pid as u32).unwrap();
        assert_eq!(mapped.write_at(b"MAP", 0).unwrap(), 3);
        let written = <SarunFs as virtiofsd::filesystem::FileSystem>::write(
            &fs,
            ctx,
            file_entry.inode,
            write_handle,
            BytesReader(b"changed"),
            7,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();
        assert_eq!(written, 7);
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs,
            ctx,
            file_entry.inode,
            0,
            write_handle,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(tmp.join("hello")).unwrap(), b"canonical bytes");
        let layer = fs.resolve(id, "hello");
        let Layer::UpperFile { owner, rowid, .. } = layer else {
            panic!("write did not copy up");
        };
        assert_eq!(std::fs::read(blob_path(owner, rowid)).unwrap(), b"changedal bytes");

        let created_name = CString::new("created").unwrap();
        let (created, created_handle, _) =
            <SarunFs as virtiofsd::filesystem::FileSystem>::create(
                &fs,
                ctx,
                entry.inode,
                &created_name,
                0o640,
                false,
                libc::O_RDWR as u32,
                0,
                virtiofsd::filesystem::Extensions::default(),
            )
            .unwrap();
        assert_eq!(created.attr.mode & 0o7777, 0o640);
        assert_eq!(created.attr.uid.into_inner(), 1234);
        assert_eq!(created.attr.gid.into_inner(), 2345);
        assert_eq!(fs.inner.inodes.lookup_count(created.inode), 1);
        let created_handle = created_handle.unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::write(
            &fs,
            ctx,
            created.inode,
            created_handle,
            BytesReader(b"new file"),
            8,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs,
            ctx,
            created.inode,
            0,
            created_handle,
            false,
            false,
            None,
        )
        .unwrap();
        let Layer::UpperFile { owner, rowid, .. } = fs.resolve(id, "created") else {
            panic!("create did not materialize upper file");
        };
        assert_eq!(std::fs::read(blob_path(owner, rowid)).unwrap(), b"new file");
        <SarunFs as virtiofsd::filesystem::FileSystem>::forget(
            &fs,
            ctx,
            created.inode,
            1,
        );
        let allocated_name = CString::new("allocated").unwrap();
        let (allocated, allocated_handle, _) =
            <SarunFs as virtiofsd::filesystem::FileSystem>::create(
                &fs,
                ctx,
                entry.inode,
                &allocated_name,
                0o600,
                false,
                libc::O_RDWR as u32,
                0,
                virtiofsd::filesystem::Extensions::default(),
            )
            .unwrap();
        let allocated_handle = allocated_handle.unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::fallocate(
            &fs,
            ctx,
            allocated.inode,
            allocated_handle,
            0,
            0,
            4096,
        )
        .unwrap();
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::write(
                &fs,
                ctx,
                allocated.inode,
                allocated_handle,
                BytesReader(b"x"),
                1,
                8192,
                None,
                false,
                false,
                0,
            )
            .unwrap(),
            1,
        );
        let hole = <SarunFs as virtiofsd::filesystem::FileSystem>::lseek(
            &fs,
            ctx,
            allocated.inode,
            allocated_handle,
            0,
            libc::SEEK_HOLE as u32,
        )
        .unwrap();
        assert!(hole <= 8192, "hole must precede the distant data byte: {hole}");
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::lseek(
                &fs,
                ctx,
                allocated.inode,
                allocated_handle,
                hole,
                libc::SEEK_DATA as u32,
            )
            .unwrap(),
            8192,
        );
        let (lock_handle, _) = <SarunFs as virtiofsd::filesystem::FileSystem>::open(
            &fs,
            ctx,
            allocated.inode,
            false,
            libc::O_RDWR as u32,
        )
        .unwrap();
        let lock_handle = lock_handle.unwrap();
        let lock = virtiofsd::fuse::FileLock {
            start: 10,
            end: 19,
            type_: libc::F_WRLCK as u32,
            pid: 123,
        };
        <SarunFs as virtiofsd::filesystem::FileSystem>::setlk(
            &fs, ctx, allocated.inode, allocated_handle, 100, lock, 0,
        )
        .unwrap();
        let conflict = <SarunFs as virtiofsd::filesystem::FileSystem>::getlk(
            &fs, ctx, allocated.inode, lock_handle, 200, lock, 0,
        )
        .unwrap();
        assert_eq!((conflict.type_, conflict.pid), (libc::F_WRLCK as u32, 123));
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::setlk(
                &fs, ctx, allocated.inode, lock_handle, 200, lock, 0,
            )
            .err()
            .unwrap()
            .raw_os_error(),
            Some(libc::EAGAIN),
        );
        <SarunFs as virtiofsd::filesystem::FileSystem>::fsync(
            &fs, ctx, allocated.inode, true, allocated_handle,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::flush(
            &fs, ctx, allocated.inode, allocated_handle, 100,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::setlk(
            &fs, ctx, allocated.inode, lock_handle, 200, lock, 0,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs, ctx, allocated.inode, 0, lock_handle, false, false, None,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs,
            ctx,
            allocated.inode,
            0,
            allocated_handle,
            false,
            false,
            None,
        )
        .unwrap();
        let Layer::UpperFile { owner, rowid, .. } = fs.resolve(id, "allocated") else {
            panic!("fallocate target missing");
        };
        assert_eq!(blob_path(owner, rowid).metadata().unwrap().len(), 8193);
        let mut setattr = virtiofsd::fuse::SetattrIn::default();
        setattr.size = 128;
        setattr.mode = 0o640;
        setattr.atime = 123;
        setattr.atimensec = 456;
        setattr.mtime = 789;
        setattr.mtimensec = 12;
        setattr.uid = unsafe { libc::geteuid() }.into();
        setattr.gid = unsafe { libc::getegid() }.into();
        let (allocated_attr, _) =
            <SarunFs as virtiofsd::filesystem::FileSystem>::setattr(
                &fs,
                ctx,
                allocated.inode,
                setattr,
                None,
                virtiofsd::filesystem::SetattrValid::SIZE
                    | virtiofsd::filesystem::SetattrValid::MODE
                    | virtiofsd::filesystem::SetattrValid::UID
                    | virtiofsd::filesystem::SetattrValid::GID
                    | virtiofsd::filesystem::SetattrValid::ATIME
                    | virtiofsd::filesystem::SetattrValid::MTIME,
            )
            .unwrap();
        assert_eq!(allocated_attr.size, 128);
        assert_eq!(allocated_attr.mode & 0o7777, 0o640);
        assert_eq!(allocated_attr.atime, 123);
        assert_eq!(allocated_attr.atimensec, 456);
        assert_eq!(allocated_attr.mtime, 789);
        assert_eq!(allocated_attr.mtimensec, 12);
        assert_eq!(allocated_attr.uid.into_inner(), unsafe { libc::geteuid() });
        assert_eq!(allocated_attr.gid.into_inner(), unsafe { libc::getegid() });
        assert_eq!(blob_path(owner, rowid).metadata().unwrap().len(), 128);
        let allocated_sqlar_size: i64 = fs
            .box_of(id)
            .unwrap()
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT sz FROM sqlar WHERE name='allocated'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(allocated_sqlar_size, 128);
        let directory_name = CString::new("directory").unwrap();
        let directory = <SarunFs as virtiofsd::filesystem::FileSystem>::mkdir(
            &fs,
            ctx,
            entry.inode,
            &directory_name,
            0o750,
            0,
            virtiofsd::filesystem::Extensions::default(),
        )
        .unwrap();
        assert_eq!(directory.attr.mode & libc::S_IFMT, libc::S_IFDIR);
        assert_eq!(directory.attr.uid.into_inner(), 1234);
        assert_eq!(directory.attr.gid.into_inner(), 2345);
        let fifo_name = CString::new("fifo").unwrap();
        let fifo = <SarunFs as virtiofsd::filesystem::FileSystem>::mknod(
            &fs,
            ctx,
            entry.inode,
            &fifo_name,
            libc::S_IFIFO | 0o600,
            0,
            0,
            virtiofsd::filesystem::Extensions::default(),
        )
        .unwrap();
        assert_eq!(fifo.attr.mode & libc::S_IFMT, libc::S_IFIFO);
        assert_eq!(fifo.attr.uid.into_inner(), 1234);
        assert_eq!(fifo.attr.gid.into_inner(), 2345);
        let link_name = CString::new("link").unwrap();
        let link_target = CString::new("created").unwrap();
        let link = <SarunFs as virtiofsd::filesystem::FileSystem>::symlink(
            &fs,
            ctx,
            &link_target,
            entry.inode,
            &link_name,
            virtiofsd::filesystem::Extensions::default(),
        )
        .unwrap();
        assert_eq!(link.attr.uid.into_inner(), 1234);
        assert_eq!(link.attr.gid.into_inner(), 2345);
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::readlink(
                &fs, ctx, link.inode,
            )
            .unwrap(),
            b"created",
        );
        let hardlink_name = CString::new("hardlink").unwrap();
        // The create lookup was explicitly forgotten above. A FUSE client
        // must reacquire the name before using its inode in a later LINK;
        // passing a forgotten numeric id tests stale-id use, not hard links.
        let created_for_link =
            <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
                &fs, ctx, entry.inode, &created_name,
            )
            .unwrap();
        let hardlink = <SarunFs as virtiofsd::filesystem::FileSystem>::link(
            &fs,
            ctx,
            created_for_link.inode,
            entry.inode,
            &hardlink_name,
        )
        .unwrap();
        assert_eq!(hardlink.attr.nlink, 2);
        let Layer::UpperFile { rowid: source_rowid, .. } = fs.resolve(id, "created") else {
            panic!("hardlink source missing");
        };
        let Layer::UpperFile { rowid: linked_rowid, .. } = fs.resolve(id, "hardlink") else {
            panic!("hardlink destination missing");
        };
        assert_eq!(
            blob_path(id, source_rowid).metadata().unwrap().ino(),
            blob_path(id, linked_rowid).metadata().unwrap().ino(),
        );
        let renamed_name = CString::new("renamed").unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::rename(
            &fs,
            ctx,
            entry.inode,
            &created_name,
            entry.inode,
            &renamed_name,
            0,
        )
        .unwrap();
        let renamed = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs, ctx, entry.inode, &renamed_name,
        )
        .unwrap();
        assert_eq!(renamed.inode, created_for_link.inode);
        <SarunFs as virtiofsd::filesystem::FileSystem>::unlink(
            &fs, ctx, entry.inode, &link_name,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::unlink(
            &fs, ctx, entry.inode, &hardlink_name,
        )
        .unwrap();
        let renamed_after_unlink =
            <SarunFs as virtiofsd::filesystem::FileSystem>::getattr(
                &fs, ctx, renamed.inode, None,
            )
            .unwrap()
            .0;
        assert_eq!(renamed_after_unlink.nlink, 1);
        <SarunFs as virtiofsd::filesystem::FileSystem>::unlink(
            &fs, ctx, entry.inode, &fifo_name,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::unlink(
            &fs, ctx, entry.inode, &renamed_name,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::rmdir(
            &fs, ctx, entry.inode, &directory_name,
        )
        .unwrap();
        let missing = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs, ctx, entry.inode, &renamed_name,
        )
        .err()
        .unwrap();
        assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));

        let (attr, _) = <SarunFs as virtiofsd::filesystem::FileSystem>::getattr(
            &fs,
            ctx,
            entry.inode,
            None,
        )
        .unwrap();
        assert_eq!(attr.ino, entry.inode);
        <SarunFs as virtiofsd::filesystem::FileSystem>::forget(
            &fs,
            ctx,
            entry.inode,
            1,
        );
        assert_eq!(fs.inner.inodes.lookup_count(entry.inode), 0);

        // Independent canonical requests may be served by different FUSE,
        // SUD, or virtio-fs workers. Exercise the shared inode, handle,
        // capture, and SQLite boundaries concurrently rather than only their
        // private unit-level locks.
        const PARALLEL_BYTES: [&[u8]; 8] = [
            b"worker-0", b"worker-1", b"worker-2", b"worker-3",
            b"worker-4", b"worker-5", b"worker-6", b"worker-7",
        ];
        let mut workers = Vec::new();
        let canonical_root =
            <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
                &fs, ctx, 1, &name,
            )
            .unwrap()
            .inode;
        for index in 0..8 {
            let fs = fs.clone();
            workers.push(std::thread::spawn(move || {
                let name = CString::new(format!("parallel-{index}")).unwrap();
                let (file, handle, _) =
                    <SarunFs as virtiofsd::filesystem::FileSystem>::create(
                        &fs,
                        ctx,
                        canonical_root,
                        &name,
                        0o600,
                        false,
                        libc::O_RDWR as u32,
                        0,
                        virtiofsd::filesystem::Extensions::default(),
                    )
                    .unwrap();
                let handle = handle.unwrap();
                let bytes = PARALLEL_BYTES[index];
                <SarunFs as virtiofsd::filesystem::FileSystem>::write(
                    &fs,
                    ctx,
                    file.inode,
                    handle,
                    BytesReader(bytes),
                    bytes.len() as u32,
                    0,
                    None,
                    false,
                    false,
                    0,
                )
                .unwrap();
                <SarunFs as virtiofsd::filesystem::FileSystem>::release(
                    &fs, ctx, file.inode, 0, handle, false, false, None,
                )
                .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        for index in 0..8 {
            assert_eq!(
                fs.box_read_file(id, &format!("parallel-{index}")).unwrap(),
                PARALLEL_BYTES[index],
            );
        }

        let stat = <SarunFs as virtiofsd::filesystem::FileSystem>::statfs(
            &fs, ctx, canonical_root,
        )
        .unwrap();
        assert!(stat.f_bsize > 0);
        assert!(stat.f_blocks >= stat.f_bfree);

        fs.add_box(Arc::new(BoxState::create(id + 2).unwrap()));
        let (handle, _) = <SarunFs as virtiofsd::filesystem::FileSystem>::opendir(
            &fs, ctx, 1, 0,
        )
        .unwrap();
        let handle = handle.unwrap();
        fs.add_box(Arc::new(BoxState::create(id + 1).unwrap()));
        let mut entries = <SarunFs as virtiofsd::filesystem::FileSystem>::readdir(
            &fs, ctx, 1, handle, 4096, 0,
        )
        .unwrap();
        let first = virtiofsd::filesystem::DirectoryIterator::next(&mut entries).unwrap();
        assert_eq!(first.name.to_bytes(), id.to_string().as_bytes());
        let snapshot_second =
            virtiofsd::filesystem::DirectoryIterator::next(&mut entries).unwrap();
        assert_eq!(snapshot_second.name.to_bytes(), (id + 2).to_string().as_bytes());
        assert!(virtiofsd::filesystem::DirectoryIterator::next(&mut entries).is_none());
        let mut resumed = <SarunFs as virtiofsd::filesystem::FileSystem>::readdir(
            &fs, ctx, 1, handle, 4096, 1,
        )
        .unwrap();
        let second = virtiofsd::filesystem::DirectoryIterator::next(&mut resumed).unwrap();
        assert_eq!(second.name.to_bytes(), (id + 2).to_string().as_bytes());
        assert!(virtiofsd::filesystem::DirectoryIterator::next(&mut resumed).is_none());
        <SarunFs as virtiofsd::filesystem::FileSystem>::releasedir(
            &fs, ctx, 1, 0, handle,
        )
        .unwrap();

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn canonical_open_handles_survive_unlink_and_rename_over_without_rebinding() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "sarun-open-lifetime-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("lower-victim"), b"lower-old").unwrap();
        // SAFETY: TEST_STATE_HOME_LOCK serializes state-home tests.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let fs = SarunFs::new(tmp.clone());
        let id = 9401;
        fs.add_box(Arc::new(BoxState::create(id).unwrap()));
        let ctx = virtiofsd::filesystem::Context {
            uid: 0.into(),
            gid: 0.into(),
            pid: std::process::id() as i32,
        };
        let box_name = CString::new(id.to_string()).unwrap();
        let root = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs, ctx, 1, &box_name,
        )
        .unwrap();

        // A lazy lower handle is privately materialized before unlink. Later
        // writes remain readable through the fd but neither touch the host nor
        // recreate the deleted namespace entry.
        let victim_name = CString::new("lower-victim").unwrap();
        let victim = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs, ctx, root.inode, &victim_name,
        )
        .unwrap();
        let (victim_handle, _) = <SarunFs as virtiofsd::filesystem::FileSystem>::open(
            &fs, ctx, victim.inode, false, libc::O_RDWR as u32,
        )
        .unwrap();
        let victim_handle = victim_handle.unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::unlink(
            &fs, ctx, root.inode, &victim_name,
        )
        .unwrap();
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
                &fs, ctx, root.inode, &victim_name,
            )
            .err()
            .unwrap()
            .raw_os_error(),
            Some(libc::ENOENT),
        );
        <SarunFs as virtiofsd::filesystem::FileSystem>::write(
            &fs,
            ctx,
            victim.inode,
            victim_handle,
            BytesReader(b"PRIVATE"),
            7,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();
        let detached_bytes = Arc::new(Mutex::new(Vec::new()));
        <SarunFs as virtiofsd::filesystem::FileSystem>::read(
            &fs,
            ctx,
            victim.inode,
            victim_handle,
            CollectWriter(detached_bytes.clone()),
            32,
            0,
            None,
            0,
        )
        .unwrap();
        assert_eq!(&*detached_bytes.lock().unwrap(), b"PRIVATEld");
        assert_eq!(std::fs::read(tmp.join("lower-victim")).unwrap(), b"lower-old");
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs, ctx, victim.inode, 0, victim_handle, false, false, None,
        )
        .unwrap();
        assert!(matches!(fs.resolve(id, "lower-victim"), Layer::Absent));

        let create = |name: &CString| {
            <SarunFs as virtiofsd::filesystem::FileSystem>::create(
                &fs,
                ctx,
                root.inode,
                name,
                0o600,
                false,
                libc::O_RDWR as u32,
                0,
                virtiofsd::filesystem::Extensions::default(),
            )
            .unwrap()
        };
        let source_name = CString::new("source").unwrap();
        let destination_name = CString::new("destination").unwrap();
        let (source, source_handle, _) = create(&source_name);
        let source_handle = source_handle.unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::write(
            &fs,
            ctx,
            source.inode,
            source_handle,
            BytesReader(b"SOURCE"),
            6,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();
        let (destination, destination_handle, _) = create(&destination_name);
        let destination_handle = destination_handle.unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::write(
            &fs,
            ctx,
            destination.inode,
            destination_handle,
            BytesReader(b"DEST"),
            4,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();

        <SarunFs as virtiofsd::filesystem::FileSystem>::rename(
            &fs,
            ctx,
            root.inode,
            &source_name,
            root.inode,
            &destination_name,
            0,
        )
        .unwrap();
        let visible = <SarunFs as virtiofsd::filesystem::FileSystem>::lookup(
            &fs, ctx, root.inode, &destination_name,
        )
        .unwrap();
        assert_eq!(visible.inode, source.inode);
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::getattr(
                &fs, ctx, destination.inode, Some(destination_handle),
            )
            .unwrap()
            .0
            .size,
            4,
        );

        // The overwritten destination fd is anonymous; modifying it cannot
        // mutate the source that now occupies its former name.
        <SarunFs as virtiofsd::filesystem::FileSystem>::write(
            &fs,
            ctx,
            destination.inode,
            destination_handle,
            BytesReader(b"OLD!"),
            4,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs,
            ctx,
            destination.inode,
            0,
            destination_handle,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(fs.box_read_file(id, "destination").unwrap(), b"SOURCE");

        // The source fd follows the rename and continues to finalize the new
        // pathname, never the vanished source name.
        <SarunFs as virtiofsd::filesystem::FileSystem>::write(
            &fs,
            ctx,
            source.inode,
            source_handle,
            BytesReader(b"RENAMED"),
            7,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::release(
            &fs, ctx, source.inode, 0, source_handle, false, false, None,
        )
        .unwrap();
        assert_eq!(fs.box_read_file(id, "destination").unwrap(), b"RENAMED");
        assert!(fs.box_read_file(id, "source").is_err());

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn projected_file_is_visible_executable_read_only_and_not_captured() {
        let _g = crate::depot::TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "sarun-projection-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let source = tmp.join("appliance-init");
        std::fs::write(&source, b"target init").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: TEST_STATE_HOME_LOCK serializes state-home tests.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let fs = SarunFs::new(tmp.clone());
        let id = 9301;
        let state = Arc::new(BoxState::create(id).unwrap());
        fs.add_box(state.clone());
        fs.project_file(id, "init", source).unwrap();

        assert_eq!(fs.box_read_file(id, "init").unwrap(), b"target init");
        assert_eq!(fs.box_file_mode(id, "init"), Some(0o755));
        assert!(fs.box_list_dir(id, "").unwrap().iter()
            .any(|(name, kind)| name == "init" && *kind == 'f'));
        let error = fs.box_write_file(id, "init", b"overwrite").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EROFS));
        assert!(state.kinds.read().unwrap().get("init").is_none());

        fs.remove_box(id);
        assert_eq!(fs.inner.synthetic.projection_count(), 0);
        let _ = std::fs::remove_dir_all(tmp);
    }
}
