// Review-view verbs in Rust: session_changes (list a box's changes) and hunks
// (unified text diff of lower vs captured). Read-only against the box's
// on-disk sqlar (a fresh RO connection coexists with a live box's writer), so
// these serve both live and finished boxes. Output shapes match the Python
// ChangeReview exactly (the UI and the conformance readers depend on it).
// apply/discard (host-mutating, need live-connection ownership routing) and
// the structural-diff job path are deferred to a later milestone.

use std::ffi::CStr;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::path::PathBuf;

use crate::hostfs;

use base64::Engine;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::params;
use serde_json::Value;
use serde_json::json;
use similar::DiffTag;
use similar::TextDiff;

use crate::capture::blob_path;
use crate::paths;

fn sqlar_path(id: i64) -> PathBuf {
    paths::state_home().join(format!("{id}.sqlar"))
}

fn open_ro(id: i64) -> Option<Connection> {
    Connection::open_with_flags(sqlar_path(id), OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

const S_IFMT: u32 = 0o170000;
const S_IFCHR: u32 = 0o020000;
const S_IFLNK: u32 = 0o120000;

pub fn session_changes(id: i64) -> Value {
    let Some(conn) = open_ro(id) else { return json!([]) };
    let mut out = vec![];
    if let Ok(mut st) = conn.prepare("SELECT name,mode,sz FROM sqlar ORDER BY name") {
        let it = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32,
                r.get::<_, i64>(2)?))
        });
        if let Ok(it) = it {
            for (name, mode, sz) in it.flatten() {
                let kind = if mode & S_IFMT == S_IFCHR { "deleted" }
                           else if mode & S_IFMT == S_IFLNK { "symlink" }
                           else { "changed" };
                out.push(json!({"path": name, "kind": kind, "size": sz}));
            }
        }
    }
    Value::Array(out)
}

/// The box's current bytes for `rel`: symlink target (raw in the row) or the
/// pool blob for a file. None if the row is missing or a tombstone.
fn current_bytes(id: i64, rel: &str) -> Option<Vec<u8>> {
    let conn = open_ro(id)?;
    let (rowid, mode, _sz, data): (i64, u32, i64, Option<Vec<u8>>) = conn
        .query_row("SELECT rowid,mode,sz,data FROM sqlar WHERE name=?1", [rel],
                   |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32, r.get(2)?, r.get(3)?)))
        .ok()?;
    if mode & S_IFMT == S_IFCHR {
        return None; // tombstone
    }
    if let Some(d) = data {
        return Some(d); // symlink target (raw) or any inline row
    }
    std::fs::read(blob_path(id, rowid)).ok()
}

fn lower_bytes(rel: &str) -> Vec<u8> {
    let p = Path::new("/").join(rel);
    match std::fs::symlink_metadata(&p) {
        Ok(m) if !m.is_dir() => std::fs::read(&p).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

pub fn current_mode(id: i64, rel: &str) -> Option<u32> {
    let conn = open_ro(id)?;
    conn.query_row("SELECT mode FROM sqlar WHERE name=?1", [rel],
                   |r| r.get::<_, i64>(0)).ok().map(|m| m as u32)
}

pub fn hunks(id: i64, rel: &str) -> Value {
    let rel = rel.trim_start_matches('/');
    let Some(mode) = current_mode(id, rel) else {
        return json!({"is_text": false, "hunks": [],
                      "diff": {"kind": "error", "error": "gone"}});
    };
    if mode & S_IFMT == S_IFCHR {
        return json!({"is_text": false, "hunks": [], "diff": {"kind": "deleted"}});
    }
    let host = Path::new("/").join(rel);
    if mode & S_IFMT == S_IFLNK {
        let tgt = current_bytes(id, rel).unwrap_or_default();
        let kind = if host.symlink_metadata().map(|m| m.file_type().is_symlink())
            .unwrap_or(false) { "modified" } else { "created" };
        return json!({"is_text": false, "hunks": [],
            "diff": {"kind": kind,
                     "diff": format!("symlink → {}", String::from_utf8_lossy(&tgt))}});
    }
    let cur = current_bytes(id, rel).unwrap_or_default();
    let low = lower_bytes(rel);
    let text = !cur.contains(&0) && !low.contains(&0);
    if !text {
        let kind = if host.exists() { "modified" } else { "created" };
        let mut d = json!({"kind": kind, "content": b64(&cur)});
        if kind == "modified" && !low.is_empty() {
            d["content_before"] = json!(b64(&low));
        }
        return json!({"is_text": false, "hunks": [], "diff": d});
    }
    // text: grouped unified diff, lines tagged like _build_hunks_display.
    let lo = String::from_utf8_lossy(&low).into_owned();
    let cu = String::from_utf8_lossy(&cur).into_owned();
    let diff = TextDiff::from_lines(&lo, &cu);
    let ll: Vec<&str> = diff.iter_old_slices().map(|s| s.trim_end_matches(['\r', '\n'])).collect();
    let ul: Vec<&str> = diff.iter_new_slices().map(|s| s.trim_end_matches(['\r', '\n'])).collect();
    let mut hunks = vec![];
    for (gi, group) in diff.grouped_ops(3).iter().enumerate() {
        if group.is_empty() { continue; }
        let (_, a0, _) = group[0].as_tag_tuple();
        let (_, alast, blast) = group[group.len() - 1].as_tag_tuple();
        let (_, _, b0) = group[0].as_tag_tuple();
        let mut lines = vec![json!(["hdr",
            format!("@@ -{},{} +{},{} @@", a0.start + 1, alast.end - a0.start,
                    b0.start + 1, blast.end - b0.start)])];
        for op in group {
            let (tag, orange, nrange) = op.as_tag_tuple();
            match tag {
                DiffTag::Equal => for k in orange { lines.push(json!([" ", ll[k]])); },
                _ => {
                    for k in orange { lines.push(json!(["-", ll[k]])); }
                    for k in nrange { lines.push(json!(["+", ul[k]])); }
                }
            }
        }
        hunks.push(json!({"index": gi, "lines": lines}));
    }
    json!({"is_text": true, "hunks": hunks})
}

/// st_mtime_ns stored for `rel` in the box's sqlar, or None.
pub fn current_mtime(id: i64, rel: &str) -> Option<i64> {
    let conn = open_ro(id)?;
    conn.query_row("SELECT mtime FROM sqlar WHERE name=?1", [rel],
                   |r| r.get::<_, i64>(0)).ok()
}

/// Mirror of Python ChangeReview.decorate: per-row lazy decoration for ONE
/// changed entry — {is_text, stale, kind}. is_text = NUL-pairwise text rule,
/// stale = host mtime newer than the stored mtime, kind refined to
/// created/modified/deleted via a single host lstat.
/// Decorate a batch of paths in one go (one RPC, one server-side host stat
/// loop). Used by the UI to decorate a window of changes-pane rows without
/// paying a round-trip per row.
pub fn decorate_many(id: i64, rels: &[&str]) -> Value {
    Value::Array(rels.iter().map(|r| decorate(id, r)).collect())
}

/// Newest-first slice of the box's change set — the source feed for a live
/// box's "recently changed" panel in the boxes view. Sorted by sqlar.mtime
/// desc, capped at `limit`. Returns the same row shape as session_changes
/// so the UI can reuse the same render path.
pub fn recent_changes(id: i64, limit: i64) -> Value {
    let Some(conn) = open_ro(id) else { return json!([]) };
    let mut out = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT name, mode, sz FROM sqlar ORDER BY mtime DESC LIMIT ?1") {
        let it = st.query_map([limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u32,
                r.get::<_, i64>(2)?,
            ))
        });
        if let Ok(it) = it {
            for (name, mode, sz) in it.flatten() {
                let kind = if mode & S_IFMT == S_IFCHR { "deleted" }
                           else if mode & S_IFMT == S_IFLNK { "symlink" }
                           else { "changed" };
                out.push(json!({"path": name, "kind": kind, "size": sz}));
            }
        }
    }
    Value::Array(out)
}

