// TLS termination + HTTP MITM. Roll-our-own on rcgen/rustls/hyper since we
// already know the destination before the first byte (no CONNECT preamble
// from the client — smoltcp accepted on a synthetic IP we ourselves bound).
//
// Flow per HTTPS connection:
//   1. tcp::dispatcher hands us (Stream, Conn) where Conn.host is the DNS-
//      reverse-looked-up hostname.
//   2. Peek a few bytes to confirm TLS ClientHello (just for paranoia); if
//      not TLS, fall back to l4::forward.
//   3. Mint or lookup a leaf cert under the engine CA via `ca::Ca::leaf_for`.
//   4. rustls ServerConfig with the leaf + ALPN ["h2","http/1.1"], and a
//      KeyLog implementation that appends to the per-box keylog sidecar so
//      tshark can decode the pcapng.
//   5. tokio-rustls accept → hyper serve_connection with our request handler.
//   6. Handler opens an outbound rustls client to `host:port`, replays the
//      request, streams the response back. Each request is gated by the
//      policy module before the upstream is dialed; on deny we send 403
//      and close.
//
// Plain HTTP is the same but skipping steps 2-4 and dialing plaintext
// upstream.

use std::sync::Arc;

use parking_lot::Mutex;

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
