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
//   view.window(view_id, start, size)   -> typed {start,total,rows} variant
//   view.filter(view_id, filter)        -> {total}
//   view.close(view_id)                 -> {ok: true}
//
// `filter` is the relation-generated optional FilterSpec. Filter changes
// recompute the Vec<usize> index table on the engine side; clients never touch
// a million-element list.
//
// Lifetime: views are stored in `Shared.views` keyed by a monotonic u64. The
// client is expected to call view.close when done. We do NOT auto-evict on
// timeout — the engine is single-instance and the data is just a Vec<usize>
// per pane, which costs ~8 bytes per row. A million changes is 8 MB.

use std::collections::HashMap;

use serde_json::Value;
use serde_json::json;

use crate::discover;
use crate::rules::{Clause, Join, Match, PathTarget, ProcFilterTarget, PipelineFilterTarget, EdgeFilterTarget, Subject, eval_clauses};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Changes,
    Procs,
    Outputs,
    Pipelines,
    BuildEdges,
}

impl Kind {
    fn from_wire(kind: crate::generated_wire::ViewKind) -> Self {
        match kind {
            crate::generated_wire::ViewKind::Changes => Kind::Changes,
            crate::generated_wire::ViewKind::Processes => Kind::Procs,
            crate::generated_wire::ViewKind::Outputs => Kind::Outputs,
            crate::generated_wire::ViewKind::Pipelines => Kind::Pipelines,
            crate::generated_wire::ViewKind::BuildEdges => Kind::BuildEdges,
        }
    }
}

/// One materialized view. `source` is the closed generated row sum holding the
/// full per-box list in render order (for procs that means the pre-flattened
/// tree rows, depth + connector included). `idx` is the surviving indices
/// after the current filter, or the natural 0..N range when no filter is active.
pub struct View {
    #[allow(dead_code)] pub sid: i64,
    pub source: ViewRows,
    pub idx: Vec<usize>,
    pub filter: Option<Vec<Clause>>,
    /// Per-row aux data the filter needs but the row itself doesn't carry:
    /// writer ids for changes, the (exe/cwd/argv) subject for procs/outputs.
    pub aux: ViewAux,
}

pub enum ViewRows {
    Changes(Vec<crate::generated_wire::ViewChangeRow>),
    Processes(Vec<crate::generated_wire::ViewProcessRow>),
    Outputs(Vec<crate::generated_wire::ViewOutputRow>),
    Pipelines(Vec<crate::generated_wire::PipelineRow>),
    BuildEdges(Vec<crate::generated_wire::BuildEdgeRow>),
}

impl ViewRows {
    fn len(&self) -> usize {
        match self {
            Self::Changes(rows) => rows.len(),
            Self::Processes(rows) => rows.len(),
            Self::Outputs(rows) => rows.len(),
            Self::Pipelines(rows) => rows.len(),
            Self::BuildEdges(rows) => rows.len(),
        }
    }
}

