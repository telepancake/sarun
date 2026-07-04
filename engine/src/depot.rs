// The depot seam — DEPOT-DESIGN.md §5 applied to the engine.
//
// `BoxDepot` is the LAYER-DATA surface of a box's store: the named tree
// of entries with whiteout (tombstone) and opaque-dir semantics, node
// metadata (mode/mtime/owner/xattr/rdev), and the loose blob files that
// hold regular-file bytes. This is sarun's production depot: the sqlar
// table is the tree index, the pool blob files are the content
// (DESIGN.md D4/D6 "index file + blob dir").
//
// Everything NOT here — process forest, outputs, brushprov, build edges,
// makevars, api log, meta flags — is bookkeeping and stays on `BoxState`
// directly (DEPOT-DESIGN.md §3: bookkeeping lives beside the depot, in
// sqlite, and must not mangle the depot API).
//
// The `writer` parameter on mutators is per-op attribution (a process
// row id) — bookkeeping the sqlar variant chooses to record inline with
// its rows; another variant may ignore it.
//
// Extracted verbatim from BoxState's inherent impl; callers now reach
// this surface ONLY through the trait (`use crate::depot::BoxDepot`).

use std::path::PathBuf;

use rusqlite::params;
use rusqlite::Connection;

use crate::capture::{now_ns, BoxState, Entry, S_IFCHR};
use crate::paths;

/// The loose content file for a regular-file node, named by its sqlar
/// rowid: live/blob/<box_id>/<rowid%1024:03x>/<rowid>. The blob layout
/// is depot-owned; nothing outside this module derives these paths.
pub fn blob_path(box_id: i64, rowid: i64) -> PathBuf {
    paths::live_home()
        .join("blob")
        .join(box_id.to_string())
        .join(format!("{:03x}", rowid % 1024))
        .join(rowid.to_string())
}

// ── the at-rest archive surface ─────────────────────────────────────────
//
// review/apply/OCI operate on at-rest .sqlar files through bare rusqlite
// Connections (open_ro/open_rw), which also serve the bookkeeping tables.
// The LAYER-DATA statements live here — this module is the only place
// that knows the sqlar layer schema — and callers pass their Connection.

/// One at-rest layer node, as stored: `data` inline bytes (symlink target
/// / reverted content), else the bytes live at `blob_path(id, rowid)`.
pub struct ArchiveNode {
    pub rowid: i64,
    pub mode: u32,
    pub mtime: i64,
    pub sz: i64,
    pub data: Option<Vec<u8>>,
    pub opaque: bool,
}

pub fn archive_node(conn: &Connection, rel: &str) -> Option<ArchiveNode> {
    conn.query_row(
        "SELECT rowid,mode,mtime,sz,data,opaque FROM sqlar WHERE name=?1", [rel],
        |r| Ok(ArchiveNode {
            rowid: r.get(0)?,
            mode: r.get::<_, i64>(1)? as u32,
            mtime: r.get(2)?,
            sz: r.get(3)?,
            data: r.get(4)?,
            opaque: r.get::<_, i64>(5)? != 0,
        })).ok()
}

pub fn archive_exists(conn: &Connection, rel: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlar WHERE name=?1", [rel], |_| Ok(()))
        .is_ok()
}

pub fn archive_mode(conn: &Connection, rel: &str) -> Option<u32> {
    conn.query_row("SELECT mode FROM sqlar WHERE name=?1", [rel],
                   |r| r.get::<_, i64>(0).map(|m| m as u32)).ok()
}

pub fn archive_mtime(conn: &Connection, rel: &str) -> Option<i64> {
    conn.query_row("SELECT mtime FROM sqlar WHERE name=?1", [rel],
                   |r| r.get(0)).ok()
}

/// All nodes (name, mode, sz), name-ordered — the session-changes listing.
pub fn archive_nodes_by_name(conn: &Connection) -> Vec<(String, u32, i64)> {
    let Ok(mut st) = conn.prepare(
        "SELECT name,mode,sz FROM sqlar ORDER BY name") else { return vec![] };
    let Ok(it) = st.query_map([], |r| Ok((
        r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32,
        r.get::<_, i64>(2)?))) else { return vec![] };
    it.flatten().collect()
}

