//! Discover the newest complete dump run for a wiki.
//!
//! Tries the Content File Exports tree first
//! (`/other/mediawiki_content_history/<dbname>/`) and falls back to the
//! legacy XML dumps path (`/<dbname>/<YYYYMMDD>/dumpstatus.json`) on 404.
//!
//! ## Test-injection design
//!
//! SPEC says "tests inject an http.Client whose transport rewrites the
//! base URL onto an httptest server". The Rust analog here is a `Config`
//! struct that carries the base URL. Production callers use
//! `discover(client, dbname)`, which is equivalent to
//! `discover_with(client, &Config::default(), dbname)` and resolves
//! against `DUMPS_BASE_URL`. Tests construct a `Config { base_url: ... }`
//! pointed at a local mock server and call `discover_with` directly.
//!
//! Content-history parts live under `<date>/xml/bzip2/`; the published
//! `SHA256SUMS` is both the completion fence and authoritative part list.
//! The directory index supplies all exact part sizes in one bounded request;
//! discovery never probes every payload object separately.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::NaiveDate;
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::header::HeaderMap;
use reqwest::StatusCode;
use serde::Deserialize;

use crate::politeness;
use crate::types::{Error, Part, Result, Run, RunSource};

/// The production base URL. Tests override via `Config`.
pub const DUMPS_BASE_URL: &str = "https://dumps.wikimedia.org";

const MAX_SMALL_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Configuration for `discover` / `fetch`. Production code uses
/// `Config::default()`; tests construct one pointed at a mock server.
#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: DUMPS_BASE_URL.to_string(),
        }
    }
}

/// Discover the newest complete run for `dbname` against the production
/// dumps host. SPEC §API.
pub fn discover(client: &Client, dbname: &str) -> Result<Run> {
    discover_with(client, &Config::default(), dbname)
}

/// Discover with an explicit `Config` (test-injection seam).
pub fn discover_with(client: &Client, cfg: &Config, dbname: &str) -> Result<Run> {
    match discover_content_history(client, cfg, dbname) {
        Ok(run) => Ok(run),
        Err(BranchErr::Unavailable) => discover_legacy(client, cfg, dbname),
        Err(BranchErr::Fatal(e)) => Err(e),
    }
}

/// Completed daily adds/changes runs newer than `after`, oldest first.
/// These are the normal maintenance source after a full-history bootstrap.
pub fn discover_incremental_with(
    client: &Client,
    cfg: &Config,
    dbname: &str,
    after: Option<NaiveDate>,
) -> Result<Vec<Run>> {
    let root = format!("{}/other/incr/{dbname}/", cfg.base_url);
    let (body, status) = http_get(client, &root)?;
    // Wikimedia does not publish an incremental tree for every database.
    // Closed and otherwise low-activity wikis can have complete XML/history
    // snapshots while `/other/incr/<dbname>/` legitimately does not exist.
    // That means there are zero daily runs; callers must still continue to
    // discover the independently published MediaWiki History frontier.
    if status == StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        return Err(Error::HttpStatus { status: status.as_u16(), url: root });
    }
    let body = String::from_utf8_lossy(&body);
    let mut dates: Vec<NaiveDate> = re_href_ymd()
        .captures_iter(&body)
        .filter_map(|capture| NaiveDate::parse_from_str(&capture[1], "%Y%m%d").ok())
        .filter(|date| after.map(|after| *date > after).unwrap_or(true))
        .collect();
    dates.sort();
    dates.dedup();

    let mut runs = Vec::new();
    for date in dates {
        let ymd = date.format("%Y%m%d");
        let dir = format!("{root}{ymd}/");
        let (status_body, status) = http_get(client, &format!("{dir}status.txt"))?;
        if status == StatusCode::NOT_FOUND {
            continue;
        }
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                url: format!("{dir}status.txt"),
            });
        }
        if !String::from_utf8_lossy(&status_body).to_ascii_lowercase().contains("done") {
            continue;
        }
        let sums_url = format!("{dir}{dbname}-{ymd}-md5sums.txt");
        let (sums, status) = http_get(client, &sums_url)?;
        if !status.is_success() {
            return Err(Error::HttpStatus { status: status.as_u16(), url: sums_url });
        }
        let wanted = format!("{dbname}-{ymd}-pages-meta-hist-incr.xml.bz2");
        let text = std::str::from_utf8(&sums)
            .map_err(|error| Error::Parse(format!("incremental md5sums not utf-8: {error}")))?;
        let digest = text.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            (name == wanted).then(|| digest.to_string())
        }).ok_or_else(|| Error::Parse(format!("{wanted} missing from {sums_url}")))?;
        if digest.len() != 32 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Parse(format!("invalid md5 for {wanted}")));
        }
        runs.push(Run {
            source: RunSource::Incremental,
            date,
            parts: vec![Part {
                url: format!("{dir}{wanted}"),
                filename: wanted,
                size_bytes: 0,
                sha256: None,
                sha1: None,
                md5: Some(digest),
            }],
        });
    }
    Ok(runs)
}

