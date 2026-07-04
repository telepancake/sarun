// On-disk box discovery — the Rust counterpart of the Python engine's
// discover_sessions(): every <box_id>.sqlar under state_home plus every
// live/<box_id> backing dir IS a box. Each box's full sqlar `meta` table and
// the root-process argv are read ONCE here (read_box) into Box_; everything
// else in the engine reads box meta from Box_.meta or the box_meta() one-off,
// never by opening a sqlar itself. Read-only.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;

use crate::paths;

pub struct Box_ {
    pub box_id: i64,
    pub name: String,
    pub parent: Option<i64>,
    pub cmd: Vec<String>,
    pub started: f64,
    pub has_sqlar: bool,
    /// The box's full sqlar `meta` table (key→value), read once at discovery.
    /// THE in-memory copy of a box's meta: callers read `oci_reference`,
    /// `oci_layer_index`, `no_host_fallback`, `oci_config`, etc. from here
    /// rather than re-opening the sqlar. `name`/`parent` above are just the two
    /// hottest keys hoisted out of this map for convenience.
    pub meta: HashMap<String, String>,
}

fn sqlar_path(box_id: i64) -> std::path::PathBuf {
    paths::state_home().join(format!("{box_id}.sqlar"))
}

/// THE single place anything opens a box's at-rest sqlar to read state: its full
/// `meta` table plus the root-process argv, in one connection. `discover()`
/// fills Box_ from it; `box_meta()` is the one-off shorthand.
fn read_box(box_id: i64) -> (HashMap<String, String>, Vec<String>) {
    let mut meta = HashMap::new();
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &sqlar_path(box_id), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return (meta, vec![]);
    };
    if let Ok(mut st) = conn.prepare("SELECT key, value FROM meta") {
        if let Ok(rows) = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for kv in rows.flatten() { meta.insert(kv.0, kv.1); }
        }
    }
    let cmd: Vec<String> = conn
        .query_row("SELECT argv FROM process WHERE root=1 ORDER BY id LIMIT 1",
                   [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    (meta, cmd)
}

/// One-off read of a single box's `meta` map (for callers that don't already
/// hold the discover() snapshot, e.g. a build seeding config from a base box).
/// Goes through the same `read_box` reader, so there is exactly one code path
/// that touches a box sqlar's meta.
pub fn box_meta(box_id: i64) -> HashMap<String, String> {
    read_box(box_id).0
}

fn ctime_of(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.created().or_else(|_| m.modified()))
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn discover() -> BTreeMap<i64, Box_> {
    let mut out = BTreeMap::new();
    let sh = paths::state_home();
    if let Ok(rd) = std::fs::read_dir(&sh) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("sqlar") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str())
                .and_then(|s| s.parse::<i64>().ok()) else { continue };
            let (meta, cmd) = read_box(id);
            let name = meta.get("name").cloned().unwrap_or_default();
            let parent = meta.get("parent_box_id").and_then(|v| v.parse().ok());
            out.insert(id, Box_ {
                box_id: id, name, parent, cmd,
                started: ctime_of(&p), has_sqlar: true, meta,
            });
        }
    }
    if let Ok(rd) = std::fs::read_dir(paths::live_home()) {
        for ent in rd.flatten() {
            let Some(id) = ent.file_name().to_str()
                .and_then(|s| s.parse::<i64>().ok()) else { continue };
            out.entry(id).or_insert_with(|| Box_ {
                box_id: id, name: String::new(), parent: None, cmd: vec![],
                started: ctime_of(&ent.path()), has_sqlar: false,
                meta: HashMap::new(),
            });
        }
    }
    out
}

pub fn display_path(boxes: &BTreeMap<i64, Box_>, box_id: i64) -> String {
    let mut parts = vec![];
    let mut cur = Some(box_id);
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cur {
        if !seen.insert(id) || parts.len() > 64 {
            break;
        }
        match boxes.get(&id) {
            Some(b) => {
                parts.push(if b.name.is_empty() { id.to_string() }
                           else { b.name.clone() });
                cur = b.parent;
            }
            None => {
                parts.push(id.to_string());
                cur = None;
            }
        }
    }
    parts.reverse();
    parts.join(".")
}

