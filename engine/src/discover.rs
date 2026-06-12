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

pub fn processes(box_id: i64) -> Value {
    let db = sqlar_path(box_id);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return json!([]);
    };
    let mut rows = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT id,tgid,ppid,parent_id,exe,argv FROM process ORDER BY id") {
        let it = st.query_map([], |r| {
            let argv: Option<String> = r.get(5)?;
            Ok(json!([
                r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                argv.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or_else(|| json!([])),
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
