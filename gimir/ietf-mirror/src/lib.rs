//! IETF internet-drafts mirror.
//!
//! Documents are stored as independent zstd-compressed frames in sequential
//! archive files (`<root>/archive/archive-NNNN`). A SQLite index (`meta.db`)
//! records `(archive_id, offset, comp_len, raw_len)` per revision — reads
//! seek directly, decompress one frame. No chains, no delta encoding, no
//! tiering: IETF drafts are small text files with a handful of revisions
//! each, not Wikipedia-scale revision history.
//!
//! Discovery: the live index (`/id/all_id.txt`) lists each draft once at
//! its LATEST revision. History is enumerated (`00..NN`) from the head
//! number; unseen candidates are fetched, 404s are watermarked missing.

pub mod readout;
#[cfg(feature = "fetch")]
mod cli;
#[cfg(feature = "fetch")]
pub use cli::cli_main;

use std::path::PathBuf;
use std::time::Duration;

#[cfg(feature = "fetch")]
use reqwest::blocking::Client;
use rusqlite::Connection;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("zstd: {0}")]
    Zstd(String),
    #[cfg(feature = "fetch")]
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("http status {status} for {url}")]
    HttpStatus { status: u16, url: String },
    #[error("parse: {0}")]
    Parse(String),
    #[error("mirror {0} is locked by another process")]
    MirrorLocked(PathBuf),
    #[error("mirror opened read-only (shared lock): updating requires Mirror::open")]
    ReadOnly,
}

/// Configuration for the HTTP side.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    pub base_url: String,
    pub delay: Duration,
    pub retries: u32,
    pub backoff: Duration,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            base_url: "https://www.ietf.org".to_string(),
            delay: Duration::from_millis(250),
            retries: 3,
            backoff: Duration::from_millis(500),
        }
    }
}

/// Store configuration — just the root directory.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    pub root: PathBuf,
}

impl MirrorConfig {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Counters from one [`Mirror::update`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateStats {
    pub drafts_seen: u64,
    pub drafts_new: u64,
    pub revisions_fetched: u64,
    pub revisions_skipped: u64,
    pub revisions_missing: u64,
    pub index_not_modified: bool,
}

/// Live progress for the `update` callback.
#[derive(Debug, Clone)]
pub struct Progress<'a> {
    pub drafts_done: u64,
    pub drafts_total: u64,
    pub label: &'a str,
    pub fetched: bool,
    pub fetched_total: u64,
    pub skipped_total: u64,
    pub missing_total: u64,
}

/// One entry of a draft's history (newest-first from [`Mirror::history`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionEntry {
    pub rev: String,
    pub date: Option<String>,
    pub text: Vec<u8>,
}

/// Maximum archive file size before rolling to a new one.
const ARCHIVE_MAX: u64 = 64 * 1024 * 1024;

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS revisions (
        name TEXT NOT NULL,
        rev TEXT NOT NULL,
        archive_id INTEGER NOT NULL,
        offset INTEGER NOT NULL,
        comp_len INTEGER NOT NULL,
        raw_len INTEGER NOT NULL,
        date TEXT,
        missing INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(name, rev)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS fetch_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID",
];

pub struct Mirror {
    conn: Connection,
    root: PathBuf,
    _lock: std::fs::File,
    read_only: bool,
    #[cfg(feature = "fetch")]
    cur_archive_id: u64,
    #[cfg(feature = "fetch")]
    cur_archive_size: u64,
}

fn flock_root(root: &std::path::Path, op: libc::c_int) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true).truncate(false).write(true)
        .open(root.join(".lock"))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) };
    if rc != 0 {
        return Err(Error::MirrorLocked(root.to_path_buf()));
    }
    Ok(f)
}

#[cfg(feature = "fetch")]
fn current_archive(root: &std::path::Path) -> Result<(u64, u64)> {
    let dir = root.join("archive");
    let mut max_id = 0u64;
    let mut max_size = 0u64;
    if dir.exists() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = name.strip_prefix("archive-") {
                if let Ok(id) = n.parse::<u64>() {
                    let sz = entry.metadata()?.len();
                    if id > max_id || (id == max_id && sz > max_size) {
                        max_id = id;
                        max_size = sz;
                    }
                }
            }
        }
    }
    Ok((max_id, max_size))
}

#[cfg(feature = "fetch")]
struct Pending {
    name: String,
    rev: String,
    archive_id: i64,
    offset: i64,
    comp_len: i64,
    raw_len: i64,
    date: Option<String>,
    missing: bool,
}

#[cfg(feature = "fetch")]
const FLUSH_EVERY_DRAFTS: u64 = 64;

