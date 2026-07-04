// Web capture (DESIGN-web.md W1/W2). The tap MITM proxy already terminates
// every box HTTP(S) flow in the clear; this module is the sink that tees the
// decoded request/response pair into the originating box's `webcap` sqlar
// table, plus the pure helpers the tee needs (header formatting, MIME
// extraction, content-decode for readers, the body cap).
//
// The sink follows `oaita/proxy.rs::log_call` exactly: hold the Overlay + the
// box id, resolve `live_box(box_id)` at record time, call the BoxState
// insert. Attribution is intrinsic (the dispatcher knows whose box it is);
// no peer-pid walk. A stale box (already dissolved) drops the row silently.

use std::sync::Arc;

use hyper::HeaderMap;

/// Max bytes of a single request/response body kept inline. Bodies over the
/// cap are streamed to the box in full but recorded header-only with the
/// truncation marker set — interactive pages and typical crawl payloads
/// (HTML/CSS/JS/JSON/images) fit; large media is noted, not stored. Same
/// frugality the oaita result budget applies to tool output.
pub const WEBCAP_BODY_MAX: usize = 8 * 1024 * 1024;

/// The request half, captured before the request is forwarded upstream (the
/// forward consumes it). Carried into the response tee so one `webcap` row
/// holds the whole exchange.
#[derive(Clone)]
pub struct ReqCap {
    pub method: String,
    pub url: String,
    pub host: String,
    pub headers: String,
    pub body: Vec<u8>,
}

/// Per-box capture sink. Cheap to clone (Arc-shared). `None` at the call site
/// means capture is off for this box — the proxy then runs its original pure
/// pass-through with zero added cost.
#[derive(Clone)]
pub struct WebCapSink {
    overlay: crate::overlay::Overlay,
    box_id: i64,
}

impl WebCapSink {
    pub fn new(overlay: crate::overlay::Overlay, box_id: i64) -> Arc<Self> {
        Arc::new(Self { overlay, box_id })
    }

    /// Insert one `webcap` row for the completed exchange. Sync (rusqlite +
    /// the RAM mirror lock are both sync), best-effort — mirrors
    /// `oaita::proxy::log_call`.
    #[allow(clippy::too_many_arguments)]
    pub fn record(&self, ts: f64, req: &ReqCap, status: i32, mime: &str,
                  resp_headers: &str, resp_body: &[u8], truncated: bool) {
        let Some(b) = self.overlay.live_box(self.box_id) else { return; };
        b.add_web_capture(ts, &req.method, &req.url, &req.host, status, mime,
                          &req.headers, resp_headers, &req.body, resp_body,
                          truncated);
        crate::control::broadcast_webcap(self.box_id);
    }
}

/// Format a header map as a canonical "K: V\n" block (values lossily UTF-8;
/// header values are almost always ASCII). Deterministic order = insertion
/// order, which for hyper is receive order — faithful to the wire.
pub fn format_headers(h: &HeaderMap) -> String {
    let mut s = String::new();
    for (k, v) in h.iter() {
        s.push_str(k.as_str());
        s.push_str(": ");
        s.push_str(&String::from_utf8_lossy(v.as_bytes()));
        s.push('\n');
    }
    s
}

/// Response Content-Type without parameters ("text/html; charset=utf-8" →
/// "text/html"), lowercased. Empty when absent. Used by the viewer to route
/// rendering and by WACZ export to classify records.
pub fn mime_of(h: &HeaderMap) -> String {
    h.get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Whether the request/response body is worth capturing given a declared
/// Content-Length. `None` length (chunked/streamed) → capture and let the tee
/// enforce the byte cap; a declared length over the cap → skip inline capture
/// (stream through untouched, record header-only).
pub fn length_within_cap(h: &HeaderMap) -> bool {
    match h.get(hyper::header::CONTENT_LENGTH)
             .and_then(|v| v.to_str().ok())
             .and_then(|s| s.parse::<usize>().ok()) {
        Some(n) => n <= WEBCAP_BODY_MAX,
        None => true,
    }
}

/// Decode a stored (raw) body to its identity payload using the recorded
/// Content-Encoding, for readers (inspection, WACZ export). identity/gzip/
/// deflate/zstd are decoded from crates already in the tree; an unknown or
/// absent encoding (incl. brotli, which has no pure-Rust decoder vendored)
/// returns the bytes unchanged. `resp_headers` is the stored "K: V\n" block.
pub fn decode_body(resp_headers: &str, body: &[u8]) -> Vec<u8> {
    let enc = header_value(resp_headers, "content-encoding")
        .unwrap_or_default().to_ascii_lowercase();
    let enc = enc.trim();
    match enc {
        "" | "identity" => body.to_vec(),
        "gzip" | "x-gzip" => {
            use std::io::Read;
            let mut out = Vec::new();
            let mut d = flate2::read::GzDecoder::new(body);
            if d.read_to_end(&mut out).is_ok() { out } else { body.to_vec() }
        }
        "deflate" => {
            use std::io::Read;
            // HTTP "deflate" is ambiguous: some servers send zlib-wrapped,
            // some raw. Try zlib first, fall back to raw, then to the bytes.
            let mut out = Vec::new();
            let mut z = flate2::read::ZlibDecoder::new(body);
            if z.read_to_end(&mut out).is_ok() { return out; }
            out.clear();
            let mut r = flate2::read::DeflateDecoder::new(body);
            if r.read_to_end(&mut out).is_ok() { out } else { body.to_vec() }
        }
        "zstd" => {
            match ruzstd::StreamingDecoder::new(body) {
                Ok(mut dec) => {
                    use std::io::Read;
                    let mut out = Vec::new();
                    if dec.read_to_end(&mut out).is_ok() { out } else { body.to_vec() }
                }
                Err(_) => body.to_vec(),
            }
        }
        // brotli ("br") and anything else: stored-and-noted, returned raw.
        _ => body.to_vec(),
    }
}

/// Look up a header value in a stored "K: V\n" block, case-insensitive on the
/// key. Returns the first match's trimmed value.
pub fn header_value(headers: &str, key: &str) -> Option<String> {
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let h = "Content-Type: text/html\nContent-Encoding: gzip\n";
        assert_eq!(header_value(h, "content-type").as_deref(), Some("text/html"));
        assert_eq!(header_value(h, "CONTENT-ENCODING").as_deref(), Some("gzip"));
        assert_eq!(header_value(h, "missing"), None);
    }

    #[test]
    fn identity_and_unknown_pass_through() {
        assert_eq!(decode_body("", b"hello"), b"hello");
        assert_eq!(decode_body("Content-Encoding: br\n", b"\x01\x02"), b"\x01\x02");
    }

    #[test]
    fn gzip_roundtrips_through_decode() {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(),
                                                  flate2::Compression::default());
        e.write_all(b"the quick brown fox").unwrap();
        let gz = e.finish().unwrap();
        assert_eq!(decode_body("Content-Encoding: gzip\n", &gz), b"the quick brown fox");
    }

    #[test]
    fn deflate_zlib_roundtrips() {
        use std::io::Write;
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(),
                                                    flate2::Compression::default());
        e.write_all(b"payload").unwrap();
        let z = e.finish().unwrap();
        assert_eq!(decode_body("Content-Encoding: deflate\n", &z), b"payload");
    }
}
