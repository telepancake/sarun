// Review-view verbs in Rust: session_changes (list a box's changes) and hunks
// (unified text diff of lower vs captured). Read-only against the box's
// on-disk sqlar (a fresh RO connection coexists with a live box's writer), so
// these serve both live and finished boxes. Output shapes match the Python
// ChangeReview exactly (the UI and the conformance readers depend on it).
// apply/discard (host-mutating, need live-connection ownership routing) and
// the structural-diff job path are deferred to a later milestone.

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
