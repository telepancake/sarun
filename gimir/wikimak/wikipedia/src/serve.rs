//! `wikimak serve <root> [addr]` — the local browse window (plan §5).
//!
//! Routes:
//!   * `GET /wiki/<title>` (+ `?asof=<YYYY-MM-DD|unix-micros>`) — render a
//!     page at τ, following `#REDIRECT` at τ ("redirected from" line).
//!   * `GET /w/history/<title>` — the page's revision list (newest-first),
//!     each with a "view at this instant" link (`?asof=<rev-micros>`).
//!   * `GET /w/allpages?filter=<substr>` — the titles listing.
//!   * `GET /w/media/<file>?w=<bucket>` — stream a materialized blob, or an
//!     inline SVG placeholder (HTTP 200) on a miss so pages stay clean.
//!   * `GET /` — redirect to `/w/allpages`.
//!
//! Every internal link carries `?asof=` when the view is time-shifted:
//! the renderer covers content links through [`RenderOptions::asof_query`];
//! the chrome links (history/allpages/date-picker) append it here.
//!
//! Concurrency: [`Instance`] is `Sync` (its state sits behind an inner
//! `Mutex`), so it is shared as `Arc` across a fixed 4-thread pool that
//! each block in `Server::recv`. A [`LuaInvoker`] is built PER RENDER — its
//! module-source cache is per-τ (per-render), so it cannot be pooled across
//! requests that may carry different τ.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use tiny_http::{Header, Method, Request, Response, Server};

use wikimak_media::{BlobMediaResolver, MediaStore};
use wikimak_scribunto::LuaInvoker;
use wikimak_wikitext::html::escape;
use wikimak_wikitext::{render, ModuleInvoker, PageStore, RenderOptions, RenderOutput, Title};

use crate::asof::AsOfView;
use crate::Instance;

/// Route prefix for lazily-materialized media; kept in sync with the
/// `/w/media/` handler and [`BlobMediaResolver`].
const MEDIA_ROUTE_PREFIX: &str = "/w/media/";
/// `#REDIRECT` follow budget at τ (plan §2 redirects, loop-capped).
const MAX_REDIRECT_HOPS: u32 = 10;
/// Worker threads blocking in `Server::recv`.
const POOL_THREADS: usize = 4;

