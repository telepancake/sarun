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
    // The DAG edges (DEPOT-DESIGN.md §8): the MAIN parent first (the tree
    // the sessions pane flattens by), then RO attachments. The UI's
    // sideways navigation cycles these.
    let mut parents: Vec<i64> = b.parent.into_iter().collect();
    // ro_attachments is heterogeneous (capture::RoAttachment): ints are
    // box ids (DAG edges → parents), objects are external references
    // (→ "attachments"). Open-error state lives on the LIVE overlay
    // ExtAttachment; this reads on-disk meta only, so the session_dicts
    // verb (control.rs) enriches these rows with "error".
    let mut attachments: Vec<Value> = vec![];
    if let Some(j) = b.meta.get("ro_attachments") {
        if let Ok(rows) = serde_json::from_str::<Vec<Value>>(j) {
            for r in rows {
                if let Some(id) = r.as_i64() {
                    parents.push(id);
                } else if r.is_object() {
                    attachments.push(json!({
                        "name": r.get("name").cloned().unwrap_or_default(),
                        "kind": r.get("kind").cloned().unwrap_or_default(),
                        "rev": r.get("rev").cloned().unwrap_or_default(),
                    }));
                }
            }
        }
    }
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
        // The DAG edges (DEPOT-DESIGN.md §8): the MAIN parent first (the
        // tree the sessions pane flattens by), then RO attachments. The
        // UI's sideways navigation cycles these.
        "parents": parents,
        // External RO references (Ext rows): identity for the UI —
        // NOT DAG edges, they have no box id.
        "attachments": attachments,
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

fn relation_path(value: &str) -> Result<crate::generated_wire::Path, String> {
    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
        .map_err(|error| format!("path exceeds relation bound: {error:?}"))
}

fn relation_os_string(value: &str) -> Result<crate::generated_wire::OsString, String> {
    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
        .map_err(|error| format!("OS string exceeds relation bound: {error:?}"))
}

fn relation_short_os_string(
    value: &str,
) -> Result<crate::generated_wire::ShortOsString, String> {
    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
        .map_err(|error| format!("short OS string exceeds relation bound: {error:?}"))
}

fn relation_argv(
    stored: Option<String>,
) -> Result<crate::wire::BoundedVec<
    crate::generated_wire::OsString,
    0,
    { crate::generated_wire::LIMIT_COMMAND_ITEMS },
>, String> {
    let words = match stored {
        Some(json) => serde_json::from_str::<Vec<String>>(&json)
            .map_err(|error| format!("invalid stored process argv: {error}"))?,
        None => Vec::new(),
    }.into_iter().map(|word| relation_os_string(&word))
        .collect::<Result<Vec<_>, _>>()?;
    crate::wire::BoundedVec::new(words)
        .map_err(|error| format!("process argv exceeds relation bound: {error:?}"))
}

fn relation_text(
    value: &str,
) -> Result<crate::wire::BoundedText<{ crate::generated_wire::LIMIT_TEXT_BYTES }>, String> {
    crate::wire::BoundedText::new(value.into())
        .map_err(|error| format!("text exceeds relation bound: {error:?}"))
}

fn relation_short_text(
    value: &str,
) -> Result<crate::wire::BoundedText<{ crate::generated_wire::LIMIT_SHORT_BYTES }>, String> {
    crate::wire::BoundedText::new(value.into())
        .map_err(|error| format!("short text exceeds relation bound: {error:?}"))
}

fn relation_blob(
    value: Vec<u8>,
) -> Result<crate::wire::BoundedBytes<{ crate::generated_wire::LIMIT_BLOB_BYTES }>, String> {
    crate::wire::BoundedBytes::new(value)
        .map_err(|error| format!("blob exceeds relation bound: {error:?}"))
}

fn relation_sql_bool(kind: &str, value: i64) -> Result<bool, String> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(format!("invalid stored {kind} boolean {value}")),
    }
}

fn relation_pipeline_stage(value: &Value) -> Result<crate::generated_wire::PipelineStage, String> {
    use crate::generated_wire::{LIMIT_STAGE_ITEMS, PipelineStage};
    let redirects = || -> Result<u32, String> {
        value.get("redirects").and_then(Value::as_u64)
            .ok_or_else(|| String::from("pipeline stage has no redirect count"))?
            .try_into().map_err(|_| "pipeline redirect count exceeds u32".into())
    };
    match value.get("kind").and_then(Value::as_str) {
        Some("simple") => {
            let words = value.get("words").and_then(Value::as_array)
                .ok_or("simple pipeline stage has no words")?
                .iter().map(|word| word.as_str()
                    .ok_or_else(|| "pipeline word is not text".into())
                    .and_then(relation_os_string))
                .collect::<Result<Vec<_>, String>>()?;
            Ok(PipelineStage::Simple {
                words: crate::wire::BoundedVec::<_, 0, LIMIT_STAGE_ITEMS>::new(words)
                    .map_err(|error| format!("pipeline words exceed relation bound: {error:?}"))?,
                redirects: redirects()?,
            })
        }
        Some("compound") => Ok(PipelineStage::Compound {
            redirects: redirects()?,
            text: relation_text(value.get("text").and_then(Value::as_str)
                .ok_or("compound pipeline stage has no text")?)?,
        }),
        Some("function") => Ok(PipelineStage::Function {
            text: relation_text(value.get("text").and_then(Value::as_str)
                .ok_or("function pipeline stage has no text")?)?,
        }),
        Some("extended_test") => Ok(PipelineStage::ExtendedTest {
            text: relation_text(value.get("text").and_then(Value::as_str)
                .ok_or("extended-test pipeline stage has no text")?)?,
        }),
        Some(kind) => Err(format!("unknown pipeline stage kind {kind:?}")),
        None => Err("pipeline stage has no kind".into()),
    }
}

fn relation_pipeline_provenance(
    value: &Value,
) -> Result<crate::generated_wire::PipelineProvenance, String> {
    use crate::generated_wire::{LIMIT_COLLECTION_ITEMS, LIMIT_STAGE_ITEMS, PipelineProvenance};
    let stages = value.get("stage_detail").and_then(Value::as_array)
        .ok_or("pipeline record has no stage detail")?
        .iter().map(relation_pipeline_stage).collect::<Result<Vec<_>, _>>()?;
    let targets = value.get("out_targets").and_then(Value::as_array)
        .ok_or("pipeline record has no output targets")?
        .iter().map(|target| target.as_str()
            .ok_or_else(|| "pipeline target is not text".into())
            .and_then(relation_path))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(PipelineProvenance {
        command: relation_text(value.get("cmd").and_then(Value::as_str)
            .ok_or("pipeline record has no command")?)?,
        negated: value.get("bang").and_then(Value::as_bool)
            .ok_or("pipeline record has no negation flag")?,
        stages: crate::wire::BoundedVec::<_, 1, LIMIT_STAGE_ITEMS>::new(stages)
            .map_err(|error| format!("pipeline stages exceed relation bound: {error:?}"))?,
        output_targets: crate::wire::BoundedVec::<_, 0, LIMIT_COLLECTION_ITEMS>::new(targets)
            .map_err(|error| format!("pipeline targets exceed relation bound: {error:?}"))?,
        uid: value.get("uid").and_then(Value::as_u64).unwrap_or(0),
        parent_uid: value.get("parent_uid").and_then(Value::as_u64).unwrap_or(0),
        sequence: value.get("seq").and_then(Value::as_u64).unwrap_or(0),
        spawned_at: value.get("spawn_ts").and_then(Value::as_f64).unwrap_or(0.0),
        nested: value.get("nested").and_then(Value::as_bool).unwrap_or(false),
        edge_output: value.get("edge_out").and_then(Value::as_str)
            .map(relation_path).transpose()?,
    })
}

