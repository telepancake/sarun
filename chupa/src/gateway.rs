//! Stable, Chupa-owned HTTP entrance to locally readable archives.
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
const ARCHIVE_GATEWAY_ADDR_ENV: &str = "CHUPA_GATEWAY_ADDR";
const LEGACY_ARCHIVE_GATEWAY_ADDR_ENV: &str = "SARUN_ARCHIVE_GATEWAY_ADDR";

fn archive_gateway_addr() -> String {
    std::env::var(ARCHIVE_GATEWAY_ADDR_ENV)
        .or_else(|_| std::env::var(LEGACY_ARCHIVE_GATEWAY_ADDR_ENV))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ADDR.to_owned())
}

#[derive(Clone, Debug)]
pub struct CaptureArchive {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct CaptureRow {
    pub id: u64,
    pub status: u16,
    pub url: String,
    pub mime: String,
    pub response_length: u64,
}

#[derive(Clone, Debug)]
pub struct CaptureDetail {
    pub status: u16,
    pub mime: String,
    pub response_body: Vec<u8>,
}

pub trait CaptureProvider: Send + Sync {
    fn archives(&self) -> Result<Vec<CaptureArchive>, String>;
    fn rows(&self, archive: i64) -> Result<Vec<CaptureRow>, String>;
    fn detail(&self, archive: i64, row: u64) -> Result<Option<CaptureDetail>, String>;
}

#[derive(Default)]
pub struct EmptyCaptureProvider;

impl CaptureProvider for EmptyCaptureProvider {
    fn archives(&self) -> Result<Vec<CaptureArchive>, String> {
        Ok(Vec::new())
    }

    fn rows(&self, _archive: i64) -> Result<Vec<CaptureRow>, String> {
        Ok(Vec::new())
    }

    fn detail(&self, _archive: i64, _row: u64) -> Result<Option<CaptureDetail>, String> {
        Ok(None)
    }
}

pub enum Enhancement {
    Ready(Vec<u8>),
    Pending,
    Original,
}

pub trait ImageEnhancer: Send + Sync {
    fn cached(&self, route: &str) -> Option<Vec<u8>>;
    fn image(&self, route: &str, mime: &str, body: &[u8]) -> Enhancement;
}

#[derive(Default)]
pub struct OriginalImages;

impl ImageEnhancer for OriginalImages {
    fn cached(&self, _route: &str) -> Option<Vec<u8>> {
        None
    }

    fn image(&self, _route: &str, _mime: &str, _body: &[u8]) -> Enhancement {
        Enhancement::Original
    }
}

pub struct GatewayServices {
    pub captures: Arc<dyn CaptureProvider>,
    pub enhancer: Arc<dyn ImageEnhancer>,
}

impl Default for GatewayServices {
    fn default() -> Self {
        Self {
            captures: Arc::new(EmptyCaptureProvider),
            enhancer: Arc::new(OriginalImages),
        }
    }
}

pub fn host_base_url() -> String {
    format!("http://{}", archive_gateway_addr())
}

pub fn browser_base_url() -> String {
    let addr = archive_gateway_addr();
    #[cfg(target_os = "macos")]
    let addr = addr.replacen("127.0.0.1", "10.0.2.2", 1);
    format!("http://{addr}")
}

struct WikiBackend {
    child: Child,
    addr: String,
    media_state: PackedMediaState,
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
    services: Arc<GatewayServices>,
}

impl Gateway {
    pub fn start(self_exe: String) -> Result<Self, String> {
        Self::start_with(self_exe, GatewayServices::default())
    }

