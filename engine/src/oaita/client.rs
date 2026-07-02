// Thin OpenAI-compatible HTTP/1.1 client built on hyper + hyper-util — the one
// piece both transports (real TCP/TLS upstream AND the engine's `--api` UDS
// proxy) share. The TLS leg uses `rustls` (already in deps for the MITM proxy
// half of the engine); the UDS leg uses tokio::net::UnixStream.
//
// Why not async-openai: reqwest, async-openai's HTTP layer, has no UDS
// connector — wiring a TCP↔UDS bridge inside the box just to keep the SDK
// alive is more code than this whole file. We pay ~200 LOC and get a single
// code path for both transports. Wire types stay as serde_json::Value: the
// model is whatever the configured upstream serves, so we never lock the
// schema; only the streaming SSE framing needs to be understood.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixStream};

/// Where the LLM endpoint lives. Broker: in-box dial of `SARUN_BROKER`
/// (the FD broker), the returned engine conn carries an HTTP/1.1
/// `api.proxy` stream — engine handles upstream auth + forwarding. TCP:
/// direct dial of a real http(s) base URL (host clients).
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// Per-call dial of the engine via the FD broker; the conn becomes an
    /// `api.proxy` stream the engine HTTP-proxies to the configured
    /// upstream. The string is the abstract broker UDS name from
    /// `SARUN_BROKER`.
    Broker(String),
    /// Real http(s) URL. The port defaults to 80/443 by scheme.
    Tcp { scheme: String, host: String, port: u16 },
    /// HTTP over a host UNIX socket: `unix://<abs-path>[#<base-path>]`.
    /// Used for engine-bridged in-box services (`svc.serve` — e.g. the
    /// `oaita local` llama-server box): the endpoint never touches any
    /// netns, host or box. The optional fragment carries the API base
    /// path (e.g. `#/v1`) since a socket path has no URL path of its own.
    Unix { path: String },
}

impl Endpoint {
    /// Pick the endpoint for THIS process. In-box `--api` clients have
    /// `SARUN_BROKER` in env (the FD broker's abstract UDS name) — for
    /// those we dial the broker per request and ride a fresh engine conn
    /// tagged `api.proxy`. Top-level / host clients fall through to a
    /// direct TCP dial of the configured base URL.
    pub fn from_env(base_url: &str) -> Result<Self, String> {
        if let Ok(name) = std::env::var("SARUN_BROKER") {
            if !name.is_empty() {
                return Ok(Endpoint::Broker(name));
            }
        }
        Self::parse_url(base_url)
    }

    pub fn parse_url(url: &str) -> Result<Self, String> {
        // Just the SCHEME://HOST[:PORT] head — the per-request path is added
        // at send time. We don't need a full URL parser for this.
        let (scheme, rest) = url.split_once("://")
            .ok_or_else(|| format!("base_url missing scheme: {url:?}"))?;
        if scheme.eq_ignore_ascii_case("unix") {
            let path = rest.split('#').next().unwrap_or(rest);
            if path.is_empty() {
                return Err(format!("unix base_url missing socket path: {url:?}"));
            }
            return Ok(Endpoint::Unix { path: path.to_string() });
        }
        let host_port = rest.split('/').next().unwrap_or("");
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|e| e.to_string())?),
            None => (host_port.to_string(),
                     if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 }),
        };
        Ok(Endpoint::Tcp { scheme: scheme.to_lowercase(), host, port })
    }

}

/// The path portion of a request URL: everything after the host. For TCP
/// dispatch the base_url's path prefix matters (so we honour …/v1); UDS
/// dispatch ignores it (engine routes by the path it sees).
fn base_path(url: &str) -> &str {
    // unix:// endpoints: the socket path IS the "host"; the API base path
    // rides in the fragment (unix:///run/x.sock#/v1).
    if url.len() >= 7 && url[..7].eq_ignore_ascii_case("unix://") {
        return match url.find('#') {
            Some(i) => &url[i + 1..],
            None => "/",
        };
    }
    match url.split_once("://") {
        Some((_, rest)) => {
            match rest.find('/') {
                Some(i) => &rest[i..],
                None => "/",
            }
        }
        None => "/",
    }
}

pub struct Client {
    pub endpoint: Endpoint,
    pub api_key: String,
    pub base_path: String,
    pub tls: Option<Arc<tokio_rustls::TlsConnector>>,
}

impl Client {
    pub fn new(endpoint: Endpoint, api_key: String, base_path: String) -> Self {
        let tls = match &endpoint {
            Endpoint::Tcp { scheme, .. } if scheme == "https" => {
                Some(Arc::new(default_tls_connector()))
            }
            _ => None,
        };
        Client { endpoint, api_key, base_path, tls }
    }

    pub fn from_resolved(base_url: &str, api_key: &str) -> Result<Self, String> {
        let endpoint = Endpoint::from_env(base_url)?;
        let path = base_path(base_url).to_string();
        Ok(Client::new(endpoint, api_key.to_string(), path))
    }

