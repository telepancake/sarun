use base64::Engine as _;
// Control socket — newline-JSON request/reply on a unix socket, speaking the
// SAME protocol as the Python ChannelServer: {"type":"ui","verb":...} verb
// calls, {"type":"subscribe"} converting the connection into a one-way event
// feed, explicit errors for unknown types/verbs. The first datagram on a
// connection may carry an SCM_RIGHTS pidfd (the register handshake); m2 does
// not run boxes yet, so register is refused politely, but any received fds
// are drained and closed so the protocol shape is honored.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;

use serde_json::Value;
use serde_json::json;

use crate::discover;

#[derive(Default)]
pub struct Shared {
    pub selected: Option<String>,
    pub subscribers: Vec<UnixStream>,
    pub overlay: Option<crate::overlay::Overlay>,
    pub box_pids: std::collections::HashMap<i64, i32>, // box_id -> runner pidfd
    pub box_runpids: std::collections::HashMap<i64, i32>, // box_id -> runner HOST pid
    /// Server-side materialized views (changes / procs / outputs) keyed by an
    /// opaque u64 the client got back from view.open. The values hold the
    /// per-box source rows + a Vec<usize> idx of survivors after the current
    /// filter, so view.window is a cheap slice.
    pub views: crate::views::Registry,
    pub next_view_id: u64,
    /// Per-box networking handles (`-n` mode only). Engine-owned: the runner
    /// asks for one in the register handshake and we hand back the netns path
    /// + gateway/box IPs; the handle stays alive (poll thread running, TAP
    /// fd open, netns anchor child SIGTERM-ed on Drop) until the box reaps.
    pub net: Option<std::sync::Arc<crate::net::Net>>,
    pub net_handles: std::collections::HashMap<i64, std::sync::Arc<crate::net::NetHandle>>,
    /// Long-lived tokio runtime handle used by the dispatcher tasks (one
    /// per-conn task per box). One runtime is plenty: the network is rarely
    /// the bottleneck, and a single runtime keeps reasoning about lifetimes
    /// simple.
    pub net_rt: Option<tokio::runtime::Handle>,
    /// oaita API proxy. Owns the upstream config + the set of `--api`-enabled
    /// boxes. Held here so per-box-channel ApiMux instances (created lazily
    /// in the box-channel frame loop) can fetch the same registry; the
    /// proxy itself has no listener — see oaita::proxy_mux.
    pub api_proxy: Option<std::sync::Arc<crate::oaita::proxy::Proxy>>,
}

fn pidfd_open(pid: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }
}
fn pidfd_signal(pidfd: i32, sig: i32) {
    unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd, sig,
                           std::ptr::null::<libc::c_void>(), 0); }
}

/// The HOST-namespace pid named by `pidfd`, read from /proc/self/fdinfo/<fd>
/// ("Pid:" line; its FIRST field is the pid in our (init) namespace). 0 on any
/// failure. This is the wrap-immune identity path — the pidfd names one exact
/// process incarnation, so a reused pid can never alias a finished runner.
fn host_pid_from_pidfd(pidfd: i32) -> i32 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/self/fdinfo/{pidfd}")) else {
        return 0;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Pid:") {
            return rest.split_whitespace().next()
                .and_then(|t| t.parse().ok()).unwrap_or(0);
        }
    }
    0
}

/// PPid of `pid` from /proc/<pid>/status (host namespace); 0 if unreadable.
fn ppid_of(pid: i32) -> i32 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Given the connecting runner's HOST pid, walk the /proc PPid chain upward and
/// return the box_id of the first LIVE box whose runner host pid is an ancestor
/// — the enclosing box of a nested launch. Kernel-derived and pid-trusted: the
/// box never supplies its own parent. None if no enclosing box is found.
fn derive_parent_box(state: &State, host_pid: i32) -> Option<i64> {
    let map: std::collections::HashMap<i32, i64> = {
        let s = state.lock().unwrap();
        s.box_runpids.iter().map(|(b, p)| (*p, *b)).collect()
    };
    if map.is_empty() || host_pid <= 1 {
        return None;
    }
    let mut pid = host_pid;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        if pid <= 1 || !seen.insert(pid) {
            break;
        }
        if let Some(&b) = map.get(&pid) {
            return Some(b);
        }
        let pp = ppid_of(pid);
        if pp <= 1 {
            break;
        }
        pid = pp;
    }
    None
}

pub type State = Arc<Mutex<Shared>>;

// The old api-proxy attribution shim (peer-pid → box-id lookup) was removed
// when the proxy moved onto the box-channel — attribution is now intrinsic
// to the channel the FRAME_API_* stream rides on.

/// Broadcast that box `box_id` has new api_log rows so the UI's API Logs pane
/// refreshes. Best-effort; the broadcaster swallows send errors.
pub fn broadcast_api_log(box_id: i64) {
    // We need the State to actually broadcast — go through a global handle
    // set up by serve().
    if let Some(state) = STATE_HANDLE.read().clone() {
        broadcast(&state, &json!({
            "type": "api_log_added",
            "sid": box_id.to_string(),
        }));
    }
}

static STATE_HANDLE: parking_lot::RwLock<Option<State>> = parking_lot::RwLock::new(None);
pub fn install_state_handle(s: State) {
    *STATE_HANDLE.write() = Some(s.clone());
}

/// Record one D9 brush-shell provenance frame for box `id`: parse the JSON
/// payload, write it into the live box's sqlar `brushprov` table, and broadcast
/// a `brush_prov` event. Best-effort — a malformed payload is dropped quietly.
fn record_brush_prov(state: &State, ov: &Option<crate::overlay::Overlay>,
                     id: i64, payload: &[u8]) {
    let Ok(rec) = serde_json::from_slice::<Value>(payload) else { return; };
    let cmd = rec.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
    // The 0-based pipeline ordinal + the wall-clock spawn instant brush captured
    // right before running this pipeline's complete-command. The spawn_ts defines
    // the attribution window; the actual process↔pipeline stamping is done in one
    // race-free pass at box teardown (finalize_brush_links), since a process row
    // (e.g. a redirect target's writer) can be materialized long after its pipeline.
    let seq = rec.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let spawn_ts = rec.get("spawn_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let record_json = rec.to_string();
    let mut prov_id = 0i64;
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            prov_id = b.add_brushprov(&cmd, &record_json, seq, spawn_ts);
            // Remember this pipeline's output-redirect targets for the exact
            // file→process linkage made at teardown.
            let targets: Vec<String> = rec.get("out_targets")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from))
                          .collect())
                .unwrap_or_default();
            b.on_brush_prov(prov_id, targets);
        }
    }
    broadcast(state, &json!({"type": "brush_prov",
                            "session_id": id.to_string(),
                            "brushprov_id": prov_id, "seq": seq,
                            "cmd": cmd, "record": rec}));
}

/// D9 nested-shell provenance verb. The brush-sh shim (a `sh -c RECIPE` the box
/// spawned, exec'd as the engine binary) sends one `brush_prov_nested` message
/// carrying ITS OWN pidfd as SCM_RIGHTS. We resolve the enclosing box from the
/// shim's /proc ancestry — the EXACT path `register` uses for a nested box — and
/// record each record as a NESTED brushprov row, broadcasting a `brush_prov`
/// event per row. Best-effort: an unresolvable box or malformed message is
/// dropped quietly (the recipe runs regardless; provenance is optional). This is
/// a one-shot control reply — it does NOT create a box channel.
fn brush_prov_nested(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    // Resolve the shim's HOST pid from its pidfd (the wrap-immune identity path),
    // then derive the enclosing box from its /proc ancestry.
    let host_pid = peer_pidfd.map(host_pid_from_pidfd).filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let ov = state.lock().unwrap().overlay.clone();
    let records = msg.get("records").and_then(Value::as_array);
    let Some(records) = records else {
        return json!({"ok": false, "error": "no records"});
    };
    let mut n = 0i64;
    for rec in records {
        let cmd = rec.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
        let seq = rec.get("seq").and_then(Value::as_i64).unwrap_or(0);
        let spawn_ts = rec.get("spawn_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let record_json = rec.to_string();
        let mut prov_id = 0i64;
        if let Some(ov) = ov.as_ref() {
            if let Some(b) = ov.live_box(id) {
                prov_id = b.add_brushprov_nested(&cmd, &record_json, seq, spawn_ts);
                // D9 brush-IS-the-shell: a nested pipeline's literal output
                // targets are written by descendants of the top-level brush
                // --inner (via the brush-sh shim → caller → recipe-process
                // chain), so the same forest-ancestry guard in
                // finalize_brush_links accepts them. Feed them into the same
                // brush_links bucket so a nested `> file` stamps its writer
                // with the NESTED brushprov row's id. Two pipelines never
                // compete for the same literal target (each file is written
                // by exactly one pipeline).
                let targets: Vec<String> = rec.get("out_targets")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from))
                              .collect())
                    .unwrap_or_default();
                b.on_brush_prov(prov_id, targets);
            }
        }
        broadcast(state, &json!({"type": "brush_prov",
                                "session_id": id.to_string(),
                                "brushprov_id": prov_id, "seq": seq,
                                "nested": true, "cmd": cmd, "record": rec}));
        n += 1;
    }
    json!({"ok": true, "recorded": n})
}