fn relation_environment(
    stored: Option<&str>,
) -> Result<crate::generated_wire::Environment, String> {
    let entries = match stored {
        Some(stored) => serde_json::from_str::<BTreeMap<String, String>>(stored)
            .map_err(|error| format!("invalid stored process environment: {error}"))?,
        None => BTreeMap::new(),
    };
    let entries = entries.into_iter().map(|(key, value)| Ok((
        relation_short_os_string(&key)?,
        relation_os_string(&value)?,
    ))).collect::<Result<BTreeMap<_, _>, String>>()?;
    crate::wire::BoundedMap::new(entries)
        .map_err(|error| format!("process environment exceeds relation bound: {error:?}"))
}

pub fn processes_typed(box_id: i64) -> Result<Vec<crate::generated_wire::ProcessRow>, String> {
    use crate::generated_wire::ProcessRow;
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(vec![]);
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
        let it = st.query_map([], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<i64>>(6)?,
        )));
        if let Ok(it) = it {
            for (id, tgid, ppid, parent, executable, argv_json, pipeline) in it.flatten() {
                rows.push(ProcessRow {
                    id: id.try_into().map_err(|_| "negative process row id")?,
                    tgid: tgid.map(|value| value.try_into()
                        .map_err(|_| "process tgid exceeds u32")).transpose()?,
                    ppid: ppid.map(|value| value.try_into()
                        .map_err(|_| "process ppid exceeds u32")).transpose()?,
                    parent: parent.map(|value| value.try_into()
                        .map_err(|_| "negative process parent row id")).transpose()?,
                    executable: relation_path(&executable)?,
                    argv: relation_argv(argv_json)?,
                    pipeline: pipeline.map(|value| value.try_into()
                        .map_err(|_| "negative process pipeline row id")).transpose()?,
                });
            }
        }
    }
    Ok(rows)
}

pub fn process_rows_json(rows: &[crate::generated_wire::ProcessRow]) -> Value {
    Value::Array(rows.iter().map(|row| json!([
        row.id,
        row.tgid,
        row.ppid,
        row.parent,
        String::from_utf8_lossy(row.executable.as_slice()),
        row.argv.as_slice().iter().map(|word|
            String::from_utf8_lossy(word.as_slice()).into_owned()).collect::<Vec<_>>(),
        row.pipeline,
    ])).collect())
}

/// oaita-proxy API log rows for a box: one row per request the engine
/// forwarded on this box's behalf. The body bytes are summary-sized (lengths
/// only) here; `api_log_detail` fetches the full request/response on demand.
pub fn api_log_typed(
    box_id: i64,
) -> Result<Vec<crate::generated_wire::ApiLogRow>, String> {
    use crate::generated_wire::ApiLogRow;
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(vec![]);
    };
    if !has_table(&conn, "api_log") { return Ok(vec![]); }
    let mut statement = conn.prepare(
        "SELECT id,ts,method,path,model,status,stream,length(req),length(resp) \
         FROM api_log ORDER BY id")
        .map_err(|error| format!("prepare API log query: {error}"))?;
    let queried = statement.query_map([], |row| Ok((
        row.get::<_, i64>(0)?,
        row.get::<_, f64>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, String>(4)?,
        row.get::<_, i64>(5)?,
        row.get::<_, i64>(6)?,
        row.get::<_, i64>(7)?,
        row.get::<_, i64>(8)?,
    ))).map_err(|error| format!("read API log rows: {error}"))?;
    let mut rows = vec![];
    for row in queried {
        let (id, time, method, path, model, status, streaming,
             request_length, response_length) =
            row.map_err(|error| format!("read API log row: {error}"))?;
        rows.push(ApiLogRow {
            id: id.try_into().map_err(|_| "negative API log row id")?,
            time,
            method: relation_short_text(&method)?,
            path: relation_text(&path)?,
            model: relation_text(&model)?,
            status: status.try_into().map_err(|_| "API status exceeds u16")?,
            streaming: relation_sql_bool("API streaming", streaming)?,
            request_length: request_length.try_into()
                .map_err(|_| "negative API request length")?,
            response_length: response_length.try_into()
                .map_err(|_| "negative API response length")?,
        });
    }
    Ok(rows)
}

/// Full request/response payloads for one api_log row.
pub fn api_log_detail_typed(
    box_id: i64,
    row_id: u64,
) -> Result<Option<crate::generated_wire::ApiLogDetail>, String> {
    use crate::generated_wire::{ApiLogDetail, ApiLogRow};
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(None);
    };
    if !has_table(&conn, "api_log") { return Ok(None); }
    let row_id = i64::try_from(row_id).map_err(|_| "API log row id exceeds sqlite range")?;
    let row = conn.query_row(
        "SELECT id,ts,method,path,model,status,stream,req,resp \
         FROM api_log WHERE id=?1",
        [row_id], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?,
            row.get::<_, String>(4)?, row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?, row.get::<_, Vec<u8>>(7)?,
            row.get::<_, Vec<u8>>(8)?,
        )));
    let (id, time, method, path, model, status, streaming, request, response) = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(format!("read API log detail: {error}")),
    };
    let summary = ApiLogRow {
        id: id.try_into().map_err(|_| "negative API log row id")?,
        time,
        method: relation_short_text(&method)?,
        path: relation_text(&path)?,
        model: relation_text(&model)?,
        status: status.try_into().map_err(|_| "API status exceeds u16")?,
        streaming: relation_sql_bool("API streaming", streaming)?,
        request_length: request.len().try_into().map_err(|_| "API request length exceeds u64")?,
        response_length: response.len().try_into()
            .map_err(|_| "API response length exceeds u64")?,
    };
    Ok(Some(ApiLogDetail {
        summary,
        request: relation_blob(request)?,
        response: relation_blob(response)?,
    }))
}

pub fn api_log_rows_json(rows: &[crate::generated_wire::ApiLogRow]) -> Value {
    Value::Array(rows.iter().map(|row| json!({
        "id": row.id, "ts": row.time, "method": row.method.as_str(),
        "path": row.path.as_str(), "model": row.model.as_str(), "status": row.status,
        "stream": if row.streaming { 1 } else { 0 },
        "req_len": row.request_length, "resp_len": row.response_length,
    })).collect())
}