/// Internal: a branch either yielded a run, declared itself unavailable
/// (so the caller falls through), or raised a fatal error.
enum BranchErr {
    Unavailable,
    Fatal(Error),
}

impl From<Error> for BranchErr {
    fn from(e: Error) -> Self {
        BranchErr::Fatal(e)
    }
}

// ---- content-history branch ------------------------------------------

fn re_href_date() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"href="(\d{4}-\d{2}-\d{2})/""#).unwrap())
}

fn re_href_ymd() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"href="(\d{8})/""#).unwrap())
}

fn re_page_part() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"-p(\d+)").unwrap())
}

fn discover_content_history(
    client: &Client,
    cfg: &Config,
    dbname: &str,
) -> std::result::Result<Run, BranchErr> {
    let root = format!(
        "{}/other/mediawiki_content_history/{}/",
        cfg.base_url, dbname
    );
    let (body, status) = http_get(client, &root)?;
    if status == StatusCode::NOT_FOUND {
        return Err(BranchErr::Unavailable);
    }
    if !status.is_success() {
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            url: root,
        }
        .into());
    }
    let body = String::from_utf8_lossy(&body);
    let mut dates: Vec<NaiveDate> = re_href_date()
        .captures_iter(&body)
        .filter_map(|c| NaiveDate::parse_from_str(&c[1], "%Y-%m-%d").ok())
        .collect();
    if dates.is_empty() {
        return Err(BranchErr::Unavailable);
    }
    dates.sort();
    dates.dedup();
    dates.reverse();

    for d in dates {
        let dir = format!("{}{}/xml/bzip2/", root, d.format("%Y-%m-%d"));
        // Wikimedia documents SHA256SUMS itself as the completion fence.
        // Avoid a redundant _SUCCESS probe on every poll.
        let sums_url = format!("{dir}SHA256SUMS");
        let (sums, status) = http_get(client, &sums_url)?;
        if status == StatusCode::NOT_FOUND {
            continue;
        }
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                url: sums_url,
            }
            .into());
        }
        let (listing, status) = http_get(client, &dir)?;
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                url: dir,
            }
            .into());
        }
        let sizes = parse_content_history_sizes(&dir, &listing)?;
        let parts = parse_sha256sums(&dir, &sums, sizes)?;
        if parts.is_empty() {
            return Err(Error::Parse(format!("empty SHA256SUMS at {dir}")).into());
        }
        return Ok(Run {
            source: RunSource::ContentHistory,
            date: d,
            parts,
        });
    }
    // Listing has dates but none is done — fall through to legacy.
    Err(BranchErr::Unavailable)
}