/// Phase 1 embedded-ninja `build_edges` verb. The shadowed `ninja` (vendored n2,
/// in-process) sends ONE message carrying the FULL parsed build graph — every
/// edge {outs, ins, cmd}, INCLUDING up-to-date targets that never execute — plus
/// ITS OWN pidfd as SCM_RIGHTS. We resolve the enclosing box from /proc ancestry
/// (the same path register/brush_prov_nested use) and store each edge in the
/// box's `build_edges` table. One-shot control reply; not a box channel.
fn build_edges(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let host_pid = peer_pidfd.map(host_pid_from_pidfd).filter(|p| *p > 0)
        .unwrap_or(0);
    if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
    let Some(id) = derive_parent_box(state, host_pid) else {
        return json!({"ok": false, "error": "no enclosing box"});
    };
    let ov = state.lock().unwrap().overlay.clone();
    let Some(edges) = msg.get("edges").and_then(Value::as_array) else {
        return json!({"ok": false, "error": "no edges"});
    };
    let mut n = 0i64;
    if let Some(ov) = ov.as_ref() {
        if let Some(b) = ov.live_box(id) {
            for e in edges {
                let outs = e.get("outs").cloned().unwrap_or_else(|| json!([]));
                let ins = e.get("ins").cloned().unwrap_or_else(|| json!([]));
                let cmd = e.get("cmd").and_then(Value::as_str);
                b.add_build_edge(&outs.to_string(), &ins.to_string(), cmd);
                n += 1;
            }
        }
    }
    broadcast(state, &json!({"type": "build_edges",
                            "session_id": id.to_string(), "edges": n}));
    json!({"ok": true, "recorded": n})
}

pub fn broadcast(state: &State, ev: &Value) {
    let data = format!("{ev}\n");
    let mut s = state.lock().unwrap();
    s.subscribers.retain(|conn| {
        let mut c = conn;
        c.write_all(data.as_bytes()).is_ok()
    });
}

