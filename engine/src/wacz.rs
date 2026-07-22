// WACZ export/import (DESIGN-web.md W6) — the interop boundary for the web
// archive. Replay itself is native (net/mitm.rs W4.2, no rewriting needed
// because sarun owns the network), but the ARCHIVE FORMAT is the standard:
// export writes spec WACZ 1.1.1 so captures open in ReplayWeb.page / pywb, and
// import reads WACZ back into a replayable box. We conform to the format; we
// don't reimplement the replay engines.
//
//   sarun web export-wacz <box> <out.wacz>   webcap → WACZ
//   sarun web import-wacz  <in.wacz> [NAME]   WACZ → a new box's webcap
//
// A WACZ is a ZIP of: archive/data.warc (WARC 1.1 response records, stored
// uncompressed so CDXJ offsets are plain), indexes/index.cdx (CDXJ, SURT-
// sorted), pages/pages.jsonl (the HTML seeds), datapackage.json (frictionless
// resource list + sha256s) and datapackage-digest.json.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

// ── small helpers (no chrono / uuid deps) ───────────────────────────────────

fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// UNIX seconds → UTC (y, mo, d, h, mi, s) via Howard Hinnant's civil-from-days.
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32, h as u32, mi as u32, s as u32)
}

/// WARC-Date form: `YYYY-MM-DDThh:mm:ssZ`.
fn iso8601(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// CDXJ / page timestamp: `YYYYMMDDhhmmss`.
fn ts14(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}")
}

/// A deterministic UUID-shaped WARC-Record-ID from a digest (16 bytes, with
/// the version-4 + variant bits set) — avoids a uuid dep and any randomness.
fn uuid_from(bytes: &[u8]) -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&bytes[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

/// SURT-canonicalize a URL for the CDXJ sort key: `scheme://host/path?q` →
/// `host-labels-reversed,)/path?q`. A pragmatic subset (no userinfo, keeps
/// port + www) — enough for the CDXJ ordering readers expect.
fn surt(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    };
    let mut labels: Vec<&str> = host.split('.').collect();
    labels.reverse();
    let mut key = labels.join(",").to_ascii_lowercase();
    if let Some(p) = port {
        key.push(':');
        key.push_str(p);
    }
    key.push(')');
    key.push_str(path);
    key
}

// ── one capture row, read from webcap ───────────────────────────────────────

struct Row {
    ts: f64,
    _method: String,
    url: String,
    status: i32,
    mime: String,
    resp_headers: String,
    resp_body: Vec<u8>,
}