/// Five-list bundle for the Sessions-view right pane: newest-first
/// previews of each kind, capped at `limit` per kind. One RPC per
/// session-switch instead of five. xattr modifications ride in the
/// changes list as their own rows (kind="xattr"), tagged with the file
/// they hang off + the xattr key — they were invisible before, now
/// they aren't.
pub fn box_summary(id: i64, limit: i64) -> Value {
    let Some(conn) = open_ro(id) else {
        return json!({"outputs":[], "changes":[], "processes":[],
                      "pipelines":[], "edges":[]});
    };
    // Files: newest-first by mtime. Same kind classification as
    // recent_changes (sqlar S_IFCHR row = whiteout = deleted).
    let mut file_rows: Vec<(i64, Value)> = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT name, mode, sz, mtime FROM sqlar ORDER BY mtime DESC LIMIT ?1") {
        if let Ok(it) = st.query_map([limit], |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32,
            r.get::<_, i64>(2)?, r.get::<_, i64>(3)?,
        ))) {
            for (name, mode, sz, mtime) in it.flatten() {
                let kind = if mode & S_IFMT == S_IFCHR { "deleted" }
                           else if mode & S_IFMT == S_IFLNK { "symlink" }
                           else { "changed" };
                file_rows.push((mtime, json!({
                    "path": name, "kind": kind, "size": sz, "mtime": mtime,
                })));
            }
        }
    }
    // xattrs: the side table has no mtime, so we ride the OWNING file's
    // mtime to mix them into one timeline. Each xattr (name,key) pair is
    // one row with kind="xattr". Key + value-byte-count surface; the raw
    // bytes don't (they could be huge / binary).
    let mut xattr_rows: Vec<(i64, Value)> = vec![];
    if has_table(&conn, "xattr") {
        if let Ok(mut st) = conn.prepare(
            "SELECT x.name, x.key, length(x.value), \
                    COALESCE(s.mtime, 0) \
             FROM xattr x LEFT JOIN sqlar s ON s.name=x.name \
             ORDER BY COALESCE(s.mtime, 0) DESC LIMIT ?1") {
            if let Ok(it) = st.query_map([limit], |r| Ok((
                r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?, r.get::<_, i64>(3)?,
            ))) {
                for (path, key, vlen, mtime) in it.flatten() {
                    xattr_rows.push((mtime, json!({
                        "path": path, "kind": "xattr",
                        "xattr_key": key, "xattr_len": vlen, "mtime": mtime,
                    })));
                }
            }
        }
    }
    // Merge file + xattr rows by mtime desc, cap at `limit`.
    let mut changes_merged: Vec<(i64, Value)> = file_rows;
    changes_merged.extend(xattr_rows);
    changes_merged.sort_by(|a, b| b.0.cmp(&a.0));
    let changes: Vec<Value> = changes_merged.into_iter()
        .take(limit as usize).map(|(_, v)| v).collect();

    let mut outputs = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT id, ts, stream, length(content), \
                CAST(substr(content,1,80) AS TEXT) \
         FROM outputs ORDER BY id DESC LIMIT ?1") {
        if let Ok(it) = st.query_map([limit], |r| Ok(json!({
            "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
            "stream": r.get::<_, i64>(2)?, "len": r.get::<_, i64>(3)?,
            "preview": r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        }))) {
            for row in it.flatten() { outputs.push(row); }
        }
    }
    // Reverse so the topmost row in the UI is the OLDEST of the tail —
    // matches how a transcript reads. Actually no: keep newest-first so
    // the user sees the latest first. The UI renders top-down.

    let mut processes = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT id, tgid, exe, argv FROM process ORDER BY id DESC LIMIT ?1") {
        if let Ok(it) = st.query_map([limit], |r| {
            let argv: String = r.get(3)?;
            let av: Vec<String> = serde_json::from_str(&argv).unwrap_or_default();
            let head = av.first().cloned().unwrap_or_default();
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "tgid": r.get::<_, i64>(1)?,
                "exe": r.get::<_, String>(2)?, "argv0": head,
            }))
        }) {
            for row in it.flatten() { processes.push(row); }
        }
    }

    let mut pipelines = vec![];
    if has_table(&conn, "brushprov") {
        if let Ok(mut st) = conn.prepare(
            "SELECT id, cmd, COALESCE(nested,0) FROM brushprov \
             ORDER BY id DESC LIMIT ?1") {
            if let Ok(it) = st.query_map([limit], |r| Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "cmd": r.get::<_, String>(1)?,
                "nested": r.get::<_, i64>(2)? != 0,
            }))) {
                for row in it.flatten() { pipelines.push(row); }
            }
        }
    }

    let mut edges = vec![];
    if has_table(&conn, "build_edges") {
        if let Ok(mut st) = conn.prepare(
            "SELECT id, outs, cmd FROM build_edges ORDER BY id DESC LIMIT ?1") {
            if let Ok(it) = st.query_map([limit], |r| {
                let outs: String = r.get(1)?;
                let arr: Vec<String> = serde_json::from_str(&outs).unwrap_or_default();
                let head = arr.first().cloned().unwrap_or_default();
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "out": head,
                    "n_outs": arr.len(),
                    "cmd": r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                }))
            }) {
                for row in it.flatten() { edges.push(row); }
            }
        }
    }

    json!({
        "outputs":   outputs,
        "changes":   changes,
        "processes": processes,
        "pipelines": pipelines,
        "edges":     edges,
    })
}

/// Cheap "does this sqlar have a given table" probe — Python-engine
/// archives won't have brushprov / build_edges / xattr; old sarun
/// archives won't have one or the other. Saves a noisy SQLITE_ERROR.
fn has_table(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name], |_| Ok(())).is_ok()
}

pub fn decorate(id: i64, rel: &str) -> Value {
    let rel = rel.trim_start_matches('/');
    let Some(mode) = current_mode(id, rel) else {
        return json!({"is_text": false, "stale": false, "kind": "changed"});
    };
    let host = Path::new("/").join(rel);
    if mode & S_IFMT == S_IFCHR {
        return json!({"is_text": false, "stale": false, "kind": "deleted"});
    }
    // is_text: both base and current NUL-free, and not a symlink/tombstone.
    let is_text = if mode & S_IFMT == S_IFLNK {
        false
    } else {
        match current_bytes(id, rel) {
            Some(cur) if !cur.contains(&0) => !lower_bytes(rel).contains(&0),
            _ => false,
        }
    };
    let hstat = host.symlink_metadata();
    let exists = hstat.is_ok();
    let kind = if exists { "modified" } else { "created" };
    let mut stale = false;
    if let Ok(md) = &hstat {
        if let Some(cm) = current_mtime(id, rel) {
            use std::os::unix::fs::MetadataExt;
            let host_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
            stale = host_ns > cm;
        }
    }
    json!({"is_text": is_text, "stale": stale, "kind": kind})
}

// ── host-mutating review actions (top-level boxes; nested promotion deferred) ──
use std::time::Duration;

fn open_rw(id: i64) -> Option<Connection> {
    let c = Connection::open(sqlar_path(id)).ok()?;
    c.busy_timeout(Duration::from_secs(3)).ok()?;
    Some(c)
}

/// The nesting context for apply/discard/finalize: how to find a box's PARENT,
/// its immediate CHILDREN, and any box's live BoxState (RAM mirror). Built from
/// the engine's `Overlay` (live boxes) + on-disk discovery (at-rest parent/child
/// links). When there is no overlay (a stale/non-server caller), every box is
/// treated as at-rest and links come from the on-disk sqlar meta alone — so the
/// nested semantics still hold for finished boxes.
///
/// A box's apply with a parent PROMOTES into that parent's overlay (a nested
/// pending change); only a TOP-LEVEL box's apply reaches the real host. A
/// discard copies each path DOWN into immediate children that inherit it before
/// the row is dropped.
pub struct NestCtx {
    overlay: Option<crate::overlay::Overlay>,
}

impl NestCtx {
    pub fn new(overlay: Option<crate::overlay::Overlay>) -> Self {
        Self { overlay }
    }

    /// `id`'s parent box id, from on-disk discovery (the authoritative sqlar
    /// meta) — same answer whether or not the box is running.
    fn parent_of(&self, id: i64) -> Option<i64> {
        crate::discover::discover().get(&id).and_then(|b| b.parent)
    }

    /// `id`'s immediate child box ids (parent_box_id == id), live + at-rest.
    fn children_of(&self, id: i64) -> Vec<i64> {
        crate::discover::discover().values()
            .filter(|b| b.parent == Some(id) && b.box_id != id)
            .map(|b| b.box_id).collect()
    }

    /// `id`'s live BoxState, used ONLY to refresh the in-RAM mirror after a
    /// write so a running FUSE mount stays coherent — never to change the
    /// logical read/write result, which is always the sqlar's.
    fn live(&self, id: i64) -> Option<std::sync::Arc<crate::capture::BoxState>> {
        self.overlay.as_ref().and_then(|o| o.live_box(id))
    }

    /// D-parent: is `id`'s `readonly_parent` flag set? Read from the sqlar meta —
    /// same answer running or not. The flag is a child's ATTITUDE toward its
    /// parent; it stops `apply` from promoting captured changes into the parent.
    fn readonly_parent_of(&self, id: i64) -> bool {
        crate::discover::box_meta(id).get("readonly_parent")
            .map(String::as_str) == Some("1")
    }
}

fn row_of(conn: &Connection, rel: &str) -> Option<(i64, u32, Option<Vec<u8>>)> {
    conn.query_row("SELECT rowid,mode,data FROM sqlar WHERE name=?1", [rel],
                   |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32, r.get(2)?))).ok()
}

fn consume(conn: &Connection, id: i64, rel: &str, rowid: i64) {
    let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [rel]);
    let _ = std::fs::remove_file(blob_path(id, rowid));
}