pub fn api_log_detail_json(row: &crate::generated_wire::ApiLogDetail) -> Value {
    json!({
        "id": row.summary.id, "ts": row.summary.time,
        "method": row.summary.method.as_str(), "path": row.summary.path.as_str(),
        "model": row.summary.model.as_str(), "status": row.summary.status,
        "stream": if row.summary.streaming { 1 } else { 0 },
        "req": String::from_utf8_lossy(row.request.as_slice()),
        "resp": String::from_utf8_lossy(row.response.as_slice()),
    })
}

/// Web capture rows for a box (DESIGN-web.md W1/W4): one row per HTTP(S)
/// request/response the tap MITM proxy teed. Summary-sized here (body lengths
/// only); `webcap_detail` fetches full headers + bodies on demand. Newest
/// first (DESC) — the browser/crawler produce these in time order and the
/// most recent captures are what the Captures pane wants at the top. A sqlar
/// written before the webcap table existed simply yields no rows.
pub fn webcap_typed(
    box_id: i64,
) -> Result<Vec<crate::generated_wire::WebCaptureRow>, String> {
    use crate::generated_wire::WebCaptureRow;
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(vec![]);
    };
    if !has_table(&conn, "webcap") { return Ok(vec![]); }
    let mut statement = conn.prepare(
        "SELECT id,ts,method,url,host,status,mime,truncated,\
         length(req_body),length(resp_body) FROM webcap ORDER BY id DESC")
        .map_err(|error| format!("prepare web capture query: {error}"))?;
    let queried = statement.query_map([], |row| Ok((
        row.get::<_, i64>(0)?, row.get::<_, f64>(1)?,
        row.get::<_, String>(2)?, row.get::<_, String>(3)?,
        row.get::<_, String>(4)?, row.get::<_, i64>(5)?,
        row.get::<_, String>(6)?, row.get::<_, i64>(7)?,
        row.get::<_, i64>(8)?, row.get::<_, i64>(9)?,
    ))).map_err(|error| format!("read web capture rows: {error}"))?;
    let mut rows = vec![];
    for row in queried {
        let (id, time, method, url, host, status, mime, truncated,
             request_length, response_length) =
            row.map_err(|error| format!("read web capture row: {error}"))?;
        rows.push(WebCaptureRow {
            id: id.try_into().map_err(|_| "negative web capture row id")?,
            time,
            method: relation_short_text(&method)?,
            url: relation_text(&url)?,
            host: relation_text(&host)?,
            status: status.try_into().map_err(|_| "web status exceeds u16")?,
            mime: relation_text(&mime)?,
            truncated: relation_sql_bool("web truncation", truncated)?,
            request_length: request_length.try_into()
                .map_err(|_| "negative web request length")?,
            response_length: response_length.try_into()
                .map_err(|_| "negative web response length")?,
        });
    }
    Ok(rows)
}

/// Full headers + bodies for one webcap row. Stored response bytes are decoded
/// to identity on this detail read via the recorded Content-Encoding; capture
/// and summary listing never pay that cost (DESIGN-web.md W2).
pub fn webcap_detail_typed(
    box_id: i64,
    row_id: u64,
) -> Result<Option<crate::generated_wire::WebCaptureDetail>, String> {
    use crate::generated_wire::{WebCaptureDetail, WebCaptureRow};
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(None);
    };
    if !has_table(&conn, "webcap") { return Ok(None); }
    let row_id = i64::try_from(row_id)
        .map_err(|_| "web capture row id exceeds sqlite range")?;
    let row = conn.query_row(
        "SELECT id,ts,method,url,host,status,mime,req_headers,resp_headers,\
         req_body,resp_body,truncated FROM webcap WHERE id=?1",
        [row_id], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?,
            row.get::<_, String>(4)?, row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?, row.get::<_, String>(7)?,
            row.get::<_, String>(8)?, row.get::<_, Vec<u8>>(9)?,
            row.get::<_, Vec<u8>>(10)?, row.get::<_, i64>(11)?,
        )));
    let (id, time, method, url, host, status, mime, request_headers,
         response_headers, request_body, response_body, truncated) = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(format!("read web capture detail: {error}")),
    };
    let response_length = response_body.len().try_into()
        .map_err(|_| "web response length exceeds u64")?;
    let response_body = crate::net::webcap::decode_body(&response_headers, &response_body);
    Ok(Some(WebCaptureDetail {
        summary: WebCaptureRow {
            id: id.try_into().map_err(|_| "negative web capture row id")?,
            time,
            method: relation_short_text(&method)?,
            url: relation_text(&url)?,
            host: relation_text(&host)?,
            status: status.try_into().map_err(|_| "web status exceeds u16")?,
            mime: relation_text(&mime)?,
            truncated: relation_sql_bool("web truncation", truncated)?,
            request_length: request_body.len().try_into()
                .map_err(|_| "web request length exceeds u64")?,
            response_length,
        },
        request_headers: relation_blob(request_headers.into_bytes())?,
        response_headers: relation_blob(response_headers.into_bytes())?,
        request_body: relation_blob(request_body)?,
        response_body: relation_blob(response_body)?,
    }))
}

/// Identity-decoded response bytes for the standalone image viewer. The typed
/// result remains bytes; only the temporary JSON listener projection applies
/// base64 (DESIGN-web.md W8).
pub fn webcap_body_typed(
    box_id: i64,
    row_id: u64,
) -> Result<Option<crate::generated_wire::WebCaptureBody>, String> {
    use crate::generated_wire::WebCaptureBody;
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(None);
    };
    if !has_table(&conn, "webcap") { return Ok(None); }
    let row_id = i64::try_from(row_id)
        .map_err(|_| "web capture row id exceeds sqlite range")?;
    let row = conn.query_row(
        "SELECT mime,resp_headers,resp_body FROM webcap WHERE id=?1",
        [row_id], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        )));
    let (mime, response_headers, response_body) = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(format!("read web capture body: {error}")),
    };
    let body = crate::net::webcap::decode_body(&response_headers, &response_body);
    Ok(Some(WebCaptureBody {
        mime: relation_text(&mime)?,
        body: relation_blob(body)?,
    }))
}

pub fn webcap_rows_json(rows: &[crate::generated_wire::WebCaptureRow]) -> Value {
    Value::Array(rows.iter().map(|row| json!({
        "id": row.id, "ts": row.time, "method": row.method.as_str(),
        "url": row.url.as_str(), "host": row.host.as_str(), "status": row.status,
        "mime": row.mime.as_str(), "truncated": if row.truncated { 1 } else { 0 },
        "req_len": row.request_length, "resp_len": row.response_length,
    })).collect())
}

