//! Stable, engine-owned HTTP entrance to locally readable archives.
//!
//! Wikipedia rendering is heavy enough to remain a specialized subprocess,
//! started on first use. Its private port never escapes into links or browser
//! history: this gateway is the public address for every archive.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tiny_http::{Header, Method, Response, Server};

pub const DEFAULT_ADDR: &str = "127.0.0.1:8642";

pub fn address() -> String {
    std::env::var("SARUN_LIBRARY_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.into())
}

pub fn browser_base_url() -> String {
    let addr = address();
    #[cfg(target_os = "macos")]
    let addr = addr.replacen("127.0.0.1", "10.0.2.2", 1);
    format!("http://{addr}")
}

struct WikiBackend {
    child: Child,
    addr: String,
}

#[derive(Default)]
struct Backends {
    wiki: HashMap<String, WikiBackend>,
}

impl Drop for Backends {
    fn drop(&mut self) {
        for (_, mut backend) in self.wiki.drain() {
            let _ = backend.child.kill();
            let _ = backend.child.wait();
        }
    }
}

pub struct Gateway {
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    workers: Vec<thread::JoinHandle<()>>,
    backends: Arc<Mutex<Backends>>,
    enhancer: Arc<crate::realplksr::Enhancer>,
}

impl Gateway {
    pub fn start(self_exe: String) -> Result<Self, String> {
        let addr = address();
        let server = Arc::new(
            Server::http(&addr)
                .map_err(|error| format!("archive gateway cannot bind http://{addr}: {error}"))?,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let backends = Arc::new(Mutex::new(Backends::default()));
        let enhancer = Arc::new(crate::realplksr::Enhancer::new());
        let mut workers = Vec::new();
        for _ in 0..4 {
            let server = Arc::clone(&server);
            let stop = Arc::clone(&stop);
            let backends = Arc::clone(&backends);
            let enhancer = Arc::clone(&enhancer);
            let self_exe = self_exe.clone();
            workers.push(thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let request = match server.recv_timeout(Duration::from_millis(250)) {
                        Ok(Some(request)) => request,
                        Ok(None) => continue,
                        Err(_) if stop.load(Ordering::Acquire) => break,
                        Err(error) => {
                            eprintln!("sarun library: receive failed: {error}");
                            continue;
                        }
                    };
                    let response = handle(
                        request.method(),
                        request.url(),
                        &self_exe,
                        &backends,
                        &enhancer,
                    );
                    let _ = request.respond(response);
                }
            }));
        }
        eprintln!("sarun library: http://{addr}/");
        Ok(Self {
            server,
            stop,
            workers,
            backends,
            enhancer,
        })
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        self.server.unblock();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        drop(self.backends);
        drop(self.enhancer);
    }
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn handle(
    method: &Method,
    url: &str,
    self_exe: &str,
    backends: &Arc<Mutex<Backends>>,
    enhancer: &crate::realplksr::Enhancer,
) -> Resp {
    if *method != Method::Get {
        return text(405, "method not allowed");
    }
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    if path == "/" {
        return home();
    }
    if path == "/rfc" || path.starts_with("/rfc/") {
        return rfc(path);
    }
    if path == "/warc" || path.starts_with("/warc/") {
        return warc(path);
    }
    let Some((dbname, rest)) = path.trim_start_matches('/').split_once('/') else {
        return wiki_redirect_or_missing(path);
    };
    let jobs = match crate::mirrors::library_jobs() {
        Ok(jobs) => jobs,
        Err(error) => return text(500, &format!("mirror inventory: {error}")),
    };
    let Some(job) = jobs
        .iter()
        .find(|job| job.kind == "wiki" && job.src == dbname)
    else {
        return text(404, "no such local archive");
    };
    let private_path = if query.is_empty() {
        format!("/{rest}")
    } else {
        format!("/{rest}?{query}")
    };
    let public_route = canonical_public_route(path, query);
    let cache_route = generation_cache_route(&job.dest, &public_route);
    if rest.starts_with("w/media/") {
        if let Some(enhanced) = enhancer.cached(&cache_route) {
            return enhanced_image(enhanced.as_ref().clone());
        }
    }
    match proxy_wiki(
        self_exe,
        &job.src,
        &job.dest,
        &private_path,
        &cache_route,
        backends,
        enhancer,
    ) {
        Ok(response) => response,
        Err(error) => text(503, &error),
    }
}

fn wiki_redirect_or_missing(path: &str) -> Resp {
    let dbname = path.trim_matches('/');
    let exists = crate::mirrors::library_jobs()
        .ok()
        .is_some_and(|jobs| jobs.iter().any(|job| job.kind == "wiki" && job.src == dbname));
    if exists {
        redirect(&format!("/{dbname}/"))
    } else {
        text(404, "no such local archive")
    }
}