const S_IFIFO: u32 = 0o010000;
const S_IFBLK: u32 = 0o060000;

/// `lsetxattr` on the leaf beneath the already-resolved `parent` dir fd,
/// surfacing the OS error instead of dropping it (audit H4). Mirrors
/// `hostfs::setxattr_at` byte for byte (the same `/proc/self/fd/<parent>/<leaf>`
/// confinement that does not follow the final symlink), except it returns a
/// Result so a failed restore can abort the apply rather than be silently lost.
/// Lives here (not hostfs) only because hostfs is out of scope for this change.
fn setxattr_at_checked(parent: BorrowedFd, name: &CStr, key: &CStr, val: &[u8])
    -> Result<(), String> {
    let leaf = name.to_str().map_err(|_| "non-utf8 leaf name".to_string())?;
    let path = format!("/proc/self/fd/{}/{}", parent.as_raw_fd(), leaf);
    let cpath = CString::new(path).map_err(|_| "NUL in xattr path".to_string())?;
    // SAFETY: valid C strings and byte buffer.
    let r = unsafe {
        libc::lsetxattr(cpath.as_ptr(), key.as_ptr(),
                        val.as_ptr().cast(), val.len(), 0)
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// Atomically write `bytes` (with exact `mode`) to the leaf `name` beneath the
/// already-resolved `parent` dir fd (audit C2'): write a sibling temp file in
/// the SAME directory, fsync it, then `renameat` it over the target — so an
/// error mid-write can never truncate or corrupt the host file that's already
/// there. On any failure the temp is unlinked and prior host content is
/// untouched. The C2 ancestor-symlink guard is preserved by the caller, which
/// resolved `parent` with per-component O_NOFOLLOW; this helper only ever
/// touches names directly under that fd. As the OLD `hostfs::write_file_at`
/// did, a pre-existing SYMLINK at the leaf is refused (we must not replace a
/// box-planted symlink with content — that was the C2-class escape the leaf
/// O_NOFOLLOW check guarded against), so we lstat first and bail on a symlink.
fn write_file_atomic_at(parent: BorrowedFd, name: &CStr, bytes: &[u8], mode: u32)
    -> Result<(), String> {
    // Refuse to clobber a symlink leaf (parity with write_file_at's O_NOFOLLOW
    // open, which errored on an existing symlink rather than following it).
    if let Some(st) = hostfs::lstat_at(parent, name) {
        if st.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err("refusing to overwrite a symlink leaf".into());
        }
    }
    // Unique sibling temp name under the SAME parent dir (same filesystem → the
    // rename is atomic). Created O_EXCL|O_NOFOLLOW so it can never race onto an
    // attacker-planted name or symlink.
    let leaf = name.to_str().map_err(|_| "non-utf8 leaf name".to_string())?;
    let tmp_name = format!(".sarun-apply-tmp-{}-{}-{}",
        std::process::id(), apply_tmp_seq(), leaf);
    // Cap the length so a very long leaf can't push us past NAME_MAX (255).
    let tmp_name: String = tmp_name.chars().take(250).collect();
    let ctmp = CString::new(tmp_name).map_err(|_| "NUL in temp name".to_string())?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
        | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid dirfd and C string; variadic mode arg for O_CREAT.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), ctmp.as_ptr(), flags, mode & 0o7777) };
    if fd < 0 {
        return Err(format!("create temp: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fresh owned fd; File takes ownership and closes it on drop.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    // Helper to clean up the temp on any failure below.
    let cleanup = || { let _ = hostfs::unlink_at(parent, &ctmp); };
    let write_res = (|| -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::File::from(owned);
        f.write_all(bytes).map_err(|e| format!("write temp: {e}"))?;
        // O_CREAT's mode is umask-masked, so set the exact mode explicitly and
        // surface any failure (audit H4: a 0600 file must not land world-readable).
        // SAFETY: valid open fd.
        if unsafe { libc::fchmod(f.as_raw_fd(), mode & 0o7777) } != 0 {
            return Err(format!("set mode: {}", std::io::Error::last_os_error()));
        }
        // Flush the data to disk before the rename so a crash can't leave a
        // renamed-but-empty file shadowing the prior content.
        f.flush().map_err(|e| format!("flush temp: {e}"))?;
        // SAFETY: valid open fd.
        if unsafe { libc::fsync(f.as_raw_fd()) } != 0 {
            return Err(format!("fsync temp: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    })();
    if let Err(e) = write_res {
        cleanup();
        return Err(e);
    }
    // Atomic replace. renameat never follows a symlink at either end, so the
    // target is replaced as a whole — no write-through-symlink escape.
    // SAFETY: valid dirfds and C strings.
    let r = unsafe {
        libc::renameat(parent.as_raw_fd(), ctmp.as_ptr(),
                       parent.as_raw_fd(), name.as_ptr())
    };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        cleanup();
        return Err(format!("rename temp into place: {e}"));
    }
    Ok(())
}

/// Monotonic counter making each apply temp file name unique within this process.
fn apply_tmp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Restore mtime / owner / xattrs onto a just-materialized leaf. Audit H4:
/// xattr (and mode, set at create time in hostfs) failures must NOT be reported
/// as a successful apply — they are surfaced as an Err. Owner is legitimately
/// best-effort: an unprivileged host user's `lchown` EPERMs on a uid/gid it
/// can't assign, so a failed owner restore is INTENTIONALLY swallowed (the
/// content is correct; only the recorded uid/gid couldn't be reproduced).
/// xattrs, by contrast, can carry security-relevant state (capabilities, ACLs,
/// SELinux labels), so a dropped setxattr is a real divergence — collected and
/// returned.
fn restore_metadata_at(conn: &Connection, rel: &str, parent: BorrowedFd, leaf: &CStr, mtime_ns: i64)
    -> Result<(), String> {
    // mtime (atime = mtime): drives downstream make/rebuild decisions.
    hostfs::utimens_at(parent, leaf, mtime_ns);
    // owner: best-effort. lchown EPERMs for an unprivileged host user (it cannot
    // give a file away to another uid/gid), which is the common case here, so a
    // failure is expected and deliberately ignored — see the doc comment above.
    if let Ok((uid, gid)) = conn.query_row(
        "SELECT uid,gid FROM ownership WHERE name=?1", [rel],
        |r| Ok((r.get::<_,i64>(0)? as u32, r.get::<_,i64>(1)? as u32))) {
        hostfs::chown_at(parent, leaf, uid, gid);
    }
    // xattrs: surface failures. A dropped setxattr can silently lose a security
    // attribute (file caps / ACL / label), so the FIRST failure aborts and is
    // returned rather than logged-and-forgotten.
    if let Ok(mut st) = conn.prepare("SELECT key,value FROM xattr WHERE name=?1") {
        if let Ok(rows) = st.query_map([rel], |r|
            Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?))) {
            for (k, v) in rows.flatten() {
                let ck = CString::new(k.clone())
                    .map_err(|_| format!("xattr key '{k}' contains NUL"))?;
                setxattr_at_checked(parent, leaf, &ck, &v)
                    .map_err(|e| format!("setxattr '{k}': {e}"))?;
            }
        }
    }
    Ok(())
}

fn materialize(conn: &Connection, id: i64, rel: &str) -> Result<(), String> {
    let root = hostfs::open_root().map_err(|e| format!("open root: {e}"))?;
    materialize_at(root.as_fd(), conn, id, rel)
}

/// Write the captured change `rel` onto the host beneath `root`, never following
/// a symlinked component of `rel` (audit C2: a box-planted symlink must not let
/// an apply escape onto an arbitrary host path). `root` is `/` in production and
/// a temp dir under test. Every host mutation goes through `hostfs`'s `*at`
/// helpers, which resolve the parent with per-component `O_NOFOLLOW` and refuse
/// to write/delete through a symlink at the leaf.
fn materialize_at(root: BorrowedFd, conn: &Connection, id: i64, rel: &str) -> Result<(), String> {
    let (rowid, mode, data) = row_of(conn, rel).ok_or("not in archive")?;
    let mtime_ns: i64 = conn.query_row("SELECT mtime FROM sqlar WHERE name=?1", [rel],
                                       |r| r.get(0)).unwrap_or(0);

    if mode & S_IFMT == S_IFCHR {
        // char-device row == deletion tombstone (the Python convention). Resolve
        // WITHOUT creating ancestors; if the parent path doesn't exist (or an
        // ancestor is a symlink we refuse to follow), there is nothing to
        // delete — a no-op, matching the old exists()-guarded behavior.
        let Ok((parent, leaf)) = hostfs::parent_beneath(root, rel, false) else {
            return Ok(());
        };
        hostfs::remove_tree_at(parent.as_fd(), &leaf)?;
        return Ok(());
    }

    // All creating branches resolve (and create) the parent dirs beneath root,
    // refusing any symlinked ancestor.
    let (parent, leaf) = hostfs::parent_beneath(root, rel, true)?;
    let parent = parent.as_fd();

    if mode & S_IFMT == S_IFLNK {
        let tgt = data.ok_or("symlink row has no target")?;
        hostfs::symlink_at(parent, &leaf, &tgt)?;
    } else if mode & S_IFMT == 0o040000 {
        hostfs::mkdir_at(parent, &leaf, mode)?;
    } else if mode & S_IFMT == S_IFIFO || mode & S_IFMT == S_IFBLK {
        // fifo / block device: recreate the node on the host.
        let rdev: i64 = conn.query_row("SELECT dev FROM rdev WHERE name=?1", [rel],
                                       |r| r.get(0)).unwrap_or(0);
        hostfs::mknod_at(parent, &leaf, mode, rdev as u64)?;
    } else {
        let bytes = match data {
            Some(d) => d,
            None => std::fs::read(blob_path(id, rowid)).map_err(|e| e.to_string())?,
        };
        // Audit C2': atomic temp-then-rename in the SAME parent dir (resolved
        // above with per-component O_NOFOLLOW), so an error mid-write can never
        // truncate or corrupt the host file already there.
        write_file_atomic_at(parent, &leaf, &bytes, mode)?;
    }
    // Audit H4: a metadata-restore failure (xattr / mode) must NOT be reported
    // as a successful apply, so it propagates.
    restore_metadata_at(conn, rel, parent, &leaf, mtime_ns)?;
    Ok(())
}

fn paths_arg(id: i64, paths: &Value) -> Vec<String> {
    if let Some(arr) = paths.as_array() {
        return arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
    }
    session_changes(id).as_array().map(|a| a.iter()
        .filter_map(|e| e.get("path").and_then(Value::as_str).map(String::from))
        .collect()).unwrap_or_default()
}

/// Audit M1: has the real host file at `rel` changed since this box captured it?
/// True when the host path exists and its mtime is strictly newer than the
/// mtime stored in the box's sqlar (the moment of capture). Same comparison the
/// `decorate` `stale` flag uses, lifted into a hard pre-apply gate. A path the
/// box created (no host file) or one with no recorded capture mtime is NOT
/// considered stale. Conservative: only refuses on a positive staleness signal,
/// so the normal apply path (host unchanged) is untouched.
fn host_changed_since_capture(id: i64, rel: &str) -> bool {
    let rel = rel.trim_start_matches('/');
    let host = Path::new("/").join(rel);
    let Ok(md) = host.symlink_metadata() else { return false };
    let Some(cap_ns) = current_mtime(id, rel) else { return false };
    use std::os::unix::fs::MetadataExt;
    let host_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
    host_ns > cap_ns
}

/// apply == PROMOTE into the parent overlay (a nested box) or WRITE the host
/// (a top-level box). Mirror of Python ChangeReview.apply. For each path: a box
/// WITH a parent promotes the captured change into the parent's overlay (a
/// pending change in the parent box), routed through the parent's live BoxState
/// when running, else its at-rest sqlar; a TOP-LEVEL box materializes the change
/// onto the real host. On success the path is consumed from this box's archive.
///
/// Audit H3: this reads the box's pool blobs, which a live FUSE write may be
/// mid-`write_at` on, so it must only run on a STOPPED box. The running-box
/// guard lives at the control-plane callers (control.rs `apply`/`discard` and
/// `review.apply`/`review.discard`), where the engine's `box_pids` live-set is
/// in reach — mirroring how `dissolve` guards itself.
///
/// TODO (audit C3): this multi-path apply is NOT transactional. It
/// materializes-then-consumes per path, so an error at path N leaves
/// 1..N-1 already written to the host AND consumed from the archive, with N..
/// still pending — there is no "nothing happened" rollback. A full fix
/// (stage all paths, then commit-or-rollback as one unit) is a large redesign
/// deliberately deferred; the per-path staleness guard below at least refuses
/// to silently clobber a host file that changed since capture.
pub fn apply(id: i64, paths: &Value, ctx: &NestCtx) -> Value {
    let Some(conn) = open_rw(id) else {
        return json!({"applied": [], "errors": [{"path": "", "error": "no archive"}]});
    };
    let parent = ctx.parent_of(id);
    // D-parent: a child marked `readonly_parent` REFUSES to promote into its
    // parent — its captured changes can be reviewed/discarded but never leak
    // up the box stack. Same flag also blocks the top-level host-materialize
    // when a no-parent box has it set (e.g. an OCI rootfs that should never
    // touch the host). The error string is the same shape Python returns so
    // the UI's error pane works uniformly.
    let ro_parent = ctx.readonly_parent_of(id);
    let mut applied = vec![];
    let mut errors = vec![];
    for rel in paths_arg(id, paths) {
        let rel = rel.trim_start_matches('/').to_string();
        let result = if ro_parent {
            Err("parent is read-only (--readonly-parent); apply refused".into())
        } else { match parent {
            Some(p) => {
                // Nested box: promote into the parent's overlay, not the host.
                let plive = ctx.live(p);
                promote_into_parent(id, p, plive.as_deref(), &rel)
            }
            None => {
                // Top-level: write the real host. Audit M1 — refuse to silently
                // overwrite a host file that changed AFTER this box captured it
                // (the host mtime is newer than the stored capture mtime). The
                // `decorate` stale flag is the same advisory the UI shows; here
                // it becomes a hard refusal so a concurrent edit isn't clobbered
                // without the user knowing. The user can re-capture / re-run to
                // pick up the new baseline.
                if host_changed_since_capture(id, &rel) {
                    Err("host file changed since capture (stale); apply refused \
                         — re-run the box to pick up the new contents".into())
                } else {
                    materialize(&conn, id, &rel)
                }
            }
        }};
        match result {
            Ok(()) => {
                if let Some((rowid, _, _)) = row_of(&conn, &rel) {
                    consume(&conn, id, &rel, rowid);
                }
                applied.push(Value::String(rel));
            }
            Err(e) => errors.push(json!({"path": rel, "error": e})),
        }
    }
    json!({"applied": applied, "errors": errors})
}

/// discard == drop each change from the box WITHOUT writing the host — but first
/// copy it DOWN into any immediate child that inherits it, so the child's merged
/// view is unchanged. Mirror of Python ChangeReview.discard. A copy-down failure
/// for a path leaves that path in place (errored) — the child must not lose its
/// inherited view.
pub fn discard(id: i64, paths: &Value, ctx: &NestCtx) -> Value {
    let mut discarded = vec![];
    let mut errors = vec![];
    let children = |b: i64| ctx.children_of(b);
    let resolve = |b: i64| ctx.live(b);
    if let Some(conn) = open_rw(id) {
        for rel in paths_arg(id, paths) {
            let rel = rel.trim_start_matches('/').to_string();
            if let Err(e) = copydown_to_children(id, &rel, &children, &resolve) {
                errors.push(json!({"path": rel, "error": e}));
                continue;
            }
            if let Some((rowid, _, _)) = row_of(&conn, &rel) {
                consume(&conn, id, &rel, rowid);
                discarded.push(Value::String(rel));
            }
        }
    }
    json!({"discarded": discarded, "errors": errors})
}

/// Split bytes into lines on '\n', keeping the terminator on each line (the last
/// line keeps whatever it had). join(result) == data exactly — byte-exact splice
/// (mirror of Python ut_split).
fn ut_split(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    let parts: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    for p in &parts[..parts.len() - 1] {
        let mut l = p.to_vec();
        l.push(b'\n');
        out.push(l);
    }
    if let Some(last) = parts.last() {
        if !last.is_empty() {
            out.push(last.to_vec());
        }
    }
    out
}

/// (lower byte-lines, upper byte-lines, grouped opcodes) for a text change, or
/// None for a non-text change. Each group is a Vec of (tag, i1, i2, j1, j2),
/// matching Python difflib.get_grouped_opcodes(3) tuple shape so the splice math
/// (a1,a2,b1,b2 = g[0][1], g[-1][2], g[0][3], g[-1][4]) carries over verbatim.
type Group = Vec<(DiffTag, usize, usize, usize, usize)>;
fn hunk_groups(id: i64, rel: &str) -> Option<(Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Group>)> {
    let rel = rel.trim_start_matches('/');
    let mode = current_mode(id, rel)?;
    if mode & S_IFMT == S_IFCHR || mode & S_IFMT == S_IFLNK {
        return None;
    }
    let cur = current_bytes(id, rel)?;
    if cur.contains(&0) {
        return None;
    }
    let low = lower_bytes(rel);
    if low.contains(&0) {
        return None;
    }
    let ll = ut_split(&low);
    let ul = ut_split(&cur);
    // Group via the SAME line-diff path hunks() uses (cross-checked equal to
    // Python difflib), then carry the indices onto the raw byte-line vectors so
    // the splice stays byte-exact (CR/CRLF, missing final newline preserved).
    let lo = String::from_utf8_lossy(&low).into_owned();
    let cu = String::from_utf8_lossy(&cur).into_owned();
    let diff = TextDiff::from_lines(&lo, &cu);
    let mut groups = vec![];
    for g in diff.grouped_ops(3) {
        if g.is_empty() {
            continue;
        }
        let mut group = vec![];
        for op in &g {
            let (tag, o, n) = op.as_tag_tuple();
            group.push((tag, o.start, o.end, n.start, n.end));
        }
        groups.push(group);
    }
    Some((ll, ul, groups))
}

/// Write `new_lower` (a sequence of raw byte-lines) to the host at `rel`,
/// refusing to write through a symlink. Mirror of Python _write_host_hunk.
fn write_host_hunk(rel: &str, new_lower: &[Vec<u8>]) -> Value {
    // Same symlink-safety as materialize (audit C2): resolve the parent beneath
    // `/` without following any symlinked component, and refuse to write through
    // a symlink at the leaf. Preserve the existing file's mode (this is an
    // in-place text edit, not a fresh capture).
    let root = match hostfs::open_root() {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": format!("open root: {e}")}),
    };
    let (parent, leaf) = match hostfs::parent_beneath(root.as_fd(), rel, true) {
        Ok(x) => x,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let bytes: Vec<u8> = new_lower.concat();
    match hostfs::write_file_preserve_mode_at(parent.as_fd(), &leaf, &bytes) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// After a hunk op the diff is gone exactly when the stored current bytes equal
/// the host's bytes; drop the row + pool blob then (mirror of SqlarSource.settle).
fn settle(id: i64, rel: &str) {
    let rel = rel.trim_start_matches('/');
    let cur = current_bytes(id, rel).unwrap_or_default();
    if cur == lower_bytes(rel) {
        if let Some(conn) = open_rw(id) {
            if let Some((rowid, _, _)) = row_of(&conn, rel) {
                consume(&conn, id, rel, rowid);
            }
        }
    }
}

/// Revert bytes back into the box's current state for `rel` (discard_hunk): write
/// the new bytes inline into the sqlar row's data and drop the stale pool blob so
/// it can't shadow the new content. Mirror of SqlarSource.write_current.
fn write_current(id: i64, rel: &str, data: &[u8]) -> Option<Value> {
    let rel = rel.trim_start_matches('/');
    let conn = open_rw(id)?;
    let rowid = match conn.execute(
        "UPDATE sqlar SET sz=?1, data=?2 WHERE name=?3",
        params![data.len() as i64, data, rel]) {
        Ok(_) => row_of(&conn, rel).map(|(r, _, _)| r),
        Err(e) => return Some(json!({"ok": false, "error": e.to_string()})),
    };
    if let Some(r) = rowid {
        let _ = std::fs::remove_file(blob_path(id, r));
    }
    None
}

/// apply_hunk: splice ONE hunk group onto the host. The box already contains it,
/// so that hunk simply stops being a difference. Byte-exact on raw byte-lines.
/// Mirror of Python ChangeReview.apply_hunk; returns {ok, ...}.
pub fn apply_hunk(id: i64, rel: &str, index: i64) -> Value {
    let Some((ll, ul, groups)) = hunk_groups(id, rel) else {
        return json!({"ok": false, "error": "not a text change"});
    };
    if index < 0 || index as usize >= groups.len() {
        return json!({"ok": false, "error": "stale hunk"});
    }
    let g = &groups[index as usize];
    let a1 = g[0].1;
    let a2 = g[g.len() - 1].2;
    let b1 = g[0].3;
    let b2 = g[g.len() - 1].4;
    let mut new_lower: Vec<Vec<u8>> = vec![];
    new_lower.extend_from_slice(&ll[..a1]);
    new_lower.extend_from_slice(&ul[b1..b2]);
    new_lower.extend_from_slice(&ll[a2..]);
    let res = write_host_hunk(rel, &new_lower);
    if res.get("ok").and_then(Value::as_bool) != Some(true) {
        return res;
    }
    settle(id, rel);
    json!({"ok": true})
}

/// discard_hunk: revert one hunk in the box (back to the host's bytes at that
/// range). Mirror of Python ChangeReview.discard_hunk; returns {ok, ...}.
pub fn discard_hunk(id: i64, rel: &str, index: i64) -> Value {
    let Some((ll, ul, groups)) = hunk_groups(id, rel) else {
        return json!({"ok": false, "error": "not a text change"});
    };
    if index < 0 || index as usize >= groups.len() {
        return json!({"ok": false, "error": "stale hunk"});
    }
    let g = &groups[index as usize];
    let a1 = g[0].1;
    let a2 = g[g.len() - 1].2;
    let b1 = g[0].3;
    let b2 = g[g.len() - 1].4;
    let mut new_upper: Vec<Vec<u8>> = vec![];
    new_upper.extend_from_slice(&ul[..b1]);
    new_upper.extend_from_slice(&ll[a1..a2]);
    new_upper.extend_from_slice(&ul[b2..]);
    let bytes: Vec<u8> = new_upper.concat();
    if let Some(err) = write_current(id, rel, &bytes) {
        return err;
    }
    settle(id, rel);
    json!({"ok": true})
}

/// finalize_by_rules: split the box's changes by the file rules — apply the
/// apply-matched paths to the host, discard everything else (the rest copies
/// nowhere for a top-level box). Used by dissolve. Returns {applied, discarded,
/// errors}; non-empty errors mean the caller must NOT free the box.
pub fn finalize_by_rules(id: i64, ctx: &NestCtx) -> Value {
    let rules = crate::rules::Rules::load();
    // Box display name (only resolved when a rule actually needs it).
    let box_name = if rules.needs_box() {
        crate::discover::display_path(&crate::discover::discover(), id)
    } else { String::new() };
    let mut apply_paths = vec![];
    let mut discard_paths = vec![];
    for e in session_changes(id).as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let rel = e.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        // The change's FIRST-WRITER provenance (exe/cwd/argv) + box, so a
        // process-/box-scoped rule decides exactly as the Python FileRules.
        let mut subject = crate::rules::Subject {
            box_name: box_name.clone(), ..Default::default() };
        if rules.needs_proc() {
            let prov = crate::discover::first_writer_prov(id, &rel);
            subject.exe = prov.get("exe").and_then(Value::as_str).unwrap_or("").to_string();
            subject.cwd = prov.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
            subject.argv = prov.get("argv").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
        }
        match rules.decide(&rel, &subject) {
            Some(crate::rules::Action::Apply) => apply_paths.push(Value::from(rel)),
            _ => discard_paths.push(Value::from(rel)),  // discard / passthrough / none
        }
    }
    let ar = apply(id, &Value::Array(apply_paths), ctx);
    // The discard pass now copies each path DOWN into immediate children that
    // inherit it (discard() does this) before dropping the row — so a finalized
    // box with children preserves each child's merged view, matching Python.
    let dr = discard(id, &Value::Array(discard_paths), ctx);
    let mut errs = ar.get("errors").and_then(Value::as_array).cloned()
        .unwrap_or_default();
    errs.extend(dr.get("errors").and_then(Value::as_array).cloned().unwrap_or_default());
    json!({"applied": ar.get("applied").cloned().unwrap_or(json!([])),
           "discarded": dr.get("discarded").cloned().unwrap_or(json!([])),
           "errors": Value::Array(errs)})
}

/// One source entry's full record (the sqlar row + its side-table rows), read
/// once from the source box's at-rest sqlar so the writers below never re-read.
struct SrcEntry {
    rowid: i64,
    mode: u32,
    mtime: i64,
    sz: i64,
    data: Option<Vec<u8>>,
    opaque: i64,
    owner: Option<(i64, i64)>,
    rdev: Option<i64>,
    xattrs: Vec<(String, Vec<u8>)>,
}

/// Read `rel`'s complete record from `src`'s on-disk sqlar (row + ownership +
/// rdev + xattrs). None if the source has no such row.
fn read_src_entry(src: i64, rel: &str) -> Option<SrcEntry> {
    let pc = open_ro(src)?;
    let (rowid, mode, mtime, sz, data, opaque): (i64, i64, i64, i64, Option<Vec<u8>>, i64) = pc
        .query_row("SELECT rowid,mode,mtime,sz,data,opaque FROM sqlar WHERE name=?1",
                   [rel], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                                  r.get(4)?, r.get(5)?))).ok()?;
    let owner: Option<(i64, i64)> = pc.query_row(
        "SELECT uid,gid FROM ownership WHERE name=?1", [rel],
        |r| Ok((r.get(0)?, r.get(1)?))).ok();
    let rdev: Option<i64> = pc.query_row("SELECT dev FROM rdev WHERE name=?1", [rel],
                                         |r| r.get(0)).ok();
    let mut xattrs: Vec<(String, Vec<u8>)> = vec![];
    if let Ok(mut st) = pc.prepare("SELECT key,value FROM xattr WHERE name=?1") {
        if let Ok(rows) = st.query_map([rel], |r|
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))) {
            xattrs = rows.flatten().collect();
        }
    }
    Some(SrcEntry { rowid, mode: mode as u32, mtime, sz, data, opaque, owner, rdev, xattrs })
}