fn parse_content_history_sizes(dir: &str, listing: &[u8]) -> Result<BTreeMap<String, u64>> {
    let text = std::str::from_utf8(listing)
        .map_err(|error| Error::Parse(format!("content-history listing not utf-8 at {dir}: {error}")))?;
    let mut sizes = BTreeMap::new();
    for line in text.lines() {
        let Some(href) = line.find("href=\"").map(|offset| offset + "href=\"".len()) else {
            continue;
        };
        let Some(href_end) = line[href..].find('"').map(|offset| href + offset) else {
            continue;
        };
        let name = &line[href..href_end];
        if !name.ends_with(".xml.bz2") {
            continue;
        }
        if std::path::Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            != Some(name)
        {
            return Err(Error::Parse(format!(
                "unsafe dump part name in content-history listing: {name:?}"
            )));
        }
        let anchor_tail = &line[href_end + 1..];
        let metadata = anchor_tail
            .find("</a>")
            .map(|offset| &anchor_tail[offset + "</a>".len()..])
            .ok_or_else(|| {
                Error::Parse(format!(
                    "dump part has no complete link in content-history listing at {dir}: {name:?}"
                ))
            })?;
        let size = metadata
            .split_whitespace()
            .last()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|size| *size != 0)
            .ok_or_else(|| {
                Error::Parse(format!(
                    "dump part has no positive exact size in content-history listing at {dir}: {name:?}"
                ))
            })?;
        if sizes.insert(name.to_string(), size).is_some() {
            return Err(Error::Parse(format!(
                "duplicate dump part in content-history listing at {dir}: {name:?}"
            )));
        }
    }
    if sizes.is_empty() {
        return Err(Error::Parse(format!(
            "content-history listing contains no sized dump parts at {dir}"
        )));
    }
    Ok(sizes)
}

fn parse_sha256sums(
    dir: &str,
    sums: &[u8],
    mut sizes: BTreeMap<String, u64>,
) -> Result<Vec<Part>> {
    let text = std::str::from_utf8(sums)
        .map_err(|e| Error::Parse(format!("SHA256SUMS not utf-8: {e}")))?;
    let mut parts = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (digest, name) = match line.find("  ") {
            Some(i) => (&line[..i], line[i + 2..].trim()),
            None => match line.find(' ') {
                Some(i) => (&line[..i], line[i + 1..].trim()),
                None => continue,
            },
        };
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Parse(format!("malformed SHA256SUMS line: {line:?}")));
        }
        if std::path::Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            != Some(name)
        {
            return Err(Error::Parse(format!("unsafe dump part name: {name:?}")));
        }
        if parts.iter().any(|part: &Part| part.filename == name) {
            return Err(Error::Parse(format!(
                "duplicate dump part in SHA256SUMS at {dir}: {name:?}"
            )));
        }
        let size = sizes.remove(name).ok_or_else(|| {
            Error::Parse(format!(
                "SHA256SUMS part has no exact size in content-history listing at {dir}: {name:?}"
            ))
        })?;
        let url = format!("{dir}{name}");
        parts.push(Part {
            url,
            filename: name.to_string(),
            size_bytes: size,
            sha256: Some(digest.to_string()),
            sha1: None,
            md5: None,
        });
    }
    if !sizes.is_empty() {
        return Err(Error::Parse(format!(
            "content-history listing has {} dump part(s) absent from SHA256SUMS at {dir}",
            sizes.len()
        )));
    }
    sort_parts_by_page_range(&mut parts);
    Ok(parts)
}

/// Sort parts ascending by the integer following the first `-p` token in
/// the filename. Filenames lacking that token sort to the end, tied by
/// lexicographic order. Stable sort.
fn sort_parts_by_page_range(parts: &mut [Part]) {
    let key = |name: &str| -> (i64, String) {
        match re_page_part().captures(name) {
            Some(c) => (c[1].parse::<i64>().unwrap_or(-1), name.to_string()),
            None => (-1, name.to_string()),
        }
    };
    parts.sort_by(|a, b| {
        let (ka, ta) = key(&a.filename);
        let (kb, tb) = key(&b.filename);
        if ka == kb {
            return ta.cmp(&tb);
        }
        match (ka < 0, kb < 0) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => ka.cmp(&kb),
        }
    });
}

// ---- legacy branch ---------------------------------------------------

#[derive(Deserialize)]
struct DumpStatus {
    jobs: BTreeMap<String, DumpStatusJob>,
}

