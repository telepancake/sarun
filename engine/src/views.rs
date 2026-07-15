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

fn source_changes(
    sid: i64,
) -> Result<(Vec<crate::generated_wire::ViewChangeRow>, Vec<Vec<i64>>), String> {
    // ONE sqlite scan: rows + their writer/last_writer in the same pass, so
    // per-row "ids" filter evaluation later is a Vec lookup not an RPC.
    let Some(conn) = discover::open_ro_for(sid) else {
        return Ok((vec![], vec![]));
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
            rows.push(view_change_row(
                &parts[..=d].join("/"), &parts[d], "dir", 0, d, true, None, None,
            )?);
            ids.push(vec![]);
        }
        rows.push(view_change_row(
            &name, &parts[leaf_depth], kind, sz, leaf_depth, false, None, None,
        )?);
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
                rows.push(view_change_row(
                    &format!("{name}#xattr={key}"), key, "xattr", *vlen,
                    leaf_depth + 1, false, Some(&name), Some(key),
                )?);
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
            rows.push(view_change_row(
                &parts[..=d].join("/"), &parts[d], "dir", 0, d, true, None, None,
            )?);
            ids.push(vec![]);
        }
        // Synthetic parent file row so the xattrs aren't hanging in
        // the air: kind="xattr-only" carries a hint glyph distinct
        // from a real "changed" file.
        rows.push(view_change_row(
            &name, &parts[leaf_depth], "xattr-only", 0, leaf_depth, false, None, None,
        )?);
        ids.push(vec![]);
        for (key, vlen) in xs {
            rows.push(view_change_row(
                &format!("{name}#xattr={key}"), &key, "xattr", vlen,
                leaf_depth + 1, false, Some(&name), Some(&key),
            )?);
            ids.push(vec![]);
        }
        prev = parts;
    }
    Ok((rows, ids))
}

fn source_outputs(
    sid: i64,
) -> Result<(Vec<crate::generated_wire::ViewOutputRow>, Vec<Subject>), String> {
    let rows = discover::outputs_typed(sid)?;
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
    for output in rows {
        let pid = output.process.and_then(|value| i64::try_from(value).ok()).unwrap_or(-1);
        let (exe, tgid) = pmap.get(&pid).cloned().unwrap_or_default();
        let argv: Vec<String> = vec![]; // outputs filter doesn't use arg
        subjects.push(Subject { box_name: String::new(),
                                exe: exe.clone(), cwd: String::new(), argv });
        rows_out.push(crate::generated_wire::ViewOutputRow {
            output,
            executable: wire_path(&exe)?,
            tgid: u32::try_from(tgid).ok().filter(|value| *value != 0),
        });
    }
    Ok((rows_out, subjects))
}

fn source_procs(
    sid: i64,
    running_only: bool,
) -> Result<(Vec<crate::generated_wire::ViewProcessRow>, Vec<Subject>), String> {
    // The flat process list and its DFS-flattened tree with depth/connector,
    // built once here on the engine side. Mirrors the old client-side
    // build_proc_tree but the ancestor `lookup` is a plain function call
    // instead of an RPC round-trip per process.
    let mut procs = discover::processes_typed(sid)?;
    // running_only (live box, default): keep only rows whose process (tgid, col 1)
    // is still alive — a pidfd probe (control::pid_alive), no stored liveness. The
    // surviving tgids ARE the filter; build_proc_tree still pulls their ancestors
    // as connectors. The caller only sets this for a live box.
    if running_only {
        use std::collections::HashMap;
        let mut alive: HashMap<u32, bool> = HashMap::new();
        procs.retain(|process| {
            let Some(tgid) = process.tgid.filter(|value| *value != 0) else { return false };
            *alive.entry(tgid).or_insert_with(|| {
                i32::try_from(tgid).is_ok_and(crate::control::pid_alive)
            })
        });
    }
    let roots = discover::proc_roots_typed(sid)?.into_iter().collect();
    let rows = build_proc_tree(&procs, &roots, sid)?;
    // Subject per row for "exe"/"cwd"/"arg" filter kinds. `cwd` lives in the
    // `process` table but ProcessRow doesn't include it; pull all
    // (rid → cwd) pairs in ONE sqlite scan so a million-row procs view isn't
    // a million per-row queries (which made view.open take ~30 s at scale).
    let cwd_by_rid: HashMap<u64, String> = discover::open_ro_for(sid)
        .and_then(|c| c.prepare("SELECT id, cwd FROM process").ok().map(|mut st| {
            st.query_map([], |r| Ok((r.get::<_, i64>(0)?,
                                     r.get::<_, Option<String>>(1)?.unwrap_or_default())))
              .ok().map(|it| it.flatten().filter_map(|(id, cwd)|
                  u64::try_from(id).ok().map(|id| (id, cwd))).collect::<HashMap<_, _>>())
              .unwrap_or_default()
        }))
        .unwrap_or_default();
    let mut subjects = Vec::with_capacity(rows.len());
    for row in &rows {
        let exe = String::from_utf8_lossy(row.executable.as_slice()).into_owned();
        let argv = row.argv.as_slice().iter()
            .map(|word| String::from_utf8_lossy(word.as_slice()).into_owned()).collect();
        let cwd = cwd_by_rid.get(&row.id).cloned().unwrap_or_default();
        subjects.push(Subject { box_name: String::new(), exe, cwd, argv });
    }
    Ok((rows, subjects))
}