pub enum ViewAux {
    Changes(Vec<Vec<i64>>),    // writer ids per row index
    Procs(Vec<Subject>),       // subject per row index
    Outputs(Vec<Subject>),     // subject per row index
    Pipelines,                 // filter targets extracted from rows inline
    BuildEdges,                // filter targets extracted from rows inline
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
    // xattr rows: one entry per (file, key) pair. Pre-loaded into a
    // map keyed on the file's name so the tree-walk loop can emit a
    // "kind=xattr" child row right after each file leaf without a
    // per-leaf sqlite hit. Empty for boxes without the xattr table or
    // without any xattr writes.
    let mut xattr_by_name: std::collections::HashMap<String, Vec<(String, i64)>>
        = std::collections::HashMap::new();
    let has_xattr = conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='xattr'",
        [], |_| Ok(())).is_ok();
    if has_xattr {
        if let Ok(mut st) = conn.prepare(
            "SELECT name, key, length(value) FROM xattr ORDER BY name, key") {
            if let Ok(it) = st.query_map([], |r| Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))) {
                for (name, key, vlen) in it.flatten() {
                    xattr_by_name.entry(name).or_default().push((key, vlen));
                }
            }
        }
    }
    // Pull leaves first (sorted by name), then walk to build the tree.
    let mut leaves: Vec<(String, &'static str, i64, Vec<i64>)> = vec![];
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
                let mut wids = vec![];
                for w in [w0, w1].into_iter().flatten() {
                    if !wids.contains(&w) { wids.push(w); }
                }
                leaves.push((name, kind, sz, wids));
            }
        }
    }
    // Build the DIRECTORY TREE in DFS order: for each leaf, emit any new
    // ancestor directories as connector rows then the leaf itself. Sorted
    // by name already gives us DFS order. Mirrors the Python prototype's
    // changes-as-tree view (CLAUDE.md: "_ch_rows is the DFS-ordered render
    // list [(key, name, depth, connector)]").
    let mut rows = vec![];
    let mut ids = vec![];
    let mut prev: Vec<String> = vec![];
    // Files whose only sqlar row is data-NULL but DO have xattrs — those
    // need to still emit xattr children. We catch them by iterating
    // xattr_by_name entries we never consumed (the file may not appear
    // in sqlar at all if it's an xattr-only modification of a lower).
    let mut consumed: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for (name, kind, sz, wids) in leaves {
        let parts: Vec<String> = name.split('/')
            .filter(|s| !s.is_empty()).map(String::from).collect();
        if parts.is_empty() { continue; }
        let leaf_depth = parts.len() - 1;
        let mut common = 0;
        while common < parts.len() && common < prev.len()
            && parts[common] == prev[common] {
            common += 1;
        }
        // Emit connector rows for any newly-entered directory levels.
        for d in common..leaf_depth {
            rows.push(json!({
                "path": parts[..=d].join("/"),
                "name": parts[d],
                "kind": "dir",
                "size": 0,
                "depth": d,
                "connector": true,
            }));
            ids.push(vec![]);
        }
        rows.push(json!({
            "path": name.clone(),
            "name": parts[leaf_depth],
            "kind": kind,
            "size": sz,
            "depth": leaf_depth,
            "connector": false,
        }));
        ids.push(wids);
        // Xattr children: one row per (file, key) pair, indented one
        // level under the file leaf. Their "name" is the key
        // (e.g. user.foo), "size" is the value byte length, "kind" is
        // "xattr". They're leaves themselves (not connectors), so the
        // cursor lands on them and they're decorate/diff-targetable
        // later (the apply/discard path can stamp them as their own
        // unit instead of being bundled with the file).
        if let Some(xs) = xattr_by_name.get(&name) {
            for (key, vlen) in xs {
                rows.push(json!({
                    "path": format!("{name}#xattr={key}"),
                    "name": key.clone(),
                    "kind": "xattr",
                    "size": vlen,
                    "depth": leaf_depth + 1,
                    "connector": false,
                    "xattr_for": name.clone(),
                    "xattr_key": key.clone(),
                }));
                ids.push(vec![]);
            }
            consumed.insert(name.clone());
        }
        prev = parts;
    }
    // Xattr-only files: their sqlar row never existed (a passthrough
    // file the box just chattr-tagged, say). Append them at the end,
    // sorted by name, each with its own minimal connector chain so
    // they slot into the right directory.
    let mut leftover: Vec<(String, Vec<(String, i64)>)> = xattr_by_name.into_iter()
        .filter(|(n, _)| !consumed.contains(n))
        .collect();
    leftover.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, xs) in leftover {
        let parts: Vec<String> = name.split('/')
            .filter(|s| !s.is_empty()).map(String::from).collect();
        if parts.is_empty() { continue; }
        let leaf_depth = parts.len() - 1;
        let mut common = 0;
        while common < parts.len() && common < prev.len()
            && parts[common] == prev[common] {
            common += 1;
        }
        for d in common..leaf_depth {
            rows.push(json!({
                "path": parts[..=d].join("/"),
                "name": parts[d],
                "kind": "dir",
                "size": 0,
                "depth": d,
                "connector": true,
            }));
            ids.push(vec![]);
        }
        // Synthetic parent file row so the xattrs aren't hanging in
        // the air: kind="xattr-only" carries a hint glyph distinct
        // from a real "changed" file.
        rows.push(json!({
            "path": name.clone(),
            "name": parts[leaf_depth],
            "kind": "xattr-only",
            "size": 0,
            "depth": leaf_depth,
            "connector": false,
        }));
        ids.push(vec![]);
        for (key, vlen) in xs {
            rows.push(json!({
                "path": format!("{name}#xattr={key}"),
                "name": key.clone(),
                "kind": "xattr",
                "size": vlen,
                "depth": leaf_depth + 1,
                "connector": false,
                "xattr_for": name.clone(),
                "xattr_key": key,
            }));
            ids.push(vec![]);
        }
        prev = parts;
    }
    (rows, ids)
}

fn source_outputs(sid: i64) -> (Vec<Value>, Vec<Subject>) {
    let rows = discover::outputs(sid);
    let rows: Vec<Value> = rows.as_array().cloned().unwrap_or_default();
    // The outputs index needs a per-row (exe, tgid) tag for the prototype's
    // "Process" column ("<basename>·<tgid>"). Pull the whole process table
    // ONCE — one sqlite scan, indexed by row id — so we don't run a
    // proc_prov / process row query per output.
    let pmap: HashMap<i64, (String, i64)> = discover::open_ro_for(sid)
        .and_then(|c| c.prepare("SELECT id, tgid, exe FROM process").ok().map(|mut st| {
            st.query_map([], |r| Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))).ok()
              .map(|it| it.flatten().map(|(id, tg, ex)| (id, (ex, tg))).collect())
              .unwrap_or_default()
        }))
        .unwrap_or_default();
    // Subject + (exe, tgid) annotations. Subject is for the filter; the
    // annotations are embedded into each row so the UI doesn't have to RPC
    // to render the index.
    let mut rows_out = Vec::with_capacity(rows.len());
    let mut subjects = Vec::with_capacity(rows.len());
    for r in rows {
        let pid = r.get("process_id").and_then(Value::as_i64).unwrap_or(-1);
        let (exe, tgid) = pmap.get(&pid).cloned().unwrap_or_default();
        let argv: Vec<String> = vec![]; // outputs filter doesn't use arg
        subjects.push(Subject { box_name: String::new(),
                                exe: exe.clone(), cwd: String::new(), argv });
        // Tack exe + tgid onto the row so the renderer can show them
        // without another round-trip per row.
        let mut r2 = r;
        if let Some(obj) = r2.as_object_mut() {
            obj.insert("exe".into(), Value::String(exe));
            obj.insert("tgid".into(), Value::Number(tgid.into()));
        }
        rows_out.push(r2);
    }
    (rows_out, subjects)
}

