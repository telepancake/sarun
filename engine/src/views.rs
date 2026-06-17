// Server-side windowed views over the per-box data the UI shows.
//
// Why this exists: a remote UI over a unix socket must NOT see the full
// dataset of a non-trivial box. A box with a million changed paths used to
// ship the entire list to the UI on every load, which the UI then re-filtered
// and re-rendered per keystroke (multi-second-per-key lag). The engine has
// the data here in-process — it's cheap to keep a materialized index and
// answer "give me rows [start..start+size)" requests.
//
// Protocol verbs (dispatched from control.rs as ui verbs):
//
//   view.open(kind, sid, filter)        -> {view_id, total}
//   view.window(view_id, start, size)   -> [row, ...]
//   view.filter(view_id, filter)        -> {total}
//   view.close(view_id)                 -> {ok: true}
//
// `filter` is null (no filter, everything visible) or a JSON array of
// Clauses. Filter changes recompute the Vec<usize> index table on the engine
// side; clients never touch a million-element list.
//
// Lifetime: views are stored in `Shared.views` keyed by a monotonic u64. The
// client is expected to call view.close when done. We do NOT auto-evict on
// timeout — the engine is single-instance and the data is just a Vec<usize>
// per pane, which costs ~8 bytes per row. A million changes is 8 MB.

use std::collections::HashMap;

use serde_json::Value;
use serde_json::json;

use crate::discover;
use crate::rules::{Clause, Join, Match, PathTarget, ProcFilterTarget, Subject, eval_clauses};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Changes,
    Procs,
    Outputs,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "changes" => Some(Kind::Changes),
            "procs" => Some(Kind::Procs),
            "outputs" => Some(Kind::Outputs),
            _ => None,
        }
    }
}

/// One materialized view. `source` is the full per-box list in render order
/// (for procs that means the pre-flattened tree rows, depth + connector
/// included). `idx` is the surviving indices after the current filter, or
/// the natural 0..N range when no filter is active.
pub struct View {
    pub kind: Kind,
    #[allow(dead_code)] pub sid: i64,
    pub source: Vec<Value>,
    pub idx: Vec<usize>,
    pub filter: Option<Vec<Clause>>,
    /// Per-row aux data the filter needs but the row itself doesn't carry:
    /// writer ids for changes, the (exe/cwd/argv) subject for procs/outputs.
    pub aux: ViewAux,
}

pub enum ViewAux {
    Changes(Vec<Vec<i64>>),    // writer ids per row index
    Procs(Vec<Subject>),       // subject per row index
    Outputs(Vec<Subject>),     // subject per row index
}

// ── building source rows per kind ────────────────────────────────────────────

fn source_changes(sid: i64) -> (Vec<Value>, Vec<Vec<i64>>) {
    // ONE sqlite scan: rows + their writer/last_writer in the same pass, so
    // per-row "ids" filter evaluation later is a Vec lookup not an RPC.
    let Some(conn) = discover::open_ro_for(sid) else {
        return (vec![], vec![]);
    };
    const S_IFMT: u32 = 0o170000;
    const S_IFCHR: u32 = 0o020000;
    const S_IFLNK: u32 = 0o120000;
    let mut rows = vec![];
    let mut ids = vec![];
    if let Ok(mut st) = conn.prepare(
        "SELECT name, mode, sz, writer, last_writer FROM sqlar ORDER BY name") {
        let it = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u32,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        });
        if let Ok(it) = it {
            for (name, mode, sz, w0, w1) in it.flatten() {
                let kind = if mode & S_IFMT == S_IFCHR { "deleted" }
                           else if mode & S_IFMT == S_IFLNK { "symlink" }
                           else { "changed" };
                rows.push(json!({"path": name, "kind": kind, "size": sz}));
                let mut wids = vec![];
                for w in [w0, w1].into_iter().flatten() {
                    if !wids.contains(&w) { wids.push(w); }
                }
                ids.push(wids);
            }
        }
    }
    (rows, ids)
}

