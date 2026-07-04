// TLS termination + HTTP MITM.
//
// The proxy core (`proxy_request`) is shared between HTTP and HTTPS. It
// takes a hyper::Request (already headers-parsed), pulls the Host (and
// the scheme it should dial upstream with), opens a fresh upstream
// connection from the host netns, replays the request, and returns the
// upstream's response. Headers are forwarded verbatim except `Host:`
// which is rewritten to the upstream authority.
//
// For HTTPS the upstream is opened via tokio-rustls + a webpki roots
// trust store (we trust the real internet just like a browser would).
// The leaf cert we mint to talk to the box is generated on demand from
// the engine CA and cached by SNI.
//
// Both paths share one `KeyLogFile` per box, so a single tshark
// `tls.keylog_file` decodes the whole pcapng.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use http_body_util::{BodyExt, Empty, Full, StreamBody, combinators::BoxBody};
use hyper::body::Bytes;
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use tokio::net::TcpStream;
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::bridge::SmoltcpStream;
use super::ca::Ca;
use super::filter::Decision;
use super::webcap::{self, ReqCap};
use super::ProxyHooks;

/// Hyper body type the proxy emits — boxed dyn so HTTP and HTTPS paths
/// share the same Response signature regardless of the upstream body.
type ProxyBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new()
        .map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} })
        .boxed()
}

fn err_response(status: hyper::StatusCode, msg: &str) -> Response<ProxyBody> {
    Response::builder().status(status)
        .body(Empty::<Bytes>::new()
              .map_err(|never| -> Box<dyn std::error::Error + Send + Sync> { match never {} })
              .boxed())
        .unwrap_or_else(|_| {
            // Building an empty-body response is infallible in practice; this
            // arm only exists to satisfy the Result. `msg` is purely advisory
            // and intentionally unused on this fallback path.
            let _ = msg;
            let mut r = Response::new(empty_body());
            *r.status_mut() = status;
            r
        })
}

