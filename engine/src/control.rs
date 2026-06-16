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

fn arg_sid(args: &[Value]) -> Option<i64> {
    args.first()?.as_str()?.parse().ok()
}

/// Unconditionally remove a box: drop it from the overlay, delete its sqlar +
/// backing + pool blobs, broadcast session_removed. The `delete` verb's body.
fn reap(state: &State, id: i64) {
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
    b.set_meta("name", &name);
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
    ov.add_box(std::sync::Arc::new(b));
    let root = crate::paths::mnt_point().join(id.to_string());
    json!({
        "ok": true, "mount": root.to_string_lossy(),
        "shm_dir": backing.to_string_lossy(),
        "owner_token": format!("{:032x}", std::process::id() as u128
                               ^ (id as u128) << 64
                               ^ std::time::SystemTime::now()
                                 .duration_since(std::time::UNIX_EPOCH)
                                 .map(|d| d.as_nanos()).unwrap_or(0)),
        "box_id": id, "session_id": id.to_string(), "name": name,
        "capture": want_capture,      // sinks + live echo mux active (off for -t/-d)
        "_box_sid": id,               // caller marker: this conn is now the box channel
    })
}

fn dispatch_ui(state: &State, msg: &Value) -> Value {
    let verb = msg.get("verb").and_then(Value::as_str).unwrap_or("");
    let empty = vec![];
    let args = msg.get("args").and_then(Value::as_array).unwrap_or(&empty);
    let boxes = discover::discover();
    let r: Value = match verb {
        "session_dicts" => Value::Array(
            boxes.values().map(|b| discover::session_dict(&boxes, b)).collect()),
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
        "brushprov" => match arg_sid(args) {
            Some(id) => discover::brushprov(id),
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
        "processes_live" => Value::Null,  // finished-style: UI falls back to processes()
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
    crate::pty::serve_pty(&argv, rows, cols, chan, None);
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
        let mut reply = if msg.get("type").and_then(Value::as_str) == Some("register") {
            register(&state, &msg, peer_pidfd.take())
        } else if msg.get("type").and_then(Value::as_str) == Some("brush_prov_nested") {
            // D9 nested-shell provenance: a one-shot control message from the
            // brush-sh shim, carrying its OWN pidfd (like register) so we resolve
            // the enclosing box from /proc ancestry. NOT a box channel — record
            // and reply once, then the connection closes. The pidfd is consumed.
            brush_prov_nested(&state, &msg, peer_pidfd.take())
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
                                        ov.mute_add(hp);
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
    let one = |msg: Value| -> Result<Value, String> {
        let mut c = UnixStream::connect(crate::paths::sock_path())
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
    for conn in listener.incoming().flatten() {
        let st = state.clone();
        std::thread::spawn(move || handle(st, conn));
    }
    Ok(())
}