pub fn session_dict(boxes: &BTreeMap<i64, Box_>, b: &Box_) -> Value {
    json!({
        "session_id": b.box_id.to_string(),
        "cmd": b.cmd,
        "shm_dir": paths::live_home().join(b.box_id.to_string()).to_string_lossy(),
        "killed": false,
        "errored": false,
        "exit_code": Value::Null,
        "live": false,
        "has_sqlar": b.has_sqlar,
        "box_id": b.box_id,
        "name": b.name,
        "run_pid": 0,
        "run_pidfd": -1,
        "parent_box_id": b.parent,
        "started": b.started,
        "pid": 0,
        "status": "finished",
        "upper": paths::live_home().join(b.box_id.to_string()).join("up")
                 .to_string_lossy(),
        "path": display_path(boxes, b.box_id),
    })
}

/// True if `table` has a column named `col` (PRAGMA table_info). Used so the
/// Rust-engine-only columns degrade gracefully on a Python-written sqlar.
fn has_col(conn: &rusqlite::Connection, table: &str, col: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut st| {
            let it = st.query_map([], |r| r.get::<_, String>(1))?;
            Ok(it.flatten().any(|c| c == col))
        }).unwrap_or(false)
}

fn has_table(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
        .and_then(|mut st| Ok(st.exists([table])?))
        .unwrap_or(false)
}

pub fn processes(box_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return json!([]);
    };
    let mut rows = vec![];
    // brush_pipeline_id is a Rust-engine column; a Python-written sqlar lacks it.
    // COALESCE over a guarded expression keeps element 6 present (NULL) either way
    // without failing the whole read. Element 6 is ADDITIVE — existing consumers
    // index 0..5 only — so it is backward-compatible.
    let col = if has_col(&conn, "process", "brush_pipeline_id")
              { "brush_pipeline_id" } else { "NULL" };
    let q = format!(
        "SELECT id,tgid,ppid,parent_id,exe,argv,{col} FROM process ORDER BY id");
    if let Ok(mut st) = conn.prepare(&q) {
        let it = st.query_map([], |r| {
            let argv: Option<String> = r.get(5)?;
            Ok(json!([
                r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                argv.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or_else(|| json!([])),
                r.get::<_, Option<i64>>(6)?,
            ]))
        });
        if let Ok(it) = it {
            for row in it.flatten() {
                rows.push(row);
            }
        }
    }
    Value::Array(rows)
}

/// oaita-proxy API log rows for a box: one row per request the engine
/// forwarded on this box's behalf. The body bytes are summary-sized (lengths
/// only) here; `api_log_detail` fetches the full request/response on demand.
pub fn api_log(box_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return json!([]);
    };
    let mut rows = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT id,ts,method,path,model,status,stream,length(req),length(resp) \
         FROM api_log ORDER BY id") {
        let it = st.query_map([], |r| Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "ts": r.get::<_, f64>(1)?,
            "method": r.get::<_, String>(2)?,
            "path": r.get::<_, String>(3)?,
            "model": r.get::<_, String>(4)?,
            "status": r.get::<_, i64>(5)?,
            "stream": r.get::<_, i64>(6)?,
            "req_len": r.get::<_, i64>(7)?,
            "resp_len": r.get::<_, i64>(8)?,
        })));
        if let Ok(it) = it { for row in it.flatten() { rows.push(row); } }
    }
    Value::Array(rows)
}

/// Full request/response payloads for one api_log row.
pub fn api_log_detail(box_id: i64, row_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Value::Null;
    };
    let row = conn.query_row(
        "SELECT id,ts,method,path,model,status,stream,req,resp \
         FROM api_log WHERE id=?1",
        [row_id],
        |r| {
            let req: Vec<u8> = r.get(7)?;
            let resp: Vec<u8> = r.get(8)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
                "method": r.get::<_, String>(2)?,
                "path": r.get::<_, String>(3)?,
                "model": r.get::<_, String>(4)?,
                "status": r.get::<_, i64>(5)?,
                "stream": r.get::<_, i64>(6)?,
                "req": String::from_utf8_lossy(&req),
                "resp": String::from_utf8_lossy(&resp),
            }))
        }
    );
    row.unwrap_or(Value::Null)
}