/// Peek any SCM_RIGHTS fds sent with the connection's first bytes (the register
/// handshake carries the runner's pidfd). KEEP the first fd open and return it
/// (the caller derives the runner's host pid from it and holds it for `kill`);
/// close any extras. MSG_PEEK leaves the data bytes queued for the BufReader,
/// and the real (no-ancillary) read later discards the duplicate fd delivery.
fn recv_first_fd(conn: &UnixStream) -> Option<i32> {
    // Wait (bounded) for the first bytes to arrive before peeking: the runner's
    // sendmsg may still be in flight when we accept, and a non-blocking peek
    // that races ahead of it would miss the pidfd — dropping a nested box's
    // only correct host-pid source. poll for readability, then a blocking peek.
    let fd = conn.as_raw_fd();
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let pr = unsafe { libc::poll(&mut pfd, 1, 30_000) };
    if pr <= 0 {
        return None;
    }
    let mut fdbuf = [0i32; 8];
    let mut io = [0u8; 1];
    let mut iov = libc::iovec { iov_base: io.as_mut_ptr().cast(), iov_len: 1 };
    let mut cmsg = [0u8; 128];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    // msg_controllen is socklen_t (u32) on glibc but size_t (usize) on musl;
    // `as _` picks the field's type on each target.
    msg.msg_controllen = cmsg.len() as _;
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_PEEK) };
    if n < 0 {
        return None;
    }
    let mut first: Option<i32> = None;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let len = (*c).cmsg_len as usize
                    - (libc::CMSG_DATA(c) as usize - c as usize);
                let count = len / std::mem::size_of::<i32>();
                for i in 0..count.min(fdbuf.len()) {
                    std::ptr::copy_nonoverlapping(
                        data.add(i * 4), (&mut fdbuf[i] as *mut i32).cast(), 4);
                    if first.is_none() {
                        first = Some(fdbuf[i]);
                    } else {
                        libc::close(fdbuf[i]);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    first
}

fn dispatch(state: &State, msg: &Value) -> Value {
    let t = msg.get("type").and_then(Value::as_str).unwrap_or("");
    match t {
        "subscribe" => json!({"ok": true, "_subscribe": true}),
        "register" => register(state, msg, None),
        "select" => {
            let sid = msg.get("sid").and_then(Value::as_str).map(String::from);
            let boxes = discover::discover();
            match sid.as_deref().and_then(|s| resolve(&boxes, s)) {
                Some(id) => {
                    state.lock().unwrap().selected = Some(id.to_string());
                    json!({"ok": true, "sid": id.to_string()})
                }
                None => json!({"ok": false,
                               "error": format!("no slopbox '{}'",
                                                sid.unwrap_or_default())}),
            }
        }
        "ui" => dispatch_ui(state, msg),
        "patch" => {
            let boxes = discover::discover();
            match msg.get("sid").and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s)) {
                Some(id) => {
                    let data = crate::review::patch_text(id);
                    json!({"ok": true, "patch":
                        base64::engine::general_purpose::STANDARD.encode(&data)})
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "apply" | "discard" => {
            let boxes = discover::discover();
            match msg.get("sid").and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s)) {
                Some(id) => {
                    let all = Value::Null; // CLI applies/discards the whole box
                    let ctx = crate::review::NestCtx::new(
                        state.lock().unwrap().overlay.clone());
                    let (r, n) = if t == "apply" {
                        let r = crate::review::apply(id, &all, &ctx);
                        let n = r.get("applied").and_then(Value::as_array)
                            .map(|a| a.len()).unwrap_or(0);
                        (r, n)
                    } else {
                        let r = crate::review::discard(id, &all, &ctx);
                        let n = r.get("discarded").and_then(Value::as_array)
                            .map(|a| a.len()).unwrap_or(0);
                        (r, n)
                    };
                    drop_if_empty(state, id);
                    json!({"ok": true, "count": n, "sid": id.to_string(),
                           "errors": r.get("errors").cloned()
                               .unwrap_or(json!([]))})
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "rename" => {
            let boxes = discover::discover();
            let newname = msg.get("name").and_then(Value::as_str).unwrap_or("");
            match msg.get("sid").and_then(Value::as_str)
                .and_then(|s| resolve(&boxes, s)) {
                Some(id) => {
                    let old = discover::display_path(&boxes, id);
                    // Route the meta write through the LIVE BoxState when the box
                    // is running (one connection — never a rival on-disk handle
                    // racing the serve thread); otherwise write the at-rest sqlar.
                    let live = state.lock().unwrap().overlay.clone()
                        .and_then(|o| o.live_box(id));
                    match live {
                        Some(cb) => cb.set_meta("name", newname),
                        None => {
                            if let Ok(c) = rusqlite::Connection::open(
                                crate::paths::state_home().join(format!("{id}.sqlar"))) {
                                let _ = c.execute(
                                    "INSERT INTO meta(key,value) VALUES('name',?1)
                                     ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                                    [newname]);
                            }
                        }
                    }
                    broadcast(state, &json!({"type": "session_renamed",
                        "session_id": id.to_string(), "name": newname}));
                    json!({"ok": true, "old": old, "name": newname})
                }
                None => json!({"ok": false, "error": "no slopbox"}),
            }
        }
        "shutdown" => {
            // Stop the engine. SIGTERM self → the existing signal handler
            // tears down the overlay + control socket; everything that
            // follows in this dispatch is racing the exit, so reply ok now
            // and let the kernel deliver the signal a few syscalls later.
            unsafe { libc::kill(libc::getpid(), libc::SIGTERM); }
            json!({"ok": true})
        }
        other => json!({"ok": false,
                        "error": format!("unknown control type '{other}'")}),
    }
}

fn resolve(boxes: &std::collections::BTreeMap<i64, discover::Box_>,
           ident: &str) -> Option<i64> {    if let Ok(id) = ident.parse::<i64>() {
        if boxes.contains_key(&id) {
            return Some(id);
        }
    }
    boxes.values()
        .find(|b| b.name == ident || discover::display_path(boxes, b.box_id) == ident)
        .map(|b| b.box_id)
}

/// The box_id of the box NAMED `name` whose parent is `parent` (None=top-level),
/// else None — the rerun/uniqueness lookup (siblings have unique NAMEs). Mirrors
/// the Python Supervisor._find_named_child (scans discovered on-disk boxes).
fn find_named_child(boxes: &std::collections::BTreeMap<i64, discover::Box_>,
                    name: &str, parent: Option<i64>) -> Option<i64> {
    boxes.values()
        .find(|b| b.name == name && b.parent == parent)
        .map(|b| b.box_id)
}

fn selected_sid(state: &State) -> Option<i64> {
    state.lock().unwrap().selected.as_ref()
        .and_then(|s| s.parse::<i64>().ok())
}

fn flows_dir_for(box_id: i64) -> Option<std::path::PathBuf> {
    let d = crate::paths::state_home().join(format!("flows/box{box_id}"));
    if d.is_dir() { Some(d) } else { None }
}

// Accept the sid as either a JSON number OR a string-of-int. Most UI verbs
// send a string (cur_sid is a String) but a few — load_pipelines /
// load_build_edges and assorted tests — send the i64 straight from
// cur_sid_i64; without the dual parse those silently got None and the verb
// returned an empty default. view.open already had this same dual parse.
fn arg_sid(args: &[Value]) -> Option<i64> {
    let v = args.first()?;
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Unconditionally remove a box: drop it from the overlay, delete its sqlar +
/// backing + pool blobs, broadcast session_removed. The `delete` verb's body.
fn reap(state: &State, id: i64) {
    // Drop the NetHandle (Tap mode only) — the Drop impl SIGTERM's the
    // netns anchor, which releases the last reference to /proc/<a>/ns/net
    // and tears down the netns + TAP. Idempotent: no-op for Off / Host.
    let _ = state.lock().unwrap().net_handles.remove(&id);
    if let Some(ov) = state.lock().unwrap().overlay.clone() {
        ov.remove_box(id);
    }
    let _ = std::fs::remove_file(crate::paths::state_home()
        .join(format!("{id}.sqlar")));
    let _ = std::fs::remove_dir_all(crate::paths::live_home()
        .join(id.to_string()));
    let _ = std::fs::remove_dir_all(crate::paths::live_home()
        .join("blob").join(id.to_string()));
    broadcast(state, &json!({"type": "session_removed",
                             "session_id": id.to_string()}));
}

/// dissolve: remove a box, finalizing its own changes by the file rules
/// (apply-matched paths materialized to the host, the rest discarded), then
/// freeing it. Refuses a running box.
///
/// A box WITH children preserves each child's merged view first: every path the
/// parent captured (apply- and discard-bound alike) is copied DOWN into each
/// child that has no entry of its own for it (copy_down_entry), so the child
/// keeps reading exactly what it saw through the parent once the parent is gone.
/// Then the children are re-parented to the dissolving box's own parent and the
/// parent is freed. Children may be LIVE: copy-down and re-parent both route
/// through the live BoxState (connection + RAM mirror) when the child is
/// running, so a mounted FUSE view keeps serving the right bytes — no rival
/// on-disk handle racing the serve thread. Only the dissolving box itself must
/// be stopped (its archive is rewritten by finalize).
fn dissolve(state: &State, id: i64) -> Value {
    if state.lock().unwrap().box_pids.contains_key(&id) {
        return json!({"ok": false, "error": "box is running; stop it first"});
    }
    let boxes = discover::discover();
    let Some(me) = boxes.get(&id) else {
        return json!({"ok": false, "error": "no slopbox"});
    };
    let grandparent = me.parent;
    let children: Vec<i64> = boxes.values()
        .filter(|b| b.parent == Some(id)).map(|b| b.box_id).collect();
    let ov = state.lock().unwrap().overlay.clone();
    // Copy-down: snapshot this box's contributed view into each child that has
    // no entry of its own, so dissolving the parent doesn't change what the
    // child sees. A live child's copy-down goes through its live BoxState.
    // Fail-closed: if any copy errors, free nothing.
    if !children.is_empty() {
        let paths = crate::review::changed_paths(id);
        for &child in &children {
            let live = ov.as_ref().and_then(|o| o.live_box(child));
            for rel in &paths {
                if let Err(e) = crate::review::copy_down_entry(
                    id, child, rel, live.as_deref()) {
                    return json!({"ok": false,
                        "error": format!("copy-down to box {child} failed: {e}"),
                        "path": rel});
                }
            }
        }
    }
    // finalize: apply rule-matched changes to the host, discard the rest
    // (fail-closed — if applying errored, don't free the box).
    let fin = crate::review::finalize_by_rules(
        id, &crate::review::NestCtx::new(ov.clone()));
    if fin.get("errors").and_then(Value::as_array).map(|a| !a.is_empty())
        .unwrap_or(false) {
        return json!({"ok": false, "error": "finalize had errors; nothing freed",
                      "finalize_errors": fin.get("errors").cloned()});
    }
    // Re-parent the children onto the dissolving box's own parent. For a LIVE
    // child write the meta through its BoxState (one connection); for one at
    // rest write the on-disk sqlar. Also update the overlay's in-RAM parent.
    for &child in &children {
        match ov.as_ref().and_then(|o| o.live_box(child)) {
            Some(cb) => cb.set_meta("parent_box_id",
                &grandparent.map(|p| p.to_string()).unwrap_or_default()),
            None => { let _ = crate::review::set_parent_meta(child, grandparent); }
        }
        if let Some(ov) = &ov {
            ov.set_box_parent(child, grandparent);
        }
    }
    reap(state, id);
    json!({"ok": true,
           "applied": fin.get("applied").cloned().unwrap_or(json!([])),
           "discarded": fin.get("discarded").cloned().unwrap_or(json!([])),
           "reparented": children})
}

/// After apply/discard, reap the box if it has no remaining changes.
fn drop_if_empty(state: &State, id: i64) {
    if crate::review::session_changes(id).as_array().map(|a| a.is_empty())
        .unwrap_or(false) {
        reap(state, id);
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
        && !s.ends_with('-')
}

/// The runner register handshake. Mints a box_id, creates the backing sentinel
/// (live/<id>/up) and the box's sqlar (root process row from the message's
/// prov), registers the box on the overlay, and acks with the <mnt>/<id> bind
/// target. The SAME connection becomes the box channel — its EOF (handled by
/// the caller via the _box_sid marker) is teardown.
///
/// `peer_pidfd` is the runner's own pidfd (SCM_RIGHTS first fd): we derive its
/// HOST pid for kill + parent derivation, and HOLD it for pid-reuse-safe kill.
/// NESTED LAUNCH: a `relname` field means the runner is inside a box; the
/// enclosing box is derived from the runner's /proc ancestry (never trusted
/// from the message), and this box is parented under it. A relname with no
/// derivable enclosing box is an error (the box's pidfd closes on the early
/// return when the caller's loop tears down). Capture mode stays downgraded in
/// the ack (no echo/sinks yet — runner behaves as -t passthrough).
fn register(state: &State, msg: &Value, peer_pidfd: Option<i32>) -> Value {
    let ov = state.lock().unwrap().overlay.clone();
    let Some(ov) = ov else {
        if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
        return json!({"ok": false, "error": "overlay mount is not available"});
    };
    // Runner host pid: from the pidfd if sent (correct for nested runners whose
    // own getpid() is a parent-namespace pid); else the claimed tgid (top-level).
    let host_pid = peer_pidfd.map(host_pid_from_pidfd).filter(|p| *p > 0)
        .or_else(|| msg.get("prov").and_then(|p| p.get("tgid"))
                 .and_then(Value::as_i64).map(|t| t as i32))
        .unwrap_or(0);
    let boxes = discover::discover();
    // ── PARENT + NAME RESOLUTION ───────────────────────────────────────────
    // IN-BOX (relname present): parent = kernel-derived enclosing box; the box
    //   supplies only a single-segment relative NAME (or "" → auto A<n>).
    // HOST (no relname): top-level by default; a supplied session_id may be a
    //   single NAME or a dotted display path (A.B) whose prefix names the parent.
    let relname = msg.get("relname").and_then(Value::as_str);
    let mut parent: Option<i64> = None;
    let mut name: Option<String> = None;
    if let Some(rel) = relname {
        if !rel.is_empty() && (!valid_name(rel) || rel.contains('.') || rel.contains('/')) {
            if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
            return json!({"ok": false,
                "error": "invalid relname: must be a single NAME segment"});
        }
        match derive_parent_box(state, host_pid) {
            Some(p) => parent = Some(p),
            None => {
                if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
                return json!({"ok": false,
                    "error": "relname supplied but no enclosing box found"});
            }
        }
        if !rel.is_empty() { name = Some(rel.to_string()); }
    } else if let Some(want) = msg.get("session_id").and_then(Value::as_str)
        .filter(|s| !s.is_empty()) {
        if let Some((prefix, last)) = want.rsplit_once('.') {
            // Dotted display path: parent = prefix box (must exist), NAME = last.
            match resolve(&boxes, prefix) {
                Some(p) => { parent = Some(p); name = Some(last.to_string()); }
                None => {
                    if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
                    return json!({"ok": false,
                        "error": format!("parent box '{prefix}' does not exist")});
                }
            }
        } else {
            name = Some(want.to_string());
        }
    }

    // ── CREATE-VS-RERUN ────────────────────────────────────────────────────
    // A named launch RERUNS the same box_id if a sibling with that NAME already
    // exists under the resolved parent (adds another root to its process forest).
    // An unnamed launch always CREATEs a fresh box_id. The runner's want_rerun is
    // advisory; the authoritative decision is the name lookup (mirrors Python).
    let mut rerun = false;
    let mut existing_id: Option<i64> = None;
    if let Some(ref nm) = name {
        if let Some(eid) = find_named_child(&boxes, nm, parent) {
            existing_id = Some(eid);
            rerun = true;
        }
    }
    if rerun && state.lock().unwrap().box_pids.contains_key(&existing_id.unwrap()) {
        if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
        return json!({"ok": false, "error": "slopbox is already running"});
    }
    let live_max = ov.box_ids().into_iter().max().unwrap_or(0);
    let id = existing_id.unwrap_or_else(||
        boxes.keys().max().copied().unwrap_or(0).max(live_max) + 1);
    let name = name.unwrap_or_else(|| format!("A{id}"));
    let env_capture = msg.get("want_env").and_then(Value::as_bool).unwrap_or(false);
    let direct = msg.get("want_direct").and_then(Value::as_bool).unwrap_or(false);
    // D-parent flags. `want_no_parent` is the runner's explicit "this box has
    // NO parent and the lower chain does NOT bottom at the host /": the box's
    // own contents are its entire filesystem (the bottom of an OCI image
    // stack). It overrides the kernel-derived parent walk, so even a runner
    // nested under another box can declare itself a rootfs.
    let want_no_parent = msg.get("want_no_parent")
        .and_then(Value::as_bool).unwrap_or(false);
    let want_readonly_parent = msg.get("want_readonly_parent")
        .and_then(Value::as_bool).unwrap_or(false);
    let want_capture = msg.get("want_capture").and_then(Value::as_bool)
        .unwrap_or(true) && !direct;
    let backing = crate::paths::live_home().join(id.to_string());
    if let Err(e) = std::fs::create_dir_all(backing.join("up")) {
        if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
        return json!({"ok": false, "error": format!("backing: {e}")});
    }
    let b = match crate::capture::BoxState::create(id) {
        Ok(b) => b,
        Err(e) => {
            if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
            return json!({"ok": false, "error": format!("sqlar: {e}")});
        }
    };
    // RERUN: reopen the existing box's recorded state so prior writes show
    // through and prior process rows keep their ids (the new root is additive).
    if rerun { b.load_mirror(); }
    b.set_env_capture(env_capture);
    b.set_direct(direct);
    b.set_is_brush(msg.get("want_brush").and_then(Value::as_bool).unwrap_or(false));
    b.set_is_api(msg.get("want_api").and_then(Value::as_bool).unwrap_or(false));
    b.set_meta("name", &name);
    // D-parent: `want_no_parent` strips any kernel-derived parent AND closes
    // the lower chain so reads never fall through to the real host. It's the
    // "OCI rootfs" / "Dockerfile FROM scratch" semantic. A child can
    // independently mark itself readonly-parent.
    let mut parent = parent;
    if want_no_parent {
        parent = None;
        b.set_no_host_fallback(true);
        b.set_meta("no_host_fallback", "1");
    }
    if want_readonly_parent {
        b.set_readonly_parent(true);
        b.set_meta("readonly_parent", "1");
    }
    if let Some(p) = parent {
        b.set_parent(Some(p));
        b.set_meta("parent_box_id", &p.to_string());
    }
    if let Some(prov) = msg.get("prov") {
        b.root_process(prov, host_pid as i64);
    }
    {
        let mut s = state.lock().unwrap();
        if host_pid > 0 { s.box_runpids.insert(id, host_pid); }
        // Hold a pidfd on the runner so `kill` can signal it pid-reuse-safely.
        // Prefer the runner's own pidfd (valid across pid namespaces); else open
        // one from the claimed tgid (top-level fallback).
        if let Some(fd) = peer_pidfd {
            s.box_pids.insert(id, fd);
        } else if host_pid > 0 {
            let fd = pidfd_open(host_pid);
            if fd >= 0 { s.box_pids.insert(id, fd); }
        }
    }
    // --api opt-in: register this box with the oaita proxy so connections
    // from inside it are accepted (and route to its api_log table). Refresh
    // the runner-pid map the proxy uses for peer attribution.
    let want_api = msg.get("want_api").and_then(Value::as_bool).unwrap_or(false);
    if want_api {
        if let Some(p) = state.lock().unwrap().api_proxy.clone() {
            p.enable_box(id);
        }
    }
    ov.add_box(std::sync::Arc::new(b));
    // Announce the new box on the subscribe stream so attached UIs
    // rebuild their session list WITHOUT a manual refresh. on_event
    // already handles session_added/removed/renamed identically — it
    // just never fired here because we forgot to broadcast it.
    // session_removed (in delete / kill paths) and session_renamed
    // (in rename) were getting sent; this is the missing third leg.
    broadcast(state, &json!({
        "type": "session_added",
        "sid": id.to_string(),
        "name": name,
        "parent": parent,
    }));
    let root = crate::paths::mnt_point().join(id.to_string());

    // ── Networking (-n boxes only) ────────────────────────────────────────
    // For Tap mode: fork the netns anchor, equip its netns with the TAP +
    // gateway IP, build a StackRuntime + flows log around the TAP fd, write
    // the augmented CA bundle to a per-box temp path, and return netns_path
    // + dns_ip + ca_pem_path so the runner can wire bwrap up.
    let (netns_path, dns_ip, ca_pem_path) =
        prepare_net(state, id, msg).unwrap_or_default();

    // D-oci: if any ancestor in the parent chain has an oci_config meta key
    // (stamped by `sarun oci load` on the top layer of an image), surface
    // env / cwd / cmd / entrypoint / user in the ack so the runner can
    // bwrap with the image's PATH set, in the image's WorkingDir, with the
    // image's User — without which `sarun img -- /bin/sh` would inherit
    // the HOST's PATH (pointing at host bins that don't exist in a closed
    // box) and the HOST's cwd (likely a path outside the image).
    let oci = oci_runtime_from_chain(parent);
    let mut reply = json!({
        "ok": true, "mount": root.to_string_lossy(),
        "shm_dir": backing.to_string_lossy(),
        "netns_path": netns_path,
        "dns_ip": dns_ip,
        "ca_pem_path": ca_pem_path,
        "owner_token": format!("{:032x}", std::process::id() as u128
                               ^ (id as u128) << 64
                               ^ std::time::SystemTime::now()
                                 .duration_since(std::time::UNIX_EPOCH)
                                 .map(|d| d.as_nanos()).unwrap_or(0)),
        "box_id": id, "session_id": id.to_string(), "name": name,
        "capture": want_capture,      // sinks + live echo mux active (off for -t/-d)
        "api": want_api,              // proxy admits this box; inner serves the in-box UDS
        "_box_sid": id,               // caller marker: this conn is now the box channel
    });
    if let Some(o) = oci {
        reply["oci"] = o;
    }
    reply
}

/// Walk the parent chain looking for an `oci_config` meta entry (stamped by
/// `sarun oci load` on the image's TOP layer). Returns the parsed runtime
/// view {env, cwd, cmd, entrypoint, user} the runner uses, or None when the
/// chain has no OCI ancestor (a non-OCI box). Reads each ancestor's sqlar
/// meta directly — at-rest boxes that aren't live yet are common here, since
/// `sarun img.SCRATCH` is the first thing a user does after `oci load`.
fn oci_runtime_from_chain(parent: Option<i64>) -> Option<Value> {
    let mut cur = parent;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let id = cur?;
        if !seen.insert(id) { return None; }
        let sqlar = crate::paths::state_home().join(format!("{id}.sqlar"));
        let conn = rusqlite::Connection::open_with_flags(
            &sqlar, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
        let cfg: Option<String> = conn.query_row(
            "SELECT value FROM meta WHERE key='oci_config'", [],
            |r| r.get(0)).ok();
        if let Some(cfg_json) = cfg {
            return parse_oci_runtime(&cfg_json);
        }
        let parent_str: Option<String> = conn.query_row(
            "SELECT value FROM meta WHERE key='parent_box_id'", [],
            |r| r.get(0)).ok();
        cur = parent_str.and_then(|s| s.parse::<i64>().ok());
    }
    None
}

/// Pull env / cwd / cmd / entrypoint / user out of the raw OCI image config
/// JSON. We don't link `oci_spec` here on purpose — those fields are stable
/// across the OCI spec versions and a hand-rolled extractor avoids dragging
/// the dep into control.rs just to read five fields.
fn parse_oci_runtime(cfg_json: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(cfg_json).ok()?;
    let inner = v.get("config")?;
    let mut out = serde_json::Map::new();
    if let Some(env) = inner.get("Env") { out.insert("env".into(), env.clone()); }
    if let Some(cwd) = inner.get("WorkingDir") {
        out.insert("cwd".into(), cwd.clone());
    }
    if let Some(cmd) = inner.get("Cmd") { out.insert("cmd".into(), cmd.clone()); }
    if let Some(ep) = inner.get("Entrypoint") {
        out.insert("entrypoint".into(), ep.clone());
    }
    if let Some(u) = inner.get("User") { out.insert("user".into(), u.clone()); }
    if out.is_empty() { None } else { Some(Value::Object(out)) }
}

/// Equip a `-n` box's netns and start its smoltcp stack.
/// Returns (netns_path, dns_ip, augmented_ca_bundle_path) — empty strings
/// when networking is off/host or anything fails (the caller's bwrap then
/// falls back to the default behavior the runner chose).
fn prepare_net(state: &State, id: i64, msg: &Value) -> Option<(String, String, String)> {
    let net_mode = msg.get("net_mode").and_then(Value::as_str).unwrap_or("off");
    if net_mode != "tap" { return Some((String::new(), String::new(), String::new())); }
    let net = state.lock().unwrap().net.clone()?;
    let box_id_u16 = net.alloc_box_id();
    let subnet = crate::net::subnet::BoxSubnet::new(box_id_u16);
    let rig = match crate::net::tap::spawn_anchor(subnet) {
        Ok(r) => r,
        Err(e) => { eprintln!("sarun-engine: tap anchor failed: {e}"); return None; }
    };
    let box_dir = crate::paths::state_home().join(format!("flows/box{id}"));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let flows = match crate::net::flows::FlowsLog::create(&box_dir, ts, box_id_u16) {
        Ok(f) => f, Err(e) => {
            eprintln!("sarun-engine: flows log: {e}"); return None;
        }
    };
    let gw_mac = derive_gw_mac(box_id_u16);
    let stack = crate::net::stack::StackRuntime::start(
        box_id_u16, subnet, gw_mac, rig.mac, rig.tap_fd, flows.clone());

    // Write the augmented CA bundle (host bundle + engine CA appended) once,
    // under the runner's runtime dir so bwrap can --ro-bind it later.
    let ca_pem_path = write_augmented_ca_bundle(&net.ca, id)
        .unwrap_or_default();

    let handle = std::sync::Arc::new(crate::net::make_handle(
        box_id_u16, subnet.gateway_ip(), subnet.box_ip(),
        rig.anchor_pid, rig.netns_path.clone(),
        stack.clone(), flows.path.clone(), flows.keylog_path.clone()));
    state.lock().unwrap().net_handles.insert(id, handle);

    // Start the per-box dispatcher: it pulls AcceptedConn off the stack's
    // accept channel and routes each new connection to the right handler
    // (HTTP MITM / HTTPS MITM / L4 forward). The keylog is per-box (file
    // sits next to the box's pcapng) so a tshark with `-o
    // tls.keylog_file:<flows>.keys` decrypts every TLS connection in the
    // pcapng. The upstream rustls config is shared (the real internet's
    // trust roots don't vary by box).
    let keylog = crate::net::mitm::KeyLogFile::new(&flows.keylog_path).ok();
    let upstream_tls = crate::net::mitm::build_upstream_client_config();
    if let (Some(rt), Some(keylog)) = (state.lock().unwrap().net_rt.clone(), keylog) {
        crate::net::dispatch::Dispatcher::start(
            stack.clone(), stack.dns.clone(),
            format!("box{id}"),
            net.ca.clone(), keylog, upstream_tls,
            net.prompts.clone(),
            rt);
    }

    Some((rig.netns_path.to_string_lossy().into_owned(),
          ipv4_str(subnet.gateway_ip()),
          ca_pem_path))
}

fn ipv4_str(o: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3])
}

fn derive_gw_mac(box_id: u16) -> [u8; 6] {
    // Locally-administered unicast; embed the box id so anchor + gateway are
    // distinguishable on a packet capture.
    [0x02, 0x73, 0x72, 0x6e, (box_id >> 8) as u8, (box_id & 0xff) as u8]
}

fn write_augmented_ca_bundle(ca: &crate::net::ca::Ca, box_id: i64) -> Option<String> {
    // Append our root to whichever system bundle exists; if none does, fall
    // back to "just our root" (a self-contained mini-bundle is still trusted).
    let mut bundle = String::new();
    for p in &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
    ] {
        if let Ok(s) = std::fs::read_to_string(p) { bundle = s; break; }
    }
    if !bundle.ends_with('\n') { bundle.push('\n'); }
    bundle.push_str(&ca.cert_pem);
    let dir = crate::paths::runtime_home().join("ca");
    std::fs::create_dir_all(&dir).ok()?;
    let p = dir.join(format!("box{box_id}.pem"));
    std::fs::write(&p, bundle).ok()?;
    Some(p.to_string_lossy().into_owned())
}

fn dispatch_ui(state: &State, msg: &Value) -> Value {
    let verb = msg.get("verb").and_then(Value::as_str).unwrap_or("");
    let empty = vec![];
    let args = msg.get("args").and_then(Value::as_array).unwrap_or(&empty);
    let boxes = discover::discover();
    let r: Value = match verb {
        "session_dicts" => {
            // discover::session_dict reads on-disk metadata only, so every box
            // looks "finished" to it. Override `live` / `status` / `pid` for
            // boxes whose runner is still registered with this engine — that's
            // what the UI uses to flip on live-mode rendering (e.g., the
            // boxes view's "recently changed" panel and the procs view's
            // active-set behavior).
            let runpids: std::collections::HashMap<i64, i32> =
                state.lock().unwrap().box_runpids.clone();
            Value::Array(boxes.values().map(|b| {
                let mut sd = discover::session_dict(&boxes, b);
                if let Some(&pid) = runpids.get(&b.box_id) {
                    if let Some(obj) = sd.as_object_mut() {
                        obj.insert("live".into(), Value::Bool(true));
                        obj.insert("status".into(),
                                   Value::String("running".into()));
                        obj.insert("pid".into(),
                                   Value::Number((pid as i64).into()));
                        obj.insert("run_pid".into(),
                                   Value::Number((pid as i64).into()));
                    }
                }
                sd
            }).collect())
        }
        "display_path" => match arg_sid(args) {
            Some(id) => Value::String(discover::display_path(&boxes, id)),
            None => Value::Null,
        },
        "resolve_box" => match args.first().and_then(Value::as_str)
            .and_then(|s| resolve(&boxes, s)) {
            Some(id) => Value::String(id.to_string()),
            None => Value::Null,
        },
        "select" => {
            if let Some(id) = arg_sid(args) {
                state.lock().unwrap().selected = Some(id.to_string());
            }
            json!({"ok": true})
        }
        "processes" => match arg_sid(args) {
            Some(id) => discover::processes(id),
            None => json!([]),
        },
        "outputs" => match arg_sid(args) {
            Some(id) => discover::outputs(id),
            None => json!([]),
        },
        "api_log" => match arg_sid(args) {
            Some(id) => discover::api_log(id),
            None => json!([]),
        },
        "api_log_detail" => match (arg_sid(args), args.get(1).and_then(Value::as_i64)) {
            (Some(id), Some(rid)) => discover::api_log_detail(id, rid),
            _ => Value::Null,
        },
        "brushprov" => match arg_sid(args) {
            Some(id) => discover::brushprov(id),
            None => json!([]),
        },
        // Phase 1 embedded-ninja: the parsed build-graph edges (outs/ins/cmd),
        // including up-to-date targets that never executed.
        "build_edges" => match arg_sid(args) {
            Some(id) => discover::build_edges(id),
            None => json!([]),
        },
        // D9 brush↔process linkage joins (both directions).
        "proc_pipeline" => match (arg_sid(args), args.get(1).and_then(Value::as_i64)) {
            (Some(id), Some(rid)) => discover::proc_pipeline(id, rid),
            _ => Value::Null,
        },
        "pipeline_procs" => match (arg_sid(args), args.get(1).and_then(Value::as_i64)) {
            (Some(id), Some(pid)) => discover::pipeline_procs(id, pid),
            _ => json!([]),
        },
        "output_detail" => match (arg_sid(args), args.get(1).and_then(Value::as_i64)) {
            (Some(id), Some(oid)) => discover::output_detail(id, oid),
            _ => Value::Null,
        },
        "processes_live" => {
            // For a box whose runner is still registered the engine returns
            // the captured process set as the "live" snapshot; the UI uses
            // null vs non-null to choose live-style vs finished-style
            // rendering. Without a separate exit-tracking pass the set
            // includes already-exited rows too — but the prototype's
            // strict-active behavior would need engine-level exit
            // detection (a separate ticket).
            let live_sids: std::collections::HashSet<i64> =
                state.lock().unwrap().box_runpids.keys().copied().collect();
            match arg_sid(args) {
                Some(id) if live_sids.contains(&id) => discover::processes(id),
                _ => Value::Null,
            }
        }
        "proc_info" => match (arg_sid(args), args.get(1).and_then(Value::as_i64)) {
            (Some(id), Some(rid)) => discover::proc_info(id, rid),
            _ => Value::Null,
        },
        "proc_prov" => match (arg_sid(args), args.get(1).and_then(Value::as_i64)) {
            (Some(id), Some(rid)) => discover::proc_prov(id, rid),
            _ => Value::Null,
        },
        "proc_roots" => match arg_sid(args) {
            Some(id) => discover::proc_roots(id), None => json!([]),
        },
        "process_env" => match (arg_sid(args), args.get(1).and_then(Value::as_i64)) {
            (Some(id), Some(rid)) => discover::process_env(id, rid),
            _ => json!({}),
        },
        "writer_id" => match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
            (Some(id), Some(rel)) => discover::writer_id(id, rel), _ => Value::Null,
        },
        "first_writer_id" => match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
            (Some(id), Some(rel)) => discover::first_writer_id(id, rel), _ => Value::Null,
        },
        "first_writer_prov" => match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
            (Some(id), Some(rel)) => discover::first_writer_prov(id, rel), _ => Value::Null,
        },
        "kill" => match arg_sid(args) {
            Some(id) => {
                let fd = state.lock().unwrap().box_pids.get(&id).copied();
                match fd {
                    Some(fd) => { pidfd_signal(fd, libc::SIGTERM); json!({"ok": true}) }
                    None => json!({"ok": false, "error": "box not running"}),
                }
            }
            None => json!({"ok": false, "error": "no slopbox"}),
        },
        "dissolve" => match arg_sid(args) {
            Some(id) => dissolve(state, id),
            None => json!({"ok": false, "error": "no slopbox"}),
        },
        "reload_rules" => {
            if let Some(ov) = state.lock().unwrap().overlay.clone() {
                ov.reload_rules();
            }
            json!(null)
        }
        "rescan" => json!(null),   // discovery is always fresh; nothing to do
        "delete" => match arg_sid(args) {
            Some(id) => { reap(state, id); json!({"ok": true, "sid": id.to_string()}) }
            None => json!({"ok": false}),
        },
        "open_files" => json!([]),
        "review_state" => json!({
            "consolidating": [], "consolidated": [],
            "selected": state.lock().unwrap().selected,
        }),
        "review_live" => json!(false),
        "review.session_changes" => match arg_sid(args) {
            Some(id) => crate::review::session_changes(id),
            None => json!([]),
        },
        "review.hunks" => {
            match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
                (Some(id), Some(rel)) => crate::review::hunks(id, rel),
                _ => json!({"is_text": false, "hunks": [],
                            "diff": {"kind": "error", "error": "bad args"}}),
            }
        }
        "review.apply" => match arg_sid(args) {
            Some(id) => {
                let ctx = crate::review::NestCtx::new(
                    state.lock().unwrap().overlay.clone());
                let r = crate::review::apply(id,
                    args.get(1).unwrap_or(&Value::Null), &ctx);
                drop_if_empty(state, id); r }
            None => json!({"applied": [], "errors": []}),
        },
        "review.discard" => match arg_sid(args) {
            Some(id) => {
                let ctx = crate::review::NestCtx::new(
                    state.lock().unwrap().overlay.clone());
                let r = crate::review::discard(id,
                    args.get(1).unwrap_or(&Value::Null), &ctx);
                drop_if_empty(state, id); r }
            None => json!({"discarded": [], "errors": []}),
        },
        "review.patch_text" => match arg_sid(args) {
            Some(id) => {
                let data = crate::review::patch_text(id);
                json!({"__b": base64::engine::general_purpose::STANDARD.encode(&data)})
            }
            None => json!({"__b": ""}),
        },
        "review.change_mode" => match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
            (Some(id), Some(rel)) => match crate::review::current_mode(id, rel) {
                Some(m) => json!(m), None => Value::Null,
            },
            _ => Value::Null,
        },
        "review.decorate" => match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
            (Some(id), Some(rel)) => crate::review::decorate(id, rel),
            _ => json!({"is_text": false, "stale": false, "kind": "changed"}),
        },
        // Newest-first slice of the box's change set, for the boxes view's
        // "recently changed" panel on a live box. limit defaults to 200.
        "review.recent_changes" => {
            let id = arg_sid(args);
            let limit = args.get(1).and_then(Value::as_i64).unwrap_or(200);
            match id {
                Some(id) => crate::review::recent_changes(id, limit),
                None => Value::Array(vec![]),
            }
        }
        // Five-list bundle for the Sessions-view right pane: newest-first
        // outputs / changes / processes / pipelines / build-edges in one
        // round-trip, capped at `limit` per kind (default 20). Changes
        // includes xattr modifications inline as kind="xattr" rows.
        "review.box_summary" => {
            let id = arg_sid(args);
            let limit = args.get(1).and_then(Value::as_i64).unwrap_or(20);
            match id {
                Some(id) => crate::review::box_summary(id, limit),
                None => json!({"outputs":[], "changes":[], "processes":[],
                               "pipelines":[], "edges":[]}),
            }
        }
        // Bulk decorate: one RPC for a whole window of changes-pane rows
        // (kind / stale / is_text per row) — the UI uses this to label the
        // changes list with +/~/- glyphs and the `!` stale marker without a
        // round-trip per row.
        "review.decorate_many" => {
            let id = arg_sid(args);
            let rels: Vec<&str> = args.get(1).and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match id {
                Some(id) => crate::review::decorate_many(id, &rels),
                None => Value::Array(vec![]),
            }
        }
        "review.apply_hunk" => match (arg_sid(args), args.get(1).and_then(Value::as_str),
                                      args.get(2).and_then(Value::as_i64)) {
            (Some(id), Some(rel), Some(ix)) => {
                let r = crate::review::apply_hunk(id, rel, ix);
                drop_if_empty(state, id); r
            }
            _ => json!({"ok": false, "error": "bad args"}),
        },
        "review.discard_hunk" => match (arg_sid(args), args.get(1).and_then(Value::as_str),
                                        args.get(2).and_then(Value::as_i64)) {
            (Some(id), Some(rel), Some(ix)) => {
                let r = crate::review::discard_hunk(id, rel, ix);
                drop_if_empty(state, id); r
            }
            _ => json!({"ok": false, "error": "bad args"}),
        },
        // ── server-side windowed views over per-box data ────────────────────
        // The UI lists are millions of rows in the limit; shipping the whole
        // set client-side made keystrokes multi-second. These verbs let the
        // client open a materialized view (filtered + sorted) and read it as
        // small windows — see views.rs.
        "view.open" => {
            let kind = args.first().and_then(Value::as_str).unwrap_or("");
            // Accept either an int or a string-of-int for the sid (the Python
            // and Rust UIs both send strings, matching the existing verbs;
            // tests sometimes pass ints).
            let sid = args.get(1).and_then(|v| v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
            let filter = args.get(2).cloned().unwrap_or(Value::Null);
            let mut s = state.lock().unwrap();
            let Shared { views, next_view_id, .. } = &mut *s;
            crate::views::open(views, next_view_id, kind, sid, &filter)
        }
        "view.window" => {
            let vid = args.first().and_then(Value::as_u64).unwrap_or(0);
            let start = args.get(1).and_then(Value::as_u64).unwrap_or(0) as usize;
            let size = args.get(2).and_then(Value::as_u64).unwrap_or(0) as usize;
            let s = state.lock().unwrap();
            crate::views::window(&s.views, vid, start, size)
        }
        "view.filter" => {
            let vid = args.first().and_then(Value::as_u64).unwrap_or(0);
            let filter = args.get(1).cloned().unwrap_or(Value::Null);
            let mut s = state.lock().unwrap();
            crate::views::set_filter(&mut s.views, vid, &filter)
        }
        "view.close" => {
            let vid = args.first().and_then(Value::as_u64).unwrap_or(0);
            let mut s = state.lock().unwrap();
            crate::views::close(&mut s.views, vid)
        }
        // At-rest-the-instant-it-exits Rust box (DESIGN.md D4): no consolidate
        // phase, no separate caches — these UI lifecycle pokes are vacuous, but
        // must not return "unknown verb".
        "consolidate_start" | "review.invalidate_consolidation"
        | "review.invalidate_struct" => json!(null),
        "ping" => {
            broadcast(state, &json!({"type": "pong"}));
            json!("pong")
        }
        "box_new" => {
            // m3a: create a box and expose <mnt>/<id> — the overlay-core path
            // (the full runner register handshake is m3b).
            let ov = state.lock().unwrap().overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not mounted"});
            };
            let id = boxes.keys().max().copied().unwrap_or(0) + 1;
            // optional parent arg (args[0]) — nests the new box for KIDS_DIR.
            let parent = args.first().and_then(Value::as_str)
                .and_then(|s| s.parse::<i64>().ok());
            match crate::capture::BoxState::create(id) {
                Ok(b) => {
                    b.set_parent(parent);
                    if let Some(p) = parent {
                        b.set_meta("parent_box_id", &p.to_string());
                    }
                    ov.add_box(std::sync::Arc::new(b));
                    // Same announce as register(): attached UIs need
                    // to know a new box exists. Without this the
                    // session list only updates on the next event of
                    // any kind (or a manual refresh).
                    broadcast(state, &json!({
                        "type": "session_added",
                        "sid": id.to_string(),
                        "parent": parent,
                    }));
                    json!({"sid": id.to_string(),
                           "root": crate::paths::mnt_point().join(id.to_string())
                                   .to_string_lossy()})
                }
                Err(e) => return json!({"ok": false,
                                        "error": format!("box_new: {e}")}),
            }
        }
        "struct_quick" => {
            match (arg_sid(args), args.get(1).and_then(Value::as_str)) {
                (Some(id), Some(rel)) => crate::review::struct_quick(id, rel),
                _ => json!({"lines": [["err", "bad args"]], "job": Value::Null}),
            }
        }
        // ── flows pane: tshark-decoded HTTP/TLS rows for one box's pcapng ──
        // flows.list  [SID]              → {ok, flows: [row, ...]}
        // flows.detail [SID, FRAME]      → {ok, text: "..."}
        // SID may be omitted to mean the currently-selected box.
        "flows.list" => {
            match arg_sid(args).or_else(|| selected_sid(state)) {
                Some(id) => match flows_dir_for(id) {
                    Some(dir) => match crate::net::flows::tshark_list(&dir) {
                        Ok(rows) => json!({"ok": true,
                            "flows": rows.iter().map(|r| r.to_json())
                                .collect::<Vec<_>>()}),
                        Err(e) => json!({"ok": false, "error": e}),
                    },
                    None => json!({"ok": false, "error": "no flows dir for box"}),
                },
                None => json!({"ok": false, "error": "no box selected"}),
            }
        }
        "flows.detail" => {
            let frame = args.get(1).and_then(Value::as_u64).unwrap_or(0);
            match arg_sid(args).or_else(|| selected_sid(state)) {
                Some(id) if frame > 0 => match flows_dir_for(id) {
                    Some(dir) => match crate::net::flows::tshark_detail(&dir, frame) {
                        Ok(text) => json!({"ok": true, "text": text}),
                        Err(e) => json!({"ok": false, "error": e}),
                    },
                    None => json!({"ok": false, "error": "no flows dir for box"}),
                },
                _ => json!({"ok": false, "error": "bad args"}),
            }
        }
        // ── banner-prompt queue verbs (the TUI is the consumer) ────────
        // prompts.peek                          → {ok, ask: {...}|null}
        // prompts.answer [ID, "yes_once|no_once|allow_save|deny_save"]
        //                                       → {ok}
        // prompts.ui_active [bool]              → {ok}
        //   The TUI calls ui_active(true) on startup and ui_active(false)
        //   on shutdown; while inactive, dispatcher Ask short-circuits to
        //   deny so no connection wedges on an absent UI.
        "prompts.peek" => {
            match state.lock().unwrap().net.clone() {
                Some(net) => match net.prompts.peek() {
                    Some(ask) => json!({"ok": true, "ask": {
                        "id": ask.id, "box": ask.box_name,
                        "host": ask.host, "port": ask.port,
                        "scheme": ask.scheme,
                    }}),
                    None => json!({"ok": true, "ask": Value::Null}),
                },
                None => json!({"ok": true, "ask": Value::Null}),
            }
        }
        "prompts.answer" => {
            let id = args.first().and_then(Value::as_u64).unwrap_or(0);
            let v = args.get(1).and_then(Value::as_str).unwrap_or("");
            let Some(verdict) = crate::net::prompt::Verdict::parse(v) else {
                return json!({"ok": false, "error": "bad verdict"});
            };
            match state.lock().unwrap().net.clone() {
                Some(net) => {
                    let ok = net.prompts.answer(id, verdict);
                    // Net rules are reloaded from disk by the dispatcher on
                    // every connection (Rules::load() is cheap), so the
                    // newly-appended line takes effect immediately for
                    // future conns without touching the FUSE-side rule
                    // cache. (Doing the reload synchronously here was
                    // hanging on RwLock contention with the FUSE serve
                    // threads.)
                    json!({"ok": ok})
                }
                None => json!({"ok": false, "error": "no net registry"}),
            }
        }
        "prompts.ui_active" => {
            let on = args.first().and_then(Value::as_bool).unwrap_or(false);
            if let Some(net) = state.lock().unwrap().net.clone() {
                net.prompts.mark_ui_active(on);
            }
            json!({"ok": true})
        }
        // flows.packets [SID, STREAM] → every frame in `tcp.stream == STREAM`
        // (i.e. the connection the user just drilled into from the flows
        // list pane). Powers the packet-list view inside Pane::Packets.
        "flows.packets" => {
            let stream = args.get(1).and_then(Value::as_i64).unwrap_or(-1);
            match arg_sid(args).or_else(|| selected_sid(state)) {
                Some(id) if stream >= 0 => match flows_dir_for(id) {
                    Some(dir) => match crate::net::flows::tshark_packets(&dir, stream) {
                        Ok(rows) => json!({"ok": true,
                            "packets": rows.iter().map(|r| r.to_json())
                                .collect::<Vec<_>>()}),
                        Err(e) => json!({"ok": false, "error": e}),
                    },
                    None => json!({"ok": false, "error": "no flows dir for box"}),
                },
                _ => json!({"ok": false, "error": "bad args"}),
            }
        }
        "struct_finish" => match args.first().and_then(Value::as_i64) {
            Some(job) => crate::review::struct_finish(job),
            None => json!({"lines": [["err", "bad job"]]}),
        },
        "struct_cancel" => {
            if let Some(job) = args.first().and_then(Value::as_i64) {
                crate::review::struct_cancel(job);
            }
            return json!({"ok": true, "r": Value::Null});
        }
        "box_drop" => {
            let ov = state.lock().unwrap().overlay.clone();
            if let (Some(ov), Some(id)) = (ov, arg_sid(args)) {
                ov.remove_box(id);
            }
            json!({"ok": true})
        }
        // ── box-rooted file ops — the engine-side half of oaita's read/write/
        //    inspect tools. Resolve name→id, hydrate the parent chain, then
        //    use the same overlay API nested boxes use. No FUSE mount needed,
        //    no subprocess. args: [name_or_id, path_rel_to_root, (write only)
        //    base64-bytes]. path must NOT start with '/'.
        "box_file_read" => {
            let ov = state.lock().unwrap().overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_file_read: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            match ov.box_read_file(id, rel) {
                Ok(bytes) => {
                    use base64::{Engine, prelude::BASE64_STANDARD};
                    json!({"bytes": BASE64_STANDARD.encode(bytes)})
                }
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            }
        }
        "box_file_write" => {
            let ov = state.lock().unwrap().overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_file_write: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            let b64 = args.get(2).and_then(Value::as_str).unwrap_or("");
            use base64::{Engine, prelude::BASE64_STANDARD};
            let bytes = match BASE64_STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => return json!({"ok": false,
                    "error": format!("bad base64: {e}")}),
            };
            match ov.box_write_file(id, rel, &bytes) {
                Ok(()) => json!({"len": bytes.len()}),
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            }
        }
        "box_dir_list" => {
            let ov = state.lock().unwrap().overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_dir_list: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            match ov.box_list_dir(id, rel) {
                Ok(entries) => Value::Array(entries.into_iter()
                    .map(|(n, k)| json!({"name": n, "kind": k.to_string()}))
                    .collect()),
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            }
        }
        "box_path_kind" => {
            let ov = state.lock().unwrap().overlay.clone();
            let Some(ov) = ov else {
                return json!({"ok": false, "error": "overlay not available"});
            };
            let Some(ident) = args.first().and_then(Value::as_str) else {
                return json!({"ok": false, "error": "box_path_kind: missing name/id"});
            };
            let Some(id) = resolve(&boxes, ident) else {
                return json!({"ok": false, "error": format!("no such box: {ident}")});
            };
            let rel = args.get(1).and_then(Value::as_str).unwrap_or("");
            json!({"kind": ov.box_path_kind(id, rel).to_string()})
        }
        other => {
            return json!({"ok": false, "error": format!("unknown verb '{other}'")});
        }
    };
    json!({"ok": true, "r": r})
}

