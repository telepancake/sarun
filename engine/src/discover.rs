// On-disk box discovery — the Rust counterpart of the Python engine's
// discover_sessions(): every <box_id>.sqlar under state_home plus every
// live/<box_id> backing dir IS a box; name/parent come from the sqlar's meta
// table, the command from the root process row. Read-only.

use std::collections::BTreeMap;
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
}

fn sqlar_path(box_id: i64) -> std::path::PathBuf {
    paths::state_home().join(format!("{box_id}.sqlar"))
}

fn read_meta(db: &Path) -> (String, Option<i64>, Vec<String>) {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return (String::new(), None, vec![]);
    };
    let get = |k: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key=?1", [k],
                       |r| r.get::<_, String>(0)).ok()
    };
    let name = get("name").unwrap_or_default();
    let parent = get("parent_box_id").and_then(|v| v.parse().ok());
    let cmd: Vec<String> = conn
        .query_row("SELECT argv FROM process WHERE root=1 ORDER BY id LIMIT 1",
                   [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    (name, parent, cmd)
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
            let (name, parent, cmd) = read_meta(&p);
            out.insert(id, Box_ {
                box_id: id, name, parent, cmd,
                started: ctime_of(&p), has_sqlar: true,
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
    if let Ok(mut st) = conn.prepare(&format!(
        "SELECT id,ts,cmd,record,pipeline,spawn_ts,{nested_col} FROM brushprov ORDER BY id")) {
        let it = st.query_map([], |r| {
            let rec: String = r.get(3)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
                "cmd": r.get::<_, String>(2)?,
                "record": serde_json::from_str::<Value>(&rec).unwrap_or(Value::Null),
                "pipeline": r.get::<_, Option<i64>>(4)?,
                "spawn_ts": r.get::<_, Option<f64>>(5)?,
                "nested": r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
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

/// D9 brush↔process linkage, process→pipeline direction: the brushprov pipeline
/// row that spawned process `row_id` (its exact cmd + parsed structure), or Null
/// if that process was not spawned by a brush pipeline (or the box isn't -b).
pub fn proc_pipeline(box_id: i64, row_id: i64) -> Value {
    let Some(c) = open_ro(box_id) else { return Value::Null };
    c.query_row(
        "SELECT bp.id,bp.ts,bp.cmd,bp.record,bp.pipeline \
         FROM process p JOIN brushprov bp ON p.brush_pipeline_id=bp.id \
         WHERE p.id=?1",
        [row_id], |r| {
            let rec: String = r.get(3)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?,
                "cmd": r.get::<_, String>(2)?,
                "record": serde_json::from_str::<Value>(&rec).unwrap_or(Value::Null),
                "pipeline": r.get::<_, Option<i64>>(4)?,
            }))
        }).unwrap_or(Value::Null)
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