/// Does `id`'s OWN view resolve `rel` to "whiteout" (a tombstone), "present"
/// (file/symlink/dir/special), or None? Mirror of Python ChangeReview._own_kind.
///
/// Reads the box's sqlar — the single authoritative store. Every overlay write
/// (live FUSE handler OR offline promote/copy-down) is write-through to the
/// sqlar, and a live box's in-RAM `kinds` mirror is a strict subset of it (a
/// whiteout is an `S_IFCHR` row, etc.), so the answer does NOT depend on whether
/// a process is running in the box.
fn own_kind(id: i64, rel: &str) -> Option<&'static str> {
    let conn = open_ro(id)?;
    let mode: u32 = conn.query_row("SELECT mode FROM sqlar WHERE name=?1", [rel],
                                   |r| r.get::<_, i64>(0)).ok().map(|m| m as u32)?;
    Some(if mode & S_IFMT == S_IFCHR { "whiteout" } else { "present" })
}

/// Does `id`'s LOWER (what it INHERITS, ignoring its own overlay) currently
/// resolve `rel` to a PRESENT entry? Walks the parent chain to the host —
/// mirror of Python ChangeReview._lower_has:
///   - no parent (top-level box): whether the host path exists or is a symlink;
///   - has parent p: inspect p's OWN entry — a whiteout means deleted (False);
///     a present entry means True; no own entry → recurse into p's lower.
/// Reads the authoritative sqlar of each box in the chain (parent links from
/// on-disk discovery), so the result does NOT depend on whether any box in the
/// chain is running. A `seen` set + depth cap guard a circular parent chain.
pub fn lower_has(id: i64, rel: &str) -> bool {
    let rel = rel.trim_start_matches('/');
    let boxes = crate::discover::discover();
    let mut cur = id;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let Some(psid) = boxes.get(&cur).and_then(|b| b.parent) else {
            let host = Path::new("/").join(rel);
            return host.symlink_metadata().is_ok();
        };
        if !seen.insert(psid) {
            return false; // cycle in the parent chain: stop safely
        }
        match own_kind(psid, rel) {
            Some("whiteout") => return false,
            Some(_) => return true,
            None => cur = psid,
        }
    }
    false // depth exceeded: treat as not found
}

