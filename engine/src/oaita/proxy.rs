// Engine-side API proxy. There is NO host UDS — the proxy is fed bytes by
// `oaita::proxy_mux`, which demultiplexes FRAME_API_OPEN/DATA/CLOSE off the
// existing box-channel into per-stream duplex pipes. The proxy reads HTTP
// off one end of each pipe, injects the Authorization header from
// `oaita.toml`, forwards to the configured upstream, streams the response
// back through the pipe (which the mux re-frames onto the box channel),
// and logs the (request, response, model, status) pair into the
// originating box's `api_log` sqlar table.
//
// Authentication: the box never sees the API key. The proxy gets it from
// `oaita.toml` (or env vars, same precedence as the client). Per-box opt-in
// is controlled at runner-launch time via the `--api` flag.
//
// Box attribution: intrinsic to the box-channel the FRAME_API_* stream
// rides on — proxy_mux passes the channel's box_id straight in. No
// SO_PEERCRED walk, no /proc PPid chasing.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
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

/// Serve ONE proxy stream from a known box. Used by `oaita::proxy_mux`: the
/// runner has already accepted an in-box conn and is forwarding its bytes
/// as FRAME_API_DATA on the box-channel; the mux feeds those bytes into a
/// duplex pipe whose far end is `io`. The box id is known a priori from
/// the box-channel identity — no peer-pid walk needed.
///
/// `state` is the engine state — needed for the budget chain debit. We do
/// the debit INSIDE handle_inner (after hyper has collected the request
/// body) instead of at the raw verb-dispatch level, so a 503 response
/// goes back through a fully-read request: no client write-after-close,
/// no `error writing a body to connection` on large prompts.
pub async fn serve_one_conn_for_box<IO>(proxy: Arc<Proxy>,
                                        state: crate::control::State,
                                        box_id: i64, io: IO)
    -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    IO: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    // Make sure the upstream config is loaded (idempotent on subsequent calls).
    proxy.reload_config().await;
    let proxy2 = proxy.clone();
    let svc = service_fn(move |req| {
        let proxy = proxy2.clone();
        let state = state.clone();
        async move { handle_inner(proxy, state, box_id, req).await }
    });
    let _ = http1::Builder::new()
        .keep_alive(false)
        .serve_connection(io, svc).await;
    Ok(())
}

// peer-pid → box-id walking is gone — attribution is now intrinsic to the
// box-channel the FRAME_API_* stream rides on.

async fn handle_inner(proxy: Arc<Proxy>, state: crate::control::State,
                      box_id: i64, req: Request<Incoming>)
    -> Result<Response<Body>, std::convert::Infallible>
{
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
    // Box opt-in gate: even if a box gets the socket bound in by mistake, we
    // refuse traffic from boxes that weren't launched with `--api`. box_id=0
    // (unattributable peer) is also rejected — only known --api boxes get
    // through. Log the refusal: without it a stuck/intermittent broker
    // problem looks identical to upstream silence from the model's side
    // (hyper just surfaces "send: error writing a body to connection"),
    // and the box has nothing in its api_log to explain why.
    if box_id == 0 || !proxy.is_enabled(box_id) {
        let msg = "this box was not launched with --api";
        log_call(&proxy, box_id, &method, &path, "", 403,
                 &[], msg.as_bytes(), false);
        return Ok(error_resp(StatusCode::FORBIDDEN, msg));
    }
    let (_head, body) = req.into_parts();
    // Drain the request body BEFORE the budget gate. The previous design
    // gated at the raw verb-dispatch layer (control.rs), wrote a 503 over
    // the conn, and returned — which closed the socket while the client
    // was still streaming its (potentially large) request body. hyper on
    // the client side then surfaced "send: error writing a body to
    // connection" instead of a parseable 503. By collecting the body
    // here first, the 503 we emit travels back through hyper on a fully-
    // consumed request — proper HTTP, no mid-write close.
    let body_bytes = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            let msg = format!("read body: {e}");
            log_call(&proxy, box_id, &method, &path, "", 400,
                     &[], msg.as_bytes(), false);
            return Ok(error_resp(StatusCode::BAD_REQUEST, &msg));
        }
    };
    let req_json: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    let model = req_json.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let is_stream = req_json.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // Budget chain debit. Walks this box's parent_box_id chain, debiting
    // each capped box; if any ancestor's pool hit zero we return a 503
    // here instead of forwarding. Logged with status=503 like other
    // proxy refusals so the model can see the round-trip happened.
    if let Err(top) = crate::oaita::budget::take_chain(&state, box_id) {
        let msg = format!("budget exhausted at box {top} — grant more turns and resume.");
        log_call(&proxy, box_id, &method, &path, &model, 503,
                 &body_bytes, msg.as_bytes(), is_stream);
        return Ok(error_resp(StatusCode::SERVICE_UNAVAILABLE, &msg));
    }

    let Some(up) = proxy.upstream.lock().await.clone() else {
        let msg = "oaita.toml not configured: set model + base_url + api_key";
        log_call(&proxy, box_id, &method, &path, &model, 503, &body_bytes,
                 msg.as_bytes(), is_stream);
        return Ok(error_resp(StatusCode::SERVICE_UNAVAILABLE, msg));
    };

    let client = match Client::from_resolved(&up.base_url, &up.api_key) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("proxy: upstream client: {e}");
            log_call(&proxy, box_id, &method, &path, &model, 502,
                     &body_bytes, msg.as_bytes(), is_stream);
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
                     &body, &resp_bytes, false);
            Response::builder().status(200)
                .header("Content-Type", "application/json")
                .body(Full::new(resp_bytes).map_err(|e| match e {}).boxed())
                .unwrap()
        }
        Err(e) => {
            let msg = format!("upstream: {e}");
            log_call(&proxy, box_id, &method, &path, &model, 502,
                     &body, msg.as_bytes(), false);
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
                     502, &body_log, e.as_bytes(), true);
        } else {
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n")));
            log_call(&proxy_clone, box_id, &method_log, &path_log, &model_log,
                     200, &body_log, &collected, true);
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

/// Append one api_log row for box `box_id`. Public + sync so the raw
/// verb-dispatch layer in control.rs can call it from the conn-setup
/// early-return paths (rt/proxy None, set_nonblocking failure, tokio
/// stream conversion failure) — without it, those paths silently
/// dropped the conn and the box had nothing in its api_log to explain
/// why hyper on the client side surfaced "send: error writing a body
/// to connection". No await: the parking_lot RwLock + rusqlite insert
/// are both sync.
pub fn log_call(proxy: &Proxy, box_id: i64,
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
