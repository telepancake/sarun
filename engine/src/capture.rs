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

/// One row of a box's RO-attachment list (§8). Serialized untagged so
/// the meta JSON stays backward/forward compatible: a bare number is a
/// Box row (the historical Vec<i64> format — old metas parse unchanged
/// and int-only lists serialize byte-identically), an object is an
/// external reference into a mirror store, served through the readout
/// trait instead of an imported copy (gimir/ATTACH-CONVERGENCE.md).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum RoAttachment {
    Box(i64),
    Ext(ExtRef),
}

/// A pinned external attachment: which store, and which immutable rev.
/// `rev` pins content, never position — git = full commit sha (frame
/// indices shift when update() prepends), wiki = head rev_id, ietf =
/// head draft rev. `name` is the display identity the UI shows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExtRef {
    pub kind: String,
    pub store: String,
    #[serde(rename = "ref")]
    pub refname: String,
    pub rev: String,
    #[serde(default)]
    pub prefix: String,
    pub name: String,
}

use crate::paths;

pub const S_IFCHR: u32 = 0o020000; // tombstone mode, matches the Python engine

/// Engine -> UI event queue, shared between the overlay (the file-change
/// producer) and every registered BoxState (the proc-table producer).
/// Items: (sid, rel, op) — the broadcaster in main.rs::serve drains this
/// and turns each entry into a JSON event on the subscribe stream
/// (type=overlay for file ops; type=process_added for op="process_added").
pub type EventQ = Arc<Mutex<VecDeque<(i64, String, &'static str)>>>;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT,
 sz INT, data BLOB, opaque INT DEFAULT 0, writer INT, last_writer INT);
-- review.rs's recent_changes/box_summary (the live \"recently changed\" panel
-- in the boxes view) run `ORDER BY mtime DESC LIMIT n` on every overlay event
-- while that panel is focused — i.e. once per file the box writes. Without an
-- index SQLite can't satisfy that ordering from an index scan and falls back
-- to a full table scan + sort every single call, so a build touching thousands
-- of files pays an O(n) sqlar scan per file written (O(n^2) over the build) —
-- this is what actually made a ~1min native build take ~20min in a box.
CREATE INDEX IF NOT EXISTS idx_sqlar_mtime ON sqlar(mtime);
CREATE TABLE IF NOT EXISTS provenance(path TEXT PRIMARY KEY, pid INT, ppid INT,
 exe TEXT, argv TEXT);
CREATE TABLE IF NOT EXISTS env(id INTEGER PRIMARY KEY AUTOINCREMENT,
 hash TEXT UNIQUE, env TEXT);
CREATE TABLE IF NOT EXISTS process(id INTEGER PRIMARY KEY AUTOINCREMENT,
 tgid INT, start INT, ppid INT, parent_id INT, exe TEXT, cwd TEXT, argv TEXT,
 env_id INT, root INT DEFAULT 0, brush_pipeline_id INT, UNIQUE(tgid, start));
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS outputs(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, process_id INT, stream INT, content BLOB, brush_pipeline_id INT);
-- Rust-engine extensions (additive; the Python readers ignore them):
CREATE TABLE IF NOT EXISTS xattr(name TEXT, key TEXT, value BLOB,
 PRIMARY KEY(name,key));
CREATE TABLE IF NOT EXISTS ownership(name TEXT PRIMARY KEY, uid INT, gid INT);
CREATE TABLE IF NOT EXISTS rdev(name TEXT PRIMARY KEY, dev INT);
CREATE TABLE IF NOT EXISTS atime(name TEXT PRIMARY KEY, ns INT);
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
--   uid: a process-global unique id the box assigns each pipeline. parent_uid:
--   the uid of the pipeline that ENCLOSED this one in-process (0 = a root), so a
--   reader can render the otherwise-flat log as a tree (make → recipe → sh -c →
--   …; xargs/subshells nest under their spawner). Both 0 for legacy boxes.
--   done_ts: wall-clock instant the pipeline's complete-command finished (0 ==
--   still running / never marked); exit_code: its status (-1 until done). The
--   [spawn_ts, done_ts] span is the pipeline's wall time; done_ts==0 on a LIVE
--   box means in-flight (useful for spotting a hang).
CREATE TABLE IF NOT EXISTS brushprov(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, cmd TEXT, record TEXT, pipeline INT, spawn_ts REAL, nested INT DEFAULT 0,
 uid INT DEFAULT 0, parent_uid INT DEFAULT 0, done_ts REAL DEFAULT 0,
 exit_code INT DEFAULT -1);
-- brushprov_id_for_uid / mark_brushprov_done key on uid; without this index
-- every pipeline event scans the whole (record-JSON-heavy) table, and a big
-- build's brushprov grows past 100k rows — the per-recipe uid scans were the
-- dominant engine cost of the kernel's `make headers` (~60k page reads per
-- header, O(n^2) over the build).
CREATE INDEX IF NOT EXISTS idx_brushprov_uid ON brushprov(uid);
-- Phase 1 embedded-ninja: one row per parsed n2/ninja build edge, captured when
-- the box's `ninja` (vendored n2 in-process) loads build.ninja — INCLUDING
-- up-to-date targets that never execute. `outs`/`ins` are JSON arrays of
-- target/dependency paths; `cmd` is the recipe command line (NULL for phony).
-- Execution columns are filled in BY THE BUILDER as edges run:
--   started_ts / ended_ts: REAL Unix epoch seconds; NULL when not yet run.
--   exit_code: 0 success / non-0 failure / NULL not-yet-run.
--   output_excerpt: first ~1KB of stderr+stdout from the recipe (best-effort,
--     trimmed at boundary; the full output lives in the outputs table).
CREATE TABLE IF NOT EXISTS build_edges(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, outs TEXT, ins TEXT, cmd TEXT,
 started_ts REAL, ended_ts REAL, exit_code INT, output_excerpt TEXT);
