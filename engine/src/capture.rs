// Per-box capture state — writes the SAME on-disk layout as the Python engine
// (<box_id>.sqlar with its schema; file bytes as pool blobs at
// live/blob/<box_id>/<rowid%1024:03x>/<rowid>), so the Python readers
// (sqlar_list/sqlar_content/process_list, the review UI) read Rust-captured
// boxes unmodified. Per DESIGN.md D4 file rows are ALWAYS data-NULL with the
// bytes in the blob (no inline tier, no consolidate phase); symlink targets are
// stored raw in the row (sz == len marks "not deflated", which the Python
// reader already handles).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use rusqlite::Connection;
use rusqlite::params;

use crate::paths;

pub const S_IFCHR: u32 = 0o020000; // tombstone mode, matches the Python engine

/// Engine -> UI event queue, shared between the overlay (the file-change
/// producer) and every registered BoxState (the proc-table producer).
/// Items: (sid, rel, op) — the broadcaster in main.rs::serve drains this
/// and turns each entry into a JSON event on the subscribe stream
/// (type=overlay for file ops; type=process_added for op="process_added").
pub type EventQ = Arc<Mutex<VecDeque<(i64, String, &'static str)>>>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT,
 sz INT, data BLOB, opaque INT DEFAULT 0, writer INT, last_writer INT);
CREATE TABLE IF NOT EXISTS provenance(path TEXT PRIMARY KEY, pid INT, ppid INT,
 exe TEXT, argv TEXT);
CREATE TABLE IF NOT EXISTS env(id INTEGER PRIMARY KEY AUTOINCREMENT,
 hash TEXT UNIQUE, env TEXT);
CREATE TABLE IF NOT EXISTS process(id INTEGER PRIMARY KEY AUTOINCREMENT,
 tgid INT, start INT, ppid INT, parent_id INT, exe TEXT, cwd TEXT, argv TEXT,
 env_id INT, root INT DEFAULT 0, brush_pipeline_id INT, UNIQUE(tgid, start));
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS outputs(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, process_id INT, stream INT, content BLOB);
-- Rust-engine extensions (additive; the Python readers ignore them):
CREATE TABLE IF NOT EXISTS xattr(name TEXT, key TEXT, value BLOB,
 PRIMARY KEY(name,key));
CREATE TABLE IF NOT EXISTS ownership(name TEXT PRIMARY KEY, uid INT, gid INT);
CREATE TABLE IF NOT EXISTS rdev(name TEXT PRIMARY KEY, dev INT);
-- D9 brush-shell semantic provenance: one row per pipeline the embedded brush
-- shell (-b) ran, carrying the exact command string + parsed structure (JSON).
--   pipeline: a 0-based ordinal of the pipeline within the brush run, so the
--   reader can present pipelines in execution order independent of row id.
--   spawn_ts: the wall-clock instant brush reported right before spawning this
--   pipeline's complete-command; the [spawn_ts, next spawn_ts) window is what a
--   brush-descendant process's real /proc start time is bucketed into to link it.
--   nested: 1 for a recipe a NESTED shell ran (a `sh -c` the box's command
--   spawned, captured via the brush-sh shim) vs 0 for a TOP-LEVEL pipeline the
--   box's own embedded brush ran. Queryable so a reader can tell the two apart.
CREATE TABLE IF NOT EXISTS brushprov(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, cmd TEXT, record TEXT, pipeline INT, spawn_ts REAL, nested INT DEFAULT 0);
-- Phase 1 embedded-ninja: one row per parsed n2/ninja build edge, captured when
-- the box's `ninja` (vendored n2 in-process) loads build.ninja — INCLUDING
-- up-to-date targets that never execute. `outs`/`ins` are JSON arrays of
-- target/dependency paths; `cmd` is the recipe command line (NULL for phony).
CREATE TABLE IF NOT EXISTS build_edges(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, outs TEXT, ins TEXT, cmd TEXT);
";

#[derive(Clone)]
pub enum Entry {
    File { rowid: i64, mode: u32 },
    Dir { mode: u32, mtime_ns: i64, opaque: bool },
    Symlink { target: PathBuf },
    Special { mode: u32, rdev: u64 },  // fifo / char / block device
    Whiteout,
}

pub struct BoxState {
    pub id: i64,
    pub conn: Mutex<Connection>,
    pub kinds: RwLock<HashMap<String, Entry>>,
    /// Optional shared engine->UI event queue. The overlay calls
    /// set_event_sink() in add_box() with its own Arc-wrapped queue,
    /// so when record_proc inserts a NEW process row it can push a
    /// (sid, "", "process_added") entry directly — no polling, no
    /// race with box teardown removing the BoxState before a 200 ms
    /// drainer notices. Stays None for off-line uses (apply review,
    /// e.g.) that don't need to broadcast.
    event_sink: Mutex<Option<EventQ>>,
    // Process FOREST caches (mirror the Python Index._proc_cache/_proc_current):
    //   proc_cache:   (tgid,start) -> process row id  — incarnation identity.
    //   proc_current: tgid -> (start, row id)         — the latest-seen incarnation,
    //                                                    used to resolve a parent_id.
    // A pid is reused over time; the (tgid,start) key makes a reused pid with a new
    // start_time a NEW row, never a dedup into a stale incarnation (PID-reuse proof).
    proc_cache: Mutex<HashMap<(u32, i64), i64>>,
    proc_current: Mutex<HashMap<u32, (i64, i64)>>,
    // The box's root runner tgids (the bubble-walk boundary): the host pid(s) of
    // each `sarun -- cmd` launch into this box. A PPid chain stops when it reaches
    // one of these (never walk above a launch into the runner's host ancestry).
    roots: Mutex<std::collections::HashSet<u32>>,
    parent: std::sync::atomic::AtomicI64, // 0 = top-level; else parent box_id
    // -e env capture: record each writer's full environment (deduped in `env`).
    env_capture: std::sync::atomic::AtomicBool,
    // -d direct: the box has NO overlay — every write goes straight to the real
    // host file, uncaptured (mirrors Python's whole-box passthrough=direct).
    direct: std::sync::atomic::AtomicBool,
    // ── parent-stack modes (D-parent: two new flags + the default) ───────────
    // readonly_parent: a child's `apply` REFUSES to promote into the parent.
    //   The captured changes can still be reviewed/discarded; they just never
    //   leak up the box stack. Per-CHILD attitude, not a parent property —
    //   different children of the same parent can pick different modes.
    // no_host_fallback: the lower-chain does NOT bottom out at the real host
    //   filesystem. When the parent walk in resolve()/scan_dir() runs out, the
    //   path is Absent rather than served from /. Set on the bottom of an OCI
    //   image stack so `ls /etc` inside the box sees only the image's /etc,
    //   never the host's. Propagates UP the chain — any box in the lookup
    //   chain having it set switches the chain to no-host mode.
    readonly_parent: std::sync::atomic::AtomicBool,
    no_host_fallback: std::sync::atomic::AtomicBool,
    // ── brush↔process linkage (D9) ───────────────────────────────────────────
    // The HOST tgid of this box's embedded brush shell (-b) --inner process, as
    // resolved by the engine from the MUTE pidfd. 0 = not a brush box. Every
    // process brush fork/execs is a descendant of this tgid in the forest, so a
    // process is "spawned by brush" iff its parent_id chain reaches the brush
    // --inner row (whose own tgid == this).
    // True when the box was launched with -b (embedded brush shell). The MUTE
    // handler stamps brush_host_tgid only for these, so a normal box's --inner
    // mute never gets mistaken for a brush root.
    is_brush: std::sync::atomic::AtomicBool,
    brush_host_tgid: std::sync::atomic::AtomicU32,
    // brush↔process link inputs: (brushprov row id, literal WRITE-redirect target
    // paths the pipeline opens for output). Collected as each FRAME_PROV arrives,
    // consumed at teardown (finalize_brush_links). The link is EXACT and race
    // free: a pipeline's output-redirect target file is written by exactly that
    // pipeline's process, so stamping that file's last_writer process row with the
    // pipeline id needs no timing/clock comparison at all.
    brush_links: Mutex<Vec<(i64, Vec<String>)>>,
}