/// Write a source entry RECORD into a destination box's overlay — ONE write path
/// (the destination's authoritative sqlar) shared by dissolve copy-down and
/// nested apply-promote, with a live destination's RAM mirror refreshed
/// afterward. Does not depend on whether a process runs in the destination.
///
/// For a tombstone source (a deletion), `tombstone_as_whiteout` chooses the
/// outcome: true → write a whiteout into the destination (the deletion must
/// shadow whatever the destination's lower still resolves); false → just drop
/// the destination's own row + blob (its lower has nothing here, so no shadow is
/// needed — a plain row-drop, mirroring _promote_into_parent's delete branch).
fn promote_record(e: &SrcEntry, src: i64, dst: i64,
                  dst_live: Option<&crate::capture::BoxState>,
                  rel: &str, tombstone_as_whiteout: bool) -> Result<(), String> {
    let kind = e.mode & S_IFMT;

    // ONE write path: write the destination's authoritative sqlar (a fresh RW
    // connection — fine for a live box too; SQLite serializes writers and the
    // box's own writes are autocommit). Whether or not a process is running in
    // the destination is irrelevant to WHAT is written; a running box just gets
    // its in-RAM `kinds` mirror refreshed afterward (see the reload_entry tail)
    // so its FUSE mount serves the new state.
    let result: Result<(), String> = (|| {
        let cc = open_rw(dst).ok_or("destination archive unavailable")?;
        if kind == S_IFCHR && !tombstone_as_whiteout {
            // Lower has nothing here: drop the destination's own row + blob.
            if let Some((rowid, _, _)) = row_of(&cc, rel) {
                consume(&cc, dst, rel, rowid);
            }
            return Ok(());
        }
        // INSERT OR REPLACE so an apply-promote OVERWRITES the destination's
        // prior view; drop any stale blob the replaced row named first. (A
        // copy-down never reaches here for an already-present destination — its
        // caller guards on has_own.)
        if let Some((old_rowid, _, _)) = row_of(&cc, rel) {
            let _ = std::fs::remove_file(blob_path(dst, old_rowid));
        }
        cc.execute("INSERT OR REPLACE INTO sqlar(name,mode,mtime,sz,data,opaque) \
                    VALUES(?1,?2,?3,?4,?5,?6)",
                   params![rel, e.mode as i64, e.mtime, e.sz, e.data, e.opaque])
            .map_err(|x| x.to_string())?;
        let new_rowid = cc.last_insert_rowid();
        if kind == 0o100000 {
            let s = blob_path(src, e.rowid);
            if s.exists() {
                let dstb = blob_path(dst, new_rowid);
                if let Some(p) = dstb.parent() {
                    std::fs::create_dir_all(p).map_err(|x| x.to_string())?;
                }
                std::fs::copy(&s, &dstb).map_err(|x| x.to_string())?;
            }
        }
        if let Some((u, g)) = e.owner {
            let _ = cc.execute("INSERT OR REPLACE INTO ownership(name,uid,gid) \
                                VALUES(?1,?2,?3)", params![rel, u, g]);
        }
        if let Some(dev) = e.rdev {
            let _ = cc.execute("INSERT OR REPLACE INTO rdev(name,dev) VALUES(?1,?2)",
                               params![rel, dev]);
        }
        for (k, v) in &e.xattrs {
            let _ = cc.execute("INSERT OR REPLACE INTO xattr(name,key,value) \
                                VALUES(?1,?2,?3)", params![rel, k, v]);
        }
        Ok(())
        // cc is dropped here, releasing the write lock BEFORE the mirror refresh.
    })();

    result?;
    // Cache-coherence, not a behavioral branch: a running destination re-reads
    // the one row we just wrote into its in-RAM mirror so its FUSE mount is
    // consistent. An at-rest destination has no mirror — nothing to do.
    if let Some(cb) = dst_live {
        cb.reload_entry(rel);
    }
    Ok(())
}