/// Common request handler. `scheme` is "http" or "https"; for "https" the
/// upstream is opened via tokio-rustls. `port` is the default port (80/443)
/// or whatever the box dialed. `hooks` (DESIGN-web.md W2/W7) are the per-box
/// proxy hooks: `None` runs the original pure pass-through with zero added
/// cost; otherwise the filter can block the request (synthetic 204) or rewrite
/// response headers, and the capture sink tees the exchange into a `webcap`
/// row while streaming the body to the box unchanged.
async fn proxy_request(scheme: &'static str, default_port: u16,
                       req: Request<Incoming>,
                       rustls_client_config: Option<Arc<rustls::ClientConfig>>,
                       hooks: Option<Arc<ProxyHooks>>)
                       -> Result<Response<ProxyBody>>
{
    let sink = hooks.as_ref().and_then(|h| h.capture.clone());
    let filter = hooks.as_ref().and_then(|h| h.filter.clone());
    // Recover the target authority from the Host header (HTTP) or the
    // request URI's authority (HTTP/2-style absolute URI).
    let host = req.headers().get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()))
        .ok_or_else(|| anyhow!("no Host"))?;
    let (host_only, port) = match host.rsplit_once(':') {
        // A non-numeric port suffix (or none) falls back to the scheme default
        // — a deliberate, correct lenient parse, not a swallowed error.
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(default_port)),
        None => (host.clone(), default_port),
    };

    // Split off the request parts BEFORE the forward consumes the body, so we
    // can record method/url/headers (and, when capturing, the body). The
    // outgoing body is boxed to a uniform type so both the streamed
    // (capture-off) and buffered (capture-on) shapes drive the same sender.
    let (parts, in_body) = req.into_parts();
    let pq = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{scheme}://{host}{pq}");

    // ── W7 request-block (adblock) ────────────────────────────────────────
    // Decide BEFORE dialing upstream. A blocked request never leaves the
    // engine; the box gets a synthetic 204. Still recorded (status 204,
    // blocked marker) so the archive shows what was filtered.
    if let Some(f) = &filter {
        if f.decide(&url, &host) == Decision::Block {
            if let Some(sink) = &sink {
                let ts = SystemTime::now().duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs_f64()).unwrap_or(0.0);
                let rc = ReqCap {
                    method: parts.method.to_string(), url: url.clone(),
                    host: host.clone(),
                    headers: webcap::format_headers(&parts.headers),
                    body: Vec::new(),
                };
                sink.record(ts, &rc, 204, "", "x-sarun-filter: blocked\n", &[], false);
            }
            return Ok(Response::builder()
                .status(hyper::StatusCode::NO_CONTENT)
                .header("x-sarun-filter", "blocked")
                .body(empty_body()).unwrap());
        }
    }

    let (out_body, reqcap): (ProxyBody, Option<ReqCap>) = if sink.is_some() {
        let mut rc = ReqCap {
            method: parts.method.to_string(),
            url: url.clone(),
            host: host.clone(),
            headers: webcap::format_headers(&parts.headers),
            body: Vec::new(),
        };
        if webcap::length_within_cap(&parts.headers) {
            // Small body (browse/crawl requests are tiny or absent); collect
            // it, cap-guarded, and forward a buffered copy.
            let collected = in_body.collect().await
                .map(|c| c.to_bytes()).unwrap_or_default();
            let bytes = if collected.len() > webcap::WEBCAP_BODY_MAX {
                collected.slice(0..webcap::WEBCAP_BODY_MAX)
            } else { collected };
            rc.body = bytes.to_vec();
            (Full::new(bytes).map_err(|n| match n {}).boxed(), Some(rc))
        } else {
            // Oversized declared upload: stream through untouched, record the
            // request header-only.
            (in_body.map_err(|e| Box::new(e) as _).boxed(), Some(rc))
        }
    } else {
        (in_body.map_err(|e| Box::new(e) as _).boxed(), None)
    };
    let out_req = Request::from_parts(parts, out_body);

    let tcp = TcpStream::connect((host_only.as_str(), port)).await
        .with_context(|| format!("dial {host_only}:{port}"))?;

    let resp: Response<ProxyBody> = if scheme == "https" {
        let cfg = rustls_client_config.ok_or_else(|| anyhow!("no client config"))?;
        let connector = tokio_rustls::TlsConnector::from(cfg);
        let dnsname = rustls::pki_types::ServerName::try_from(host_only.clone())
            .map_err(|e| anyhow!("bad dns name: {e}"))?;
        let tls = connector.connect(dnsname, tcp).await
            .with_context(|| format!("tls handshake {host_only}"))?;
        let io = TokioIo::new(tls);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        // Drive the upstream HTTP/1 connection. An error here (upstream reset
        // mid-response, etc.) is a real fault behind a body that may already
        // be streaming to the box — log it instead of vanishing.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("sarun-engine: net: mitm upstream https conn: {e}");
            }
        });
        let resp = sender.send_request(out_req).await?;
        resp.map(|b| b.map_err(|e| Box::new(e) as _).boxed())
    } else {
        let io = TokioIo::new(tcp);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("sarun-engine: net: mitm upstream http conn: {e}");
            }
        });
        let resp = sender.send_request(out_req).await?;
        resp.map(|b| b.map_err(|e| Box::new(e) as _).boxed())
    };
    let mut resp = resp;

    // ── W7 response header rewrite ────────────────────────────────────────
    // Strip configured headers (CSP/X-Frame-Options for rendering, tracking
    // headers) before the box OR the capture sink sees them, so both the live
    // view and the archive reflect the filtered response.
    if let Some(f) = &filter {
        f.rewrite_response_headers(resp.headers_mut());
    }

    // Capturing path: tee the response body into a webcap row while it streams
    // to the box. Non-capturing path returns the response untouched.
    match (sink, reqcap) {
        (Some(sink), Some(req)) => Ok(tee_response(sink, req, resp)),
        _ => Ok(resp),
    }
}