-- oaita `--api` proxy log: one row per request the engine forwarded on this
-- box's behalf. Routed AROUND the network proxy (the API call leaves through
-- the engine's HOST-namespace upstream connection, not the box's netns or
-- host-loopback), so it has its OWN log surface — the network pcap/MITM
-- views would not see it. `model` is what the request asked for (best-effort
-- — may be empty if the wire body omits it). `req`/`resp` are full bytes
-- (for streaming: a SSE-frame-concatenated reconstitution).
-- Makefile variable assignments, one row per (name, location, value, make
-- working dir) the box's embedded makes evaluated — the searchable record
-- behind the UI's Vars view (where did this variable get this value).
-- UNIQUE collapses the identical re-assignments every sub-make repeats.
-- edge_out / uid anchor the assignment to its execution context (the recipe
-- build edge's primary output / the enclosing pipeline's brushprov uid) so
-- the Vars view can cross-navigate to Build / Pipes.
-- rhs is the UNEXPANDED assignment text and refs the space-joined variable
-- names it dereferences — how the assignment looked and what fed it, so the
-- Vars view can walk the chain. Both capped at capture time (frugal).
-- flags is a compact what-kind-of-assignment tag: the make op (:= = +=
-- ?= !=) plus origin when notable (env cmd ovr auto), or sh / sh x for
-- shell rows (x = exported).
CREATE TABLE IF NOT EXISTS makevar(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, name TEXT, loc TEXT, value TEXT, make_dir TEXT,
 edge_out TEXT, uid INT, rhs TEXT, refs TEXT, flags TEXT,
 UNIQUE(name, loc, value, make_dir));
CREATE INDEX IF NOT EXISTS idx_makevar_name ON makevar(name);
CREATE TABLE IF NOT EXISTS api_log(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, method TEXT, path TEXT, model TEXT, status INT,
 stream INT DEFAULT 0, req BLOB, resp BLOB);
