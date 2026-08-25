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
//!   * `GET /` — redirect to the wiki's configured main page.
//!
//! Every internal link carries `?asof=` when the view is time-shifted:
//! the renderer covers content links through [`RenderOptions::asof_query`];
//! the chrome links (history/allpages/date-picker) append it here.
//!
//! Concurrency: depot requests open their read-side [`Instance`] per request.
//! Archive requests reuse parsed generation metadata while the selected
//! generation is unchanged, but reopen the lease-bearing archive access layer
//! for each request after re-checking the selector and maintenance marker. A
//! A fresh [`LuaInvoker`] is still built PER RENDER so mutable Lua values,
//! initialized module tables, callbacks, and τ state cannot cross requests.
//! Its immutable source bytecode is loaded from the server-owned cache shared
//! by the four workers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use tiny_http::{Header, Method, Request, Response, Server};

use wikimak_media::{BlobMediaResolver, MediaStore};
use wikimak_scribunto::{LuaBytecodeCache, LuaInvoker, LuaModuleSourceScope};
use wikimak_wikitext::html::escape;
use wikimak_wikitext::{render, ModuleInvoker, PageStore, RenderOptions, RenderOutput, Title};

use crate::asof::AsOfView;
use crate::archive_browse::{ArchiveAsOfView, ArchiveBrowseIndex, ArchiveBrowseMetadata};
use crate::Instance;

/// Route prefix for lazily-materialized media; kept in sync with the
/// `/w/media/` handler and [`BlobMediaResolver`].
const MEDIA_ROUTE_PREFIX: &str = "/w/media/";
/// `#REDIRECT` follow budget at τ (plan §2 redirects, loop-capped).
const MAX_REDIRECT_HOPS: u32 = 10;
/// Worker threads blocking in `Server::recv`.
const POOL_THREADS: usize = 4;

pub struct ServeConfig {
    /// Existing wikimak instance root.
    pub root: PathBuf,
    /// Bind address, e.g. `127.0.0.1:8642`.
    pub addr: String,
    /// Blob-cache root for materialized media. Offline serve (no `fetch`
    /// in the media crate) turns every miss into an inline placeholder.
    pub media_cache: PathBuf,
    /// Optional Kiwix image source selected for this mirror.  The ZIM is
    /// opened read-only; its directory is indexed at startup and image
    /// clusters are read on demand.
    pub kiwix_source: Option<PathBuf>,
    /// Optional directory containing packed `media-*.data` files and their
    /// independently mmap-able hash/offset arrays.
    pub packed_media: Option<PathBuf>,
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

struct App {
    source: AppSource,
    bytecode_cache: LuaBytecodeCache,
}

enum AppSource {
    Depot(Instance),
    Archive(Arc<ArchiveBrowseIndex>),
}

enum PageView<'a> {
    Depot(AsOfView<'a>),
    Archive(ArchiveAsOfView<'a>),
}

impl PageStore for PageView<'_> {
    fn page_text(&self, title: &Title) -> Option<String> {
        match self {
            Self::Depot(view) => view.page_text(title),
            Self::Archive(view) => view.page_text(title),
        }
    }

    fn page_exists(&self, title: &Title) -> bool {
        match self {
            Self::Depot(view) => view.page_exists(title),
            Self::Archive(view) => view.page_exists(title),
        }
    }

    fn page_id(&self, title: &Title) -> Option<u64> {
        match self {
            Self::Depot(view) => view.page_id(title),
            Self::Archive(view) => view.page_id(title),
        }
    }

    fn page_count(&self, namespace: Option<i32>) -> Option<u64> {
        match self {
            Self::Depot(view) => view.page_count(namespace),
            Self::Archive(view) => view.page_count(namespace),
        }
    }

    fn category_members(&self, category: &Title) -> Option<Vec<Title>> {
        match self {
            Self::Depot(view) => view.category_members(category),
            Self::Archive(view) => view.category_members(category),
        }
    }

    fn site(&self) -> &wikimak_wikitext::SiteConfig {
        match self {
            Self::Depot(view) => view.site(),
            Self::Archive(view) => view.site(),
        }
    }

    fn timestamp_micros(&self) -> i64 {
        match self {
            Self::Depot(view) => view.timestamp_micros(),
            Self::Archive(view) => view.timestamp_micros(),
        }
    }
}

/// Page text cache for one render request. The renderer can revisit the same
/// normalized title through a template and through Scribunto, so keep the
/// complete result (including `None`) until this page response is finished.
struct CachedPageStore<'a> {
    inner: &'a dyn PageStore,
    page_text: RefCell<HashMap<Title, Option<String>>>,
}

impl<'a> CachedPageStore<'a> {
    fn new(inner: &'a dyn PageStore) -> Self {
        Self {
            inner,
            page_text: RefCell::new(HashMap::new()),
        }
    }
}

impl PageStore for CachedPageStore<'_> {
    fn page_text(&self, title: &Title) -> Option<String> {
        self.page_text
            .borrow_mut()
            .entry(title.clone())
            .or_insert_with(|| self.inner.page_text(title))
            .clone()
    }

    fn page_exists(&self, title: &Title) -> bool {
        self.inner.page_exists(title)
    }

    fn page_id(&self, title: &Title) -> Option<u64> {
        self.inner.page_id(title)
    }

    fn page_count(&self, namespace: Option<i32>) -> Option<u64> {
        self.inner.page_count(namespace)
    }

    fn category_members(&self, category: &Title) -> Option<Vec<Title>> {
        self.inner.category_members(category)
    }

    fn site(&self) -> &wikimak_wikitext::SiteConfig {
        self.inner.site()
    }

    fn timestamp_micros(&self) -> i64 {
        self.inner.timestamp_micros()
    }
}

enum ServerSource {
    Depot(PathBuf),
    Archive(PathBuf),
}

struct ServerApp {
    source: ServerSource,
    media: Arc<MediaStore>,
    archive_cache: Mutex<ArchiveRequestCache>,
    depot_bytecode_cache: Option<LuaBytecodeCache>,
}

struct CachedArchive {
    generation_id: String,
    backrefs: BackrefIdentity,
    metadata: ArchiveBrowseMetadata,
    bytecode_cache: LuaBytecodeCache,
}

/// Identity of the independently published optional backreference sidecar.
/// The writer publishes a replacement by rename, so device/inode plus the
/// small-file metadata detects both absence/presence and atomic replacement
/// without reading the sidecar payload on every request.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BackrefIdentity {
    present: bool,
    bytes: u64,
    modified: Option<(u64, u32)>,
    device: u64,
    inode: u64,
}

