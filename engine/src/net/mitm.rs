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

use anyhow::{Context, Result, anyhow};
use http_body_util::{BodyExt, Empty, combinators::BoxBody};
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use tokio::net::TcpStream;

use super::bridge::SmoltcpStream;
use super::ca::Ca;

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
            let _ = msg;
            let mut r = Response::new(empty_body());
            *r.status_mut() = status;
            r
        })
}

/// Common request handler. `scheme` is "http" or "https"; for "https" the
/// upstream is opened via tokio-rustls. `port` is the default port (80/443)
/// or whatever the box dialed.
async fn proxy_request(scheme: &'static str, default_port: u16,
                       req: Request<Incoming>,
                       rustls_client_config: Option<Arc<rustls::ClientConfig>>)
                       -> Result<Response<ProxyBody>>
{
    // Recover the target authority from the Host header (HTTP) or the
    // request URI's authority (HTTP/2-style absolute URI).
    let host = req.headers().get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()))
        .ok_or_else(|| anyhow!("no Host"))?;
    let (host_only, port) = match host.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(default_port)),
        None => (host.clone(), default_port),
    };

    let tcp = TcpStream::connect((host_only.as_str(), port)).await
        .with_context(|| format!("dial {host_only}:{port}"))?;

    if scheme == "https" {
        let cfg = rustls_client_config.ok_or_else(|| anyhow!("no client config"))?;
        let connector = tokio_rustls::TlsConnector::from(cfg);
        let dnsname = rustls::pki_types::ServerName::try_from(host_only.clone())
            .map_err(|e| anyhow!("bad dns name: {e}"))?;
        let tls = connector.connect(dnsname, tcp).await
            .with_context(|| format!("tls handshake {host_only}"))?;
        let io = TokioIo::new(tls);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(async move { let _ = conn.await; });
        let resp = sender.send_request(req).await?;
        Ok(resp.map(|b| b.map_err(|e| Box::new(e) as _).boxed()))
    } else {
        let io = TokioIo::new(tcp);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(async move { let _ = conn.await; });
        let resp = sender.send_request(req).await?;
        Ok(resp.map(|b| b.map_err(|e| Box::new(e) as _).boxed()))
    }
}

pub async fn serve_http(box_side: SmoltcpStream, _host_hint: &str,
                        port: u16) -> Result<()> {
    let io = TokioIo::new(box_side);
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| async move {
        match proxy_request("http", port, req, None).await {
            Ok(r) => Ok::<_, std::convert::Infallible>(r),
            Err(e) => {
                eprintln!("sarun-net: http proxy: {e}");
                Ok(err_response(hyper::StatusCode::BAD_GATEWAY, &e.to_string()))
            }
        }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, svc);
    let _ = conn.await;
    Ok(())
}

pub async fn serve_https(box_side: SmoltcpStream, host: &str,
                         ca: Arc<Ca>, keylog: Arc<KeyLogFile>,
                         upstream: Arc<rustls::ClientConfig>) -> Result<()> {
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
        async move {
            match proxy_request("https", 443, req, Some(cfg)).await {
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
    let _ = conn.await;
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
    for p in &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
    ] {
        if let Ok(pem) = std::fs::read(p) {
            let mut s = pem.as_slice();
            for c in rustls_pemfile_certs(&mut s) {
                let _ = roots.add(c);
            }
            break;
        }
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
        let _ = writeln!(self.file.lock(), "{label} {cr} {s}");
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