    pub fn start_with(self_exe: String, services: GatewayServices) -> Result<Self, String> {
        let addr = archive_gateway_addr();
        let server = Arc::new(
            Server::http(&addr)
                .map_err(|error| format!("archive gateway cannot bind http://{addr}: {error}"))?,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let backends = Arc::new(Mutex::new(Backends::default()));
        let services = Arc::new(services);
        let mut workers = Vec::new();
        for _ in 0..4 {
            let server = Arc::clone(&server);
            let stop = Arc::clone(&stop);
            let backends = Arc::clone(&backends);
            let services = Arc::clone(&services);
            let self_exe = self_exe.clone();
            workers.push(thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let request = match server.recv_timeout(Duration::from_millis(250)) {
                        Ok(Some(request)) => request,
                        Ok(None) => continue,
                        Err(_) if stop.load(Ordering::Acquire) => break,
                        Err(error) => {
                            eprintln!("chupa library: receive failed: {error}");
                            continue;
                        }
                    };
                    let response = handle(
                        request.method(),
                        request.url(),
                        &self_exe,
                        &backends,
                        &services,
                    );
                    let _ = request.respond(response);
                }
            }));
        }
        eprintln!("chupa library: http://{addr}/");
        Ok(Self {
            server,
            stop,
            workers,
            backends,
            services,
        })
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        self.server.unblock();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        drop(self.backends);
        drop(self.services);
    }
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn handle(
    method: &Method,
    url: &str,
    self_exe: &str,
    backends: &Arc<Mutex<Backends>>,
    services: &GatewayServices,
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
        return warc(path, services.captures.as_ref());
    }
    let Some((dbname, rest)) = path.trim_start_matches('/').split_once('/') else {
        return wiki_redirect_or_missing(path);
    };
    let jobs = match crate::supervisor::library_jobs() {
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
    let (addr, media_fingerprint) = match ensure_wiki_backend(
        self_exe,
        &job.src,
        &job.dest,
        backends,
    ) {
        Ok(admission) => admission,
        Err(error) => return text(503, &error),
    };
    // Compute the cache generation after backend admission.  If publication
    // retired an old renderer above, this response must not consult the old
    // renderer generation's enhanced image entry.
    let cache_route = generation_cache_route(&job.dest, &public_route, media_fingerprint);
    if rest.starts_with("w/media/") {
        if let Some(enhanced) = services.enhancer.cached(&cache_route) {
            return enhanced_image(enhanced);
        }
    }
    match proxy_wiki(
        &job.src,
        &addr,
        &private_path,
        &cache_route,
        services.enhancer.as_ref(),
    ) {
        Ok(response) => response,
        Err(error) => text(503, &error),
    }
}

fn wiki_redirect_or_missing(path: &str) -> Resp {
    let dbname = path.trim_matches('/');
    let exists = crate::supervisor::library_jobs()
        .ok()
        .is_some_and(|jobs| jobs.iter().any(|job| job.kind == "wiki" && job.src == dbname));
    if exists {
        redirect(&format!("/{dbname}/"))
    } else {
        text(404, "no such local archive")
    }
}

fn proxy_wiki(
    dbname: &str,
    addr: &str,
    path: &str,
    public_route: &str,
    enhancer: &dyn ImageEnhancer,
) -> Result<Resp, String> {
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

fn packed_media_root(root: &str) -> Option<std::path::PathBuf> {
    wikimak_wikipedia::resolve_packed_media_path(std::path::Path::new(root), None)
}

fn media_data_entry_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    std::path::Path::new(name.as_ref())
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("data")
        && name.starts_with("media-")
}

fn media_alias_entry_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    let Some(part) = name
        .strip_prefix("media-alias-")
        .and_then(|value| value.strip_suffix(".aliases"))
    else {
        return false;
    };
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

fn hash_metadata<H: Hasher>(metadata: &std::fs::Metadata, hasher: &mut H) {
    metadata.len().hash(hasher);
    metadata.is_dir().hash(hasher);
    metadata.is_file().hash(hasher);
    metadata.is_symlink().hash(hasher);
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .hash(hasher);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.dev().hash(hasher);
        metadata.ino().hash(hasher);
        metadata.nlink().hash(hasher);
    }
}