pub fn webcap_detail_json(row: &crate::generated_wire::WebCaptureDetail) -> Value {
    json!({
        "id": row.summary.id, "ts": row.summary.time,
        "method": row.summary.method.as_str(), "url": row.summary.url.as_str(),
        "host": row.summary.host.as_str(), "status": row.summary.status,
        "mime": row.summary.mime.as_str(),
        "req_headers": String::from_utf8_lossy(row.request_headers.as_slice()),
        "resp_headers": String::from_utf8_lossy(row.response_headers.as_slice()),
        "req_body": String::from_utf8_lossy(row.request_body.as_slice()),
        "resp_body": String::from_utf8_lossy(row.response_body.as_slice()),
        "truncated": if row.summary.truncated { 1 } else { 0 },
    })
}

pub fn webcap_body_json(row: &crate::generated_wire::WebCaptureBody) -> Value {
    use base64::Engine as _;
    json!({
        "mime": row.mime.as_str(),
        "b64": base64::engine::general_purpose::STANDARD.encode(row.body.as_slice()),
    })
}

/// One replayed response: the RAW stored bytes + verbatim headers of the
/// capture that answers a request. Byte-identical to what the box first
/// received (Content-Encoding kept), so a replay browser decodes it exactly.
pub struct ReplayHit {
    pub status: i32,
    pub resp_headers: String,
    pub resp_body: Vec<u8>,
}

/// Replay lookup (DESIGN-web.md W4.2): the newest capture in box `box_id`
/// whose URL matches `url` (and method, when the row recorded one), at or
/// before `asof` when given. This is what the replay proxy serves instead of
/// dialing upstream — exact-URL match, newest-first. Returns None when the
/// archive has no such capture (the caller answers 404, keeping replay
/// sealed: a miss is never a live fetch).
pub fn webcap_replay(box_id: i64, url: &str, method: &str,
                     asof: Option<f64>) -> Option<ReplayHit> {
    let db = sqlar_path(box_id);
    let conn = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    webcap_replay_conn(&conn, url, method, asof)
}

/// The replay query on an open connection (split out for testing). method is
/// advisory: a capture recorded with an empty method still matches. asof:
/// newest with ts <= asof (negative sentinel = no bound).
fn webcap_replay_conn(conn: &rusqlite::Connection, url: &str, method: &str,
                      asof: Option<f64>) -> Option<ReplayHit> {
    let sql = "SELECT status,resp_headers,resp_body FROM webcap \
               WHERE url=?1 AND (method=?2 OR method='') \
               AND (?3 < 0 OR ts <= ?3) ORDER BY ts DESC LIMIT 1";
    conn.query_row(sql, rusqlite::params![url, method, asof.unwrap_or(-1.0)],
        |r| Ok(ReplayHit {
            status: r.get::<_, i64>(0)? as i32,
            resp_headers: r.get(1)?,
            resp_body: r.get(2)?,
        })).ok()
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    fn seed() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE webcap(id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL,
             method TEXT, url TEXT, host TEXT, status INT, mime TEXT,
             req_headers TEXT, resp_headers TEXT, req_body BLOB, resp_body BLOB,
             truncated INT DEFAULT 0);").unwrap();
        let ins = "INSERT INTO webcap(ts,method,url,host,status,mime,\
                   req_headers,resp_headers,req_body,resp_body,truncated) \
                   VALUES(?1,'GET',?2,'x',?3,'text/html','','ct: html\n','',?4,0)";
        c.execute(ins, rusqlite::params![1.0, "https://x/", 200, b"OLD".to_vec()]).unwrap();
        c.execute(ins, rusqlite::params![9.0, "https://x/", 200, b"NEW".to_vec()]).unwrap();
        c.execute(ins, rusqlite::params![5.0, "https://x/a.js", 404, b"".to_vec()]).unwrap();
        c
    }

    #[test]
    fn replay_returns_newest_and_respects_asof() {
        let c = seed();
        // Newest capture for the URL wins.
        let h = webcap_replay_conn(&c, "https://x/", "GET", None).unwrap();
        assert_eq!(h.resp_body, b"NEW");
        assert_eq!(h.status, 200);
        // asof before the newest → the older capture.
        let h = webcap_replay_conn(&c, "https://x/", "GET", Some(3.0)).unwrap();
        assert_eq!(h.resp_body, b"OLD");
        // A different captured resource (a 404 the box actually got).
        assert_eq!(webcap_replay_conn(&c, "https://x/a.js", "GET", None).unwrap().status, 404);
        // A URL never captured → None (the proxy answers 404, sealed).
        assert!(webcap_replay_conn(&c, "https://x/missing", "GET", None).is_none());
    }
}

fn relation_output_stream(value: i64) -> Result<crate::generated_wire::EchoStream, String> {
    use crate::generated_wire::EchoStream;
    match value {
        0 => Ok(EchoStream::Stdout),
        1 => Ok(EchoStream::Stderr),
        value => Err(format!("unknown captured output stream {value}")),
    }
}

pub fn outputs_typed(box_id: i64) -> Result<Vec<crate::generated_wire::OutputRow>, String> {
    use crate::generated_wire::OutputRow;
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(vec![]);
    };
    let mut rows = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT id,ts,process_id,stream,length(content) FROM outputs ORDER BY id") {
        let it = st.query_map([], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        )));
        if let Ok(it) = it {
            for (id, time, process, stream, length) in it.flatten() {
                let stream = relation_output_stream(stream)?;
                rows.push(OutputRow {
                    id: id.try_into().map_err(|_| "negative output row id")?,
                    time,
                    process: process.map(|value| value.try_into()
                        .map_err(|_| "negative output process id")).transpose()?,
                    stream,
                    length: length.try_into().map_err(|_| "negative output length")?,
                });
            }
        }
    }
    Ok(rows)
}

pub fn output_rows_json(rows: &[crate::generated_wire::OutputRow]) -> Value {
    use crate::generated_wire::EchoStream;
    Value::Array(rows.iter().map(|row| json!({
        "id": row.id,
        "ts": row.time,
        "process_id": row.process,
        "stream": match row.stream { EchoStream::Stdout => 0, EchoStream::Stderr => 1 },
        "len": row.length,
    })).collect())
}

