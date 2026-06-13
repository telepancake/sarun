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

const S_IFIFO: u32 = 0o010000;
const S_IFBLK: u32 = 0o060000;

fn restore_metadata(conn: &Connection, rel: &str, host: &Path, mtime_ns: i64) {
    let c = std::ffi::CString::new(host.as_os_str().as_bytes()).unwrap();
    // mtime (atime = mtime): drives downstream make/rebuild decisions.
    if mtime_ns > 0 {
        let ts = libc::timespec {
            tv_sec: mtime_ns.div_euclid(1_000_000_000),
            tv_nsec: mtime_ns.rem_euclid(1_000_000_000),
        };
        let times = [ts, ts];
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(),
                                 libc::AT_SYMLINK_NOFOLLOW); }
    }
    // owner (best-effort; lchown EPERMs for an unprivileged host user — ignored).
    if let Ok((uid, gid)) = conn.query_row(
        "SELECT uid,gid FROM ownership WHERE name=?1", [rel],
        |r| Ok((r.get::<_,i64>(0)? as u32, r.get::<_,i64>(1)? as u32))) {
        unsafe { libc::lchown(c.as_ptr(), uid, gid); }
    }
    // xattrs.
    if let Ok(mut st) = conn.prepare("SELECT key,value FROM xattr WHERE name=?1") {
        if let Ok(rows) = st.query_map([rel], |r|
            Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?))) {
            for (k, v) in rows.flatten() {
                if let Ok(ck) = std::ffi::CString::new(k) {
                    unsafe { libc::lsetxattr(c.as_ptr(), ck.as_ptr(),
                        v.as_ptr().cast(), v.len(), 0); }
                }
            }
        }
    }
}

fn materialize(conn: &Connection, id: i64, rel: &str) -> Result<(), String> {
    let (rowid, mode, data) = row_of(conn, rel).ok_or("not in archive")?;
    let mtime_ns: i64 = conn.query_row("SELECT mtime FROM sqlar WHERE name=?1", [rel],
                                       |r| r.get(0)).unwrap_or(0);
    let host = Path::new("/").join(rel);
    let is_symlink = host.symlink_metadata().map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if mode & S_IFMT == S_IFCHR {
        // char-device row == deletion tombstone (the Python convention).
        if host.is_dir() && !is_symlink {
            std::fs::remove_dir_all(&host).map_err(|e| e.to_string())?;
        } else if host.exists() || is_symlink {
            std::fs::remove_file(&host).map_err(|e| e.to_string())?;
        }
        return Ok(());
    } else if mode & S_IFMT == S_IFLNK {
        let tgt = data.ok_or("symlink row has no target")?;
        if host.exists() || is_symlink { let _ = std::fs::remove_file(&host); }
        if let Some(p) = host.parent() { let _ = std::fs::create_dir_all(p); }
        let t = std::ffi::OsStr::from_bytes(&tgt);
        std::os::unix::fs::symlink(t, &host).map_err(|e| e.to_string())?;
    } else if mode & S_IFMT == 0o040000 {
        std::fs::create_dir_all(&host).map_err(|e| e.to_string())?;
        let _ = std::fs::set_permissions(&host,
            std::fs::Permissions::from_mode(mode & 0o7777));
    } else if mode & S_IFMT == S_IFIFO || mode & S_IFMT == S_IFBLK {
        // fifo / block device: recreate the node on the host.
        if host.exists() || is_symlink { let _ = std::fs::remove_file(&host); }
        if let Some(p) = host.parent() { let _ = std::fs::create_dir_all(p); }
        let rdev: i64 = conn.query_row("SELECT dev FROM rdev WHERE name=?1", [rel],
                                       |r| r.get(0)).unwrap_or(0);
        let c = std::ffi::CString::new(host.as_os_str().as_bytes()).unwrap();
        if unsafe { libc::mknod(c.as_ptr(), mode, rdev as libc::dev_t) } != 0 {
            return Err("mknod failed".into());
        }
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
    restore_metadata(conn, rel, &host, mtime_ns);
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