/// Web capture rows for a box (DESIGN-web.md W1/W4): one row per HTTP(S)
/// request/response the tap MITM proxy teed. Summary-sized here (body lengths
/// only); `webcap_detail` fetches full headers + bodies on demand. Newest
/// first (DESC) — the browser/crawler produce these in time order and the
/// most recent captures are what the Captures pane wants at the top. A sqlar
/// written before the webcap table existed simply yields no rows.
pub fn webcap(box_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return json!([]);
    };
    let mut rows = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT id,ts,method,url,host,status,mime,truncated,\
         length(req_body),length(resp_body) FROM webcap ORDER BY id DESC") {
        let it = st.query_map([], |r| Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "ts": r.get::<_, f64>(1)?,
            "method": r.get::<_, String>(2)?,
            "url": r.get::<_, String>(3)?,
            "host": r.get::<_, String>(4)?,
            "status": r.get::<_, i64>(5)?,
            "mime": r.get::<_, String>(6)?,
            "truncated": r.get::<_, i64>(7)?,
            "req_len": r.get::<_, i64>(8)?,
            "resp_len": r.get::<_, i64>(9)?,
        })));
        if let Ok(it) = it { for row in it.flatten() { rows.push(row); } }
    }
    Value::Array(rows)
}

/// Full headers + bodies for one webcap row. The response body is returned
/// BOTH raw-lossy (for binary inspection) and, when it decodes to text,
/// identity-decoded via the recorded Content-Encoding (DESIGN-web.md W2).
pub fn webcap_detail(box_id: i64, row_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Value::Null;
    };
    let row = conn.query_row(
        "SELECT id,ts,method,url,host,status,mime,req_headers,resp_headers,\
         req_body,resp_body,truncated FROM webcap WHERE id=?1",
        [row_id],
        |r| {
            let resp_headers: String = r.get(8)?;
            let req_body: Vec<u8> = r.get(9)?;
            let resp_body: Vec<u8> = r.get(10)?;
            let decoded = crate::net::webcap::decode_body(&resp_headers, &resp_body);
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
                "method": r.get::<_, String>(2)?,
                "url": r.get::<_, String>(3)?,
                "host": r.get::<_, String>(4)?,
                "status": r.get::<_, i64>(5)?,
                "mime": r.get::<_, String>(6)?,
                "req_headers": r.get::<_, String>(7)?,
                "resp_headers": resp_headers,
                "req_body": String::from_utf8_lossy(&req_body),
                "resp_body": String::from_utf8_lossy(&decoded),
                "truncated": r.get::<_, i64>(11)?,
            }))
        }
    );
    row.unwrap_or(Value::Null)
}

pub fn outputs(box_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return json!([]);
    };
    let mut rows = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT id,ts,process_id,stream,length(content) FROM outputs ORDER BY id") {
        let it = st.query_map([], |r| Ok(json!({
            "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
            "process_id": r.get::<_, Option<i64>>(2)?,
            "stream": r.get::<_, i64>(3)?, "len": r.get::<_, i64>(4)?,
        })));
        if let Ok(it) = it { for row in it.flatten() { rows.push(row); } }
    }
    Value::Array(rows)
}