/// D9 brush-shell semantic provenance rows for a box: each is one pipeline the
/// embedded brush shell (-b) ran, with its exact command string and the parsed
/// pipeline/redirect structure. Empty for boxes not run with -b.
pub fn brushprov_typed(box_id: i64) -> Result<Vec<crate::generated_wire::PipelineRow>, String> {
    use crate::generated_wire::{LIMIT_COLLECTION_ITEMS, PipelineRow};
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(vec![]);
    };
    let mut processes: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    if let Ok(mut statement) = conn.prepare(
        "SELECT brush_pipeline_id,id FROM process \
         WHERE brush_pipeline_id IS NOT NULL ORDER BY id") {
        if let Ok(rows) = statement.query_map([], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))) {
            for (pipeline, process) in rows.flatten() {
                let pipeline = u64::try_from(pipeline)
                    .map_err(|_| "negative process pipeline row id")?;
                let process = u64::try_from(process)
                    .map_err(|_| "negative process row id")?;
                processes.entry(pipeline).or_default().push(process);
            }
        }
    }
    let mut result = vec![];
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
        let rows = st.query_map([], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
            row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            row.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
            row.get::<_, Option<i64>>(10)?.unwrap_or(-1),
        )));
        if let Ok(rows) = rows {
            for (id, time, command, record, pipeline, spawned_at, nested,
                 uid, parent_uid, done_at, exit_code) in rows.flatten() {
                let id = u64::try_from(id).map_err(|_| "negative pipeline row id")?;
                let record: Value = serde_json::from_str(&record)
                    .map_err(|error| format!("invalid stored pipeline record: {error}"))?;
                let record = if record.is_null() {
                    None
                } else {
                    Some(relation_pipeline_provenance(&record)?)
                };
                let process_rows = processes.remove(&id).unwrap_or_default();
                result.push(PipelineRow {
                    id,
                    time,
                    command: relation_text(&command)?,
                    record,
                    pipeline: pipeline.map(|value| value.try_into()
                        .map_err(|_| "negative pipeline sequence")).transpose()?,
                    spawned_at,
                    done_at: (done_at != 0.0).then_some(done_at),
                    nested,
                    uid: (uid > 0).then(|| uid.try_into()
                        .map_err(|_| "negative pipeline uid")).transpose()?,
                    parent_uid: (parent_uid > 0).then(|| parent_uid.try_into()
                        .map_err(|_| "negative pipeline parent uid")).transpose()?,
                    exit_code: (exit_code != -1).then(|| exit_code.try_into()
                        .map_err(|_| "pipeline exit code exceeds i32")).transpose()?,
                    processes: crate::wire::BoundedVec::<_, 0, LIMIT_COLLECTION_ITEMS>::new(
                        process_rows,
                    ).map_err(|error| format!(
                        "pipeline process list exceeds relation bound: {error:?}"
                    ))?,
                });
            }
        }
    }
    Ok(result)
}

fn pipeline_stage_json(stage: &crate::generated_wire::PipelineStage) -> Value {
    use crate::generated_wire::PipelineStage;
    match stage {
        PipelineStage::Simple { words, redirects } => json!({
            "kind": "simple",
            "words": words.as_slice().iter().map(|word|
                String::from_utf8_lossy(word.as_slice()).into_owned()).collect::<Vec<_>>(),
            "redirects": redirects,
        }),
        PipelineStage::Compound { redirects, text } => json!({
            "kind": "compound", "redirects": redirects, "text": text.as_str(),
        }),
        PipelineStage::Function { text } => json!({
            "kind": "function", "text": text.as_str(),
        }),
        PipelineStage::ExtendedTest { text } => json!({
            "kind": "extended_test", "text": text.as_str(),
        }),
    }
}

pub fn pipeline_provenance_json(record: &crate::generated_wire::PipelineProvenance) -> Value {
    let mut value = json!({
        "cmd": record.command.as_str(),
        "bang": record.negated,
        "stages": record.stages.as_slice().len(),
        "stage_detail": record.stages.as_slice().iter().map(pipeline_stage_json)
            .collect::<Vec<_>>(),
        "out_targets": record.output_targets.as_slice().iter().map(|path|
            String::from_utf8_lossy(path.as_slice()).into_owned()).collect::<Vec<_>>(),
        "uid": record.uid,
        "parent_uid": record.parent_uid,
        "seq": record.sequence,
        "spawn_ts": record.spawned_at,
    });
    if record.nested { value["nested"] = Value::Bool(true); }
    if let Some(edge) = &record.edge_output {
        value["edge_out"] = Value::String(String::from_utf8_lossy(edge.as_slice()).into_owned());
    }
    value
}

#[cfg(test)]
mod relation_row_tests {
    use super::*;
    use crate::generated_wire::PipelineStage;

    #[test]
    fn stored_pipeline_json_normalizes_once_to_the_closed_relation_type() {
        let stored = json!({
            "cmd": "echo hi > out",
            "bang": false,
            "stages": 1,
            "stage_detail": [{
                "kind": "simple", "words": ["echo", "hi"], "redirects": 1
            }],
            "out_targets": ["out"],
            "uid": 12,
            "parent_uid": 4,
            "seq": 3,
            "spawn_ts": 1.25,
            "nested": true,
            "edge_out": "out"
        });
        let record = relation_pipeline_provenance(&stored).unwrap();
        assert_eq!(record.command.as_str(), "echo hi > out");
        assert_eq!((record.uid, record.parent_uid, record.sequence), (12, 4, 3));
        assert!(matches!(record.stages.as_slice()[0], PipelineStage::Simple { .. }));
        assert_eq!(record.edge_output.as_ref().unwrap().as_slice(), b"out");

        let rendered = pipeline_provenance_json(&record);
        assert_eq!(rendered["cmd"], stored["cmd"]);
        assert_eq!(rendered["stage_detail"], stored["stage_detail"]);
        assert_eq!(rendered["out_targets"], stored["out_targets"]);
        assert_eq!(rendered["edge_out"], stored["edge_out"]);
    }

    #[test]
    fn old_pipeline_records_receive_the_relation_sentinels_not_an_alternate_shape() {
        let stored = json!({
            "cmd": "true", "bang": false,
            "stage_detail": [{"kind": "simple", "words": ["true"], "redirects": 0}],
            "out_targets": []
        });
        let record = relation_pipeline_provenance(&stored).unwrap();
        assert_eq!((record.uid, record.parent_uid, record.sequence), (0, 0, 0));
        assert_eq!(record.spawned_at, 0.0);
        assert!(!record.nested);
    }