fn proxy_wiki(
    self_exe: &str,
    dbname: &str,
    root: &str,
    path: &str,
    public_route: &str,
    backends: &Arc<Mutex<Backends>>,
    enhancer: &crate::realplksr::Enhancer,
) -> Result<Resp, String> {
    let addr = ensure_wiki_backend(self_exe, dbname, root, backends)?;
    let mut stream =
        TcpStream::connect(&addr).map_err(|error| format!("{dbname} renderer: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|error| error.to_string())?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| format!("{dbname} renderer request: {error}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| format!("{dbname} renderer response: {error}"))?;
    proxy_response(dbname, public_route, raw, enhancer)
}

fn ensure_wiki_backend(
    self_exe: &str,
    dbname: &str,
    root: &str,
    backends: &Arc<Mutex<Backends>>,
) -> Result<String, String> {
    let mut backends = backends.lock().expect("archive backend registry poisoned");
    if let Some(backend) = backends.wiki.get_mut(dbname) {
        if backend.child.try_wait().ok().flatten().is_none() {
            return Ok(backend.addr.clone());
        }
        backends.wiki.remove(dbname);
    }
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|error| format!("reserve renderer port: {error}"))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("read renderer port: {error}"))?
        .to_string();
    drop(listener);
    let mut child = Command::new(self_exe)
        .args(["wikimak", "serve", root, &addr])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start {dbname} renderer: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(&addr).is_ok() {
            break;
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect {dbname} renderer: {error}"))?
        {
            return Err(format!("{dbname} renderer exited before startup ({status})"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{dbname} renderer did not start within 10 seconds"));
        }
        thread::sleep(Duration::from_millis(50));
    }
    backends.wiki.insert(
        dbname.into(),
        WikiBackend {
            child,
            addr: addr.clone(),
        },
    );
    Ok(addr)
}

fn proxy_response(
    dbname: &str,
    public_route: &str,
    raw: Vec<u8>,
    enhancer: &crate::realplksr::Enhancer,
) -> Result<Resp, String> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| format!("{dbname} renderer returned malformed HTTP"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("{dbname} renderer returned malformed status"))?;
    let mut headers = Vec::new();
    let mut mime = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if ["content-length", "transfer-encoding", "connection"]
            .iter()
            .any(|drop| name.eq_ignore_ascii_case(drop))
        {
            continue;
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-type") {
            mime = value.to_string();
            continue;
        }
        let value = if name.eq_ignore_ascii_case("location") && value.starts_with('/') {
            format!("/{dbname}{value}")
        } else {
            value.to_string()
        };
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            headers.push(header);
        }
    }
    let mut body = raw[split + 4..].to_vec();
    if mime.starts_with("text/html") {
        body = rewrite_wiki_html(dbname, &String::from_utf8_lossy(&body)).into_bytes();
    } else if status == 200 && mime.starts_with("image/") {
        let state = match enhancer.image(public_route, &mime, &body) {
            crate::realplksr::Enhancement::Ready(enhanced) => {
                body = enhanced.as_ref().clone();
                mime = "image/jpeg".into();
                "ready"
            }
            crate::realplksr::Enhancement::Pending => "processing",
            crate::realplksr::Enhancement::Original => "original",
        };
        headers.push(header("X-Sarun-Enhanced", state));
        headers.push(header("Cache-Control", "no-store"));
    }
    if !mime.is_empty() {
        headers.push(header("Content-Type", &mime));
    }
    let mut response = Response::from_data(body).with_status_code(status);
    for header in headers {
        response = response.with_header(header);
    }
    Ok(response)
}

fn rewrite_wiki_html(dbname: &str, html: &str) -> String {
    let base = format!("/{dbname}");
    let mut out = html
        .replace("href=\"/wiki/", &format!("href=\"{base}/wiki/"))
        .replace("href=\"/w/", &format!("href=\"{base}/w/"))
        .replace("src=\"/w/", &format!("src=\"{base}/w/"))
        .replace("action=\"/wiki/", &format!("action=\"{base}/wiki/"))
        .replace("action=\"/w/", &format!("action=\"{base}/w/"))
        .replace("href=\"/\"", &format!("href=\"{base}/\""));
    if let Ok(jobs) = crate::mirrors::library_jobs() {
        for job in jobs.into_iter().filter(|job| job.kind == "wiki") {
            for host in wiki_hosts(&job.src) {
                let local = format!("/{}/wiki/", job.src);
                out = out.replace(&format!("https://{host}/wiki/"), &local);
                out = out.replace(&format!("http://{host}/wiki/"), &local);
                out = out.replace(&format!("//{host}/wiki/"), &local);
            }
        }
    }
    let image_upgrade = r#"<script>
(() => {
  const retry = (img, attempt = 0) => {
    if (attempt > 180 || !img.isConnected) return;
    const url = new URL(img.src);
    url.searchParams.set("sarun_enhanced", "1");
    fetch(url, {cache: "no-store"}).then(response => {
      const state = response.headers.get("X-Sarun-Enhanced");
      if (state === "ready") return response.blob().then(blob => {
        const old = img.dataset.sarunBlob;
        const next = URL.createObjectURL(blob);
        img.src = next;
        img.dataset.sarunBlob = next;
        if (old) URL.revokeObjectURL(old);
      });
      if (state === "processing")
        setTimeout(() => retry(img, attempt + 1), 1500);
    }).catch(() => {});
  };
  for (const img of document.querySelectorAll('img[src*="/w/media/"]')) {
    if (img.complete) retry(img);
    else img.addEventListener("load", () => retry(img), {once: true});
  }
})();
</script>"#;
    if let Some(position) = out.rfind("</body>") {
        out.insert_str(position, image_upgrade);
    }
    out
}