impl Mirror {
    pub fn open(cfg: MirrorConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;
        std::fs::create_dir_all(cfg.root.join("archive"))?;
        let lock = flock_root(&cfg.root, libc::LOCK_EX)?;
        let conn = Connection::open(cfg.root.join("meta.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        for stmt in DDL {
            conn.execute(stmt, [])?;
        }
        #[cfg(feature = "fetch")]
        let (cur_id, cur_size) = current_archive(&cfg.root)?;
        Ok(Mirror {
            conn,
            root: cfg.root,
            _lock: lock,
            read_only: false,
            #[cfg(feature = "fetch")]
            cur_archive_id: cur_id,
            #[cfg(feature = "fetch")]
            cur_archive_size: cur_size,
        })
    }

    pub fn open_read(cfg: MirrorConfig) -> Result<Self> {
        if !cfg.root.join("meta.db").exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no mirror at {}", cfg.root.display()),
            )));
        }
        let lock = flock_root(&cfg.root, libc::LOCK_SH)?;
        let conn = Connection::open_with_flags(
            cfg.root.join("meta.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        Ok(Mirror {
            conn,
            root: cfg.root,
            _lock: lock,
            read_only: true,
            #[cfg(feature = "fetch")]
            cur_archive_id: 0,
            #[cfg(feature = "fetch")]
            cur_archive_size: 0,
        })
    }

    #[cfg(feature = "fetch")]
    fn write_frame(&mut self, text: &[u8]) -> Result<(u64, u64, u64, u64)> {
        let comp = zstd::bulk::compress(text, 3)
            .map_err(|e| Error::Zstd(e.to_string()))?;
        let raw_len = text.len() as u64;
        let comp_len = comp.len() as u64;

        if self.cur_archive_size > 0 && self.cur_archive_size + comp_len > ARCHIVE_MAX {
            self.cur_archive_id += 1;
            self.cur_archive_size = 0;
        }

        let archive_id = self.cur_archive_id;
        let offset = self.cur_archive_size;

        let path = self.root
            .join("archive")
            .join(format!("archive-{:04}", archive_id));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true).append(true).open(&path)?;
        file.write_all(&comp)?;
        self.cur_archive_size += comp_len;

        Ok((archive_id, offset, comp_len, raw_len))
    }

    fn read_frame(&self, archive_id: u64, offset: u64, comp_len: usize, raw_len: usize) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.root
            .join("archive")
            .join(format!("archive-{:04}", archive_id));
        let mut file = std::fs::File::open(&path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; comp_len];
        file.read_exact(&mut buf)?;
        zstd::bulk::decompress(&buf, raw_len)
            .map_err(|e| Error::Zstd(e.to_string()))
    }

    #[cfg(feature = "fetch")]
    pub fn update(
        &mut self,
        client: &Client,
        cfg: &FetchConfig,
        mut progress: impl FnMut(&Progress),
    ) -> Result<UpdateStats> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut stats = UpdateStats::default();
        let Some((index, etag, last_modified)) = self.fetch_index_conditional(client, cfg)?
        else {
            stats.index_not_modified = true;
            return Ok(stats);
        };
        let total = index.len() as u64;
        let mut pacer = Pacer::new(cfg.delay);
        let mut done = 0u64;

        let mut pending: Vec<Pending> = Vec::new();
        let mut drafts_since_flush = 0u64;

        for (name, (latest, date)) in &index {
            stats.drafts_seen += 1;
            done += 1;
            let latest_n: u8 = latest
                .parse()
                .map_err(|_| Error::Parse(format!("bad revision {latest:?} for {name}")))?;
            for n in 0..=latest_n {
                let rev = format!("{n:02}");
                if self.rev_seen(name, &rev)? {
                    stats.revisions_skipped += 1;
                    progress(&Progress {
                        drafts_done: done, drafts_total: total,
                        label: &format!("{name}-{rev}"), fetched: false,
                        fetched_total: stats.revisions_fetched,
                        skipped_total: stats.revisions_skipped,
                        missing_total: stats.revisions_missing,
                    });
                    continue;
                }
                let label = format!("{name}-{rev}");
                let url = format!("{}/archive/id/{label}.txt", cfg.base_url);
                pacer.pace();
                let d = if rev == *latest { date.as_deref() } else { None };
                match http_get(client, &url, cfg)? {
                    Some(text) => {
                        let (aid, off, clen, rlen) = self.write_frame(&text)?;
                        pending.push(Pending {
                            name: name.clone(), rev: rev.clone(),
                            archive_id: aid as i64, offset: off as i64,
                            comp_len: clen as i64, raw_len: rlen as i64,
                            date: d.map(|s| s.to_string()), missing: false,
                        });
                        stats.revisions_fetched += 1;
                        progress(&Progress {
                            drafts_done: done, drafts_total: total,
                            label: &label, fetched: true,
                            fetched_total: stats.revisions_fetched,
                            skipped_total: stats.revisions_skipped,
                            missing_total: stats.revisions_missing,
                        });
                    }
                    None => {
                        pending.push(Pending {
                            name: name.clone(), rev: rev.clone(),
                            archive_id: 0, offset: 0, comp_len: 0, raw_len: 0,
                            date: d.map(|s| s.to_string()), missing: true,
                        });
                        stats.revisions_missing += 1;
                        progress(&Progress {
                            drafts_done: done, drafts_total: total,
                            label: &label, fetched: false,
                            fetched_total: stats.revisions_fetched,
                            skipped_total: stats.revisions_skipped,
                            missing_total: stats.revisions_missing,
                        });
                    }
                }
            }
            if stats.drafts_new == 0 && stats.revisions_fetched > 0 {
                stats.drafts_new = 1;
            }
            drafts_since_flush += 1;
            if drafts_since_flush >= FLUSH_EVERY_DRAFTS {
                self.commit_pending(&mut pending)?;
                drafts_since_flush = 0;
            }
        }

        self.commit_pending(&mut pending)?;
        self.set_index_validators(etag.as_deref(), last_modified.as_deref())?;
        Ok(stats)
    }

    #[cfg(feature = "fetch")]
    fn commit_pending(&mut self, pending: &mut Vec<Pending>) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut mark = tx.prepare_cached(
                "INSERT OR REPLACE INTO revisions(name, rev, archive_id, offset, comp_len, raw_len, date, missing)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for p in pending.iter() {
                mark.execute(rusqlite::params![
                    p.name, p.rev, p.archive_id, p.offset,
                    p.comp_len, p.raw_len, p.date.as_deref(),
                    if p.missing { 1i64 } else { 0i64 },
                ])?;
            }
        }
        tx.commit()?;
        pending.clear();
        Ok(())
    }

    pub fn drafts(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT name FROM revisions WHERE missing = 0 ORDER BY name",
        )?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn head(&self, name: &str) -> Result<Option<RevisionEntry>> {
        let mut st = self.conn.prepare(
            "SELECT rev, archive_id, offset, comp_len, raw_len, date
             FROM revisions WHERE name = ?1 AND missing = 0
             ORDER BY rev DESC LIMIT 1",
        )?;
        let mut rows = st.query([name])?;
        match rows.next()? {
            Some(r) => {
                let rev: String = r.get(0)?;
                let aid: i64 = r.get(1)?;
                let off: i64 = r.get(2)?;
                let clen: i64 = r.get(3)?;
                let rlen: i64 = r.get(4)?;
                let date: Option<String> = r.get(5)?;
                let text = self.read_frame(aid as u64, off as u64, clen as usize, rlen as usize)?;
                Ok(Some(RevisionEntry { rev, date, text }))
            }
            None => Ok(None),
        }
    }

    pub fn revision(&self, name: &str, rev: &str) -> Result<Option<RevisionEntry>> {
        let mut st = self.conn.prepare(
            "SELECT archive_id, offset, comp_len, raw_len, date
             FROM revisions WHERE name = ?1 AND rev = ?2 AND missing = 0",
        )?;
        let mut rows = st.query(rusqlite::params![name, rev])?;
        match rows.next()? {
            Some(r) => {
                let aid: i64 = r.get(0)?;
                let off: i64 = r.get(1)?;
                let clen: i64 = r.get(2)?;
                let rlen: i64 = r.get(3)?;
                let date: Option<String> = r.get(4)?;
                let text = self.read_frame(aid as u64, off as u64, clen as usize, rlen as usize)?;
                Ok(Some(RevisionEntry { rev: rev.to_string(), date, text }))
            }
            None => Ok(None),
        }
    }

    pub fn history(&self, name: &str) -> Result<Vec<RevisionEntry>> {
        let mut st = self.conn.prepare(
            "SELECT rev, archive_id, offset, comp_len, raw_len, date
             FROM revisions WHERE name = ?1 AND missing = 0
             ORDER BY rev DESC",
        )?;
        let rows = st.query_map([name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (rev, aid, off, clen, rlen, date) = row?;
            let text = self.read_frame(aid as u64, off as u64, clen as usize, rlen as usize)?;
            out.push(RevisionEntry { rev, date, text });
        }
        Ok(out)
    }

    #[cfg(feature = "fetch")]
    fn rev_seen(&self, name: &str, rev: &str) -> Result<bool> {
        let mut st = self.conn.prepare_cached(
            "SELECT COUNT(*) FROM revisions WHERE name = ?1 AND rev = ?2",
        )?;
        let n: u64 = st.query_row(rusqlite::params![name, rev], |r| r.get(0))?;
        Ok(n > 0)
    }

    #[cfg(feature = "fetch")]
    fn fetch_index_conditional(
        &self,
        client: &Client,
        cfg: &FetchConfig,
    ) -> Result<Option<(DraftIndex, Option<String>, Option<String>)>> {
        use reqwest::header;
        #[cfg(feature = "fetch")]
        use std::io::Read;
        let url = format!("{}/id/all_id.txt", cfg.base_url);
        let etag = self.fetch_meta("index_etag")?;
        let last_mod = self.fetch_meta("index_last_modified")?;
        retrying(cfg, || {
            let mut req = client.get(&url);
            if let Some(v) = &etag {
                req = req.header(header::IF_NONE_MATCH, v);
            }
            if let Some(v) = &last_mod {
                req = req.header(header::IF_MODIFIED_SINCE, v);
            }
            let resp = req.send()?;
            let status = resp.status().as_u16();
            if status == 304 {
                return Ok(None);
            }
            if !resp.status().is_success() {
                return Err(Error::HttpStatus { status, url: url.clone() });
            }
            let hdr = |k: header::HeaderName| {
                resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string)
            };
            let (new_etag, new_lm) = (hdr(header::ETAG), hdr(header::LAST_MODIFIED));
            let mut body = Vec::new();
            let mut r = resp;
            r.read_to_end(&mut body)?;
            Ok(Some((parse_index(&String::from_utf8_lossy(&body)), new_etag, new_lm)))
        })
    }

    #[cfg(feature = "fetch")]
    fn fetch_meta(&self, key: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare("SELECT value FROM fetch_meta WHERE key = ?1")?;
        let mut rows = st.query([key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    #[cfg(feature = "fetch")]
    fn set_index_validators(
        &mut self,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM fetch_meta WHERE key IN ('index_etag','index_last_modified')",
            [],
        )?;
        for (k, v) in [("index_etag", etag), ("index_last_modified", last_modified)] {
            if let Some(v) = v {
                tx.execute(
                    "INSERT INTO fetch_meta(key, value) VALUES(?1, ?2)",
                    rusqlite::params![k, v],
                )?;
            }
        }
        Ok(tx.commit()?)
    }
}

pub type DraftIndex = std::collections::BTreeMap<String, (String, Option<String>)>;

#[cfg(feature = "fetch")]
pub fn fetch_index(client: &Client, cfg: &FetchConfig) -> Result<DraftIndex> {
    let url = format!("{}/id/all_id.txt", cfg.base_url);
    let body = http_get(client, &url, cfg)?.ok_or(Error::HttpStatus { status: 404, url })?;
    Ok(parse_index(&String::from_utf8_lossy(&body)))
}

pub fn parse_index(text: &str) -> DraftIndex {
    let mut out: DraftIndex = std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let docname = fields.next().unwrap_or("").trim();
        let date = fields.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let Some((base, rev)) = split_rev(docname) else { continue };
        match out.entry(base.to_string()) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert((rev.to_string(), date));
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                if rev > o.get().0.as_str() {
                    o.insert((rev.to_string(), date));
                }
            }
        }
    }
    out
}