// ── proc tree (lifted from ui.rs, with in-process ancestor lookup) ───────────

const PROC_TREE_DEPTH: usize = 64;

#[derive(Clone)]
struct NodeInfo {
    tgid: Option<u32>,
    ppid: Option<u32>,
    parent: Option<u64>,
    executable: crate::generated_wire::Path,
    argv: crate::wire::BoundedVec<
        crate::generated_wire::OsString,
        0,
        { crate::generated_wire::LIMIT_COMMAND_ITEMS },
    >,
}

fn process_node(process: &crate::generated_wire::ProcessRow) -> NodeInfo {
    NodeInfo {
        tgid: process.tgid,
        ppid: process.ppid,
        parent: process.parent,
        executable: process.executable.clone(),
        argv: process.argv.clone(),
    }
}

fn process_info_node(process: crate::generated_wire::ProcessInfo) -> NodeInfo {
    NodeInfo {
        tgid: process.tgid,
        ppid: process.ppid,
        parent: process.parent,
        executable: process.executable,
        argv: process.argv,
    }
}

fn build_proc_tree(
    procs: &[crate::generated_wire::ProcessRow],
    roots: &std::collections::HashSet<u64>,
    sid: i64,
) -> Result<Vec<crate::generated_wire::ViewProcessRow>, String> {
    use std::collections::HashMap;
    use std::collections::HashSet;

    let mut members: HashMap<u64, NodeInfo> = HashMap::new();
    for process in procs {
        members.insert(process.id, process_node(process));
    }
    let mut nodes: HashMap<u64, NodeInfo> = members.clone();

    // root→self row-id path per member, unioning structural ancestors into
    // `nodes` so their connector rows can carry tgid/ppid/exe in the output.
    let mut member_paths: HashMap<u64, Vec<u64>> = HashMap::new();
    let member_ids: Vec<u64> = members.keys().copied().collect();
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
                    let Some(process) = discover::proc_info_typed(sid, cur)? else { break };
                    process_info_node(process)
                }
            };
            let Some(parent_id) = got.parent else { break };
            if parent_id == 0 { break; }
            if !nodes.contains_key(&parent_id) {
                if let Some(process) = discover::proc_info_typed(sid, parent_id)? {
                    nodes.insert(parent_id, process_info_node(process));
                } else {
                    break;
                }
            }
            cur = parent_id;
        }
        path.reverse();
        member_paths.insert(start, path);
    }

    let mut paths: Vec<Vec<u64>> = member_paths.values().cloned().collect();
    paths.sort();

    let mut emitted: HashSet<u64> = HashSet::new();
    let mut out = vec![];
    for path in &paths {
        for (depth, &rid) in path.iter().enumerate() {
            if !emitted.insert(rid) { continue; }
            let connector = !members.contains_key(&rid);
            let Some(node) = nodes.get(&rid) else { continue };
            out.push(crate::generated_wire::ViewProcessRow {
                id: rid,
                tgid: node.tgid,
                ppid: node.ppid,
                executable: node.executable.clone(),
                argv: node.argv.clone(),
                depth: u32::try_from(depth).map_err(|_| "process tree depth exceeds u32")?,
                connector,
            });
        }
    }
    Ok(out)
}

fn source_pipelines(sid: i64) -> Result<Vec<crate::generated_wire::PipelineRow>, String> {
    discover::brushprov_typed(sid)
}

fn source_build_edges(sid: i64) -> Result<Vec<crate::generated_wire::BuildEdgeRow>, String> {
    discover::build_edges_typed(sid)
}

fn wire_path(value: &str) -> Result<crate::generated_wire::Path, String> {
    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
        .map_err(|error| format!("path exceeds relation bound: {error:?}"))
}

fn wire_os_string(value: &str) -> Result<crate::generated_wire::OsString, String> {
    crate::wire::BoundedBytes::new(value.as_bytes().to_vec())
        .map_err(|error| format!("OS string exceeds relation bound: {error:?}"))
}

