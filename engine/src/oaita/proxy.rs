// Engine-side API proxy. Listens on `{runtime_home}/api.sock` (the same path
// that `--api` boxes get bind-mounted at `/run/sarun/api.sock` inside): a box
// speaks plain HTTP/1.1 over the UDS, we inject the Authorization header from
// `oaita.toml`, forward to the configured upstream, stream the response back,
// and log the (request, response, model, status) pair into the originating
// box's `api_log` sqlar table.
//
// Authentication: the box never sees the API key. The proxy gets it from
// `oaita.toml` (or env vars, same precedence as the client). Per-box opt-in
// is controlled at runner-launch time via the new `--api` flag.
//
// Box attribution: SO_PEERCRED on the accepted UDS conn gives us the
// connecting client's pid; we walk /proc PPid chain up to a registered
// runner host pid (the same trick `derive_parent_box` uses in control.rs)
// and that's the box. Unattributable → box_id=0.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::oaita::client::Client;
use crate::oaita::config::Config;

type Body = BoxBody<Bytes, String>;

/// Per-engine proxy registry (lives on control::Shared.api_proxy). Holds the
/// log sink and the resolved upstream config; rebuilt on `reload`.
pub struct Proxy {
    pub upstream: Mutex<Option<UpstreamConfig>>,
    pub overlay: parking_lot::RwLock<Option<crate::overlay::Overlay>>,
    /// The set of currently-active boxes that were launched with `--api`.
    /// Controlled by control::register / control::box_finish via the engine.
    pub api_boxes: parking_lot::RwLock<std::collections::HashSet<i64>>,
}

#[derive(Clone)]
pub struct UpstreamConfig {
    pub base_url: String,
    pub api_key: String,
    pub model_hint: String, // logged when the request omits a model field
}

impl Proxy {
    pub fn new() -> Self {
        Self {
            upstream: Mutex::new(None),
            overlay: parking_lot::RwLock::new(None),
            api_boxes: parking_lot::RwLock::new(Default::default()),
        }
    }
    pub fn set_overlay(&self, ov: crate::overlay::Overlay) {
        *self.overlay.write() = Some(ov);
    }
    pub async fn reload_config(&self) {
        let cfg = Config::load();
        let (model_hint, base_url, api_key) = cfg.resolve()
            .unwrap_or_else(|_| ("".into(), "".into(), "".into()));
        if base_url.is_empty() {
            *self.upstream.lock().await = None;
        } else {
            *self.upstream.lock().await = Some(UpstreamConfig {
                base_url, api_key, model_hint,
            });
        }
    }
    pub fn enable_box(&self, box_id: i64) { self.api_boxes.write().insert(box_id); }
    pub fn disable_box(&self, box_id: i64) { self.api_boxes.write().remove(&box_id); }
    pub fn is_enabled(&self, box_id: i64) -> bool {
        self.api_boxes.read().contains(&box_id)
    }
}

/// Spawn the proxy on the engine's tokio runtime. Returns when the listener
/// is bound (so callers can rely on the socket existing before they spawn
/// boxes that bind-mount it). The accept loop runs forever.
pub async fn serve(proxy: Arc<Proxy>, sock_path: PathBuf) -> std::io::Result<()> {
    if sock_path.exists() { let _ = std::fs::remove_file(&sock_path); }
    if let Some(parent) = sock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(&sock_path)?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&sock_path,
        std::fs::Permissions::from_mode(0o600));
    proxy.reload_config().await;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((conn, _)) => {
                    let proxy = proxy.clone();
                    let peer_pid = peer_pid_of(&conn);
                    let attrib_box = box_id_for_peer(peer_pid);
                    tokio::spawn(async move {
                        let io = TokioIo::new(conn);
                        let proxy2 = proxy.clone();
                        let svc = service_fn(move |req| {
                            let proxy = proxy2.clone();
                            async move { handle_inner(proxy, attrib_box, req).await }
                        });
                        let _ = http1::Builder::new()
                            .keep_alive(false)
                            .serve_connection(io, svc).await;
                    });
                }
                Err(e) => eprintln!("oaita-proxy: accept: {e}"),
            }
        }
    });
    Ok(())
}

fn peer_pid_of(conn: &tokio::net::UnixStream) -> i32 {
    use std::os::fd::AsRawFd;
    let fd = conn.as_raw_fd();
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED,
        (&mut cred as *mut libc::ucred).cast(), &mut len) };
    if ok == 0 { cred.pid } else { 0 }
}

fn ppid_of(pid: i32) -> i32 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { return 0; };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

fn box_id_for_peer(peer_pid: i32) -> i64 {
    if peer_pid <= 0 { return 0; }
    let mut pid = peer_pid;
    for _ in 0..64 {
        if let Some(b) = crate::control::api_box_for_pid(pid) {
            return b;
        }
        let pp = ppid_of(pid);
        if pp <= 1 { break; }
        pid = pp;
    }
    0
}