/// Promote `rel`'s captured change from `box_id` (the box being APPLIED) INTO
/// `parent`'s overlay — a nested apply captures the change as a PENDING change
/// in the parent box instead of writing the host. Mirror of Python
/// _promote_into_parent. `parent_live` routes the write through the parent's
/// live BoxState (RAM mirror) when the parent is running. A deletion promotes as
/// a whiteout iff the PARENT's own lower (its parent chain) still resolves rel to
/// a present entry; otherwise it drops the parent's own row.
pub fn promote_into_parent(box_id: i64, parent: i64,
                           parent_live: Option<&crate::capture::BoxState>,
                           rel: &str) -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    let Some(e) = read_src_entry(box_id, rel) else {
        return Err("not in archive".into());
    };
    let tombstone_as_whiteout = lower_has(parent, rel);
    promote_record(&e, box_id, parent, parent_live, rel, tombstone_as_whiteout)
}

/// Copy a single parent entry DOWN into a child box, but ONLY if the child has
/// no entry of its own for that path. This preserves the child's merged view
/// (read-through-parent) at the instant the parent is dissolved OR a parent path
/// is discarded: a path the child inherited from the parent (never touched
/// itself) would change once the parent's row is dropped, so we snapshot the
/// parent's version into the child first. If the child already has its own row
/// for `rel`, its view is self-contained and we leave it untouched.
///
/// `child_live` is the child's live `BoxState` when a process is running in it —
/// used ONLY to refresh the child's RAM mirror after the write (via
/// promote_record), never to change what is read or written. Whether the child
/// "has its own entry" is read from its authoritative sqlar regardless. Files
/// copy the parent blob into the child's pool under a fresh rowid;
/// symlinks/tombstones/special carry their row data + side tables. A tombstone
/// copies down AS a whiteout (the child keeps seeing 'absent').
pub fn copy_down_entry(parent: i64, child: i64, rel: &str,
                       child_live: Option<&crate::capture::BoxState>)
    -> Result<(), String> {
    let rel = rel.trim_start_matches('/');
    // Child already speaks for this path (its own sqlar row exists) — its view
    // is self-contained, nothing to copy down. Read the sqlar, not a live mirror.
    let has = open_ro(child).and_then(|c| c.query_row(
        "SELECT 1 FROM sqlar WHERE name=?1", [rel], |_| Ok(())).ok()).is_some();
    if has {
        return Ok(());
    }
    let Some(e) = read_src_entry(parent, rel) else {
        return Err("parent has no such entry".into());
    };
    promote_record(&e, parent, child, child_live, rel, /*tombstone_as_whiteout=*/true)
}