impl BoxState {
    /// Plug the shared engine event queue in; overlay::add_box calls
    /// this with its own Arc-wrapped queue so record_proc can push
    /// notifications directly. Idempotent — overwrites any prior sink.
    pub fn set_event_sink(&self, sink: EventQ) {
        *self.event_sink.lock().unwrap() = Some(sink);
    }

    /// Append one (sid, rel, op) entry to the shared event queue, if a
    /// sink is plugged in. Bounded — sheds the oldest half on overflow
    /// (matches the overlay's own file-event push_event policy).
    fn push_event(&self, rel: &str, op: &'static str) {
        let g = self.event_sink.lock().unwrap();
        let Some(sink) = g.as_ref() else { return };
        let mut q = sink.lock().unwrap();
        if q.len() >= 4096 {
            let drop_n = 4096 / 2;
            q.drain(..drop_n);
        }
        q.push_back((self.id, rel.to_string(), op));
    }

    pub fn set_parent(&self, p: Option<i64>) {
        self.parent.store(p.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }
    pub fn parent(&self) -> Option<i64> {
        match self.parent.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None, p => Some(p),
        }
    }
    pub fn set_env_capture(&self, on: bool) {
        self.env_capture.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn env_capture(&self) -> bool {
        self.env_capture.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_direct(&self, on: bool) {
        self.direct.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn direct(&self) -> bool {
        self.direct.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_readonly_parent(&self, on: bool) {
        self.readonly_parent.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn readonly_parent(&self) -> bool {
        self.readonly_parent.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_no_host_fallback(&self, on: bool) {
        self.no_host_fallback.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn no_host_fallback(&self) -> bool {
        self.no_host_fallback.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Record the HOST tgid of this box's embedded brush --inner process (the
    /// brush↔process linkage root). The engine resolves it from the MUTE pidfd
    /// the brush shell sends and stamps it here, so the forest walk can decide
    /// which process rows brush spawned. 0 disables linkage.
    pub fn set_is_brush(&self, on: bool) {
        self.is_brush.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn is_brush(&self) -> bool {
        self.is_brush.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_brush_host_tgid(&self, tgid: u32) {
        self.brush_host_tgid.store(tgid, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn brush_host_tgid(&self) -> u32 {
        self.brush_host_tgid.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Called when a pipeline's FRAME_PROV arrives: remember its (brushprov id,
    /// literal output-redirect target paths) so the EXACT link can be made at
    /// teardown. `targets` are box-absolute paths as brush parsed them.
    pub fn on_brush_prov(&self, pipeline_id: i64, targets: Vec<String>) {
        if pipeline_id == 0 || targets.is_empty() { return; }
        self.brush_links.lock().unwrap().push((pipeline_id, targets));
    }

    /// Finalize the brush↔process linkage at box teardown, when the brush shell
    /// has exited and every process row + file row exists. For each pipeline that
    /// declared an output-redirect target, resolve that file's LAST writer process
    /// row (the process that actually streamed the pipeline's output into it) and
    /// stamp it with the pipeline id — IF that process is a real brush descendant
    /// (forest ancestry reaches the brush --inner row), a guard against a stale
    /// pre-existing writer. This is EXACT and race-free: a pipeline's `> file`
    /// target is written by exactly that pipeline's process, no clock involved.
    pub fn finalize_brush_links(&self) {
        let bt = self.brush_host_tgid();
        if bt == 0 { return; }
        let links = std::mem::take(&mut *self.brush_links.lock().unwrap());
        if links.is_empty() { return; }
        let conn = self.conn.lock().unwrap();
        // Forest map for the descendant guard.
        let mut by_id: HashMap<i64, (u32, Option<i64>)> = HashMap::new();
        if let Ok(mut st) = conn.prepare("SELECT id,tgid,parent_id FROM process") {
            if let Ok(it) = st.query_map([], |r| Ok((
                r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u32,
                r.get::<_, Option<i64>>(2)?))) {
                for (id, tg, par) in it.flatten() { by_id.insert(id, (tg, par)); }
            }
        }
        let is_brush_descendant = |start: i64| -> bool {
            let mut cur = by_id.get(&start).and_then(|(_, p)| *p);
            let mut hops = 0;
            while let Some(p) = cur {
                if hops > 128 { return false; }
                hops += 1;
                match by_id.get(&p) {
                    Some((ptg, ppar)) => {
                        if *ptg == bt { return true; }
                        cur = *ppar;
                    }
                    None => return false,
                }
            }
            false
        };
        for (pipeline_id, targets) in links {
            for t in targets {
                // Targets are box-absolute (/root/x); sqlar names are relative.
                let rel = t.trim_start_matches('/');
                let writer: Option<i64> = conn.query_row(
                    "SELECT last_writer FROM sqlar WHERE name=?1", [rel],
                    |r| r.get::<_, Option<i64>>(0)).ok().flatten();
                let Some(w) = writer else { continue };
                // The writer either IS the brush --inner process itself (an
                // IN-PROCESS pipeline stage: a brush builtin or a bundled
                // coreutil like `tr`/`sort` writing the redirect target from the
                // --inner pid — no forked child) OR is a forked descendant of it.
                // Both are this pipeline's writer; link both. (Pre-coreutils a
                // writer == the brush root meant "brush wrote it, not a pipeline
                // process" and was skipped — that no longer holds now that
                // pipeline stages can run in-process.)
                let writer_is_root = by_id.get(&w).map(|(tg, _)| *tg) == Some(bt);
                if !writer_is_root && !is_brush_descendant(w) { continue; }
                let _ = conn.execute(
                    "UPDATE process SET brush_pipeline_id=?2 \
                     WHERE id=?1 AND brush_pipeline_id IS NULL",
                    params![w, pipeline_id]);
            }
        }
    }

    /// Dedup the environment text (a stable JSON object string) into the `env`
    /// table by hash (the Python `env` table dedups by sha256; the Rust readers
    /// join on env_id, not the hash, so any stable unique key suffices) and
    /// return its env_id. Caller holds the conn lock.
    fn ensure_env(conn: &Connection, env_json: &str) -> Option<i64> {
        use std::hash::Hash;
        use std::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        env_json.hash(&mut h);
        let hash = format!("{:016x}", h.finish());
        let _ = conn.execute("INSERT OR IGNORE INTO env(hash,env) VALUES(?1,?2)",
                             params![hash, env_json]);
        conn.query_row("SELECT id FROM env WHERE hash=?1", [hash], |r| r.get(0)).ok()
    }

    /// Read /proc/<pid>/environ and return it as a stable JSON object string
    /// ({"VAR":"val",...}, keys sorted), matching the Python env-capture shape
    /// (json.dumps(env, sort_keys=True)). None if unreadable. A BTreeMap gives
    /// serde_json sorted-key output.
    fn read_environ_json(pid: u32) -> Option<String> {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let mut map = std::collections::BTreeMap::new();
        for kv in raw.split(|b| *b == 0) {
            if kv.is_empty() { continue; }
            let s = String::from_utf8_lossy(kv);
            if let Some((k, v)) = s.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
        serde_json::to_string(&map).ok()
    }
}

pub fn blob_path(box_id: i64, rowid: i64) -> PathBuf {
    paths::live_home()
        .join("blob")
        .join(box_id.to_string())
        .join(format!("{:03x}", rowid % 1024))
        .join(rowid.to_string())
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

impl BoxState {
    pub fn create(id: i64) -> rusqlite::Result<Self> {
        let db = paths::state_home().join(format!("{id}.sqlar"));
        let conn = Connection::open(&db)?;
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF;")?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", 1)?;
        Ok(BoxState {
            id,
            conn: Mutex::new(conn),
            kinds: RwLock::new(HashMap::new()),
            event_sink: Mutex::new(None),
            proc_cache: Mutex::new(HashMap::new()),
            proc_current: Mutex::new(HashMap::new()),
            roots: Mutex::new(std::collections::HashSet::new()),
            parent: std::sync::atomic::AtomicI64::new(0),
            env_capture: std::sync::atomic::AtomicBool::new(false),
            direct: std::sync::atomic::AtomicBool::new(false),
            readonly_parent: std::sync::atomic::AtomicBool::new(false),
            no_host_fallback: std::sync::atomic::AtomicBool::new(false),
            is_brush: std::sync::atomic::AtomicBool::new(false),
            brush_host_tgid: std::sync::atomic::AtomicU32::new(0),
            brush_links: Mutex::new(vec![]),
        })
    }

    /// Repopulate the in-RAM `kinds` mirror + the tgid->row cache from an
    /// existing on-disk sqlar. Used on RERUN: a `run NAME` into an existing box
    /// reopens its db, so the prior run's writes must show through and previously
    /// recorded processes keep their row ids (so a new root is an ADDITIONAL
    /// row, not a dedup). Mirrors the Python Index._load_mirror.
    pub fn load_mirror(&self) {
        let conn = self.conn.lock().unwrap();
        // D-parent: restore the box's parent-stack modes (read-only-parent /
        // no-host-fallback) from sqlar meta so a rerun reopens with the same
        // semantics. A missing/unset key means the default (off).
        let read_flag = |k: &str| -> bool {
            conn.query_row("SELECT value FROM meta WHERE key=?1", [k],
                           |r| r.get::<_, String>(0))
                .ok().as_deref() == Some("1")
        };
        if read_flag("readonly_parent") { self.set_readonly_parent(true); }
        if read_flag("no_host_fallback") { self.set_no_host_fallback(true); }
        // Parent linkage: an at-rest sqlar carries parent_box_id in meta;
        // restore it so load-mirror-from-disk produces the same in-RAM
        // BoxState shape a register handshake would. Without this an
        // OCI-loaded layer hydrated into the overlay would look top-level
        // and its OWN parent chain would be invisible to resolve().
        if let Ok(s) = conn.query_row(
            "SELECT value FROM meta WHERE key='parent_box_id'", [],
            |r| r.get::<_, String>(0))
        {
            if let Ok(p) = s.parse::<i64>() {
                if p > 0 { self.set_parent(Some(p)); }
            }
        }
        let mut kinds = self.kinds.write().unwrap();
        if let Ok(mut st) = conn.prepare("SELECT name,mode,sz,data FROM sqlar") {
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32,
                    r.get::<_, i64>(2)?, r.get::<_, Option<Vec<u8>>>(3)?))
            });
            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let (name, mode, sz, data) = row;
                    let ft = mode & 0o170000;
                    let entry = if mode == S_IFCHR {
                        Entry::Whiteout
                    } else if ft == 0o120000 {
                        let bytes = data.unwrap_or_default();
                        let t = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = sz;
                        Entry::Symlink { target: PathBuf::from(t) }
                    } else if ft == 0o040000 {
                        // D-opaque (OCI): a non-zero opaque column flips the
                        // dir's mask-lower attitude — when this box appears in
                        // the chain, the dir's lower-layer contents are wiped.
                        let opaque: i64 = conn.query_row(
                            "SELECT opaque FROM sqlar WHERE name=?1", [&name],
                            |r| r.get(0)).unwrap_or(0);
                        Entry::Dir { mode, mtime_ns: 0, opaque: opaque != 0 }
                    } else if ft == 0o010000 || ft == 0o060000 {
                        Entry::Special { mode, rdev: 0 }
                    } else {
                        let rowid: i64 = conn.query_row(
                            "SELECT rowid FROM sqlar WHERE name=?1", [&name],
                            |r| r.get(0)).unwrap_or(0);
                        Entry::File { rowid, mode }
                    };
                    kinds.insert(name, entry);
                }
            }
        }
        // Repopulate the incarnation caches + hierarchy roots from any rows recorded
        // by earlier runs of this box (rerun reopens the same db). ORDER BY id ASC so
        // proc_current keeps the highest-id incarnation per tgid as "current".
        let mut cache = self.proc_cache.lock().unwrap();
        let mut current = self.proc_current.lock().unwrap();
        let mut roots = self.roots.lock().unwrap();
        if let Ok(mut st) = conn.prepare(
            "SELECT id,tgid,start,root FROM process ORDER BY id") {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u32,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get::<_, i64>(3)?))
            }) {
                for (rid, tgid, start, root) in rows.flatten() {
                    cache.insert((tgid, start), rid);
                    current.insert(tgid, (start, rid));
                    if root != 0 { roots.insert(tgid); }
                }
            }
        }
    }

    // ── process FOREST (mirror of the Python Index process-tree builder) ──────
    //
    // The capture records a CONNECTED FOREST: each writing process's row carries a
    // `parent_id` pointing at the ROW id of its parent process's current incarnation,
    // chained up the PPid ladder to a ROOT (the box's `sarun -- cmd` runner, root=1).
    // Process identity is (tgid,start) — start is field 22 of /proc/<pid>/stat, so a
    // reused pid with a new start_time is a new row, not a dedup. Caches mirror
    // Python's _proc_cache ((tgid,start)->row) and _proc_current (tgid->latest row).

    /// The thread-group id of `pid` (FUSE ctx.pid is a thread id) from
    /// /proc/<pid>/status `Tgid:`; falls back to `pid` itself if unreadable.
    fn tgid_of(pid: u32) -> u32 {
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("Tgid:") {
                    if let Ok(t) = rest.trim().parse::<u32>() { return t; }
                }
            }
        }
        pid
    }

    /// (ppid, start_time) parsed from /proc/<pid>/stat. The comm field (field 2)
    /// may contain spaces and ')' so the split is anchored after the LAST ')',
    /// matching the Python `_parse_proc_stat`. Both 0 on any error.
    fn parse_stat(pid: u32) -> (u32, i64) {
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/stat")) else { return (0, 0) };
        let s = String::from_utf8_lossy(&raw);
        let Some(rp) = s.rfind(')') else { return (0, 0) };
        // fields after ')': field 3 (state) is rest[0]; ppid is field 4 -> rest[1];
        // starttime is field 22 -> rest[19].
        let rest: Vec<&str> = s[rp + 1..].split_whitespace().collect();
        let ppid = rest.get(1).and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
        let start = rest.get(19).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
        (ppid, start)
    }

    fn start_time_of(pid: u32) -> i64 { Self::parse_stat(pid).1 }

    /// Best-effort provenance of a process by host pid: (tgid, start, ppid, exe,
    /// cwd, argv). A vanished pid yields zeros/empties, never panics.
    fn read_prov(pid: u32) -> (u32, i64, u32, String, String, Vec<String>) {
        let tgid = Self::tgid_of(pid);
        let (ppid, start) = Self::parse_stat(pid);
        let proc_ = |f: &str| format!("/proc/{pid}/{f}");
        let exe = std::fs::read_link(proc_("exe"))
            .map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let cwd = std::fs::read_link(proc_("cwd"))
            .map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let argv: Vec<String> = std::fs::read(proc_("cmdline"))
            .unwrap_or_default().split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        (tgid, start, ppid, exe, cwd, argv)
    }

    /// The process-table row for `pid`, recorded on first sight, with its FULL
    /// ancestry up to the box root linked in (so the table stays one connected
    /// forest). Returns the writer's row id.
    pub fn writer_for(&self, pid: u32) -> i64 {
        let (tgid, start, ppid, exe, cwd, argv) = Self::read_prov(pid);
        if let Some(id) = self.proc_cache.lock().unwrap().get(&(tgid, start)) {
            return *id;
        }
        // -e env capture: read /proc/<pid>/environ now (before any lock, before the
        // process can exit). The pid (thread id) shares the tgid's environ.
        let env_json = if self.env_capture() { Self::read_environ_json(pid) }
                       else { None };
        self.record_proc(tgid, start, ppid, &exe, &cwd, &argv, env_json, false)
            .unwrap_or(tgid as i64)
    }

    /// Insert/dedup ONE process incarnation (identity (tgid,start)) and return its
    /// row id. The parent is recorded FIRST (so parent_id is the parent's CURRENT
    /// incarnation ROW id, never a pid), bubbling the PPid chain up to the box root
    /// — this is what makes the table one connected forest. `root` marks an
    /// incarnation a hierarchy root (the bubbling boundary). Mirrors the Python
    /// `process_from_prov`. `tgid==0` yields None.
    fn record_proc(&self, tgid: u32, start: i64, ppid: u32, exe: &str, cwd: &str,
                   argv: &[String], env_json: Option<String>, root: bool) -> Option<i64> {
        if tgid == 0 { return None; }
        if let Some(rid) = self.proc_cache.lock().unwrap().get(&(tgid, start)).copied() {
            self.proc_current.lock().unwrap().insert(tgid, (start, rid));
            if root {
                self.roots.lock().unwrap().insert(tgid);
                let conn = self.conn.lock().unwrap();
                let _ = conn.execute("UPDATE process SET root=1 WHERE id=?1", [rid]);
            }
            return Some(rid);
        }
        if root { self.roots.lock().unwrap().insert(tgid); }
        // Resolve the parent to its CURRENT incarnation row id (NULL for a root or an
        // unreachable parent) by recording the parent FIRST. A root is its own
        // boundary: never walk above a launch into the runner's host ancestry.
        let parent_id = if root { None }
                        else { self.resolve_parent(ppid, 0, &mut std::collections::HashSet::new()) };
        let conn = self.conn.lock().unwrap();
        let eid: Option<i64> = env_json.and_then(|j| Self::ensure_env(&conn, &j));
        let argv_json = serde_json::to_string(&argv).unwrap_or_default();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO process(tgid,start,ppid,parent_id,exe,cwd,argv,env_id,root)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![tgid, start, ppid, parent_id, exe, cwd, argv_json, eid,
                    if root { 1 } else { 0 }],
        ).unwrap_or(0);
        let rowid: i64 = conn
            .query_row("SELECT id FROM process WHERE tgid=?1 AND start=?2",
                       params![tgid, start], |r| r.get(0))
            .unwrap_or(0);
        drop(conn);
        self.proc_cache.lock().unwrap().insert((tgid, start), rowid);
        self.proc_current.lock().unwrap().insert(tgid, (start, rowid));
        // Notify the broadcaster about new process rows: push directly
        // into the shared event queue (set by overlay::add_box). The
        // broadcaster turns it into a type=process_added event. Direct
        // push, not a counter — no race with box teardown removing the
        // BoxState before a periodic drainer notices.
        if inserted > 0 {
            self.push_event("", "process_added");
        }
        Some(rowid)
    }

    /// Record the parent process `ppid` (and so its whole PPid chain) and return its
    /// CURRENT incarnation ROW id, so the per-box process table forms ONE forest
    /// rooted at each launch and a child's parent_id is a row id (PID-reuse proof),
    /// never a tgid. Best-effort: a failed ancestor /proc read links a minimal row
    /// and stops. STOPS at: ppid<=1 (init), a box root (the launch boundary), a
    /// depth/cycle cap (64 levels / seen-set). Mirrors the Python `_resolve_parent`.
    fn resolve_parent(&self, ppid: u32, depth: u32,
                      seen: &mut std::collections::HashSet<u32>) -> Option<i64> {
        if ppid <= 1 { return None; }
        let ptgid = { let t = Self::tgid_of(ppid); if t == 0 { ppid } else { t } };
        // A box root is the bubbling boundary: its row is the top of the chain. The
        // parent's current incarnation row id is what we link to; do not walk above.
        if self.roots.lock().unwrap().contains(&ptgid) {
            return self.current_row(ptgid);
        }
        if depth >= 64 { return self.current_row(ptgid); }
        if seen.contains(&ptgid) { return self.current_row(ptgid); }
        seen.insert(ptgid);
        // Key the parent on its LIVE (tgid,start): a reused pid is a new incarnation.
        let pstart = Self::start_time_of(ppid);
        if pstart != 0 {
            if let Some(rid) = self.proc_cache.lock().unwrap().get(&(ptgid, pstart)).copied() {
                self.proc_current.lock().unwrap().insert(ptgid, (pstart, rid));
                return Some(rid);
            }
        }
        let (_t, gstart, gppid, exe, cwd, argv) = Self::read_prov(ppid);
        let start = if gstart != 0 { gstart } else { pstart };
        let gtgid = if gppid != 0 { Self::tgid_of(gppid) } else { 0 };
        let env_json = if self.env_capture() { Self::read_environ_json(ppid) } else { None };
        // Record this ancestor (recurses up via its own ppid) and return its row id.
        // Inline the recursion through record_proc's parent resolution by recording
        // with the resolved grandparent chain.
        self.record_proc_with_parent(ptgid, start, gtgid, gppid, &exe, &cwd, &argv,
                                      env_json, depth, seen)
    }

    /// record_proc variant used while bubbling: resolves the parent via the SAME
    /// depth/seen state so the cycle/depth caps span the whole walk.
    #[allow(clippy::too_many_arguments)]
    fn record_proc_with_parent(&self, tgid: u32, start: i64, ppid_tgid: u32,
                               parent_pid: u32, exe: &str, cwd: &str, argv: &[String],
                               env_json: Option<String>, depth: u32,
                               seen: &mut std::collections::HashSet<u32>) -> Option<i64> {
        if tgid == 0 { return None; }
        if let Some(rid) = self.proc_cache.lock().unwrap().get(&(tgid, start)).copied() {
            self.proc_current.lock().unwrap().insert(tgid, (start, rid));
            return Some(rid);
        }
        let parent_id = self.resolve_parent(parent_pid, depth + 1, seen);
        let conn = self.conn.lock().unwrap();
        let eid: Option<i64> = env_json.and_then(|j| Self::ensure_env(&conn, &j));
        let argv_json = serde_json::to_string(&argv).unwrap_or_default();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO process(tgid,start,ppid,parent_id,exe,cwd,argv,env_id,root)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0)",
            params![tgid, start, ppid_tgid, parent_id, exe, cwd, argv_json, eid],
        );
        let rowid: i64 = conn
            .query_row("SELECT id FROM process WHERE tgid=?1 AND start=?2",
                       params![tgid, start], |r| r.get(0))
            .unwrap_or(0);
        drop(conn);
        self.proc_cache.lock().unwrap().insert((tgid, start), rowid);
        self.proc_current.lock().unwrap().insert(tgid, (start, rowid));
        Some(rowid)
    }

    /// The process-table ROW id of `tgid`'s latest-seen incarnation, or None.
    fn current_row(&self, tgid: u32) -> Option<i64> {
        self.proc_current.lock().unwrap().get(&tgid).map(|(_, rid)| *rid)
    }

    /// Upsert the file row for `rel` (data stays NULL — D4) and return its
    /// rowid, which names the pool blob. First writer sticks; last_writer moves.
    pub fn ensure_file_row(&self, rel: &str, mode: u32, writer: i64) -> i64 {
        if let Some(Entry::File { rowid, .. }) = self.kinds.read().unwrap().get(rel) {
            return *rowid;
        }
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO sqlar(name,mode,mtime,sz,data,writer,last_writer)
             VALUES(?1,?2,?3,0,NULL,?4,?4)
             ON CONFLICT(name) DO UPDATE SET mode=excluded.mode,
             last_writer=excluded.last_writer",
            params![rel, mode, now_ns(), writer],
        );
        let rowid: i64 = conn
            .query_row("SELECT rowid FROM sqlar WHERE name=?1", [rel], |r| r.get(0))
            .unwrap_or(0);
        drop(conn);
        self.kinds.write().unwrap()
            .insert(rel.to_string(), Entry::File { rowid, mode });
        rowid
    }

    /// Final size/mtime for a file row once its blob settles (close/flush).
    pub fn finalize_file(&self, rel: &str, sz: i64, mtime_ns: i64, writer: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE sqlar SET sz=?2, mtime=?3, last_writer=?4 WHERE name=?1",
            params![rel, sz, mtime_ns, writer],
        );
    }

    /// Apply a new mode to an existing file/dir row (chmod). The audit found
    /// the old path silently no-op'd: ensure_file_row early-returns for an
    /// existing row and never ran its mode UPDATE. This is the explicit fix.
    pub fn set_mode(&self, rel: &str, full_mode: u32) {
        {
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute("UPDATE sqlar SET mode=?2 WHERE name=?1",
                                 params![rel, full_mode]);
        }
        if let Some(e) = self.kinds.write().unwrap().get_mut(rel) {
            match e {
                Entry::File { mode, .. } => *mode = full_mode,
                Entry::Dir { mode, .. } => *mode = full_mode,
                _ => {}
            }
        }
    }

    /// utimes: store the row's mtime (ns). Files/dirs/symlinks all keep mtime
    /// in the sqlar row, so this is a single UPDATE + mirror touch for dirs.
    pub fn set_mtime(&self, rel: &str, mtime_ns: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("UPDATE sqlar SET mtime=?2 WHERE name=?1",
                             rusqlite::params![rel, mtime_ns]);
        if let Some(Entry::Dir { mtime_ns: m, .. }) =
            self.kinds.write().unwrap().get_mut(rel) {
            *m = mtime_ns;
        }
    }

    /// chown: stored in a side table (the box squashes to one uid in-namespace,
    /// so this is fidelity for apply-time host restoration, not an in-box uid).
    pub fn set_owner(&self, rel: &str, uid: u32, gid: u32) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO ownership(name,uid,gid) VALUES(?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET uid=excluded.uid, gid=excluded.gid",
            rusqlite::params![rel, uid, gid]);
    }

    pub fn owner_of(&self, rel: &str) -> Option<(u32, u32)> {
        self.conn.lock().unwrap().query_row(
            "SELECT uid,gid FROM ownership WHERE name=?1", [rel],
            |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32))).ok()
    }

    // ── xattr (side table; the box's processes get real getfattr/setfattr) ──
    pub fn set_xattr(&self, rel: &str, key: &str, value: &[u8]) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO xattr(name,key,value) VALUES(?1,?2,?3)
             ON CONFLICT(name,key) DO UPDATE SET value=excluded.value",
            rusqlite::params![rel, key, value]);
    }
    pub fn get_xattr(&self, rel: &str, key: &str) -> Option<Vec<u8>> {
        self.conn.lock().unwrap().query_row(
            "SELECT value FROM xattr WHERE name=?1 AND key=?2",
            rusqlite::params![rel, key], |r| r.get(0)).ok()
    }
    pub fn list_xattr(&self, rel: &str) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        let mut out = vec![];
        if let Ok(mut st) = conn.prepare("SELECT key FROM xattr WHERE name=?1") {
            if let Ok(it) = st.query_map([rel], |r| r.get::<_, String>(0)) {
                out = it.flatten().collect();
            }
        }
        out
    }
    pub fn remove_xattr(&self, rel: &str, key: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM xattr WHERE name=?1 AND key=?2",
                     rusqlite::params![rel, key]).map(|n| n > 0).unwrap_or(false)
    }

    /// mknod/mkfifo: a special-file row (mode carries S_IFIFO/S_IFCHR/S_IFBLK);
    /// char/block rdev goes in the side table.
    pub fn set_special(&self, rel: &str, mode: u32, rdev: u64, writer: i64) {
        {
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute(
                "INSERT INTO sqlar(name,mode,mtime,sz,data,writer,last_writer)
                 VALUES(?1,?2,?3,0,NULL,?4,?4)
                 ON CONFLICT(name) DO UPDATE SET mode=excluded.mode",
                rusqlite::params![rel, mode, now_ns(), writer]);
            if rdev != 0 {
                let _ = conn.execute(
                    "INSERT INTO rdev(name,dev) VALUES(?1,?2)
                     ON CONFLICT(name) DO UPDATE SET dev=excluded.dev",
                    rusqlite::params![rel, rdev as i64]);
            }
        }
        self.kinds.write().unwrap()
            .insert(rel.to_string(), Entry::Special { mode, rdev });
    }

    pub fn set_dir(&self, rel: &str, mode: u32, writer: i64) {
        let m = mode | 0o040000;
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO sqlar(name,mode,mtime,sz,data,writer,last_writer)
             VALUES(?1,?2,?3,0,NULL,?4,?4)
             ON CONFLICT(name) DO UPDATE SET mode=excluded.mode",
            params![rel, m, now_ns(), writer],
        );
        drop(conn);
        // Preserve a prior opaque flag on update — set_dir for an existing
        // opaque dir must not silently clear it. Default is false on first
        // creation (use set_opaque() to flip).
        let mut kinds = self.kinds.write().unwrap();
        let was_opaque = matches!(kinds.get(rel),
            Some(Entry::Dir { opaque: true, .. }));
        kinds.insert(rel.to_string(),
            Entry::Dir { mode: m, mtime_ns: now_ns(), opaque: was_opaque });
    }

    /// Mark `rel` as an OPAQUE directory (OCI/AUFS `.wh..wh..opq` semantics):
    /// when this box appears in the resolve/scan_dir chain, the directory's
    /// LOWER-layer contributions are wiped. The dir itself stays visible (the
    /// upper-layer Dir entry is unchanged); only its children from below are
    /// masked. Idempotent. If the dir row doesn't exist yet, it's created.
    pub fn set_opaque(&self, rel: &str, writer: i64) {
        {
            let conn = self.conn.lock().unwrap();
            // Upsert as a dir row with opaque=1. Mode 40755 is a sensible
            // default for an auto-created dir; an explicit later set_dir
            // can refine it (and our update above preserves opaque).
            let _ = conn.execute(
                "INSERT INTO sqlar(name,mode,mtime,sz,data,opaque,writer,last_writer)
                 VALUES(?1,?2,?3,0,NULL,1,?4,?4)
                 ON CONFLICT(name) DO UPDATE SET opaque=1",
                params![rel, 0o040755u32, now_ns(), writer],
            );
        }
        let mut kinds = self.kinds.write().unwrap();
        match kinds.get(rel).cloned() {
            Some(Entry::Dir { mode, mtime_ns, .. }) => {
                kinds.insert(rel.to_string(),
                    Entry::Dir { mode, mtime_ns, opaque: true });
            }
            _ => {
                kinds.insert(rel.to_string(), Entry::Dir {
                    mode: 0o040755, mtime_ns: now_ns(), opaque: true });
            }
        }
    }

    /// Is `rel` an opaque directory in this box? (Used by the overlay
    /// resolve/scan_dir paths to honor the OCI opaque-dir semantics.)
    pub fn is_opaque(&self, rel: &str) -> bool {
        matches!(self.kinds.read().unwrap().get(rel),
            Some(Entry::Dir { opaque: true, .. }))
    }

    pub fn set_symlink(&self, rel: &str, target: &std::path::Path, writer: i64) {
        let t = target.as_os_str().as_encoded_bytes();
        let conn = self.conn.lock().unwrap();
        // Raw bytes with sz == len: the Python reader treats len(data)==sz as
        // "not deflated" and returns the bytes as-is.
        let _ = conn.execute(
            "INSERT INTO sqlar(name,mode,mtime,sz,data,writer,last_writer)
             VALUES(?1,?2,?3,?4,?5,?6,?6)
             ON CONFLICT(name) DO UPDATE SET data=excluded.data, sz=excluded.sz",
            params![rel, 0o120777u32, now_ns(), t.len() as i64, t, writer],
        );
        drop(conn);
        self.kinds.write().unwrap()
            .insert(rel.to_string(), Entry::Symlink { target: target.to_path_buf() });
    }

    pub fn set_whiteout(&self, rel: &str, writer: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO sqlar(name,mode,mtime,sz,data,writer,last_writer)
             VALUES(?1,?2,0,0,NULL,?3,?3)
             ON CONFLICT(name) DO UPDATE SET mode=excluded.mode, data=NULL, sz=0",
            params![rel, S_IFCHR, writer],
        );
        drop(conn);
        self.kinds.write().unwrap().insert(rel.to_string(), Entry::Whiteout);
    }

    /// Append one captured stdout/stderr write to the outputs table, attributed
    /// to the writing process (stream 0=stdout, 1=stderr).
    pub fn add_output(&self, stream: i32, pid: u32, content: &[u8]) {
        let writer = self.writer_for(pid);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO outputs(ts,process_id,stream,content) VALUES(?1,?2,?3,?4)",
            rusqlite::params![ts, writer, stream, content]);
    }

    /// Record one D9 brush-shell provenance frame: the exact command string
    /// plus the full parsed-structure JSON the embedded brush shell reported.
    /// Returns the inserted brushprov row id (0 on failure). `pipeline` is the
    /// 0-based execution ordinal. The caller marks this id as the box's current
    /// pipeline (set_current_pipeline) so subsequently-recorded brush-descendant
    /// processes are stamped with it.
    pub fn add_brushprov(&self, cmd: &str, record_json: &str, pipeline: i64,
                         spawn_ts: f64) -> i64 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO brushprov(ts,cmd,record,pipeline,spawn_ts) \
             VALUES(?1,?2,?3,?4,?5)",
            params![ts, cmd, record_json, pipeline, spawn_ts]);
        conn.last_insert_rowid()
    }

    /// Record a NESTED-shell provenance row (a recipe a `sh -c` the box
    /// spawned ran, EXECUTED by the brush-sh shim through embedded brush-core
    /// — there is no real-shell fallback). Same shape as add_brushprov but
    /// with nested=1, so a reader can distinguish it from a top-level
    /// pipeline. Process↔pipeline linkage IS available for nested rows whose
    /// pipeline has a literal `> file` target: control.rs feeds those targets
    /// into the same brush_links bucket as top-level rows, and
    /// finalize_brush_links stamps the writer with this row's id under the
    /// forest-ancestry guard (the nested-shim's descendants chain up through
    /// the box's brush --inner, so the guard accepts them).
    pub fn add_brushprov_nested(&self, cmd: &str, record_json: &str, pipeline: i64,
                                spawn_ts: f64) -> i64 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO brushprov(ts,cmd,record,pipeline,spawn_ts,nested) \
             VALUES(?1,?2,?3,?4,?5,1)",
            params![ts, cmd, record_json, pipeline, spawn_ts]);
        conn.last_insert_rowid()
    }

    /// Record one parsed n2/ninja build edge (Phase 1 embedded-ninja). `outs`
    /// and `ins` are serialized JSON arrays; `cmd` is the recipe command line
    /// (None for a phony edge). Returns the inserted row id (0 on failure).
    pub fn add_build_edge(&self, outs_json: &str, ins_json: &str,
                          cmd: Option<&str>) -> i64 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO build_edges(ts,outs,ins,cmd) VALUES(?1,?2,?3,?4)",
            params![ts, outs_json, ins_json, cmd]);
        conn.last_insert_rowid()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        );
    }

    /// True if this (live) box has its OWN entry for `rel` — i.e. it copied-up,
    /// wrote, deleted, or otherwise spoke for the path itself, so its merged
    /// view is self-contained and a parent copy-down must NOT override it. The
    /// in-RAM `kinds` mirror is authoritative for a live box (every write
    /// updates it in lockstep with the row).
    pub fn has_own(&self, rel: &str) -> bool {
        self.kinds.read().unwrap().contains_key(rel)
    }

    /// The box's ROOT process row (root=1): the `sarun -- cmd` runner itself, the
    /// top of this launch's process forest and the bubble-walk boundary. Provenance
    /// comes from the register message body (exe/cwd/argv); `host_pid` is the runner's
    /// REAL host pid (pidfd-derived, correct even for a nested runner) — its /proc
    /// start_time is the incarnation key so a writer bubbling up its PPid chain reaches
    /// THIS row (matching the Python Supervisor, which records the root with the host
    /// pid's real start_time, not 0). On RERUN a second launch adds ANOTHER root row
    /// and its subtree, keeping the forest connected across runs.
    pub fn root_process(&self, prov: &serde_json::Value, host_pid: i64) {
        let g = |k: &str| prov.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        // tgid: the real host pid when known (so /proc + the bubble chain agree),
        // else the runner's self-reported tgid/pid from prov.
        let tgid = if host_pid > 0 { host_pid as u32 }
                   else {
                       prov.get("tgid").and_then(|v| v.as_i64())
                           .or_else(|| prov.get("pid").and_then(|v| v.as_i64()))
                           .unwrap_or(0) as u32
                   };
        let ppid = prov.get("ppid").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
        // start_time identifies the incarnation; read the host pid's real start so the
        // (tgid,start) identity matches what writers see when they bubble up to it.
        let start = if tgid > 0 { Self::start_time_of(tgid) } else { 0 };
        let argv: Vec<String> = prov.get("argv").and_then(|v| v.as_array())
            .map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default();
        // -e env capture: the root's env. Prefer the env the runner sent in prov
        // (its full HOST env — correct even for a nested runner whose tgid is a
        // parent-namespace pid the engine can't /proc-read); else read the host
        // tgid's /proc/<tgid>/environ.
        let env_json = if self.env_capture() {
            prov.get("env").and_then(|e| e.as_object()).map(|m| {
                let bt: std::collections::BTreeMap<String, String> = m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect();
                serde_json::to_string(&bt).unwrap_or_default()
            }).or_else(|| if tgid > 0 { Self::read_environ_json(tgid) }
                          else { None })
        } else { None };
        self.record_proc(tgid, start, ppid, &g("exe"), &g("cwd"), &argv,
                         env_json, true);
    }

    /// Move the upper row old->new (reusing the blob — rowid is stable, so the
    /// pool file at blob_path(id,rowid) stays put). Drops any pre-existing new
    /// row first. Mirror updated to match. The caller decides whether to white
    /// out `old` afterwards (it does when a lower file shows through there).
    pub fn rename_row(&self, old: &str, new: &str) {
        let entry = self.kinds.read().unwrap().get(old).cloned();
        let Some(entry) = entry else { return };
        {
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [new]);
            let _ = conn.execute("UPDATE sqlar SET name=?2 WHERE name=?1",
                                 params![old, new]);
        }
        let mut k = self.kinds.write().unwrap();
        k.remove(old);
        k.insert(new.to_string(), entry);
    }

    /// Move a whole subtree old/ -> new/ in place (UPDATE name, rowids — and
    /// thus blob addresses — preserved). Used for directory rename.
    pub fn reparent(&self, old: &str, new: &str) {
        let op = format!("{old}/");
        let conn = self.conn.lock().unwrap();
        let names: Vec<String> = {
            let mut st = match conn.prepare(
                "SELECT name FROM sqlar WHERE name=?1 OR name LIKE ?2") {
                Ok(s) => s, Err(_) => return,
            };
            let like = format!("{op}%");
            let it = st.query_map(params![old, like], |r| r.get::<_, String>(0));
            match it { Ok(it) => it.flatten().collect(), Err(_) => return }
        };
        for name in &names {
            let nn = if name == old { new.to_string() }
                     else { format!("{new}/{}", &name[op.len()..]) };
            let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [&nn]);
            let _ = conn.execute("UPDATE sqlar SET name=?2 WHERE name=?1",
                                 params![name, nn]);
        }
        drop(conn);
        let mut k = self.kinds.write().unwrap();
        for name in names {
            let nn = if name == old { new.to_string() }
                     else { format!("{new}/{}", &name[op.len()..]) };
            if let Some(e) = k.remove(&name) {
                k.insert(nn, e);
            }
        }
    }

    /// Drop a row entirely (an upper-only file was unlinked: nothing to white
    /// out, the change simply un-happens). Removes the blob too.
    pub fn drop_row(&self, rel: &str) {
        let rowid = match self.kinds.write().unwrap().remove(rel) {
            Some(Entry::File { rowid, .. }) => Some(rowid),
            _ => None,
        };
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [rel]);
        drop(conn);
        if let Some(rid) = rowid {
            let _ = std::fs::remove_file(blob_path(self.id, rid));
        }
    }

    pub fn entry(&self, rel: &str) -> Option<Entry> {
        self.kinds.read().unwrap().get(rel).cloned()
    }

    /// Drop `rel` from the live RAM mirror only (the on-disk row/blob is handled
    /// by the caller). Used when a delete promotes into a parent whose lower has
    /// nothing to shadow, so the parent's own row is removed entirely.
    pub fn forget_kind(&self, rel: &str) {
        self.kinds.write().unwrap().remove(rel);
    }

    /// Direct overlay children of dir `rel`: (whiteout names, present names).
    pub fn children_of(&self, rel: &str) -> (Vec<String>, Vec<String>) {
        let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
        let mut white = vec![];
        let mut present = vec![];
        for (p, e) in self.kinds.read().unwrap().iter() {
            if !p.starts_with(&prefix) || p.len() == prefix.len() {
                continue;
            }
            let tail = &p[prefix.len()..];
            if tail.contains('/') {
                continue;
            }
            match e {
                Entry::Whiteout => white.push(tail.to_string()),
                _ => present.push(tail.to_string()),
            }
        }
        (white, present)
    }
}