fn hash_metadata_identity<H: Hasher>(metadata: &std::fs::Metadata, hasher: &mut H) {
    metadata.is_dir().hash(hasher);
    metadata.is_file().hash(hasher);
    metadata.is_symlink().hash(hasher);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.dev().hash(hasher);
        metadata.ino().hash(hasher);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackedMediaPathSignal {
    len: u64,
    modified: Option<std::time::SystemTime>,
    is_dir: bool,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackedMediaPublicationSignal {
    shared: Option<PackedMediaPathSignal>,
    legacy: Option<PackedMediaPathSignal>,
}

fn packed_media_path_signal(path: &std::path::Path) -> Option<PackedMediaPathSignal> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(PackedMediaPathSignal {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        is_dir: metadata.is_dir(),
        #[cfg(unix)]
        dev: {
            use std::os::unix::fs::MetadataExt;
            metadata.dev()
        },
        #[cfg(unix)]
        ino: {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        },
    })
}

/// Cheap publication signal for the repositories that automatic serving may
/// select. `MediaRepositoryWriter` publishes immutable parts by renaming the
/// companions first and the discoverable `.data` file last, syncing the
/// repository directory after each part. The directory metadata therefore
/// changes at the publication boundary without enumerating its children.
///
/// This deliberately does not validate either repository. Validation and the
/// full child fingerprint below are cold-start or publication-refresh work;
/// the unchanged-signal request path performs only these bounded metadata
/// lookups. The writer never edits a published part in place.
fn packed_media_publication_signal(root: &str) -> PackedMediaPublicationSignal {
    let archive = std::path::Path::new(root);
    PackedMediaPublicationSignal {
        shared: packed_media_path_signal(&wikimak_wikipedia::shared_packed_media_path(archive)),
        legacy: packed_media_path_signal(&archive.with_extension("media")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackedMediaState {
    signal: PackedMediaPublicationSignal,
    fingerprint: Option<u64>,
}

impl PackedMediaState {
    fn new(root: &str) -> Self {
        Self::new_with(root, packed_media_fingerprint)
    }

    fn new_with(root: &str, full_scan: impl FnOnce(&str) -> Option<u64>) -> Self {
        let signal = packed_media_publication_signal(root);
        let fingerprint = full_scan(root);
        // Keep the pre-scan signal. If publication races the scan, the next
        // request sees the changed signal and refreshes instead of pairing a
        // new signal with the old scan result.
        Self {
            signal,
            fingerprint,
        }
    }

    fn refresh(&mut self, root: &str) -> Option<u64> {
        self.refresh_with(root, packed_media_fingerprint)
    }

    fn refresh_with(
        &mut self,
        root: &str,
        full_scan: impl FnOnce(&str) -> Option<u64>,
    ) -> Option<u64> {
        let signal = packed_media_publication_signal(root);
        if signal != self.signal {
            self.fingerprint = full_scan(root);
            // As above, a publication during the scan remains observable on
            // the next request rather than being hidden by a post-scan read.
            self.signal = signal;
        }
        self.fingerprint
    }
}

/// Full identity of the packed repository visible to a renderer. This is
/// intentionally called only after the cheap publication signal changes.
/// Negative-cache sentinels are deliberately excluded: MediaStore checks the
/// packed resolver before materializing or consulting those sentinels. The
/// directory and published packed-part metadata are included, so atomic directory
/// publication, symlink retargeting, hardlink-backed replacement, and newly
/// visible `.data` parts all change the identity.
fn packed_media_fingerprint(root: &str) -> Option<u64> {
    let path = packed_media_root(root)?;
    let link_metadata = std::fs::symlink_metadata(&path).ok()?;
    let target_metadata = std::fs::metadata(&path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    // Do not include the directory mtime: a negative-cache `.404` sentinel
    // changes it, but does not change the packed resolver that must serve the
    // next request. Directory identity and the packed children below do.
    hash_metadata_identity(&link_metadata, &mut hasher);
    hash_metadata_identity(&target_metadata, &mut hasher);
    if !target_metadata.is_dir() {
        return Some(hasher.finish());
    }
    let mut entries = std::fs::read_dir(&path)
        .ok()?
        .flatten()
        .filter(|entry| {
            media_data_entry_name(&entry.file_name()) || media_alias_entry_name(&entry.file_name())
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let data_name = entry.file_name();
        if media_alias_entry_name(&data_name) {
            data_name.hash(&mut hasher);
            std::fs::symlink_metadata(entry.path())
                .ok()
                .map(|metadata| hash_metadata(&metadata, &mut hasher));
            std::fs::metadata(entry.path())
                .ok()
                .map(|metadata| hash_metadata(&metadata, &mut hasher));
            continue;
        }
        data_name.hash(&mut hasher);
        for suffix in ["data", "hashes", "offsets", "lengths", "format"] {
            let part = data_name
                .to_string_lossy()
                .strip_suffix(".data")
                .map(|stem| path.join(format!("{stem}.{suffix}")));
            let Some(part) = part else { continue };
            suffix.hash(&mut hasher);
            std::fs::symlink_metadata(&part)
                .ok()
                .map(|metadata| hash_metadata(&metadata, &mut hasher));
            std::fs::metadata(&part)
                .ok()
                .map(|metadata| hash_metadata(&metadata, &mut hasher));
        }
    }
    Some(hasher.finish())
}

fn wiki_backend_args(root: &str, addr: &str) -> Vec<String> {
    // wikimak's serve default resolves parent/wikimedia.media, falling back
    // to the legacy per-mirror cache only when no valid shared catalogue is
    // published.  Keep that policy in wikimak rather than duplicating a
    // partial "has .data" test here.
    vec!["wikimak".into(), "serve".into(), root.into(), addr.into()]
}

fn ensure_wiki_backend(
    self_exe: &str,
    dbname: &str,
    root: &str,
    backends: &Arc<Mutex<Backends>>,
) -> Result<(String, Option<u64>), String> {
    let mut backends = backends.lock().expect("archive backend registry poisoned");
    let media_state = if let Some(mut backend) = backends.wiki.remove(dbname) {
        let previous_fingerprint = backend.media_state.fingerprint;
        let fingerprint = backend.media_state.refresh(root);
        let alive = backend.child.try_wait().ok().flatten().is_none();
        if alive && previous_fingerprint == fingerprint {
            let addr = backend.addr.clone();
            backends.wiki.insert(dbname.into(), backend);
            return Ok((addr, fingerprint));
        }
        if alive {
            let _ = backend.child.kill();
        }
        let _ = backend.child.wait();
        backend.media_state
    } else {
        PackedMediaState::new(root)
    };
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|error| format!("reserve renderer port: {error}"))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("read renderer port: {error}"))?
        .to_string();
    drop(listener);
    let args = wiki_backend_args(root, &addr);
    let mut child = Command::new(self_exe)
        .args(&args)
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
    let media_fingerprint = media_state.fingerprint;
    backends.wiki.insert(
        dbname.into(),
        WikiBackend {
            child,
            addr: addr.clone(),
            media_state,
        },
    );
    Ok((addr, media_fingerprint))
}

fn proxy_response(
    dbname: &str,
    public_route: &str,
    raw: Vec<u8>,
    enhancer: &dyn ImageEnhancer,
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
            Enhancement::Ready(enhanced) => {
                body = enhanced;
                mime = "image/jpeg".into();
                "ready"
            }
            Enhancement::Pending => "processing",
            Enhancement::Original => "original",
        };
        headers.push(header("X-Chupa-Enhanced", state));
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
    if let Ok(jobs) = crate::supervisor::library_jobs() {
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
    url.searchParams.set("chupa_enhanced", "1");
    fetch(url, {cache: "no-store"}).then(response => {
      const state = response.headers.get("X-Chupa-Enhanced");
      if (state === "ready") img.src = url.toString();
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
        .filter(|part| !part.starts_with("chupa_enhanced="))
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

fn generation_cache_route(
    root: &str,
    public_route: &str,
    media_fingerprint: Option<u64>,
) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    media_fingerprint.hash(&mut hasher);
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
    let jobs = match crate::supervisor::library_jobs() {
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

fn warc(path: &str, captures: &dyn CaptureProvider) -> Resp {
    let tail = path.trim_start_matches("/warc").trim_matches('/');
    if tail.is_empty() {
        let mut items = String::new();
        let archives = match captures.archives() {
            Ok(archives) => archives,
            Err(error) => return text(500, &error),
        };
        for archive in archives {
            if captures.rows(archive.id).ok().is_some_and(|rows| !rows.is_empty()) {
                let label = if archive.name.is_empty() {
                    archive.id.to_string()
                } else {
                    format!("{} ({})", escape(&archive.name), archive.id)
                };
                items.push_str(&format!(
                    r#"<li><a href="/warc/{}/">{label}</a></li>"#,
                    archive.id
                ));
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
        return match captures.detail(box_id, row_id) {
            Ok(Some(capture)) => bytes(
                capture.status,
                capture.mime.as_str(),
                capture.response_body.as_slice().to_vec(),
            ),
            Ok(None) => text(404, "no such captured response"),
            Err(error) => text(500, &error),
        };
    }
    let rows = match captures.rows(box_id) {
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
    let jobs = crate::supervisor::library_jobs().unwrap_or_default();
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
            "Chupa archive library",
            &format!(
                "<h1>Chupa archive library</h1><h2>Wikis</h2><ul>{wiki}</ul>\
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
        .with_header(header("X-Chupa-Enhanced", "ready"))
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
    use std::ffi::OsString;
    use std::sync::Mutex;

    static GATEWAY_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        previous: Option<OsString>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(ARCHIVE_GATEWAY_ADDR_ENV, value) },
                None => unsafe { std::env::remove_var(ARCHIVE_GATEWAY_ADDR_ENV) },
            }
        }
    }

    fn set_gateway_addr(value: Option<&str>) -> EnvRestore {
        let previous = std::env::var_os(ARCHIVE_GATEWAY_ADDR_ENV);
        match value {
            Some(value) => unsafe { std::env::set_var(ARCHIVE_GATEWAY_ADDR_ENV, value) },
            None => unsafe { std::env::remove_var(ARCHIVE_GATEWAY_ADDR_ENV) },
        }
        EnvRestore { previous }
    }

    #[test]
    fn gateway_urls_use_default_address_when_override_is_unset() {
        let _guard = GATEWAY_ENV_LOCK.lock().unwrap();
        let _restore = set_gateway_addr(None);
        assert_eq!(host_base_url(), "http://127.0.0.1:8642");
        let expected = if cfg!(target_os = "macos") {
            "http://10.0.2.2:8642"
        } else {
            "http://127.0.0.1:8642"
        };
        assert_eq!(browser_base_url(), expected);
    }

    #[test]
    fn gateway_urls_use_non_empty_address_override() {
        let _guard = GATEWAY_ENV_LOCK.lock().unwrap();
        let _restore = set_gateway_addr(Some("127.0.0.1:18642"));
        assert_eq!(host_base_url(), "http://127.0.0.1:18642");
        let expected = if cfg!(target_os = "macos") {
            "http://10.0.2.2:18642"
        } else {
            "http://127.0.0.1:18642"
        };
        assert_eq!(browser_base_url(), expected);
    }

    #[test]
    fn gateway_urls_ignore_empty_address_override() {
        let _guard = GATEWAY_ENV_LOCK.lock().unwrap();
        let _restore = set_gateway_addr(Some(""));
        assert_eq!(host_base_url(), "http://127.0.0.1:8642");
        let expected = if cfg!(target_os = "macos") {
            "http://10.0.2.2:8642"
        } else {
            "http://127.0.0.1:8642"
        };
        assert_eq!(browser_base_url(), expected);
    }

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
                "w=250&chupa_enhanced=1"
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
        assert!(rewritten.contains("chupa_enhanced"));
        assert!(rewritten.contains("cache: \"no-store\""));
        assert!(!rewritten.contains("createObjectURL"));
        assert!(rewritten.contains("img.src = url.toString()"));
    }

    #[test]
    fn packed_media_fingerprint_ignores_negative_cache_and_tracks_published_data() {
        let root_path = std::env::temp_dir()
            .join(format!("sarun-gateway-media-{}", std::process::id()))
            .join("lvwiki.swdump");
        let media = root_path.parent().unwrap().join("wikimedia.media");
        let _ = std::fs::remove_dir_all(root_path.parent().unwrap());
        std::fs::create_dir_all(&media).unwrap();
        let root = root_path.to_string_lossy().into_owned();

        std::fs::write(media.join("media-webp-0000.data"), b"image").unwrap();
        std::fs::write(media.join("media-webp-0000.hashes"), 1_u64.to_le_bytes()).unwrap();
        std::fs::write(media.join("media-webp-0000.offsets"), 0_u32.to_le_bytes()).unwrap();
        let before = packed_media_fingerprint(&root);
        std::fs::write(
            media.join("media-alias-00000000.aliases"),
            b"sarun-packed-media-alias-part-v1\n",
        )
        .unwrap();
        assert_ne!(
            packed_media_fingerprint(&root),
            before,
            "publishing an alias-only part must retire the renderer generation"
        );
        std::fs::write(media.join("File-name.404"), b"404").unwrap();
        let after_alias = packed_media_fingerprint(&root);
        assert_eq!(
            packed_media_fingerprint(&root),
            after_alias,
            "negative cache entries must not retire a live backend"
        );

        std::fs::write(media.join("media-webp-0001.hashes"), 2_u64.to_le_bytes()).unwrap();
        std::fs::write(media.join("media-webp-0001.offsets"), 0_u32.to_le_bytes()).unwrap();
        std::fs::write(media.join("media-webp-0001.data"), b"new image").unwrap();
        let after_data = packed_media_fingerprint(&root);
        assert_ne!(after_data, before);
        let args = wiki_backend_args(&root, "127.0.0.1:1234");
        assert_eq!(
            args,
            vec![
                "wikimak",
                "serve",
                root.as_str(),
                "127.0.0.1:1234",
            ]
        );

        // Publishing through a symlinked repository and seeding the packed
        // part through a hardlink are both visible to the identity check.
        let published = root_path.parent().unwrap().join("published-media");
        std::fs::rename(&media, &published).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&published, &media).unwrap();
        #[cfg(windows)]
        std::fs::create_dir(&media).unwrap();
        let symlinked = packed_media_fingerprint(&root);
        assert_ne!(symlinked, after_data);
        let source = root_path.parent().unwrap().join("seed.data");
        std::fs::write(&source, b"new image").unwrap();
        std::fs::write(published.join("media-webp-0002.hashes"), 3_u64.to_le_bytes()).unwrap();
        std::fs::write(published.join("media-webp-0002.offsets"), 0_u32.to_le_bytes()).unwrap();
        std::fs::hard_link(&source, published.join("media-webp-0002.data")).unwrap();
        assert_ne!(packed_media_fingerprint(&root), symlinked);

        let _ = std::fs::remove_dir_all(root_path.parent().unwrap());
    }

    #[test]
    fn packed_media_fingerprint_tracks_legacy_fallback() {
        let parent = std::env::temp_dir().join(format!(
            "sarun-gateway-legacy-media-{}",
            std::process::id()
        ));
        let root_path = parent.join("lvwiki.swdump");
        let legacy = root_path.with_extension("media");
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("media-webp-0000.hashes"), 1_u64.to_le_bytes()).unwrap();
        std::fs::write(legacy.join("media-webp-0000.offsets"), 0_u32.to_le_bytes()).unwrap();
        std::fs::write(legacy.join("media-webp-0000.data"), b"image").unwrap();
        let root = root_path.to_string_lossy().into_owned();
        let before = packed_media_fingerprint(&root);

        std::fs::write(legacy.join("media-webp-0001.hashes"), 2_u64.to_le_bytes()).unwrap();
        std::fs::write(legacy.join("media-webp-0001.offsets"), 0_u32.to_le_bytes()).unwrap();
        std::fs::write(legacy.join("media-webp-0001.data"), b"new image").unwrap();
        assert_ne!(packed_media_fingerprint(&root), before);

        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn packed_media_state_scans_only_after_repository_publication() {
        let parent = std::env::temp_dir().join(format!(
            "sarun-gateway-media-state-{}",
            std::process::id()
        ));
        let root_path = parent.join("lvwiki.swdump");
        let media = parent.join("wikimedia.media");
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&media).unwrap();
        let root = root_path.to_string_lossy().into_owned();

        // Use enough complete parts to keep the evidence shaped like the
        // hundreds-part HDD repository, while the assertion below proves the
        // steady-state path does not revisit any of them.
        for part in 0_u64..64 {
            let stem = format!("media-webp-{part:04}");
            std::fs::write(media.join(format!("{stem}.hashes")), (part + 1).to_le_bytes())
                .unwrap();
            std::fs::write(media.join(format!("{stem}.offsets")), 0_u32.to_le_bytes()).unwrap();
            std::fs::write(media.join(format!("{stem}.lengths")), 5_u32.to_le_bytes()).unwrap();
            std::fs::write(
                media.join(format!("{stem}.format")),
                b"sarun-packed-media-v2 lengths=u32\n",
            )
            .unwrap();
            std::fs::write(media.join(format!("{stem}.data")), b"image").unwrap();
        }

        let mut full_scans = 0;
        let mut state = PackedMediaState::new_with(&root, |root| {
            full_scans += 1;
            packed_media_fingerprint(root)
        });
        let before = state.fingerprint;
        let signal_before = state.signal.clone();
        for _ in 0..16 {
            assert_eq!(
                state.refresh_with(&root, |root| {
                    full_scans += 1;
                    packed_media_fingerprint(root)
                }),
                before,
                "unchanged publication signal must reuse the cached fingerprint"
            );
        }
        assert_eq!(full_scans, 1, "steady-state admission must not rescan parts");

        // Match MediaRepositoryWriter's publication order: all companions
        // become visible before the discoverable .data name.
        let stem = "media-webp-0064";
        std::fs::write(media.join(format!("{stem}.hashes")), 65_u64.to_le_bytes()).unwrap();
        std::fs::write(media.join(format!("{stem}.offsets")), 0_u32.to_le_bytes()).unwrap();
        std::fs::write(media.join(format!("{stem}.lengths")), 9_u32.to_le_bytes()).unwrap();
        std::fs::write(
            media.join(format!("{stem}.format")),
            b"sarun-packed-media-v2 lengths=u32\n",
        )
        .unwrap();
        std::fs::write(media.join(format!("{stem}.data")), b"new image").unwrap();
        assert_ne!(
            packed_media_publication_signal(&root),
            signal_before,
            "publishing a new part must change the cheap repository signal"
        );

        let after = state.refresh_with(&root, |root| {
            full_scans += 1;
            packed_media_fingerprint(root)
        });
        assert_ne!(after, before, "the published part must become visible");
        assert_eq!(full_scans, 2, "publication causes one refresh scan");
        for _ in 0..16 {
            state.refresh_with(&root, |root| {
                full_scans += 1;
                packed_media_fingerprint(root)
            });
        }
        assert_eq!(full_scans, 2, "steady state after publication remains scan-free");
        std::fs::remove_dir_all(parent).unwrap();
    }
}