fn split_rev(docname: &str) -> Option<(&str, &str)> {
    let (base, rev) = docname.rsplit_once('-')?;
    if rev.len() == 2 && rev.bytes().all(|b| b.is_ascii_digit()) && !base.is_empty() {
        Some((base, rev))
    } else {
        None
    }
}

#[cfg(feature = "fetch")]
struct Pacer {
    delay: Duration,
    last: Option<std::time::Instant>,
}

#[cfg(feature = "fetch")]
impl Pacer {
    fn new(delay: Duration) -> Self {
        Pacer { delay, last: None }
    }
    fn pace(&mut self) {
        if let Some(t) = self.last {
            let elapsed = t.elapsed();
            if elapsed < self.delay {
                std::thread::sleep(self.delay - elapsed);
            }
        }
        self.last = Some(std::time::Instant::now());
    }
}

#[cfg(feature = "fetch")]
fn retrying<T>(cfg: &FetchConfig, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 0u32;
    loop {
        match f() {
            Err(e) if attempt < cfg.retries && retryable(&e) => {
                std::thread::sleep(cfg.backoff * 2u32.saturating_pow(attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

#[cfg(feature = "fetch")]
fn retryable(e: &Error) -> bool {
    match e {
        Error::Http(_) => true,
        Error::HttpStatus { status, .. } => *status >= 500,
        _ => false,
    }
}

#[cfg(feature = "fetch")]
fn http_get(client: &Client, url: &str, cfg: &FetchConfig) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    retrying(cfg, || {
        let resp = client.get(url).send()?;
        let status = resp.status();
        if status.as_u16() == 404 {
            let _ = resp.bytes();
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Error::HttpStatus { status: status.as_u16(), url: url.to_string() });
        }
        let mut body = Vec::new();
        let mut r = resp;
        r.read_to_end(&mut body)?;
        Ok(Some(body))
    })
}
