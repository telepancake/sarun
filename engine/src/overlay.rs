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

/// True if any ANCESTOR directory of `rel` is marked OPAQUE in box `b`. Walks
/// rel's path components upward (rel="a/b/c/d" → checks "a/b/c", "a/b", "a",
/// then the box root ""). The box root itself IS a valid opaque target — a
/// layer can carry a `.wh..wh..opq` directly at its top to opacify EVERYTHING
/// from below. Root rel ("") has no ancestors → false.
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
    sink: Option<i32>, // Some(stream) → writes go to the outputs table, not a blob
    passthrough: bool, // writes go straight to the real host file (uncaptured)
    // Kernel passthrough backing registration; kept alive as long as the fd
    // (its Drop closes the registration). Only set for readonly-ruled reads.
    _backing: Option<fuser::BackingId>,
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
        Self {
            sh: load_glob_lines(
                &crate::paths::shadow_sh_glob_path(),
                &["/bin/sh", "/usr/bin/sh",
                  "/bin/bash", "/usr/bin/bash",
                  "/bin/dash", "/usr/bin/dash"]),
            make: load_glob_lines(
                &crate::paths::shadow_make_glob_path(),
                &["/bin/make", "/usr/bin/make",
                  "/bin/gmake", "/usr/bin/gmake"]),
            ninja: load_glob_lines(
                &crate::paths::shadow_ninja_glob_path(),
                &["/bin/ninja", "/usr/bin/ninja"]),
            self_exe: std::env::current_exe().ok(),
        }
    }
}