fn source_procs(sid: i64, running_only: bool) -> (Vec<Value>, Vec<Subject>) {
    // The flat process list and its DFS-flattened tree with depth/connector,
    // built once here on the engine side. Mirrors the old client-side
    // build_proc_tree but the ancestor `lookup` is a plain function call
    // instead of an RPC round-trip per process.
    let procs_v = discover::processes(sid);
    let mut procs: Vec<Value> = procs_v.as_array().cloned().unwrap_or_default();
    // running_only (live box, default): keep only rows whose process (tgid, col 1)
    // is still alive — a pidfd probe (control::pid_alive), no stored liveness. The
    // surviving tgids ARE the filter; build_proc_tree still pulls their ancestors
    // as connectors. The caller only sets this for a live box.
    if running_only {
        use std::collections::HashMap;
        let mut alive: HashMap<i64, bool> = HashMap::new();
        procs.retain(|p| {
            let tgid = p.as_array().and_then(|a| a.get(1)).and_then(Value::as_i64).unwrap_or(0);
            tgid > 0 && *alive.entry(tgid).or_insert_with(|| crate::control::pid_alive(tgid as i32))
        });
    }
    let roots: std::collections::HashSet<i64> = discover::proc_roots(sid)
        .as_array().cloned().unwrap_or_default()
        .iter().filter_map(Value::as_i64).collect();
    let rows = build_proc_tree(&procs, &roots, sid);
    // Subject per row for "exe"/"cwd"/"arg" filter kinds. `cwd` lives in the
    // `process` table but discover::processes() doesn't include it; pull all
    // (rid → cwd) pairs in ONE sqlite scan so a million-row procs view isn't
    // a million per-row queries (which made view.open take ~30 s at scale).
    let cwd_by_rid: HashMap<i64, String> = discover::open_ro_for(sid)
        .and_then(|c| c.prepare("SELECT id, cwd FROM process").ok().map(|mut st| {
            st.query_map([], |r| Ok((r.get::<_, i64>(0)?,
                                     r.get::<_, Option<String>>(1)?.unwrap_or_default())))
              .ok().map(|it| it.flatten().collect::<HashMap<_, _>>())
              .unwrap_or_default()
        }))
        .unwrap_or_default();
    let mut subjects = Vec::with_capacity(rows.len());
    for r in &rows {
        let rid = r.get("rid").and_then(Value::as_i64).unwrap_or(-1);
        let exe = r.get("exe").and_then(Value::as_str).unwrap_or("").to_string();
        let argv = r.get("argv").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default();
        let cwd = cwd_by_rid.get(&rid).cloned().unwrap_or_default();
        subjects.push(Subject { box_name: String::new(), exe, cwd, argv });
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

fn source_pipelines(sid: i64) -> Vec<Value> {
    let v = discover::brushprov(sid);
    v.as_array().cloned().unwrap_or_default()
}

fn source_build_edges(sid: i64) -> Vec<Value> {
    let v = discover::build_edges(sid);
    v.as_array().cloned().unwrap_or_default()
}

fn wire_path(value: &str) -> Result<crate::generated_wire::Path, String> {
    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
        .map_err(|error| format!("path exceeds relation bound: {error:?}"))
}

fn wire_os_string(value: &str) -> Result<crate::generated_wire::OsString, String> {
    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
        .map_err(|error| format!("OS string exceeds relation bound: {error:?}"))
}

fn wire_text(value: &str) -> Result<crate::wire::BoundedText<{crate::generated_wire::LIMIT_TEXT_BYTES}>, String> {
    crate::wire::BoundedText::new(value.into())
        .map_err(|error| format!("text exceeds relation bound: {error:?}"))
}

fn required_u64(value: Option<&Value>, field: &str) -> Result<u64, String> {
    value
        .and_then(|value| value.as_u64().or_else(|| value.as_i64()?.try_into().ok()))
        .ok_or_else(|| format!("view row has invalid {field}"))
}

fn optional_u64(value: Option<&Value>, field: &str) -> Result<Option<u64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => required_u64(Some(value), field).map(Some),
    }
}

