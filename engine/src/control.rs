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
        "register" => json!({"ok": false,
                             "error": "engine m2: boxes not yet supported"}),
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
        other => json!({"ok": false,
                        "error": format!("unknown control type '{other}'")}),
    }
}

fn resolve(boxes: &std::collections::BTreeMap<i64, discover::Box_>,
           ident: &str) -> Option<i64> {
    if let Ok(id) = ident.parse::<i64>() {
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
        "outputs" => json!([]),
        "open_files" => json!([]),
        "review_state" => json!({
            "consolidating": [], "consolidated": [],
            "selected": state.lock().unwrap().selected,
        }),
        "review_live" => json!(false),
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
        let reply = dispatch(&state, &msg);
        let subscribe = reply.get("_subscribe").and_then(Value::as_bool)
            .unwrap_or(false);
        if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
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