fn read_rows(box_id: i64) -> anyhow::Result<Vec<Row>> {
    let db = crate::paths::state_home().join(format!("{box_id}.sqlar"));
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut st = conn.prepare(
        "SELECT ts,method,url,status,mime,resp_headers,resp_body \
         FROM webcap ORDER BY ts",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(Row {
                ts: r.get(0)?,
                _method: r.get(1)?,
                url: r.get(2)?,
                status: r.get(3)?,
                mime: r.get(4)?,
                resp_headers: r.get(5)?,
                resp_body: r.get(6)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// The HTTP response block a `response` WARC record wraps: status line +
/// headers (CRLF) + blank line + body. Reproduces what the box received.
fn http_response_block(row: &Row) -> Vec<u8> {
    let reason = reason_phrase(row.status);
    let mut b = format!("HTTP/1.1 {} {}\r\n", row.status, reason).into_bytes();
    for line in row.resp_headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            b.extend_from_slice(format!("{}: {}\r\n", k.trim(), v.trim()).as_bytes());
        }
    }
    b.extend_from_slice(b"\r\n");
    b.extend_from_slice(&row.resp_body);
    b
}

fn reason_phrase(status: i32) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

// ── export ──────────────────────────────────────────────────────────────────

struct CdxEntry {
    surt: String,
    ts: String,
    json: String,
}

pub fn export(box_id: i64, out: &Path) -> anyhow::Result<usize> {
    let rows = read_rows(box_id)?;
    let mut warc: Vec<u8> = Vec::new();
    let mut cdx: Vec<CdxEntry> = Vec::new();
    let mut pages: Vec<String> = Vec::new();

    // warcinfo record first (conventional; not indexed).
    write_warcinfo(&mut warc);

    for row in &rows {
        let secs = row.ts as i64;
        let block = http_response_block(row);
        let payload_digest = format!("sha256:{}", sha256_hex(&row.resp_body));
        let block_digest = format!("sha256:{}", sha256_hex(&block));
        let rec_id = format!(
            "urn:uuid:{}",
            uuid_from(&Sha256::digest(block_digest.as_bytes()))
        );
        let header = format!(
            "WARC/1.1\r\n\
             WARC-Type: response\r\n\
             WARC-Record-ID: <{rec_id}>\r\n\
             WARC-Date: {date}\r\n\
             WARC-Target-URI: {url}\r\n\
             WARC-Payload-Digest: {payload_digest}\r\n\
             WARC-Block-Digest: {block_digest}\r\n\
             Content-Type: application/http; msgtype=response\r\n\
             Content-Length: {len}\r\n\r\n",
            date = iso8601(secs),
            url = row.url,
            len = block.len()
        );
        let offset = warc.len();
        warc.extend_from_slice(header.as_bytes());
        warc.extend_from_slice(&block);
        warc.extend_from_slice(b"\r\n\r\n");
        let record_len = warc.len() - offset;

        cdx.push(CdxEntry {
            surt: surt(&row.url),
            ts: ts14(secs),
            json: serde_json::json!({
                "url": row.url, "mime": row.mime,
                "status": row.status.to_string(),
                "digest": payload_digest, "length": record_len,
                "offset": offset, "filename": "data.warc",
            })
            .to_string(),
        });
        if row.mime.starts_with("text/html") {
            pages.push(
                serde_json::json!({
                    "id": uuid_from(&Sha256::digest(row.url.as_bytes())),
                    "url": row.url, "ts": iso8601(secs),
                })
                .to_string(),
            );
        }
    }

    // CDXJ: SURT-sorted `<surt> <ts> <json>` lines.
    cdx.sort_by(|a, b| (a.surt.as_str(), a.ts.as_str()).cmp(&(b.surt.as_str(), b.ts.as_str())));
    let cdxj: String = cdx
        .iter()
        .map(|e| format!("{} {} {}\n", e.surt, e.ts, e.json))
        .collect();

    // pages.jsonl: a header line then one page per HTML seed.
    let mut pages_jsonl = serde_json::json!(
        {"format": "json-pages-1.0", "id": "pages", "title": "Pages"})
    .to_string();
    pages_jsonl.push('\n');
    for p in &pages {
        pages_jsonl.push_str(p);
        pages_jsonl.push('\n');
    }

    // datapackage.json hashes the three payload files.
    let resources = [
        ("data.warc", "archive/data.warc", warc.as_slice()),
        ("index.cdx", "indexes/index.cdx", cdxj.as_bytes()),
        ("pages.jsonl", "pages/pages.jsonl", pages_jsonl.as_bytes()),
    ];
    let res_json: Vec<serde_json::Value> = resources
        .iter()
        .map(|(n, p, b)| {
            serde_json::json!({
                "name": n, "path": p,
                "hash": format!("sha256:{}", sha256_hex(b)), "bytes": b.len(),
            })
        })
        .collect();
    let datapackage = serde_json::json!({
        "profile": "data-package",
        "resources": res_json,
        "wacz_version": "1.1.1",
        "software": "sarun",
    })
    .to_string();
    let dp_digest = serde_json::json!({
        "path": "datapackage.json",
        "hash": format!("sha256:{}", sha256_hex(datapackage.as_bytes())),
    })
    .to_string();

    // Zip it. The WARC is Stored (uncompressed) so CDXJ offsets index it
    // directly; the small text files are Deflated.
    let f = std::fs::File::create(out)?;
    let mut zw = zip::ZipWriter::new(f);
    let stored: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflate: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zw.start_file("archive/data.warc", stored)?;
    zw.write_all(&warc)?;
    zw.start_file("indexes/index.cdx", deflate)?;
    zw.write_all(cdxj.as_bytes())?;
    zw.start_file("pages/pages.jsonl", deflate)?;
    zw.write_all(pages_jsonl.as_bytes())?;
    zw.start_file("datapackage.json", deflate)?;
    zw.write_all(datapackage.as_bytes())?;
    zw.start_file("datapackage-digest.json", deflate)?;
    zw.write_all(dp_digest.as_bytes())?;
    zw.finish()?;
    Ok(rows.len())
}

fn write_warcinfo(warc: &mut Vec<u8>) {
    let fields = "software: sarun\r\nformat: WARC File Format 1.1\r\n";
    let id = uuid_from(&Sha256::digest(b"sarun-warcinfo"));
    let header = format!(
        "WARC/1.1\r\nWARC-Type: warcinfo\r\nWARC-Record-ID: <urn:uuid:{id}>\r\n\
         WARC-Date: {date}\r\nWARC-Filename: data.warc\r\n\
         Content-Type: application/warc-fields\r\nContent-Length: {len}\r\n\r\n",
        date = iso8601(0),
        len = fields.len()
    );
    warc.extend_from_slice(header.as_bytes());
    warc.extend_from_slice(fields.as_bytes());
    warc.extend_from_slice(b"\r\n\r\n");
}

// ── import ──────────────────────────────────────────────────────────────────

/// Parse the WARC `response` records out of a WACZ and write them into a fresh
/// box's webcap. Returns the new box id. Pure file op (no engine needed): the
/// new `<id>.sqlar` appears as a box on the next discovery.
pub fn import(wacz: &Path, name: Option<&str>) -> anyhow::Result<i64> {
    let f = std::fs::File::open(wacz)?;
    let mut zip = zip::ZipArchive::new(f)?;
    // Find the WARC entry (archive/*.warc; .gz not handled — we write plain).
    let warc_name = (0..zip.len())
        .find_map(|i| {
            let n = zip.by_index(i).ok()?.name().to_string();
            (n.starts_with("archive/") && n.ends_with(".warc")).then_some(n)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no archive/*.warc in WACZ (gzipped WARCs \
                                       aren't supported yet)"
            )
        })?;
    let mut warc = Vec::new();
    std::io::copy(&mut zip.by_name(&warc_name)?, &mut warc)?;
    let records = parse_warc_responses(&warc);
    if records.is_empty() {
        anyhow::bail!("no response records in {warc_name}");
    }

    let id = alloc_box_id();
    let db = crate::paths::state_home().join(format!("{id}.sqlar"));
    let tmp = crate::paths::state_home().join(format!("{id}.sqlar.tmp"));
    {
        let conn = rusqlite::Connection::open(&tmp)?;
        conn.execute_batch(crate::capture::SCHEMA)?;
        let nm = name
            .map(str::to_string)
            .unwrap_or_else(|| format!("WACZ{id}"));
        conn.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('name',?1)",
            [&nm],
        )?;
        let ins = "INSERT INTO webcap(ts,method,url,host,status,mime,\
                   req_headers,resp_headers,req_body,resp_body,truncated) \
                   VALUES(?1,?2,?3,?4,?5,?6,'',?7,x'',?8,0)";
        for rec in &records {
            conn.execute(
                ins,
                rusqlite::params![
                    rec.ts,
                    "GET",
                    rec.url,
                    rec.host,
                    rec.status,
                    rec.mime,
                    rec.resp_headers,
                    rec.resp_body
                ],
            )?;
        }
    }
    std::fs::rename(&tmp, &db)?;
    Ok(id)
}

struct ImportRec {
    ts: f64,
    url: String,
    host: String,
    status: i32,
    mime: String,
    resp_headers: String,
    resp_body: Vec<u8>,
}

/// Minimal WARC reader: walk records by their `Content-Length`, keep the
/// `response` ones, and split their HTTP block into status/headers/body.
fn parse_warc_responses(warc: &[u8]) -> Vec<ImportRec> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = find(&warc[pos..], b"WARC/1.") {
        let rec_start = pos + rel;
        // header block ends at the first blank line (\r\n\r\n).
        let Some(hdr_end_rel) = find(&warc[rec_start..], b"\r\n\r\n") else {
            break;
        };
        let hdr = &warc[rec_start..rec_start + hdr_end_rel];
        let block_start = rec_start + hdr_end_rel + 4;
        let fields = std::str::from_utf8(hdr).unwrap_or("");
        let clen: usize = warc_field(fields, "content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let block_end = (block_start + clen).min(warc.len());
        let is_response = warc_field(fields, "warc-type")
            .map(|v| v.eq_ignore_ascii_case("response"))
            .unwrap_or(false);
        let uri = warc_field(fields, "warc-target-uri").unwrap_or_default();
        let date = warc_field(fields, "warc-date").unwrap_or_default();
        if is_response && !uri.is_empty() {
            if let Some(rec) = split_http(&warc[block_start..block_end], &uri, &date) {
                out.push(rec);
            }
        }
        // Advance past this record's block + the inter-record CRLFs.
        pos = block_end;
    }
    out
}

/// Split an `HTTP/1.1 <status> …\r\n<headers>\r\n\r\n<body>` block.
fn split_http(block: &[u8], url: &str, date: &str) -> Option<ImportRec> {
    let sep = find(block, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&block[..sep]).ok()?;
    let body = block[sep + 4..].to_vec();
    let mut lines = head.lines();
    let status_line = lines.next()?;
    let status: i32 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut headers = String::new();
    let mut mime = String::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            headers.push_str(&format!("{k}: {v}\n"));
            if k.eq_ignore_ascii_case("content-type") {
                mime = v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase();
            }
        }
    }
    let host = url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    Some(ImportRec {
        ts: warc_date_to_unix(date) as f64,
        url: url.to_string(),
        host,
        status,
        mime,
        resp_headers: headers,
        resp_body: body,
    })
}

fn warc_field(header: &str, key: &str) -> Option<String> {
    header.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(key).then(|| {
            v.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// ISO8601 `YYYY-MM-DDThh:mm:ssZ` → unix seconds (inverse of civil()).
fn warc_date_to_unix(s: &str) -> i64 {
    let digits: Vec<i64> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    if digits.len() < 6 {
        return 0;
    }
    let (y, mo, d, h, mi, se) = (
        digits[0], digits[1], digits[2], digits[3], digits[4], digits[5],
    );
    // days_from_civil (Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + h * 3600 + mi * 60 + se
}

fn alloc_box_id() -> i64 {
    let mut max = 0i64;
    for dir in [crate::paths::state_home(), crate::paths::live_home()] {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Some(id) = e
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    max = max.max(id);
                }
            }
        }
    }
    max + 1
}

// ── CLI ──────────────────────────────────────────────────────────────────────

pub fn cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("export-wacz") => {
            let (Some(boxref), Some(out)) = (args.get(1), args.get(2)) else {
                eprintln!("usage: sarun web export-wacz <box> <out.wacz>");
                return 2;
            };
            let Some(box_id) = resolve_box(boxref) else {
                eprintln!("sarun web: no such box: {boxref}");
                return 1;
            };
            match export(box_id, &PathBuf::from(out)) {
                Ok(n) => {
                    eprintln!("sarun web: exported {n} captures → {out}");
                    0
                }
                Err(e) => {
                    eprintln!("sarun web export-wacz: {e:#}");
                    1
                }
            }
        }
        Some("import-wacz") => {
            let Some(inp) = args.get(1) else {
                eprintln!("usage: sarun web import-wacz <in.wacz> [NAME]");
                return 2;
            };
            match import(&PathBuf::from(inp), args.get(2).map(String::as_str)) {
                Ok(id) => {
                    println!("{id}");
                    0
                }
                Err(e) => {
                    eprintln!("sarun web import-wacz: {e:#}");
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: sarun web <export-wacz|import-wacz> …");
            2
        }
    }
}

/// Resolve a box reference (numeric id or a box NAME) to an id.
fn resolve_box(s: &str) -> Option<i64> {
    if let Ok(id) = s.parse::<i64>() {
        return Some(id);
    }
    crate::discover::discover()
        .values()
        .find(|b| b.name == s)
        .map(|b| b.box_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surt_reverses_host() {
        assert_eq!(
            surt("https://www.example.com/a?b=1"),
            "com,example,www)/a?b=1"
        );
        assert_eq!(surt("http://x.io:8080/"), "io,x:8080)/");
    }

    #[test]
    fn civil_and_iso_roundtrip() {
        // 2021-01-01T00:00:00Z = 1609459200.
        assert_eq!(iso8601(1609459200), "2021-01-01T00:00:00Z");
        assert_eq!(ts14(1609459200), "20210101000000");
        assert_eq!(warc_date_to_unix("2021-01-01T00:00:00Z"), 1609459200);
    }

    #[test]
    fn warc_response_roundtrips_through_parse() {
        // Build one response record the way export does, then parse it back.
        let row = Row {
            ts: 1609459200.0,
            _method: "GET".into(),
            url: "https://ex.test/p".into(),
            status: 200,
            mime: "text/html".into(),
            resp_headers: "content-type: text/html\ncontent-length: 5\n".into(),
            resp_body: b"hello".to_vec(),
        };
        let block = http_response_block(&row);
        let mut warc = Vec::new();
        let header = format!(
            "WARC/1.1\r\nWARC-Type: response\r\nWARC-Target-URI: {}\r\n\
             WARC-Date: {}\r\nContent-Type: application/http; msgtype=response\r\n\
             Content-Length: {}\r\n\r\n",
            row.url,
            iso8601(1609459200),
            block.len()
        );
        warc.extend_from_slice(header.as_bytes());
        warc.extend_from_slice(&block);
        warc.extend_from_slice(b"\r\n\r\n");

        let recs = parse_warc_responses(&warc);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].url, "https://ex.test/p");
        assert_eq!(recs[0].status, 200);
        assert_eq!(recs[0].host, "ex.test");
        assert_eq!(recs[0].mime, "text/html");
        assert_eq!(recs[0].resp_body, b"hello");
        assert_eq!(recs[0].ts as i64, 1609459200);
    }
}