    #[test]
    fn detail_rows_project_only_after_closed_type_construction() {
        use crate::generated_wire::{EchoStream, OutputDetail, OutputRow, ProcessInfo,
            ProcessSubject};
        use base64::Engine as _;

        let pipeline = pipeline_summary(9, 1.5, "echo hi".into(), "null".into(), Some(3))
            .unwrap();
        assert_eq!(pipeline_summary_json(&pipeline), json!({
            "id": 9, "ts": 1.5, "cmd": "echo hi", "record": null, "pipeline": 3,
        }));
        assert!(pipeline_summary(-1, 0.0, String::new(), "null".into(), None).is_err());
        assert!(pipeline_summary(1, 0.0, String::new(), "{".into(), None).is_err());

        let argv = relation_argv(Some(r#"["echo","hi"]"#.into())).unwrap();
        let info = ProcessInfo {
            tgid: Some(20), ppid: Some(10), parent: Some(2),
            executable: relation_path("/bin/echo").unwrap(), argv: argv.clone(),
        };
        assert_eq!(process_info_json(&info), json!([
            20, 10, 2, "/bin/echo", ["echo", "hi"],
        ]));
        let subject = ProcessSubject {
            executable: info.executable.clone(), cwd: relation_path("/tmp").unwrap(), argv,
        };
        assert_eq!(process_subject_json(&subject), json!({
            "exe": "/bin/echo", "cwd": "/tmp", "argv": ["echo", "hi"],
        }));

        let detail = OutputDetail {
            summary: OutputRow {
                id: 7, time: 2.5, process: Some(2), stream: EchoStream::Stderr, length: 3,
            },
            content: crate::wire::BoundedBytes::new(vec![0, 1, 255]).unwrap(),
        };
        assert_eq!(output_detail_json(&detail), json!({
            "id": 7, "ts": 2.5, "process_id": 2, "stream": 1,
            "content": {"__b": base64::engine::general_purpose::STANDARD.encode([0, 1, 255])},
        }));
    }

    #[test]
    fn stored_environment_must_match_the_closed_byte_map() {
        let environment = relation_environment(Some(r#"{"A":"1","PATH":"/bin"}"#))
            .unwrap();
        assert_eq!(environment_json(&environment), json!({"A": "1", "PATH": "/bin"}));
        assert!(relation_environment(Some(r#"{"A":1}"#)).is_err());
        assert_eq!(environment_json(&relation_environment(None).unwrap()), json!({}));
    }

    #[test]
    fn api_and_web_bytes_project_only_at_the_listener_boundary() {
        use base64::Engine as _;
        use crate::generated_wire::{ApiLogDetail, ApiLogRow, WebCaptureBody,
            WebCaptureDetail, WebCaptureRow};

        let api = ApiLogRow {
            id: 3, time: 1.0, method: relation_short_text("POST").unwrap(),
            path: relation_text("/v1/chat").unwrap(), model: relation_text("m").unwrap(),
            status: 200, streaming: true, request_length: 2, response_length: 3,
        };
        assert_eq!(api_log_rows_json(&[api.clone()]), json!([{
            "id": 3, "ts": 1.0, "method": "POST", "path": "/v1/chat", "model": "m",
            "status": 200, "stream": 1, "req_len": 2, "resp_len": 3,
        }]));
        let api_detail = ApiLogDetail {
            summary: api, request: relation_blob(b"{}".to_vec()).unwrap(),
            response: relation_blob(b"yes".to_vec()).unwrap(),
        };
        assert_eq!(api_log_detail_json(&api_detail)["resp"], "yes");

        let web = WebCaptureRow {
            id: 4, time: 2.0, method: relation_short_text("GET").unwrap(),
            url: relation_text("https://x/").unwrap(), host: relation_text("x").unwrap(),
            status: 200, mime: relation_text("image/png").unwrap(), truncated: false,
            request_length: 0, response_length: 3,
        };
        let web_detail = WebCaptureDetail {
            summary: web.clone(), request_headers: relation_blob(vec![]).unwrap(),
            response_headers: relation_blob(b"x: y".to_vec()).unwrap(),
            request_body: relation_blob(vec![]).unwrap(),
            response_body: relation_blob(vec![0, 1, 255]).unwrap(),
        };
        assert_eq!(webcap_rows_json(&[web]), json!([{
            "id": 4, "ts": 2.0, "method": "GET", "url": "https://x/", "host": "x",
            "status": 200, "mime": "image/png", "truncated": 0,
            "req_len": 0, "resp_len": 3,
        }]));
        assert_eq!(webcap_detail_json(&web_detail)["resp_headers"], "x: y");
        let body = WebCaptureBody {
            mime: relation_text("image/png").unwrap(),
            body: relation_blob(vec![0, 1, 255]).unwrap(),
        };
        assert_eq!(webcap_body_json(&body)["b64"],
            base64::engine::general_purpose::STANDARD.encode([0, 1, 255]));
        assert!(relation_sql_bool("test", 2).is_err());
    }
}

pub fn pipeline_rows_json(rows: &[crate::generated_wire::PipelineRow]) -> Value {
    Value::Array(rows.iter().map(|row| json!({
        "id": row.id,
        "ts": row.time,
        "cmd": row.command.as_str(),
        "record": row.record.as_ref().map(pipeline_provenance_json),
        "pipeline": row.pipeline,
        "spawn_ts": row.spawned_at,
        "nested": row.nested,
        "uid": row.uid.unwrap_or(0),
        "parent_uid": row.parent_uid.unwrap_or(0),
        "done_ts": row.done_at.unwrap_or(0.0),
        "exit_code": row.exit_code.unwrap_or(-1),
        "processes": row.processes.as_slice(),
    })).collect())
}

/// Phase 1 embedded-ninja: the parsed build-graph edges captured when the box's
/// `ninja` (vendored n2 in-process) loaded build.ninja. Each row is one edge
/// {outs, ins, cmd}, INCLUDING up-to-date targets that never executed. Empty for
/// boxes that never ran ninja (or whose sqlar predates the build_edges table).
pub fn build_edges_typed(box_id: i64) -> Result<Vec<crate::generated_wire::BuildEdgeRow>, String> {
    use crate::generated_wire::{BuildEdgeRow, LIMIT_COLLECTION_ITEMS};
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Ok(vec![]);
    };
    if !has_table(&conn, "build_edges") { return Ok(vec![]); }
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
        let values = st.query_map([], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<String>>(8)?,
        )));
        if let Ok(values) = values {
            for (id, time, outputs, inputs, command, started_at, ended_at,
                 exit_code, output_excerpt) in values.flatten() {
                let outputs = serde_json::from_str::<Vec<String>>(&outputs)
                    .map_err(|error| format!("invalid stored build outputs: {error}"))?
                    .into_iter().map(|path| relation_path(&path))
                    .collect::<Result<Vec<_>, _>>()?;
                let inputs = serde_json::from_str::<Vec<String>>(&inputs)
                    .map_err(|error| format!("invalid stored build inputs: {error}"))?
                    .into_iter().map(|path| relation_path(&path))
                    .collect::<Result<Vec<_>, _>>()?;
                rows.push(BuildEdgeRow {
                    id: id.try_into().map_err(|_| "negative build edge row id")?,
                    time,
                    outputs: crate::wire::BoundedVec::<_, 1, LIMIT_COLLECTION_ITEMS>::new(outputs)
                        .map_err(|error| format!(
                            "build edge outputs exceed relation bound: {error:?}"
                        ))?,
                    inputs: crate::wire::BoundedVec::<_, 0, LIMIT_COLLECTION_ITEMS>::new(inputs)
                        .map_err(|error| format!(
                            "build edge inputs exceed relation bound: {error:?}"
                        ))?,
                    command: command.as_deref().map(relation_text).transpose()?,
                    started_at,
                    ended_at,
                    exit_code: exit_code.map(|value| value.try_into()
                        .map_err(|_| "build edge exit code exceeds i32")).transpose()?,
                    output_excerpt: output_excerpt.as_deref().map(relation_text).transpose()?,
                });
            }
        }
    }
    Ok(rows)
}

pub fn build_edge_rows_json(rows: &[crate::generated_wire::BuildEdgeRow]) -> Value {
    Value::Array(rows.iter().map(|row| json!({
        "id": row.id,
        "ts": row.time,
        "outs": row.outputs.as_slice().iter().map(|path|
            String::from_utf8_lossy(path.as_slice()).into_owned()).collect::<Vec<_>>(),
        "ins": row.inputs.as_slice().iter().map(|path|
            String::from_utf8_lossy(path.as_slice()).into_owned()).collect::<Vec<_>>(),
        "cmd": row.command.as_ref().map(|value| value.as_str()),
        "started_ts": row.started_at,
        "ended_ts": row.ended_at,
        "exit_code": row.exit_code,
        "output_excerpt": row.output_excerpt.as_ref().map(|value| value.as_str()),
    })).collect())
}

fn pipeline_summary(
    id: i64,
    time: f64,
    command: String,
    record: String,
    pipeline: Option<i64>,
) -> Result<crate::generated_wire::PipelineSummary, String> {
    let record: Value = serde_json::from_str(&record)
        .map_err(|error| format!("invalid stored pipeline record: {error}"))?;
    Ok(crate::generated_wire::PipelineSummary {
        id: id.try_into().map_err(|_| "negative pipeline row id")?,
        time,
        command: relation_text(&command)?,
        record: if record.is_null() {
            None
        } else {
            Some(relation_pipeline_provenance(&record)?)
        },
        pipeline: pipeline.map(|value| value.try_into()
            .map_err(|_| "negative pipeline sequence")).transpose()?,
    })
}

fn pipeline_summary_by_id(
    conn: &rusqlite::Connection,
    id: i64,
) -> Result<Option<crate::generated_wire::PipelineSummary>, String> {
    let row = conn.query_row(
        "SELECT id,ts,cmd,record,pipeline FROM brushprov WHERE id=?1",
        [id],
        |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<i64>>(4)?,
        )),
    );
    match row {
        Ok((id, time, command, record, pipeline)) =>
            pipeline_summary(id, time, command, record, pipeline).map(Some),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("read pipeline row: {error}")),
    }
}