fn source_outputs(sid: i64) -> (Vec<Value>, Vec<Subject>) {
    let rows = discover::outputs(sid);
    let rows: Vec<Value> = rows.as_array().cloned().unwrap_or_default();
    // Subject per row, via the row's process_id. Cache by pid so a chatty box
    // with thousands of outputs from a few processes pays the prov cost once.
    let mut cache: HashMap<i64, Subject> = HashMap::new();
    let mut subjects = Vec::with_capacity(rows.len());
    for r in &rows {
        let pid = r.get("process_id").and_then(Value::as_i64).unwrap_or(-1);
        let s = if pid < 0 {
            Subject::default()
        } else if let Some(s) = cache.get(&pid) {
            s.clone()
        } else {
            let prov = discover::proc_prov(sid, pid);
            let s = subject_from_prov(&prov);
            cache.insert(pid, s.clone());
            s
        };
        subjects.push(s);
    }
    (rows, subjects)
}

fn subject_from_prov(v: &Value) -> Subject {
    Subject {
        box_name: String::new(),
        exe: v.get("exe").and_then(Value::as_str).unwrap_or("").to_string(),
        cwd: v.get("cwd").and_then(Value::as_str).unwrap_or("").to_string(),
        argv: v.get("argv").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default(),
    }
}

fn source_procs(sid: i64) -> (Vec<Value>, Vec<Subject>) {
    // The flat process list and its DFS-flattened tree with depth/connector,
    // built once here on the engine side. Mirrors the old client-side
    // build_proc_tree but the ancestor `lookup` is a plain function call
    // instead of an RPC round-trip per process.
    let procs_v = discover::processes(sid);
    let procs: Vec<Value> = procs_v.as_array().cloned().unwrap_or_default();
    let roots: std::collections::HashSet<i64> = discover::proc_roots(sid)
        .as_array().cloned().unwrap_or_default()
        .iter().filter_map(Value::as_i64).collect();
    let rows = build_proc_tree(&procs, &roots, sid);
    // Subject per row for "exe"/"cwd"/"arg" filter kinds. Connector rows get a
    // default subject (they don't survive filter anyway when their kind isn't
    // "ids"); cwd is filled lazily via proc_prov, cached by rid.
    let mut cache: HashMap<i64, Subject> = HashMap::new();
    let mut subjects = Vec::with_capacity(rows.len());
    for r in &rows {
        let rid = r.get("rid").and_then(Value::as_i64).unwrap_or(-1);
        let s = if rid < 0 {
            Subject::default()
        } else if let Some(s) = cache.get(&rid) {
            s.clone()
        } else {
            let exe = r.get("exe").and_then(Value::as_str).unwrap_or("").to_string();
            let argv = r.get("argv").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            let cwd = discover::proc_prov(sid, rid)
                .get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
            let s = Subject { box_name: String::new(), exe, cwd, argv };
            cache.insert(rid, s.clone());
            s
        };
        subjects.push(s);
    }
    (rows, subjects)
}

// ── proc tree (lifted from ui.rs, with in-process ancestor lookup) ───────────

const PROC_TREE_DEPTH: usize = 64;

type NodeInfo = (i64, i64, Option<i64>, String, Vec<String>);

fn node_info(p: &Value) -> NodeInfo {
    let a = p.as_array();
    let g = |i: usize| a.and_then(|x| x.get(i)).and_then(Value::as_i64);
    let exe = a.and_then(|x| x.get(4)).and_then(Value::as_str).unwrap_or("").to_string();
    let argv = a.and_then(|x| x.get(5)).and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    (g(1).unwrap_or(0), g(2).unwrap_or(0), g(3), exe, argv)
}