/// One recvmsg on the box channel: read up to `buf` bytes and capture the first
/// SCM_RIGHTS fd if any (a MUTE frame attaches --inner's pidfd). Returns the
/// byte count (0 = EOF, <0 = error) and sets `*fd` to a received fd.
fn recv_frame_bytes(raw: i32, buf: &mut [u8], fd: &mut Option<i32>) -> isize {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    // socklen_t on glibc, size_t on musl — `as _` matches the field type.
    msg.msg_controllen = cmsg.len() as _;
    let n = unsafe { libc::recvmsg(raw, &mut msg, 0) };
    if n > 0 {
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET
                    && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let mut got = 0i32;
                    std::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(c), (&mut got as *mut i32).cast(), 4);
                    if fd.is_none() { *fd = Some(got); } else { libc::close(got); }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
    }
    n
}

/// A UnixStream with a few leading bytes (already pulled into the BufReader
/// before we noticed this was a PTY connection) replayed first on read. Writes
/// and clones go straight to the underlying stream. This lets `pty::serve_pty`
/// treat the whole connection — prebuffered frame bytes included — as one Read.
struct Prebuffered {
    pre: Vec<u8>,
    pos: usize,
    inner: UnixStream,
}

impl std::io::Read for Prebuffered {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.pre.len() {
            let n = (self.pre.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.pre[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}
impl std::io::Write for Prebuffered {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.inner.write(buf) }
    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}
impl crate::pty::CloneStream for Prebuffered {
    fn clone_stream(&self) -> Self {
        // The clone shares the socket but NOT the one-time prebuffer (only the
        // original replays it, so bytes are never delivered twice).
        Prebuffered {
            pre: vec![],
            pos: 0,
            inner: self.inner.try_clone().expect("UnixStream::try_clone"),
        }
    }
    fn shutdown_read(&self) {
        let _ = self.inner.shutdown(std::net::Shutdown::Read);
    }
}

/// Engine-held-PTY connection (D7/D9). `msg` is the `pty_spawn` request:
///   {"type":"pty_spawn","argv":[...],"rows":R,"cols":C}
/// We ack one JSON line, then the connection becomes a bidirectional FRAME_PTY_*
/// mux driven by `crate::pty::serve_pty` (master↔client + EOF). `prebuf` is any
/// bytes the BufReader already consumed past the request line.
///
/// HONEST SCOPE: the command is spawned DIRECTLY on the engine's PTY (no bwrap /
/// overlay box). The mux/render/input loop is the proven, reusable half; wrapping
/// the PTY child in an overlay-backed box reuses this exact frame mux and is the
/// documented follow-on (DESIGN.md D9 — PTY mode toggle over the box channel).
fn handle_pty_spawn(msg: &Value, writer: &mut UnixStream, prebuf: Vec<u8>) {
    let argv: Vec<String> = msg.get("argv").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if argv.is_empty() {
        let _ = writer.write_all(b"{\"ok\":false,\"error\":\"pty_spawn: empty argv\"}\n");
        return;
    }
    let rows = msg.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
    let cols = msg.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
    // cwd: the UI's $PWD at the moment it sent the spawn — what the user
    // sees as "where I am". Without this the child inherits the engine
    // daemon's cwd (whatever it was when the daemon started, usually $HOME)
    // and `bash -i` opens in the wrong dir. Engine daemon is long-lived so
    // its own cwd is unreliable; the UI's is correct per-launch.
    let cwd: Option<std::path::PathBuf> = msg.get("cwd").and_then(Value::as_str)
        .map(std::path::PathBuf::from);
    // env: portable_pty's CommandBuilder starts from a MINIMAL env by
    // default — SHELL/HOME/USER/PATH absent, so `bash -i` lands in a
    // broken shell. The UI ships its own envvars and we lay them on top
    // of the daemon's so the user gets a normal session.
    let env: Vec<(String, String)> = msg.get("env").and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)|
            v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default();
    // Ack BEFORE the frame mux begins so the client knows to switch to frames.
    if writer.write_all(b"{\"ok\":true,\"r\":\"pty\"}\n").is_err() {
        return;
    }
    let _ = writer.flush();
    let stream = match writer.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let chan = Prebuffered { pre: prebuf, pos: 0, inner: stream };
    crate::pty::serve_pty(&argv, rows, cols, chan, None,
                          cwd.as_deref(), &env);
}

fn handle(state: State, conn: UnixStream) {
    // The register handshake carries the runner's pidfd as the connection's
    // first SCM_RIGHTS fd; keep it for host-pid derivation + kill. It belongs
    // to the FIRST message only (a register); close it if that never comes.
    let mut peer_pidfd = recv_first_fd(&conn);
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => {
            if let Some(fd) = peer_pidfd { unsafe { libc::close(fd); } }
            return;
        }
    });
    let mut writer = conn;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                if let Some(fd) = peer_pidfd.take() { unsafe { libc::close(fd); } }
                return;
            }
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            if let Some(fd) = peer_pidfd.take() { unsafe { libc::close(fd); } }
            return;
        };
        // Engine-held PTY (D7/D9): this connection becomes a bidirectional
        // FRAME_PTY_* mux. Handled in its own function, fully separate from the
        // newline-JSON verb dispatch below. Any register pidfd is irrelevant to a
        // PTY connection, so close it.
        if msg.get("type").and_then(Value::as_str) == Some("pty_spawn") {
            if let Some(fd) = peer_pidfd.take() { unsafe { libc::close(fd); } }
            handle_pty_spawn(&msg, &mut writer, reader.buffer().to_vec());
            return;
        }
        // The oaita API proxy lives on the existing box-channel as new
        // FRAME_API_* frame types — not as a top-level connection type.
        // See the FRAME_API_* handling in the post-register frame loop
        // below and frames::FRAME_API_{OPEN,DATA,CLOSE}.
        let mut reply = if msg.get("type").and_then(Value::as_str) == Some("register") {
            register(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("brush_prov_nested") {
            // D9 nested-shell provenance: a one-shot control message from the
            // brush-sh shim, carrying its OWN pidfd (like register) so we resolve
            // the enclosing box from /proc ancestry. NOT a box channel — record
            // and reply once, then the connection closes. The pidfd is consumed.
            brush_prov_nested(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("build_edges") {
            // Phase 1 embedded-ninja: a one-shot control message from the
            // shadowed `ninja` (vendored n2) carrying its OWN pidfd, resolved to
            // the enclosing box by /proc ancestry exactly like brush_prov_nested.
            build_edges(&state, &msg, peer_pidfd.take())
        } else {
            dispatch(&state, &msg)
        };
        let subscribe = reply.get("_subscribe").and_then(Value::as_bool)
            .unwrap_or(false);
        let box_sid = reply.as_object_mut()
            .and_then(|o| o.remove("_box_sid"))
            .and_then(|v| v.as_i64());
        if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
            return;
        }
        if let Some(id) = box_sid {
            // This connection IS the box's muxed channel now. Register it as the
            // echo writer (the sink-write handler frames captured bytes onto it),
            // then read MUTE/UNMUTE frames from --inner: MUTE carries --inner's
            // pidfd (SCM_RIGHTS) → resolve its host pid and mute it (its echo
            // readback is not re-recorded); UNMUTE / EOF unmutes. EOF = teardown.
            let ov = state.lock().unwrap().overlay.clone();
            if let Some(ov) = ov.as_ref() {
                if let Ok(w) = writer.try_clone() {
                    ov.set_echo(id, Arc::new(Mutex::new(w)));
                }
            }
            let raw = reader.get_ref().as_raw_fd();
            let mut fbuf = reader.buffer().to_vec();
            let mut pending_fd: Option<i32> = None;
            let mut muted_pid: Option<i32> = None;
            // Per-box-channel oaita API mux. Created lazily on the first
            // FRAME_API_OPEN this channel sees — most boxes never call the
            // proxy, so we skip the tokio-runtime + Proxy lookup on the
            // common path. When a box did register with `--api`, the
            // runner forwards in-box `/run/sarun/api.sock` connections as
            // FRAME_API_* streams on this channel; the mux demultiplexes.
            let mut api_mux: Option<crate::oaita::proxy_mux::ApiMux> = None;
            loop {
                let (frames, used) = crate::frames::decode(&fbuf);
                for (ft, _) in &frames {
                    match *ft {
                        crate::frames::FRAME_MUTE => {
                            if let Some(fd) = pending_fd.take() {
                                let hp = host_pid_from_pidfd(fd);
                                unsafe { libc::close(fd); }
                                if hp > 0 {
                                    muted_pid = Some(hp);
                                    if let Some(ov) = ov.as_ref() {
                                        ov.mute_add(hp, id);
                                        // D9 brush↔process linkage: for a brush box
                                        // the muted pid IS the embedded brush shell's
                                        // --inner host tgid — the forest root every
                                        // pipeline process descends from. Record it so
                                        // brush-descendant rows can be attributed.
                                        if let Some(b) = ov.live_box(id) {
                                            if b.is_brush() {
                                                b.set_brush_host_tgid(hp as u32);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        crate::frames::FRAME_UNMUTE => {
                            if let (Some(hp), Some(ov)) =
                                (muted_pid.take(), ov.as_ref()) {
                                ov.mute_remove(hp);
                            }
                        }
                        _ => {}
                    }
                }
                // D9 brush provenance (separate from MUTE/UNMUTE): a FRAME_PROV
                // carries a JSON object describing one shell command the box's
                // embedded brush shell ran. Record it into the box's sqlar and
                // broadcast a `brush_prov` event so live UIs see it.
                for (ft, payload) in &frames {
                    if *ft == crate::frames::FRAME_PROV {
                        record_brush_prov(&state, &ov, id, payload);
                    }
                }
                // oaita API proxy frames. Lazy-init the per-channel mux on
                // first OPEN so non-api boxes pay nothing. After init,
                // every FRAME_API_* on this channel feeds the same mux —
                // attribution is implicit because the channel IS the box.
                for (ft, payload) in &frames {
                    if !matches!(*ft, crate::frames::FRAME_API_OPEN
                                 | crate::frames::FRAME_API_DATA
                                 | crate::frames::FRAME_API_CLOSE) {
                        continue;
                    }
                    if api_mux.is_none() {
                        let (rt_opt, proxy_opt, writer_opt) = {
                            let s = state.lock().unwrap();
                            (s.net_rt.clone(), s.api_proxy.clone(),
                             ov.as_ref().and_then(|ov| ov.echo_writer(id)))
                        };
                        if let (Some(rt), Some(proxy), Some(writer)) =
                            (rt_opt, proxy_opt, writer_opt)
                        {
                            api_mux = Some(crate::oaita::proxy_mux::ApiMux::new(
                                id, proxy, rt, writer));
                        }
                    }
                    let Some(mux) = api_mux.as_ref() else { continue; };
                    let Some((stream_id, body)) =
                        crate::frames::api_parse(payload) else { continue; };
                    match *ft {
                        crate::frames::FRAME_API_OPEN  => mux.open(stream_id),
                        crate::frames::FRAME_API_DATA  => mux.data(stream_id, body),
                        crate::frames::FRAME_API_CLOSE => mux.close(stream_id),
                        _ => unreachable!(),
                    }
                }
                fbuf.drain(..used);
                if frames.is_empty() {
                    let mut tmp = [0u8; 4096];
                    let mut fd = None;
                    let n = recv_frame_bytes(raw, &mut tmp, &mut fd);
                    if n <= 0 { break; }
                    if let Some(f) = fd {
                        if let Some(old) = pending_fd.replace(f) {
                            unsafe { libc::close(old); }
                        }
                    }
                    fbuf.extend_from_slice(&tmp[..n as usize]);
                }
            }
            if let Some(mux) = &api_mux { mux.shutdown(); }
            if let Some(fd) = pending_fd { unsafe { libc::close(fd); } }
            if let Some(ov) = ov.as_ref() {
                if let Some(hp) = muted_pid { ov.mute_remove(hp); }
                // D9 brush↔process linkage: now that the box channel hit EOF the
                // brush shell has exited — ALL pipelines + process rows exist, so
                // attribute every brush-spawned process to its pipeline in one
                // race-free pass (no-op for non-brush boxes).
                if let Some(b) = ov.live_box(id) { b.finalize_brush_links(); }
                ov.clear_echo(id);
                ov.remove_box(id);
            }
            {
                let mut s = state.lock().unwrap();
                if let Some(fd) = s.box_pids.remove(&id) {
                    unsafe { libc::close(fd); }
                }
                s.box_runpids.remove(&id);
                if let Some(p) = s.api_proxy.clone() {
                    p.disable_box(id);
                }
            }
            broadcast(&state, &json!({"type": "session_removed",
                                      "session_id": id.to_string()}));
            return;
        }
        if subscribe {
            // The connection becomes a one-way event feed: park it in the
            // subscriber list; broadcast() writes to it and prunes on error.
            state.lock().unwrap().subscribers.push(writer);
            return;
        }
    }
}

/// A leading CLI token that names a box: all-caps start, [A-Z0-9-.] body
/// (dots allow a nested display path A.B), no trailing '-'. Mirrors the Python
/// `valid_dotted_name` gate that turns `slopbox NAME …` into a box op.
pub fn is_box_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()
                         || c == '-' || c == '.')
        && !s.ends_with('-')
}

/// CLI conveniences `sarun-engine NAME [op [arg]]` — connect to the running
/// engine's control socket and act on the named box (the verbs already exist
/// engine-side). `NAME` alone selects; `patch` prints the unified diff; `apply`
/// / `discard` act on the whole box; `rename NEW` renames. Mirrors the Python
/// `slopbox NAME patch|apply|discard|rename`.
pub fn cli_box_op(argv: &[String]) -> i32 {
    let name = argv[0].as_str();
    let op = argv.get(1).map(String::as_str);
    // IN-BOX vs HOST socket selection: a `sarun OAITA-X discard` invoked
    // from INSIDE a box (e.g. by oaita's cleanup_spawned_subagents when a
    // sub-agent settles) reaches the engine via the UI socket bind-mounted
    // at /run/sarun/ui.sock (the path runner.rs uses). The host
    // runtime path isn't connectable from inside the box (different
    // namespace, no bind mount). Mirror runner::run's path-presence
    // detection.
    const UI_SOCK_INBOX: &str = "/run/sarun/ui.sock";
    let sock = if std::path::Path::new(UI_SOCK_INBOX).exists() {
        std::path::PathBuf::from(UI_SOCK_INBOX)
    } else {
        crate::paths::sock_path()
    };
    let one = |msg: Value| -> Result<Value, String> {
        let mut c = UnixStream::connect(&sock)
            .map_err(|_| "no engine running".to_string())?;
        c.write_all(format!("{msg}\n").as_bytes()).map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(&c).read_line(&mut line).map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    };
    let report = |r: Result<Value, String>| -> i32 {
        match r {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => 0,
            Ok(v) => { eprintln!("sarun-engine: {}",
                v.get("error").and_then(Value::as_str).unwrap_or("failed")); 1 }
            Err(e) => { eprintln!("sarun-engine: {e}"); 1 }
        }
    };
    match op {
        None => report(one(json!({"type": "select", "sid": name}))),
        Some("apply") | Some("discard") => {
            let t = op.unwrap();
            match one(json!({"type": t, "sid": name})) {
                Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                    println!("{}: {} {}", name,
                        v.get("count").and_then(Value::as_i64).unwrap_or(0), t);
                    0
                }
                other => report(other),
            }
        }
        Some("rename") => {
            let new = argv.get(2).map(String::as_str).unwrap_or("");
            report(one(json!({"type": "rename", "sid": name, "name": new})))
        }
        Some("patch") => {
            match one(json!({"type": "patch", "sid": name})) {
                Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                    if let Some(b64) = v.get("patch").and_then(Value::as_str) {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD
                            .decode(b64) {
                            use std::io::Write;
                            let _ = std::io::stdout().write_all(&bytes);
                        }
                    }
                    0
                }
                other => report(other),
            }
        }
        Some(o) => { eprintln!("sarun-engine: unknown op '{o}'"); 2 }
    }
}

pub fn serve(state: State, sock: &std::path::Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock)?;
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o600);
    std::fs::set_permissions(sock, mode)?;
    // ALSO listen on an abstract socket keyed off the same path. Boxes
    // reach this via the host netns without any filesystem path — no
    // bind-mount into the box's /tmp or /run, no mkdir, no permissions
    // path to navigate. Abstract sockets are scoped to the network
    // namespace; every box that shares host netns (which every --api
    // box does, since the proxy needs upstream access) reaches it.
    // For -n boxes that unshare netns, abstract is unreachable AND
    // they have no engine business anyway.
    let abs_listener = abstract_listener(sock)?;
    let st_abs = state.clone();
    std::thread::spawn(move || {
        for conn in abs_listener.incoming().flatten() {
            let st = st_abs.clone();
            std::thread::spawn(move || handle(st, conn));
        }
    });
    for conn in listener.incoming().flatten() {
        let st = state.clone();
        std::thread::spawn(move || handle(st, conn));
    }
    Ok(())
}

/// Bind a Linux abstract Unix socket whose name is keyed off the filesystem
/// `sock` path — that way every engine instance with a distinct XDG_RUNTIME_DIR
/// (the host common case is one engine per user) gets a distinct abstract
/// name. Returns the bound listener.
pub fn abstract_listener(sock: &std::path::Path) -> std::io::Result<UnixListener> {
    use std::os::linux::net::SocketAddrExt;
    let name = abstract_name(sock);
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    UnixListener::bind_addr(&addr)
}

/// The abstract-socket name string for `sock` — `"sarun:<absolute-sock-path>"`.
/// Symmetric helper for clients (runner, in-box oaita) to compute the same name
/// without crossing files.
pub fn abstract_name(sock: &std::path::Path) -> String {
    format!("sarun:{}", sock.display())
}