/// Wrap the upstream response so each body frame is forwarded to the box
/// immediately AND copied into a capped accumulator; when the stream ends the
/// completed exchange is recorded as one `webcap` row (DESIGN-web.md W2). The
/// forward is byte-for-byte — the box sees exactly what upstream sent — so
/// this never changes what the box receives, only records a copy.
fn tee_response(sink: Arc<webcap::WebCapSink>, req: ReqCap,
                resp: Response<ProxyBody>) -> Response<ProxyBody> {
    let (parts, mut body) = resp.into_parts();
    let status = parts.status.as_u16() as i32;
    let resp_headers = webcap::format_headers(&parts.headers);
    let mime = webcap::mime_of(&parts.headers);
    // A declared over-cap length means don't bother accumulating (we'd only
    // truncate); stream through and record header-only.
    let mut capturing = webcap::length_within_cap(&parts.headers);
    let over_cap_declared = !capturing;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
        Result<Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>>>();
    tokio::spawn(async move {
        let mut acc: Vec<u8> = Vec::new();
        let mut truncated = over_cap_declared;
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        if capturing {
                            let room = webcap::WEBCAP_BODY_MAX.saturating_sub(acc.len());
                            if data.len() > room {
                                acc.extend_from_slice(&data[..room]);
                                truncated = true;
                                capturing = false;
                            } else {
                                acc.extend_from_slice(data);
                            }
                        } else {
                            truncated = true;
                        }
                    }
                    if tx.send(Ok(frame)).is_err() { break; }
                }
                Some(Err(e)) => { let _ = tx.send(Err(e)); break; }
                None => break,
            }
        }
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        sink.record(ts, &req, status, &mime, &resp_headers, &acc, truncated);
    });
    let stream = UnboundedReceiverStream::new(rx);
    // Disambiguate: both BodyExt and StreamExt provide `boxed` in scope.
    let new_body = BodyExt::boxed(StreamBody::new(stream));
    Response::from_parts(parts, new_body)
}

pub async fn serve_http(box_side: SmoltcpStream, _host_hint: &str,
                        port: u16, hooks: Option<Arc<ProxyHooks>>) -> Result<()> {
    let io = TokioIo::new(box_side);
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let hooks = hooks.clone();
        async move {
            match proxy_request("http", port, req, None, hooks).await {
                Ok(r) => Ok::<_, std::convert::Infallible>(r),
                Err(e) => {
                    eprintln!("sarun-net: http proxy: {e}");
                    Ok(err_response(hyper::StatusCode::BAD_GATEWAY, &e.to_string()))
                }
            }
        }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, svc);
    // A box-side connection error (client hangup mid-request, framing error)
    // is surfaced rather than swallowed so a broken proxy session is visible.
    if let Err(e) = conn.await {
        eprintln!("sarun-engine: net: mitm box-side http conn: {e}");
    }
    Ok(())
}

pub async fn serve_https(box_side: SmoltcpStream, host: &str,
                         ca: Arc<Ca>, keylog: Arc<KeyLogFile>,
                         upstream: Arc<rustls::ClientConfig>,
                         hooks: Option<Arc<ProxyHooks>>) -> Result<()> {
    let leaf = ca.leaf_for(host).context("mint leaf")?;
    let cert_chain: Vec<rustls::pki_types::CertificateDer> = vec![
        rustls::pki_types::CertificateDer::from(leaf.cert_der.clone()),
        rustls::pki_types::CertificateDer::from(ca.cert_der.clone()),
    ];
    let key = rustls::pki_types::PrivateKeyDer::try_from(leaf.key_der.clone())
        .map_err(|e| anyhow!("leaf key: {e}"))?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .context("server cfg")?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg.key_log = keylog.clone();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let tls = acceptor.accept(box_side).await.context("box-side tls")?;
    let io = TokioIo::new(tls);
    let upstream_cfg = upstream.clone();
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let cfg = upstream_cfg.clone();
        let hooks = hooks.clone();
        async move {
            match proxy_request("https", 443, req, Some(cfg), hooks).await {
                Ok(r) => Ok::<_, std::convert::Infallible>(r),
                Err(e) => {
                    eprintln!("sarun-net: https proxy: {e}");
                    Ok(err_response(hyper::StatusCode::BAD_GATEWAY, &e.to_string()))
                }
            }
        }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, svc);
    // Same as serve_http: surface a box-side TLS/HTTP connection error.
    if let Err(e) = conn.await {
        eprintln!("sarun-engine: net: mitm box-side https conn: {e}");
    }
    Ok(())
}