/// All nodes with content + opaque, name-ordered — the layer-export walk
/// (OCI build_layer_tar).
#[allow(clippy::type_complexity)]
pub fn archive_all_nodes(conn: &Connection)
    -> rusqlite::Result<Vec<(i64, String, u32, Option<Vec<u8>>, i64)>>
{
    let mut st = conn.prepare(
        "SELECT rowid,name,mode,data,opaque FROM sqlar ORDER BY name")?;
    let rows = st.query_map([], |r| Ok((
        r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u32,
        r.get::<_, Option<Vec<u8>>>(3)?, r.get::<_, i64>(4)?,
    )))?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

/// (name, mode) of every node — the cheap shape scan.
pub fn archive_names_modes(conn: &Connection) -> Vec<(String, u32)> {
    let Ok(mut st) = conn.prepare("SELECT name,mode FROM sqlar") else {
        return vec![];
    };
    let Ok(rows) = st.query_map([], |r| Ok((
        r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))) else {
        return vec![];
    };
    rows.flatten().collect()
}

/// Most-recently-touched nodes: (name, mode, sz, mtime), mtime-descending.
pub fn archive_recent(conn: &Connection, limit: i64)
    -> Vec<(String, u32, i64, i64)>
{
    let Ok(mut st) = conn.prepare(
        "SELECT name, mode, sz, mtime FROM sqlar ORDER BY mtime DESC LIMIT ?1")
    else { return vec![] };
    let Ok(it) = st.query_map([limit], |r| Ok((
        r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32,
        r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))) else { return vec![] };
    it.flatten().collect()
}

/// Remove a node row (the caller handles its blob file).
pub fn archive_delete(conn: &Connection, rel: &str) {
    let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [rel]);
}

/// Write content INLINE into an existing node row (discard-hunk revert),
/// returning its rowid so the caller can drop the stale pool blob.
pub fn archive_write_inline(conn: &Connection, rel: &str, data: &[u8])
    -> rusqlite::Result<Option<i64>>
{
    conn.execute("UPDATE sqlar SET sz=?1, data=?2 WHERE name=?3",
                 params![data.len() as i64, data, rel])?;
    Ok(archive_node(conn, rel).map(|n| n.rowid))
}

