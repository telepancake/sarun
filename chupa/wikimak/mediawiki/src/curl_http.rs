//! Bounded macOS transport for small official Wikimedia metadata.
//!
//! The dump payload reader already uses `/usr/bin/curl` on macOS.  Keep the
//! discovery side on the same known-working system transport when reqwest's
//! connector cannot establish a public TCP connection.  This module is not
//! used for injected/local test origins and never writes a response to disk.

use std::io;
use std::process::Command;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::types::{Error, Result};

const MAX_SMALL_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SMALL_RESPONSE_ARG: &str = "67108864";
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum RequestKind {
    Get,
}

pub(crate) struct Response {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn handles(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if scheme != "https" {
        return false;
    }
    let Some(authority) = rest.split('/').next() else {
        return false;
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    host == "wikimedia.org"
        || host.ends_with(".wikimedia.org")
        || host == "wikipedia.org"
        || host.ends_with(".wikipedia.org")
}

pub(crate) fn request(url: &str, kind: RequestKind) -> Result<Response> {
    if !handles(url) {
        return Err(Error::Parse(format!(
            "curl metadata transport refuses non-Wikimedia URL {url}"
        )));
    }
    let output = configured_command(url, kind).output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::Io(io::Error::other(format!(
            "curl metadata request failed for {url} ({}): {}",
            output.status,
            if detail.is_empty() {
                "no diagnostic"
            } else {
                detail.as_str()
            }
        ))));
    }
    if output.stdout.len() > MAX_SMALL_RESPONSE_BYTES {
        return Err(Error::Parse(format!(
            "curl metadata response exceeded the 64 MiB bound for {url}"
        )));
    }
    if output.stderr.len() > MAX_RESPONSE_HEADER_BYTES {
        return Err(Error::Parse(format!(
            "curl metadata response headers exceeded the 64 KiB bound for {url}"
        )));
    }
    parse_response(&output.stderr, output.stdout, url)
}

fn configured_command(url: &str, kind: RequestKind) -> Command {
    let mut command = Command::new("/usr/bin/curl");
    // Ignore ~/.curlrc and system curl configuration. The transport contract
    // is entirely represented by the bounded arguments below.
    command.arg("--disable");
    command.args([
        "--location",
        "--max-redirs",
        "5",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--silent",
        "--show-error",
        "--connect-timeout",
        "30",
        "--max-time",
        "300",
        "--speed-limit",
        "1",
        "--speed-time",
        "30",
        "--max-filesize",
        MAX_SMALL_RESPONSE_ARG,
        "--user-agent",
        &user_agent(),
        "--dump-header",
        "/dev/stderr",
    ]);
    match kind {
        RequestKind::Get => {}
    }
    command.arg("--url").arg(url);
    command
}

fn user_agent() -> String {
    let operator = std::env::var("SARUN_WIKIMEDIA_CONTACT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("; operator: {value}"))
        .unwrap_or_default();
    format!(
        "sarun-wikimak/{} (+https://github.com/telepancake/sarun{operator})",
        env!("CARGO_PKG_VERSION")
    )
}

fn parse_response(raw_headers: &[u8], body: Vec<u8>, url: &str) -> Result<Response> {
    let mut cursor = 0;
    loop {
        let relative_end = raw_headers[cursor..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
            .ok_or_else(|| Error::Parse(format!("curl returned no HTTP headers for {url}")))?;
        let end = cursor + relative_end;
        let header = &raw_headers[cursor..end];
        let (status, reason) = response_status(header).ok_or_else(|| {
            Error::Parse(format!("curl returned malformed HTTP status for {url}"))
        })?;
        cursor = end;
        if status.is_informational()
            || status.is_redirection()
            || reason.eq_ignore_ascii_case("connection established")
        {
            continue;
        }
        let headers = response_headers(header, url)?;
        return Ok(Response {
            status,
            headers,
            body,
        });
    }
}

fn response_status(header: &[u8]) -> Option<(StatusCode, &str)> {
    let line = header.split(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).ok()?;
    let mut fields = line.splitn(3, ' ');
    let protocol = fields.next()?;
    if !protocol.starts_with("HTTP/") {
        return None;
    }
    let status = StatusCode::from_u16(fields.next()?.parse().ok()?).ok()?;
    Some((status, fields.next().unwrap_or_default().trim()))
}

fn response_headers(header: &[u8], url: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for line in header.split(|byte| *byte == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Err(Error::Parse(format!(
                "curl returned malformed HTTP header for {url}"
            )));
        };
        let name = HeaderName::from_bytes(&line[..colon]).map_err(|_| {
            Error::Parse(format!("curl returned invalid HTTP header name for {url}"))
        })?;
        let value = HeaderValue::from_bytes(line[colon + 1..].trim_ascii()).map_err(|_| {
            Error::Parse(format!("curl returned invalid HTTP header value for {url}"))
        })?;
        headers.append(name, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_redirect_headers_separately_from_final_body() {
        let headers = b"HTTP/1.1 301 Moved\r\nLocation: https://dumps.wikimedia.org/x\r\n\r\n\
                        HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Test: yes\r\n\r\n";
        let body = b"HTTP/1.1 is ordinary response text\n{\"ok\":true}".to_vec();
        let response =
            parse_response(headers, body.clone(), "https://dumps.wikimedia.org/start").unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.headers["content-type"], "application/json");
        assert_eq!(response.headers["x-test"], "yes");
        assert_eq!(response.body, body);
    }

    #[test]
    fn rejects_non_wikimedia_and_insecure_origins() {
        assert!(handles("https://dumps.wikimedia.org/robots.txt"));
        assert!(handles("https://en.wikipedia.org/w/api.php"));
        assert!(!handles("http://dumps.wikimedia.org/robots.txt"));
        assert!(!handles("https://wikimedia.example/robots.txt"));
    }

    #[test]
    fn curl_configuration_is_disabled_before_any_request_options() {
        let command = configured_command(
            "https://dumps.wikimedia.org/robots.txt",
            RequestKind::Get,
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(arguments.first().map(String::as_str), Some("--disable"));
        assert!(arguments.windows(2).any(|pair| {
            pair == ["--max-filesize", MAX_SMALL_RESPONSE_ARG]
        }));
    }
}