fn optional_u32(value: Option<&Value>, field: &str) -> Result<Option<u32>, String> {
    optional_u64(value, field)?
        .map(|value| u32::try_from(value).map_err(|_| format!("view row {field} exceeds u32")))
        .transpose()
}

fn typed_change_rows(rows: Vec<Value>) -> Result<Vec<crate::generated_wire::ViewChangeRow>, String> {
    use crate::generated_wire::{ChangeKind, ViewChangeRow};
    rows.into_iter().map(|row| {
        let path = row.get("path").and_then(Value::as_str)
            .ok_or("change view row has no path")?;
        let name = row.get("name").and_then(Value::as_str)
            .ok_or("change view row has no name")?;
        let kind = match row.get("kind").and_then(Value::as_str) {
            Some("changed") => ChangeKind::Changed,
            Some("deleted") => ChangeKind::Deleted,
            Some("symlink") => ChangeKind::Symlink,
            Some("created") => ChangeKind::Created,
            Some("modified") => ChangeKind::Modified,
            Some("xattr") => ChangeKind::Xattr,
            Some("dir") => ChangeKind::Directory,
            Some("xattr-only") => ChangeKind::XattrOnly,
            Some(kind) => return Err(format!("unknown change view kind {kind:?}")),
            None => return Err("change view row has no kind".into()),
        };
        Ok(ViewChangeRow {
            path: wire_path(path)?,
            name: wire_os_string(name)?,
            kind,
            size: required_u64(row.get("size"), "change size")?,
            depth: u32::try_from(required_u64(row.get("depth"), "change depth")?)
                .map_err(|_| "change view depth exceeds u32")?,
            connector: row.get("connector").and_then(Value::as_bool)
                .ok_or("change view row has no connector flag")?,
            xattr_for: row.get("xattr_for").and_then(Value::as_str)
                .map(wire_path).transpose()?,
            xattr_key: row.get("xattr_key").and_then(Value::as_str)
                .map(wire_os_string).transpose()?,
        })
    }).collect()
}

fn typed_process_rows(rows: Vec<Value>) -> Result<Vec<crate::generated_wire::ViewProcessRow>, String> {
    use crate::generated_wire::{LIMIT_COMMAND_ITEMS, ViewProcessRow};
    rows.into_iter().map(|row| {
        let argv = row.get("argv").and_then(Value::as_array)
            .ok_or("process view row has no argv")?
            .iter().map(|word| {
                word.as_str().ok_or_else(|| "process argv contains non-text".into())
                    .and_then(wire_os_string)
            }).collect::<Result<Vec<_>, String>>()?;
        Ok(ViewProcessRow {
            id: required_u64(row.get("rid"), "process row id")?,
            tgid: optional_u32(row.get("tgid"), "process tgid")?.filter(|value| *value != 0),
            ppid: optional_u32(row.get("ppid"), "process ppid")?.filter(|value| *value != 0),
            executable: wire_path(row.get("exe").and_then(Value::as_str)
                .ok_or("process view row has no executable")?)?,
            argv: crate::wire::BoundedVec::<_, 0, LIMIT_COMMAND_ITEMS>::new(argv)
                .map_err(|error| format!("process argv exceeds relation bound: {error:?}"))?,
            depth: u32::try_from(required_u64(row.get("depth"), "process depth")?)
                .map_err(|_| "process view depth exceeds u32")?,
            connector: row.get("connector").and_then(Value::as_bool)
                .ok_or("process view row has no connector flag")?,
        })
    }).collect()
}

fn typed_output_rows(rows: Vec<Value>) -> Result<Vec<crate::generated_wire::ViewOutputRow>, String> {
    use crate::generated_wire::{EchoStream, OutputRow, ViewOutputRow};
    rows.into_iter().map(|row| {
        let stream = match row.get("stream").and_then(Value::as_i64) {
            Some(0) => EchoStream::Stdout,
            Some(1) => EchoStream::Stderr,
            Some(stream) => return Err(format!("unknown captured output stream {stream}")),
            None => return Err("output view row has no stream".into()),
        };
        Ok(ViewOutputRow {
            output: OutputRow {
                id: required_u64(row.get("id"), "output row id")?,
                time: row.get("ts").and_then(Value::as_f64)
                    .ok_or("output view row has no timestamp")?,
                process: optional_u64(row.get("process_id"), "output process id")?,
                stream,
                length: required_u64(row.get("len"), "output length")?,
            },
            executable: wire_path(row.get("exe").and_then(Value::as_str)
                .ok_or("output view row has no executable")?)?,
            tgid: optional_u32(row.get("tgid"), "output tgid")?.filter(|value| *value != 0),
        })
    }).collect()
}