#[derive(Deserialize)]
struct DumpStatusJob {
    status: String,
    #[serde(default)]
    files: BTreeMap<String, DumpStatusFile>,
}

#[derive(Deserialize)]
struct DumpStatusFile {
    #[serde(default)]
    size: u64,
    #[serde(default)]
    url: String,
    #[serde(default)]
    sha1: String,
}

fn discover_legacy(client: &Client, cfg: &Config, dbname: &str) -> Result<Run> {
    let root = format!("{}/{}/", cfg.base_url, dbname);
    let (body, status) = http_get(client, &root)?;
    if status == StatusCode::NOT_FOUND {
        return Err(Error::NoCompleteRun {
            dbname: dbname.to_string(),
        });
    }
    if !status.is_success() {
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            url: root,
        });
    }
    let body = String::from_utf8_lossy(&body);
    let mut dates: Vec<(NaiveDate, String)> = re_href_ymd()
        .captures_iter(&body)
        .filter_map(|c| {
            let ymd = c[1].to_string();
            NaiveDate::parse_from_str(&ymd, "%Y%m%d")
                .ok()
                .map(|d| (d, ymd))
        })
        .collect();
    if dates.is_empty() {
        return Err(Error::NoCompleteRun {
            dbname: dbname.to_string(),
        });
    }
    dates.sort_by(|a, b| a.0.cmp(&b.0));
    dates.dedup_by(|a, b| a.0 == b.0);
    dates.reverse();

    for (d, ymd) in dates {
        let url = format!("{root}{ymd}/dumpstatus.json");
        let (raw, code) = http_get(client, &url)?;
        if code == StatusCode::NOT_FOUND {
            continue;
        }
        if !code.is_success() {
            continue;
        }
        let ds: DumpStatus =
            serde_json::from_slice(&raw).map_err(|e| Error::Parse(format!("parse {url}: {e}")))?;
        let Some(job) = ds.jobs.get("metahistorybz2dump") else {
            continue;
        };
        if job.status != "done" {
            continue;
        }
        let mut parts: Vec<Part> = job
            .files
            .iter()
            .map(|(name, rec)| {
                let mut u = rec.url.clone();
                if u.is_empty() {
                    u = format!("/{dbname}/{ymd}/{name}");
                }
                if u.starts_with('/') {
                    u = format!("{}{}", cfg.base_url, u);
                }
                Part {
                    url: u,
                    filename: name.clone(),
                    size_bytes: rec.size,
                    sha256: None,
                    sha1: if rec.sha1.is_empty() {
                        None
                    } else {
                        Some(rec.sha1.clone())
                    },
                    md5: None,
                }
            })
            .collect();
        sort_parts_by_page_range(&mut parts);
        return Ok(Run {
            source: RunSource::Legacy,
            date: d,
            parts,
        });
    }
    Err(Error::NoCompleteRun {
        dbname: dbname.to_string(),
    })
}

// ---- HTTP helpers ----------------------------------------------------

/// Fetch a small metadata document. Dump payloads must use [`crate::fetch`].
pub fn get_small(client: &Client, url: &str) -> Result<(Vec<u8>, StatusCode)> {
    http_get(client, url)
}

/// Fetch the one-time essential siteinfo bootstrap through the same global
/// request scheduler, without applying dump-path robots rules.  Siteinfo is
/// the only deliberately exempt request: without it the archive cannot
/// interpret pages.
pub fn get_siteinfo(client: &Client, url: &str) -> Result<(Vec<u8>, StatusCode)> {
    http_get_inner(client, url, false)
}

fn http_get(client: &Client, url: &str) -> Result<(Vec<u8>, StatusCode)> {
    http_get_inner(client, url, true)
}