/// Copy `rel` DOWN into every immediate child of `sid` that inherits it (has no
/// own entry) BEFORE `sid`'s own row is dropped — so discarding from `sid` never
/// changes a child's merged view. Mirror of Python _copydown_to_children.
/// `children_of(sid)` lists the immediate child box ids (live + at-rest);
/// `resolve_live(c)` is each child's live BoxState when running. Returns
/// Err(msg) if any child copy-down failed (the caller MUST NOT then drop the
/// row — the child would lose its inherited view).
fn copydown_to_children<C, F>(sid: i64, rel: &str, children_of: &C, resolve_live: &F)
    -> Result<(), String>
    where C: Fn(i64) -> Vec<i64>,
          F: Fn(i64) -> Option<std::sync::Arc<crate::capture::BoxState>> {
    let kids = children_of(sid);
    if kids.is_empty() {
        return Ok(());
    }
    // Source claims this path; if its bytes can't be read, fail closed.
    if read_src_entry(sid, rel).is_none() {
        return Err(format!("copy-down: {rel} not readable from source"));
    }
    for child in kids {
        let live = resolve_live(child);
        copy_down_entry(sid, child, rel, live.as_deref())
            .map_err(|e| format!("copy-down into {child}: {e}"))?;
    }
    Ok(())
}

/// Rewrite a child box's parent pointer in its sqlar meta (the on-disk source
/// discover() reads `parent_box_id` from). `new` = Some(grandparent) reparents;
/// None promotes the child to top-level (deletes the key).
pub fn set_parent_meta(child: i64, new: Option<i64>) -> Result<(), String> {
    let cc = open_rw(child).ok_or("child archive unavailable")?;
    match new {
        Some(p) => cc.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('parent_box_id',?1)",
            params![p.to_string()]),
        None => cc.execute("DELETE FROM meta WHERE key='parent_box_id'", []),
    }.map_err(|e| e.to_string())?;
    Ok(())
}

/// Mark a child box's sqlar as `no_host_fallback=1` — the closure bit that stops
/// resolve()/scan_dir() falling absent paths through to the real host. Used by
/// dissolve(): when the box being freed carried the closure (an OCI image's
/// --no-parent base), each child must inherit it, or re-parenting onto the
/// grandparent (often top-level) would silently re-open the child to the host.
/// The on-disk write is for at-rest children; a live child flips its in-RAM
/// atomic via BoxState::set_no_host_fallback as well.
pub fn set_no_host_meta(child: i64) -> Result<(), String> {
    let cc = open_rw(child).ok_or("child archive unavailable")?;
    cc.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('no_host_fallback','1')",
        [],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

/// All changed paths a box captured (apply- and discard-bound alike) — the set
/// a child may have inherited a view of through this box.
pub fn changed_paths(id: i64) -> Vec<String> {
    session_changes(id).as_array().map(|a| a.iter()
        .filter_map(|e| e.get("path").and_then(Value::as_str).map(String::from))
        .collect()).unwrap_or_default()
}

/// Unified diff for the whole box (the `patch` CLI verb). Per changed path: a
/// git-style ---/+++ header and the text hunks, or a one-line note for
/// binary/symlink/deleted. Best-effort, human-facing.
pub fn patch_text(id: i64) -> Vec<u8> {
    let mut out = String::new();
    let changes = session_changes(id);
    for e in changes.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let rel = e.get("path").and_then(Value::as_str).unwrap_or("");
        let h = hunks(id, rel);
        if h.get("is_text").and_then(Value::as_bool) == Some(true) {
            out.push_str(&format!("--- a/{rel}\n+++ b/{rel}\n"));
            for hk in h.get("hunks").and_then(Value::as_array).unwrap_or(&vec![]) {
                for line in hk.get("lines").and_then(Value::as_array)
                    .unwrap_or(&vec![]) {
                    if let Some(pair) = line.as_array() {
                        let tag = pair[0].as_str().unwrap_or(" ");
                        let txt = pair[1].as_str().unwrap_or("");
                        let pre = if tag == "hdr" { "" } else { tag };
                        out.push_str(&format!("{pre}{txt}\n"));
                    }
                }
            }
        } else {
            let kind = h.get("diff").and_then(|d| d.get("kind"))
                .and_then(Value::as_str).unwrap_or("changed");
            out.push_str(&format!("--- a/{rel}\n+++ b/{rel}\n# {kind} (non-text)\n"));
        }
    }
    out.into_bytes()
}

// ── structural diff (binary detail pane) ────────────────────────────────────
// Mirrors the Python ChangeReview.structural_diff_{quick,finish}: sniff the
// type of a binary change's bytes, pick a differ argv template (readelf -Wa for
// ELF, ar/unzip/tar for other recognized types), run that differ on the base
// and current bytes INSIDE a locked-down bwrap sandbox, and return a unified
// diff of the two textual dumps. The quick verb returns the type line(s) + the
// header immediately plus a job id; the finish verb runs the (heavy, sandboxed)
// dump synchronously in its handler thread and returns the full line list.
// Wire shapes match what the Python RemoteReview expects:
//   struct_quick  -> {"lines": [[style,text],...], "job": <id|null>}
//   struct_finish -> {"lines": [[style,text],...]}
// where each tuple is a 2-element JSON array [style, text].

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

const STRUCT_MAX: usize = 4 * 1024 * 1024;
const SANDBOX_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

struct StructJob {
    argv: Vec<String>,
    base: Vec<u8>,
    cur: Vec<u8>,
    head: Vec<Value>,
}

fn job_registry() -> &'static StdMutex<HashMap<i64, StructJob>> {
    static REG: OnceLock<StdMutex<HashMap<i64, StructJob>>> = OnceLock::new();
    REG.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn next_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn pair(style: &str, text: &str) -> Value {
    json!([style, text])
}

/// Best-effort type sniff. Read the common magic numbers directly (no libmagic
/// dependency); fall back to `file --brief` for anything else. Produces strings
/// `differ_for` matches against ("ELF", "ar archive", …).
fn struct_type(data: &[u8]) -> String {
    if data.len() >= 4 && &data[..4] == b"\x7fELF" {
        return "ELF binary".to_string();
    }
    if data.len() >= 8 && &data[..8] == b"!<arch>\n" {
        return "current ar archive".to_string();
    }
    if data.len() >= 2 && &data[..2] == b"PK" {
        return "Zip archive data".to_string();
    }
    if data.len() >= 2 && &data[..2] == b"\x1f\x8b" {
        return "gzip compressed data".to_string();
    }
    if let Some(t) = file_type(&data[..data.len().min(65536)]) {
        if !t.is_empty() {
            return t;
        }
    }
    "data".to_string()
}