-- web capture (DESIGN-web.md W1): one row per HTTP(S) request/response the tap
-- MITM proxy teed on this box's behalf, addressed by URL. This is the CONTENT
-- record (full request/response bodies) that the pcapng/keylog flows are
-- the PACKET record of — two views of the same traffic. Opt-in per box (the
-- browser and the crawler enable it; ordinary boxes don't accumulate one).
-- resp_body holds the RAW upstream bytes (byte-identical, Content-Encoding
-- kept verbatim in resp_headers) so replay is exact and no decode is paid at
-- capture time; readers decode to identity on demand via the recorded
-- Content-Encoding (webcap::decode_body). Bodies over WEBCAP_BODY_MAX are
-- recorded header-only with a truncation marker. host is indexed for
-- all-captures-of-a-site; url for replay lookup. Newest-first per url =
-- gimir PageView chain (DESIGN-web.md W0).
CREATE TABLE IF NOT EXISTS webcap(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, method TEXT, url TEXT, host TEXT, status INT, mime TEXT,
 req_headers TEXT, resp_headers TEXT, req_body BLOB, resp_body BLOB,
 truncated INT DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_webcap_host ON webcap(host);
CREATE INDEX IF NOT EXISTS idx_webcap_url ON webcap(url);
-- sud TRACE stream (engine/DESIGN-sud.md step 2): the raw wire-format event
-- stream a sud box's tracer emitted (EXEC/ARGV/ENV/OPEN/CWD/STDOUT/STDERR/EXIT),
-- teed live to live/<id>/sud.trace and folded into the box's durable record by
-- SUD trace finalization. Single-row (one blob per box): set_sudtrace deletes then
-- inserts so a rerun overwrites. Decoded on demand by the `sudtrace` control
-- verb (crate::sudwire::Decoder). Absent for FUSE boxes — only sud boxes ever
-- populate it, which is what gates the UI's Trace chip.
CREATE TABLE IF NOT EXISTS sudtrace(content BLOB);
";

#[derive(Clone)]
pub enum Entry {
    File { rowid: i64, mode: u32 },
    /// `rebased`: backdrop-anchored (DEPOT-DESIGN.md §2) — this dir's
    /// recorded LOWER contributions (parent boxes) are erased, while the
    /// backdrop (host) still shows through. Distinct from `opaque`,
    /// which masks recorded lower AND backdrop children. Produced by
    /// rotation; stored as bit 1 of the `opaque` column.
    Dir { mode: u32, mtime_ns: i64, opaque: bool, rebased: bool },
    Symlink { target: PathBuf },
    Special { mode: u32, rdev: u64 },  // fifo / char / block device
    Whiteout,
    /// A hole (DEPOT-DESIGN.md §2): "this key is not occluded" — skip
    /// every recorded lower layer and resolve from the BACKDROP (host,
    /// or nothing under no_host_fallback), LIVE at access time. The
    /// artifact rotation leaves where the new parent-encoding contains
    /// changes that were never this layer's. Stored as a whiteout row
    /// (mode == S_IFCHR) with the backdrop-anchor bit set.
    Hole,
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
    // virtio-fs reports guest thread ids. PID 1 supplies the corresponding
    // guest thread-group identities over the appliance relation; these values
    // must never be looked up in the host's /proc namespace.
    guest_tgids: Mutex<HashMap<u32, u32>>,
    /// Live in-flight builtin activity (kati recipes / \$(shell) / parse
    /// phases), pushed by the box's watchdog as `box_activity` frames:
    /// (description, age seconds, received-at unix ts). Ephemeral — a UI
    /// inspection feed, not capture.
    pub activity: Mutex<Vec<(String, u64)>>,
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
    // True when the box was launched with --api: the inner runner serves
    // /run/sarun/api.sock inside the box and mux'es each call as FRAME_API_*
    // on the box channel; the FUSE overlay also substitutes a SAFE oaita.toml
    // (model only, no api_key, no base_url) over the box's view of the host
    // config path so a box can't read the host's real api_key by `cat`ing
    // the file. Mirrors is_brush.
    is_api: std::sync::atomic::AtomicBool,
    is_tap: std::sync::atomic::AtomicBool,
    // brush↔process link inputs: (brushprov row id, literal WRITE-redirect target
    // paths the pipeline opens for output). Collected as each FRAME_PROV arrives,
    // consumed at teardown (finalize_brush_links). The link is EXACT and race
    // free: a pipeline's output-redirect target file is written by exactly that
    // pipeline's process, so stamping that file's last_writer process row with the
    // pipeline id needs no timing/clock comparison at all.
    brush_links: Mutex<Vec<(i64, Vec<String>)>>,
    // RO attachments (DEPOT-DESIGN.md §8): the ordered list of layers
    // this box references READ-ONLY, conceptually between this box and
    // its parent in the lookup chain. Any mutation of a key an
    // attachment matches is EROFS — which is what guarantees the
    // captured layer is independent of the attachments (copy-up is the
    // only path lower content takes into the upper, and it is exactly
    // the rejected operation). Persisted in meta for rerun.
    ro_attachments: Mutex<Vec<RoAttachment>>,
    // The brushprov row id of the currently-executing pipeline (0 = none).
    // Set by record_brush_prov, cleared by brush_prov_done. Used by
    // add_output to stamp each output row with its pipeline.
    cur_brush_pipeline: std::sync::atomic::AtomicI64,
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
    pub fn set_is_api(&self, on: bool) {
        self.is_api.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn is_api(&self) -> bool {
        self.is_api.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_is_tap(&self, on: bool) {
        self.is_tap.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn is_tap(&self) -> bool {
        self.is_tap.load(std::sync::atomic::Ordering::Relaxed)
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

pub(crate) fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

impl BoxState {
    pub fn create(id: i64) -> rusqlite::Result<Self> {
        let db = paths::state_home().join(format!("{id}.sqlar"));
        let conn = Connection::open(&db)?;
        // synchronous=OFF is deliberate, not an oversight. This sqlar holds a
        // box's captured writes in escrow for review; the host is never touched
        // until an explicit apply. An OS crash/power loss can therefore only
        // lose or corrupt an in-progress, re-runnable box — never host data — so
        // crash-durability is not a requirement here. OFF avoids an fsync per
        // write on the hot capture path (a build copies up tens of thousands of
        // files); WAL/NORMAL would add fsync latency plus -wal/-shm side files
        // (extra peak disk, no longer a single file while live) to buy
        // durability this store does not need.
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF;")?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", 1)?;
        Ok(BoxState {
            id,
            conn: Mutex::new(conn),
            kinds: RwLock::new(HashMap::new()),
            event_sink: Mutex::new(None),
            proc_cache: Mutex::new(HashMap::new()),
            activity: Mutex::new(Vec::new()),
            proc_current: Mutex::new(HashMap::new()),
            guest_tgids: Mutex::new(HashMap::new()),
            roots: Mutex::new(std::collections::HashSet::new()),
            parent: std::sync::atomic::AtomicI64::new(0),
            env_capture: std::sync::atomic::AtomicBool::new(false),
            direct: std::sync::atomic::AtomicBool::new(false),
            readonly_parent: std::sync::atomic::AtomicBool::new(false),
            no_host_fallback: std::sync::atomic::AtomicBool::new(false),
            is_brush: std::sync::atomic::AtomicBool::new(false),
            brush_host_tgid: std::sync::atomic::AtomicU32::new(0),
            brush_links: Mutex::new(vec![]),
            cur_brush_pipeline: std::sync::atomic::AtomicI64::new(0),
            is_api: std::sync::atomic::AtomicBool::new(false),
            is_tap: std::sync::atomic::AtomicBool::new(false),
            ro_attachments: Mutex::new(vec![]),
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
        if let Ok(s) = conn.query_row(
            "SELECT value FROM meta WHERE key='ro_attachments'", [],
            |r| r.get::<_, String>(0))
        {
            if let Ok(rows) = serde_json::from_str::<Vec<RoAttachment>>(&s) {
                *self.ro_attachments.lock().unwrap() = rows;
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
                    let entry = Self::entry_from_row(&conn, &name, mode, sz, data);
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
        // Identify by the PROCESS (tgid), not the writing thread: FUSE reports the
        // writing thread's TID, and an in-process box runs many pipelines on
        // spawned worker threads (recipes / $(shell)) that share one tgid but each
        // have their OWN thread start_time. Keying identity on the tid's start_time
        // would mint a near-duplicate row per thread for a single process. Read
        // start/ppid/exe/cwd/argv from the tgid so all threads collapse to one row
        // (and matches how the ROOT row's start is read — start_time_of(tgid)).
        let ident = if tgid != 0 { tgid } else { pid };
        let (ppid, start) = Self::parse_stat(ident);
        let proc_ = |f: &str| format!("/proc/{ident}/{f}");
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
        // A vanished pid reads back identity-less: tgid_of falls back to the raw
        // pid, parse_stat yields start=0, and exe/argv are empty. NEVER mint a
        // blank (tgid,0) row from that — it would be filed as a distinct
        // incarnation from the process's real (tgid,real_start) record and win
        // nothing. Reuse this tgid's last LIVE incarnation if we recorded one
        // (e.g. the write handler characterized the writer while it was alive);
        // only if we never saw it alive at all do we fall back to the bare tgid.
        if start == 0 && exe.is_empty() && argv.is_empty() {
            if let Some((_, rid)) = self.proc_current.lock().unwrap().get(&tgid) {
                return *rid;
            }
            return tgid as i64;
        }
        // -e env capture: read /proc/<pid>/environ now (before any lock, before the
        // process can exit). The pid (thread id) shares the tgid's environ.
        let env_json = if self.env_capture() { Self::read_environ_json(pid) }
                       else { None };
        self.record_proc(tgid, start, ppid, &exe, &cwd, &argv, env_json, false)
            .unwrap_or(tgid as i64)
    }

    /// Resolve a virtio-fs request actor exclusively through guest identities
    /// supplied by the paired PID 1. Numeric guest PIDs are never meaningful
    /// host PIDs; falling back to host `/proc` here can attribute a guest write
    /// to an unrelated host process with the same number.
    pub fn guest_writer_for(&self, pid: u32) -> i64 {
        let tgid = self.guest_tgids.lock().unwrap().get(&pid).copied().unwrap_or(pid);
        self.proc_current.lock().unwrap().get(&tgid)
            .map(|(_, row)| *row).unwrap_or(0)
    }

    /// Record one process/thread observation made inside the appliance's own
    /// procfs. `event.pid` maps the FUSE request thread id to the provenance's
    /// thread group. Parent linkage is resolved only against other guest rows;
    /// a guest process whose parent has not appeared yet attaches to this
    /// launch's stored host-runner root rather than escaping into host /proc.
    pub fn record_guest_process(
        &self,
        event: &crate::generated_wire::GuestProcessEvent,
    ) -> Result<i64, String> {
        let provenance = &event.provenance;
        let start = i64::try_from(event.start)
            .map_err(|_| "guest process start identity exceeds i64")?;
        let text = |value: &[u8], field: &str| {
            std::str::from_utf8(value).map(str::to_owned)
                .map_err(|_| format!("guest process {field} is not UTF-8"))
        };
        let exe = text(provenance.executable.as_slice(), "executable")?;
        let cwd = text(provenance.cwd.as_slice(), "cwd")?;
        let argv = provenance.argv.as_slice().iter()
            .map(|value| text(value.as_slice(), "argument"))
            .collect::<Result<Vec<_>, _>>()?;
        let argv_json = serde_json::to_string(&argv).map_err(|error| error.to_string())?;
        self.guest_tgids.lock().unwrap().insert(event.pid, provenance.tgid);

        if let Some(row) = self.proc_cache.lock().unwrap()
            .get(&(provenance.tgid, start)).copied()
        {
            self.proc_current.lock().unwrap()
                .insert(provenance.tgid, (start, row));
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute(
                "UPDATE process SET ppid=?1,exe=?2,cwd=?3,argv=?4 WHERE id=?5",
                params![provenance.ppid, exe, cwd, argv_json, row],
            );
            return Ok(row);
        }

        let parent_id = u32::try_from(provenance.ppid).ok()
            .filter(|parent| *parent > 1)
            .and_then(|parent| self.proc_current.lock().unwrap()
                .get(&parent).map(|(_, row)| *row));
        let conn = self.conn.lock().unwrap();
        let root = conn.query_row(
            "SELECT id FROM process WHERE root<>0 ORDER BY id DESC LIMIT 1",
            [], |row| row.get::<_, i64>(0),
        ).ok();
        let parent_id = parent_id.or(root);
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO process(tgid,start,ppid,parent_id,exe,cwd,argv,root) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
            params![provenance.tgid, start, provenance.ppid, parent_id,
                    exe, cwd, argv_json],
        ).map_err(|error| error.to_string())?;
        let row = conn.query_row(
            "SELECT id FROM process WHERE tgid=?1 AND start=?2",
            params![provenance.tgid, start], |row| row.get::<_, i64>(0),
        ).map_err(|error| error.to_string())?;
        drop(conn);
        self.proc_cache.lock().unwrap().insert((provenance.tgid, start), row);
        self.proc_current.lock().unwrap().insert(provenance.tgid, (start, row));
        if inserted > 0 {
            self.push_event("", "process_added");
        }
        Ok(row)
    }

    /// Mint (or refresh) a process row purely from trace-EVENT data — for
    /// pids that are already gone by the time their EXEC event reaches the
    /// engine (an `as`/`echo`-sized tool lives shorter than the pipe
    /// latency, so the /proc snapshot in writer_for finds nothing and the
    /// row silently never exists). Identity start = the event timestamp:
    /// unique per exec, and no /proc is left to disagree with.
    pub fn record_proc_event(&self, tgid: u32, ppid: u32, ts_ns: i64,
                             exe: &str, cwd: &str, argv: &[String]) -> Option<i64> {
        // If a live snapshot already recorded this tgid, refresh its image
        // instead (same semantics as exec_refresh's in-place update).
        if let Some((start, rid)) = self.proc_current.lock().unwrap()
                                        .get(&tgid).copied() {
            let _ = start;
            let argv_json = serde_json::to_string(&argv).unwrap_or_default();
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute(
                "UPDATE process SET exe=?1, argv=?2 WHERE id=?3 AND exe<>?1",
                params![exe, argv_json, rid]);
            return Some(rid);
        }
        self.record_proc(tgid, ts_ns, ppid, exe, cwd, argv, None, false)
    }

    /// EXEC-event refresh: an in-place execve keeps the pid AND its kernel
    /// start time, so the (tgid,start) incarnation key — and therefore the
    /// cached process row — CANNOT tell the new image from the old. Vendor
    /// toolchains hit this constantly: `gcc` is a shell wrapper ending in
    /// `exec real-gcc "$@"`, so every tool invocation re-execs in place and
    /// the snapshot taken at the FIRST exec (the wrapper, or the shell shim)
    /// is what the process table shows forever — the actual compilers never
    /// appear. On an EXEC event, re-read /proc and, when the row's image is
    /// stale, update it in place (same row id, so output/edge attribution by
    /// row id is unaffected).
    /// `event_exe` is the EXEC event's own exe blob — the program the BOX
    /// sees. For a wrapper-launched tool (sud32 userspace-loading a 32-bit
    /// binary) /proc/<pid>/exe is the WRAPPER for the process's whole life,
    /// so /proc would file every 32-bit compiler as "sud32"; the event's
    /// exe wins whenever it is non-empty. (argv needs no such override —
    /// the loader rewrites the visible cmdline to the real program's.)
    /// Returns None when the process was already gone from /proc — the
    /// caller can then mint the row from the trace event's own data
    /// (record_proc_event).
    pub fn exec_refresh(&self, pid: u32, event_exe: &str) -> Option<i64> {
        let (tgid, start, _ppid, exe, cwd, argv) = Self::read_prov(pid);
        if start == 0 && exe.is_empty() && argv.is_empty() {
            // Process already gone — nothing fresher to record.
            return None;
        }
        let exe = if event_exe.is_empty() { exe }
                  else { event_exe.to_string() };
        let cached = self.proc_cache.lock().unwrap().get(&(tgid, start)).copied();
        let Some(rid) = cached else {
            // First sighting of this incarnation: normal record path,
            // then correct the /proc-snapshotted exe to the event's.
            let rid = self.writer_for(pid);
            if !event_exe.is_empty() {
                let conn = self.conn.lock().unwrap();
                let _ = conn.execute(
                    "UPDATE process SET exe=?1 WHERE id=?2 AND exe<>?1",
                    params![exe, rid]);
            }
            return Some(rid);
        };
        let argv_json = serde_json::to_string(&argv).unwrap_or_default();
        let conn = self.conn.lock().unwrap();
        let stale: bool = conn
            .query_row("SELECT exe<>?1 OR argv<>?2 FROM process WHERE id=?3",
                       params![exe, argv_json, rid], |r| r.get(0))
            .unwrap_or(false);
        if stale {
            let _ = conn.execute(
                "UPDATE process SET exe=?1, argv=?2, cwd=?3 WHERE id=?4",
                params![exe, argv_json, cwd, rid]);
            drop(conn);
            self.push_event("", "process_added");
        }
        Some(rid)
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
        // sarun: skip phantom ancestor rows. resolve_parent's bubble-walk
        // calls read_prov(ppid) for every ancestor it has to materialize;
        // when that pid has already exited (typical with brush's many
        // short-lived fork-exec-exits), read_link("/proc/<pid>/exe")
        // silently yields "" and argv is empty. Recording the row leaks
        // a useless entry into the process table with no exe path that
        // the UI later renders as "exe ?". Such rows never anchor any
        // FUSE op; only the bubble-walk ever produces them.
        if exe.is_empty() && argv.is_empty() {
            return None;
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













    /// Build the in-RAM `kinds` Entry for one sqlar row. The single mapping used
    /// both by the initial mirror load and by `reload_entry`.
    pub(crate) fn entry_from_row(conn: &Connection, name: &str, mode: u32, sz: i64,
                      data: Option<Vec<u8>>) -> Entry {
        let ft = mode & 0o170000;
        // The `opaque` column is a bitfield: bit 0 = opaque-dir, bit 1 =
        // backdrop-anchored (§2 anchor axis). Python readers treat any
        // non-zero value as opaque-ish; additive.
        let flags: i64 = conn.query_row(
            "SELECT opaque FROM sqlar WHERE name=?1", [name],
            |r| r.get(0)).unwrap_or(0);
        if mode == S_IFCHR {
            if flags & 2 != 0 { Entry::Hole } else { Entry::Whiteout }
        } else if ft == 0o120000 {
            let bytes = data.unwrap_or_default();
            let t = String::from_utf8_lossy(&bytes).into_owned();
            let _ = sz;
            Entry::Symlink { target: PathBuf::from(t) }
        } else if ft == 0o040000 {
            Entry::Dir { mode, mtime_ns: 0, opaque: flags & 1 != 0,
                         rebased: flags & 2 != 0 }
        } else if ft == 0o010000 || ft == 0o060000 {
            Entry::Special { mode, rdev: 0 }
        } else {
            let rowid: i64 = conn.query_row(
                "SELECT rowid FROM sqlar WHERE name=?1", [name],
                |r| r.get(0)).unwrap_or(0);
            Entry::File { rowid, mode }
        }
    }



    /// Only the box-id rows — what the hydrate walk and the id-based
    /// chain hops consume. External rows are invisible here.
    pub fn ro_attachment_box_ids(&self) -> Vec<i64> {
        self.ro_attachments.lock().unwrap().iter()
            .filter_map(|r| match r { RoAttachment::Box(id) => Some(*id),
                                      RoAttachment::Ext(_) => None })
            .collect()
    }

    /// The full ordered list, box and external rows alike.
    pub fn ro_attachment_list(&self) -> Vec<RoAttachment> {
        self.ro_attachments.lock().unwrap().clone()
    }

    /// Replace the RO attachment list (ordered, topmost first) and
    /// persist it in meta so a rerun reopens with the same view.
    pub fn set_ro_attachments(&self, rows: Vec<RoAttachment>) {
        let json = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into());
        self.set_meta("ro_attachments", &json);
        *self.ro_attachments.lock().unwrap() = rows;
    }

    pub fn set_cur_brush_pipeline(&self, id: i64) {
        self.cur_brush_pipeline.store(id, std::sync::atomic::Ordering::Relaxed);
    }

    /// Append one captured stdout/stderr write to the outputs table, attributed
    /// to the writing process (stream 0=stdout, 1=stderr).
    pub fn add_output(&self, stream: i32, pid: u32, content: &[u8]) {
        let writer = self.writer_for(pid);
        self.add_output_for_writer(stream, writer, content);
    }

    pub fn add_guest_output(&self, stream: i32, pid: u32, content: &[u8]) {
        let writer = self.guest_writer_for(pid);
        self.add_output_for_writer(stream, writer, content);
    }

    fn add_output_for_writer(&self, stream: i32, writer: i64, content: &[u8]) {
        let pipeline = self.cur_brush_pipeline.load(std::sync::atomic::Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO outputs(ts,process_id,stream,content,brush_pipeline_id) \
             VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![ts, writer, stream, content,
                              if pipeline > 0 { Some(pipeline) } else { None::<i64> }]);
    }

    /// Retroactively fix brush_pipeline_id on stderr output rows captured by the
    /// FUSE handler during a $(shell) recipe. The FUSE path wrote these rows with
    /// whatever cur_brush_pipeline was set at the time (racy); this stamps them
    /// with the correct pipeline. Scoped by timestamp (recipe start → now) and
    /// stream=1 (stderr).
    pub fn fixup_output_attribution(&self, start_ts: f64, pipeline_id: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE outputs SET brush_pipeline_id=?1 \
             WHERE ts >= ?2 AND stream = 1 AND brush_pipeline_id IS NOT ?1",
            rusqlite::params![pipeline_id, start_ts]);
    }

    /// Look up the brushprov row id for a given uid. Returns 0 if not found.
    pub fn brushprov_id_for_uid(&self, uid: i64) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id FROM brushprov WHERE uid=?1",
            params![uid],
            |row| row.get(0),
        ).unwrap_or(0)
    }

    /// Record one D9 brush-shell provenance frame: the exact command string
    /// plus the full parsed-structure JSON the embedded brush shell reported.
    /// Returns the inserted brushprov row id (0 on failure). `pipeline` is the
    /// 0-based execution ordinal. The caller marks this id as the box's current
    /// pipeline (set_current_pipeline) so subsequently-recorded brush-descendant
    /// processes are stamped with it.
    pub fn add_brushprov(&self, cmd: &str, record_json: &str, pipeline: i64,
                         spawn_ts: f64, uid: i64, parent_uid: i64) -> i64 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO brushprov(ts,cmd,record,pipeline,spawn_ts,uid,parent_uid) \
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![ts, cmd, record_json, pipeline, spawn_ts, uid, parent_uid]);
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
                                spawn_ts: f64, uid: i64, parent_uid: i64) -> i64 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO brushprov(ts,cmd,record,pipeline,spawn_ts,nested,uid,parent_uid) \
             VALUES(?1,?2,?3,?4,?5,1,?6,?7)",
            params![ts, cmd, record_json, pipeline, spawn_ts, uid, parent_uid]);
        conn.last_insert_rowid()
    }

    /// D9 pipeline completion: stamp done_ts + exit_code on the brushprov rows
    /// with these uids (the pipelines of one just-finished complete-command).
    /// `uid` is the box-assigned per-pipeline id; matching is scoped to this box.
    pub fn mark_brushprov_done(&self, uids: &[i64], code: i64, done_ts: f64) {
        if uids.is_empty() { return; }
        let conn = self.conn.lock().unwrap();
        for uid in uids {
            let _ = conn.execute(
                "UPDATE brushprov SET done_ts=?1, exit_code=?2 WHERE uid=?3 AND uid!=0",
                params![done_ts, code, uid]);
        }
    }

    /// Record one oaita-proxy API call into this box's `api_log` table.
    /// `ts` is the wall-clock UNIX seconds when the proxy finished handling
    /// the call. `req`/`resp` are the FULL bytes (response is a SSE-frames
    /// reconstitution when `stream` is true). Best-effort: a stale connection
    /// drops the row silently.
    pub fn add_api_log(&self, ts: f64, method: &str, path: &str,
                       model: &str, status: i32, req: &[u8], resp: &[u8],
                       stream: bool) -> i64 {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO api_log(ts,method,path,model,status,stream,req,resp) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![ts, method, path, model, status,
                    if stream { 1 } else { 0 }, req, resp]);
        conn.last_insert_rowid()
    }

    /// Record one web capture into this box's `webcap` table (DESIGN-web.md
    /// W1). `ts` is wall-clock UNIX seconds. Bodies are identity-encoded
    /// (already decompressed by the caller); `truncated` marks a body that
    /// exceeded the capture cap and was stored header-only. Best-effort: a
    /// stale connection drops the row silently, exactly like `add_api_log`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_web_capture(&self, ts: f64, method: &str, url: &str, host: &str,
                           status: i32, mime: &str, req_headers: &str,
                           resp_headers: &str, req_body: &[u8],
                           resp_body: &[u8], truncated: bool) -> i64 {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO webcap(ts,method,url,host,status,mime,\
             req_headers,resp_headers,req_body,resp_body,truncated) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![ts, method, url, host, status, mime,
                    req_headers, resp_headers, req_body, resp_body,
                    if truncated { 1 } else { 0 }]);
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

    /// Record a batch of makefile variable assignments (make_vars frame).
    /// INSERT OR IGNORE: the unique key collapses identical repeats across
    /// sub-makes, keeping the table proportional to distinct assignments.
    pub fn add_makevars(&self, rows: &[crate::control::MakeVarRow]) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let conn = self.conn.lock().unwrap();
        for r in rows {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO makevar\
                 (ts,name,loc,value,make_dir,edge_out,uid,rhs,refs,flags) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![ts, r.name, r.loc, r.value, r.make_dir, r.edge_out,
                        r.uid, r.rhs, r.refs, r.flags]);
        }
    }

    /// Mark a build edge as STARTED running. The edge is identified by either
    /// its primary output `out` (kati: == outs[0]) or its exact recipe `cmd`
    /// (n2: == the stored cmdline). The FIRST matching edge that hasn't started
    /// is stamped — outputs/cmdlines are effectively unique per edge, so this
    /// resolves the one running edge. Best-effort (an unmatched key is a no-op).
    pub fn mark_build_edge_started(&self, out: Option<&str>, cmd: Option<&str>, ts: f64) {
        let conn = self.conn.lock().unwrap();
        if let Some(out) = out {
            let _ = conn.execute(
                "UPDATE build_edges SET started_ts=?1 WHERE id=(\
                   SELECT id FROM build_edges \
                   WHERE json_extract(outs,'$[0]')=?2 AND started_ts IS NULL \
                   ORDER BY id LIMIT 1)",
                params![ts, out]);
        } else if let Some(cmd) = cmd {
            let _ = conn.execute(
                "UPDATE build_edges SET started_ts=?1 WHERE id=(\
                   SELECT id FROM build_edges \
                   WHERE cmd=?2 AND started_ts IS NULL \
                   ORDER BY id LIMIT 1)",
                params![ts, cmd]);
        }
    }

    /// Mark a build edge as FINISHED (the first started-but-not-ended edge with
    /// this output/cmd key), stamping `ended_ts` + `exit_code`. Best-effort.
    pub fn mark_build_edge_done(&self, out: Option<&str>, cmd: Option<&str>,
                                code: i64, ts: f64, excerpt: Option<&str>) {
        let conn = self.conn.lock().unwrap();
        if let Some(out) = out {
            let _ = conn.execute(
                "UPDATE build_edges SET ended_ts=?1, exit_code=?2, \
                        output_excerpt=?4 WHERE id=(\
                   SELECT id FROM build_edges \
                   WHERE json_extract(outs,'$[0]')=?3 \
                     AND started_ts IS NOT NULL AND ended_ts IS NULL \
                   ORDER BY id LIMIT 1)",
                params![ts, code, out, excerpt]);
        } else if let Some(cmd) = cmd {
            let _ = conn.execute(
                "UPDATE build_edges SET ended_ts=?1, exit_code=?2, \
                        output_excerpt=?4 WHERE id=(\
                   SELECT id FROM build_edges \
                   WHERE cmd=?3 \
                     AND started_ts IS NOT NULL AND ended_ts IS NULL \
                   ORDER BY id LIMIT 1)",
                params![ts, code, cmd, excerpt]);
        }
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        );
    }

    /// Store the box's raw sud TRACE stream (the bytes teed to
    /// live/<id>/sud.trace) as the single-row `sudtrace` blob. DELETE + INSERT
    /// so a rerun of the box overwrites the prior run's trace rather than
    /// accumulating rows. Empty input still writes an (empty) row — callers
    /// only invoke this when a trace file existed.
    pub fn set_sudtrace(&self, content: &[u8]) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM sudtrace", []);
        let _ = conn.execute("INSERT INTO sudtrace(content) VALUES(?1)",
                             params![content]);
    }

    /// The box's stored sud TRACE stream, or None if this box never captured
    /// one (every FUSE box, and a sud box swept before this table existed).
    pub fn get_sudtrace(&self) -> Option<Vec<u8>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT content FROM sudtrace LIMIT 1", [],
                       |r| r.get::<_, Option<Vec<u8>>>(0)).ok().flatten()
    }

    /// Read one `meta` row by key. None if absent.
    pub fn get_meta(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM meta WHERE key=?1", [key],
                       |r| r.get::<_, String>(0)).ok()
    }

    /// The box's ROOT process row (root=1): the `sarun -- cmd` runner itself, the
    /// top of this launch's process forest and the bubble-walk boundary. Provenance
    /// comes from the register message body (exe/cwd/argv); `host_pid` is the runner's
    /// REAL host pid (pidfd-derived, correct even for a nested runner) — its /proc
    /// start_time is the incarnation key so a writer bubbling up its PPid chain reaches
    /// THIS row (matching the Python Supervisor, which records the root with the host
    /// pid's real start_time, not 0). On RERUN a second launch adds ANOTHER root row
    /// and its subtree, keeping the forest connected across runs.
    pub fn root_process(
        &self,
        provenance: &crate::generated_wire::ProcessProvenance,
        host_pid: i64,
    ) -> Result<(), String> {
        let text = |value: &[u8], field: &str| std::str::from_utf8(value)
            .map(str::to_owned).map_err(|_| format!("root process {field} is not UTF-8"));
        // tgid: the real host pid when known (so /proc + the bubble chain agree),
        // else the runner's self-reported tgid/pid from prov.
        let tgid = if host_pid > 0 {
            u32::try_from(host_pid).map_err(|_| "root host pid exceeds u32")?
        } else {
            provenance.tgid
        };
        let ppid = u32::try_from(provenance.ppid).map_err(|_| "negative root parent pid")?;
        // start_time identifies the incarnation; read the host pid's real start so the
        // (tgid,start) identity matches what writers see when they bubble up to it.
        let start = if tgid > 0 { Self::start_time_of(tgid) } else { 0 };
        let argv = provenance.argv.as_slice().iter().map(|value|
            text(value.as_slice(), "argument")).collect::<Result<Vec<_>, _>>()?;
        if argv.is_empty() {
            return Err("resolved root process has an empty argv".into());
        }
        // -e env capture: the root's env. Prefer the env the runner sent in prov
        // (its full HOST env — correct even for a nested runner whose tgid is a
        // parent-namespace pid the engine can't /proc-read); else read the host
        // tgid's /proc/<tgid>/environ.
        let env_json = if self.env_capture() {
            provenance.environment.as_ref().map(|environment| {
                let values = environment.as_map().iter().map(|(key, value)| Ok((
                    text(key.as_slice(), "environment key")?,
                    text(value.as_slice(), "environment value")?,
                ))).collect::<Result<std::collections::BTreeMap<_, _>, String>>()?;
                serde_json::to_string(&values).map_err(|error| error.to_string())
            }).transpose()?.or_else(|| if tgid > 0 { Self::read_environ_json(tgid) }
                                     else { None })
        } else { None };
        self.record_proc(tgid, start, ppid,
                         &text(provenance.executable.as_slice(), "executable")?,
                         &text(provenance.cwd.as_slice(), "cwd")?, &argv,
                         env_json, true);
        Ok(())
    }





}