fn view_change_row(
    path: &str,
    name: &str,
    kind: &str,
    size: i64,
    depth: usize,
    connector: bool,
    xattr_for: Option<&str>,
    xattr_key: Option<&str>,
) -> Result<crate::generated_wire::ViewChangeRow, String> {
    use crate::generated_wire::{ChangeKind, ViewChangeRow};
    let kind = match kind {
        "changed" => ChangeKind::Changed,
        "deleted" => ChangeKind::Deleted,
        "symlink" => ChangeKind::Symlink,
        "created" => ChangeKind::Created,
        "modified" => ChangeKind::Modified,
        "xattr" => ChangeKind::Xattr,
        "dir" => ChangeKind::Directory,
        "xattr-only" => ChangeKind::XattrOnly,
        kind => return Err(format!("unknown change view kind {kind:?}")),
    };
    Ok(ViewChangeRow {
        path: wire_path(path)?,
        name: wire_os_string(name)?,
        kind,
        size: u64::try_from(size).map_err(|_| "change size is negative")?,
        depth: u32::try_from(depth).map_err(|_| "change view depth exceeds u32")?,
        connector,
        xattr_for: xattr_for.map(wire_path).transpose()?,
        xattr_key: xattr_key.map(wire_os_string).transpose()?,
    })
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
            let (rows, ids) = source_changes(sid)?;
            (ViewRows::Changes(rows), ViewAux::Changes(ids))
        }
        Kind::Procs => {
            let (rows, subjects) = source_procs(sid, procs_running_only)?;
            (ViewRows::Processes(rows), ViewAux::Procs(subjects))
        }
        Kind::Outputs => {
            let (rows, subjects) = source_outputs(sid)?;
            (ViewRows::Outputs(rows), ViewAux::Outputs(subjects))
        }
        Kind::Pipelines => {
            (ViewRows::Pipelines(source_pipelines(sid)?), ViewAux::Pipelines)
        }
        Kind::BuildEdges => {
            (ViewRows::BuildEdges(source_build_edges(sid)?), ViewAux::BuildEdges)
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
        let change = vec![view_change_row(
            "src/main.rs", "main.rs", "changed", 12, 1, false, None, None,
        ).unwrap()];
        assert_eq!(change[0].kind, ChangeKind::Changed);

        let process = vec![crate::generated_wire::ViewProcessRow {
            id: 7,
            tgid: Some(41),
            ppid: Some(1),
            executable: wire_path("/bin/sh").unwrap(),
            argv: crate::wire::BoundedVec::new(vec![
                wire_os_string("sh").unwrap(),
                wire_os_string("-c").unwrap(),
                wire_os_string("true").unwrap(),
            ]).unwrap(),
            depth: 0,
            connector: false,
        }];
        assert_eq!(process[0].tgid, Some(41));

        let output = vec![crate::generated_wire::ViewOutputRow {
            output: crate::generated_wire::OutputRow {
                id: 8,
                time: 1.25,
                process: Some(7),
                stream: EchoStream::Stderr,
                length: 4,
            },
            executable: wire_path("/bin/sh").unwrap(),
            tgid: Some(41),
        }];
        assert_eq!(output[0].output.stream, EchoStream::Stderr);

        let pipeline = vec![crate::generated_wire::PipelineRow {
            id: 9,
            time: 1.0,
            command: crate::wire::BoundedText::new("echo hi > out".into()).unwrap(),
            record: Some(crate::generated_wire::PipelineProvenance {
                command: crate::wire::BoundedText::new("echo hi > out".into()).unwrap(),
                negated: false,
                stages: crate::wire::BoundedVec::new(vec![PipelineStage::Simple {
                    words: crate::wire::BoundedVec::new(vec![
                        wire_os_string("echo").unwrap(), wire_os_string("hi").unwrap(),
                    ]).unwrap(),
                    redirects: 1,
                }]).unwrap(),
                output_targets: crate::wire::BoundedVec::new(vec![wire_path("out").unwrap()])
                    .unwrap(),
                uid: 12,
                parent_uid: 4,
                sequence: 3,
                spawned_at: 1.1,
                nested: true,
                edge_output: Some(wire_path("out").unwrap()),
            }),
            pipeline: Some(3),
            spawned_at: Some(1.1),
            done_at: Some(1.2),
            nested: true,
            uid: Some(12),
            parent_uid: Some(4),
            exit_code: Some(0),
            processes: crate::wire::BoundedVec::new(vec![7]).unwrap(),
        }];
        let record = pipeline[0].record.as_ref().unwrap();
        assert!(matches!(record.stages.as_slice()[0], PipelineStage::Simple { .. }));
        assert_eq!(record.edge_output.as_ref().unwrap().as_slice(), b"out");

        let edge = vec![crate::generated_wire::BuildEdgeRow {
            id: 10,
            time: 2.0,
            outputs: crate::wire::BoundedVec::new(vec![wire_path("out").unwrap()]).unwrap(),
            inputs: crate::wire::BoundedVec::new(vec![wire_path("in").unwrap()]).unwrap(),
            command: Some(crate::wire::BoundedText::new("cc in -o out".into()).unwrap()),
            started_at: Some(2.1),
            ended_at: Some(2.2),
            exit_code: Some(0),
            output_excerpt: Some(crate::wire::BoundedText::new("ok".into()).unwrap()),
        }];
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
