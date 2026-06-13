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
}

pub type State = Arc<Mutex<Shared>>;

pub fn broadcast(state: &State, ev: &Value) {
    let data = format!("{ev}\n");
    let mut s = state.lock().unwrap();
    s.subscribers.retain(|conn| {
        let mut c = conn;
        c.write_all(data.as_bytes()).is_ok()
    });
}

/// Drain any SCM_RIGHTS fds sent with the connection's first bytes (register
/// handshakes carry a pidfd) and close them — read nothing else; the byte
/// stream itself is consumed by the normal reader afterwards.
fn drain_ancillary(conn: &UnixStream) {
    let mut fdbuf = [0i32; 8];
    let mut io = [0u8; 0];
    let mut iov = libc::iovec { iov_base: io.as_mut_ptr().cast(), iov_len: 0 };
    let mut cmsg = [0u8; 128];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len();
    let n = unsafe {
        libc::recvmsg(conn.as_raw_fd(), &mut msg,
                      libc::MSG_PEEK | libc::MSG_DONTWAIT)
    };
    if n < 0 {
        return;
    }
    // With MSG_PEEK the fds are still delivered (and duplicated); collect and
    // close them. The data bytes stay queued for the BufReader.
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
                    libc::close(fdbuf[i]);
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
}

fn dispatch(state: &State, msg: &Value) -> Value {
    let t = msg.get("type").and_then(Value::as_str).unwrap_or("");
    match t {
        "subscribe" => json!({"ok": true, "_subscribe": true}),
        "register" => register(state, msg),
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
                    let (r, n) = if t == "apply" {
                        let r = crate::review::apply(id, &all);
                        let n = r.get("applied").and_then(Value::as_array)
                            .map(|a| a.len()).unwrap_or(0);
                        (r, n)
                    } else {
                        let r = crate::review::discard(id, &all);
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
                    if let Some(ov) = state.lock().unwrap().overlay.clone() {
                        let _ = ov; // live-box meta is the disk sqlar either way
                    }
                    if let Some(c) = rusqlite::Connection::open(
                        crate::paths::state_home().join(format!("{id}.sqlar"))).ok() {
                        let _ = c.execute(
                            "INSERT INTO meta(key,value) VALUES('name',?1)
                             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                            [newname]);
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

fn arg_sid(args: &[Value]) -> Option<i64> {
    args.first()?.as_str()?.parse().ok()
}

/// After apply/discard, if the box has no remaining changes, remove it from the
/// overlay (live) and delete its now-empty sqlar — the "reap empty" behaviour.
fn drop_if_empty(state: &State, id: i64) {
    if crate::review::session_changes(id).as_array().map(|a| a.is_empty())
        .unwrap_or(false) {
        if let Some(ov) = state.lock().unwrap().overlay.clone() {
            ov.remove_box(id);
        }
        let _ = std::fs::remove_file(crate::paths::state_home()
            .join(format!("{id}.sqlar")));
        let _ = std::fs::remove_dir_all(crate::paths::live_home()
            .join(id.to_string()));
        broadcast(state, &json!({"type": "session_removed",
                                 "session_id": id.to_string()}));
    }
}

/// The runner register handshake (m3b). Mints a box_id, creates the backing
/// sentinel (live/<id>/up) and the box's sqlar (root process row from the
/// message's prov), registers the box on the overlay, and acks with the
/// <mnt>/<id> bind target. The SAME connection becomes the box channel —
/// its EOF (handled by the caller via the _box_sid marker) is teardown.
/// Honest m3b limits: capture mode is downgraded in the ack (no sink files /
/// echo frames yet — the runner then behaves as -t passthrough), and nested
/// (relname) registration is refused.
fn register(state: &State, msg: &Value) -> Value {
    let ov = state.lock().unwrap().overlay.clone();
    let Some(ov) = ov else {
        return json!({"ok": false, "error": "overlay mount is not available"});
    };
    if msg.get("relname").is_some() {
        return json!({"ok": false,
                      "error": "engine m3b: nested boxes not yet supported"});
    }
    let boxes = discover::discover();
    let live_max = ov.box_ids().into_iter().max().unwrap_or(0);
    let id = boxes.keys().max().copied().unwrap_or(0).max(live_max) + 1;
    // NAME: the runner-supplied session_id is only a NAME candidate.
    let want = msg.get("session_id").and_then(Value::as_str).unwrap_or("");
    let valid = !want.is_empty()
        && want.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && want.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()
                            || c == '-')
        && !want.ends_with('-');
    let name = if valid { want.to_string() } else { format!("A{id}") };
    let backing = crate::paths::live_home().join(id.to_string());
    if let Err(e) = std::fs::create_dir_all(backing.join("up")) {
        return json!({"ok": false, "error": format!("backing: {e}")});
    }
    let b = match crate::capture::BoxState::create(id) {
        Ok(b) => b,
        Err(e) => return json!({"ok": false, "error": format!("sqlar: {e}")}),
    };
    b.set_meta("name", &name);
    if let Some(prov) = msg.get("prov") {
        b.root_process(prov);
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
        "capture": false,             // m3b: echo/sinks not yet — runner runs -t-style
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
            Some(id) => { let r = crate::review::apply(id,
                args.get(1).unwrap_or(&Value::Null)); drop_if_empty(state, id); r }
            None => json!({"applied": [], "errors": []}),
        },
        "review.discard" => match arg_sid(args) {
            Some(id) => { let r = crate::review::discard(id,
                args.get(1).unwrap_or(&Value::Null)); drop_if_empty(state, id); r }
            None => json!({"discarded": [], "errors": []}),
        },
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
            match crate::capture::BoxState::create(id) {
                Ok(b) => {
                    ov.add_box(std::sync::Arc::new(b));
                    json!({"sid": id.to_string(),
                           "root": crate::paths::mnt_point().join(id.to_string())
                                   .to_string_lossy()})
                }
                Err(e) => return json!({"ok": false,
                                        "error": format!("box_new: {e}")}),
            }
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

fn handle(state: State, conn: UnixStream) {
    drain_ancillary(&conn);
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut writer = conn;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { return };
        let mut reply = dispatch(&state, &msg);
        let subscribe = reply.get("_subscribe").and_then(Value::as_bool)
            .unwrap_or(false);
        let box_sid = reply.as_object_mut()
            .and_then(|o| o.remove("_box_sid"))
            .and_then(|v| v.as_i64());
        if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
            return;
        }
        if let Some(id) = box_sid {
            // This connection IS the box's channel now: hold it open (frames
            // are ignored in m3b — no echo yet); EOF is the teardown signal.
            let mut sink = [0u8; 4096];
            loop {
                use std::io::Read;
                match reader.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let ov = state.lock().unwrap().overlay.clone();
            if let Some(ov) = ov {
                ov.remove_box(id);
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
