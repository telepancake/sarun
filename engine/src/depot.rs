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
    -> rusqlite::Result<Vec<(String, i64, i64, i64)>>
{
    let mut st = conn.prepare(
        "SELECT name, mode, sz, mtime FROM sqlar ORDER BY mtime DESC LIMIT ?1")?;
    let it = st.query_map([limit], |r| Ok((
        r.get::<_, String>(0)?, r.get::<_, i64>(1)?,
        r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?;
    it.collect()
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

/// Clear a node row's INLINE data — the bytes now live at
/// `blob_path(id, rowid)`. The editor save path (review::write_file) uses
/// this to convert a discard-hunk-reverted (inline) row back into a
/// standard blob-backed capture row before writing through the overlay.
pub fn archive_clear_inline(conn: &Connection, rel: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE sqlar SET data=NULL WHERE name=?1", [rel])?;
    Ok(())
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

/// Wipe a box's LAYER data — every sqlar row, its pool blobs, and the
/// node side tables (ownership/rdev/xattr). Bookkeeping tables (process,
/// outputs, meta, …) are untouched. Used by rotation before re-importing
/// the rewritten encoding.
pub fn archive_clear(conn: &Connection, box_id: i64) -> Result<(), String> {
    let mut st = conn.prepare("SELECT rowid,mode,data FROM sqlar")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(i64, u32, bool)> = st.query_map([], |r| Ok((
        r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u32,
        r.get::<_, Option<Vec<u8>>>(2)?.is_some())))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok()).collect();
    drop(st);
    for (rowid, mode, has_inline) in rows {
        if mode & 0o170000 == 0o100000 && !has_inline {
            let _ = std::fs::remove_file(blob_path(box_id, rowid));
        }
    }
    for t in ["sqlar", "ownership", "rdev", "xattr", "atime"] {
        conn.execute(&format!("DELETE FROM {t}"), [])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub trait BoxDepot {
    // ── nodes ────────────────────────────────────────────────────────────
    /// Upsert the file row for `rel` and return its rowid — the key that
    /// names the loose blob file (`blob_file`). First writer sticks.
    fn ensure_file_row(&self, rel: &str, mode: u32, writer: i64) -> i64;
    /// Final size/mtime once the blob settles (close/flush).
    fn finalize_file(&self, rel: &str, sz: i64, mtime_ns: i64, writer: i64);
    /// Normalize a discard-hunk-reverted INLINE regular-file row back to a
    /// blob-backed capture row: write the inline `data` bytes out to the
    /// row's pool blob and clear the column, then refresh the RAM mirror.
    /// `copy_up` calls this when a resolved row has no backing blob, so an
    /// inline row (which `discard_hunk`/`archive_write_inline` leaves behind
    /// — bytes in `data`, blob dropped) is transparently re-materialized
    /// before ANY writer (a live FUSE write OR `box_write_file`) sources
    /// copy_up from it. Without this, a re-run box writing a file whose hunks
    /// were discarded would fail its own capture path against its own
    /// history. Returns whether it acted (false for already-blob-backed rows,
    /// symlinks/tombstones, and absent paths).
    fn outline_inline_row(&self, rel: &str) -> std::io::Result<bool>;
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
    fn set_atime(&self, rel: &str, atime_ns: i64);
    fn atime_of(&self, rel: &str) -> Option<i64>;
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
    /// Direct overlay children of dir `rel`:
    /// (whiteout names, present names, hole names).
    fn children_of(&self, rel: &str) -> (Vec<String>, Vec<String>, Vec<String>);
    /// Refresh the in-RAM mirror entry for ONE path from the store, after
    /// an offline write through a separate connection.
    fn reload_entry(&self, rel: &str);
}

fn clear_node_metadata(conn: &Connection, rel: &str) {
    for table in ["ownership", "rdev", "xattr", "atime"] {
        let _ = conn.execute(&format!("DELETE FROM {table} WHERE name=?1"), [rel]);
    }
}

fn move_node_metadata(conn: &Connection, old: &str, new: &str) {
    clear_node_metadata(conn, new);
    for table in ["ownership", "rdev", "xattr", "atime"] {
        let _ = conn.execute(
            &format!("UPDATE {table} SET name=?2 WHERE name=?1"),
            params![old, new],
        );
    }
}

impl BoxDepot for BoxState {

    /// Upsert the file row for `rel` (data stays NULL — D4) and return its
    /// rowid, which names the pool blob. First writer sticks; last_writer moves.
    fn ensure_file_row(&self, rel: &str, mode: u32, writer: i64) -> i64 {
        if let Some(Entry::File { rowid, .. }) = self.kinds.read().unwrap().get(rel) {
            return *rowid;
        }
        let conn = self.conn.lock().unwrap();
        clear_node_metadata(&conn, rel);
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

    /// Materialize an INLINE-data regular-file row to its pool blob and clear
    /// the column (the invariant a regular-file row's bytes live at
    /// `blob_path(id, rowid)`, restored). No-op for blob-backed rows, for
    /// symlink/tombstone rows (inline is their natural form), and for absent
    /// paths. See the trait doc for why the write path funnels through here.
    fn outline_inline_row(&self, rel: &str) -> std::io::Result<bool> {
        let (rowid, data) = {
            let conn = self.conn.lock().unwrap();
            let Some(n) = archive_node(&conn, rel) else { return Ok(false) };
            if n.mode & 0o170000 != 0o100000 {
                return Ok(false); // symlink target / whiteout — inline is right
            }
            match n.data {
                Some(d) => (n.rowid, d),
                None => return Ok(false), // already blob-backed
            }
        };
        let bp = blob_path(self.id, rowid);
        if let Some(p) = bp.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&bp, &data)?;
        {
            let conn = self.conn.lock().unwrap();
            archive_clear_inline(&conn, rel).map_err(std::io::Error::other)?;
        }
        // The RAM mirror still describes the (now stale) inline row; refresh
        // it from the authoritative sqlar so a live mount serves the blob.
        self.reload_entry(rel);
        Ok(true)
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

    fn set_atime(&self, rel: &str, atime_ns: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO atime(name,ns) VALUES(?1,?2)
             ON CONFLICT(name) DO UPDATE SET ns=excluded.ns",
            rusqlite::params![rel, atime_ns],
        );
    }

    fn atime_of(&self, rel: &str) -> Option<i64> {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT ns FROM atime WHERE name=?1", [rel], |row| row.get(0))
            .ok()
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
        let (was_opaque, was_rebased) = match kinds.get(rel) {
            Some(Entry::Dir { opaque, rebased, .. }) => (*opaque, *rebased),
            _ => (false, false),
        };
        kinds.insert(rel.to_string(),
            Entry::Dir { mode: m, mtime_ns: now_ns(), opaque: was_opaque,
                         rebased: was_rebased });
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
            Some(Entry::Dir { mode, mtime_ns, rebased, .. }) => {
                kinds.insert(rel.to_string(),
                    Entry::Dir { mode, mtime_ns, opaque: true, rebased });
            }
            _ => {
                kinds.insert(rel.to_string(), Entry::Dir {
                    mode: 0o040755, mtime_ns: now_ns(), opaque: true,
                    rebased: false });
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
        let stale_rowid = match self.kinds.read().unwrap().get(rel) {
            Some(Entry::File { rowid, .. }) => Some(*rowid),
            _ => None,
        };
        let conn = self.conn.lock().unwrap();
        clear_node_metadata(&conn, rel);
        let _ = conn.execute(
            "INSERT INTO sqlar(name,mode,mtime,sz,data,writer,last_writer)
             VALUES(?1,?2,0,0,NULL,?3,?3)
             ON CONFLICT(name) DO UPDATE SET mode=excluded.mode, data=NULL, sz=0",
            params![rel, S_IFCHR, writer],
        );
        drop(conn);
        if let Some(rid) = stale_rowid {
            let _ = std::fs::remove_file(blob_path(self.id, rid));
        }
        self.kinds.write().unwrap().insert(rel.to_string(), Entry::Whiteout);
    }

    /// Move the upper row old->new (reusing the blob — rowid is stable, so the
    /// pool file at blob_path(id,rowid) stays put). Drops any pre-existing new
    /// row first. Mirror updated to match. The caller decides whether to white
    /// out `old` afterwards (it does when a lower file shows through there).
    fn rename_row(&self, old: &str, new: &str) {
        let entry = self.kinds.read().unwrap().get(old).cloned();
        let Some(entry) = entry else { return };
        let stale_rowid = match self.kinds.read().unwrap().get(new) {
            Some(Entry::File { rowid, .. }) => Some(*rowid),
            _ => None,
        };
        {
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [new]);
            move_node_metadata(&conn, old, new);
            let _ = conn.execute("UPDATE sqlar SET name=?2 WHERE name=?1",
                                 params![old, new]);
        }
        if let Some(rid) = stale_rowid {
            let _ = std::fs::remove_file(blob_path(self.id, rid));
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
        let kinds = self.kinds.read().unwrap();
        for name in &names {
            let nn = if name == old { new.to_string() }
                     else { format!("{new}/{}", &name[op.len()..]) };
            if let Some(Entry::File { rowid, .. }) = kinds.get(&nn) {
                let _ = std::fs::remove_file(blob_path(self.id, *rowid));
            }
            let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [&nn]);
            move_node_metadata(&conn, name, &nn);
            let _ = conn.execute("UPDATE sqlar SET name=?2 WHERE name=?1",
                                 params![name, nn]);
        }
        drop(kinds);
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
        clear_node_metadata(&conn, rel);
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
    fn children_of(&self, rel: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
        let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
        let mut white = vec![];
        let mut present = vec![];
        let mut holes = vec![];
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
                Entry::Hole => holes.push(tail.to_string()),
                _ => present.push(tail.to_string()),
            }
        }
        (white, present, holes)
    }
}

// ── canonical layer export / import ─────────────────────────────────────
//
// A box's captured layer ⇄ the depot node model (gimir depot crate), so a
// box can move through the canonical wire form (transfer, stream, chains).
//
// Mapping, pinned by the round-trip criterion (DEPOT-DESIGN.md §3 — the
// serialized form contains exactly what cannot be derived):
//   sqlar name "a/b/c"            → nested nodes by byte segment
//   whiteout row (mode==S_IFCHR)  → tombstone node
//   dir row                       → live node, `opaque` from its column,
//                                   attrs mode/mtime
//   symlink row                   → blob = target bytes, attrs mode/mtime
//   fifo/device row               → no blob, attrs mode/mtime (+rdev)
//   file row                      → blob = pool-blob (or inline) bytes,
//                                   attrs mode/mtime
//   ownership / rdev / xattr      → attrs uid/gid/rdev, "x:<key>"
//   the box-root opaque marker (name="") → opaque on the layer root
// sz is derived (blob length) and not exported. writer/last_writer are
// bookkeeping and not exported. A node carries attrs IFF a row exists —
// implicit path components round-trip as attr-less interior nodes.

fn a(v: impl ToString) -> Vec<u8> {
    v.to_string().into_bytes()
}

fn parse_a<T: std::str::FromStr>(attrs: &depot_model::Attrs, key: &[u8]) -> Option<T> {
    attrs.get(key).and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
}

/// Insert `node` (already shaped) at slash-separated `name` in the tree.
fn tree_insert(root: &mut depot_model::Node, name: &str, node: depot_model::Node) {
    let mut cur = root;
    let mut it = name.split('/').peekable();
    while let Some(seg) = it.next() {
        if it.peek().is_none() {
            // Merge: an implicit interior node may already exist (children
            // inserted first under ORDER BY name they cannot — parents sort
            // first — but "" segments aside, be safe and keep children).
            let entry = cur.children.entry(seg.as_bytes().to_vec())
                .or_insert_with(depot_model::Node::keep);
            let kids = std::mem::take(&mut entry.children);
            *entry = node;
            for (k, v) in kids {
                entry.children.entry(k).or_insert(v);
            }
            return;
        }
        cur = cur.children.entry(seg.as_bytes().to_vec())
            .or_insert_with(depot_model::Node::keep);
    }
}

/// Export a box's captured layer as a canonical depot layer. `conn` is an
/// open connection to the box's sqlar; `box_id` names its blob pool.
pub fn export_layer(conn: &Connection, box_id: i64)
    -> Result<depot_model::Layer, String>
{
    use depot_model::{BlobOp, Node};
    let mut side: std::collections::HashMap<String, depot_model::Attrs> =
        std::collections::HashMap::new();
    let mut load_side = |sql: &str, key: &[u8]| -> Result<(), String> {
        let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| e.to_string())?;
        for (name, v) in rows.flatten() {
            side.entry(name).or_default().insert(key.to_vec(), a(v));
        }
        Ok(())
    };
    load_side("SELECT name,uid FROM ownership", b"uid")?;
    load_side("SELECT name,gid FROM ownership", b"gid")?;
    load_side("SELECT name,dev FROM rdev", b"rdev")?;
    {
        let mut st = conn.prepare("SELECT name,key,value FROM xattr")
            .map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, String>(1)?,
            r.get::<_, Vec<u8>>(2)?))).map_err(|e| e.to_string())?;
        for (name, k, v) in rows.flatten() {
            let mut key = b"x:".to_vec();
            key.extend_from_slice(k.as_bytes());
            side.entry(name).or_default().insert(key, v);
        }
    }

    let mut root = Node::keep();
    let mut root_attrs: Option<depot_model::Attrs> = None;
    let mut st = conn.prepare(
        "SELECT rowid,name,mode,mtime,data,opaque FROM sqlar ORDER BY name")
        .map_err(|e| e.to_string())?;
    let rows = st.query_map([], |r| Ok((
        r.get::<_, i64>(0)?, r.get::<_, String>(1)?,
        r.get::<_, i64>(2)? as u32, r.get::<_, i64>(3)?,
        r.get::<_, Option<Vec<u8>>>(4)?, r.get::<_, i64>(5)?,
    ))).map_err(|e| e.to_string())?;
    for row in rows.flatten() {
        let (rowid, name, mode, mtime, data, opaque) = row;
        if name.is_empty() {
            // The box-root marker (today only its opaque bit matters).
            root.opaque = opaque != 0;
            root_attrs = Some(depot_model::Attrs::new());
            continue;
        }
        if mode == S_IFCHR {
            // opaque bit 1 = backdrop anchor: an anchored whiteout row IS
            // a hole ("not occluded"), not a deletion.
            let node = if opaque & 2 != 0 { Node::hole() }
                       else { Node::tombstone() };
            tree_insert(&mut root, &name, node);
            continue;
        }
        let mut attrs = side.remove(&name).unwrap_or_default();
        attrs.insert(b"mode".to_vec(), a(mode));
        attrs.insert(b"mtime".to_vec(), a(mtime));
        let ft = mode & 0o170000;
        let blob = if ft == 0o040000 {
            BlobOp::Keep // directory: no bytes
        } else if ft == 0o120000 {
            BlobOp::Set(data.unwrap_or_default().into()) // symlink target
        } else if ft == 0o010000 || ft == 0o060000 || ft == 0o020000 {
            BlobOp::Keep // fifo / device: no bytes
        } else {
            // Regular file: inline data (reverted content) or pool blob.
            match data {
                Some(d) => BlobOp::Set(d.into()),
                None => BlobOp::Set(
                    std::fs::read(blob_path(box_id, rowid)).unwrap_or_default().into()),
            }
        };
        let node = Node {
            presence: depot_model::Presence::Live,
            blob,
            opaque: ft == 0o040000 && opaque & 1 != 0,
            attrs: Some(attrs),
            anchor: if opaque & 2 != 0 { depot_model::Anchor::Backdrop }
                    else { depot_model::Anchor::Lower },
            children: Default::default(),
        };
        tree_insert(&mut root, &name, node);
    }
    if let Some(ra) = root_attrs {
        root.attrs = Some(ra);
    }
    Ok(depot_model::Layer { root })
}

fn import_node(conn: &Connection, box_id: i64, name: &str,
               node: &depot_model::Node) -> Result<(), String> {
    use depot_model::{BlobOp, Presence};
    if node.presence == Presence::Tombstone {
        archive_upsert(conn, name, S_IFCHR, 0, 0, None, 0)?;
        return Ok(());
    }
    let anchor_bit: i64 = if node.anchor == depot_model::Anchor::Backdrop { 2 }
                          else { 0 };
    if node.attrs.is_none() && anchor_bit != 0
        && matches!(node.blob, BlobOp::Keep)
    {
        // A backdrop-anchored node without recorded facets is a HOLE.
        archive_upsert(conn, name, S_IFCHR, 0, 0, None, anchor_bit)?;
        // (A hole may still carry children in the model; the sqlar
        // variant's rows are per-name, so children import normally.)
    }
    if let Some(attrs) = &node.attrs {
        let mode: u32 = parse_a(attrs, b"mode")
            .ok_or_else(|| format!("{name}: node without mode attr"))?;
        let mtime: i64 = parse_a(attrs, b"mtime").unwrap_or(0);
        let ft = mode & 0o170000;
        let rowid = match (&node.blob, ft) {
            (_, 0o040000) => {
                archive_upsert(conn, name, mode, mtime, 0, None,
                               node.opaque as i64 | anchor_bit)?
            }
            (BlobOp::Set(t), 0o120000) => {
                // Symlink: target inline, sz == len (the "not deflated" mark).
                archive_upsert(conn, name, mode, mtime, t.len() as i64,
                               Some(t), anchor_bit)?
            }
            (BlobOp::Set(bytes), _) => {
                // Regular file: bytes ALWAYS to the pool blob (D4).
                let rid = archive_upsert(conn, name, mode, mtime,
                                         bytes.len() as i64, None, anchor_bit)?;
                let bp = blob_path(box_id, rid);
                if let Some(p) = bp.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::write(&bp, bytes).map_err(|e| e.to_string())?;
                rid
            }
            (BlobOp::Keep, _) => {
                // fifo / device node: row only.
                archive_upsert(conn, name, mode, mtime, 0, None, anchor_bit)?
            }
            (BlobOp::Remove, _) => {
                return Err(format!("{name}: Remove blob in a full layer"));
            }
        };
        let _ = rowid;
        if let (Some(uid), Some(gid)) =
            (parse_a::<i64>(attrs, b"uid"), parse_a::<i64>(attrs, b"gid")) {
            let _ = conn.execute(
                "INSERT INTO ownership(name,uid,gid) VALUES(?1,?2,?3)
                 ON CONFLICT(name) DO UPDATE SET uid=excluded.uid, gid=excluded.gid",
                params![name, uid, gid]);
        }
        if let Some(dev) = parse_a::<i64>(attrs, b"rdev") {
            let _ = conn.execute(
                "INSERT INTO rdev(name,dev) VALUES(?1,?2)
                 ON CONFLICT(name) DO UPDATE SET dev=excluded.dev",
                params![name, dev]);
        }
        for (k, v) in attrs {
            if let Some(xk) = k.strip_prefix(b"x:".as_slice()) {
                let xk = String::from_utf8_lossy(xk).into_owned();
                let _ = conn.execute(
                    "INSERT INTO xattr(name,key,value) VALUES(?1,?2,?3)
                     ON CONFLICT(name,key) DO UPDATE SET value=excluded.value",
                    params![name, xk, v]);
            }
        }
    }
    for (seg, child) in &node.children {
        let child_name = if name.is_empty() {
            String::from_utf8_lossy(seg).into_owned()
        } else {
            format!("{name}/{}", String::from_utf8_lossy(seg))
        };
        import_node(conn, box_id, &child_name, child)?;
    }
    Ok(())
}

/// Import a canonical depot layer into a box's sqlar + blob pool. The
/// inverse of `export_layer`; existing rows with the same names are
/// replaced. The caller refreshes any live mirror afterwards
/// (`load_mirror`/`reload_entry`).
pub fn import_layer(conn: &Connection, box_id: i64,
                    layer: &depot_model::Layer) -> Result<(), String> {
    if layer.root.opaque {
        let _ = conn.execute(
            "INSERT INTO sqlar(name,mode,mtime,sz,data,opaque) \
             VALUES('',?1,0,0,NULL,1)
             ON CONFLICT(name) DO UPDATE SET opaque=1",
            params![0o040755u32]);
    }
    import_node(conn, box_id, "", &layer.root)
}

#[cfg(test)]
pub(crate) static TEST_STATE_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use depot_model::codec;

    /// Round-trip: a box layer with every node kind exports to a canonical
    /// layer, survives encode/decode, imports into a fresh box, and
    /// re-exports byte-identically. This is the transfer-fidelity check
    /// for the sqlar variant (canonical encoding = the wire form).
    #[test]
    fn export_import_roundtrip_canonical() {
        let _g = TEST_STATE_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir()
            .join(format!("sarun-depotrt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: serialized by TEST_STATE_HOME_LOCK with the other
        // state-home-dependent test.
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let src_id = 9101;
        let src = BoxState::create(src_id).unwrap();
        // Regular file with pool blob.
        let rid = src.ensure_file_row("dir/file.txt", 0o100644, 0);
        let bp = blob_path(src_id, rid);
        std::fs::create_dir_all(bp.parent().unwrap()).unwrap();
        std::fs::write(&bp, b"contents").unwrap();
        src.finalize_file("dir/file.txt", 8, 1234, 0);
        // Executable + xattr + owner.
        let rid2 = src.ensure_file_row("bin/tool", 0o100755, 0);
        let bp2 = blob_path(src_id, rid2);
        std::fs::create_dir_all(bp2.parent().unwrap()).unwrap();
        std::fs::write(&bp2, b"#!/bin/sh\n").unwrap();
        src.finalize_file("bin/tool", 10, 99, 0);
        src.set_xattr("bin/tool", "user.tag", b"v1");
        src.set_owner("bin/tool", 1000, 1000);
        // Dir (opaque), symlink, whiteout, fifo.
        src.set_dir("dir", 0o040755, 0);
        src.set_opaque("masked", 0);
        src.set_symlink("link", std::path::Path::new("dir/file.txt"), 0);
        src.set_whiteout("gone", 0);
        src.set_special("pipe", 0o010644, 0, 0);

        let layer = {
            let conn = src.conn.lock().unwrap();
            export_layer(&conn, src_id).unwrap()
        };

        // Through the canonical wire form.
        let bytes = codec::encode(&layer);
        let layer2 = codec::decode(&bytes).unwrap();
        assert_eq!(layer2, layer, "canonical round-trip changed the layer");

        // Import into a fresh box; re-export must be byte-identical.
        let dst_id = 9102;
        let dst = BoxState::create(dst_id).unwrap();
        {
            let conn = dst.conn.lock().unwrap();
            import_layer(&conn, dst_id, &layer2).unwrap();
        }
        let back = {
            let conn = dst.conn.lock().unwrap();
            export_layer(&conn, dst_id).unwrap()
        };
        assert_eq!(
            codec::encode(&back), bytes,
            "transfer through import lost fidelity"
        );

        // And the imported box serves the bytes from its OWN pool.
        let n = {
            let conn = dst.conn.lock().unwrap();
            archive_node(&conn, "dir/file.txt").unwrap()
        };
        assert_eq!(std::fs::read(blob_path(dst_id, n.rowid)).unwrap(),
                   b"contents");
        assert_eq!(n.mtime, 1234);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn fresh_box(label: &str) -> (BoxState, i64, std::path::PathBuf) {
        let tmp = std::env::temp_dir()
            .join(format!("sarun-depotblob-{}-{}", std::process::id(), label));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();
        let id = 9201;
        let b = BoxState::create(id).unwrap();
        (b, id, tmp)
    }

    fn add_file(b: &BoxState, id: i64, rel: &str, content: &[u8]) -> std::path::PathBuf {
        let rid = b.ensure_file_row(rel, 0o100644, 0);
        let bp = blob_path(id, rid);
        std::fs::create_dir_all(bp.parent().unwrap()).unwrap();
        std::fs::write(&bp, content).unwrap();
        b.finalize_file(rel, content.len() as i64, 0, 0);
        bp
    }

    #[test]
    fn rename_row_cleans_up_overwritten_blob() {
        let _g = TEST_STATE_HOME_LOCK.lock().unwrap();
        let (b, id, tmp) = fresh_box("rename");
        let bp_dst = add_file(&b, id, "dst", b"old dst");
        let _bp_src = add_file(&b, id, "src", b"src");
        assert!(bp_dst.exists());
        b.rename_row("src", "dst");
        assert!(!bp_dst.exists(), "rename_row orphaned the overwritten blob");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn set_whiteout_cleans_up_overwritten_blob() {
        let _g = TEST_STATE_HOME_LOCK.lock().unwrap();
        let (b, id, tmp) = fresh_box("whiteout");
        let bp = add_file(&b, id, "victim", b"will be whited out");
        assert!(bp.exists());
        b.set_whiteout("victim", 0);
        assert!(!bp.exists(), "set_whiteout orphaned the blob");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reparent_cleans_up_overwritten_blobs() {
        let _g = TEST_STATE_HOME_LOCK.lock().unwrap();
        let (b, id, tmp) = fresh_box("reparent");
        let bp_a = add_file(&b, id, "src/f", b"src f");
        let bp_dst = add_file(&b, id, "dst/f", b"dst f");
        assert!(bp_dst.exists());
        b.reparent("src", "dst");
        assert!(!bp_dst.exists(), "reparent orphaned the overwritten blob");
        assert!(bp_a.exists(), "reparent should preserve the moved blob");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
