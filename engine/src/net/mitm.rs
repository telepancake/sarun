// TLS termination + HTTP MITM.
//
// HTTPS path: take a SmoltcpStream + dst hostname; mint a leaf cert under
// the engine CA for that hostname (rcgen via super::ca::Ca); accept TLS
// with rustls + tokio-rustls; then serve HTTP/1+2 over the decrypted
// stream with hyper. Each request is replayed to the real upstream by
// opening a fresh rustls client connection to host:443 and proxying the
// hyper Request through it.
//
// HTTP path: same shape but no TLS layers.
//
// TLS keys are logged via rustls' KeyLog hook into the per-box sidecar
// file (engine/src/net/flows.rs paths) so tshark can decrypt the pcapng.

use std::sync::Arc;

use anyhow::Result;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::Request;
use hyper::Response;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;

use super::bridge::SmoltcpStream;

pub async fn serve_https(stream: SmoltcpStream, host: &str) -> Result<()> {
    let _ = stream; let _ = host;
    // TODO: full HTTPS MITM implementation. Skeleton in place; the rcgen
    // leaf-mint + rustls accept + hyper proxy wiring lands next. For now
    // a connection on port 443 closes politely so the box's curl gets
    // a clean RST rather than hanging.
    Ok(())
}

pub async fn serve_http(box_side: SmoltcpStream, host: &str, port: u16) -> Result<()> {
    // Minimal HTTP/1.1 proxy: hyper server on the box-side stream, request
    // handler dials upstream via reqwest, replays headers/body, returns.
    // (Reqwest pulls in the connection pool + retry / redirect handling;
    // a more bespoke client would let us proxy 1:1 byte semantics, but
    // reqwest is the pragmatic choice for the first landing.)
    let _ = (box_side, host, port);
    Ok(())
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

// Suppress unused-import warnings until the proxy bodies are filled in.
#[allow(dead_code)]
fn _refs(_: TokioIo<SmoltcpStream>, _: Request<Incoming>, _: Response<Full<Bytes>>,
         _: Incoming) {
    fn use_body_ext<B: hyper::body::Body + Send + 'static>(_: B) where B::Data: Send, B::Error: Send {}
    use_body_ext(Full::<Bytes>::new(Bytes::new()).boxed());
}