/// D9 brush-shell semantic provenance rows for a box: each is one pipeline the
/// embedded brush shell (-b) ran, with its exact command string and the parsed
/// pipeline/redirect structure. Empty for boxes not run with -b.
pub fn brushprov(box_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return json!([]);
    };
    let mut rows = vec![];
    // `nested` is a Rust-engine column (D9 follow-on); a sqlar written before it
    // existed (or by Python) lacks it — select 0 in that case so old archives
    // still read.
    let nested_col = if has_col(&conn, "brushprov", "nested")
                     { "nested" } else { "0" };
    // uid/parent_uid are newer Rust-engine columns (pipeline-tree nesting); a
    // sqlar written before them selects 0 so old archives still read flat.
    let uid_col = if has_col(&conn, "brushprov", "uid") { "uid" } else { "0" };
    let puid_col = if has_col(&conn, "brushprov", "parent_uid") { "parent_uid" } else { "0" };
    let done_col = if has_col(&conn, "brushprov", "done_ts") { "done_ts" } else { "0" };
    let ec_col = if has_col(&conn, "brushprov", "exit_code") { "exit_code" } else { "-1" };
    if let Ok(mut st) = conn.prepare(&format!(
        "SELECT id,ts,cmd,record,pipeline,spawn_ts,{nested_col},{uid_col},{puid_col},\
         {done_col},{ec_col} FROM brushprov ORDER BY id")) {
        let it = st.query_map([], |r| {
            let rec: String = r.get(3)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
                "cmd": r.get::<_, String>(2)?,
                "record": serde_json::from_str::<Value>(&rec).unwrap_or(Value::Null),
                "pipeline": r.get::<_, Option<i64>>(4)?,
                "spawn_ts": r.get::<_, Option<f64>>(5)?,
                "nested": r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
                "uid": r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                "parent_uid": r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                "done_ts": r.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                "exit_code": r.get::<_, Option<i64>>(10)?.unwrap_or(-1),
            }))
        });
        if let Ok(it) = it { for row in it.flatten() { rows.push(row); } }
    }
    // Attach the process row ids each pipeline spawned (the D9 brush↔process
    // linkage, pipeline→processes direction). One extra grouped query, joined in.
    if let Ok(mut st) = conn.prepare(
        "SELECT brush_pipeline_id,id FROM process \
         WHERE brush_pipeline_id IS NOT NULL ORDER BY id") {
        let mut by_pl: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
        if let Ok(it) = st.query_map([], |r| Ok((
            r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))) {
            for (pl, pid) in it.flatten() {
                by_pl.entry(pl).or_default().push(Value::from(pid));
            }
        }
        for row in rows.iter_mut() {
            if let Some(id) = row.get("id").and_then(Value::as_i64) {
                let procs = by_pl.remove(&id).unwrap_or_default();
                row["processes"] = Value::Array(procs);
            }
        }
    }
    Value::Array(rows)
}

/// Phase 1 embedded-ninja: the parsed build-graph edges captured when the box's
/// `ninja` (vendored n2 in-process) loaded build.ninja. Each row is one edge
/// {outs, ins, cmd}, INCLUDING up-to-date targets that never executed. Empty for
/// boxes that never ran ninja (or whose sqlar predates the build_edges table).
pub fn build_edges(box_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return json!([]);
    };
    if !has_table(&conn, "build_edges") { return json!([]); }
    // The execution columns (started_ts / ended_ts / exit_code /
    // output_excerpt) were added later; old boxes that ran before
    // this schema rev don't have them. Pick a SELECT list that
    // tolerates either layout (the COALESCE-to-NULL is on the
    // missing columns; SQLite returns NULL for columns absent in
    // the SELECT result of an old table). Probe once.
    let has_exec = has_col(&conn, "build_edges", "started_ts");
    let mut rows = vec![];
    let sql = if has_exec {
        "SELECT id, ts, outs, ins, cmd, \
                started_ts, ended_ts, exit_code, output_excerpt \
         FROM build_edges ORDER BY id"
    } else {
        "SELECT id, ts, outs, ins, cmd, \
                NULL, NULL, NULL, NULL \
         FROM build_edges ORDER BY id"
    };
    if let Ok(mut st) = conn.prepare(sql) {
        let it = st.query_map([], |r| {
            let outs: String = r.get(2)?;
            let ins: String = r.get(3)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, f64>(1)?,
                "outs": serde_json::from_str::<Value>(&outs).unwrap_or(json!([])),
                "ins": serde_json::from_str::<Value>(&ins).unwrap_or(json!([])),
                "cmd": r.get::<_, Option<String>>(4)?,
                "started_ts":  r.get::<_, Option<f64>>(5)?,
                "ended_ts":    r.get::<_, Option<f64>>(6)?,
                "exit_code":   r.get::<_, Option<i64>>(7)?,
                "output_excerpt": r.get::<_, Option<String>>(8)?,
            }))
        });
        if let Ok(it) = it { for row in it.flatten() { rows.push(row); } }
    }
    Value::Array(rows)
}