/// D9 brush↔process linkage, process→pipeline direction: the brushprov pipeline
/// row that spawned process `row_id`, or None if the process was not spawned by
/// one. Walk parent_id because output writers are often grandchildren of the
/// pipeline command (for example `rm` forked by a shell forked by make).
pub fn proc_pipeline_typed(
    box_id: i64,
    row_id: u64,
) -> Result<Option<crate::generated_wire::PipelineSummary>, String> {
    let Some(conn) = open_ro(box_id) else { return Ok(None) };
    if !has_table(&conn, "process") || !has_table(&conn, "brushprov")
        || !has_col(&conn, "process", "brush_pipeline_id") {
        return Ok(None);
    }
    let mut current = i64::try_from(row_id)
        .map_err(|_| "process row id exceeds sqlite range")?;
    for _ in 0..64 {
        let row = conn.query_row(
            "SELECT bp.id,bp.ts,bp.cmd,bp.record,bp.pipeline \
             FROM process p JOIN brushprov bp ON p.brush_pipeline_id=bp.id \
             WHERE p.id=?1",
            [current], |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
            )));
        match row {
            Ok((id, time, command, record, pipeline)) =>
                return pipeline_summary(id, time, command, record, pipeline).map(Some),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => return Err(format!("read process pipeline: {error}")),
        }
        match conn.query_row(
            "SELECT parent_id FROM process WHERE id=?1", [current],
            |row| row.get::<_, Option<i64>>(0)) {
            Ok(Some(parent)) if parent != current => current = parent,
            Err(rusqlite::Error::QueryReturnedNoRows) | Ok(None) => break,
            Err(error) => return Err(format!("read process parent: {error}")),
            _ => break,
        }
    }
    Ok(None)
}

/// Output→pipeline: find the brushprov pipeline that produced output `output_id`.
/// Uses the output's own brush_pipeline_id column (stamped at capture time).
/// Falls back to the process→pipeline parent walk ONLY when the column does
/// not exist (pre-column data). When the column exists but is NULL, the output
/// genuinely has no pipeline — guessing via process ancestry produces wrong
/// results for in-process builtins whose process_id is the shared brush root.
pub fn output_pipeline_typed(
    box_id: i64,
    output_id: u64,
) -> Result<Option<crate::generated_wire::PipelineSummary>, String> {
    let Some(conn) = open_ro(box_id) else { return Ok(None) };
    if !has_table(&conn, "outputs") { return Ok(None); }
    let output_id = i64::try_from(output_id)
        .map_err(|_| "output row id exceeds sqlite range")?;
    let has_bp_col = has_col(&conn, "outputs", "brush_pipeline_id");
    let bp_col = if has_bp_col { "brush_pipeline_id" } else { "NULL" };
    let q = format!("SELECT process_id,{bp_col} FROM outputs WHERE id=?1");
    let (process_id, pipeline_id): (Option<i64>, Option<i64>) = match conn.query_row(
        &q, [output_id], |row| Ok((row.get(0)?, row.get(1)?))) {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(format!("read output pipeline reference: {error}")),
    };
    if let Some(pipeline_id) = pipeline_id {
        return pipeline_summary_by_id(&conn, pipeline_id);
    }
    // Only fall back to process-tree walk when the column doesn't exist at
    // all (old data). If the column exists but is NULL, the output was not
    // attributed to any pipeline at capture time — don't guess.
    if !has_bp_col {
        if let Some(process_id) = process_id {
            let process_id = process_id.try_into()
                .map_err(|_| "negative output process row id")?;
            return proc_pipeline_typed(box_id, process_id);
        }
    }
    Ok(None)
}

/// D9 brush↔process linkage, pipeline→processes direction: the process row ids
/// the brushprov pipeline `brushprov_id` spawned (empty if none/unknown).
pub fn pipeline_procs_typed(box_id: i64, pipeline_id: u64) -> Result<Vec<u64>, String> {
    let Some(conn) = open_ro(box_id) else { return Ok(vec![]) };
    if !has_table(&conn, "process") || !has_col(&conn, "process", "brush_pipeline_id") {
        return Ok(vec![]);
    }
    let pipeline_id = i64::try_from(pipeline_id)
        .map_err(|_| "pipeline row id exceeds sqlite range")?;
    let mut statement = conn.prepare(
        "SELECT id FROM process WHERE brush_pipeline_id=?1 ORDER BY id")
        .map_err(|error| format!("prepare pipeline process query: {error}"))?;
    let rows = statement.query_map([pipeline_id], |row| row.get::<_, i64>(0))
        .map_err(|error| format!("read pipeline process rows: {error}"))?;
    let mut result = vec![];
    for row in rows {
        let id = row.map_err(|error| format!("read pipeline process row: {error}"))?;
        result.push(id.try_into().map_err(|_| "negative process row id")?);
    }
    Ok(result)
}

pub fn open_ro_for(box_id: i64) -> Option<rusqlite::Connection> { open_ro(box_id) }