/// Per-box rustls ClientConfig pointed at the system trust store. One per
/// engine is enough — the upstream world's trust roots don't change per-box.
pub fn build_upstream_client_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    // Trust the OS's CA bundle wherever rustls-native-certs would have
    // found it — but we don't depend on that crate; instead we load the
    // most common paths ourselves. Failure → empty store; HTTPS dials will
    // get UnknownIssuer until the user provides one. Pragmatic, not perfect.
    let mut added = 0usize;
    let mut rejected = 0usize;
    for p in &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
    ] {
        if let Ok(pem) = std::fs::read(p) {
            let mut s = pem.as_slice();
            for c in rustls_pemfile_certs(&mut s) {
                // A single malformed cert in the bundle shouldn't abort the
                // load, but tally rejects so a silently-thinned trust store
                // (→ UnknownIssuer for some upstreams) is diagnosable.
                if roots.add(c).is_ok() { added += 1; } else { rejected += 1; }
            }
            break;
        }
    }
    if rejected > 0 {
        eprintln!("sarun-engine: net: {rejected} upstream CA cert(s) rejected \
                   while loading trust store");
    }
    if added == 0 {
        // No roots → every HTTPS upstream dial fails UnknownIssuer. This is a
        // serious, otherwise-mysterious condition; make it loud.
        eprintln!("sarun-engine: net: WARNING no upstream CA roots loaded — \
                   HTTPS MITM upstream dials will fail to verify");
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

/// Minimal PEM cert iterator (no extra crate). Parses every BEGIN/END CERT
/// block; ignores anything else.
fn rustls_pemfile_certs(input: &mut &[u8])
    -> Vec<rustls::pki_types::CertificateDer<'static>>
{
    // PEM is ASCII; a non-UTF8 bundle is malformed and yields no certs. The
    // caller (build_upstream_client_config) already warns loudly when zero
    // roots end up loaded, so the empty fallback here is safe.
    let s = std::str::from_utf8(input).unwrap_or("");
    let mut out = vec![];
    let mut acc = String::new();
    let mut in_block = false;
    for line in s.lines() {
        if line.starts_with("-----BEGIN CERTIFICATE") {
            in_block = true; acc.clear(); continue;
        }
        if line.starts_with("-----END CERTIFICATE") {
            in_block = false;
            use base64::Engine;
            if let Ok(der) = base64::engine::general_purpose::STANDARD
                .decode(acc.as_bytes()) {
                out.push(rustls::pki_types::CertificateDer::from(der));
            }
            continue;
        }
        if in_block { acc.push_str(line.trim()); }
    }
    out
}

#[derive(Debug)]
pub struct KeyLogFile {
    file: Mutex<std::fs::File>,
}

impl KeyLogFile {
    pub fn new(path: &std::path::Path) -> std::io::Result<Arc<Self>> {
        let f = std::fs::OpenOptions::new()
            .create(true).append(true).open(path)?;
        Ok(Arc::new(Self { file: Mutex::new(f) }))
    }
}

impl rustls::KeyLog for KeyLogFile {
    fn log(&self, label: &str, client_random: &[u8], secret: &[u8]) {
        use std::io::Write;
        let cr = hex(client_random);
        let s = hex(secret);
        // A keylog write failure means this connection won't decrypt in the
        // flows pane later; surface it (called a handful of times per conn, not
        // per packet, so this won't flood).
        if let Err(e) = writeln!(self.file.lock(), "{label} {cr} {s}") {
            eprintln!("sarun-engine: net: keylog write: {e}");
        }
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(char::from_digit((x >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((x & 0xf) as u32, 16).unwrap());
    }
    s
}