/// Shell out to `file --brief` on the leading bytes (best-effort fallback).
fn file_type(data: &[u8]) -> Option<String> {
    let tmp = scratch_file("sniff", data).ok()?;
    let out = std::process::Command::new("file")
        .arg("--brief").arg(&tmp).output().ok();
    let _ = std::fs::remove_file(&tmp);
    let out = out?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Pick (argv_template, label) for a recognized binary type, else None.
/// `{in}` is the placeholder for the input path inside the sandbox. Mirrors the
/// Python differ_for choices for the tools available here.
fn differ_for(mtype: &str, data: &[u8]) -> Option<(Vec<String>, String)> {
    if data.is_empty() {
        return None;
    }
    let mt = mtype.to_lowercase();
    let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if mt.contains("elf") {
        return Some((v(&["readelf", "-Wa", "{in}"]), "ELF (readelf -Wa)".into()));
    }
    if mt.contains("ar archive") {
        return Some((v(&["ar", "t", "{in}"]), "ar archive (ar t)".into()));
    }
    if mt.contains("zip archive") || &data[..data.len().min(2)] == b"PK" {
        return Some((v(&["unzip", "-l", "{in}"]), "zip (unzip -l)".into()));
    }
    if mt.contains("tar archive") || mt.contains("gzip compressed")
        || mt.contains("bzip2") || mt.contains("xz compressed") {
        return Some((v(&["tar", "-tvf", "{in}"]), "tar (tar -tvf)".into()));
    }
    None
}

/// FAST half: type line(s) + differ selection. Returns the wire dict
/// {"lines": [...], "job": <id|null>}. When `job` is null the lines are the
/// complete result (unrecognized type or over the size cap). Never panics.
pub fn struct_quick(id: i64, rel: &str) -> Value {
    let rel = rel.trim_start_matches('/');
    let base = lower_bytes(rel);
    let cur = current_bytes(id, rel).unwrap_or_default();
    let mut lines: Vec<Value> = vec![];
    if !base.is_empty() && !cur.is_empty() {
        lines.push(pair("type", &format!("type (base): {}", struct_type(&base))));
        lines.push(pair("type", &format!("type (current): {}", struct_type(&cur))));
    } else {
        let sniff = if cur.is_empty() { &base } else { &cur };
        lines.push(pair("type", &format!("type: {}", struct_type(sniff))));
    }
    let sniff = if cur.is_empty() { base.clone() } else { cur.clone() };
    let Some((argv, label)) = differ_for(&struct_type(&sniff), &sniff) else {
        return json!({"lines": lines, "job": Value::Null});
    };
    lines.push(pair("hdr", &format!("\u{2500}\u{2500} structural diff \u{b7} {label} \u{2500}\u{2500}")));
    if base.len() > STRUCT_MAX || cur.len() > STRUCT_MAX {
        lines.push(pair("dim", &format!("(skipped: file exceeds {STRUCT_MAX} bytes)")));
        return json!({"lines": lines, "job": Value::Null});
    }
    let jid = next_id() as i64;
    job_registry().lock().unwrap().insert(jid, StructJob {
        argv, base, cur, head: lines.clone(),
    });
    json!({"lines": lines, "job": jid})
}

/// SLOW half: run the sandboxed dump(s) for `job` and build the unified
/// structural diff. Returns {"lines": [...]}. Never panics.
pub fn struct_finish(job_id: i64) -> Value {
    let Some(job) = job_registry().lock().unwrap().remove(&job_id) else {
        return json!({"lines": [["err", "unknown struct job"]]});
    };
    let mut lines = job.head.clone();
    let dump = |data: &[u8]| -> String {
        if data.is_empty() {
            return String::new();
        }
        match run_on_untrusted(&job.argv, data) {
            Ok(out) => out,
            Err(e) => format!("<parser error: {e}>"),
        }
    };
    if !job.base.is_empty() && !job.cur.is_empty() {
        let bd = dump(&job.base);
        let cd = dump(&job.cur);
        let diff = TextDiff::from_lines(&bd, &cd);
        let bl: Vec<&str> = diff.iter_old_slices()
            .map(|s| s.trim_end_matches(['\r', '\n'])).collect();
        let cl: Vec<&str> = diff.iter_new_slices()
            .map(|s| s.trim_end_matches(['\r', '\n'])).collect();
        let mut any = false;
        for group in diff.grouped_ops(3) {
            if group.is_empty() { continue; }
            let (_, a0, _) = group[0].as_tag_tuple();
            let (_, alast, blast) = group[group.len() - 1].as_tag_tuple();
            let (_, _, b0) = group[0].as_tag_tuple();
            lines.push(pair("@", &format!("@@ -{},{} +{},{} @@",
                a0.start + 1, alast.end - a0.start, b0.start + 1, blast.end - b0.start)));
            any = true;
            for op in &group {
                let (tag, orange, nrange) = op.as_tag_tuple();
                match tag {
                    DiffTag::Equal => for k in orange { lines.push(pair(" ", bl[k])); },
                    _ => {
                        for k in orange { lines.push(pair("-", bl[k])); }
                        for k in nrange { lines.push(pair("+", cl[k])); }
                    }
                }
            }
        }
        if !any {
            lines.push(pair("dim", "(structural dumps identical)"));
        }
    } else {
        let which_side = if job.cur.is_empty() { "base" } else { "current" };
        lines.push(pair("dim", &format!("({which_side} only)")));
        let data = if job.cur.is_empty() { &job.base } else { &job.cur };
        for ln in dump(data).split('\n') {
            lines.push(pair(" ", ln.trim_end_matches('\r')));
        }
    }
    json!({"lines": lines})
}

pub fn struct_cancel(job_id: i64) {
    job_registry().lock().unwrap().remove(&job_id);
}

/// Write `data` to a uniquely-named scratch file under the system temp dir.
fn scratch_file(tag: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir();
    let p = dir.join(format!("sarun-ut-{tag}-{}-{}", std::process::id(), next_id()));
    std::fs::write(&p, data)?;
    Ok(p)
}

/// Run `argv` (with a {in} placeholder) over untrusted `data` inside a throwaway
/// bwrap, as the Python run_on_untrusted does: the bytes go to a temp dir that
/// is ro-bound into a `--unshare-*` / `--cap-drop ALL` / `--die-with-parent`
/// sandbox with `/` mounted read-only, and {in} resolves to the path inside.
/// If bwrap is unavailable, runs the differ directly on the host temp file
/// (noted in any error). Output is capped at 256 KiB. Never panics.
fn run_on_untrusted(argv: &[String], data: &[u8]) -> Result<String, String> {
    // A dedicated dir so we can ro-bind exactly the input into the sandbox.
    let dir = std::env::temp_dir()
        .join(format!("sarun-utd-{}-{}", std::process::id(), next_id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let host_in = dir.join("in");
    let res = (|| {
        std::fs::write(&host_in, data).map_err(|e| e.to_string())?;
        let inside_dir = "/tmp/ut";
        let inside_in = format!("{inside_dir}/in");
        let is_in = |a: &str| a.starts_with('{') && a.ends_with('}')
            && &a[1..a.len() - 1] == "in";
        let out = if which("bwrap") {
            let mut cmd = std::process::Command::new("bwrap");
            cmd.args(["--unshare-pid", "--unshare-ipc", "--unshare-uts",
                      "--unshare-net", "--die-with-parent", "--new-session",
                      "--cap-drop", "ALL", "--ro-bind", "/", "/",
                      "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
            cmd.arg("--ro-bind").arg(&dir).arg(inside_dir);
            cmd.args(["--chdir", inside_dir, "--clearenv",
                      "--setenv", "PATH", SANDBOX_PATH, "--"]);
            cmd.args(argv.iter().map(|a| if is_in(a) { inside_in.clone() }
                                       else { a.clone() }));
            cmd.stdin(std::process::Stdio::null());
            cmd.output().map_err(|e| format!("spawn failed: {e}"))?
        } else {
            let real: Vec<String> = argv.iter().map(|a| if is_in(a) {
                host_in.to_string_lossy().into_owned() } else { a.clone() }).collect();
            std::process::Command::new(&real[0]).args(&real[1..])
                .stdin(std::process::Stdio::null())
                .output().map_err(|e| format!("spawn failed (no bwrap): {e}"))?
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let capped: String = stdout.chars().take(256 * 1024).collect();
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let msg: String = err.trim().chars().take(2000).collect();
            return Err(if msg.is_empty() {
                format!("exit {:?}", out.status.code()) } else { msg });
        }
        Ok(capped)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    res
}

fn which(prog: &str) -> bool {
    std::env::var_os("PATH").map(|paths| {
        std::env::split_paths(&paths).any(|p| p.join(prog).is_file())
    }).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{BoxState, Entry};

    /// The unified write path: promoting a change into a LIVE parent box writes
    /// the parent's authoritative sqlar AND refreshes its in-RAM `kinds` mirror,
    /// so a running FUSE mount serves the promoted file without caring that the
    /// write came from review rather than the box's own handler. (Audit: overlay
    /// read/write must not behave differently based on whether a process runs.)
    #[test]
    fn promote_into_live_parent_refreshes_the_ram_mirror() {
        let tmp = std::env::temp_dir()
            .join(format!("sarun-promote-{}-{:?}", std::process::id(),
                          std::time::SystemTime::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: state_home() is derived from XDG_STATE_HOME; no concurrent test
        // in this binary reads it (the others use temp_dir()).
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp); }
        std::fs::create_dir_all(crate::paths::state_home()).unwrap();

        let (parent_id, child_id) = (9001, 9002);
        let parent = BoxState::create(parent_id).unwrap(); // the LIVE destination
        let child = BoxState::create(child_id).unwrap();

        // Child captures a regular-file change: foo.txt = "hi".
        let rid = child.ensure_file_row("foo.txt", 0o100644, 0);
        let cblob = crate::capture::blob_path(child_id, rid);
        std::fs::create_dir_all(cblob.parent().unwrap()).unwrap();
        std::fs::write(&cblob, b"hi").unwrap();
        child.finalize_file("foo.txt", 2, 0, 0);

        assert!(parent.entry("foo.txt").is_none(), "precondition: parent empty");

        promote_into_parent(child_id, parent_id, Some(&parent), "foo.txt").unwrap();

        // sqlar got the row...
        let mode: i64 = {
            let c = parent.conn.lock().unwrap();
            c.query_row("SELECT mode FROM sqlar WHERE name='foo.txt'", [], |r| r.get(0))
                .expect("row in parent sqlar")
        };
        assert_eq!(mode as u32 & S_IFMT, 0o100000, "promoted as a regular file");

        // ...AND the live mirror was refreshed with the right rowid + bytes.
        match parent.entry("foo.txt") {
            Some(Entry::File { rowid, .. }) => {
                let pblob = crate::capture::blob_path(parent_id, rowid);
                assert_eq!(std::fs::read(&pblob).unwrap(), b"hi", "promoted blob copied");
            }
            Some(_) => panic!("live parent mirror has wrong entry kind after promote"),
            None => panic!("live parent mirror NOT refreshed by promote (still absent)"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
