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
use crate::depot::blob_path;

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
type Key = crate::sarunfs::NodeKey;

/// True if any ANCESTOR directory of `rel` is marked OPAQUE in box `b`. Walks
/// rel's path components upward (rel="a/b/c/d" → checks "a/b/c", "a/b", "a",
/// then the box root ""). The box root itself IS a valid opaque target — a
/// layer can carry a `.wh..wh..opq` directly at its top to opacify EVERYTHING
/// from below. Root rel ("") has no ancestors → false.
/// Any ancestor dir of `rel` (or the box root) marked REBASED in this
/// box (backdrop-anchored, DEPOT-DESIGN.md §2): everything recorded
/// BELOW this box is erased for that subtree, while the backdrop (host)
/// shows through — so the chain walk must stop here and fall to host.
fn has_rebased_ancestor(b: &BoxState, rel: &str) -> bool {
    if rel.is_empty() { return false; }
    let mut p = Path::new(rel).parent();
    while let Some(ancestor) = p {
        let s = ancestor.to_string_lossy();
        let s = s.as_ref();
        if matches!(b.entry(s), Some(Entry::Dir { rebased: true, .. })) {
            return true;
        }
        if s.is_empty() { break; }
        p = ancestor.parent();
    }
    false
}

fn has_opaque_ancestor(b: &BoxState, rel: &str) -> bool {
    if rel.is_empty() { return false; }
    let mut p = Path::new(rel).parent();
    while let Some(ancestor) = p {
        let s = ancestor.to_string_lossy();
        let s = s.as_ref();
        // Note: s.is_empty() == true is the BOX ROOT — a valid opacity
        // target. is_opaque("") asks "is the box root marked opaque?".
        if b.is_opaque(s) {
            return true;
        }
        if s.is_empty() { break; }   // walked past the root; stop
        p = ancestor.parent();
    }
    false
}

/// Clone-able handle: fuser owns one clone as the mounted filesystem, the
/// control plane holds another to add/remove boxes. All state is behind the
/// shared Inner.
#[derive(Clone)]
pub struct SarunFs {
    inner: Arc<Inner>,
    root: Key,
}

/// Transitional source-compatible name for control-plane callers.  Filesystem
/// policy lives in `SarunFs`; transports must not grow another implementation.
pub type Overlay = SarunFs;

struct Inner {
    lower: PathBuf,
    boxes: RwLock<BTreeMap<i64, Arc<BoxState>>>,
    inodes: crate::sarunfs::InodeTable,
    detached_attrs: RwLock<HashMap<u64, FileAttr>>,
    fhs: RwLock<HashMap<u64, Mutex<Fh>>>,
    dir_fhs: RwLock<HashMap<u64, Arc<Vec<DirNode>>>>,
    next_fh: AtomicU64,
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
    // Open handles to the synthetic JOBSERVER file: fh -> is-O_NONBLOCK. Kept
    // apart from `fhs` (no blob/file behind them) so read/write dispatch to the
    // slip pool instead of a backing file.
    jobserver_fhs: RwLock<HashMap<u64, bool>>,
    rules: RwLock<crate::rules::Rules>,  // passthrough decisions (reload verb)
    /// Lazy shadowing for -b boxes: at lookup/open time, if the
    /// box-relative path matches one of the compiled glob patterns,
    /// the FUSE layer serves `self_exe` (the engine binary) instead
    /// of the host file. NO pre-enumeration of the host filesystem —
    /// matching is per-lookup, the way the user asked for it.
    shadows: RwLock<Shadows>,
    // ── live echo mux (the captured-output readback channel) ──
    // Per-box framed writer over the box's ONE muxed connection: the sink-write
    // handler frames captured bytes as ECHO and sends them back to --inner.
    echo: RwLock<HashMap<i64, std::sync::Arc<Mutex<std::os::unix::net::UnixStream>>>>,
    // Per-box open-sink count: when it returns to 0 (both sinks released at child
    // exit) the engine sends ECHO_DONE so --inner stops without truncation.
    sink_open: Mutex<HashMap<i64, u32>>,
    // Globally muted HOST tgids → the box id that muted tgid OWNS. A muted
    // tgid's write to an ANCESTOR box's sink (sink box_id != its own box) is
    // echo readback travelling up — echoed onward but NOT recorded (it was
    // already captured once at its origin box). But a muted tgid's write to ITS
    // OWN box's sink is FIRST-PARTY output (e.g. a brush in-process builtin like
    // echo/printf, which writes fd 1 from the muted --inner pid) and MUST be
    // recorded. A MUTE frame adds --inner's own host tgid mapped to its box id;
    // UNMUTE / connection-close removes it.
    muted: RwLock<std::collections::HashMap<i32, i64>>,
    // D5 (rule-gated): true iff the kernel negotiated FUSE_PASSTHROUGH at init.
    // ONLY read-only opens of `readonly`-RULED paths register backing fds; never
    // a blind per-open guess (see DESIGN.md D5). daemon_reads counts read() ops
    // the daemon served (test observability: stays ~0 for passthrough'd reads).
    passthrough_ok: std::sync::atomic::AtomicBool,
    daemon_reads: AtomicU64,
    /// Engine-to-UI event queue, drained by the broadcaster thread in
    /// main.rs::serve. Two producers, ONE queue:
    ///   * overlay's mutating FS ops (write / create / mkdir / ...)
    ///     push (sid, rel, op) → broadcast as type=overlay.
    ///   * each registered BoxState (record_proc) pushes
    ///     (sid, "", "process_added") → broadcast as type=process_added.
    /// add_box() hands every BoxState a clone of this Arc so a direct
    /// push from the producer beats any race with box teardown.
    /// Bounded by OVERLAY_EVT_CAP (oldest half-shed on overflow).
    events: crate::capture::EventQ,
}

/// Soft cap on the per-overlay event queue. The control loop drains every
/// few hundred ms; anything still queued past this bound is the oldest
/// half-shed, which is fine for "what just changed" notifications (the UI
/// re-reads the underlying view on the broadcast).
pub const OVERLAY_EVT_CAP: usize = 4096;

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
    sink: Option<i32>, // Some(stream) → writes go to the outputs table, not a blob
    passthrough: bool, // writes go straight to the real host file (uncaptured)
    // Kernel passthrough backing registration; kept alive as long as the fd
    // (its Drop closes the registration). Only set for readonly-ruled reads.
    _backing: Option<fuser::BackingId>,
}

#[derive(Clone)]
struct DirNode {
    inode: u64,
    kind: FileType,
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
    Jobserver,
    Sink { box_id: i64, stream: i32 },
    File { file: File, box_id: i64, rel: String },
}

struct NodeSetattr {
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    mtime: Option<SystemTime>,
}

/// Bridges a deferred FUSE read reply into the slip pool. The pool calls exactly
/// one of grant/deny_again, fulfilling the read that was blocked acquiring a slip
/// (grant → one byte; deny_again → EAGAIN for an O_NONBLOCK caller, or when the
/// waiting pid was reaped). `ReplyData` is `Send`, so the pool can hold it across
/// threads and reply later from a release/reap.
struct SlipReplyData(Option<fuser::ReplyData>);
impl crate::slippool::SlipReply for SlipReplyData {
    fn grant(mut self: Box<Self>) {
        if let Some(r) = self.0.take() {
            r.data(&[crate::slippool::SLIP]);
        }
    }
    fn deny_again(mut self: Box<Self>) {
        if let Some(r) = self.0.take() {
            r.error(Errno::EAGAIN);
        }
    }
}

// Reserved box-root paths: the box's stdout/stderr write THROUGH these, and the
// overlay routes the bytes to the outputs table (per-write pid attribution).
// They resolve by exact lookup but are never listed in readdir.
const SINK_STDOUT: &str = ".slopbox-stdout";
const SINK_STDERR: &str = ".slopbox-stderr";
// Hidden synthetic dir at each box root listing the box's live children, each
// routing to that child's real overlay-root inode (the nested-launch bind
// target). Reachable by explicit lookup; never listed in the box-root readdir.
const KIDS_DIR: &str = ".slopbox-kids";

// Synthetic jobserver token file at each box root. read() acquires one slip from
// the engine-global pool (blocking until one frees, unless the fd is O_NONBLOCK),
// write() releases one. Reached by explicit lookup; never listed in readdir.
// Because every op is a FUSE request, the engine sees the caller pid and can keep
// a per-pid ledger + reap leaked slips on exit (slippool.rs).
const JOBSERVER: &str = ".slopbox-jobserver";

