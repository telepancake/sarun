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
//! Per SPEC §"Wire facts": parts live under `<date>/xml/bzip2/`
//! alongside `SHA256SUMS` and `_SUCCESS`. The discoverer keys "done"
//! off the presence of `_SUCCESS` and authoritatively lists parts from
//! `SHA256SUMS`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::NaiveDate;
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::types::{Error, Part, Result, Run, RunSource};

/// The production base URL. Tests override via `Config`.
pub const DUMPS_BASE_URL: &str = "https://dumps.wikimedia.org";

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
        if !http_exists(client, &format!("{dir}_SUCCESS")) {
            continue;
        }
        let sums = match http_fetch_ok(client, &format!("{dir}SHA256SUMS")) {
            Some(s) => s,
            None => continue,
        };
        let parts = parse_sha256sums(client, &dir, &sums)?;
        return Ok(Run {
            source: RunSource::ContentHistory,
            date: d,
            parts,
        });
    }
    // Listing has dates but none is done — fall through to legacy.
    Err(BranchErr::Unavailable)
}

fn parse_sha256sums(client: &Client, dir: &str, sums: &[u8]) -> Result<Vec<Part>> {
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
        if digest.len() != 64 {
            return Err(Error::Parse(format!("malformed SHA256SUMS line: {line:?}")));
        }
        let url = format!("{dir}{name}");
        let size = http_resolve_size(client, &url)?;
        parts.push(Part {
            url,
            filename: name.to_string(),
            size_bytes: size,
            sha256: Some(digest.to_string()),
            sha1: None,
        });
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

fn http_get(client: &Client, url: &str) -> Result<(Vec<u8>, StatusCode)> {
    let resp = client.get(url).send()?;
    let status = resp.status();
    let body = resp.bytes()?;
    Ok((body.to_vec(), status))
}

/// Existence probe that transfers NO body bytes: HEAD first; a server
/// that rejects/doesn't match HEAD (405, or a mock that only routes
/// GET) is retried with a plain GET whose response is dropped after
/// the status line — the body is never read.
fn http_exists(client: &Client, url: &str) -> bool {
    if let Ok(r) = client.head(url).send() {
        if r.status().is_success() {
            return true;
        }
        if r.status() == StatusCode::METHOD_NOT_ALLOWED || r.status() == StatusCode::NOT_FOUND {
            // fall through to the GET probe: a HEAD 404 may be a
            // GET-only route (mock servers), and 405 is an explicit
            // "use another method".
        } else {
            return false;
        }
    }
    match client.get(url).send() {
        Ok(r) => r.status().is_success(), // response dropped unread
        Err(_) => false,
    }
}

fn http_fetch_ok(client: &Client, url: &str) -> Option<Vec<u8>> {
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().ok().map(|b| b.to_vec())
}

/// The `Content-Length` header as a number, if present and parseable.
/// Read from the raw header — NOT `Response::content_length()`, whose
/// value can reflect the (absent) HEAD body rather than the entity.
fn header_content_length(resp: &Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The total size from a `Content-Range: bytes X-Y/TOTAL` (or
/// `bytes */TOTAL`) header.
fn header_content_range_total(resp: &Response) -> Option<u64> {
    let v = resp.headers().get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    let total = v.rsplit('/').next()?.trim();
    total.parse().ok()
}

/// Resolve a part's size from HTTP metadata WITHOUT transferring the
/// body (a dump part is gigabytes; the old GET-and-drain here pulled
/// every one of them just to learn a number the headers already
/// carry). HEAD + `Content-Length` is the happy path; servers that
/// don't answer HEAD get a `Range: bytes=0-0` GET whose headers
/// (`Content-Range` total on 206/416, `Content-Length` on an
/// ignored-Range 200) are read and whose body never is. No usable
/// header on any route is a LOUD error, never a silent drain.
fn http_resolve_size(client: &Client, url: &str) -> Result<u64> {
    if let Ok(resp) = client.head(url).send() {
        if resp.status().is_success() {
            if let Some(len) = header_content_length(&resp) {
                return Ok(len);
            }
        } else if !matches!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
        ) {
            return Err(Error::HttpStatus {
                status: resp.status().as_u16(),
                url: url.to_string(),
            });
        }
        // success-without-length, 405, or a GET-only mock route: fall
        // through to the Range probe.
    }
    let resp: Response = client.get(url).header("Range", "bytes=0-0").send()?;
    let status = resp.status();
    // Response is dropped unread in every arm below — headers only.
    if status == StatusCode::PARTIAL_CONTENT || status == StatusCode::RANGE_NOT_SATISFIABLE {
        return header_content_range_total(&resp).ok_or_else(|| {
            Error::Parse(format!("no total in Content-Range for {url}"))
        });
    }
    if status.is_success() {
        return header_content_length(&resp)
            .ok_or_else(|| Error::Parse(format!("no Content-Length for {url}")));
    }
    Err(Error::HttpStatus {
        status: status.as_u16(),
        url: url.to_string(),
    })
}