/// INSERT OR REPLACE a full node row (apply-promote / copy-down target),
/// returning the new rowid.
pub fn archive_upsert(conn: &Connection, rel: &str, mode: u32, mtime: i64,
                      sz: i64, data: Option<&[u8]>, opaque: i64)
    -> Result<i64, String>
{
    conn.execute(
        "INSERT OR REPLACE INTO sqlar(name,mode,mtime,sz,data,opaque) \
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![rel, mode as i64, mtime, sz, data, opaque])
        .map_err(|x| x.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub trait BoxDepot {
    // ── nodes ────────────────────────────────────────────────────────────
    /// Upsert the file row for `rel` and return its rowid — the key that
    /// names the loose blob file (`blob_file`). First writer sticks.
    fn ensure_file_row(&self, rel: &str, mode: u32, writer: i64) -> i64;
    /// Final size/mtime once the blob settles (close/flush).
    fn finalize_file(&self, rel: &str, sz: i64, mtime_ns: i64, writer: i64);
    fn set_dir(&self, rel: &str, mode: u32, writer: i64);
    fn set_symlink(&self, rel: &str, target: &std::path::Path, writer: i64);
    /// fifo / char / block device node.
    fn set_special(&self, rel: &str, mode: u32, rdev: u64, writer: i64);
    /// First-class deletion: masks the name in every lower layer.
    fn set_whiteout(&self, rel: &str, writer: i64);
    /// Opaque directory: masks LOWER-layer children only; the dir itself
    /// stays visible.
    fn set_opaque(&self, rel: &str, writer: i64);
    fn is_opaque(&self, rel: &str) -> bool;
    // ── node metadata ────────────────────────────────────────────────────
    fn set_mode(&self, rel: &str, full_mode: u32);
    fn set_mtime(&self, rel: &str, mtime_ns: i64);
    fn set_owner(&self, rel: &str, uid: u32, gid: u32);
    fn owner_of(&self, rel: &str) -> Option<(u32, u32)>;
    fn set_xattr(&self, rel: &str, key: &str, value: &[u8]);
    fn get_xattr(&self, rel: &str, key: &str) -> Option<Vec<u8>>;
    fn list_xattr(&self, rel: &str) -> Vec<String>;
    fn remove_xattr(&self, rel: &str, key: &str) -> bool;
    // ── tree restructuring ──────────────────────────────────────────────
    /// Move one node old→new (content/blob key preserved).
    fn rename_row(&self, old: &str, new: &str);
    /// Move a whole subtree old/ → new/ in place.
    fn reparent(&self, old: &str, new: &str);
    /// Drop a node entirely (the change un-happens; blob removed).
    fn drop_row(&self, rel: &str);
    // ── readout ─────────────────────────────────────────────────────────
    fn entry(&self, rel: &str) -> Option<Entry>;
    /// Direct overlay children of dir `rel`: (whiteout names, present names).
    fn children_of(&self, rel: &str) -> (Vec<String>, Vec<String>);
    /// Refresh the in-RAM mirror entry for ONE path from the store, after
    /// an offline write through a separate connection.
    fn reload_entry(&self, rel: &str);
    /// The loose file holding a regular-file node's bytes, named by the
    /// rowid `ensure_file_row` returned.
    fn blob_file(&self, rowid: i64) -> PathBuf;
}

impl BoxDepot for BoxState {
    fn blob_file(&self, rowid: i64) -> PathBuf {
        blob_path(self.id, rowid)
    }

    /// Upsert the file row for `rel` (data stays NULL — D4) and return its
    /// rowid, which names the pool blob. First writer sticks; last_writer moves.
    fn ensure_file_row(&self, rel: &str, mode: u32, writer: i64) -> i64 {
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
    fn finalize_file(&self, rel: &str, sz: i64, mtime_ns: i64, writer: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE sqlar SET sz=?2, mtime=?3, last_writer=?4 WHERE name=?1",
            params![rel, sz, mtime_ns, writer],
        );
    }

    /// Apply a new mode to an existing file/dir row (chmod). The audit found
    /// the old path silently no-op'd: ensure_file_row early-returns for an
    /// existing row and never ran its mode UPDATE. This is the explicit fix.
    fn set_mode(&self, rel: &str, full_mode: u32) {
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
    fn set_mtime(&self, rel: &str, mtime_ns: i64) {
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
    fn set_owner(&self, rel: &str, uid: u32, gid: u32) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO ownership(name,uid,gid) VALUES(?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET uid=excluded.uid, gid=excluded.gid",
            rusqlite::params![rel, uid, gid]);
    }

    fn owner_of(&self, rel: &str) -> Option<(u32, u32)> {
        self.conn.lock().unwrap().query_row(
            "SELECT uid,gid FROM ownership WHERE name=?1", [rel],
            |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32))).ok()
    }

    // ── xattr (side table; the box's processes get real getfattr/setfattr) ──
    fn set_xattr(&self, rel: &str, key: &str, value: &[u8]) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO xattr(name,key,value) VALUES(?1,?2,?3)
             ON CONFLICT(name,key) DO UPDATE SET value=excluded.value",
            rusqlite::params![rel, key, value]);
    }

    fn get_xattr(&self, rel: &str, key: &str) -> Option<Vec<u8>> {
        self.conn.lock().unwrap().query_row(
            "SELECT value FROM xattr WHERE name=?1 AND key=?2",
            rusqlite::params![rel, key], |r| r.get(0)).ok()
    }

    fn list_xattr(&self, rel: &str) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        let mut out = vec![];
        if let Ok(mut st) = conn.prepare("SELECT key FROM xattr WHERE name=?1") {
            if let Ok(it) = st.query_map([rel], |r| r.get::<_, String>(0)) {
                out = it.flatten().collect();
            }
        }
        out
    }

    fn remove_xattr(&self, rel: &str, key: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM xattr WHERE name=?1 AND key=?2",
                     rusqlite::params![rel, key]).map(|n| n > 0).unwrap_or(false)
    }

    /// mknod/mkfifo: a special-file row (mode carries S_IFIFO/S_IFCHR/S_IFBLK);
    /// char/block rdev goes in the side table.
    fn set_special(&self, rel: &str, mode: u32, rdev: u64, writer: i64) {
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

    fn set_dir(&self, rel: &str, mode: u32, writer: i64) {
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
    fn set_opaque(&self, rel: &str, writer: i64) {
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
    fn is_opaque(&self, rel: &str) -> bool {
        matches!(self.kinds.read().unwrap().get(rel),
            Some(Entry::Dir { opaque: true, .. }))
    }

    fn set_symlink(&self, rel: &str, target: &std::path::Path, writer: i64) {
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

    /// Refresh the in-RAM `kinds` mirror entry for ONE path from the
    /// authoritative sqlar. Called after an OFFLINE write to this box's sqlar
    /// (apply-promote / copy-down done through a separate connection) so a
    /// running FUSE mount serves the new state — the write path no longer needs
    /// to care whether a process is running in the box. If the row is gone, the
    /// mirror entry is removed. sarun: keeps live/at-rest writes uniform.
    fn reload_entry(&self, rel: &str) {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT mode,sz,data FROM sqlar WHERE name=?1", [rel],
            |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?))).ok();
        let mut kinds = self.kinds.write().unwrap();
        match row {
            Some((mode, sz, data)) => {
                let entry = Self::entry_from_row(&conn, rel, mode, sz, data);
                kinds.insert(rel.to_string(), entry);
            }
            None => {
                kinds.remove(rel);
            }
        }
    }

    fn set_whiteout(&self, rel: &str, writer: i64) {
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

    /// Move the upper row old->new (reusing the blob — rowid is stable, so the
    /// pool file at blob_path(id,rowid) stays put). Drops any pre-existing new
    /// row first. Mirror updated to match. The caller decides whether to white
    /// out `old` afterwards (it does when a lower file shows through there).
    fn rename_row(&self, old: &str, new: &str) {
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
    fn reparent(&self, old: &str, new: &str) {
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
    fn drop_row(&self, rel: &str) {
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

    fn entry(&self, rel: &str) -> Option<Entry> {
        self.kinds.read().unwrap().get(rel).cloned()
    }

    /// Direct overlay children of dir `rel`: (whiteout names, present names).
    fn children_of(&self, rel: &str) -> (Vec<String>, Vec<String>) {
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