async fn handle_inner(proxy: Arc<Proxy>, box_id: i64, req: Request<Incoming>)
    -> Result<Response<Body>, std::convert::Infallible>
{
    // Box opt-in gate: even if a box gets the socket bound in by mistake, we
    // refuse traffic from boxes that weren't launched with `--api`. box_id=0
    // (unattributable peer) is also rejected — only known --api boxes get
    // through.
    if box_id == 0 || !proxy.is_enabled(box_id) {
        return Ok(error_resp(StatusCode::FORBIDDEN,
                             "this box was not launched with --api"));
    }
    let method = req.method().to_string();
    // Strip the box-side base path prefix (we set OPENAI_BASE_URL=…/v1 in the
    // box, so the incoming URI looks like `/v1/chat/completions`). The
    // upstream client then prepends its OWN base path from the configured
    // upstream `base_url`. Without this strip the upstream sees a doubled
    // prefix (e.g. `/api/v1/v1/chat/completions` → 404).
    let raw_path = req.uri().path().to_string();
    let path = raw_path.strip_prefix("/v1")
        .map(|s| if s.is_empty() { "/" } else { s }.to_string())
        .unwrap_or_else(|| raw_path.clone());
    let (_head, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return Ok(error_resp(StatusCode::BAD_REQUEST,
            &format!("read body: {e}"))),
    };
    let req_json: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    let model = req_json.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let is_stream = req_json.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let Some(up) = proxy.upstream.lock().await.clone() else {
        let msg = "oaita.toml not configured: set model + base_url + api_key";
        log_call(&proxy, box_id, &method, &path, &model, 503, &body_bytes,
                 msg.as_bytes(), is_stream).await;
        return Ok(error_resp(StatusCode::SERVICE_UNAVAILABLE, msg));
    };

    let client = match Client::from_resolved(&up.base_url, &up.api_key) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("proxy: upstream client: {e}");
            log_call(&proxy, box_id, &method, &path, &model, 502,
                     &body_bytes, msg.as_bytes(), is_stream).await;
            return Ok(error_resp(StatusCode::BAD_GATEWAY, &msg));
        }
    };

    if is_stream {
        Ok(proxy_stream(proxy, box_id, client, method, path, model, body_bytes).await)
    } else {
        Ok(proxy_buffered(proxy, box_id, client, method, path, model, body_bytes).await)
    }
}

fn error_resp(status: StatusCode, msg: &str) -> Response<Body> {
    let body = Bytes::from(msg.to_string().into_bytes());
    Response::builder().status(status)
        .header("Content-Type", "text/plain")
        .body(Full::new(body).map_err(|e| match e {}).boxed())
        .unwrap()
}

async fn proxy_buffered(proxy: Arc<Proxy>, box_id: i64, client: Client,
                        method: String, path: String, model: String,
                        body: Bytes) -> Response<Body>
{
    let req_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let result = client.post_raw(&path, req_json).await;
    match result {
        Ok(resp_bytes) => {
            log_call(&proxy, box_id, &method, &path, &model, 200,
                     &body, &resp_bytes, false).await;
            Response::builder().status(200)
                .header("Content-Type", "application/json")
                .body(Full::new(resp_bytes).map_err(|e| match e {}).boxed())
                .unwrap()
        }
        Err(e) => {
            let msg = format!("upstream: {e}");
            log_call(&proxy, box_id, &method, &path, &model, 502,
                     &body, msg.as_bytes(), false).await;
            error_resp(StatusCode::BAD_GATEWAY, &msg)
        }
    }
}

async fn proxy_stream(proxy: Arc<Proxy>, box_id: i64, client: Client,
                      method: String, path: String, model: String,
                      body: Bytes) -> Response<Body>
{
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, String>>();
    let proxy_clone = proxy.clone();
    let req_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let body_log = body.clone();
    let method_log = method.clone();
    let path_log = path.clone();
    let model_log = model.clone();
    tokio::spawn(async move {
        let mut collected = Vec::<u8>::new();
        let res = client.post_stream(&path, req_json, |payload| {
            let frame = format!("data: {payload}\n\n");
            collected.extend_from_slice(frame.as_bytes());
            let _ = tx.send(Ok(Bytes::from(frame)));
        }).await;
        if let Err(e) = res {
            let _ = tx.send(Err(e.clone()));
            log_call(&proxy_clone, box_id, &method_log, &path_log, &model_log,
                     502, &body_log, e.as_bytes(), true).await;
        } else {
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n")));
            log_call(&proxy_clone, box_id, &method_log, &path_log, &model_log,
                     200, &body_log, &collected, true).await;
        }
    });
    let stream = UnboundedReceiverStream::new(rx).map(|r| r.map(Frame::data));
    let body = BodyExt::boxed(StreamBody::new(stream));
    Response::builder().status(200)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(body)
        .unwrap()
}

async fn log_call(proxy: &Arc<Proxy>, box_id: i64,
                  method: &str, path: &str, model: &str, status: i32,
                  req: &[u8], resp: &[u8], stream: bool) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let ov = proxy.overlay.read().clone();
    let Some(ov) = ov else { return; };
    let Some(b) = ov.live_box(box_id) else { return; };
    b.add_api_log(ts, method, path, model, status, req, resp, stream);
    crate::control::broadcast_api_log(box_id);
}