fn sink_stream(rel: &str) -> Option<i32> {
    match rel {
        SINK_STDOUT => Some(0),
        SINK_STDERR => Some(1),
        _ => None,
    }
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

/// The raw shadow glob pattern strings (sh, make, ninja) from the SAME
/// config files + defaults the FUSE shadows compile. Shared with the sud
/// runner, which expands them into concrete wrapper remap rules — the two
/// backends must honor the same shadow configuration (a make matched by
/// shadow_make.glob was shadowed under FUSE but ran REAL under sud,
/// recording processes but no pipelines and no build edges).
pub fn shadow_glob_strings() -> (Vec<String>, Vec<String>, Vec<String>) {
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

enum Layer {
    Absent,
    UpperFile { owner: i64, rowid: i64, mode: u32 },
    UpperDir { mode: u32, mtime_ns: i64 },
    UpperSymlink { target: PathBuf },
    UpperSpecial { mode: u32, rdev: u64 },
    /// A regular file served by an external RO attachment. size/mode are
    /// carried from the readout's entry so getattr NEVER decodes blob
    /// bytes; only open()/box_read_file call att.blob(rel).
    ExtFile { att: Arc<crate::attach::ExtAttachment>, rel: String,
              size: u64, mode: u32 },
    Lower,
}

/// One hop of a box's lookup chain: a box's own overlay, or an external
/// RO attachment served through a mirror-store readout. Scoped to the
/// overlay — chain_of is the single funnel; BoxState is NOT trait-
/// objected (its ~40 box_of consumers stay concrete).
enum ChainLink {
    Box(Arc<BoxState>),
    Ext(Arc<crate::attach::ExtAttachment>),
}

/// Top-level directory names the overlay always presents as an empty, virtual
/// landing pad — resolve-time only, never written to any box's upper, never
/// captured. They give the runner's bwrap a mount target on images that ship
/// no such dir (busybox, distroless, scratch); bwrap then mounts the real fs
/// (procfs, devtmpfs, the host /sys bind, a /tmp tmpfs) straight over it,
/// exactly as it would over a dir the image did ship. This is precisely the set
/// the runner mounts (see runner.rs bwrap setup): proc, dev, sys, tmp. A
/// landing pad only ever surfaces when nothing real provides the path — a host
/// box's host dirs and an image's own /proc etc. are untouched — and for --api
/// boxes /tmp is intercepted earlier in resolve(), so it never reaches here.
fn is_synthetic_landing(rel: &str) -> bool {
    matches!(rel, "proc" | "dev" | "sys" | "tmp")
}

impl SarunFs {
    /// Drain queued (box_id, rel, op) events out of the overlay — the
    /// control loop calls this on a tick and broadcasts each one as a
    /// type=overlay event to subscribers. Returns at most OVERLAY_EVT_CAP
    /// items; anything beyond is dropped before the call returns to keep
    /// the queue from growing unbounded under a write storm.
    pub fn drain_events(&self) -> Vec<(i64, String, &'static str)> {
        let mut q = self.inner.events.lock().unwrap();
        let drained: Vec<_> = q.drain(..).collect();
        drained
    }

    /// Append one change notification; called from the mutating FS ops.
    /// Bounded — drops the oldest half if we'd exceed OVERLAY_EVT_CAP.
    fn push_event(&self, box_id: i64, rel: String, op: &'static str) {
        let mut q = self.inner.events.lock().unwrap();
        if q.len() >= OVERLAY_EVT_CAP {
            let drop_n = OVERLAY_EVT_CAP / 2;
            q.drain(..drop_n);
        }
        q.push_back((box_id, rel, op));
    }

    pub fn new(lower: PathBuf) -> Self {
        let ov = SarunFs { inner: Arc::new(Inner {
            lower,
            boxes: RwLock::new(BTreeMap::new()),
            inodes: crate::sarunfs::InodeTable::new((0, String::new())),
            detached_attrs: RwLock::new(HashMap::new()),
            fhs: RwLock::new(HashMap::new()),
            dir_fhs: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            ext: RwLock::new(HashMap::new()),
            cache: std::sync::OnceLock::new(),
            jobserver_fhs: RwLock::new(HashMap::new()),
            rules: RwLock::new(crate::rules::Rules::load()),
            echo: RwLock::new(HashMap::new()),
            sink_open: Mutex::new(HashMap::new()),
            muted: RwLock::new(std::collections::HashMap::new()),
            passthrough_ok: std::sync::atomic::AtomicBool::new(false),
            daemon_reads: AtomicU64::new(0),
            events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            shadows: RwLock::new(Shadows::load()),
        }), root: (0, String::new()) };
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
    pub fn export_box(&self, box_id: i64) -> Result<Self, Errno> {
        self.box_of(box_id).ok_or(Errno::ENOENT)?;
        Ok(Self {
            inner: self.inner.clone(),
            root: (box_id, String::new()),
        })
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

    /// The host path of the engine binary — what we serve as the
    /// shadow target. Cloned out under the read lock; the path
    /// rarely changes during a run (only on Shadows::reload).
    fn shadow_target_path(&self) -> Option<PathBuf> {
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
        self.inner.echo.write().unwrap().insert(id, conn);
    }
    /// Drop the box's echo writer (box channel closing / teardown).
    pub fn clear_echo(&self, id: i64) {
        self.inner.echo.write().unwrap().remove(&id);
        self.inner.sink_open.lock().unwrap().remove(&id);
    }
    /// The box-channel writer stored under id (set by control::handle as the
    /// echo conn). Reused by the oaita API mux to frame FRAME_API_DATA
    /// responses back over the same channel — no second control conn.
    pub fn echo_writer(&self, id: i64)
        -> Option<std::sync::Arc<Mutex<std::os::unix::net::UnixStream>>>
    {
        self.inner.echo.read().unwrap().get(&id).cloned()
    }
    pub fn mute_add(&self, host_pid: i32, box_id: i64) {
        if host_pid > 0 { self.inner.muted.write().unwrap().insert(host_pid, box_id); }
    }
    pub fn mute_remove(&self, host_pid: i32) {
        self.inner.muted.write().unwrap().remove(&host_pid);
    }
    /// If `pid`'s tgid is muted, returns the box id that muted tgid OWNS (so the
    /// sink-write path can tell a first-party write to its own box's sink from an
    /// echo readback bubbling up through an ancestor sink). None if not muted.
    fn muted_owner(&self, pid: u32) -> Option<i64> {
        let m = self.inner.muted.read().unwrap();
        if m.is_empty() { return None; }
        m.get(&(tgid_of(pid) as i32)).copied()
    }
    /// Frame `data` as an ECHO for `id`'s stream and send it over the box
    /// channel (best-effort; a dropped/blocked channel never fails a write).
    fn echo_send(&self, id: i64, stream: i32, data: &[u8]) {
        let conn = self.inner.echo.read().unwrap().get(&id).cloned();
        if let Some(conn) = conn {
            let frame = crate::frames::encode(crate::frames::FRAME_ECHO,
                &crate::frames::echo_payload(stream as u8, data));
            use std::io::Write;
            let mut c = conn.lock().unwrap();
            let _ = c.write_all(&frame);
        }
    }
    /// Note a sink fd opened for `id` (capture start: out + err = 2).
    fn note_sink_open(&self, id: i64) {
        *self.inner.sink_open.lock().unwrap().entry(id).or_insert(0) += 1;
    }
    /// Note a sink fd released for `id`; when the count returns to 0 (child
    /// exited, both sinks closed) send ECHO_DONE so --inner stops cleanly.
    fn note_sink_release(&self, id: i64) {
        let zero = {
            let mut m = self.inner.sink_open.lock().unwrap();
            if let Some(c) = m.get_mut(&id) {
                *c = c.saturating_sub(1);
                *c == 0
            } else { false }
        };
        if zero {
            let conn = self.inner.echo.read().unwrap().get(&id).cloned();
            if let Some(conn) = conn {
                let frame = crate::frames::encode(crate::frames::FRAME_ECHO_DONE, &[]);
                use std::io::Write;
                let _ = conn.lock().unwrap().write_all(&frame);
            }
        }
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
        b.set_event_sink(self.inner.events.clone());
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
        match self.resolve(bid, rel) {
            Layer::UpperFile { owner, rowid, .. } => {
                // Restore an inline (discard-reverted) row to its blob so this
                // host-side read sees the reverted bytes, exactly as the FUSE
                // read path does. See ensure_upper_blob.
                self.ensure_upper_blob(owner, rowid, rel);
                std::fs::read(crate::depot::blob_path(owner, rowid))
            }
            Layer::Lower => std::fs::read(self.host(rel)),
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
        match self.resolve(bid, rel) {
            Layer::UpperFile { mode, .. } | Layer::ExtFile { mode, .. } =>
                Some(mode & 0o7777),
            Layer::Lower => std::fs::symlink_metadata(self.host(rel)).ok()
                .map(|m| m.permissions().mode() & 0o7777),
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
        let writer = b.writer_for(0);
        let mtime_ns = SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64).unwrap_or(0);
        b.finalize_file(rel, sz, mtime_ns, writer);
        Ok(())
    }

    /// Install engine-owned bytes with an explicit mode through the same upper
    /// representation.  QEMU uses this for `/init`; it is still an ordinary
    /// captured file as far as every transport and later review are concerned.
    pub fn box_install_file(&self, bid: i64, rel: &str, bytes: &[u8], mode: u32)
        -> std::io::Result<()>
    {
        self.box_write_file(bid, rel, bytes)?;
        let r#box = self.box_of(bid).ok_or_else(|| std::io::Error::new(
            std::io::ErrorKind::NotFound, format!("box {bid} not registered")))?;
        r#box.set_mode(rel, libc::S_IFREG | (mode & 0o7777));
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
                FileType::RegularFile => 'f',
                FileType::Directory => 'd',
                FileType::Symlink => 'l',
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
            Layer::Lower => match self.host(rel).symlink_metadata() {
                Ok(m) if m.file_type().is_symlink() => 'l',
                Ok(m) if m.is_dir() => 'd',
                Ok(m) if m.is_file() => 'f',
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

    /// Live child box ids of `bid` (their parent() == bid) — KIDS_DIR entries.
    fn children_of_box(&self, bid: i64) -> Vec<i64> {
        self.inner.boxes.read().unwrap().values()
            .filter(|c| c.parent() == Some(bid)).map(|c| c.id).collect()
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

    fn key_of(&self, ino: INodeNo) -> Option<Key> {
        if u64::from(ino) == 1 {
            Some(self.root.clone())
        } else {
            self.inner.inodes.key(u64::from(ino))
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
        if rel.is_empty() { self.inner.lower.clone() } else { self.inner.lower.join(rel) }
    }

    /// A box's OWN layer for `rel` (single level, no parent walk) — used by the
    /// WRITE paths, which operate on the box's own overlay. UpperFile.owner is
    /// the box itself.
    fn layer(&self, b: &BoxState, rel: &str) -> Layer {
        match b.entry(rel) {
            Some(Entry::Whiteout) => Layer::Absent,
            Some(Entry::File { rowid, mode }) =>
                Layer::UpperFile { owner: b.id, rowid, mode },
            Some(Entry::Dir { mode, mtime_ns, .. }) => Layer::UpperDir { mode, mtime_ns },
            Some(Entry::Symlink { target }) => Layer::UpperSymlink { target },
            Some(Entry::Special { mode, rdev }) => Layer::UpperSpecial { mode, rdev },
            // A hole in the box's own upper: the backdrop (host) shows.
            Some(Entry::Hole) | None => {
                if self.host(rel).symlink_metadata().is_ok() {
                    Layer::Lower
                } else {
                    Layer::Absent
                }
            }
        }
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
        // --api substitute: /tmp is presented as a symlink to a per-box dir
        // under oaita's state home. Lifts the model's strongest write-target
        // prior into ordinary overlay-captured space so apply/inspect/discard
        // work — without this /tmp is a bwrap-private tmpfs and writes there
        // are a black hole. Decided per ORIGINATING box (no chain walk),
        // since `bid` is the box whose view is being computed; a parent box
        // having or not having --api is irrelevant to this box's /tmp.
        if rel == "tmp" {
            if let Some(orig) = self.box_of(bid) {
                if orig.is_api() {
                    let target = crate::paths::oaita_state_home()
                        .join(".tmp").join(bid.to_string());
                    return Layer::UpperSymlink { target };
                }
            }
        }
        // D-parent: any box in the lookup chain having `no_host_fallback` set
        // closes the bottom of the stack — when the parent walk runs out, the
        // path is Absent rather than served from the real host /. Set on the
        // bottom of an OCI image stack so `ls /etc` inside the box sees only
        // the image's /etc, never the host's. The chain is each box, its RO
        // attachments, then its parent (chain_of).
        let mut no_host = false;
        for link in self.chain_of(bid) {
            let b = match link {
                ChainLink::Box(b) => b,
                // Attachments are resolved VIEWS: no whiteouts, holes or
                // opacity — a miss just falls through to the next link.
                ChainLink::Ext(att) => {
                    match att.entry(rel) {
                        Some(e) if e.dir =>
                            return Layer::UpperDir { mode: e.mode,
                                                     mtime_ns: 0 },
                        Some(e) =>
                            return Layer::ExtFile {
                                att, rel: rel.to_string(),
                                size: e.size, mode: e.mode },
                        None => {}
                    }
                    continue;
                }
            };
            let id = b.id;
            if b.no_host_fallback() { no_host = true; }
            match b.entry(rel) {
                Some(Entry::Whiteout) => return Layer::Absent,
                Some(Entry::File { rowid, mode }) =>
                    return Layer::UpperFile { owner: id, rowid, mode },
                Some(Entry::Dir { mode, mtime_ns, .. }) =>
                    return Layer::UpperDir { mode, mtime_ns },
                Some(Entry::Symlink { target }) =>
                    return Layer::UpperSymlink { target },
                Some(Entry::Special { mode, rdev }) =>
                    return Layer::UpperSpecial { mode, rdev },
                // A hole: "this key is not occluded" — every recorded
                // layer below is skipped; the backdrop (host, or nothing
                // under no_host) shows through LIVE.
                Some(Entry::Hole) => break,
                None => {
                    // D-opaque (OCI): an upper box can mark a directory as
                    // opaque (`.wh..wh..opq` convention) — when we don't have
                    // our own entry for `rel`, but an ANCESTOR dir of `rel` is
                    // opaque in this box, the lower-layer chain past this box
                    // can't contribute (the opaque marker wipes everything
                    // below for that dir's subtree). Return Absent immediately.
                    if has_opaque_ancestor(&b, rel) {
                        return Layer::Absent;
                    }
                    // A REBASED ancestor erases everything recorded
                    // below this box for the subtree; the backdrop shows.
                    if has_rebased_ancestor(&b, rel) {
                        break;
                    }
                    // not in this box → next link in the chain
                }
            }
        }
        // Synthetic landing pad (see is_synthetic_landing): reaching here means
        // NO box in the chain has any entry for `rel` — every real Dir / File /
        // Symlink / Special / Whiteout already returned above, so a directory an
        // image actually ships is never touched. As a last resort, for the
        // mount-target names (proc/dev/sys/tmp), present an empty virtual dir so
        // bwrap has something to mount over on minimal images. NON-DESTRUCTIVE:
        // chain_dir_has_children backs the pad off the instant any box holds a
        // child under `rel`, so it can never stand in for — let alone hide — a
        // directory that has content. Resolve-only: never a sqlar row, never in
        // apply/discard/the change summary.
        if no_host {
            if is_synthetic_landing(rel) && !self.chain_dir_has_children(bid, rel) {
                return Layer::UpperDir { mode: 0o0555, mtime_ns: 0 };
            }
            return Layer::Absent;
        }
        if self.host(rel).symlink_metadata().is_ok() {
            Layer::Lower
        } else if is_synthetic_landing(rel) && !self.chain_dir_has_children(bid, rel) {
            Layer::UpperDir { mode: 0o0555, mtime_ns: 0 }
        } else {
            Layer::Absent
        }
    }

    /// True if any box in the chain rooted at `bid` carries a direct child under
    /// `rel` — i.e. that directory actually holds something in the merged view.
    /// The synthetic landing pad consults this to stay non-destructive: a pad is
    /// only ever offered for a path that is empty in EVERY box of the chain, so
    /// it can never hide a real directory's contents.
    fn chain_dir_has_children(&self, bid: i64, rel: &str) -> bool {
        for link in self.chain_of(bid) {
            match link {
                ChainLink::Box(b) => {
                    let (_white, present, _holes) = b.children_of(rel);
                    if !present.is_empty() { return true; }
                }
                ChainLink::Ext(att) => {
                    if !att.children(rel).is_empty() { return true; }
                }
            }
        }
        false
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

    fn synth_file_attr(&self, ino: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino), size: 0, blocks: 0,
            atime: UNIX_EPOCH, mtime: UNIX_EPOCH, ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH, kind: FileType::RegularFile,
            perm: 0o666, nlink: 1, uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
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

    /// Attributes for (box, rel) through the FULL merge (own → parent chain →
    /// host), or None when absent.
    fn attr_of(&self, b: &BoxState, ino: u64, rel: &str) -> Option<FileAttr> {
        let layer = self.resolve(b.id, rel);
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
                a.kind = FileType::RegularFile;
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
                a.kind = FileType::RegularFile;
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
                a.kind = FileType::RegularFile;
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
            if let Some(exe) = self.shadow_target_path() {
                if let Ok(md) = std::fs::metadata(&exe) {
                    let mut a = self.attr_from_md(ino, &md);
                    a.kind = FileType::RegularFile;
                    // Keep exec bits — most shadow targets are
                    // /bin/sh-shaped things the box wants to exec.
                    return Some(a);
                }
            }
        }
        match layer {
            Layer::Absent => None,
            Layer::Lower => self.lower_attr(ino, rel),
            Layer::UpperFile { owner, rowid, mode } => {
                let bp = blob_path(owner, rowid);
                let md = bp.metadata().ok()?;
                let mut a = self.attr_from_md(ino, &md);
                a.perm = (mode & 0o7777) as u16;
                a.kind = FileType::RegularFile;
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
        }
    }

    /// Transport-neutral lookup.  Both the host `/dev/fuse` adapter and the
    /// virtio-fs server enter policy here, including synthetic nodes and inode
    /// lookup lifetime accounting.
    fn lookup_node(&self, parent: u64, name: &OsStr) -> Result<FileAttr, Errno> {
        let (bid, prel) = self.key_of(INodeNo(parent)).ok_or(Errno::ENOENT)?;
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
            if prel.is_empty() && name == KIDS_DIR {
                let ino = self.ino_for(&(bid, KIDS_DIR.to_string()));
                self.synth_dir_attr(ino, 0o40755, 0)
            } else if prel == KIDS_DIR {
                let cid = name.parse::<i64>().map_err(|_| Errno::ENOENT)?;
                if self.box_of(cid).and_then(|child| child.parent()) != Some(bid) {
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
                if prel.is_empty() && (sink_stream(&rel).is_some() || rel == JOBSERVER) {
                    self.synth_file_attr(ino)
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
        self.inner.inodes.acquire(u64::from(attr.ino), 1);
        Ok(attr)
    }

    fn getattr_node(&self, inode: u64) -> Result<FileAttr, Errno> {
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
        if bid == 0 || rel.is_empty() || rel == KIDS_DIR {
            return Ok(self.synth_dir_attr(inode, 0o40755, 0));
        }
        if sink_stream(&rel).is_some() || rel == JOBSERVER {
            return Ok(self.synth_file_attr(inode));
        }
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        self.attr_of(&b, inode, &rel)
            .or_else(|| self.inner.detached_attrs.read().unwrap().get(&inode).copied())
            .ok_or(Errno::ENOENT)
    }

    fn readlink_node(&self, inode: u64) -> Result<Vec<u8>, Errno> {
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
        self.box_of(bid).ok_or(Errno::ENOENT)?;
        match self.resolve(bid, &rel) {
            Layer::UpperSymlink { target } =>
                Ok(target.as_os_str().as_encoded_bytes().to_vec()),
            Layer::Lower => std::fs::read_link(self.host(&rel))
                .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
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
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
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
        b.set_xattr(&rel, name, value);
        Ok(())
    }

    fn get_xattr_node(&self, inode: u64, name: &OsStr) -> Result<Vec<u8>, Errno> {
        self.getattr_node(inode)?;
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        b.get_xattr(&rel, name).ok_or(Errno::ENODATA)
    }

    fn list_xattr_node(&self, inode: u64) -> Result<Vec<u8>, Errno> {
        self.getattr_node(inode)?;
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
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
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        if self.ro_denied(bid, &rel) {
            return Err(Errno::EROFS);
        }
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        b.remove_xattr(&rel, name).then_some(()).ok_or(Errno::ENODATA)
    }

    fn open_node(
        &self,
        pid: u32,
        inode: u64,
        flags: u32,
        allow_backing: bool,
    ) -> Result<OpenedNode, Errno> {
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
        let b = self.box_of(bid).ok_or(Errno::ENOENT)?;
        let want_write = flags & libc::O_ACCMODE as u32 != libc::O_RDONLY as u32;
        if want_write && self.ro_denied(bid, &rel) {
            return Err(Errno::EROFS);
        }
        if rel == JOBSERVER {
            let nonblock = flags & libc::O_NONBLOCK as u32 != 0;
            let handle = self.inner.next_fh.fetch_add(1, Ordering::Relaxed);
            self.inner.jobserver_fhs.write().unwrap().insert(handle, nonblock);
            return Ok(OpenedNode {
                handle,
                direct_io: true,
                nonseekable: true,
                keep_cache: false,
                backing_candidate: false,
            });
        }
        if let Some(stream) = sink_stream(&rel) {
            self.note_sink_open(bid);
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                file: None,
                upper: false,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                sink: Some(stream),
                passthrough: false,
                _backing: None,
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
                .open(&host)
                .map_err(Errno::from)?;
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                file: Some(file),
                upper: true,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                sink: None,
                passthrough: true,
                _backing: None,
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
                file: Some(file),
                upper: false,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                sink: None,
                passthrough: false,
                _backing: None,
            });
            return Ok(OpenedNode {
                handle,
                direct_io: false,
                nonseekable: false,
                keep_cache: false,
                backing_candidate: false,
            });
        }
        let (file, upper) = match self.resolve(bid, &rel) {
            Layer::UpperFile { owner, rowid, .. } => {
                self.ensure_upper_blob(owner, rowid, &rel);
                let own = owner == bid;
                let file = OpenOptions::new()
                    .read(true)
                    .write(want_write && own)
                    .open(blob_path(owner, rowid))
                    .map_err(|_| Errno::EIO)?;
                (file, own)
            }
            Layer::Lower => {
                let host = if b.is_api() && Self::matches_host_oaita_config(&rel) {
                    crate::paths::api_box_oaita_toml_path()
                } else if b.is_brush() && self.shadow_matches(&rel) {
                    self.shadow_target_path().unwrap_or_else(|| self.host(&rel))
                } else {
                    self.host(&rel)
                };
                (File::open(host).map_err(|_| Errno::EACCES)?, false)
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
                (file, false)
            }
            _ => return Err(Errno::ENOENT),
        };
        const FMODE_EXEC: u32 = 0x20;
        let backing_candidate = allow_backing
            && !want_write
            && flags & FMODE_EXEC == 0
            && (b.direct() || self.is_passthrough_read(&rel))
            && self.inner.passthrough_ok.load(Ordering::Relaxed);
        let handle = self.reg_fh(FhInner {
            box_id: bid,
            rel,
            file: Some(file),
            upper,
            dirty: false,
            last_pid: pid,
            last_tgid: 0,
            sink: None,
            passthrough: false,
            _backing: None,
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
        parent: u64,
        name: &OsStr,
        mode: u32,
    ) -> Result<(FileAttr, OpenedNode), Errno> {
        let (bid, parent_rel) = self.key_of(INodeNo(parent)).ok_or(Errno::ENOENT)?;
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
            attr.kind = FileType::RegularFile;
            attr.perm = (mode & 0o7777) as u16;
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                file: Some(file),
                upper: true,
                dirty: false,
                last_pid: pid,
                last_tgid: 0,
                sink: None,
                passthrough: true,
                _backing: None,
            });
            (attr, handle)
        } else {
            let writer = b.writer_for(pid);
            let rowid = b.ensure_file_row(&rel, mode | libc::S_IFREG, writer);
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
            attr.kind = FileType::RegularFile;
            attr.perm = (mode & 0o7777) as u16;
            self.push_event(bid, rel.clone(), "create");
            let handle = self.reg_fh(FhInner {
                box_id: bid,
                rel,
                file: Some(file),
                upper: true,
                dirty: true,
                last_pid: pid,
                last_tgid: 0,
                sink: None,
                passthrough: false,
                _backing: None,
            });
            (attr, handle)
        };
        self.inner.inodes.acquire(u64::from(attr.ino), 1);
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
        let (box_id, parent_rel) = self.key_of(INodeNo(parent)).ok_or(Errno::ENOENT)?;
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

    fn mkdir_node(&self, pid: u32, parent: u64, name: &OsStr, mode: u32)
        -> Result<FileAttr, Errno>
    {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        if !matches!(self.resolve(box_id, &rel), Layer::Absent) {
            return Err(Errno::EEXIST);
        }
        b.set_dir(&rel, mode, b.writer_for(pid));
        let inode = self.ino_for(&(box_id, rel.clone()));
        self.push_event(box_id, rel, "mkdir");
        let attr = self.synth_dir_attr(inode, mode | libc::S_IFDIR, 0);
        self.inner.inodes.acquire(inode, 1);
        Ok(attr)
    }

    fn symlink_node(
        &self,
        pid: u32,
        parent: u64,
        name: &OsStr,
        target: &Path,
    ) -> Result<FileAttr, Errno> {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        b.set_symlink(&rel, target, b.writer_for(pid));
        let inode = self.ino_for(&(box_id, rel.clone()));
        self.push_event(box_id, rel, "symlink");
        let attr = self.synth_link_attr(
            inode,
            target.as_os_str().as_encoded_bytes().len() as u64,
        );
        self.inner.inodes.acquire(inode, 1);
        Ok(attr)
    }

    fn unlink_node(&self, pid: u32, parent: u64, name: &OsStr) -> Result<(), Errno> {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        let inode = self.ino_for(&(box_id, rel.clone()));
        let attr = self.attr_of(&b, inode, &rel).ok_or(Errno::ENOENT)?;
        if attr.kind == FileType::Directory {
            return Err(Errno::EISDIR);
        }
        self.inner.inodes.detach(&(box_id, rel.clone()));
        self.inner.detached_attrs.write().unwrap().insert(inode, attr);
        b.drop_row(&rel);
        b.set_whiteout(&rel, b.writer_for(pid));
        self.push_event(box_id, rel, "unlink");
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
        if attr.kind != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        if !self.scan_dir(&b, &rel, false).is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        self.inner.inodes.detach(&(box_id, rel.clone()));
        self.inner.detached_attrs.write().unwrap().insert(inode, attr);
        b.drop_row(&rel);
        b.set_whiteout(&rel, b.writer_for(pid));
        self.push_event(box_id, rel, "rmdir");
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
        let (box_id, old_parent) = self.key_of(INodeNo(parent)).ok_or(Errno::EACCES)?;
        let (new_box_id, new_parent) = self
            .key_of(INodeNo(new_parent))
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
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &old_rel) || self.ro_denied(box_id, &new_rel) {
            return Err(Errno::EROFS);
        }
        if flags & libc::RENAME_NOREPLACE != 0
            && !matches!(self.resolve(box_id, &new_rel), Layer::Absent)
        {
            return Err(Errno::EEXIST);
        }
        let writer = b.writer_for(pid);
        let lower_old = self.host(&old_rel).symlink_metadata().is_ok();
        match self.layer(&b, &old_rel) {
            Layer::Absent => return Err(Errno::ENOENT),
            Layer::UpperDir { .. } => {
                b.reparent(&old_rel, &new_rel);
                if self.host(&old_rel).is_dir() {
                    b.set_whiteout(&old_rel, writer);
                }
            }
            Layer::Lower => {
                self.copy_up(&b, &old_rel, pid).map_err(|_| Errno::EIO)?;
                b.rename_row(&old_rel, &new_rel);
                b.set_whiteout(&old_rel, writer);
            }
            Layer::UpperFile { .. }
            | Layer::UpperSymlink { .. }
            | Layer::UpperSpecial { .. } => {
                b.rename_row(&old_rel, &new_rel);
                if lower_old {
                    b.set_whiteout(&old_rel, writer);
                }
            }
            Layer::ExtFile { .. } => return Err(Errno::EROFS),
        }
        self.remap_inode_subtree(box_id, &old_rel, &new_rel);
        self.push_event(box_id, old_rel, "rename_src");
        self.push_event(box_id, new_rel, "rename_dst");
        Ok(())
    }

    fn mknod_node(
        &self,
        pid: u32,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
    ) -> Result<FileAttr, Errno> {
        let (box_id, rel) = self.child_path(parent, name)?;
        let b = self.box_of(box_id).ok_or(Errno::ENOENT)?;
        if self.ro_denied(box_id, &rel) {
            return Err(Errno::EROFS);
        }
        match mode & libc::S_IFMT {
            libc::S_IFREG => {
                let rowid = b.ensure_file_row(&rel, mode, b.writer_for(pid));
                let path = blob_path(box_id, rowid);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(Errno::from)?;
                }
                File::create(path).map_err(Errno::from)?;
            }
            libc::S_IFIFO | libc::S_IFCHR | libc::S_IFBLK | libc::S_IFSOCK => {
                b.set_special(&rel, mode, rdev as u64, b.writer_for(pid));
            }
            _ => return Err(Errno::EINVAL),
        }
        self.push_event(box_id, rel.clone(), "mknod");
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
    ) -> Result<FileAttr, Errno> {
        let (source_box, source_rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
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
        let new_rowid = b.ensure_file_row(&new_rel, source_mode, b.writer_for(pid));
        let destination = blob_path(box_id, new_rowid);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(Errno::from)?;
        }
        std::fs::hard_link(blob_path(box_id, source_rowid), &destination)
            .map_err(Errno::from)?;
        let new_inode = self.ino_for(&(box_id, new_rel.clone()));
        let attr = self.attr_of(&b, new_inode, &new_rel).ok_or(Errno::EIO)?;
        self.inner.inodes.acquire(new_inode, 1);
        self.push_event(box_id, new_rel, "link");
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
    ) -> Result<FileAttr, Errno> {
        let (box_id, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
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
            if let Some(mtime) = request.mtime {
                let duration = mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
                let timestamp = libc::timespec {
                    tv_sec: duration.as_secs() as _,
                    tv_nsec: duration.subsec_nanos() as _,
                };
                let times = [timestamp, timestamp];
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
        }
        if let Some(mode) = request.mode {
            let permissions = mode & 0o7777;
            let writer = b.writer_for(pid);
            match self.layer(&b, &rel) {
                Layer::UpperFile { .. } => b.set_mode(&rel, libc::S_IFREG | permissions),
                Layer::UpperDir { .. } => b.set_mode(&rel, libc::S_IFDIR | permissions),
                Layer::UpperSymlink { .. } => {}
                Layer::Lower if self.host(&rel).is_dir() => b.set_dir(&rel, permissions, writer),
                Layer::Lower => {
                    self.copy_up(&b, &rel, pid).map_err(|_| Errno::EIO)?;
                    b.set_mode(&rel, libc::S_IFREG | permissions);
                }
                Layer::Absent => return Err(Errno::ENOENT),
                Layer::UpperSpecial { mode, .. } => b.set_mode(&rel, (mode & libc::S_IFMT) | permissions),
                Layer::ExtFile { .. } => return Err(Errno::EROFS),
            }
        }
        if request.uid.is_some() || request.gid.is_some() {
            let current = b.owner_of(&rel).unwrap_or((0, 0));
            let uid = request.uid.unwrap_or(current.0);
            let gid = request.gid.unwrap_or(current.1);
            if matches!(self.layer(&b, &rel), Layer::Lower) && !self.host(&rel).is_dir() {
                self.copy_up(&b, &rel, pid).map_err(|_| Errno::EIO)?;
            }
            if let Layer::UpperFile { rowid, .. } = self.layer(&b, &rel) {
                self.ensure_upper_blob(box_id, rowid, &rel);
                let path = CString::new(blob_path(box_id, rowid).as_os_str().as_encoded_bytes())
                    .map_err(|_| Errno::EINVAL)?;
                if unsafe { libc::lchown(path.as_ptr(), uid, gid) } != 0 {
                    return Err(Errno::from(std::io::Error::last_os_error()));
                }
            } else if matches!(self.layer(&b, &rel), Layer::Absent) {
                return Err(Errno::ENOENT);
            }
            b.set_owner(&rel, uid, gid);
        }
        if let Some(mtime) = request.mtime {
            if matches!(self.layer(&b, &rel), Layer::Lower) && !self.host(&rel).is_dir() {
                self.copy_up(&b, &rel, pid).map_err(|_| Errno::EIO)?;
            }
            let nanos = mtime
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos() as i64)
                .unwrap_or(0);
            b.set_mtime(&rel, nanos);
            if let Layer::UpperFile { rowid, .. } = self.layer(&b, &rel) {
                self.ensure_upper_blob(box_id, rowid, &rel);
                OpenOptions::new()
                    .write(true)
                    .open(blob_path(box_id, rowid))
                    .map_err(Errno::from)?
                    .set_modified(mtime)
                    .map_err(Errno::from)?;
            }
        }
        self.attr_of(&b, inode, &rel).ok_or(Errno::ENOENT)
    }

    fn read_file_node(&self, handle: u64) -> Result<File, Errno> {
        self.inner.daemon_reads.fetch_add(1, Ordering::Relaxed);
        let handles = self.inner.fhs.read().unwrap();
        let handle = handles.get(&handle).ok_or(Errno::EBADF)?;
        let handle = handle.lock().unwrap();
        handle
            .inner
            .file
            .as_ref()
            .ok_or(Errno::EBADF)?
            .try_clone()
            .map_err(Errno::from)
    }

    fn prepare_write_node(&self, pid: u32, handle: u64) -> Result<WriteTarget, Errno> {
        if self.inner.jobserver_fhs.read().unwrap().contains_key(&handle) {
            return Ok(WriteTarget::Jobserver);
        }
        let handles = self.inner.fhs.read().unwrap();
        let handle = handles.get(&handle).ok_or(Errno::EBADF)?;
        let mut handle = handle.lock().unwrap();
        if let Some(stream) = handle.inner.sink {
            return Ok(WriteTarget::Sink {
                box_id: handle.inner.box_id,
                stream,
            });
        }
        if !handle.inner.upper {
            let b = self.box_of(handle.inner.box_id).ok_or(Errno::EIO)?;
            handle.inner.file = Some(
                self.copy_up(&b, &handle.inner.rel.clone(), pid)
                    .map_err(|_| Errno::EIO)?,
            );
            handle.inner.upper = true;
        }
        handle.inner.dirty = true;
        if pid != handle.inner.last_pid || handle.inner.last_tgid == 0 {
            handle.inner.last_tgid = tgid_of(pid);
            if let Some(b) = self.box_of(handle.inner.box_id) {
                b.writer_for(handle.inner.last_tgid);
            }
        }
        handle.inner.last_pid = pid;
        let file = handle
            .inner
            .file
            .as_ref()
            .ok_or(Errno::EBADF)?
            .try_clone()
            .map_err(Errno::from)?;
        Ok(WriteTarget::File {
            file,
            box_id: handle.inner.box_id,
            rel: handle.inner.rel.clone(),
        })
    }

    fn finish_file_write(&self, box_id: i64, rel: String) {
        self.push_event(box_id, rel, "write");
    }

    fn write_sink_node(&self, pid: u32, box_id: i64, stream: i32, data: &[u8]) {
        let record = match self.muted_owner(pid) {
            None => true,
            Some(owner) => owner == box_id,
        };
        if record {
            if let Some(b) = self.box_of(box_id) {
                b.add_output(stream, pid, data);
            }
        }
        self.echo_send(box_id, stream, data);
    }

    fn release_jobserver_slip(&self, pid: u32) {
        let pid = tgid_of(pid) as i32;
        let _ = crate::slippool::global().lock().unwrap().release(pid);
    }

    fn release_node(&self, handle: u64) -> Result<(), Errno> {
        if self.inner.jobserver_fhs.write().unwrap().remove(&handle).is_some() {
            return Ok(());
        }
        let handle = self
            .inner
            .fhs
            .write()
            .unwrap()
            .remove(&handle)
            .ok_or(Errno::EBADF)?;
        let handle = handle.into_inner().unwrap();
        if handle.inner.sink.is_some() {
            self.note_sink_release(handle.inner.box_id);
        } else if handle.inner.dirty && !handle.inner.passthrough {
            if let Some(b) = self.box_of(handle.inner.box_id) {
                let writer_id = if handle.inner.last_tgid != 0 {
                    handle.inner.last_tgid
                } else {
                    handle.inner.last_pid
                };
                let writer = b.writer_for(writer_id);
                if let Some(metadata) = handle.inner.file.as_ref().and_then(|file| file.metadata().ok()) {
                    b.finalize_file(
                        &handle.inner.rel,
                        metadata.size() as i64,
                        metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
                        writer,
                    );
                }
            }
        }
        Ok(())
    }

    fn sync_file_node(&self, handle: u64, data_only: bool) -> Result<(), Errno> {
        if self.inner.jobserver_fhs.read().unwrap().contains_key(&handle) {
            return Ok(());
        }
        let handles = self.inner.fhs.read().unwrap();
        let handle = handles.get(&handle).ok_or(Errno::EBADF)?;
        let handle = handle.lock().unwrap();
        let Some(file) = handle.inner.file.as_ref() else {
            return Ok(());
        };
        if data_only {
            file.sync_data().map_err(Errno::from)
        } else {
            file.sync_all().map_err(Errno::from)
        }
    }

    fn statfs_node(&self) -> Result<libc::statvfs64, Errno> {
        let path = CString::new(self.inner.lower.as_os_str().as_encoded_bytes())
            .map_err(|_| Errno::EINVAL)?;
        let mut stat: libc::statvfs64 = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs64(path.as_ptr(), &mut stat) } == 0 {
            Ok(stat)
        } else {
            Err(Errno::from(std::io::Error::last_os_error()))
        }
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
        let writer = b.writer_for(pid);
        // Source the lower bytes + mode from the parent-chain resolution.
        let (src, mode): (Option<PathBuf>, u32) = match self.resolve(b.id, rel) {
            Layer::UpperFile { owner, rowid, mode } => {
                // Re-materialize an inline (discard-reverted) row to its blob
                // so the copy below — and the box's own re-run write — has a
                // source. See ensure_upper_blob.
                self.ensure_upper_blob(owner, rowid, rel);
                (Some(blob_path(owner, rowid)), mode)
            }
            Layer::Lower => {
                let m = self.host(rel).symlink_metadata().map(|m| m.mode())
                    .unwrap_or(0o100644);
                (Some(self.host(rel)), m)
            }
            // Unreachable: every mutation path EROFS'd at ro_denied
            // before copy_up could see an attachment-resolved key.
            Layer::ExtFile { .. } =>
                return Err(std::io::Error::from_raw_os_error(libc::EROFS)),
            _ => (None, 0o100644),
        };
        let rowid = b.ensure_file_row(rel, mode, writer);
        let bp = blob_path(b.id, rowid);
        if let Some(parent) = bp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !bp.exists() {
            match src {
                Some(s) => { std::fs::copy(&s, &bp)?; }
                None => { File::create(&bp)?; }
            }
        }
        OpenOptions::new().read(true).write(true).open(&bp)
    }

    fn reg_fh(&self, fh: FhInner) -> u64 {
        let n = self.inner.next_fh.fetch_add(1, Ordering::Relaxed);
        self.inner.fhs.write().unwrap().insert(n, Mutex::new(Fh { inner: fh }));
        n
    }

    /// Merged listing of (box, rel) through the FULL chain: host entries, then
    /// each box from ROOT down to the child applied in order (so a deeper box's
    /// whiteouts hide and its entries override shallower layers). (name, kind,
    /// child-ino, Option<attr>).
    fn scan_dir(&self, b: &BoxState, rel: &str, plus: bool)
                -> Vec<(String, FileType, u64, Option<FileAttr>)> {
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
            if let Ok(rd) = std::fs::read_dir(self.host(rel)) {
                for ent in rd.flatten() {
                    if let Some(n) = ent.file_name().to_str() {
                        names.insert(n.to_string(), ());
                    }
                }
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
                if bx.is_opaque(rel) || has_opaque_ancestor(&bx, rel) {
                    names.clear();
                }
                // REBASED here (this dir, or an ancestor): everything the
                // chain recorded so far is erased for this subtree; the
                // backdrop (host) still shows through — re-seed it.
                if matches!(bx.entry(rel),
                            Some(Entry::Dir { rebased: true, .. }))
                    || has_rebased_ancestor(&bx, rel)
                {
                    names.clear();
                    if !no_host {
                        if let Ok(rd) = std::fs::read_dir(self.host(rel)) {
                            for ent in rd.flatten() {
                                if let Some(n) = ent.file_name().to_str() {
                                    names.insert(n.to_string(), ());
                                }
                            }
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
                        if self.host(&hp).symlink_metadata().is_ok() {
                            names.insert(h.clone(), ());
                        }
                    }
                }
                for p in present { names.insert(p, ()); }
            }
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
        if self.getattr_node(inode)?.kind != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        let (bid, rel) = self.key_of(INodeNo(inode)).ok_or(Errno::ENOENT)?;
        if bid == 0 {
            return Ok(self
                .inner
                .boxes
                .read()
                .unwrap()
                .keys()
                .map(|id| DirNode {
                    inode: self.ino_for(&(*id, String::new())),
                    kind: FileType::Directory,
                    name: id.to_string(),
                })
                .collect());
        }
        if rel == KIDS_DIR {
            return Ok(self
                .children_of_box(bid)
                .into_iter()
                .map(|id| DirNode {
                    inode: self.ino_for(&(id, String::new())),
                    kind: FileType::Directory,
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
        let handle = self.inner.next_fh.fetch_add(1, Ordering::Relaxed);
        self.inner.dir_fhs.write().unwrap().insert(handle, snapshot);
        Ok(handle)
    }

    fn read_directory(&self, handle: u64, offset: u64) -> Result<Vec<DirNode>, Errno> {
        let snapshots = self.inner.dir_fhs.read().unwrap();
        let snapshot = snapshots.get(&handle).ok_or(Errno::EBADF)?;
        let start = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        Ok(snapshot.iter().skip(start).cloned().collect())
    }

    fn close_directory(&self, handle: u64) -> Result<(), Errno> {
        self.inner
            .dir_fhs
            .write()
            .unwrap()
            .remove(&handle)
            .map(|_| ())
            .ok_or(Errno::EBADF)
    }
}

impl Filesystem for SarunFs {
    fn init(&mut self, _req: &Request,
            config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // Negotiate kernel FUSE_PASSTHROUGH (kernel 6.9+). WHICH opens use it is
        // decided per-path by the `readonly` file rule — never automatically.
        // set_max_stack_depth(2) is needed because backing files can live on a
        // stacked fs (the container's overlayfs root). Never fail init over this.
        let ok = config.add_capabilities(fuser::InitFlags::FUSE_PASSTHROUGH).is_ok()
            && config.set_max_stack_depth(2).is_ok();
        self.inner.passthrough_ok.store(ok, Ordering::Relaxed);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.lookup_node(u64::from(parent), name) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inner.inodes.forget(u64::from(ino), nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>,
               reply: ReplyAttr) {
        match self.getattr_node(u64::from(ino)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(error),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.readlink_node(u64::from(ino)) {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(error),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let opened = match self.open_node(req.pid(), u64::from(ino), flags.0 as u32, true) {
            Ok(opened) => opened,
            Err(error) => return reply.error(error),
        };
        let mut response_flags = FopenFlags::empty();
        if opened.direct_io {
            response_flags |= FopenFlags::FOPEN_DIRECT_IO;
        }
        if opened.nonseekable {
            response_flags |= FopenFlags::FOPEN_NONSEEKABLE;
        }
        if opened.keep_cache {
            response_flags |= FopenFlags::FOPEN_KEEP_CACHE;
        }
        if opened.backing_candidate {
            let fhs = self.inner.fhs.read().unwrap();
            if let Some(handle) = fhs.get(&opened.handle) {
                let mut handle = handle.lock().unwrap();
                if let Some(file) = handle.inner.file.as_ref() {
                    if let Ok(backing) = reply.open_backing(file) {
                        reply.opened_passthrough(
                            FileHandle(opened.handle),
                            response_flags,
                            &backing,
                        );
                        handle.inner._backing = Some(backing);
                        return;
                    }
                }
            }
        }
        reply.opened(FileHandle(opened.handle), response_flags);
    }

    fn create(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32,
              _umask: u32, _flags: i32, reply: ReplyCreate) {
        match self.create_node(req.pid(), u64::from(parent), name, mode) {
            Ok((attr, opened)) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(opened.handle),
                FopenFlags::empty(),
            ),
            Err(error) => reply.error(error),
        }
    }

    fn read(&self, req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64,
            size: u32, _flags: OpenFlags, _lo: Option<LockOwner>, reply: ReplyData) {
        // Slip-pool acquire: a read on the JOBSERVER file claims one slip for the
        // caller pid. If the pool is empty we DEFER the reply (block the read)
        // until a release frees one — unless the handle is O_NONBLOCK, in which
        // case the pool denies it with EAGAIN at once.
        let jsfh = self.inner.jobserver_fhs.read().unwrap().get(&u64::from(fh)).copied();
        if let Some(nonblock) = jsfh {
            // Key by the process (TGID), so acquire and release agree and the
            // pidfd reaper can watch a real process. A thread's read maps to its
            // owning process.
            let pid = tgid_of(req.pid()) as i32;
            let r = Box::new(SlipReplyData(Some(reply)));
            let watch = crate::slippool::global().lock().unwrap().acquire(pid, r, nonblock);
            // Drop the pool lock (above) BEFORE registering the watch — watch may
            // itself reap (taking the pool lock) if the pid already exited.
            if let crate::slippool::Watch::Pid(p) = watch {
                crate::slippool::watch_pid(p);
            }
            return;
        }
        let f = match self.read_file_node(u64::from(fh)) {
            Ok(file) => file,
            Err(error) => return reply.error(error),
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
        match self.prepare_write_node(req.pid(), u64::from(fh)) {
            Ok(WriteTarget::Jobserver) => {
                self.release_jobserver_slip(req.pid());
                reply.written(data.len() as u32);
            }
            Ok(WriteTarget::Sink { box_id, stream }) => {
                self.write_sink_node(req.pid(), box_id, stream, data);
                reply.written(data.len() as u32);
            }
            Ok(WriteTarget::File { file, box_id, rel }) => {
                match file.write_at(data, offset) {
                    Ok(written) => {
                        self.finish_file_write(box_id, rel);
                        reply.written(written as u32);
                    }
                    Err(error) => reply.error(Errno::from(error)),
                }
            }
            Err(error) => reply.error(error),
        }
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
               _flags: OpenFlags, _lo: Option<LockOwner>, _flush: bool,
               reply: ReplyEmpty) {
        match self.release_node(u64::from(fh)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn setattr(&self, req: &Request, ino: INodeNo, mode: Option<u32>,
               uid: Option<u32>, gid: Option<u32>, size: Option<u64>,
               _atime: Option<TimeOrNow>, mtime: Option<TimeOrNow>,
               _ctime: Option<SystemTime>, _fh: Option<FileHandle>,
               _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>,
               _bkuptime: Option<SystemTime>, _flags: Option<fuser::BsdFileFlags>,
               reply: ReplyAttr) {
        let mtime = mtime.map(|time| match time {
            TimeOrNow::SpecificTime(time) => time,
            TimeOrNow::Now => SystemTime::now(),
        });
        match self.setattr_node(
            req.pid(),
            u64::from(ino),
            NodeSetattr { mode, uid, gid, size, mtime },
        ) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(error),
        }
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32,
             _umask: u32, reply: ReplyEntry) {
        match self.mkdir_node(req.pid(), u64::from(parent), name, mode) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    fn symlink(&self, req: &Request, parent: INodeNo, link_name: &OsStr,
               target: &Path, reply: ReplyEntry) {
        match self.symlink_node(req.pid(), u64::from(parent), link_name, target) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32,
             _umask: u32, rdev: u32, reply: ReplyEntry) {
        match self.mknod_node(req.pid(), u64::from(parent), name, mode, rdev) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    fn link(&self, req: &Request, ino: INodeNo, newparent: INodeNo,
            newname: &OsStr, reply: ReplyEntry) {
        match self.link_node(req.pid(), u64::from(ino), u64::from(newparent), newname) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    fn fallocate(&self, req: &Request, _ino: INodeNo, fh: FileHandle,
                 offset: u64, length: u64, mode: i32, reply: ReplyEmpty) {
        match self.fallocate_node(req.pid(), u64::from(fh), mode as u32, offset, length) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn setxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, value: &[u8],
                flags: i32, _position: u32, reply: ReplyEmpty) {
        match self.set_xattr_node(u64::from(ino), name, value, flags as u32) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32,
                reply: fuser::ReplyXattr) {
        match self.get_xattr_node(u64::from(ino), name) {
            Ok(val) => {
                if size == 0 { reply.size(val.len() as u32); }
                else if (size as usize) < val.len() { reply.error(Errno::ERANGE); }
                else { reply.data(&val); }
            }
            Err(error) => reply.error(error),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32,
                 reply: fuser::ReplyXattr) {
        match self.list_xattr_node(u64::from(ino)) {
            Ok(buffer) if size == 0 => reply.size(buffer.len() as u32),
            Ok(buffer) if (size as usize) < buffer.len() => reply.error(Errno::ERANGE),
            Ok(buffer) => reply.data(&buffer),
            Err(error) => reply.error(error),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr,
                   reply: ReplyEmpty) {
        match self.remove_xattr_node(u64::from(ino), name) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr,
              reply: ReplyEmpty) {
        match self.unlink_node(req.pid(), u64::from(parent), name) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr,
             reply: ReplyEmpty) {
        match self.rmdir_node(req.pid(), u64::from(parent), name) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    // Safe no-op/durability ops real programs call — ENOSYS here (the fuser
    // default) makes fsync()/access() fail spuriously. Backing fds are real
    // files, so an fsync on them is genuine; flush/access just succeed.
    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
             _lock_owner: LockOwner, reply: ReplyEmpty) {
        match self.sync_file_node(u64::from(fh), false) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool,
             reply: ReplyEmpty) {
        match self.sync_file_node(u64::from(fh), _datasync) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle,
                _datasync: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        match self.getattr_node(u64::from(_ino)) {
            Ok(_) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        match self.statfs_node() {
            Ok(stat) => reply.statfs(
                stat.f_blocks,
                stat.f_bfree,
                stat.f_bavail,
                stat.f_files,
                stat.f_ffree,
                stat.f_bsize as u32,
                stat.f_namemax as u32,
                stat.f_frsize as u32,
            ),
            Err(error) => reply.error(error),
        }
    }

    fn rename(&self, req: &Request, parent: INodeNo, name: &OsStr,
              newparent: INodeNo, newname: &OsStr, flags: fuser::RenameFlags,
              reply: ReplyEmpty) {
        match self.rename_node(
            req.pid(),
            u64::from(parent),
            name,
            u64::from(newparent),
            newname,
            flags.bits(),
        ) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.open_directory(u64::from(ino)) {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Err(error) => reply.error(error),
        }
    }

    fn readdir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64,
               mut reply: ReplyDirectory) {
        let entries = match self.read_directory(u64::from(fh), offset) {
            Ok(entries) => entries,
            Err(error) => return reply.error(error),
        };
        for (index, entry) in entries.into_iter().enumerate() {
            let next = offset.saturating_add(index as u64).saturating_add(1);
            if reply.add(INodeNo(entry.inode), next, entry.kind, entry.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
                   offset: u64, mut reply: ReplyDirectoryPlus) {
        let entries = match self.read_directory(u64::from(fh), offset) {
            Ok(entries) => entries,
            Err(error) => return reply.error(error),
        };
        for (index, entry) in entries.into_iter().enumerate() {
            let Ok(attr) = self.getattr_node(entry.inode) else { continue };
            let next = offset.saturating_add(index as u64).saturating_add(1);
            if reply.add(INodeNo(entry.inode), next, entry.name, &TTL, &attr,
                         Generation(0)) {
                break;
            }
            self.inner.inodes.acquire(entry.inode, 1);
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
                  _flags: OpenFlags, reply: ReplyEmpty) {
        match self.close_directory(u64::from(fh)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
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

/// Canonical filesystem protocol implementation.  vhost-user-fs and the
/// forthcoming SUD ring call this trait directly; the legacy fuser callbacks
/// above are now an adapter over the same policy operations.
impl virtiofsd::filesystem::FileSystem for SarunFs {
    type Inode = u64;
    type Handle = u64;
    type DirIter = crate::sarunfs::DirIter;

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
            inode: u64::from(attr.ino),
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
            .open_node(ctx.pid as u32, inode, flags, false)
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
        Ok((Some(opened.handle), options))
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
                parent,
                OsStr::from_bytes(name.to_bytes()),
                mode,
            )
            .map_err(virtio_error)?;
        Ok((
            virtiofsd::filesystem::Entry {
                inode: u64::from(attr.ino),
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
                parent,
                OsStr::from_bytes(name.to_bytes()),
                mode,
            )
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: u64::from(attr.ino),
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
                parent,
                OsStr::from_bytes(name.to_bytes()),
                mode,
                rdev,
            )
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: u64::from(attr.ino),
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
                parent,
                OsStr::from_bytes(name.to_bytes()),
                Path::new(OsStr::from_bytes(linkname.to_bytes())),
            )
            .map_err(virtio_error)?;
        Ok(virtiofsd::filesystem::Entry {
            inode: u64::from(attr.ino),
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
            inode: u64::from(attr.ino),
            generation: 0,
            attr: crate::sarunfs::virtio_attr(attr),
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn read<W: virtiofsd::filesystem::ZeroCopyWriter>(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        mut writer: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> std::io::Result<usize> {
        let file = self.read_file_node(handle).map_err(virtio_error)?;
        writer.read_from_file_at(&file, size as usize, offset, None)
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
            WriteTarget::Jobserver => {
                self.release_jobserver_slip(ctx.pid as u32);
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
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> std::io::Result<()> {
        self.release_node(handle).map_err(virtio_error)
    }

    fn flush(
        &self,
        _ctx: virtiofsd::filesystem::Context,
        _inode: u64,
        handle: u64,
        _lock_owner: u64,
    ) -> std::io::Result<()> {
        self.sync_file_node(handle, false).map_err(virtio_error)
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
    // resolve/ro_denied/chain_dir_has_children WITHOUT the store (here
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
        assert!(ov.chain_dir_has_children(owner, ""));
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
        // SAFETY: TEST_STATE_HOME_LOCK serializes state-home tests.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let fs = SarunFs::new(tmp.clone());
        let id = 9201;
        fs.add_box(Arc::new(BoxState::create(id).unwrap()));
        let ctx = virtiofsd::filesystem::Context {
            uid: 0.into(),
            gid: 0.into(),
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
        assert_eq!(blob_path(owner, rowid).metadata().unwrap().len(), 4096);
        let mut setattr = virtiofsd::fuse::SetattrIn::default();
        setattr.size = 128;
        setattr.mode = 0o640;
        let (allocated_attr, _) =
            <SarunFs as virtiofsd::filesystem::FileSystem>::setattr(
                &fs,
                ctx,
                allocated.inode,
                setattr,
                None,
                virtiofsd::filesystem::SetattrValid::SIZE
                    | virtiofsd::filesystem::SetattrValid::MODE,
            )
            .unwrap();
        assert_eq!(allocated_attr.size, 128);
        assert_eq!(allocated_attr.mode & 0o7777, 0o640);
        assert_eq!(blob_path(owner, rowid).metadata().unwrap().len(), 128);
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
        assert_eq!(
            <SarunFs as virtiofsd::filesystem::FileSystem>::readlink(
                &fs, ctx, link.inode,
            )
            .unwrap(),
            b"created",
        );
        let hardlink_name = CString::new("hardlink").unwrap();
        let hardlink = <SarunFs as virtiofsd::filesystem::FileSystem>::link(
            &fs,
            ctx,
            created.inode,
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
        assert_eq!(renamed.inode, created.inode);
        <SarunFs as virtiofsd::filesystem::FileSystem>::unlink(
            &fs, ctx, entry.inode, &link_name,
        )
        .unwrap();
        <SarunFs as virtiofsd::filesystem::FileSystem>::unlink(
            &fs, ctx, entry.inode, &hardlink_name,
        )
        .unwrap();
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
        assert!(virtiofsd::filesystem::DirectoryIterator::next(&mut entries).is_none());
        <SarunFs as virtiofsd::filesystem::FileSystem>::releasedir(
            &fs, ctx, 1, 0, handle,
        )
        .unwrap();

        let _ = std::fs::remove_dir_all(tmp);
    }
}