/// D9 brush↔process linkage, process→pipeline direction: the brushprov pipeline
/// row that spawned process `row_id` (its exact cmd + parsed structure), or Null
/// if that process was not spawned by a brush pipeline (or the box isn't -b).
/// Walks up the process tree (parent_id) when the direct process lacks a
/// brush_pipeline_id — output writers are often grandchildren of the pipeline
/// command (e.g. `rm` forked by a shell forked by make).
pub fn proc_pipeline(box_id: i64, row_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return Value::Null };
    let mut cur = row_id;
    for _ in 0..64 {
        if let Ok(v) = c.query_row(
            "SELECT bp.id,bp.ts,bp.cmd,bp.record,bp.pipeline \
             FROM process p JOIN brushprov bp ON p.brush_pipeline_id=bp.id \
             WHERE p.id=?1",
            [cur], |r| {
                let rec: String = r.get(3)?;
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
                    "cmd": r.get::<_, String>(2)?,
                    "record": serde_json::from_str::<Value>(&rec).unwrap_or(Value::Null),
                    "pipeline": r.get::<_, Option<i64>>(4)?,
                }))
            }) {
            return v;
        }
        match c.query_row(
            "SELECT parent_id FROM process WHERE id=?1", [cur],
            |r| r.get::<_, Option<i64>>(0)) {
            Ok(Some(pid)) if pid != cur => cur = pid,
            _ => break,
        }
    }
    Value::Null
}

/// Output→pipeline: find the brushprov pipeline that produced output `output_id`.
/// Uses the output's own brush_pipeline_id column (stamped at capture time).
/// Falls back to the process→pipeline parent walk ONLY when the column does
/// not exist (pre-column data). When the column exists but is NULL, the output
/// genuinely has no pipeline — guessing via process ancestry produces wrong
/// results for in-process builtins whose process_id is the shared brush root.
pub fn output_pipeline(box_id: i64, output_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return Value::Null };
    let has_bp_col = has_col(&c, "outputs", "brush_pipeline_id");
    let bp_col = if has_bp_col { "brush_pipeline_id" } else { "NULL" };
    let q = format!("SELECT process_id,{bp_col} FROM outputs WHERE id=?1");
    let (process_id, bp_id): (Option<i64>, Option<i64>) = match c.query_row(
        &q, [output_id], |r| Ok((r.get(0)?, r.get(1)?))) {
        Ok(v) => v,
        Err(_) => return Value::Null,
    };
    if let Some(bp) = bp_id {
        if let Ok(v) = c.query_row(
            "SELECT id,ts,cmd,record,pipeline FROM brushprov WHERE id=?1",
            [bp], |r| {
                let rec: String = r.get(3)?;
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
                    "cmd": r.get::<_, String>(2)?,
                    "record": serde_json::from_str::<Value>(&rec).unwrap_or(Value::Null),
                    "pipeline": r.get::<_, Option<i64>>(4)?,
                }))
            }) {
            return v;
        }
    }
    // Only fall back to process-tree walk when the column doesn't exist at
    // all (old data). If the column exists but is NULL, the output was not
    // attributed to any pipeline at capture time — don't guess.
    if !has_bp_col {
        if let Some(pid) = process_id {
            let v = proc_pipeline(box_id, pid);
            if !v.is_null() { return v; }
        }
    }
    Value::Null
}

/// D9 brush↔process linkage, pipeline→processes direction: the process row ids
/// the brushprov pipeline `brushprov_id` spawned (empty if none/unknown).
pub fn pipeline_procs(box_id: i64, brushprov_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return json!([]) };
    let mut out = vec![];
    if let Ok(mut st) = c.prepare(
        "SELECT id FROM process WHERE brush_pipeline_id=?1 ORDER BY id") {
        if let Ok(it) = st.query_map([brushprov_id], |r| r.get::<_, i64>(0)) {
            out = it.flatten().map(Value::from).collect();
        }
    }
    Value::Array(out)
}

pub fn open_ro_for(box_id: i64) -> Option<rusqlite::Connection> { open_ro(box_id) }

fn open_ro(box_id: i64) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        sqlar_path(box_id), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// (tgid, ppid, parent_id, exe, argv) for one process row — the proc-tree
/// connector resolver. None if the row id isn't recorded.
pub fn proc_info(box_id: i64, row_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return Value::Null };
    c.query_row("SELECT tgid,ppid,parent_id,exe,argv FROM process WHERE id=?1",
                [row_id], |r| {
        let argv: Option<String> = r.get(4)?;
        Ok(json!([r.get::<_,Option<i64>>(0)?, r.get::<_,Option<i64>>(1)?,
                  r.get::<_,Option<i64>>(2)?,
                  r.get::<_,Option<String>>(3)?.unwrap_or_default(),
                  argv.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                      .unwrap_or_else(|| json!([]))]))
    }).unwrap_or(Value::Null)
}