fn backrefs_identity(destination: &Path) -> Result<BackrefIdentity, String> {
    let path = destination.with_extension("swrefs");
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackrefIdentity {
                present: false,
                bytes: 0,
                modified: None,
                device: 0,
                inode: 0,
            })
        }
        Err(error) => return Err(format!("inspect backreference sidecar {}: {error}", path.display())),
    };
    if !metadata.is_file() {
        return Err(format!(
            "backreference sidecar {} is not a regular file",
            path.display()
        ));
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()));
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    Ok(BackrefIdentity {
        present: true,
        bytes: metadata.len(),
        modified,
        device,
        inode,
    })
}

/// Reuses the expensive parsed archive metadata without retaining a generation
/// lease between requests. The selector is checked for every ordinary request;
/// its generation name is the publication identity, while the maintenance
/// marker closes serving during an update.
struct ArchiveRequestCache {
    cached: Option<CachedArchive>,
}

impl ArchiveRequestCache {
    fn new() -> Self {
        Self { cached: None }
    }

    fn clear(&mut self) {
        // The cache owns mmap-backed metadata and the generation-scoped Lua
        // bytecode/source cache. Request indexes and their generation leases
        // are owned by the returned Arc and drop with it; in-flight App values
        // retain the old bytecode handle only until their request finishes.
        self.cached.take();
    }

    fn bytecode_cache(&self) -> Option<LuaBytecodeCache> {
        self.cached
            .as_ref()
            .map(|cached| cached.bytecode_cache.clone())
    }

    fn remember(
        &mut self,
        index: Arc<ArchiveBrowseIndex>,
        backrefs: BackrefIdentity,
    ) -> Arc<ArchiveBrowseIndex> {
        let generation_id = index.generation_id().as_str().to_owned();
        self.cached = Some(CachedArchive {
            generation_id,
            backrefs,
            metadata: index.metadata(),
            bytecode_cache: LuaBytecodeCache::new(),
        });
        index
    }

    fn open(&mut self, destination: &std::path::Path) -> Result<Arc<ArchiveBrowseIndex>, String> {
        let selected = match crate::installation_lifecycle::serving_pair(destination) {
            Ok(Some(pair)) => {
                let Some(generation_id) = pair
                    .archive
                    .file_name()
                    .and_then(|name| name.to_str())
                else {
                    let message = format!(
                        "selected archive path has no valid generation name: {}",
                        pair.archive.display()
                    );
                    self.clear();
                    return Err(message);
                };
                let generation_id = generation_id.to_owned();
                (generation_id, pair)
            }
            Ok(None) => {
                self.clear();
                return Err(format!("{} has no committed generation", destination.display()));
            }
            Err(error) => {
                // This includes the durable update-maintenance marker. Do not
                // leave the previous generation holding cleanup hostage after
                // serving has been closed.
                self.clear();
                return Err(error);
            }
        };
        let (generation_id, selected_pair) = selected;
        let backrefs = match backrefs_identity(destination) {
            Ok(identity) => identity,
            Err(error) => {
                self.clear();
                drop(selected_pair);
                return Err(error);
            }
        };

        if self
            .cached
            .as_ref()
            .is_some_and(|cached| cached.generation_id == generation_id)
        {
            let refresh_backrefs = self
                .cached
                .as_ref()
                .is_some_and(|cached| cached.backrefs != backrefs);
            let metadata = self
                .cached
                .as_ref()
                .expect("generation cache entry just checked")
                .metadata
                .clone();
            let sidecar = destination.with_extension("swrefs");
            let opened = if refresh_backrefs {
                ArchiveBrowseIndex::open_request_with_backrefs(
                    &metadata,
                    &selected_pair.archive,
                    Some(sidecar.as_path()),
                )
            } else {
                ArchiveBrowseIndex::open_request(&metadata, &selected_pair.archive)
            };
            match opened {
                Ok(index) => {
                    let index = Arc::new(index);
                    if refresh_backrefs {
                        let refreshed_metadata = index.metadata();
                        if let Some(cached) = self.cached.as_mut() {
                            cached.backrefs = backrefs;
                            cached.metadata = refreshed_metadata;
                        }
                    }
                    drop(selected_pair);
                    return Ok(index);
                }
                Err(error) => {
                    // A publication bug or external replacement may leave a
                    // different object at the same generation path. Do not
                    // retain the stale layout and wedge all future requests;
                    // discard it and perform the ordinary cold open exactly
                    // once. That path rechecks selector and maintenance state.
                    self.clear();
                    drop(selected_pair);
                    return match open_archive_request(destination) {
                        Ok(index) => Ok(self.remember(index, backrefs)),
                        Err(cold_error) => Err(format!(
                            "cached archive reopen failed: {error}; cold reopen failed: {cold_error}"
                        )),
                    };
                }
            }
        }

        self.clear();
        let index = open_archive_request(destination)?;
        drop(selected_pair);
        Ok(self.remember(index, backrefs))
    }
}

struct BrowseRevision {
    meta: crate::RevisionMeta,
    visibility: Option<BrowseVisibility>,
}

struct BrowseVisibility {
    deleted_parts: String,
    parts_are_suppressed: bool,
    deleted_by_page_deletion: bool,
    page_deletion_timestamp: String,
}

struct BrowseAction {
    event_type: String,
    timestamp: String,
    comment: String,
    actor: String,
    historical_title: String,
    current_title: String,
}