#[cfg(test)]
mod ro_attachment_tests {
    use super::*;

    // The untagged serde is the compat contract: historical Vec<i64>
    // metas parse, and int-only lists serialize byte-identically so a
    // box that never gains an external attachment never changes format.
    #[test]
    fn old_format_round_trips_byte_identical() {
        let rows: Vec<RoAttachment> = serde_json::from_str("[3,7]").unwrap();
        assert!(matches!(rows[..], [RoAttachment::Box(3), RoAttachment::Box(7)]));
        assert_eq!(serde_json::to_string(&rows).unwrap(), "[3,7]");
    }

    #[test]
    fn mixed_list_round_trips() {
        let j = r#"[7,{"kind":"git","store":"/m/s","ref":"main","rev":"abc","prefix":"sdk","name":"git:x/main@abc"}]"#;
        let rows: Vec<RoAttachment> = serde_json::from_str(j).unwrap();
        let RoAttachment::Ext(e) = &rows[1] else { panic!("ext row") };
        assert_eq!((e.kind.as_str(), e.refname.as_str(), e.rev.as_str()),
                   ("git", "main", "abc"));
        let back: Vec<RoAttachment> =
            serde_json::from_str(&serde_json::to_string(&rows).unwrap()).unwrap();
        assert!(matches!(back[0], RoAttachment::Box(7)));
        assert!(matches!(&back[1], RoAttachment::Ext(e2) if e2.prefix == "sdk"));
    }

    // Missing prefix (older ext rows or hand-written) defaults empty.
    #[test]
    fn ext_prefix_defaults_empty() {
        let j = r#"[{"kind":"wiki","store":"/w","ref":"12","rev":"99","name":"wiki:12@99"}]"#;
        let rows: Vec<RoAttachment> = serde_json::from_str(j).unwrap();
        assert!(matches!(&rows[0], RoAttachment::Ext(e) if e.prefix.is_empty()));
    }
}