fn build_proc_tree(procs: &[Value], roots: &std::collections::HashSet<i64>, sid: i64)
    -> Vec<Value>
{
    use std::collections::HashMap;
    use std::collections::HashSet;

    let mut members: HashMap<i64, NodeInfo> = HashMap::new();
    for p in procs {
        if let Some(rid) = p.as_array().and_then(|x| x.first()).and_then(Value::as_i64) {
            members.insert(rid, node_info(p));
        }
    }
    let mut nodes: HashMap<i64, NodeInfo> = members.clone();

    // root→self row-id path per member, unioning structural ancestors into
    // `nodes` so their connector rows can carry tgid/ppid/exe in the output.
    let mut member_paths: HashMap<i64, Vec<i64>> = HashMap::new();
    let member_ids: Vec<i64> = members.keys().copied().collect();
    for &start in &member_ids {
        let mut path = vec![];
        let mut seen = HashSet::new();
        let mut cur = start;
        for _ in 0..PROC_TREE_DEPTH {
            if seen.contains(&cur) { break; }
            seen.insert(cur);
            path.push(cur);
            if roots.contains(&cur) { break; }
            let got = match nodes.get(&cur) {
                Some(n) => n.clone(),
                None => {
                    let v = discover::proc_info(sid, cur);
                    let a = match v.as_array() { Some(a) => a, None => break };
                    let g = |i: usize| a.get(i).and_then(Value::as_i64);
                    let exe = a.get(3).and_then(Value::as_str).unwrap_or("").to_string();
                    let argv = a.get(4).and_then(Value::as_array)
                        .map(|x| x.iter().filter_map(Value::as_str).map(String::from).collect())
                        .unwrap_or_default();
                    (g(0).unwrap_or(0), g(1).unwrap_or(0), g(2), exe, argv)
                }
            };
            let Some(parent_id) = got.2 else { break };
            if parent_id == 0 { break; }
            if !nodes.contains_key(&parent_id) {
                let v = discover::proc_info(sid, parent_id);
                if let Some(a) = v.as_array() {
                    let g = |i: usize| a.get(i).and_then(Value::as_i64);
                    let exe = a.get(3).and_then(Value::as_str).unwrap_or("").to_string();
                    let argv = a.get(4).and_then(Value::as_array)
                        .map(|x| x.iter().filter_map(Value::as_str).map(String::from).collect())
                        .unwrap_or_default();
                    nodes.insert(parent_id,
                                 (g(0).unwrap_or(0), g(1).unwrap_or(0), g(2), exe, argv));
                } else {
                    break;
                }
            }
            cur = parent_id;
        }
        path.reverse();
        member_paths.insert(start, path);
    }

    let mut paths: Vec<Vec<i64>> = member_paths.values().cloned().collect();
    paths.sort();

    let mut emitted: HashSet<i64> = HashSet::new();
    let mut out = vec![];
    for path in &paths {
        for (depth, &rid) in path.iter().enumerate() {
            if !emitted.insert(rid) { continue; }
            let connector = !members.contains_key(&rid);
            let n = nodes.get(&rid).cloned()
                .unwrap_or((0, 0, None, String::new(), vec![]));
            out.push(json!({
                "rid": rid,
                "tgid": n.0,
                "ppid": n.1,
                "exe": n.3,
                "argv": n.4,
                "depth": depth,
                "connector": connector,
            }));
        }
    }
    out
}

// ── filter parsing + application ────────────────────────────────────────────

fn parse_filter(v: &Value) -> Option<Vec<Clause>> {
    let arr = v.as_array()?;
    if arr.is_empty() { return None; }
    let mut out = Vec::with_capacity(arr.len());
    for c in arr {
        let kind = c.get("kind").and_then(Value::as_str)?.to_string();
        let pattern = c.get("pattern").and_then(Value::as_str)?.to_string();
        let join = match c.get("join").and_then(Value::as_str) {
            Some("or") => Join::Or, _ => Join::And,
        };
        let negate = c.get("negate").and_then(Value::as_bool).unwrap_or(false);
        let enabled = c.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        out.push(Clause { m: Match { kind, pattern }, join, negate, enabled });
    }
    if out.iter().all(|c| !c.enabled) { return None; }
    Some(out)
}