    /// POST the JSON body to `path` (e.g. "/chat/completions") and return
    /// the wire response — full body buffered. For streaming responses,
    /// use `post_stream`.
    pub async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let raw = self.post_raw(path, body).await?;
        serde_json::from_slice(&raw).map_err(|e| format!("response not JSON: {e}"))
    }

    pub async fn post_raw(&self, path: &str, body: Value) -> Result<Bytes, String> {
        let mut resp = self.send(path, body).await?;
        // Surface non-2xx (engine-side 503 from budget exhaustion;
        // upstream 4xx/5xx) as Err so the driver loop can react instead
        // of treating an error body as a model response.
        let status = resp.status();
        let body = resp.body_mut().collect().await
            .map(|c| c.to_bytes()).unwrap_or_default();
        if !status.is_success() {
            let preview = String::from_utf8_lossy(&body[..body.len().min(2048)]);
            return Err(format!("upstream {status}: {preview}"));
        }
        Ok(body)
    }

    /// Stream Server-Sent-Events from `path`. The callback gets each `data:`
    /// payload (without the prefix); a sentinel "[DONE]" causes us to stop.
    pub async fn post_stream<F>(&self, path: &str, mut body: Value, mut on_event: F)
        -> Result<(), String>
    where F: FnMut(&str)
    {
        // Caller sets stream=true on the body, but enforce it here too in case
        // they forgot — streaming is the whole point of this method.
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), json!(true));
        }
        let mut resp = self.send(path, body).await?;
        // Non-2xx — read the error body and surface it. Without this, an
        // OpenAI-style {"error":{"message":...}} reply has no `data:` lines,
        // the SSE loop sees nothing, gen claims success, and you get an
        // empty assistant turn with no idea why.
        let status = resp.status();
        if !status.is_success() {
            let body = resp.body_mut().collect().await
                .map(|c| c.to_bytes()).unwrap_or_default();
            let preview = String::from_utf8_lossy(&body[..body.len().min(2048)]);
            return Err(format!("upstream {status}: {preview}"));
        }
        let mut buf = Vec::<u8>::new();
        while let Some(frame) = resp.body_mut().frame().await {
            let frame = frame.map_err(|e| format!("stream frame: {e}"))?;
            let Some(data) = frame.data_ref() else { continue; };
            buf.extend_from_slice(data);
            // Split on \n\n — SSE event delimiter.
            loop {
                let Some(pos) = find_double_newline(&buf) else { break; };
                let event_bytes = buf.drain(..pos + 2).collect::<Vec<u8>>();
                let event = String::from_utf8_lossy(&event_bytes);
                for line in event.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        let payload = rest.trim_start();
                        if payload == "[DONE]" { return Ok(()); }
                        on_event(payload);
                    }
                }
            }
        }
        Ok(())
    }

    async fn send(&self, path: &str, body: Value)
        -> Result<Response<Incoming>, String>
    {
        let full_path = if path.starts_with('/') {
            format!("{}{path}", self.base_path.trim_end_matches('/'))
        } else {
            format!("{}/{path}", self.base_path.trim_end_matches('/'))
        };
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| format!("encode body: {e}"))?;
        let host_header = match &self.endpoint {
            Endpoint::Broker(_) => "oaita-proxy".to_string(),
            Endpoint::Unix { .. } => "localhost".to_string(),
            Endpoint::Tcp { host, port, scheme } => {
                let default = (scheme == "https" && *port == 443)
                           || (scheme == "http" && *port == 80);
                if default { host.clone() } else { format!("{host}:{port}") }
            }
        };
        let mut req = Request::builder()
            .method("POST")
            .uri(full_path)
            .header("Host", &host_header)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Length", body_bytes.len().to_string());
        if !self.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.api_key));
        }
        let req = req.body(Full::new(Bytes::from(body_bytes)))
            .map_err(|e| format!("build req: {e}"))?;

        match &self.endpoint {
            Endpoint::Broker(name) => {
                // For --api boxes: dial the FD broker (an abstract UDS the
                // box's inner serves) per LLM call; the broker queues us,
                // engine sends back a fresh socketpair half via SCM_RIGHTS.
                // We tag that conn `api.proxy` — the engine then HTTP-
                // proxies to the configured upstream. One conn per call;
                // no demux on the box-channel.
                let name_owned = name.clone();
                let std_stream = tokio::task::spawn_blocking(move ||
                    crate::runner::broker_dial(&name_owned))
                    .await
                    .map_err(|e| format!("broker join: {e}"))?
                    .map_err(|e| format!("broker dial: {e}"))?;
                std_stream.set_nonblocking(true)
                    .map_err(|e| format!("set_nonblocking: {e}"))?;
                let mut stream = UnixStream::from_std(std_stream)
                    .map_err(|e| format!("from_std: {e}"))?;
                // Engine derives the box id from the broker handshake
                // (FD broker hands out conns with a hint_box_id = THIS
                // box). No session field on the wire — budget identity
                // is intrinsic to the broker, not client-supplied.
                stream.write_all(b"{\"type\":\"api.proxy\"}\n").await
                    .map_err(|e| format!("api.proxy header: {e}"))?;
                let (mut sender, conn) = hyper::client::conn::http1::handshake(
                    TokioIo::new(stream)).await
                    .map_err(|e| format!("handshake: {e}"))?;
                tokio::spawn(async move { let _ = conn.await; });
                sender.send_request(req).await.map_err(|e| format!("send: {e}"))
            }
            Endpoint::Unix { path } => {
                let stream = UnixStream::connect(path).await
                    .map_err(|e| format!("dial {path}: {e}"))?;
                let (mut sender, conn) = hyper::client::conn::http1::handshake(
                    TokioIo::new(stream)).await
                    .map_err(|e| format!("handshake: {e}"))?;
                tokio::spawn(async move { let _ = conn.await; });
                sender.send_request(req).await.map_err(|e| format!("send: {e}"))
            }
            Endpoint::Tcp { scheme, host, port } => {
                let tcp = TcpStream::connect((host.as_str(), *port)).await
                    .map_err(|e| format!("dial {host}:{port}: {e}"))?;
                if scheme == "https" {
                    let tls = self.tls.as_ref().unwrap().clone();
                    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
                        .map_err(|e| format!("server name {host:?}: {e}"))?;
                    let tls_stream = tls.connect(server_name, tcp).await
                        .map_err(|e| format!("tls: {e}"))?;
                    let (mut sender, conn) = hyper::client::conn::http1::handshake(
                        TokioIo::new(tls_stream)).await
                        .map_err(|e| format!("handshake: {e}"))?;
                    tokio::spawn(async move { let _ = conn.await; });
                    sender.send_request(req).await.map_err(|e| format!("send: {e}"))
                } else {
                    let (mut sender, conn) = hyper::client::conn::http1::handshake(
                        TokioIo::new(tcp)).await
                        .map_err(|e| format!("handshake: {e}"))?;
                    tokio::spawn(async move { let _ = conn.await; });
                    sender.send_request(req).await.map_err(|e| format!("send: {e}"))
                }
            }
        }
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
        .or_else(|| buf.windows(4).position(|w| w == b"\r\n\r\n"))
}