fn typed_pipeline_stage(value: &Value) -> Result<crate::generated_wire::PipelineStage, String> {
    use crate::generated_wire::{LIMIT_STAGE_ITEMS, PipelineStage};
    match value.get("kind").and_then(Value::as_str) {
        Some("simple") => {
            let words = value.get("words").and_then(Value::as_array)
                .ok_or("simple pipeline stage has no words")?
                .iter().map(|word| {
                    word.as_str().ok_or_else(|| "pipeline word is not text".into())
                        .and_then(wire_os_string)
                }).collect::<Result<Vec<_>, String>>()?;
            Ok(PipelineStage::Simple {
                words: crate::wire::BoundedVec::<_, 0, LIMIT_STAGE_ITEMS>::new(words)
                    .map_err(|error| format!("pipeline words exceed relation bound: {error:?}"))?,
                redirects: u32::try_from(required_u64(value.get("redirects"), "redirect count")?)
                    .map_err(|_| "pipeline redirect count exceeds u32")?,
            })
        }
        Some("compound") => Ok(PipelineStage::Compound {
            redirects: u32::try_from(required_u64(value.get("redirects"), "redirect count")?)
                .map_err(|_| "pipeline redirect count exceeds u32")?,
            text: wire_text(value.get("text").and_then(Value::as_str)
                .ok_or("compound pipeline stage has no text")?)?,
        }),
        Some("function") => Ok(PipelineStage::Function {
            text: wire_text(value.get("text").and_then(Value::as_str)
                .ok_or("function pipeline stage has no text")?)?,
        }),
        Some("extended_test") => Ok(PipelineStage::ExtendedTest {
            text: wire_text(value.get("text").and_then(Value::as_str)
                .ok_or("extended-test pipeline stage has no text")?)?,
        }),
        Some(kind) => Err(format!("unknown pipeline stage kind {kind:?}")),
        None => Err("pipeline stage has no kind".into()),
    }
}