fn rebuild_idx(view: &mut View) {
    let Some(clauses) = view.filter.as_ref() else {
        view.idx = (0..view.source.len()).collect();
        return;
    };
    match (&view.kind, &view.aux) {
        (Kind::Changes, ViewAux::Changes(ids)) => {
            view.idx = view.source.iter().enumerate().filter_map(|(i, c)| {
                let rel = c.get("path").and_then(Value::as_str).unwrap_or("");
                let row_ids = ids.get(i).cloned().unwrap_or_default();
                let t = PathTarget { rel, subject: Subject::default(), ids: row_ids };
                if eval_clauses(&t, clauses) { Some(i) } else { None }
            }).collect();
        }
        (Kind::Procs, ViewAux::Procs(subjects)) => {
            view.idx = view.source.iter().enumerate().filter_map(|(i, r)| {
                if r.get("connector").and_then(Value::as_bool) == Some(true) {
                    return None;   // connectors never survive a typed filter
                }
                let rid = r.get("rid").and_then(Value::as_i64).unwrap_or(-1);
                let subject = subjects.get(i).cloned().unwrap_or_default();
                let t = ProcFilterTarget { row_id: rid, subject };
                if eval_clauses(&t, clauses) { Some(i) } else { None }
            }).collect();
        }
        (Kind::Outputs, ViewAux::Outputs(subjects)) => {
            view.idx = view.source.iter().enumerate().filter_map(|(i, r)| {
                let pid = r.get("process_id").and_then(Value::as_i64).unwrap_or(-1);
                let subject = subjects.get(i).cloned().unwrap_or_default();
                let t = ProcFilterTarget { row_id: pid, subject };
                if eval_clauses(&t, clauses) { Some(i) } else { None }
            }).collect();
        }
        // (kind, aux) are constructed in lockstep in `open`, so the cross
        // arms are unreachable — fall back to the unfiltered set defensively.
        _ => view.idx = (0..view.source.len()).collect(),
    }
}

// ── verb dispatchers ────────────────────────────────────────────────────────

pub type Registry = HashMap<u64, View>;

pub fn open(reg: &mut Registry, next_id: &mut u64,
            kind_s: &str, sid: i64, filter_v: &Value) -> Value {
    let Some(kind) = Kind::parse(kind_s) else {
        return json!({"ok": false, "error": format!("unknown view kind {kind_s:?}")});
    };
    let (source, aux) = match kind {
        Kind::Changes => {
            let (s, a) = source_changes(sid);
            (s, ViewAux::Changes(a))
        }
        Kind::Procs => {
            let (s, a) = source_procs(sid);
            (s, ViewAux::Procs(a))
        }
        Kind::Outputs => {
            let (s, a) = source_outputs(sid);
            (s, ViewAux::Outputs(a))
        }
    };
    let filter = parse_filter(filter_v);
    let mut view = View { kind, sid, source, idx: vec![], filter, aux };
    rebuild_idx(&mut view);
    *next_id += 1;
    let id = *next_id;
    let total = view.idx.len();
    reg.insert(id, view);
    json!({"view_id": id, "total": total})
}

pub fn window(reg: &Registry, view_id: u64, start: usize, size: usize) -> Value {
    let Some(view) = reg.get(&view_id) else {
        return json!({"ok": false, "error": "unknown view_id"});
    };
    let end = start.saturating_add(size).min(view.idx.len());
    let start = start.min(end);
    let rows: Vec<Value> = view.idx[start..end]
        .iter()
        .map(|&i| view.source[i].clone())
        .collect();
    json!({"start": start, "rows": rows, "total": view.idx.len()})
}

pub fn set_filter(reg: &mut Registry, view_id: u64, filter_v: &Value) -> Value {
    let Some(view) = reg.get_mut(&view_id) else {
        return json!({"ok": false, "error": "unknown view_id"});
    };
    view.filter = parse_filter(filter_v);
    rebuild_idx(view);
    json!({"total": view.idx.len()})
}

pub fn close(reg: &mut Registry, view_id: u64) -> Value {
    reg.remove(&view_id);
    json!({"ok": true})
}