fn open_ro(box_id: i64) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        sqlar_path(box_id), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// (tgid, ppid, parent_id, exe, argv) for one process row — the proc-tree
/// connector resolver. None if the row id isn't recorded.
pub fn proc_info_typed(
    box_id: i64,
    row_id: u64,
) -> Result<Option<crate::generated_wire::ProcessInfo>, String> {
    let Some(conn) = open_ro(box_id) else { return Ok(None) };
    if !has_table(&conn, "process") { return Ok(None); }
    let row_id = i64::try_from(row_id).map_err(|_| "process row id exceeds sqlite range")?;
    let row = conn.query_row(
        "SELECT tgid,ppid,parent_id,exe,argv FROM process WHERE id=?1",
        [row_id],
        |row| Ok((
            row.get::<_, Option<i64>>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            row.get::<_, Option<String>>(4)?,
        )),
    );
    let (tgid, ppid, parent, executable, argv) = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(format!("read process row: {error}")),
    };
    Ok(Some(crate::generated_wire::ProcessInfo {
        tgid: tgid.map(|value| value.try_into()
            .map_err(|_| "process tgid exceeds u32")).transpose()?,
        ppid: ppid.map(|value| value.try_into()
            .map_err(|_| "process ppid exceeds u32")).transpose()?,
        parent: parent.map(|value| value.try_into()
            .map_err(|_| "negative process parent row id")).transpose()?,
        executable: relation_path(&executable)?,
        argv: relation_argv(argv)?,
    }))
}

/// Provenance dict {exe,cwd,argv} of one process row — the procs-pane filter.
pub fn proc_prov_typed(
    box_id: i64,
    row_id: u64,
) -> Result<Option<crate::generated_wire::ProcessSubject>, String> {
    let Some(conn) = open_ro(box_id) else { return Ok(None) };
    if !has_table(&conn, "process") { return Ok(None); }
    let row_id = i64::try_from(row_id).map_err(|_| "process row id exceeds sqlite range")?;
    let row = conn.query_row(
        "SELECT exe,cwd,argv FROM process WHERE id=?1", [row_id], |row| Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, Option<String>>(2)?,
        )));
    match row {
        Ok((executable, cwd, argv)) => Ok(Some(crate::generated_wire::ProcessSubject {
            executable: relation_path(&executable)?,
            cwd: relation_path(&cwd)?,
            argv: relation_argv(argv)?,
        })),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("read process provenance: {error}")),
    }
}

/// Hierarchy-root row ids (process.root=1) — the proc-tree walk boundary.
pub fn proc_roots_typed(box_id: i64) -> Result<Vec<u64>, String> {
    let Some(conn) = open_ro(box_id) else { return Ok(vec![]) };
    if !has_table(&conn, "process") { return Ok(vec![]); }
    let mut statement = conn.prepare("SELECT id FROM process WHERE root=1 ORDER BY id")
        .map_err(|error| format!("prepare process root query: {error}"))?;
    let rows = statement.query_map([], |row| row.get::<_, i64>(0))
        .map_err(|error| format!("read process root rows: {error}"))?;
    let mut roots = vec![];
    for row in rows {
        let id = row.map_err(|error| format!("read process root row: {error}"))?;
        roots.push(id.try_into().map_err(|_| "negative process root row id")?);
    }
    Ok(roots)
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

/// One captured output row with its original content bytes. Human/JSON
/// projection is deliberately outside this database decoder.
pub fn output_detail_typed(
    box_id: i64,
    output_id: u64,
) -> Result<Option<crate::generated_wire::OutputDetail>, String> {
    use crate::generated_wire::{OutputDetail, OutputRow};
    let Some(conn) = open_ro(box_id) else { return Ok(None) };
    if !has_table(&conn, "outputs") { return Ok(None); }
    let output_id = i64::try_from(output_id)
        .map_err(|_| "output row id exceeds sqlite range")?;
    let row = conn.query_row(
        "SELECT id,ts,process_id,stream,content FROM outputs WHERE id=?1",
        [output_id], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default(),
        )));
    let (id, time, process, stream, content) = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(format!("read output detail: {error}")),
    };
    let stream = relation_output_stream(stream)?;
    Ok(Some(OutputDetail {
        summary: OutputRow {
            id: id.try_into().map_err(|_| "negative output row id")?,
            time,
            process: process.map(|value| value.try_into()
                .map_err(|_| "negative output process id")).transpose()?,
            stream,
            length: content.len().try_into().map_err(|_| "output length exceeds u64")?,
        },
        content: crate::wire::BoundedBytes::new(content)
            .map_err(|error| format!("output content exceeds relation bound: {error:?}"))?,
    }))
}

/// The deduped environment of one process row (env table via env_id), or {}.
pub fn process_env_typed(
    box_id: i64,
    process_id: u64,
) -> Result<crate::generated_wire::Environment, String> {
    let Some(conn) = open_ro(box_id) else {
        return relation_environment(None);
    };
    if !has_table(&conn, "process") || !has_table(&conn, "env") {
        return relation_environment(None);
    }
    let process_id = i64::try_from(process_id)
        .map_err(|_| "process row id exceeds sqlite range")?;
    let stored = conn.query_row(
        "SELECT env.env FROM process JOIN env ON process.env_id=env.id WHERE process.id=?1",
        [process_id], |row| row.get::<_, Option<String>>(0));
    let stored = match stored {
        Ok(stored) => stored,
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(format!("read process environment: {error}")),
    };
    relation_environment(stored.as_deref())
}

pub fn pipeline_summary_json(row: &crate::generated_wire::PipelineSummary) -> Value {
    json!({
        "id": row.id, "ts": row.time, "cmd": row.command.as_str(),
        "record": row.record.as_ref().map(pipeline_provenance_json),
        "pipeline": row.pipeline,
    })
}

pub fn process_info_json(row: &crate::generated_wire::ProcessInfo) -> Value {
    json!([
        row.tgid, row.ppid, row.parent,
        String::from_utf8_lossy(row.executable.as_slice()),
        row.argv.as_slice().iter().map(|word|
            String::from_utf8_lossy(word.as_slice()).into_owned()).collect::<Vec<_>>(),
    ])
}

pub fn process_subject_json(row: &crate::generated_wire::ProcessSubject) -> Value {
    json!({
        "exe": String::from_utf8_lossy(row.executable.as_slice()),
        "cwd": String::from_utf8_lossy(row.cwd.as_slice()),
        "argv": row.argv.as_slice().iter().map(|word|
            String::from_utf8_lossy(word.as_slice()).into_owned()).collect::<Vec<_>>(),
    })
}

pub fn output_detail_json(row: &crate::generated_wire::OutputDetail) -> Value {
    use base64::Engine as _;
    use crate::generated_wire::EchoStream;
    json!({
        "id": row.summary.id, "ts": row.summary.time,
        "process_id": row.summary.process,
        "stream": match row.summary.stream {
            EchoStream::Stdout => 0, EchoStream::Stderr => 1,
        },
        "content": {"__b": base64::engine::general_purpose::STANDARD
            .encode(row.content.as_slice())},
    })
}

pub fn environment_json(environment: &crate::generated_wire::Environment) -> Value {
    Value::Object(environment.as_map().iter().map(|(key, value)| (
        String::from_utf8_lossy(key.as_slice()).into_owned(),
        Value::String(String::from_utf8_lossy(value.as_slice()).into_owned()),
    )).collect())
}
