// Per-box capture state — writes the SAME on-disk layout as the Python engine
// (<box_id>.sqlar with its schema; file bytes as pool blobs at
// live/blob/<box_id>/<rowid%1024:03x>/<rowid>), so the Python readers
// (sqlar_list/sqlar_content/process_list, the review UI) read Rust-captured
// boxes unmodified. Per DESIGN.md D4 file rows are ALWAYS data-NULL with the
// bytes in the blob (no inline tier, no consolidate phase); symlink targets are
// stored raw in the row (sz == len marks "not deflated", which the Python
// reader already handles).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::RwLock;

use rusqlite::Connection;
use rusqlite::params;

use crate::paths;

pub const S_IFCHR: u32 = 0o020000; // tombstone mode, matches the Python engine

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT,
 sz INT, data BLOB, opaque INT DEFAULT 0, writer INT, last_writer INT);
CREATE TABLE IF NOT EXISTS provenance(path TEXT PRIMARY KEY, pid INT, ppid INT,
 exe TEXT, argv TEXT);
CREATE TABLE IF NOT EXISTS env(id INTEGER PRIMARY KEY AUTOINCREMENT,
 hash TEXT UNIQUE, env TEXT);
CREATE TABLE IF NOT EXISTS process(id INTEGER PRIMARY KEY AUTOINCREMENT,
 tgid INT, start INT, ppid INT, parent_id INT, exe TEXT, cwd TEXT, argv TEXT,
 env_id INT, root INT DEFAULT 0, UNIQUE(tgid, start));
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS outputs(id INTEGER PRIMARY KEY AUTOINCREMENT,
 ts REAL, process_id INT, stream INT, content BLOB);
-- Rust-engine extensions (additive; the Python readers ignore them):
CREATE TABLE IF NOT EXISTS xattr(name TEXT, key TEXT, value BLOB,
 PRIMARY KEY(name,key));
CREATE TABLE IF NOT EXISTS ownership(name TEXT PRIMARY KEY, uid INT, gid INT);
CREATE TABLE IF NOT EXISTS rdev(name TEXT PRIMARY KEY, dev INT);
";

#[derive(Clone)]
pub enum Entry {
    File { rowid: i64, mode: u32 },
    Dir { mode: u32, mtime_ns: i64 },
    Symlink { target: PathBuf },
    Special { mode: u32, rdev: u64 },  // fifo / char / block device
    Whiteout,
}

pub struct BoxState {
    pub id: i64,
    pub conn: Mutex<Connection>,
    pub kinds: RwLock<HashMap<String, Entry>>,
    procs: Mutex<HashMap<u32, i64>>, // tgid -> process row id
    parent: std::sync::atomic::AtomicI64, // 0 = top-level; else parent box_id
    // -e env capture: record each writer's full environment (deduped in `env`).
    env_capture: std::sync::atomic::AtomicBool,
    // -d direct: the box has NO overlay — every write goes straight to the real
    // host file, uncaptured (mirrors Python's whole-box passthrough=direct).
    direct: std::sync::atomic::AtomicBool,
}

impl BoxState {
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
            procs: Mutex::new(HashMap::new()),
            parent: std::sync::atomic::AtomicI64::new(0),
            env_capture: std::sync::atomic::AtomicBool::new(false),
            direct: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Repopulate the in-RAM `kinds` mirror + the tgid->row cache from an
    /// existing on-disk sqlar. Used on RERUN: a `run NAME` into an existing box
    /// reopens its db, so the prior run's writes must show through and previously
    /// recorded processes keep their row ids (so a new root is an ADDITIONAL
    /// row, not a dedup). Mirrors the Python Index._load_mirror.
    pub fn load_mirror(&self) {
        let conn = self.conn.lock().unwrap();
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
                        Entry::Dir { mode, mtime_ns: 0 }
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
        // Seed the tgid->row cache so already-recorded writers dedup correctly.
        let mut procs = self.procs.lock().unwrap();
        if let Ok(mut st) = conn.prepare(
            "SELECT tgid,id FROM process WHERE start=0") {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)?))
            }) {
                for (tgid, rid) in rows.flatten() { procs.insert(tgid, rid); }
            }
        }
    }

    /// The process-table row for `pid`, recorded on first sight (exe/argv/cwd
    /// from /proc — per-write attribution, see D5).
    pub fn writer_for(&self, pid: u32) -> i64 {
        if let Some(id) = self.procs.lock().unwrap().get(&pid) {
            return *id;
        }
        let proc_ = |f: &str| format!("/proc/{pid}/{f}");
        let exe = std::fs::read_link(proc_("exe"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let cwd = std::fs::read_link(proc_("cwd"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let argv: Vec<String> = std::fs::read(proc_("cmdline"))
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        // -e env capture: record this writer's environment (deduped) so the
        // process row links to it via env_id. Read /proc/<pid>/environ now,
        // before the lock (and before the process can exit).
        let env_json = if self.env_capture() { Self::read_environ_json(pid) }
                       else { None };
        let conn = self.conn.lock().unwrap();
        let eid: Option<i64> = env_json
            .and_then(|j| Self::ensure_env(&conn, &j));
        let _ = conn.execute(
            "INSERT OR IGNORE INTO process(tgid,start,ppid,parent_id,exe,cwd,argv,env_id,root)
             VALUES(?1,0,0,NULL,?2,?3,?4,?5,0)",
            params![pid, exe, cwd, serde_json::to_string(&argv).unwrap_or_default(), eid],
        );
        let rowid: i64 = conn
            .query_row("SELECT id FROM process WHERE tgid=?1 AND start=0", [pid],
                       |r| r.get(0))
            .unwrap_or(0);
        drop(conn);
        self.procs.lock().unwrap().insert(pid, rowid);
        rowid
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
        self.kinds.write().unwrap()
            .insert(rel.to_string(), Entry::Dir { mode: m, mtime_ns: now_ns() });
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

    /// The box's ROOT process row (root=1): the runner itself, provenance from
    /// the register message body (tgid/exe/cwd/argv).
    pub fn root_process(&self, prov: &serde_json::Value) {
        let g = |k: &str| prov.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let tgid = prov.get("tgid").and_then(|v| v.as_i64())
            .or_else(|| prov.get("pid").and_then(|v| v.as_i64())).unwrap_or(0);
        let ppid = prov.get("ppid").and_then(|v| v.as_i64()).unwrap_or(0);
        let argv = prov.get("argv").cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
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
            }).or_else(|| if tgid > 0 { Self::read_environ_json(tgid as u32) }
                          else { None })
        } else { None };
        let conn = self.conn.lock().unwrap();
        let eid: Option<i64> = env_json.and_then(|j| Self::ensure_env(&conn, &j));
        let _ = conn.execute(
            "INSERT OR IGNORE INTO process(tgid,start,ppid,parent_id,exe,cwd,argv,env_id,root)
             VALUES(?1,0,?2,NULL,?3,?4,?5,?6,1)",
            params![tgid, ppid, g("exe"), g("cwd"), argv.to_string(), eid],
        );
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