impl App {
    fn view(&self, timestamp_micros: Option<i64>) -> Result<PageView<'_>, String> {
        match &self.source {
            AppSource::Depot(inst) => AsOfView::new(inst, timestamp_micros)
                .map(PageView::Depot)
                .map_err(|error| error.to_string()),
            AppSource::Archive(archive) => {
                Ok(PageView::Archive(archive.view(timestamp_micros)))
            }
        }
    }

    fn source_scope(&self, timestamp_micros: Option<i64>) -> Option<LuaModuleSourceScope> {
        // Only current-head archive content has a stable cross-render source
        // identity here. Explicit as-of views can select different module
        // revisions and therefore keep source lookup per render.
        match (&self.source, timestamp_micros) {
            (AppSource::Archive(archive), None) => Some(LuaModuleSourceScope::new(
                archive.generation_id().as_str(),
            )),
            _ => None,
        }
    }

    fn page_id_by_title(&self, title: &str, timestamp_micros: Option<i64>) -> Option<u64> {
        match &self.source {
            AppSource::Depot(inst) => inst
                .page_id_by_title_at(title, timestamp_micros)
                .ok()
                .flatten(),
            AppSource::Archive(archive) => {
                archive.page_id_by_title(title, timestamp_micros.unwrap_or(i64::MAX))
            }
        }
    }

    fn page_text_at(&self, page_id: u64, timestamp_micros: Option<i64>) -> Option<Vec<u8>> {
        match &self.source {
            AppSource::Depot(inst) => inst
                .page_text_at(page_id, timestamp_micros)
                .ok()
                .flatten(),
            AppSource::Archive(archive) => archive
                .page_text_at(page_id, timestamp_micros.unwrap_or(i64::MAX))
                .ok()
                .flatten(),
        }
    }

    fn pages(&self, filter: Option<&str>, limit: usize) -> Vec<(u64, String)> {
        match &self.source {
            AppSource::Depot(inst) => inst.pages(filter, limit).unwrap_or_default(),
            AppSource::Archive(archive) => archive.pages(filter, limit),
        }
    }

    fn page_history(
        &self,
        page_id: u64,
        timestamp_micros: Option<i64>,
    ) -> Vec<BrowseRevision> {
        match &self.source {
            AppSource::Depot(inst) => {
                let Ok(history) = inst.page_history(page_id) else {
                    return Vec::new();
                };
                history
                    .filter_map(Result::ok)
                    .map(|entry| {
                        let visibility = inst
                            .revision_visibility(entry.meta.rev_id)
                            .ok()
                            .flatten()
                            .map(|state| BrowseVisibility {
                                deleted_parts: state.deleted_parts,
                                parts_are_suppressed: state.parts_are_suppressed,
                                deleted_by_page_deletion: state.deleted_by_page_deletion,
                                page_deletion_timestamp: state.page_deletion_timestamp,
                            });
                        BrowseRevision {
                            meta: entry.meta,
                            visibility,
                        }
                    })
                    .collect()
            }
            AppSource::Archive(archive) => archive
                .revision_at(page_id, timestamp_micros.unwrap_or(i64::MAX))
                .ok()
                .flatten()
                .map(|revision| BrowseRevision {
                    meta: revision.meta,
                    visibility: revision.visibility.map(|state| BrowseVisibility {
                        deleted_parts: format!("mask 0x{:02x}", state.deleted_parts),
                        parts_are_suppressed: state.parts_are_suppressed,
                        deleted_by_page_deletion: state.deleted_by_page_deletion,
                        page_deletion_timestamp: state
                            .page_deletion_timestamp_micros
                            .map(micros_to_datetime)
                            .unwrap_or_default(),
                    }),
                })
                .into_iter()
                .collect(),
        }
    }

    fn page_actions(&self, page_id: u64) -> Vec<BrowseAction> {
        match &self.source {
            AppSource::Depot(inst) => inst
                .page_actions(page_id)
                .unwrap_or_default()
                .into_iter()
                .map(|action| BrowseAction {
                    event_type: action.event_type,
                    timestamp: action.timestamp,
                    comment: action.comment,
                    actor: action.actor,
                    historical_title: action.historical_title,
                    current_title: action.current_title,
                })
                .collect(),
            AppSource::Archive(_) => Vec::new(),
        }
    }
}

/// Start the server and block, dispatching requests across the pool.
pub fn serve(inst: Instance, cfg: ServeConfig) -> Result<(), String> {
    // The caller historically handed serve a writable instance. Release its
    // exclusive lifetime lock before accepting requests; each request below
    // takes only the shared read lock needed to decode its response.
    drop(inst);
    let server = Server::http(&cfg.addr).map_err(|e| format!("bind {}: {e}", cfg.addr))?;
    let server = Arc::new(server);
    // No repos + no `fetch` feature ⇒ every media miss is an offline miss
    // (inline placeholder). A prefetch driver could pass a commons chain.
    let packed_media = crate::resolve_packed_media_path(&cfg.root, cfg.packed_media.as_deref());
    let media = match (packed_media, cfg.kiwix_source) {
        (Some(path), _) => MediaStore::with_packed(cfg.media_cache, Vec::new(), path)
            .map_err(|error| error.to_string())?,
        (None, Some(path)) => MediaStore::with_kiwix(cfg.media_cache, Vec::new(), path)
            .map_err(|error| error.to_string())?,
        (None, None) => MediaStore::new(cfg.media_cache, Vec::new()),
    };
    let app = Arc::new(ServerApp {
        source: ServerSource::Depot(cfg.root),
        media: Arc::new(media),
        archive_cache: Mutex::new(ArchiveRequestCache::new()),
        depot_bytecode_cache: Some(LuaBytecodeCache::new()),
    });

    serve_http(server, app, &cfg.addr)
}

pub fn serve_archive(
    destination: PathBuf,
    addr: String,
    media_cache: PathBuf,
    kiwix_source: Option<PathBuf>,
    packed_media: Option<PathBuf>,
) -> Result<(), String> {
    let server = Server::http(&addr).map_err(|e| format!("bind {addr}: {e}"))?;
    let server = Arc::new(server);
    let packed_media = crate::resolve_packed_media_path(&destination, packed_media.as_deref());
    let media = match (packed_media, kiwix_source) {
        (Some(path), _) => MediaStore::with_packed(media_cache, Vec::new(), path)
            .map_err(|error| error.to_string())?,
        (None, Some(path)) => MediaStore::with_kiwix(media_cache, Vec::new(), path)
            .map_err(|error| error.to_string())?,
        (None, None) => MediaStore::new(media_cache, Vec::new()),
    };
    let app = Arc::new(ServerApp {
        source: ServerSource::Archive(destination),
        media: Arc::new(media),
        archive_cache: Mutex::new(ArchiveRequestCache::new()),
        depot_bytecode_cache: None,
    });
    serve_http(server, app, &addr)
}