fn typed_pipeline_provenance(value: &Value) -> Result<crate::generated_wire::PipelineProvenance, String> {
    use crate::generated_wire::{LIMIT_COLLECTION_ITEMS, LIMIT_STAGE_ITEMS, PipelineProvenance};
    let stages = value.get("stage_detail").and_then(Value::as_array)
        .ok_or("pipeline record has no stage detail")?
        .iter().map(typed_pipeline_stage).collect::<Result<Vec<_>, _>>()?;
    let targets = value.get("out_targets").and_then(Value::as_array)
        .ok_or("pipeline record has no output targets")?
        .iter().map(|target| {
            target.as_str().ok_or_else(|| "pipeline target is not text".into())
                .and_then(wire_path)
        }).collect::<Result<Vec<_>, String>>()?;
    Ok(PipelineProvenance {
        command: wire_text(value.get("cmd").and_then(Value::as_str)
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
            .map(wire_path).transpose()?,
    })
}

fn typed_pipeline_rows(rows: Vec<Value>) -> Result<Vec<crate::generated_wire::PipelineRow>, String> {
    use crate::generated_wire::{LIMIT_COLLECTION_ITEMS, PipelineRow};
    rows.into_iter().map(|row| {
        let processes = row.get("processes").and_then(Value::as_array)
            .ok_or("pipeline row has no process list")?
            .iter().map(|value| required_u64(Some(value), "pipeline process id"))
            .collect::<Result<Vec<_>, _>>()?;
        let done_at = row.get("done_ts").and_then(Value::as_f64).filter(|value| *value != 0.0);
        let exit_code = row.get("exit_code").and_then(Value::as_i64).filter(|value| *value != -1)
            .map(|value| i32::try_from(value).map_err(|_| "pipeline exit code exceeds i32"))
            .transpose()?;
        Ok(PipelineRow {
            id: required_u64(row.get("id"), "pipeline row id")?,
            time: row.get("ts").and_then(Value::as_f64)
                .ok_or("pipeline row has no timestamp")?,
            command: wire_text(row.get("cmd").and_then(Value::as_str)
                .ok_or("pipeline row has no command")?)?,
            record: row.get("record").filter(|value| !value.is_null())
                .map(typed_pipeline_provenance).transpose()?,
            pipeline: optional_u64(row.get("pipeline"), "pipeline sequence")?,
            spawned_at: row.get("spawn_ts").and_then(Value::as_f64),
            done_at,
            nested: row.get("nested").and_then(Value::as_bool).unwrap_or(false),
            uid: optional_u64(row.get("uid"), "pipeline uid")?.filter(|value| *value != 0),
            parent_uid: optional_u64(row.get("parent_uid"), "pipeline parent uid")?
                .filter(|value| *value != 0),
            exit_code,
            processes: crate::wire::BoundedVec::<_, 0, LIMIT_COLLECTION_ITEMS>::new(processes)
                .map_err(|error| format!("pipeline process list exceeds relation bound: {error:?}"))?,
        })
    }).collect()
}

fn typed_build_edge_rows(rows: Vec<Value>) -> Result<Vec<crate::generated_wire::BuildEdgeRow>, String> {
    use crate::generated_wire::{BuildEdgeRow, LIMIT_COLLECTION_ITEMS};
    rows.into_iter().map(|row| {
        let paths = |field: &str| -> Result<Vec<crate::generated_wire::Path>, String> {
            row.get(field).and_then(Value::as_array)
                .ok_or_else(|| format!("build edge has no {field} list"))?
                .iter().map(|value| {
                    value.as_str().ok_or_else(|| format!("build edge {field} is not text"))
                        .and_then(wire_path)
                }).collect()
        };
        let outputs = paths("outs")?;
        let inputs = paths("ins")?;
        Ok(BuildEdgeRow {
            id: required_u64(row.get("id"), "build edge row id")?,
            time: row.get("ts").and_then(Value::as_f64)
                .ok_or("build edge has no timestamp")?,
            outputs: crate::wire::BoundedVec::<_, 1, LIMIT_COLLECTION_ITEMS>::new(outputs)
                .map_err(|error| format!("build edge outputs exceed relation bound: {error:?}"))?,
            inputs: crate::wire::BoundedVec::<_, 0, LIMIT_COLLECTION_ITEMS>::new(inputs)
                .map_err(|error| format!("build edge inputs exceed relation bound: {error:?}"))?,
            command: row.get("cmd").and_then(Value::as_str).map(wire_text).transpose()?,
            started_at: row.get("started_ts").and_then(Value::as_f64),
            ended_at: row.get("ended_ts").and_then(Value::as_f64),
            exit_code: row.get("exit_code").and_then(Value::as_i64)
                .map(|value| i32::try_from(value).map_err(|_| "build edge exit code exceeds i32"))
                .transpose()?,
            output_excerpt: row.get("output_excerpt").and_then(Value::as_str)
                .map(wire_text).transpose()?,
        })
    }).collect()
}

// ── filter parsing + application ────────────────────────────────────────────

fn relation_filter(
    filter: Option<&crate::generated_wire::FilterSpec>,
) -> Option<Vec<Clause>> {
    let clauses = filter?.as_slice();
    if clauses.is_empty() { return None; }
    let out = clauses.iter().map(|clause| {
        let kind = match clause.kind {
            crate::generated_wire::FilterKind::Path => "path",
            crate::generated_wire::FilterKind::Box => "box",
            crate::generated_wire::FilterKind::Exe => "exe",
            crate::generated_wire::FilterKind::Cwd => "cwd",
            crate::generated_wire::FilterKind::Arg => "arg",
            crate::generated_wire::FilterKind::Ids => "ids",
            crate::generated_wire::FilterKind::Err => "err",
            crate::generated_wire::FilterKind::Cmd => "cmd",
            crate::generated_wire::FilterKind::Target => "target",
        };
        let join = match clause.join {
            crate::generated_wire::FilterJoin::And => Join::And,
            crate::generated_wire::FilterJoin::Or => Join::Or,
        };
        Clause {
            m: Match {
                kind: kind.into(),
                pattern: clause.pattern.as_str().into(),
            },
            join,
            negate: clause.negated,
            enabled: clause.enabled,
        }
    }).collect::<Vec<_>>();
    if out.iter().all(|c| !c.enabled) { return None; }
    Some(out)
}

fn rebuild_idx(view: &mut View) {
    let Some(clauses) = view.filter.as_ref() else {
        view.idx = (0..view.source.len()).collect();
        return;
    };
    match (&view.source, &view.aux) {
        (ViewRows::Changes(rows), ViewAux::Changes(ids)) => {
            view.idx = rows.iter().enumerate().filter_map(|(i, row)| {
                // A filter is a "show me these rows" set — connectors are
                // tree-scaffolding, not changes to match against; they only
                // appear in the unfiltered tree view.
                if row.connector {
                    return None;
                }
                let rel = std::str::from_utf8(row.path.as_slice()).unwrap_or("");
                let row_ids = ids.get(i).cloned().unwrap_or_default();
                let t = PathTarget { rel, subject: Subject::default(), ids: row_ids };
                if eval_clauses(&t, clauses) { Some(i) } else { None }
            }).collect();
        }
        (ViewRows::Processes(rows), ViewAux::Procs(subjects)) => {
            view.idx = rows.iter().enumerate().filter_map(|(i, row)| {
                if row.connector {
                    return None;   // connectors never survive a typed filter
                }
                let rid = i64::try_from(row.id).unwrap_or(i64::MAX);
                let subject = subjects.get(i).cloned().unwrap_or_default();
                let t = ProcFilterTarget { row_id: rid, subject, err: false };
                if eval_clauses(&t, clauses) { Some(i) } else { None }
            }).collect();
        }
        (ViewRows::Outputs(rows), ViewAux::Outputs(subjects)) => {
            view.idx = rows.iter().enumerate().filter_map(|(i, row)| {
                let pid = row.output.process
                    .and_then(|id| i64::try_from(id).ok()).unwrap_or(-1);
                let subject = subjects.get(i).cloned().unwrap_or_default();
                let err = row.output.stream == crate::generated_wire::EchoStream::Stderr;
                let t = ProcFilterTarget { row_id: pid, subject, err };
                if eval_clauses(&t, clauses) { Some(i) } else { None }
            }).collect();
        }
        (ViewRows::Pipelines(rows), ViewAux::Pipelines) => {
            view.idx = rows.iter().enumerate().filter_map(|(i, row)| {
                let row_id = i64::try_from(row.id).unwrap_or(i64::MAX);
                let cmd = row.command.as_str().to_string();
                let err = row.exit_code.is_some_and(|code| code > 0);
                let t = PipelineFilterTarget { row_id, cmd, err };
                if eval_clauses(&t, clauses) { Some(i) } else { None }
            }).collect();
        }
        (ViewRows::BuildEdges(rows), ViewAux::BuildEdges) => {
            view.idx = rows.iter().enumerate().filter_map(|(i, row)| {
                let row_id = i64::try_from(row.id).unwrap_or(i64::MAX);
                let targets = row.outputs.as_slice().iter()
                    .map(|path| String::from_utf8_lossy(path.as_slice()).into_owned())
                    .collect();
                let cmd = row.command.as_ref().map(|value| value.as_str())
                    .unwrap_or("").to_string();
                let err = row.exit_code.is_some_and(|code| code != 0);
                let t = EdgeFilterTarget { row_id, targets, cmd, err };
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

pub fn open(
    reg: &mut Registry,
    next_id: &mut u64,
    wire_kind: crate::generated_wire::ViewKind,
    sid: i64,
    filter: Option<crate::generated_wire::FilterSpec>,
    procs_running_only: bool,
) -> Result<crate::generated_wire::ViewOpenResult, String> {
    let kind = Kind::from_wire(wire_kind);
    let (source, aux) = match kind {
        Kind::Changes => {
            let (s, a) = source_changes(sid);
            (ViewRows::Changes(typed_change_rows(s)?), ViewAux::Changes(a))
        }
        Kind::Procs => {
            let (s, a) = source_procs(sid, procs_running_only);
            (ViewRows::Processes(typed_process_rows(s)?), ViewAux::Procs(a))
        }
        Kind::Outputs => {
            let (s, a) = source_outputs(sid);
            (ViewRows::Outputs(typed_output_rows(s)?), ViewAux::Outputs(a))
        }
        Kind::Pipelines => {
            let s = source_pipelines(sid);
            (ViewRows::Pipelines(typed_pipeline_rows(s)?), ViewAux::Pipelines)
        }
        Kind::BuildEdges => {
            let s = source_build_edges(sid);
            (ViewRows::BuildEdges(typed_build_edge_rows(s)?), ViewAux::BuildEdges)
        }
    };
    let filter = relation_filter(filter.as_ref());
    let mut view = View { sid, source, idx: vec![], filter, aux };
    rebuild_idx(&mut view);
    *next_id = next_id.checked_add(1).ok_or("view identity exhausted")?;
    let id = *next_id;
    let total = view.idx.len();
    reg.insert(id, view);
    Ok(crate::generated_wire::ViewOpenResult {
        view: id,
        total: u64::try_from(total).map_err(|_| "view row count exceeds u64")?,
    })
}

pub fn window(
    reg: &Registry,
    view_id: u64,
    start: u64,
    size: u64,
) -> Result<crate::generated_wire::ViewWindow, String> {
    use crate::generated_wire::{LIMIT_COLLECTION_ITEMS, ViewWindow};
    let Some(view) = reg.get(&view_id) else {
        return Err("unknown view_id".into());
    };
    let start = usize::try_from(start).map_err(|_| "view window start exceeds usize")?;
    let size = usize::try_from(size).map_err(|_| "view window size exceeds usize")?;
    let end = start.saturating_add(size).min(view.idx.len());
    let start = start.min(end);
    let start_wire = u64::try_from(start).map_err(|_| "view window start exceeds u64")?;
    let total = u64::try_from(view.idx.len()).map_err(|_| "view row count exceeds u64")?;
    macro_rules! window_rows {
        ($rows:expr, $variant:ident) => {{
            let selected = view.idx[start..end].iter()
                .map(|&index| $rows[index].clone()).collect();
            let rows = crate::wire::BoundedVec::<_, 0, LIMIT_COLLECTION_ITEMS>::new(selected)
                .map_err(|error| format!("view window exceeds relation bound: {error:?}"))?;
            ViewWindow::$variant { start: start_wire, total, rows }
        }};
    }
    Ok(match &view.source {
        ViewRows::Changes(rows) => window_rows!(rows, Changes),
        ViewRows::Processes(rows) => window_rows!(rows, Processes),
        ViewRows::Outputs(rows) => window_rows!(rows, Outputs),
        ViewRows::Pipelines(rows) => window_rows!(rows, Pipelines),
        ViewRows::BuildEdges(rows) => window_rows!(rows, BuildEdges),
    })
}

pub fn set_filter(
    reg: &mut Registry,
    view_id: u64,
    filter: Option<crate::generated_wire::FilterSpec>,
) -> Result<crate::generated_wire::ViewFilterResult, String> {
    let Some(view) = reg.get_mut(&view_id) else {
        return Err("unknown view_id".into());
    };
    view.filter = relation_filter(filter.as_ref());
    rebuild_idx(view);
    Ok(crate::generated_wire::ViewFilterResult {
        total: u64::try_from(view.idx.len()).map_err(|_| "view row count exceeds u64")?,
    })
}

pub fn find(reg: &Registry, view_id: u64, target_id: u64) -> Result<Option<u64>, String> {
    let Some(view) = reg.get(&view_id) else {
        return Err("unknown view_id".into());
    };
    for (pos, &i) in view.idx.iter().enumerate() {
        let id = match &view.source {
            ViewRows::Changes(_) => None,
            ViewRows::Processes(rows) => Some(rows[i].id),
            ViewRows::Outputs(rows) => Some(rows[i].output.id),
            ViewRows::Pipelines(rows) => Some(rows[i].id),
            ViewRows::BuildEdges(rows) => Some(rows[i].id),
        };
        if id == Some(target_id) {
            return u64::try_from(pos).map(Some)
                .map_err(|_| "view position exceeds u64".into());
        }
    }
    Ok(None)
}

pub fn close(reg: &mut Registry, view_id: u64) -> Result<(), String> {
    reg.remove(&view_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated_wire::{ChangeKind, EchoStream, PipelineStage, ViewWindow};

    #[test]
    fn every_view_row_family_materializes_into_its_closed_window_variant() {
        let change = typed_change_rows(vec![json!({
            "path": "src/main.rs", "name": "main.rs", "kind": "changed",
            "size": 12, "depth": 1, "connector": false,
        })]).unwrap();
        assert_eq!(change[0].kind, ChangeKind::Changed);

        let process = typed_process_rows(vec![json!({
            "rid": 7, "tgid": 41, "ppid": 1, "exe": "/bin/sh",
            "argv": ["sh", "-c", "true"], "depth": 0, "connector": false,
        })]).unwrap();
        assert_eq!(process[0].tgid, Some(41));

        let output = typed_output_rows(vec![json!({
            "id": 8, "ts": 1.25, "process_id": 7, "stream": 1, "len": 4,
            "exe": "/bin/sh", "tgid": 41,
        })]).unwrap();
        assert_eq!(output[0].output.stream, EchoStream::Stderr);

        let pipeline = typed_pipeline_rows(vec![json!({
            "id": 9, "ts": 1.0, "cmd": "echo hi > out", "pipeline": 3,
            "spawn_ts": 1.1, "nested": true, "uid": 12, "parent_uid": 4,
            "done_ts": 1.2, "exit_code": 0, "processes": [7],
            "record": {
                "cmd": "echo hi > out", "bang": false, "stages": 1,
                "stage_detail": [{
                    "kind": "simple", "words": ["echo", "hi"], "redirects": 1
                }],
                "out_targets": ["out"], "uid": 12, "parent_uid": 4,
                "seq": 3, "spawn_ts": 1.1, "nested": true,
                "edge_out": "out"
            }
        })]).unwrap();
        let record = pipeline[0].record.as_ref().unwrap();
        assert!(matches!(record.stages.as_slice()[0], PipelineStage::Simple { .. }));
        assert_eq!(record.edge_output.as_ref().unwrap().as_slice(), b"out");

        let edge = typed_build_edge_rows(vec![json!({
            "id": 10, "ts": 2.0, "outs": ["out"], "ins": ["in"],
            "cmd": "cc in -o out", "started_ts": 2.1, "ended_ts": 2.2,
            "exit_code": 0, "output_excerpt": "ok",
        })]).unwrap();
        assert_eq!(edge[0].outputs.as_slice()[0].as_slice(), b"out");

        let families = [
            ViewRows::Changes(change),
            ViewRows::Processes(process),
            ViewRows::Outputs(output),
            ViewRows::Pipelines(pipeline),
            ViewRows::BuildEdges(edge),
        ];
        for (index, source) in families.into_iter().enumerate() {
            let view_id = index as u64 + 1;
            let mut registry = Registry::new();
            registry.insert(view_id, View {
                sid: 1,
                source,
                idx: vec![0],
                filter: None,
                aux: match index {
                    0 => ViewAux::Changes(vec![vec![]]),
                    1 => ViewAux::Procs(vec![Subject::default()]),
                    2 => ViewAux::Outputs(vec![Subject::default()]),
                    3 => ViewAux::Pipelines,
                    _ => ViewAux::BuildEdges,
                },
            });
            let window = window(&registry, view_id, 0, 1).unwrap();
            assert_eq!(window.code(), index as u64 + 1);
            match window {
                ViewWindow::Changes { rows, .. } => assert_eq!(rows.as_slice().len(), 1),
                ViewWindow::Processes { rows, .. } => assert_eq!(rows.as_slice().len(), 1),
                ViewWindow::Outputs { rows, .. } => assert_eq!(rows.as_slice().len(), 1),
                ViewWindow::Pipelines { rows, .. } => assert_eq!(rows.as_slice().len(), 1),
                ViewWindow::BuildEdges { rows, .. } => assert_eq!(rows.as_slice().len(), 1),
            }
        }
    }
}