fn default_tls_connector() -> tokio_rustls::TlsConnector {
    // rustls 0.23 fails its auto-pick when more than one rustls user is
    // linked into the binary (we have both the MITM proxy half of `-n`
    // boxes and now this client). Install `ring` explicitly the first
    // time we build a connector; subsequent calls hit the already-set
    // branch and return an error we ignore.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut root_store = rustls::RootCertStore::empty();
    // Pull the host trust store via rustls-native-certs is the usual route,
    // but we don't have it in deps; the augmented CA bundle in the box is
    // exposed via $SSL_CERT_FILE — read it if set. Otherwise webpki-roots
    // would be the choice, but to avoid a new dep we just fall back to
    // /etc/ssl/certs/ca-certificates.crt (the canonical Debian/Ubuntu path
    // which sarun's CA augmentation also targets — see runner.rs).
    let pem_path = std::env::var("SSL_CERT_FILE")
        .unwrap_or_else(|_| "/etc/ssl/certs/ca-certificates.crt".to_string());
    if let Ok(pem) = std::fs::read(&pem_path) {
        let mut cursor = &pem[..];
        let mut count = 0usize;
        loop {
            match rustls_pemfile::read_one(&mut cursor) {
                Ok(Some(rustls_pemfile::Item::X509Certificate(der))) => {
                    if root_store.add(der).is_ok() { count += 1; }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        if count == 0 {
            eprintln!("oaita: warning — no CA certs loaded from {pem_path}");
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// One-shot helper for callers that just want to dial and stream.
pub async fn stream_text(client: &Client, path: &str, body: Value,
                         mut on_delta: impl FnMut(&str))
    -> Result<(), String>
{
    client.post_stream(path, body, |payload| {
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return; };
        if let Some(choices) = v.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(d) = choice.get("delta").and_then(Value::as_object) {
                    if let Some(content) = d.get("content").and_then(Value::as_str) {
                        on_delta(content);
                    }
                }
            }
        }
    }).await
}

/// Async tokio runtime accessor — oaita CLI commands spin a current-thread
/// runtime; the engine proxy uses the existing multi-thread one.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().expect("tokio runtime");
    rt.block_on(f)
}

/// Best-effort: send `data` then close — used by the proxy when proxying a
/// 4xx error response straight through.
pub async fn write_and_close<W: AsyncWriteExt + Unpin>(w: &mut W, data: &[u8]) {
    let _ = w.write_all(data).await;
    let _ = w.shutdown().await;
}