fn serve_http(server: Arc<Server>, app: Arc<ServerApp>, addr: &str) -> Result<(), String> {
    eprintln!("wikimak serve: listening on http://{addr}");
    let mut handles = Vec::new();
    for _ in 0..POOL_THREADS {
        let server = Arc::clone(&server);
        let app = Arc::clone(&app);
        handles.push(thread::spawn(move || {
            while let Ok(req) = server.recv() {
                let resp = handle(&app, &req);
                let _ = req.respond(resp);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn handle(server: &ServerApp, req: &Request) -> Resp {
    if *req.method() != Method::Get {
        return text_resp(405, "method not allowed");
    }
    let url = req.url();
    let (path, query_raw) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url, ""),
    };
    dispatch(server, path, query_raw)
}

fn dispatch(server: &ServerApp, path: &str, query_raw: &str) -> Resp {
    let query = parse_query(query_raw);
    if let Some(rest) = path.strip_prefix("/w/media/") {
        return media_response(&server.media, &percent_decode(rest), &query);
    }

    let (source, bytecode_cache) = match &server.source {
        ServerSource::Depot(root) => {
            match Instance::open_read(crate::instance::read_config(root.clone())) {
                Ok(inst) => {
                    let Some(cache) = server.depot_bytecode_cache.clone() else {
                        return text_resp(503, "mirror temporarily unavailable: depot Lua cache missing");
                    };
                    (AppSource::Depot(inst), cache)
                }
                Err(error) => {
                    return text_resp(
                        503,
                        &format!("mirror temporarily unavailable: {error}"),
                    )
                }
            }
        }
        ServerSource::Archive(destination) => {
            let archive = match server.archive_cache.lock() {
                Ok(mut cache) => {
                    match cache.open(destination) {
                        Ok(archive) => match cache.bytecode_cache() {
                            Some(bytecode_cache) => Ok((archive, bytecode_cache)),
                            None => Err("archive cache has no generation bytecode cache".to_owned()),
                        },
                        Err(error) => Err(error),
                    }
                }
                Err(_) => Err("archive request cache is poisoned".to_owned()),
            };
            match archive {
                Ok((archive, bytecode_cache)) => (AppSource::Archive(archive), bytecode_cache),
                Err(error) => {
                    return text_resp(
                        503,
                        &format!("mirror temporarily unavailable: {error}"),
                    )
                }
            }
        }
    };
    let app = App {
        source,
        bytecode_cache,
    };

    if path == "/" {
        return home_response(&app);
    }
    if let Some(rest) = path.strip_prefix("/wiki/") {
        return page_response(&app, &percent_decode(rest), &query);
    }
    if let Some(rest) = path.strip_prefix("/w/history/") {
        return history_response(&app, &percent_decode(rest), &query);
    }
    if path == "/w/allpages" {
        return allpages_response(&app, &query);
    }
    not_found_page(&app, &query)
}

fn open_archive_request(destination: &std::path::Path) -> Result<Arc<ArchiveBrowseIndex>, String> {
    ArchiveBrowseIndex::open_installed(destination)
        .map(Arc::new)
        .map_err(|error| error.to_string())
}

fn home_response(app: &App) -> Resp {
    let main_page = match &app.source {
        AppSource::Depot(inst) => inst
            .site_config_at(None)
            .ok()
            .flatten()
            .and_then(|snapshot| {
                snapshot
                    .get("base")
                    .and_then(|base| base.as_str())
                    .map(str::to_string)
            })
            .and_then(|base| base.split_once("/wiki/").map(|(_, title)| title.to_string()))
            .map(|title| percent_decode(title.split(['?', '#']).next().unwrap_or(&title)))
            .filter(|title| !title.is_empty()),
        AppSource::Archive(archive) => archive
            .site_info()
            .base
            .split_once("/wiki/")
            .map(|(_, title)| percent_decode(title.split(['?', '#']).next().unwrap_or(title)))
            .filter(|title| !title.is_empty()),
    };
    match main_page {
        Some(title) => redirect(&format!(
            "/wiki/{}",
            wikimak_wikitext::html::encode_path(&title)
        )),
        None => redirect("/w/allpages"),
    }
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
    app: &App,
    raw: &str,
    ts: Option<i64>,
) -> (Option<u64>, String, Option<String>, Option<Vec<u8>>) {
    let original = raw.replace('_', " ").trim().to_string();
    let mut current = original.clone();
    let mut redirected_from = None;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..=MAX_REDIRECT_HOPS {
        let pid = match app.page_id_by_title(&current, ts) {
            Some(id) => id,
            None => return (None, current, redirected_from, None),
        };
        if !seen.insert(pid) {
            return (Some(pid), current, redirected_from, None);
        }
        let text = match app.page_text_at(pid, ts) {
            Some(t) => t,
            None => return (Some(pid), current, redirected_from, None),
        };
        match wikimak_wikitext::parse_redirect(&String::from_utf8_lossy(&text)) {
            Some(target) => {
                if redirected_from.is_none() {
                    redirected_from = Some(original.clone());
                }
                current = target.replace('_', " ").trim().to_string();
            }
            None => return (Some(pid), current, redirected_from, Some(text)),
        }
    }
    (None, current, redirected_from, None)
}

fn page_response(app: &App, raw_title: &str, query: &HashMap<String, String>) -> Resp {
    let (ts, asof_query) = asof_from_query(query);
    let view = match app.view(ts) {
        Ok(v) => v,
        Err(e) => return html_resp(500, &error_shell(&format!("site config: {e}"))),
    };
    let site = view.site();

    let (_page_id, resolved_title, redirected_from, text) = resolve_page(app, raw_title, ts);
    let title_obj = Title::parse(&resolved_title, site);
    let display = title_obj.prefixed(site);
    let page_path = format!("/wiki/{}", wikimak_wikitext::html::encode_path(&display));

    let (content, out): (String, Option<RenderOutput>) = match text {
        Some(bytes) => {
            let wikitext = String::from_utf8_lossy(&bytes);
            let invoker = match app.source_scope(ts) {
                Some(scope) => LuaInvoker::with_cache_and_source_scope(
                    app.bytecode_cache.clone(),
                    scope,
                ),
                None => LuaInvoker::with_cache(app.bytecode_cache.clone()),
            };
            let media_resolver = BlobMediaResolver::new(MEDIA_ROUTE_PREFIX);
            let opts = RenderOptions {
                invoker: Some(&invoker as &dyn ModuleInvoker),
                media: Some(&media_resolver),
                link_prefix: "/wiki/".into(),
                asof_query: asof_query.clone(),
            };
            let store = CachedPageStore::new(&view);
            let out = render(&store, &title_obj, &wikitext, &opts);
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
    body.push_str(&instance_footer(site, ts, Some(&display)));

    html_resp(200, &shell(site, &escape(&display), &body))
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

fn history_response(app: &App, raw_title: &str, query: &HashMap<String, String>) -> Resp {
    let (ts, asof_query) = asof_from_query(query);
    let view = match app.view(ts) {
        Ok(v) => v,
        Err(e) => return html_resp(500, &error_shell(&format!("site config: {e}"))),
    };
    let site = view.site();
    let key = raw_title.replace('_', " ").trim().to_string();
    let display = Title::parse(&key, site).prefixed(site);
    let page_path = format!("/wiki/{}", wikimak_wikitext::html::encode_path(&display));

    let page_id = app.page_id_by_title(&key, ts);

    let mut rows = String::new();
    if let Some(pid) = page_id {
        for e in app.page_history(pid, ts) {
                let micros = e.meta.ts.timestamp_micros();
                let who = match &e.meta.contributor {
                    crate::ContributorMeta::Named { username, .. } => username.clone(),
                    crate::ContributorMeta::Anonymous { ip } => ip.clone(),
                    crate::ContributorMeta::Hidden => "(hidden)".to_string(),
                };
                let visibility = e
                    .visibility
                    .map(|state| {
                        let mut notes = Vec::new();
                        if !state.deleted_parts.is_empty() {
                            notes.push(format!(
                                "upstream marks {} hidden{}",
                                escape(&state.deleted_parts),
                                if state.parts_are_suppressed {
                                    " (suppressed)"
                                } else {
                                    ""
                                }
                            ));
                        }
                        if state.deleted_by_page_deletion {
                            notes.push(if state.page_deletion_timestamp.is_empty() {
                                "upstream page deletion".to_string()
                            } else {
                                format!(
                                    "upstream page deletion at {}",
                                    escape(&state.page_deletion_timestamp)
                                )
                            });
                        }
                        if notes.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " · <span class=\"visibility\">archived locally; {}</span>",
                                notes.join("; ")
                            )
                        }
                    })
                    .unwrap_or_default();
                rows.push_str(&format!(
                    r#"<li><a href="/wiki/{path}?asof={micros}">{when}</a> · rev {rev} · {len} bytes · {who}{comment}{visibility}</li>"#,
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
                    visibility = visibility,
                ));
        }
    }
    if rows.is_empty() {
        rows.push_str("<li class=\"noarticle\">No revisions.</li>");
    }

    let mut actions = String::new();
    if let Some(pid) = page_id {
        for action in app.page_actions(pid) {
                let titles = match (
                    action.historical_title.is_empty(),
                    action.current_title.is_empty(),
                    action.historical_title == action.current_title,
                ) {
                    (false, false, false) => format!(
                        " · {} → {}",
                        escape(&action.historical_title),
                        escape(&action.current_title)
                    ),
                    (false, _, _) => format!(" · {}", escape(&action.historical_title)),
                    (_, false, _) => format!(" · {}", escape(&action.current_title)),
                    _ => String::new(),
                };
                actions.push_str(&format!(
                    "<li>{when} · {kind} · {actor}{titles}{comment}</li>",
                    when = escape(&action.timestamp),
                    kind = escape(&action.event_type),
                    actor = if action.actor.is_empty() {
                        "(hidden)".to_string()
                    } else {
                        escape(&action.actor)
                    },
                    comment = if action.comment.is_empty() {
                        String::new()
                    } else {
                        format!(" · <span class=\"comment\">{}</span>", escape(&action.comment))
                    },
                ));
        }
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
    if !actions.is_empty() {
        body.push_str("<h2>Page actions</h2>");
        body.push_str(&format!("<ul class=\"history\">{actions}</ul>"));
    }
    body.push_str(&instance_footer(site, ts, Some(&display)));

    html_resp(200, &shell(site, &escape(&display), &body))
}

// ---------------------------------------------------------------------------
// allpages
// ---------------------------------------------------------------------------

fn allpages_response(app: &App, query: &HashMap<String, String>) -> Resp {
    let (ts, asof_query) = asof_from_query(query);
    let view = match app.view(ts) {
        Ok(v) => v,
        Err(e) => return html_resp(500, &error_shell(&format!("site config: {e}"))),
    };
    let site = view.site();
    let filter = query.get("filter").map(String::as_str).filter(|s| !s.is_empty());

    const PAGE_LIMIT: usize = 500;
    let pages = app.pages(filter, PAGE_LIMIT + 1);
    let truncated = pages.len() > PAGE_LIMIT;

    let mut rows = String::new();
    for (_id, title) in pages.iter().take(PAGE_LIMIT) {
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
    if truncated {
        body.push_str(
            "<p class=\"result-note\">Showing the first 500 matches. Narrow the filter to continue.</p>",
        );
    }
    body.push_str(&format!("<ul class=\"allpages\">{rows}</ul>"));
    body.push_str(&instance_footer(site, ts, None));

    html_resp(200, &shell(site, "All pages", &body))
}

// ---------------------------------------------------------------------------
// media
// ---------------------------------------------------------------------------

fn media_response(media: &MediaStore, raw_file: &str, query: &HashMap<String, String>) -> Resp {
    let w = query.get("w").map(String::as_str).unwrap_or("orig");
    let width = if w == "orig" { None } else { w.parse::<u32>().ok() };
    match media.read_with_type(raw_file, width) {
        Ok((file_type, bytes)) => bytes_resp(
            200,
            file_type
                .as_deref()
                .map(mime_for_storage_type)
                .unwrap_or_else(|| mime_for(raw_file)),
            bytes,
        ),
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

fn mime_for_storage_type(file_type: &str) -> &'static str {
    match file_type.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpeg" | "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "svg+xml" => "image/svg+xml",
        "webp" => "image/webp",
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
    site: &wikimak_wikitext::SiteConfig,
    page_path: &str,
    ts: Option<i64>,
    asof_query: &str,
) -> String {
    let _ = app;
    let date_val = ts.map(micros_to_date).unwrap_or_default();
    let site_name = if site.site_name.is_empty() {
        "Wikipedia mirror"
    } else {
        &site.site_name
    };
    let history = page_path
        .strip_prefix("/wiki/")
        .map(|title| {
            format!(
                r#"<a href="/w/history/{title}{asof}">History</a>"#,
                asof = asof_query
            )
        })
        .unwrap_or_default();
    // The date form GETs back to THIS page, preserving the current path.
    format!(
        r#"<header class="bar">
  <div class="primary">
    <a class="brand" href="/">{site_name}</a>
    <nav>
      <a href="/w/allpages{asof}">All pages</a>
      {history}
    </nav>
  </div>
  <form class="asof" method="get" action="{action}">
    <label>As of <input type="date" name="asof" value="{date}"></label>
    <button type="submit">Go</button>
    {now}
  </form>
</header>"#,
        asof = asof_query,
        site_name = escape(site_name),
        history = history,
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

fn instance_footer(
    site: &wikimak_wikitext::SiteConfig,
    ts: Option<i64>,
    source_title: Option<&str>,
) -> String {
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
    let source = if site.server.is_empty() {
        String::new()
    } else if let Some(title) = source_title {
        let path = wikimak_wikitext::html::encode_path(title);
        format!(
            r#" · <a href="{server}/wiki/{path}">source page</a> · <a href="{server}/w/index.php?title={query}&action=history">revision history</a>"#,
            server = escape(&site.server),
            query = urlq(title),
        )
    } else {
        format!(r#" · <a href="{}">source site</a>"#, escape(&site.server))
    };
    format!(
        r#"<footer class="site"><span>{}</span> · <span>{}</span>{source} · <span>Text from Wikimedia, available under <a href="https://creativecommons.org/licenses/by-sa/4.0/">CC BY-SA 4.0</a>; additional terms may apply.</span></footer>"#,
        escape(&name),
        escape(&tau),
    )
}

fn not_found_page(app: &App, query: &HashMap<String, String>) -> Resp {
    let (ts, _asof) = asof_from_query(query);
    let body = match app.view(ts) {
        Ok(view) => {
            let site = view.site();
            format!(
                "{}<h1 class=\"page-title\">Not found</h1><p>No such route.</p>{}",
                header_bar(app, site, "/w/allpages", ts, ""),
                instance_footer(site, ts, None),
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

#[cfg(test)]
mod tests {
    use super::{
        dispatch, open_archive_request, ArchiveRequestCache, CachedPageStore, ServerApp,
        ServerSource,
    };
    use crate::archive::{
        ArchiveWriter, CompressionSettings, ManifestRecord, Record, RevisionRecord,
        SiteInfoRecord,
    };
    use crate::installation_lifecycle::{install, selected_generation_paths};
    use crate::{ContributorMeta, RevisionMeta};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, HashMap};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use wikimak_scribunto::LuaInvoker;
    use wikimak_wikitext::{render, NamespaceInfo, PageStore, RenderOptions, SiteConfig, Title};

    #[test]
    fn media_request_does_not_open_archive_source() {
        let temporary = tempfile::tempdir().unwrap();
        let mut media = wikimak_media::MediaStore::new(
            temporary.path().join("media-cache"),
            Vec::new(),
        );
        #[cfg(feature = "fetch")]
        media.set_allow_fetch(false);
        let server = ServerApp {
            source: ServerSource::Archive(
                temporary
                    .path()
                    .join("archive-that-must-not-be-opened.swdump"),
            ),
            media: Arc::new(media),
            archive_cache: Mutex::new(ArchiveRequestCache::new()),
            depot_bytecode_cache: None,
        };

        let response = dispatch(&server, "/w/media/Lestvitsa-1387.jpg", "w=orig");

        // An attempted archive open would fail with 503. The media-only
        // route serves its normal offline placeholder without opening it.
        assert_eq!(response.status_code(), 200);
    }

    struct CountingPageStore {
        pages: HashMap<Title, String>,
        calls: RefCell<HashMap<Title, usize>>,
        site: SiteConfig,
    }

    impl CountingPageStore {
        fn new() -> Self {
            let mut namespaces = BTreeMap::new();
            for (id, canonical) in [(0, ""), (10, "Template"), (828, "Module")] {
                namespaces.insert(
                    id,
                    NamespaceInfo {
                        id,
                        canonical: canonical.into(),
                        localized: canonical.into(),
                        aliases: Vec::new(),
                        case_first_letter: true,
                    },
                );
            }
            Self {
                pages: HashMap::new(),
                calls: RefCell::new(HashMap::new()),
                site: SiteConfig {
                    namespaces,
                    ..Default::default()
                },
            }
        }

        fn add_page(&mut self, title: &str, text: &str) {
            self.pages
                .insert(Title::parse(title, &self.site), text.to_string());
        }

        fn calls_for(&self, title: &str) -> usize {
            let title = Title::parse(title, &self.site);
            self.calls.borrow().get(&title).copied().unwrap_or(0)
        }
    }

    impl PageStore for CountingPageStore {
        fn page_text(&self, title: &Title) -> Option<String> {
            *self.calls.borrow_mut().entry(title.clone()).or_default() += 1;
            self.pages.get(title).cloned()
        }

        fn page_exists(&self, title: &Title) -> bool {
            self.pages.contains_key(title)
        }

        fn site(&self) -> &SiteConfig {
            &self.site
        }

        fn timestamp_micros(&self) -> i64 {
            0
        }
    }

    fn render_request(store: &CountingPageStore, text: &str) {
        let title = Title::parse("Main", store.site());
        let cached = CachedPageStore::new(store);
        let invoker = LuaInvoker::new().unwrap();
        let opts = RenderOptions {
            invoker: Some(&invoker),
            ..Default::default()
        };
        let output = render(&cached, &title, text, &opts);
        assert!(output.misses.failed_invokes.is_empty());
    }

    #[test]
    fn page_text_cache_shares_template_and_lua_lookups_per_request() {
        let mut store = CountingPageStore::new();
        store.add_page("Template:Echo", "echo");
        store.add_page(
            "Module:Probe",
            r#"
                local p = {}
                function p.main()
                    local present = mw.title.new("Template:Echo"):getContent() or "missing"
                    local absent = mw.title.new("Template:Missing"):getContent() or "missing"
                    return present .. "/" .. absent
                end
                return p
            "#,
        );
        let text = concat!(
            "{{Echo}} {{Template:Echo}} ",
            "{{Missing}} {{Template:Missing}} ",
            "{{#invoke:Probe|main}} {{#invoke:Probe|main}}"
        );

        render_request(&store, text);
        assert_eq!(store.calls_for("Template:Echo"), 1);
        assert_eq!(store.calls_for("Template:Missing"), 1);
        assert_eq!(store.calls_for("Module:Probe"), 1);

        render_request(&store, text);
        assert_eq!(store.calls_for("Template:Echo"), 2);
        assert_eq!(store.calls_for("Template:Missing"), 2);
        assert_eq!(store.calls_for("Module:Probe"), 2);
    }

    fn candidate(root: &Path, name: &str, text: &str) -> (PathBuf, PathBuf) {
        let archive = root.join(format!("{name}.swdump"));
        let title = archive.with_extension("swtitle");
        let output = crate::archive_set::ArchiveSetOutput::new_in(root, 1 << 20).unwrap();
        let mut writer = ArchiveWriter::with_ref_prefix(
            output,
            128,
            CompressionSettings::default(),
            b"serve request lease fixture reference",
        )
        .unwrap();
        writer
            .write(&Record::PageState {
                page_id: 1,
                timestamp_micros: 2,
                title: "Main Page".into(),
                namespace: Some(0),
                deleted: false,
            })
            .unwrap();
        writer
            .write(&Record::Revision {
                page_id: 1,
                revision: RevisionRecord {
                    meta: RevisionMeta {
                        rev_id: 1,
                        parent_id: 0,
                        ts: chrono::DateTime::from_timestamp_micros(1).unwrap(),
                        contributor: ContributorMeta::Named {
                            username: "fixture".into(),
                            user_id: 1,
                        },
                        comment: "fixture".into(),
                        sha1: String::new(),
                        flags: 0,
                        text_len: text.len() as u64,
                    },
                    has_text: true,
                    text: text.as_bytes().to_vec(),
                    visibility: None,
                    history: None,
                },
            })
            .unwrap();
        writer
            .write(&Record::Manifest {
                timestamp_micros: 1,
                manifest: ManifestRecord {
                    wiki_db: "testwiki".into(),
                    content_snapshot: name.into(),
                    metadata_snapshot: name.into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: 1,
                site_info: SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "testwiki".into(),
                    base: "https://example.invalid/wiki/Main_Page".into(),
                    generator: "MediaWiki".into(),
                    case: "first-letter".into(),
                    language: "en".into(),
                    rtl: false,
                    server: "https://example.invalid".into(),
                    script_path: "/w".into(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (output, _) = writer.finish().unwrap();
        output.finish().unwrap().persist(&archive).unwrap();
        let generation_id = crate::generation::GenerationId::from_plan_bytes(
            format!("serve-request-lease-{name}").as_bytes(),
        );
        crate::title_index::build(&archive, &title, &generation_id).unwrap();
        (archive, title)
    }

    #[test]
    fn archive_request_lease_is_dropped_before_update_and_new_selector_read() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate_root = temporary.path().join("candidates");
        let destination = temporary.path().join("library").join("testwiki.swdump");
        std::fs::create_dir_all(&candidate_root).unwrap();

        let (first_archive, first_title) = candidate(
            &candidate_root,
            "first",
            "first generation text",
        );
        install(first_archive, first_title, &destination).unwrap();

        {
            let archive = open_archive_request(&destination).unwrap();
            assert_eq!(
                archive.page_text_at(1, i64::MAX).unwrap().as_deref(),
                Some(b"first generation text".as_slice())
            );
        }

        let (selected_archive, _) = selected_generation_paths(&destination)
            .unwrap()
            .expect("the first candidate must be selected");
        assert!(
            crate::archive::try_acquire_archive_cleanup_lease(&selected_archive)
                .unwrap()
                .is_some(),
            "the request-scoped reader must release its shared lease without an engine restart"
        );

        let (second_archive, second_title) = candidate(
            &candidate_root,
            "second",
            "second generation text",
        );
        install(second_archive, second_title, &destination).unwrap();

        let archive = open_archive_request(&destination).unwrap();
        assert_eq!(
            archive.page_text_at(1, i64::MAX).unwrap().as_deref(),
            Some(b"second generation text".as_slice())
        );
    }

    #[test]
    fn archive_request_cache_reuses_and_refreshes_on_selector_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate_root = temporary.path().join("candidates");
        let destination = temporary.path().join("library").join("testwiki.swdump");
        std::fs::create_dir_all(&candidate_root).unwrap();

        let (first_archive, first_title) = candidate(
            &candidate_root,
            "cache-first",
            "first generation text",
        );
        install(first_archive, first_title, &destination).unwrap();

        let mut cache = ArchiveRequestCache::new();
        let discoveries_before = crate::archive::IndexedArchiveSet::layout_discovery_count();
        let first = cache.open(&destination).unwrap();
        let discoveries_after_first =
            crate::archive::IndexedArchiveSet::layout_discovery_count();
        assert_eq!(
            discoveries_after_first,
            discoveries_before.saturating_add(1),
            "the cold request should validate the immutable archive layout exactly once"
        );
        let first_generation_id = first.generation_id().as_str().to_owned();
        let title_index_identity = first.title_index_identity();
        let again = cache.open(&destination).unwrap();
        assert!(!Arc::ptr_eq(&first, &again));
        assert_eq!(title_index_identity, again.title_index_identity());
        assert_eq!(
            crate::archive::IndexedArchiveSet::layout_discovery_count(),
            discoveries_after_first,
            "a repeated request must use the cached layout instead of reopening every segment"
        );
        drop(again);
        drop(first);

        let (second_archive, second_title) = candidate(
            &candidate_root,
            "cache-second",
            "second generation text",
        );
        install(second_archive, second_title, &destination).unwrap();

        let discoveries_before_selector_refresh =
            crate::archive::IndexedArchiveSet::layout_discovery_count();
        let refreshed = cache.open(&destination).unwrap();
        assert_eq!(
            crate::archive::IndexedArchiveSet::layout_discovery_count(),
            discoveries_before_selector_refresh.saturating_add(1),
            "a selector change must discard the old layout and validate the new generation"
        );
        assert_ne!(refreshed.generation_id().as_str(), first_generation_id);
        assert_eq!(
            refreshed.page_text_at(1, i64::MAX).unwrap().as_deref(),
            Some(b"second generation text".as_slice())
        );
        let refreshed_again = cache.open(&destination).unwrap();
        assert!(!Arc::ptr_eq(&refreshed, &refreshed_again));
        assert_eq!(
            refreshed.title_index_identity(),
            refreshed_again.title_index_identity()
        );
    }

    #[cfg(unix)]
    #[test]
    fn archive_request_cache_recovers_from_same_path_root_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate_root = temporary.path().join("candidates");
        let destination = temporary.path().join("library").join("testwiki.swdump");
        std::fs::create_dir_all(&candidate_root).unwrap();

        let (archive, title) = candidate(
            &candidate_root,
            "cache-root-replacement",
            "generation text",
        );
        install(archive, title, &destination).unwrap();

        let mut cache = ArchiveRequestCache::new();
        let initial = cache.open(&destination).unwrap();
        drop(initial);
        let (selected_archive, _) = selected_generation_paths(&destination)
            .unwrap()
            .expect("the generation must be selected");
        let displaced = temporary.path().join("displaced-generation");
        std::fs::rename(&selected_archive, &displaced).unwrap();
        std::fs::create_dir(&selected_archive).unwrap();
        for entry in std::fs::read_dir(&displaced).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            std::fs::copy(entry.path(), selected_archive.join(entry.file_name())).unwrap();
        }

        let discoveries_before_recovery =
            crate::archive::IndexedArchiveSet::layout_discovery_count();
        let recovered = cache.open(&destination).unwrap();
        assert_eq!(
            recovered.page_text_at(1, i64::MAX).unwrap().as_deref(),
            Some(b"generation text".as_slice())
        );
        assert_eq!(
            crate::archive::IndexedArchiveSet::layout_discovery_count(),
            discoveries_before_recovery.saturating_add(1),
            "same-path replacement must trigger one cold layout discovery"
        );
        drop(recovered);

        let later = cache.open(&destination).unwrap();
        assert_eq!(
            crate::archive::IndexedArchiveSet::layout_discovery_count(),
            discoveries_before_recovery.saturating_add(1),
            "the recovered layout must be reused on later requests"
        );
        drop(later);
    }

    #[test]
    fn archive_request_cache_refreshes_same_generation_backrefs_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate_root = temporary.path().join("candidates");
        let destination = temporary.path().join("library").join("testwiki.swdump");
        std::fs::create_dir_all(&candidate_root).unwrap();

        let (archive, title) = candidate(
            &candidate_root,
            "cache-backrefs",
            "generation text",
        );
        install(archive, title, &destination).unwrap();

        let mut cache = ArchiveRequestCache::new();
        let without_backrefs = cache.open(&destination).unwrap();
        let generation_id = without_backrefs.generation_id().as_str().to_owned();
        assert_eq!(without_backrefs.category_member_titles(1).unwrap(), None);
        let (selected_archive, selected_title) = selected_generation_paths(&destination)
            .unwrap()
            .expect("the generation must be selected");
        drop(without_backrefs);

        let sidecar = destination.with_extension("swrefs");
        crate::backrefs::build(&selected_archive, &selected_title, &sidecar).unwrap();
        let discoveries_after_backref_build =
            crate::archive::IndexedArchiveSet::layout_discovery_count();
        let with_backrefs = cache.open(&destination).unwrap();
        assert_eq!(with_backrefs.generation_id().as_str(), generation_id);
        assert_eq!(with_backrefs.category_member_titles(1).unwrap(), Some(Vec::new()));
        assert_eq!(
            crate::archive::IndexedArchiveSet::layout_discovery_count(),
            discoveries_after_backref_build,
            "same-generation backreference refresh must not rediscover immutable archive layout"
        );
        drop(with_backrefs);

        let replacement = temporary.path().join("replacement.swrefs");
        std::fs::write(&replacement, b"not a backref sidecar").unwrap();
        std::fs::rename(&replacement, &sidecar).unwrap();
        let discoveries_before_replacement =
            crate::archive::IndexedArchiveSet::layout_discovery_count();
        let after_replacement = cache.open(&destination).unwrap();
        assert_eq!(after_replacement.category_member_titles(1).unwrap(), None);
        assert_eq!(
            crate::archive::IndexedArchiveSet::layout_discovery_count(),
            discoveries_before_replacement,
            "backreference replacement must refresh only the sidecar metadata"
        );
    }

    #[test]
    fn archive_request_cache_does_not_block_idle_update_maintenance() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate_root = temporary.path().join("candidates");
        let destination = temporary.path().join("library").join("testwiki.swdump");
        std::fs::create_dir_all(&candidate_root).unwrap();

        let (archive, title) = candidate(
            &candidate_root,
            "cache-maintenance",
            "generation text",
        );
        install(archive, title, &destination).unwrap();

        let mut cache = ArchiveRequestCache::new();
        let cached = cache.open(&destination).unwrap();
        let generation_id = cached.generation_id().as_str().to_owned();
        let next_generation_id = crate::generation::GenerationId::from_plan_bytes(
            b"cache-maintenance-next",
        );

        let (selected_archive, _) = selected_generation_paths(&destination)
            .unwrap()
            .expect("the initial generation must be selected");
        assert!(
            crate::archive::try_acquire_archive_cleanup_lease(&selected_archive)
                .unwrap()
                .is_none(),
            "the active request must hold the generation lease"
        );
        drop(cached);

        let guard = crate::installation_lifecycle::begin_update_maintenance(
            &destination,
            &generation_id,
            next_generation_id.as_str(),
            "cache-maintenance-update",
        )
        .unwrap();
        drop(guard);
    }
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
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
  line-height: 1.6; color: #202122; background: #fff;
  margin: 0; padding: 0 0 3rem;
}
.bar {
  display: flex; flex-wrap: wrap; gap: 1rem; align-items: center;
  justify-content: space-between;
  padding: 0.65rem max(1rem, calc((100vw - 100rem) / 2));
  background: #f8f9fa; border-bottom: 1px solid #a2a9b1;
  font-size: 0.9rem;
}
.bar .primary, .bar nav { display: flex; gap: 1.25rem; align-items: center; }
.bar .brand { color: #202122; font-family: Georgia, serif; font-size: 1.15rem; font-weight: 600; }
.bar .asof { display: flex; gap: 0.4rem; align-items: center; }
.bar .now { margin-left: 0.5rem; }
a { color: #3366cc; text-decoration: none; }
a:hover { text-decoration: underline; }
a.new, .new a { color: #ba0000; }
h1.page-title {
  font-family: 'Linux Libertine', Georgia, serif; font-weight: normal;
  border-bottom: 1px solid #a2a9b1;
  margin: 1.25rem auto 0.75rem; padding: 0 1rem 0.25rem; max-width: 100rem;
}
.content, .redirect-note, .misses, .catlinks, .allpages, .history, .filter, .noarticle {
  margin-left: auto; margin-right: auto; max-width: 100rem;
  padding-left: 1rem; padding-right: 1rem;
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
.content table.wikitable { background: #f8f9fa; border: 1px solid #a2a9b1; }
.content table.wikitable td, .content table.wikitable th {
  border: 1px solid #a2a9b1; padding: 0.3rem 0.6rem;
}
.content table.wikitable th { background: #eaecf0; }
.content img { max-width: 100%; height: auto; }
.content .floatright, .content .tright { float: right; clear: right; margin: 0 0 1rem 1rem; }
.content .floatleft, .content .tleft { float: left; clear: left; margin: 0 1rem 1rem 0; }
.content .center { margin-left: auto; margin-right: auto; text-align: center; }
.content pre { overflow: auto; white-space: pre-wrap; }
.mw-inputbox-unavailable {
  margin: 0.75rem 0; padding: 0.6rem 0.75rem;
  border: 1px solid #c8ccd1; background: #f8f9fa; color: #54595d;
  font-size: 0.9rem;
}
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
ul.allpages { columns: 20rem; column-gap: 2rem; }
ul.allpages li { break-inside: avoid; }
.history .comment { color: #54595d; font-style: italic; }
.filter { font-family: sans-serif; font-size: 0.9rem; margin-bottom: 0.8rem; }
.filter input { min-width: min(28rem, 65vw); padding: 0.35rem 0.5rem; }
.filter button, .asof button { padding: 0.3rem 0.65rem; }
.result-note { max-width: 100rem; margin: 0.5rem auto; padding: 0 1rem; color: #54595d; }
footer.site {
  max-width: 100rem; margin: 2rem auto 0;
  padding: 0.6rem 1rem 0; border-top: 1px solid #a2a9b1;
  font-family: sans-serif; font-size: 0.8rem; color: #54595d;
}
[dir="rtl"] .content .infobox { float: left; margin: 0 1rem 1rem 0; }
"#;