fn canonical_public_route(path: &str, query: &str) -> String {
    let query = query
        .split('&')
        .filter(|part| !part.starts_with("sarun_enhanced="))
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

fn generation_cache_route(root: &str, public_route: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    if let Ok(metadata) = std::fs::metadata(root) {
        metadata.len().hash(&mut hasher);
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .hash(&mut hasher);
    }
    format!("{:016x}:{public_route}", hasher.finish())
}

fn wiki_hosts(dbname: &str) -> Vec<String> {
    match dbname {
        "commonswiki" => vec!["commons.wikimedia.org".into()],
        "wikidatawiki" => vec!["www.wikidata.org".into(), "wikidata.org".into()],
        "mediawikiwiki" => vec!["www.mediawiki.org".into(), "mediawiki.org".into()],
        name if name.ends_with("wiki") => {
            vec![format!("{}.wikipedia.org", &name[..name.len() - 4])]
        }
        _ => Vec::new(),
    }
}

fn rfc(path: &str) -> Resp {
    let jobs = match crate::mirrors::library_jobs() {
        Ok(jobs) => jobs,
        Err(error) => return text(500, &format!("mirror inventory: {error}")),
    };
    let Some(job) = jobs.iter().find(|job| job.kind == "ietf") else {
        return text(404, "no IETF mirror is configured");
    };
    let mirror = match ietf_mirror::Mirror::open_read(ietf_mirror::MirrorConfig::new(
        job.dest.clone().into(),
    )) {
        Ok(mirror) => mirror,
        Err(error) => return text(503, &format!("IETF mirror unavailable: {error}")),
    };
    let tail = path.trim_start_matches("/rfc").trim_matches('/');
    if tail.is_empty() {
        let drafts = match mirror.drafts() {
            Ok(drafts) => drafts,
            Err(error) => return text(500, &error.to_string()),
        };
        let items = drafts
            .iter()
            .map(|draft| format!(r#"<li><a href="/rfc/{draft}">{draft}</a></li>"#))
            .collect::<String>();
        return html(200, &page("IETF drafts", &format!("<h1>IETF drafts</h1><ul>{items}</ul>")));
    }
    let mut parts = tail.split('/');
    let draft = percent_decode(parts.next().unwrap_or_default());
    let revision = parts.next().map(percent_decode);
    let entry = match revision {
        Some(revision) => mirror.revision(&draft, &revision),
        None => mirror.head(&draft),
    };
    match entry {
        Ok(Some(entry)) => bytes(200, "text/plain; charset=utf-8", entry.text),
        Ok(None) => text(404, "no such draft revision"),
        Err(error) => text(500, &error.to_string()),
    }
}

fn warc(path: &str) -> Resp {
    let tail = path.trim_start_matches("/warc").trim_matches('/');
    if tail.is_empty() {
        let mut items = String::new();
        for (id, session) in crate::discover::discover() {
            if crate::discover::webcap_typed(id)
                .ok()
                .is_some_and(|rows| !rows.is_empty())
            {
                let label = if session.name.is_empty() {
                    id.to_string()
                } else {
                    format!("{} ({id})", escape(&session.name))
                };
                items.push_str(&format!(r#"<li><a href="/warc/{id}/">{label}</a></li>"#));
            }
        }
        return html(
            200,
            &page("Web archives", &format!("<h1>Web archives</h1><ul>{items}</ul>")),
        );
    }
    let mut parts = tail.split('/');
    let Some(box_id) = parts.next().and_then(|part| part.parse::<i64>().ok()) else {
        return text(400, "invalid archive id");
    };
    if let Some(row_id) = parts.next().and_then(|part| part.parse::<u64>().ok()) {
        return match crate::discover::webcap_detail_typed(box_id, row_id) {
            Ok(Some(capture)) => bytes(
                capture.summary.status,
                capture.summary.mime.as_str(),
                capture.response_body.as_slice().to_vec(),
            ),
            Ok(None) => text(404, "no such captured response"),
            Err(error) => text(500, &error),
        };
    }
    let rows = match crate::discover::webcap_typed(box_id) {
        Ok(rows) => rows,
        Err(error) => return text(500, &error),
    };
    let items = rows
        .iter()
        .map(|row| {
            format!(
                r#"<li><a href="/warc/{box_id}/{}">{} {}</a> <small>{} · {} bytes</small></li>"#,
                row.id,
                row.status,
                escape(row.url.as_str()),
                escape(row.mime.as_str()),
                row.response_length,
            )
        })
        .collect::<String>();
    html(
        200,
        &page(
            "Captured responses",
            &format!("<h1>Captured responses in box {box_id}</h1><ul>{items}</ul>"),
        ),
    )
}

fn home() -> Resp {
    let jobs = crate::mirrors::library_jobs().unwrap_or_default();
    let mut wiki = String::new();
    let mut ietf = false;
    for job in jobs {
        match job.kind.as_str() {
            "wiki" => wiki.push_str(&format!(
                r#"<li><a href="/{0}/">{0}</a> <small>{1}</small></li>"#,
                escape(&job.src),
                escape(&job.dest),
            )),
            "ietf" => ietf = true,
            _ => {}
        }
    }
    let ietf = if ietf {
        r#"<li><a href="/rfc/">IETF drafts and RFC work</a></li>"#
    } else {
        ""
    };
    html(
        200,
        &page(
            "Sarun archive library",
            &format!(
                "<h1>Sarun archive library</h1><h2>Wikis</h2><ul>{wiki}</ul>\
                 <h2>Other archives</h2><ul>{ietf}<li><a href=\"/warc/\">Web captures / WARC</a></li></ul>"
            ),
        ),
    )
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>body{{font:16px system-ui,sans-serif;max-width:72rem;margin:2rem auto;padding:0 1rem}}\
         li{{margin:.35rem 0}}small{{color:#666}}</style></head><body>{body}</body></html>",
        escape(title),
    )
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid HTTP header")
}

fn bytes(status: u16, mime: &str, body: Vec<u8>) -> Resp {
    let status = if (100..=599).contains(&status) {
        status
    } else {
        502
    };
    let mime = Header::from_bytes(b"Content-Type", mime.as_bytes())
        .unwrap_or_else(|_| header("Content-Type", "application/octet-stream"));
    Response::from_data(body)
        .with_status_code(status)
        .with_header(mime)
}

fn enhanced_image(body: Vec<u8>) -> Resp {
    bytes(200, "image/jpeg", body)
        .with_header(header("X-Sarun-Enhanced", "ready"))
        .with_header(header("Cache-Control", "no-store"))
}

fn text(status: u16, body: &str) -> Resp {
    bytes(status, "text/plain; charset=utf-8", body.as_bytes().to_vec())
}

fn html(status: u16, body: &str) -> Resp {
    bytes(status, "text/html; charset=utf-8", body.as_bytes().to_vec())
}

fn redirect(location: &str) -> Resp {
    Response::from_data(Vec::new())
        .with_status_code(302)
        .with_header(header("Location", location))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_wiki_links_stay_beneath_gateway_mount() {
        let source = r#"<a href="/wiki/A">A</a><a href="/w/allpages">all</a><img src="/w/media/X">"#;
        let rewritten = rewrite_wiki_html("lvwiki", source);
        assert!(rewritten.contains(r#"href="/lvwiki/wiki/A""#));
        assert!(rewritten.contains(r#"href="/lvwiki/w/allpages""#));
        assert!(rewritten.contains(r#"src="/lvwiki/w/media/X""#));
    }

    #[test]
    fn known_wikimedia_hosts_have_stable_local_routes() {
        assert_eq!(wiki_hosts("lvwiki"), vec!["lv.wikipedia.org"]);
        assert_eq!(wiki_hosts("commonswiki"), vec!["commons.wikimedia.org"]);
    }

    #[test]
    fn enhancement_poll_marker_is_not_part_of_the_ram_cache_key() {
        assert_eq!(
            canonical_public_route(
                "/lvwiki/w/media/Riga.jpg",
                "w=250&sarun_enhanced=1"
            ),
            "/lvwiki/w/media/Riga.jpg?w=250"
        );
    }

    #[test]
    fn rendered_wiki_pages_poll_for_ram_only_image_replacements() {
        let rewritten = rewrite_wiki_html(
            "lvwiki",
            r#"<html><body><img src="/w/media/Riga.jpg?w=250"></body></html>"#,
        );
        assert!(rewritten.contains(r#"src="/lvwiki/w/media/Riga.jpg?w=250""#));
        assert!(rewritten.contains("sarun_enhanced"));
        assert!(rewritten.contains("cache: \"no-store\""));
    }
}