fn http_get_inner(client: &Client, url: &str, check_robots: bool) -> Result<(Vec<u8>, StatusCode)> {
    if check_robots {
        politeness::ensure_robots(client, url)?;
    }
    #[cfg(target_os = "macos")]
    if crate::curl_http::handles(url) {
        return curl_get_small(url);
    }
    for attempt in 0..4 {
        let mut permit = politeness::acquire(url)?;
        match client.get(url).timeout(METADATA_REQUEST_TIMEOUT).send() {
            Ok(resp) => {
                let status = resp.status();
                let retry_after = politeness::parse_retry_after_header(resp.headers());
                if attempt < politeness::MAX_RESPONSE_RETRIES
                    && politeness::should_retry_response(status, retry_after)
                {
                    let delay = permit.retry_delay(Some(status.as_u16()), retry_after);
                    drop(resp);
                    drop(permit);
                    std::thread::sleep(delay);
                    continue;
                }
                if status == StatusCode::TOO_MANY_REQUESTS {
                    let _ = permit.retry_delay(Some(status.as_u16()), retry_after);
                }
                let body = read_bounded_metadata_body(resp, url)?;
                permit.release_now();
                return Ok((body, status));
            }
            Err(error) if attempt < 3 && (error.is_connect() || error.is_timeout()) => {
                let delay = permit.transport_delay(
                    std::time::Duration::from_secs(2u64.saturating_pow(attempt + 1).max(5)),
                );
                drop(permit);
                std::thread::sleep(delay);
            }
            Err(error) => {
                permit.release_now();
                return Err(error.into());
            }
        }
    }
    unreachable!("discovery retry loop returns")
}

fn read_bounded_metadata_body(mut response: Response, url: &str) -> Result<Vec<u8>> {
    if header_content_length(response.headers())
        .is_some_and(|length| length > MAX_SMALL_RESPONSE_BYTES)
    {
        return Err(Error::Parse(format!(
            "metadata response exceeded the 64 MiB bound for {url}"
        )));
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(MAX_SMALL_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_SMALL_RESPONSE_BYTES {
        return Err(Error::Parse(format!(
            "metadata response exceeded the 64 MiB bound for {url}"
        )));
    }
    Ok(body)
}

#[cfg(target_os = "macos")]
fn curl_get_small(url: &str) -> Result<(Vec<u8>, StatusCode)> {
    for attempt in 0..4 {
        let mut permit = politeness::acquire(url)?;
        match crate::curl_http::request(url, crate::curl_http::RequestKind::Get) {
            Ok(response) => {
                let retry_after = politeness::parse_retry_after_header(&response.headers);
                if attempt < politeness::MAX_RESPONSE_RETRIES
                    && politeness::should_retry_response(response.status, retry_after)
                {
                    let delay = permit.retry_delay(Some(response.status.as_u16()), retry_after);
                    drop(permit);
                    std::thread::sleep(delay);
                    continue;
                }
                if response.status == StatusCode::TOO_MANY_REQUESTS {
                    let _ = permit.retry_delay(Some(response.status.as_u16()), retry_after);
                }
                permit.release_now();
                return Ok((response.body, response.status));
            }
            Err(error) if attempt < 3 && matches!(error, Error::Io(_)) => {
                let delay = permit.transport_delay(
                    std::time::Duration::from_secs(2u64.saturating_pow(attempt + 1).max(5)),
                );
                drop(permit);
                std::thread::sleep(delay);
            }
            Err(error) => {
                permit.release_now();
                return Err(error);
            }
        }
    }
    unreachable!("curl discovery retry loop returns")
}

/// The `Content-Length` header as a number, if present and parseable.
/// Read from the raw header — NOT `Response::content_length()`, whose
/// value can reflect the (absent) HEAD body rather than the entity.
fn header_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use super::*;

    fn read_request_headers(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "client closed before sending request headers");
            request.extend_from_slice(&chunk[..count]);
            assert!(request.len() <= 64 * 1024, "request headers are unbounded");
        }
    }

    #[test]
    fn metadata_get_rejects_advertised_body_above_bound_before_reading() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_headers(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_SMALL_RESPONSE_BYTES + 1
            )
            .unwrap();
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let error = get_small(&client, &format!("http://{address}/metadata.json"))
            .expect_err("oversized metadata must fail closed");
        server.join().unwrap();

        assert!(error.to_string().contains("exceeded the 64 MiB bound"));
    }
}
