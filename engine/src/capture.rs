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
";

#[derive(Clone)]
pub enum Entry {
    File { rowid: i64, mode: u32 },
    Dir { mode: u32, mtime_ns: i64 },
    Symlink { target: PathBuf },
    Whiteout,
}

pub struct BoxState {
    pub id: i64,
    pub conn: Mutex<Connection>,
    pub kinds: RwLock<HashMap<String, Entry>>,
    procs: Mutex<HashMap<u32, i64>>, // tgid -> process row id
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
        })
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
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO process(tgid,start,ppid,parent_id,exe,cwd,argv,root)
             VALUES(?1,0,0,NULL,?2,?3,?4,0)",
            params![pid, exe, cwd, serde_json::to_string(&argv).unwrap_or_default()],
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