pub struct ServeConfig {
    /// Bind address, e.g. `127.0.0.1:8642`.
    pub addr: String,
    /// Blob-cache root for materialized media. Offline serve (no `fetch`
    /// in the media crate) turns every miss into an inline placeholder.
    pub media_cache: PathBuf,
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

struct App {
    inst: Arc<Instance>,
    media: Arc<MediaStore>,
}

/// Start the server and block, dispatching requests across the pool.
pub fn serve(inst: Instance, cfg: ServeConfig) -> Result<(), String> {
    let server = Server::http(&cfg.addr).map_err(|e| format!("bind {}: {e}", cfg.addr))?;
    let server = Arc::new(server);
    // No repos + no `fetch` feature ⇒ every media miss is an offline miss
    // (inline placeholder). A prefetch driver could pass a commons chain.
    let media = MediaStore::new(cfg.media_cache, Vec::new());
    let app = Arc::new(App {
        inst: Arc::new(inst),
        media: Arc::new(media),
    });

    eprintln!("wikimak serve: listening on http://{}", cfg.addr);

    let mut handles = Vec::new();
    for _ in 0..POOL_THREADS {
        let server = Arc::clone(&server);
        let app = Arc::clone(&app);
        handles.push(thread::spawn(move || loop {
            match server.recv() {
                Ok(req) => {
                    let resp = handle(&app, &req);
                    let _ = req.respond(resp);
                }
                Err(_) => break,
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn handle(app: &App, req: &Request) -> Resp {
    if *req.method() != Method::Get {
        return text_resp(405, "method not allowed");
    }
    let url = req.url();
    let (path, query_raw) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url, ""),
    };
    let query = parse_query(query_raw);

    if path == "/" {
        return redirect("/w/allpages");
    }
    if let Some(rest) = path.strip_prefix("/wiki/") {
        return page_response(app, &percent_decode(rest), &query);
    }
    if let Some(rest) = path.strip_prefix("/w/history/") {
        return history_response(app, &percent_decode(rest), &query);
    }
    if path == "/w/allpages" {
        return allpages_response(app, &query);
    }
    if let Some(rest) = path.strip_prefix("/w/media/") {
        return media_response(app, &percent_decode(rest), &query);
    }
    not_found_page(app, &query)
}

// ---------------------------------------------------------------------------
// asof parsing
// ---------------------------------------------------------------------------

/// Parse the `asof` query value into (τ-micros, link-suffix). A bare
/// integer is unix micros; `YYYY-MM-DD` is END-of-day UTC (so "as of that
/// day" captures edits made during it). The suffix is `?asof=<raw>` and is
/// appended to every chrome link so the date sticks through navigation;
/// empty when browsing the head.
fn asof_from_query(query: &HashMap<String, String>) -> (Option<i64>, String) {
    if let Some(raw) = query.get("asof") {
        if let Some(ts) = parse_asof(raw) {
            return (Some(ts), format!("?asof={}", urlq(raw)));
        }
    }
    (None, String::new())
}

fn parse_asof(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<i64>().ok();
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() == 3 {
        let y = parts[0].parse::<i32>().ok()?;
        let m = parts[1].parse::<u32>().ok()?;
        let d = parts[2].parse::<u32>().ok()?;
        let dt = chrono::NaiveDate::from_ymd_opt(y, m, d)?.and_hms_opt(23, 59, 59)?;
        return Some(dt.and_utc().timestamp_micros());
    }
    None
}

/// τ-micros → `YYYY-MM-DD` for the date-picker input value.
fn micros_to_date(ts: i64) -> String {
    match chrono::DateTime::from_timestamp_micros(ts) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

fn micros_to_datetime(ts: i64) -> String {
    match chrono::DateTime::from_timestamp_micros(ts) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => ts.to_string(),
    }
}

// ---------------------------------------------------------------------------
// page render
// ---------------------------------------------------------------------------

/// Resolve a requested title through `#REDIRECT` at τ, returning
/// `(page_id, resolved_title, redirected_from)`. Incoming underscores are
/// folded to spaces to match import's space-form title keys (fuller
/// normalization — first-letter case — is the documented import-time gap).
fn resolve_page(
    inst: &Instance,
    raw: &str,
    ts: Option<i64>,
) -> (Option<u64>, String, Option<String>) {
    let original = raw.replace('_', " ").trim().to_string();
    let mut current = original.clone();
    let mut redirected_from = None;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..=MAX_REDIRECT_HOPS {
        let pid = match inst.page_id_by_title_at(&current, ts).ok().flatten() {
            Some(id) => id,
            None => return (None, current, redirected_from),
        };
        if !seen.insert(pid) {
            return (Some(pid), current, redirected_from);
        }
        let text = match inst.page_text_at(pid, ts).ok().flatten() {
            Some(t) => t,
            None => return (Some(pid), current, redirected_from),
        };
        match wikimak_wikitext::parse_redirect(&String::from_utf8_lossy(&text)) {
            Some(target) => {
                if redirected_from.is_none() {
                    redirected_from = Some(original.clone());
                }
                current = target.replace('_', " ").trim().to_string();
            }
            None => return (Some(pid), current, redirected_from),
        }
    }
    (None, current, redirected_from)
}

fn page_response(app: &App, raw_title: &str, query: &HashMap<String, String>) -> Resp {
    let (ts, asof_query) = asof_from_query(query);
    let inst: &Instance = &app.inst;
    let view = match AsOfView::new(inst, ts) {
        Ok(v) => v,
        Err(e) => return html_resp(500, &error_shell(&format!("site config: {e}"))),
    };
    let site = view.site();

    let (page_id, resolved_title, redirected_from) = resolve_page(inst, raw_title, ts);
    let title_obj = Title::parse(&resolved_title, site);
    let display = title_obj.prefixed(site);
    let page_path = format!("/wiki/{}", wikimak_wikitext::html::encode_path(&display));

    let text = page_id.and_then(|pid| inst.page_text_at(pid, ts).ok().flatten());

    let (content, out): (String, Option<RenderOutput>) = match text {
        Some(bytes) => {
            let wikitext = String::from_utf8_lossy(&bytes);
            let invoker = LuaInvoker::new().ok();
            let media_resolver = BlobMediaResolver::new(MEDIA_ROUTE_PREFIX);
            let opts = RenderOptions {
                invoker: invoker.as_ref().map(|i| i as &dyn ModuleInvoker),
                media: Some(&media_resolver),
                link_prefix: "/wiki/".into(),
                asof_query: asof_query.clone(),
            };
            let out = render(&view, &title_obj, &wikitext, &opts);
            (out.html.clone(), Some(out))
        }
        None => (
            format!(
                r#"<p class="noarticle">There is currently no text at this title{}.</p>"#,
                if ts.is_some() { " as of this instant" } else { "" }
            ),
            None,
        ),
    };

    let mut body = String::new();
    body.push_str(&header_bar(app, site, &page_path, ts, &asof_query));
    body.push_str(&format!("<h1 class=\"page-title\">{}</h1>", escape(&display)));
    if let Some(from) = &redirected_from {
        body.push_str(&format!(
            r#"<div class="redirect-note">(redirected from <a href="/wiki/{}{}">{}</a>)</div>"#,
            wikimak_wikitext::html::encode_path(&Title::parse(from, site).prefixed(site)),
            asof_query,
            escape(from),
        ));
    }
    if let Some(out) = &out {
        body.push_str(&misses_badge(&out.misses));
    }
    body.push_str("<div class=\"content\">");
    body.push_str(&content);
    body.push_str("</div>");
    if let Some(out) = &out {
        body.push_str(&categories_footer(&out.categories, &asof_query));
    }
    body.push_str(&instance_footer(site, ts));

    html_resp(200, &shell(site, &escape(&display), &body))
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

fn history_response(app: &App, raw_title: &str, query: &HashMap<String, String>) -> Resp {
    let (ts, asof_query) = asof_from_query(query);
    let inst: &Instance = &app.inst;
    let view = match AsOfView::new(inst, ts) {
        Ok(v) => v,
        Err(e) => return html_resp(500, &error_shell(&format!("site config: {e}"))),
    };
    let site = view.site();
    let key = raw_title.replace('_', " ").trim().to_string();
    let display = Title::parse(&key, site).prefixed(site);
    let page_path = format!("/wiki/{}", wikimak_wikitext::html::encode_path(&display));

    let page_id = inst.page_id_by_title_at(&key, ts).ok().flatten();

    let mut rows = String::new();
    if let Some(pid) = page_id {
        if let Ok(hist) = inst.page_history(pid) {
            for entry in hist {
                let e = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let micros = e.meta.ts.timestamp_micros();
                let who = match &e.meta.contributor {
                    crate::ContributorMeta::Named { username, .. } => username.clone(),
                    crate::ContributorMeta::Anonymous { ip } => ip.clone(),
                    crate::ContributorMeta::Hidden => "(hidden)".to_string(),
                };
                rows.push_str(&format!(
                    r#"<li><a href="/wiki/{path}?asof={micros}">{when}</a> · rev {rev} · {len} bytes · {who}{comment}</li>"#,
                    path = wikimak_wikitext::html::encode_path(&display),
                    micros = micros,
                    when = escape(&micros_to_datetime(micros)),
                    rev = e.meta.rev_id,
                    len = e.meta.text_len,
                    who = escape(&who),
                    comment = if e.meta.comment.is_empty() {
                        String::new()
                    } else {
                        format!(" · <span class=\"comment\">{}</span>", escape(&e.meta.comment))
                    },
                ));
            }
        }
    }
    if rows.is_empty() {
        rows.push_str("<li class=\"noarticle\">No revisions.</li>");
    }

    let mut body = String::new();
    body.push_str(&header_bar(app, site, &page_path, ts, &asof_query));
    body.push_str(&format!(
        r#"<h1 class="page-title">Revision history: <a href="{path}{asof}">{disp}</a></h1>"#,
        path = page_path,
        asof = asof_query,
        disp = escape(&display),
    ));
    body.push_str(&format!("<ul class=\"history\">{rows}</ul>"));
    body.push_str(&instance_footer(site, ts));

    html_resp(200, &shell(site, &escape(&display), &body))
}

// ---------------------------------------------------------------------------
// allpages
// ---------------------------------------------------------------------------

fn allpages_response(app: &App, query: &HashMap<String, String>) -> Resp {
    let (ts, asof_query) = asof_from_query(query);
    let inst: &Instance = &app.inst;
    let view = match AsOfView::new(inst, ts) {
        Ok(v) => v,
        Err(e) => return html_resp(500, &error_shell(&format!("site config: {e}"))),
    };
    let site = view.site();
    let filter = query.get("filter").map(String::as_str).filter(|s| !s.is_empty());

    let pages = inst.pages(filter, 5000).unwrap_or_default();

    let mut rows = String::new();
    for (_id, title) in &pages {
        rows.push_str(&format!(
            r#"<li><a href="/wiki/{path}{asof}">{disp}</a></li>"#,
            path = wikimak_wikitext::html::encode_path(title),
            asof = asof_query,
            disp = escape(title),
        ));
    }
    if rows.is_empty() {
        rows.push_str("<li class=\"noarticle\">No pages.</li>");
    }

    let filter_val = filter.unwrap_or("");
    let mut body = String::new();
    body.push_str(&header_bar(app, site, "/w/allpages", ts, &asof_query));
    body.push_str("<h1 class=\"page-title\">All pages</h1>");
    body.push_str(&format!(
        r#"<form class="filter" method="get" action="/w/allpages">
             <label>Filter <input type="text" name="filter" value="{}"></label>
             {}
             <button type="submit">Go</button>
           </form>"#,
        escape(filter_val),
        if let Some(ts) = ts {
            format!(r#"<input type="hidden" name="asof" value="{}">"#, escape(&micros_to_date(ts)))
        } else {
            String::new()
        },
    ));
    body.push_str(&format!("<ul class=\"allpages\">{rows}</ul>"));
    body.push_str(&instance_footer(site, ts));

    html_resp(200, &shell(site, "All pages", &body))
}

// ---------------------------------------------------------------------------
// media
// ---------------------------------------------------------------------------

fn media_response(app: &App, raw_file: &str, query: &HashMap<String, String>) -> Resp {
    let w = query.get("w").map(String::as_str).unwrap_or("orig");
    let width = if w == "orig" { None } else { w.parse::<u32>().ok() };
    match app.media.materialize(raw_file, width) {
        Ok(path) => match std::fs::read(&path) {
            Ok(bytes) => bytes_resp(200, mime_for(raw_file), bytes),
            Err(_) => placeholder_svg(raw_file),
        },
        // Miss / offline / not-found → inline placeholder, HTTP 200 so the
        // embedding page stays clean (plan §4 offline rendering).
        Err(_) => placeholder_svg(raw_file),
    }
}

fn mime_for(file: &str) -> &'static str {
    let lower = file.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ogg" | "oga" => "audio/ogg",
        "ogv" => "video/ogg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn placeholder_svg(file: &str) -> Resp {
    let label = escape(file);
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="90" viewBox="0 0 120 90">
  <rect width="120" height="90" fill="#e8e8e8" stroke="#bbb"/>
  <text x="60" y="42" font-family="sans-serif" font-size="9" fill="#666" text-anchor="middle">no media</text>
  <text x="60" y="56" font-family="sans-serif" font-size="7" fill="#999" text-anchor="middle">{label}</text>
</svg>"##
    );
    bytes_resp(200, "image/svg+xml", svg.into_bytes())
}

// ---------------------------------------------------------------------------
// HTML shell + chrome
// ---------------------------------------------------------------------------

fn shell(site: &wikimak_wikitext::SiteConfig, title: &str, body: &str) -> String {
    let dir = if site.rtl { " dir=\"rtl\"" } else { "" };
    let lang = if site.lang.is_empty() {
        String::new()
    } else {
        format!(" lang=\"{}\"", escape(&site.lang))
    };
    format!(
        "<!doctype html>\n<html{lang}{dir}>\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>{css}</style>\n</head>\n<body>\n{body}\n</body>\n</html>\n",
        css = CSS,
    )
}

fn error_shell(msg: &str) -> String {
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n\
         <style>{CSS}</style>\n</head>\n<body>\n<div class=\"error\">{}</div>\n</body>\n</html>\n",
        escape(msg),
    )
}

fn header_bar(
    app: &App,
    _site: &wikimak_wikitext::SiteConfig,
    page_path: &str,
    ts: Option<i64>,
    asof_query: &str,
) -> String {
    let _ = app;
    let date_val = ts.map(micros_to_date).unwrap_or_default();
    // The date form GETs back to THIS page, preserving the current path.
    format!(
        r#"<header class="bar">
  <nav>
    <a href="/w/allpages{asof}">All pages</a>
    <a href="{hist}{asof}">History</a>
  </nav>
  <form class="asof" method="get" action="{action}">
    <label>As of <input type="date" name="asof" value="{date}"></label>
    <button type="submit">Go</button>
    {now}
  </form>
</header>"#,
        asof = asof_query,
        hist = page_path.replacen("/wiki/", "/w/history/", 1),
        action = page_path,
        date = escape(&date_val),
        now = if ts.is_some() {
            format!(r#"<a class="now" href="{page_path}">now</a>"#)
        } else {
            String::new()
        },
    )
}

fn misses_badge(misses: &wikimak_wikitext::RenderMisses) -> String {
    let n = misses.unknown_tags.len()
        + misses.failed_invokes.len()
        + misses.missing_templates.len()
        + misses.missing_media.len();
    if n == 0 {
        return String::new();
    }
    let mut detail = Vec::new();
    if !misses.unknown_tags.is_empty() {
        detail.push(format!("unknown tags: {}", misses.unknown_tags.join(", ")));
    }
    if !misses.failed_invokes.is_empty() {
        detail.push(format!("failed invokes: {}", misses.failed_invokes.join(", ")));
    }
    if !misses.missing_templates.is_empty() {
        detail.push(format!("missing templates: {}", misses.missing_templates.join(", ")));
    }
    if !misses.missing_media.is_empty() {
        detail.push(format!("missing media: {}", misses.missing_media.join(", ")));
    }
    format!(
        r#"<div class="misses" title="{}">{} render miss{}</div>"#,
        escape(&detail.join(" · ")),
        n,
        if n == 1 { "" } else { "es" },
    )
}

fn categories_footer(categories: &[String], asof_query: &str) -> String {
    if categories.is_empty() {
        return String::new();
    }
    let links: Vec<String> = categories
        .iter()
        .map(|c| {
            format!(
                r#"<a href="/w/allpages?filter={}{}">{}</a>"#,
                urlq(c),
                if asof_query.is_empty() {
                    String::new()
                } else {
                    format!("&asof={}", &asof_query[6..]) // strip leading "?asof="
                },
                escape(c),
            )
        })
        .collect();
    format!(
        r#"<div class="catlinks"><span class="catlabel">Categories:</span> {}</div>"#,
        links.join(" · ")
    )
}

fn instance_footer(site: &wikimak_wikitext::SiteConfig, ts: Option<i64>) -> String {
    let name = if !site.site_name.is_empty() {
        site.site_name.clone()
    } else if !site.db_name.is_empty() {
        site.db_name.clone()
    } else {
        "wiki".to_string()
    };
    let tau = match ts {
        Some(ts) => format!("τ = {}", micros_to_datetime(ts)),
        None => "τ = now (head)".to_string(),
    };
    format!(
        r#"<footer class="site"><span>{}</span> · <span>{}</span></footer>"#,
        escape(&name),
        escape(&tau),
    )
}

fn not_found_page(app: &App, query: &HashMap<String, String>) -> Resp {
    let (ts, _asof) = asof_from_query(query);
    let body = match AsOfView::new(&app.inst, ts) {
        Ok(view) => {
            let site = view.site();
            format!(
                "{}<h1 class=\"page-title\">Not found</h1><p>No such route.</p>{}",
                header_bar(app, site, "/w/allpages", ts, ""),
                instance_footer(site, ts),
            )
        }
        Err(_) => "<h1>Not found</h1>".to_string(),
    };
    html_resp(404, &shell(&wikimak_wikitext::SiteConfig::default(), "Not found", &body))
}

// ---------------------------------------------------------------------------
// response builders + url helpers
// ---------------------------------------------------------------------------

fn html_resp(code: u16, html: &str) -> Resp {
    Response::from_data(html.as_bytes().to_vec())
        .with_status_code(code)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
}

fn bytes_resp(code: u16, mime: &str, bytes: Vec<u8>) -> Resp {
    Response::from_data(bytes)
        .with_status_code(code)
        .with_header(header("Content-Type", mime))
}

fn text_resp(code: u16, msg: &str) -> Resp {
    Response::from_data(msg.as_bytes().to_vec())
        .with_status_code(code)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

fn redirect(location: &str) -> Resp {
    Response::from_data(Vec::new())
        .with_status_code(302)
        .with_header(header("Location", location))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header name/value are valid ASCII")
}

/// Percent-decode a URL path/segment (`%XX` → byte, `+` left as-is in a
/// path). Lossy UTF-8 on the decoded bytes.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an `application/x-www-form-urlencoded` query into a map (last
/// value wins). `+` → space in values; `%XX` decoded.
fn parse_query(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(query_decode(k), query_decode(v));
    }
    out
}

fn query_decode(s: &str) -> String {
    percent_decode(&s.replace('+', " "))
}

/// Percent-encode a query value: keep RFC 3986 unreserved bytes, `%XX`
/// everything else. Enough for filter substrings and asof values.
fn urlq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

const CSS: &str = r#"
:root { color-scheme: light; }
* { box-sizing: border-box; }
body {
  font-family: Georgia, 'Times New Roman', serif;
  line-height: 1.6; color: #202122; background: #fff;
  margin: 0; padding: 0 0 3rem;
}
.bar {
  display: flex; flex-wrap: wrap; gap: 1rem; align-items: center;
  justify-content: space-between;
  padding: 0.6rem 1rem; background: #f6f6f6; border-bottom: 1px solid #a2a9b1;
  font-family: sans-serif; font-size: 0.9rem;
}
.bar nav a { margin-right: 1rem; }
.bar .asof { display: flex; gap: 0.4rem; align-items: center; }
.bar .now { margin-left: 0.5rem; }
a { color: #3366cc; text-decoration: none; }
a:hover { text-decoration: underline; }
a.new, .new a { color: #ba0000; }
h1.page-title {
  font-family: 'Linux Libertine', Georgia, serif; font-weight: normal;
  border-bottom: 1px solid #a2a9b1; margin: 1rem 1rem 0.5rem; padding-bottom: 0.2rem;
}
.content, .redirect-note, .misses, .catlinks, .allpages, .history, .filter, .noarticle {
  margin-left: 1rem; margin-right: 1rem;
}
.redirect-note { color: #54595d; font-style: italic; margin-bottom: 0.5rem; }
.misses {
  display: inline-block; font-family: sans-serif; font-size: 0.8rem;
  background: #fef6e7; border: 1px solid #edab00; border-radius: 2px;
  padding: 0.1rem 0.5rem; margin-bottom: 0.6rem; cursor: help; color: #71570b;
}
.content table {
  border-collapse: collapse; margin: 0.5rem 0;
}
.content table.infobox, .content .infobox {
  float: right; clear: right; width: 22em; margin: 0 0 1rem 1rem;
  background: #f8f9fa; border: 1px solid #a2a9b1; font-size: 0.88rem;
  font-family: sans-serif;
}
.content .infobox td, .content .infobox th { padding: 0.25rem 0.5rem; vertical-align: top; border: 1px solid #eaecf0; }
.content table td, .content table th { border: 1px solid #a2a9b1; padding: 0.3rem 0.6rem; }
.content th { background: #eaecf0; }
.error, span.error {
  color: #d33; border: 1px solid #d33; background: #fff0f0;
  padding: 0 0.3rem; border-radius: 2px; font-family: sans-serif; font-size: 0.9rem;
}
.catlinks {
  margin-top: 1.5rem; padding: 0.4rem 1rem; border-top: 1px solid #a2a9b1;
  font-family: sans-serif; font-size: 0.85rem;
}
.catlabel { font-weight: bold; }
ul.allpages, ul.history { font-family: sans-serif; font-size: 0.9rem; }
.history .comment { color: #54595d; font-style: italic; }
.filter { font-family: sans-serif; font-size: 0.9rem; margin-bottom: 0.8rem; }
footer.site {
  margin: 2rem 1rem 0; padding-top: 0.6rem; border-top: 1px solid #a2a9b1;
  font-family: sans-serif; font-size: 0.8rem; color: #54595d;
}
[dir="rtl"] .content .infobox { float: left; margin: 0 1rem 1rem 0; }
"#;