/// Provenance dict {exe,cwd,argv} of one process row — the procs-pane filter.
pub fn proc_prov(box_id: i64, row_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return Value::Null };
    c.query_row("SELECT exe,cwd,argv FROM process WHERE id=?1", [row_id], |r| {
        let argv: Option<String> = r.get(2)?;
        Ok(json!({"exe": r.get::<_,Option<String>>(0)?.unwrap_or_default(),
                  "cwd": r.get::<_,Option<String>>(1)?.unwrap_or_default(),
                  "argv": argv.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                      .unwrap_or_else(|| json!([]))}))
    }).unwrap_or(Value::Null)
}

/// Hierarchy-root row ids (process.root=1) — the proc-tree walk boundary.
pub fn proc_roots(box_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return json!([]) };
    let mut out = vec![];
    if let Ok(mut st) = c.prepare("SELECT id FROM process WHERE root=1") {
        if let Ok(it) = st.query_map([], |r| r.get::<_, i64>(0)) {
            out = it.flatten().map(Value::from).collect();
        }
    }
    Value::Array(out)
}

fn writer_col(box_id: i64, rel: &str, col: &str) -> Value {
    let Some(c) = open_ro(box_id) else { return Value::Null };
    let rel = rel.trim_start_matches('/');
    c.query_row(&format!("SELECT {col} FROM sqlar WHERE name=?1"), [rel],
                |r| r.get::<_, Option<i64>>(0))
        .ok().flatten().map(Value::from).unwrap_or(Value::Null)
}
pub fn writer_id(box_id: i64, rel: &str) -> Value { writer_col(box_id, rel, "last_writer") }
pub fn first_writer_id(box_id: i64, rel: &str) -> Value { writer_col(box_id, rel, "writer") }

/// Provenance {pid,ppid,exe,cwd,argv} of the FIRST writer of `rel`.
pub fn first_writer_prov(box_id: i64, rel: &str) -> Value {
    let Some(c) = open_ro(box_id) else { return Value::Null };
    let rel = rel.trim_start_matches('/');
    c.query_row("SELECT process.tgid,process.ppid,process.exe,process.cwd,process.argv \
                 FROM sqlar JOIN process ON sqlar.writer=process.id WHERE sqlar.name=?1",
                [rel], |r| {
        let argv: Option<String> = r.get(4)?;
        Ok(json!({"pid": r.get::<_,Option<i64>>(0)?, "ppid": r.get::<_,Option<i64>>(1)?,
                  "exe": r.get::<_,Option<String>>(2)?.unwrap_or_default(),
                  "cwd": r.get::<_,Option<String>>(3)?.unwrap_or_default(),
                  "argv": argv.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                      .unwrap_or_else(|| json!([]))}))
    }).unwrap_or(Value::Null)
}

/// One captured output row WITH its content (bytes → {"__b": base64} so the
/// Python wire_decode hands the UI real bytes).
pub fn output_detail(box_id: i64, oid: i64) -> Value {
    use base64::Engine as _;
    let Some(c) = open_ro(box_id) else { return Value::Null };
    c.query_row("SELECT id,ts,process_id,stream,content FROM outputs WHERE id=?1",
                [oid], |r| {
        let content: Option<Vec<u8>> = r.get(4)?;
        let b64 = base64::engine::general_purpose::STANDARD
            .encode(content.unwrap_or_default());
        Ok(json!({"id": r.get::<_,i64>(0)?, "ts": r.get::<_,f64>(1)?,
                  "process_id": r.get::<_,Option<i64>>(2)?,
                  "stream": r.get::<_,i64>(3)?, "content": {"__b": b64}}))
    }).unwrap_or(Value::Null)
}

/// The deduped environment of one process row (env table via env_id), or {}.
pub fn process_env(box_id: i64, proc_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return json!({}) };
    c.query_row("SELECT env.env FROM process JOIN env ON process.env_id=env.id \
                 WHERE process.id=?1", [proc_id], |r| r.get::<_, Option<String>>(0))
        .ok().flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}))
}
