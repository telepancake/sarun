// Review-view verbs in Rust: session_changes (list a box's changes) and hunks
// (unified text diff of lower vs captured). Read-only against the box's
// on-disk sqlar (a fresh RO connection coexists with a live box's writer), so
// these serve both live and finished boxes. Output shapes match the Python
// ChangeReview exactly (the UI and the conformance readers depend on it).
// apply/discard (host-mutating, need live-connection ownership routing) and
// the structural-diff job path are deferred to a later milestone.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use base64::Engine;
use rusqlite::Connection;
use rusqlite::OpenFlags;
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

// ── host-mutating review actions (top-level boxes; nested promotion deferred) ──
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn open_rw(id: i64) -> Option<Connection> {
    let c = Connection::open(sqlar_path(id)).ok()?;
    c.busy_timeout(Duration::from_secs(3)).ok()?;
    Some(c)
}

fn row_of(conn: &Connection, rel: &str) -> Option<(i64, u32, Option<Vec<u8>>)> {
    conn.query_row("SELECT rowid,mode,data FROM sqlar WHERE name=?1", [rel],
                   |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32, r.get(2)?))).ok()
}

fn consume(conn: &Connection, id: i64, rel: &str, rowid: i64) {
    let _ = conn.execute("DELETE FROM sqlar WHERE name=?1", [rel]);
    let _ = std::fs::remove_file(blob_path(id, rowid));
}

fn materialize(conn: &Connection, id: i64, rel: &str) -> Result<(), String> {
    let (rowid, mode, data) = row_of(conn, rel).ok_or("not in archive")?;
    let host = Path::new("/").join(rel);
    let is_symlink = host.symlink_metadata().map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if mode & S_IFMT == S_IFCHR {
        if host.is_dir() && !is_symlink {
            std::fs::remove_dir_all(&host).map_err(|e| e.to_string())?;
        } else if host.exists() || is_symlink {
            std::fs::remove_file(&host).map_err(|e| e.to_string())?;
        }
    } else if mode & S_IFMT == S_IFLNK {
        let tgt = data.ok_or("symlink row has no target")?;
        if host.exists() || is_symlink { let _ = std::fs::remove_file(&host); }
        if let Some(p) = host.parent() { let _ = std::fs::create_dir_all(p); }
        let t = std::ffi::OsStr::from_bytes(&tgt);
        std::os::unix::fs::symlink(t, &host).map_err(|e| e.to_string())?;
    } else if mode & S_IFMT == 0o040000 {
        std::fs::create_dir_all(&host).map_err(|e| e.to_string())?;
    } else {
        if is_symlink { return Err("refusing to write through a symlink".into()); }
        if let Some(p) = host.parent() { let _ = std::fs::create_dir_all(p); }
        let bytes = match data {
            Some(d) => d,
            None => std::fs::read(blob_path(id, rowid)).map_err(|e| e.to_string())?,
        };
        std::fs::write(&host, &bytes).map_err(|e| e.to_string())?;
        let _ = std::fs::set_permissions(&host,
            std::fs::Permissions::from_mode(mode & 0o7777));
    }
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

pub fn apply(id: i64, paths: &Value) -> Value {
    let Some(conn) = open_rw(id) else {
        return json!({"applied": [], "errors": [{"path": "", "error": "no archive"}]});
    };
    let mut applied = vec![];
    let mut errors = vec![];
    for rel in paths_arg(id, paths) {
        let rel = rel.trim_start_matches('/').to_string();
        match materialize(&conn, id, &rel) {
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

pub fn discard(id: i64, paths: &Value) -> Value {
    let mut discarded = vec![];
    if let Some(conn) = open_rw(id) {
        for rel in paths_arg(id, paths) {
            let rel = rel.trim_start_matches('/').to_string();
            if let Some((rowid, _, _)) = row_of(&conn, &rel) {
                consume(&conn, id, &rel, rowid);
                discarded.push(Value::String(rel));
            }
        }
    }
    json!({"discarded": discarded, "errors": []})
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