fn load_glob_lines(file: &std::path::Path, defaults: &[&str])
    -> Vec<glob::Pattern>
{
    let raw: Vec<String> = match std::fs::read_to_string(file) {
        Ok(s) => s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => defaults.iter().map(|s| (*s).to_string()).collect(),
    };
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

enum Layer {
    Absent,
    UpperFile { owner: i64, rowid: i64, mode: u32 },
    UpperDir { mode: u32, mtime_ns: i64 },
    UpperSymlink { target: PathBuf },
    UpperSpecial { mode: u32, rdev: u64 },
    Lower,
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

impl Overlay {
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
        let mut i2k = HashMap::new();
        i2k.insert(1u64, (0i64, String::new()));
        let mut k2i = HashMap::new();
        k2i.insert((0i64, String::new()), 1u64);
        let ov = Overlay { inner: Arc::new(Inner {
            lower,
            boxes: RwLock::new(BTreeMap::new()),
            ino_to_key: RwLock::new(i2k),
            key_to_ino: RwLock::new(k2i),
            next_ino: AtomicU64::new(2),
            fhs: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            rules: RwLock::new(crate::rules::Rules::load()),
            echo: RwLock::new(HashMap::new()),
            sink_open: Mutex::new(HashMap::new()),
            muted: RwLock::new(std::collections::HashMap::new()),
            passthrough_ok: std::sync::atomic::AtomicBool::new(false),
            daemon_reads: AtomicU64::new(0),
            events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            shadows: RwLock::new(Shadows::load()),
        }) };
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

    /// Box-relative path matches one of the engine-binary FUSE
    /// shadows. Only --api boxes get them — they're the in-box PATH
    /// entries the oaita driver and its sub-agents resolve `sarun`
    /// and `oaita` through. The runner exec's its OWN `inner` via the
    /// inherited /proc/self/fd/N (see runner.rs), so no universal
    /// in-box path is needed for the engine.
    fn is_engine_shadow_path(rel: &str, api: bool) -> bool {
        api && (rel == "usr/local/bin/oaita"
                || rel == "usr/local/bin/sarun")
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
        self.inner.boxes.write().unwrap().insert(b.id, b);
        // Hydrate any at-rest parent chain into the overlay's live box map so
        // resolve()/scan_dir() can WALK INTO the ancestors during the child's
        // FUSE ops. Without this, a child whose parent is an at-rest box (e.g.
        // an OCI image layer created by `sarun oci load`) would see the chain
        // truncate at its own contents — every read past its own entries would
        // fall through to host (or Absent under no_host_fallback), missing
        // every layer below. Idempotent — already-loaded ancestors are kept.
        self.hydrate_chain(parent);
    }

    /// Open + load-mirror each at-rest box up the parent chain rooted at
    /// `start`, adding to `self.boxes` (under the same lock discipline as
    /// add_box). Stops on missing sqlar, on a cycle, or after 64 hops.
    fn hydrate_chain(&self, start: Option<i64>) {
        let mut cur = start;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let Some(id) = cur else { return };
            if !seen.insert(id) { return; }
            if self.inner.boxes.read().unwrap().contains_key(&id) {
                // already live — but its parent chain may still need work.
                cur = self.inner.boxes.read().unwrap().get(&id)
                    .and_then(|b| b.parent());
                continue;
            }
            // Open the at-rest sqlar; `BoxState::create` is a CREATE-IF-NOT-
            // EXISTS open + schema upsert (additive), so on an existing
            // sqlar it just rebinds. load_mirror() then populates `kinds`
            // and restores the parent-stack mode flags from meta.
            match BoxState::create(id) {
                Ok(pb) => {
                    pb.load_mirror();
                    let next = pb.parent();
                    self.inner.boxes.write().unwrap()
                        .insert(pb.id, Arc::new(pb));
                    cur = next;
                }
                Err(_) => return,
            }
        }
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
                std::fs::read(crate::capture::blob_path(owner, rowid))
            }
            Layer::Lower => std::fs::read(self.host(rel)),
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
            Layer::UpperFile { .. } => 'f',
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
        let oldp = format!("{rel_o}/");
        let mut k2i = self.inner.key_to_ino.write().unwrap();
        let mut i2k = self.inner.ino_to_key.write().unwrap();
        let moves: Vec<(String, String)> = k2i.keys()
            .filter(|(b, _)| *b == bid)
            .filter_map(|(_, rel)| {
                if rel == rel_o {
                    Some((rel.clone(), rel_n.to_string()))
                } else if let Some(tail) = rel.strip_prefix(&oldp) {
                    Some((rel.clone(), format!("{rel_n}/{tail}")))
                } else {
                    None
                }
            })
            .collect();
        for (old, new) in moves {
            if let Some(ino) = k2i.remove(&(bid, old)) {
                k2i.insert((bid, new.clone()), ino);
                i2k.insert(ino, (bid, new));
            }
        }
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
            None => {
                if self.host(rel).symlink_metadata().is_ok() {
                    Layer::Lower
                } else {
                    Layer::Absent
                }
            }
        }
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
        let mut cur = Some(bid);
        let mut seen = 0;
        // D-parent: any box in the lookup chain having `no_host_fallback` set
        // closes the bottom of the stack — when the parent walk runs out, the
        // path is Absent rather than served from the real host /. Set on the
        // bottom of an OCI image stack so `ls /etc` inside the box sees only
        // the image's /etc, never the host's.
        let mut no_host = false;
        while let Some(id) = cur {
            seen += 1;
            if seen > 64 { break; }
            let Some(b) = self.box_of(id) else { break };
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
                    cur = b.parent();  // not in this box → try its parent
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
        let mut cur = Some(bid);
        let mut seen = 0;
        while let Some(id) = cur {
            seen += 1;
            if seen > 64 { break; }
            let Some(b) = self.box_of(id) else { break };
            let (_white, present) = b.children_of(rel);
            if !present.is_empty() { return true; }
            cur = b.parent();
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
        // Engine-binary FUSE shadow at /usr/local/bin/{oaita,sarun}
        // for --api boxes — the path the in-box oaita driver and its
        // sub-agents look up via PATH. The host doesn't have these
        // paths, so the shadow has to fire BEFORE the lower-layer
        // stat — synthesize the engine binary's attrs directly. No
        // bwrap binds, no host-path leakage into the mount namespace.
        if Self::is_engine_shadow_path(rel, b.is_api()) {
            if let Some(exe) = self.shadow_target_path() {
                if let Ok(md) = std::fs::metadata(&exe) {
                    let mut a = self.attr_from_md(ino, &md);
                    a.kind = FileType::RegularFile;
                    return Some(a);
                }
            }
        }
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

    /// D3: the first actual write to `rel` copies the RESOLVED lower bytes
    /// (the parent box's version if nested, else the host file, else empty)
    /// into a fresh pool blob in THIS box (creating the row + provenance) and
    /// returns the RW blob file.
    fn copy_up(&self, b: &BoxState, rel: &str, pid: u32) -> std::io::Result<File> {
        let writer = b.writer_for(pid);
        // Source the lower bytes + mode from the parent-chain resolution.
        let (src, mode): (Option<PathBuf>, u32) = match self.resolve(b.id, rel) {
            Layer::UpperFile { owner, rowid, mode } =>
                (Some(blob_path(owner, rowid)), mode),
            Layer::Lower => {
                let m = self.host(rel).symlink_metadata().map(|m| m.mode())
                    .unwrap_or(0o100644);
                (Some(self.host(rel)), m)
            }
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
        // chain of box ids, root-first.
        let mut chain = vec![b.id];
        let mut cur = b.parent();
        let mut guard = 0;
        while let Some(p) = cur {
            guard += 1; if guard > 64 { break; }
            chain.push(p);
            cur = self.box_of(p).and_then(|bx| bx.parent());
        }
        chain.reverse();
        // D-parent: skip host seeding when any box in the chain disables it
        // (matches resolve()'s no_host_fallback semantics — the box stack is
        // closed at the bottom, no /etc-from-host bleed-through).
        let no_host = chain.iter().filter_map(|id| self.box_of(*id))
                           .any(|bx| bx.no_host_fallback());
        if !no_host {
            if let Ok(rd) = std::fs::read_dir(self.host(rel)) {
                for ent in rd.flatten() {
                    if let Some(n) = ent.file_name().to_str() {
                        names.insert(n.to_string(), ());
                    }
                }
            }
        }
        for id in chain {
            if let Some(bx) = self.box_of(id) {
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
                let (white, present) = bx.children_of(rel);
                for w in &white { names.remove(w); }
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
}

impl Filesystem for Overlay {
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
        // The hidden synthetic KIDS_DIR at a box root, and routing through it to
        // a live child's REAL overlay-root inode (nested-launch bind target).
        if prel.is_empty() && name == KIDS_DIR {
            let ino = self.ino_for(&(bid, KIDS_DIR.to_string()));
            return reply.entry(&TTL, &self.synth_dir_attr(ino, 0o40755, 0),
                               Generation(0));
        }
        if prel == KIDS_DIR {
            if let Ok(cid) = name.parse::<i64>() {
                if self.box_of(cid).and_then(|c| c.parent()) == Some(bid) {
                    let cino = self.ino_for(&(cid, String::new()));
                    return reply.entry(&TTL,
                        &self.synth_dir_attr(cino, 0o40755, 0), Generation(0));
                }
            }
            return reply.error(Errno::ENOENT);
        }
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        let ino = self.ino_for(&(bid, rel.clone()));
        if prel.is_empty() && sink_stream(&rel).is_some() {
            return reply.entry(&TTL, &self.synth_file_attr(ino), Generation(0));
        }
        // Engine self-hide: sarun's own host dirs are invisible to boxes.
        // --api substitution path is checked inside attr_of BEFORE this,
        // so the substituted oaita.toml stays visible for api boxes.
        if b.is_api()
            && (Self::oaita_config_ancestor_or_self(&rel)
                || Self::oaita_state_ancestor_self_or_within(&rel))
        {
            // exempt — fall through to normal lookup
        } else if Self::is_engine_path(&rel) {
            return reply.error(Errno::ENOENT);
        }
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
        if bid == 0 || rel.is_empty() || rel == KIDS_DIR {
            return reply.attr(&TTL, &self.synth_dir_attr(u64::from(ino), 0o40755, 0));
        }
        if sink_stream(&rel).is_some() {
            return reply.attr(&TTL, &self.synth_file_attr(u64::from(ino)));
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
        let Some(_) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        match self.resolve(bid, &rel) {
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
        if let Some(stream) = sink_stream(&rel) {
            // stdout/stderr sink: a write-only channel into the outputs table
            // (+ the live echo readback). Count it so the last release flushes
            // ECHO_DONE.
            self.note_sink_open(bid);
            let n = self.reg_fh(FhInner {
                box_id: bid, rel, file: None, upper: false, dirty: false,
                last_pid: req.pid(), sink: Some(stream), passthrough: false, _backing: None });
            return reply.opened(FileHandle(n), FopenFlags::empty());
        }
        let want_write = !matches!(flags.acc_mode(),
                                   fuser::OpenAccMode::O_RDONLY);
        // -d direct: the whole box is passthrough (no overlay) — writes land on
        // the real host, uncaptured. Else a per-path passthrough file rule.
        if want_write && (b.direct() || self.is_passthrough(&rel, bid, req.pid())) {
            // passthrough rule: writes go straight to the REAL host file, never
            // captured. Open (creating) the host path directly.
            let host = self.host(&rel);
            if let Some(p) = host.parent() { let _ = std::fs::create_dir_all(p); }
            match OpenOptions::new().read(true).write(true).create(true).open(&host) {
                Ok(f) => {
                    let n = self.reg_fh(FhInner {
                        box_id: bid, rel, file: Some(f), upper: true, dirty: false,
                        last_pid: req.pid(), sink: None, passthrough: true, _backing: None });
                    return reply.opened(FileHandle(n), FopenFlags::empty());
                }
                Err(_) => return reply.error(Errno::EACCES),
            }
        }
        // resolve() so a child opening a file that lives in its PARENT box (or
        // the host) finds it. `upper` (this box owns the blob) is true ONLY when
        // the resolved owner IS this box; a parent's file or the host file is
        // served read-only until the first write triggers copy-up-from-parent.
        // Engine-binary FUSE shadow: same paths attr_of synthesizes for —
        // open the engine binary read-only so the --api box can exec
        // it from /usr/local/bin/{oaita,sarun}. No bwrap binds.
        if !want_write && Self::is_engine_shadow_path(&rel, b.is_api()) {
            if let Some(exe) = self.shadow_target_path() {
                if let Ok(f) = File::open(&exe) {
                    let n = self.reg_fh(FhInner {
                        box_id: bid, rel, file: Some(f), upper: false,
                        dirty: false, last_pid: req.pid(), sink: None,
                        passthrough: false, _backing: None });
                    return reply.opened(FileHandle(n), FopenFlags::empty());
                }
            }
        }
        let (file, upper) = match self.resolve(bid, &rel) {
            Layer::UpperFile { owner, rowid, .. } => {
                let bp = blob_path(owner, rowid);
                let own = owner == bid;
                match OpenOptions::new().read(true).write(want_write && own).open(&bp) {
                    Ok(f) => (Some(f), own),
                    Err(_) => return reply.error(Errno::EIO),
                }
            }
            Layer::Lower => {
                // Brush-mode shadow: open the engine binary instead
                // of the host file when this rel matches a shadow
                // pattern. Read-only — the box never writes back
                // through the shadow (anyone trying to copy-on-write
                // /bin/sh would land here too, but write opens are
                // gated above and this branch is read-only-passthrough).
                // --api substitute: open the safe-for-box oaita.toml
                // instead of the host config when the box is --api and
                // the rel is the host oaita.toml path.
                let host = if b.is_api() && Self::matches_host_oaita_config(&rel) {
                    crate::paths::api_box_oaita_toml_path()
                } else if b.is_brush() && self.shadow_matches(&rel) {
                    self.shadow_target_path().unwrap_or_else(|| self.host(&rel))
                } else { self.host(&rel) };
                match File::open(host) {
                    Ok(f) => (Some(f), false),
                    Err(_) => return reply.error(Errno::EACCES),
                }
            },
            _ => return reply.error(Errno::ENOENT),
        };
        // D5 (rule-gated): a READ-ONLY open of a HOST-DIRECT path (the existing
        // `passthrough` file rule, or a -d direct box) gets a kernel backing fd,
        // so the kernel serves reads with the daemon out of the loop (the build-
        // read-storm win). The user declares these paths host-direct via the
        // rule — never an automatic guess. Exec opens stay daemon-served (mmap of
        // a passthrough-backed file EIOs). The kernel limit (a write-open of a
        // file with a live passthrough read fd EIOs) is therefore SCOPED to
        // user-declared host-direct paths, and — because passthrough rules are
        // PATH-ONLY — those paths are host-direct in every box, so the EIO is
        // uniform, never a captured-vs-passthrough divergence (see DESIGN.md D5).
        const FMODE_EXEC: i32 = 0x20;
        let is_exec = flags.0 & FMODE_EXEC != 0;
        if !want_write && !is_exec && (b.direct() || self.is_passthrough_read(&rel))
            && self.inner.passthrough_ok.load(Ordering::Relaxed) {
            if let Some(f) = file.as_ref() {
                if let Ok(backing) = reply.open_backing(f) {
                    let n = self.reg_fh(FhInner {
                        box_id: bid, rel, file, upper, dirty: false,
                        last_pid: req.pid(), sink: None, passthrough: false,
                        _backing: None });
                    reply.opened_passthrough(FileHandle(n), FopenFlags::empty(),
                                             &backing);
                    if let Some(h) = self.inner.fhs.read().unwrap().get(&n) {
                        h.lock().unwrap().inner._backing = Some(backing);
                    }
                    return;
                }
                // open_backing failed: fall through to daemon-served.
            }
        }
        let n = self.reg_fh(FhInner {
            box_id: bid, rel, file, upper, dirty: false, last_pid: req.pid(), sink: None, passthrough: false, _backing: None });
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
        if b.direct() || self.is_passthrough(&rel, bid, req.pid()) {
            // passthrough (file rule, or -d whole-box direct): create the file on
            // the REAL host, uncaptured.
            let host = self.host(&rel);
            if let Some(p) = host.parent() { let _ = std::fs::create_dir_all(p); }
            match OpenOptions::new().read(true).write(true).create(true)
                .truncate(true).open(&host) {
                Ok(f) => {
                    let ino = self.ino_for(&(bid, rel.clone()));
                    let md = f.metadata().ok();
                    let mut attr = md.map(|m| self.attr_from_md(ino, &m))
                        .unwrap_or_else(|| self.synth_file_attr(ino));
                    attr.kind = FileType::RegularFile;
                    attr.perm = (mode & 0o7777) as u16;
                    let n = self.reg_fh(FhInner {
                        box_id: bid, rel, file: Some(f), upper: true, dirty: false,
                        last_pid: req.pid(), sink: None, passthrough: true, _backing: None });
                    return reply.created(&TTL, &attr, Generation(0),
                                         FileHandle(n), FopenFlags::empty());
                }
                Err(_) => return reply.error(Errno::EACCES),
            }
        }
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
        self.push_event(bid, rel.clone(), "create");
        let n = self.reg_fh(FhInner {
            box_id: bid, rel, file: Some(f), upper: true,
            dirty: true, last_pid: req.pid(), sink: None, passthrough: false, _backing: None });
        reply.created(&TTL, &attr, Generation(0), FileHandle(n),
                      FopenFlags::empty());
    }

    fn read(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64,
            size: u32, _flags: OpenFlags, _lo: Option<LockOwner>, reply: ReplyData) {
        // The daemon served this read (a passthrough'd read never reaches here —
        // the kernel serves it directly). Counter is test observability.
        self.inner.daemon_reads.fetch_add(1, Ordering::Relaxed);
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
        if let Some(stream) = h.inner.sink {
            // stdout/stderr sink. MUTE: a muted writer's write to an ANCESTOR
            // box's sink (owner box != this sink's box) is a nested box's echo
            // readback travelling UP — echo it onward so it keeps propagating,
            // but do NOT record it (already captured once at the origin box). A
            // muted writer's write to ITS OWN box's sink, however, is first-party
            // output (e.g. a brush in-process builtin writing fd 1 from the muted
            // --inner pid) and IS recorded. A non-muted writer is always
            // recorded. Either way, echo it live.
            let bid = h.inner.box_id;
            let pid = req.pid();
            drop(h);
            drop(fhs);
            let record = match self.muted_owner(pid) {
                None => true,             // not muted: record
                Some(owner) => owner == bid,  // muted: record only its own sink
            };
            if record {
                if let Some(b) = self.box_of(bid) {
                    b.add_output(stream, pid, data);
                }
            }
            self.echo_send(bid, stream, data);
            return reply.written(data.len() as u32);
        }
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
        let box_id = h.inner.box_id;
        let rel = h.inner.rel.clone();
        match f.write_at(data, offset) {
            Ok(n) => {
                // Per-write notification so a subscribed UI can refresh a
                // live box's panes without waiting for its periodic tick.
                drop(h); drop(fhs);
                self.push_event(box_id, rel, "write");
                reply.written(n as u32)
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
               _flags: OpenFlags, _lo: Option<LockOwner>, _flush: bool,
               reply: ReplyEmpty) {
        let h = self.inner.fhs.write().unwrap().remove(&u64::from(fh));
        if let Some(h) = h {
            let h = h.into_inner().unwrap();
            if h.inner.sink.is_some() {
                // A capture sink closed (child exited / redirected fd done): when
                // the box's last sink releases, flush ECHO_DONE so --inner stops
                // reading without truncating still-in-flight echo bytes.
                self.note_sink_release(h.inner.box_id);
            } else if h.inner.dirty && !h.inner.passthrough {
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
               uid: Option<u32>, gid: Option<u32>, size: Option<u64>,
               _atime: Option<TimeOrNow>, mtime: Option<TimeOrNow>,
               _ctime: Option<SystemTime>, _fh: Option<FileHandle>,
               _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>,
               _bkuptime: Option<SystemTime>, _flags: Option<fuser::BsdFileFlags>,
               reply: ReplyAttr) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        // HOST-DIRECT (passthrough file rule, or -d direct): metadata ops hit the
        // REAL host file, never copy-up/capture — mirroring the host-direct
        // read/write path. This is the fix for the O_TRUNC bug: the kernel
        // delivers `> file`'s truncate as setattr(size=0); routing it through
        // copy_up captured a spurious row AND left the host file's tail intact
        // (the write went host-direct, the truncate went to a blob). Truncate
        // propagates the real errno; chmod/chown/utimes are best-effort.
        if b.direct() || self.is_passthrough(&rel, bid, req.pid()) {
            let host = self.host(&rel);
            let cpath = std::ffi::CString::new(host.as_os_str().as_encoded_bytes());
            if let Some(sz) = size {
                match OpenOptions::new().write(true).open(&host) {
                    Ok(f) => if let Err(e) = f.set_len(sz) {
                        return reply.error(Errno::from(e));
                    },
                    Err(e) => return reply.error(Errno::from(e)),
                }
            }
            if let (Some(m), Ok(c)) = (mode, &cpath) {
                unsafe { libc::chmod(c.as_ptr(), (m & 0o7777) as libc::mode_t); }
            }
            if (uid.is_some() || gid.is_some()) && cpath.is_ok() {
                let c = cpath.as_ref().unwrap();
                // uid_t (-1) == no change.
                unsafe { libc::lchown(c.as_ptr(), uid.unwrap_or(u32::MAX),
                                      gid.unwrap_or(u32::MAX)); }
            }
            if let (Some(t), Ok(c)) = (mtime, &cpath) {
                let st = match t {
                    TimeOrNow::SpecificTime(s) => s,
                    TimeOrNow::Now => SystemTime::now(),
                };
                let d = st.duration_since(UNIX_EPOCH).unwrap_or_default();
                let ts = libc::timespec { tv_sec: d.as_secs() as libc::time_t,
                                          tv_nsec: d.subsec_nanos() as i64 };
                let times = [ts, ts];
                unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(),
                                         times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW); }
            }
            return match self.attr_of(&b, u64::from(ino), &rel) {
                Some(a) => reply.attr(&TTL, &a),
                None => reply.error(Errno::ENOENT),
            };
        }
        if let Some(sz) = size {
            // truncate: a write — copy-up if still lower, then set_len.
            let f = match self.layer(&b, &rel) {
                Layer::UpperFile { rowid, .. } => OpenOptions::new().write(true)
                    .open(blob_path(bid, rowid)).ok(),
                Layer::Lower => self.copy_up(&b, &rel, req.pid()).ok(),
                _ => None,
            };
            let Some(f) = f else { return reply.error(Errno::EIO) };
            // Propagate the REAL kernel errno (EFBIG/EINVAL on an over-large
            // truncate, etc.) — not a blanket EIO that hides it.
            if let Err(e) = f.set_len(sz) {
                return reply.error(Errno::from(e));
            }
        }
        if let Some(m) = mode {
            // chmod: the row's mode is the truth (blob perms are an artifact).
            // Files and dirs both; a still-lower target is copied up / captured
            // first so the mode change has a row to live on.
            let perm = m & 0o7777;
            let writer = b.writer_for(req.pid());
            match self.layer(&b, &rel) {
                Layer::UpperFile { .. } => b.set_mode(&rel, 0o100000 | perm),
                Layer::UpperDir { .. } => b.set_mode(&rel, 0o040000 | perm),
                Layer::UpperSymlink { .. } => {}   // symlink mode is ignored
                Layer::Lower => {
                    if self.host(&rel).is_dir() {
                        b.set_dir(&rel, perm, writer);
                    } else if self.copy_up(&b, &rel, req.pid()).is_ok() {
                        b.set_mode(&rel, 0o100000 | perm);
                    }
                }
                Layer::Absent => {}
                Layer::UpperSpecial { .. } => {}
            }
        }
        // chown: a regular file does a REAL chown on its backing blob and
        // propagates the errno — the non-root engine rejecting chown-to-others
        // with EPERM is the box's actual single-uid reality (matches the Python
        // engine's os.chown-on-backing, and the pjdfstest permission matrices).
        // A dir/symlink chown is an accepted no-op (no backing file to own).
        // The side table still records the request for apply-time restoration.
        if uid.is_some() || gid.is_some() {
            let cur = b.owner_of(&rel).unwrap_or((u32::MAX, u32::MAX));
            let nu = uid.unwrap_or(if cur.0 == u32::MAX { 0 } else { cur.0 });
            let ng = gid.unwrap_or(if cur.1 == u32::MAX { 0 } else { cur.1 });
            match self.layer(&b, &rel) {
                Layer::Absent => return reply.error(Errno::ENOENT),
                Layer::UpperDir { .. } | Layer::UpperSymlink { .. }
                | Layer::UpperSpecial { .. } => b.set_owner(&rel, nu, ng),
                Layer::Lower if self.host(&rel).is_dir() => b.set_owner(&rel, nu, ng),
                _ => {
                    // regular file: copy up, then real lchown on the blob.
                    if matches!(self.layer(&b, &rel), Layer::Lower)
                        && self.copy_up(&b, &rel, req.pid()).is_err() {
                        return reply.error(Errno::EIO);
                    }
                    if let Layer::UpperFile { rowid, .. } = self.layer(&b, &rel) {
                        let c = std::ffi::CString::new(
                            blob_path(bid, rowid).as_os_str().as_encoded_bytes()).unwrap();
                        let r = unsafe { libc::lchown(c.as_ptr(), nu, ng) };
                        if r != 0 {
                            return reply.error(Errno::from(
                                std::io::Error::last_os_error()));
                        }
                    }
                    b.set_owner(&rel, nu, ng);
                }
            }
        }
        // utimes: record mtime. A file's getattr reads its BLOB's metadata, so
        // set the blob's mtime too; dirs/symlinks read the row, so set_mtime.
        if let Some(t) = mtime {
            let st = match t {
                TimeOrNow::SpecificTime(s) => s,
                TimeOrNow::Now => SystemTime::now(),
            };
            let ns = st.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if matches!(self.layer(&b, &rel), Layer::Lower)
                && !self.host(&rel).is_dir() {
                let _ = self.copy_up(&b, &rel, req.pid());
            }
            b.set_mtime(&rel, ns);
            if let Layer::UpperFile { rowid, .. } = self.layer(&b, &rel) {
                if let Ok(f) = OpenOptions::new().write(true)
                    .open(blob_path(bid, rowid)) {
                    let _ = f.set_modified(st);
                }
            }
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
        let ino = self.ino_for(&(bid, rel.clone()));
        self.push_event(bid, rel, "mkdir");
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
        let ino = self.ino_for(&(bid, rel.clone()));
        self.push_event(bid, rel, "symlink");
        reply.entry(&TTL, &self.synth_link_attr(
            ino, target.as_os_str().as_encoded_bytes().len() as u64),
            Generation(0));
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32,
             _umask: u32, rdev: u32, reply: ReplyEntry) {
        let Some((bid, prel)) = self.key_of(parent) else {
            return reply.error(Errno::ENOENT);
        };
        if bid == 0 { return reply.error(Errno::EPERM); }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let Some(name) = name.to_str() else { return reply.error(Errno::EINVAL) };
        let rel = if prel.is_empty() { name.to_string() }
                  else { format!("{prel}/{name}") };
        match mode & libc::S_IFMT {
            libc::S_IFREG => {
                // mknod of a regular file = create an empty file.
                let writer = b.writer_for(req.pid());
                let rowid = b.ensure_file_row(&rel, mode, writer);
                let bp = blob_path(bid, rowid);
                if let Some(p) = bp.parent() { let _ = std::fs::create_dir_all(p); }
                let _ = File::create(&bp);
            }
            libc::S_IFIFO | libc::S_IFCHR | libc::S_IFBLK | libc::S_IFSOCK =>
                b.set_special(&rel, mode, rdev as u64, b.writer_for(req.pid())),
            _ => return reply.error(Errno::EINVAL),
        }
        self.push_event(bid, rel.clone(), "mknod");
        let ino = self.ino_for(&(bid, rel.clone()));
        match self.attr_of(&b, ino, &rel) {
            Some(a) => reply.entry(&TTL, &a, Generation(0)),
            None => reply.error(Errno::EIO),
        }
    }

    fn link(&self, req: &Request, ino: INodeNo, newparent: INodeNo,
            newname: &OsStr, reply: ReplyEntry) {
        // Hardlink as copy-up: a new row backed by a fresh copy of the source
        // bytes. Not true inode sharing (nlink stays 1), but it stops the EPERM
        // that breaks git clone --local / ccache — they get a working second
        // name. Same approximation the Python engine's _link_overlay makes.
        let (Some((sbid, srel)), Some((nbid, nprel))) =
            (self.key_of(ino), self.key_of(newparent)) else {
            return reply.error(Errno::ENOENT);
        };
        if sbid != nbid || sbid == 0 { return reply.error(Errno::EXDEV); }
        let Some(b) = self.box_of(sbid) else { return reply.error(Errno::ENOENT) };
        let Some(name) = newname.to_str() else { return reply.error(Errno::EINVAL) };
        let nrel = if nprel.is_empty() { name.to_string() }
                   else { format!("{nprel}/{name}") };
        // materialise source bytes into the new name's blob.
        if self.copy_up(&b, &srel, req.pid()).is_err() {
            return reply.error(Errno::EIO);
        }
        let src_rowid = match self.layer(&b, &srel) {
            Layer::UpperFile { rowid, .. } => rowid,
            _ => return reply.error(Errno::EPERM), // only files link here
        };
        let writer = b.writer_for(req.pid());
        let nrow = b.ensure_file_row(&nrel, 0o100644, writer);
        let dst = blob_path(sbid, nrow);
        if let Some(p) = dst.parent() { let _ = std::fs::create_dir_all(p); }
        if std::fs::copy(blob_path(sbid, src_rowid), &dst).is_err() {
            return reply.error(Errno::EIO);
        }
        let ino2 = self.ino_for(&(sbid, nrel.clone()));
        match self.attr_of(&b, ino2, &nrel) {
            Some(a) => reply.entry(&TTL, &a, Generation(0)),
            None => reply.error(Errno::EIO),
        }
    }

    fn fallocate(&self, req: &Request, ino: INodeNo, _fh: FileHandle,
                 offset: u64, length: u64, _mode: i32, reply: ReplyEmpty) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let f = match self.layer(&b, &rel) {
            Layer::UpperFile { rowid, .. } =>
                OpenOptions::new().write(true).open(blob_path(bid, rowid)).ok(),
            Layer::Lower => self.copy_up(&b, &rel, req.pid()).ok(),
            _ => None,
        };
        let Some(f) = f else { return reply.error(Errno::EIO) };
        // grow the file to offset+length if needed (the common posix_fallocate
        // preallocate path); never shrink.
        let want = offset + length;
        if let Ok(md) = f.metadata() {
            if md.len() < want && f.set_len(want).is_err() {
                return reply.error(Errno::EIO);
            }
        }
        reply.ok();
    }

    fn setxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, value: &[u8],
                _flags: i32, _position: u32, reply: ReplyEmpty) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        if let Some(k) = name.to_str() { b.set_xattr(&rel, k, value); }
        reply.ok();
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32,
                reply: fuser::ReplyXattr) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let v = name.to_str().and_then(|k| b.get_xattr(&rel, k));
        match v {
            Some(val) => {
                if size == 0 { reply.size(val.len() as u32); }
                else if (size as usize) < val.len() { reply.error(Errno::ERANGE); }
                else { reply.data(&val); }
            }
            None => reply.error(Errno::ENODATA),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32,
                 reply: fuser::ReplyXattr) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        let mut buf = Vec::new();
        for k in b.list_xattr(&rel) {
            buf.extend_from_slice(k.as_bytes());
            buf.push(0);
        }
        if size == 0 { reply.size(buf.len() as u32); }
        else if (size as usize) < buf.len() { reply.error(Errno::ERANGE); }
        else { reply.data(&buf); }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr,
                   reply: ReplyEmpty) {
        let Some((bid, rel)) = self.key_of(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        match name.to_str() {
            Some(k) if b.remove_xattr(&rel, k) => reply.ok(),
            _ => reply.error(Errno::ENODATA),
        }
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
            Some(Entry::File { .. }) | Some(Entry::Symlink { .. })
            | Some(Entry::Special { .. }) => {
                b.drop_row(&rel);
                if lower_exists {
                    b.set_whiteout(&rel, writer);
                }
            }
            _ => b.set_whiteout(&rel, writer),
        }
        self.push_event(bid, rel, "unlink");
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
        self.push_event(bid, rel, "rmdir");
        reply.ok();
    }

    // Safe no-op/durability ops real programs call — ENOSYS here (the fuser
    // default) makes fsync()/access() fail spuriously. Backing fds are real
    // files, so an fsync on them is genuine; flush/access just succeed.
    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
             _lock_owner: LockOwner, reply: ReplyEmpty) {
        if let Some(h) = self.inner.fhs.read().unwrap().get(&u64::from(fh)) {
            if let Some(f) = h.lock().unwrap().inner.file.as_ref() {
                let _ = f.sync_all();
            }
        }
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool,
             reply: ReplyEmpty) {
        if let Some(h) = self.inner.fhs.read().unwrap().get(&u64::from(fh)) {
            if let Some(f) = h.lock().unwrap().inner.file.as_ref() {
                let _ = f.sync_all();
            }
        }
        reply.ok();
    }

    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle,
                _datasync: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        reply.ok(); // permission is enforced by the box's bwrap uid, not here
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // Report the lower filesystem's real numbers (df, build free-space checks).
        let c = std::ffi::CString::new(self.inner.lower.as_os_str()
            .as_encoded_bytes()).unwrap();
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0 {
            reply.statfs(s.f_blocks as u64, s.f_bfree as u64, s.f_bavail as u64,
                         s.f_files as u64, s.f_ffree as u64, s.f_bsize as u32,
                         255, s.f_frsize as u32);
        } else {
            reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
        }
    }

    fn rename(&self, req: &Request, parent: INodeNo, name: &OsStr,
              newparent: INodeNo, newname: &OsStr, _flags: fuser::RenameFlags,
              reply: ReplyEmpty) {
        let (Some((bo, po)), Some((bn, pn))) =
            (self.key_of(parent), self.key_of(newparent)) else {
            return reply.error(Errno::EACCES);
        };
        if bo == 0 || bn == 0 || bo != bn {
            return reply.error(Errno::EXDEV); // no cross-box / root rename
        }
        let Some(b) = self.box_of(bo) else { return reply.error(Errno::ENOENT) };
        let (Some(no), Some(nn)) = (name.to_str(), newname.to_str()) else {
            return reply.error(Errno::EINVAL);
        };
        let join = |p: &str, n: &str| if p.is_empty() { n.to_string() }
                                      else { format!("{p}/{n}") };
        let rel_o = join(&po, no);
        let rel_n = join(&pn, nn);
        let writer = b.writer_for(req.pid());
        let lower_o = self.host(&rel_o).symlink_metadata().is_ok();
        match self.layer(&b, &rel_o) {
            Layer::Absent => return reply.error(Errno::ENOENT),
            Layer::UpperDir { .. } => {
                b.reparent(&rel_o, &rel_n);
                if self.host(&rel_o).is_dir() { b.set_whiteout(&rel_o, writer); }
            }
            Layer::Lower => {
                // copy-up the source to a real upper row, then move it.
                match self.copy_up(&b, &rel_o, req.pid()) {
                    Ok(_) => {}
                    Err(_) => return reply.error(Errno::EIO),
                }
                b.rename_row(&rel_o, &rel_n);
                b.set_whiteout(&rel_o, writer); // lower still shows through old name
            }
            Layer::UpperFile { .. } | Layer::UpperSymlink { .. }
            | Layer::UpperSpecial { .. } => {
                b.rename_row(&rel_o, &rel_n);
                if lower_o { b.set_whiteout(&rel_o, writer); }
            }
        }
        self.remap_inode_subtree(bo, &rel_o, &rel_n);
        self.push_event(bo, rel_o, "rename_src");
        self.push_event(bo, rel_n, "rename_dst");
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
        if rel == KIDS_DIR {
            for (i, cid) in self.children_of_box(bid).into_iter().enumerate() {
                if (i as u64) < offset { continue; }
                let cino = self.ino_for(&(cid, String::new()));
                if reply.add(INodeNo(cino), (i + 1) as u64, FileType::Directory,
                             cid.to_string()) { break; }
            }
            return reply.ok();
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        // Engine self-hide: omit children that resolve to one of sarun's
        // own host dirs. --api oaita.toml is exempt (the substituted file
        // stays visible). Matches lookup's gate, so a ls/readdir agrees
        // with stat: nothing tells the box that hidden dirs ever existed.
        let entries: Vec<_> = self.scan_dir(&b, &rel, false).into_iter()
            .filter(|(name, _, _, _)| {
                let child_rel = if rel.is_empty() { name.clone() }
                                else { format!("{rel}/{name}") };
                if b.is_api()
                    && (Self::oaita_config_ancestor_or_self(&child_rel)
                        || Self::oaita_state_ancestor_self_or_within(&child_rel))
                {
                    return true;
                }
                !Self::is_engine_path(&child_rel)
            })
            .collect();
        for (i, (name, kind, cino, _)) in entries.into_iter().enumerate() {
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
        if rel == KIDS_DIR {
            for (i, cid) in self.children_of_box(bid).into_iter().enumerate() {
                if (i as u64) < offset { continue; }
                let cino = self.ino_for(&(cid, String::new()));
                let a = self.synth_dir_attr(cino, 0o40755, 0);
                if reply.add(INodeNo(cino), (i + 1) as u64, cid.to_string(),
                             &TTL, &a, Generation(0)) { break; }
            }
            return reply.ok();
        }
        let Some(b) = self.box_of(bid) else { return reply.error(Errno::ENOENT) };
        // Engine self-hide (mirrors readdir's filter) — see is_engine_path.
        let entries: Vec<_> = self.scan_dir(&b, &rel, true).into_iter()
            .filter(|(name, _, _, _)| {
                let child_rel = if rel.is_empty() { name.clone() }
                                else { format!("{rel}/{name}") };
                if b.is_api()
                    && (Self::oaita_config_ancestor_or_self(&child_rel)
                        || Self::oaita_state_ancestor_self_or_within(&child_rel))
                {
                    return true;
                }
                !Self::is_engine_path(&child_rel)
            })
            .collect();
        for (i, (name, _k, cino, attr)) in entries.into_iter().enumerate() {
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
