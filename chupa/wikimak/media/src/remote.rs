//! Ranged Kiwix importer.
//!
//! ZIM archives are indexed containers rather than sequential streams. This
//! importer reads bounded HTTP ranges, keeps directory metadata in memory,
//! and writes the final hash/offset/data files directly. It never creates a
//! local ZIM copy.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, RANGE, RETRY_AFTER};
use reqwest::Url;

use crate::kiwix::{image_identity, legacy_image_key, KiwixImagePreference};
use crate::packed::{media_title_hash, KiwixPackStats, MediaRepositoryWriter, Reservation};

const CATALOG_URL: &str = "https://download.kiwix.org/zim/wikipedia/";
const RANGE_WINDOW: u64 = 16 * 1024 * 1024;
const CLUSTER_BATCH: u64 = 32 * 1024 * 1024;
const MAX_RETRIES: usize = 5;
#[cfg(test)]
const MAX_REDIRECTS: usize = 5;
const MAX_FALLBACK_RETRY: Duration = Duration::from_secs(30);
const ZIM_MAGIC: u32 = 72_173_914;

#[derive(Debug, thiserror::Error)]
pub enum RemoteKiwixError {
    #[error("kiwix catalogue: {0}")]
    Http(#[from] reqwest::Error),
    #[error("kiwix catalogue: {0}")]
    Io(#[from] io::Error),
    #[error("kiwix catalogue: {0}")]
    Parse(String),
}

#[derive(Debug, Clone)]
pub struct KiwixRelease {
    pub name: String,
    pub url: String,
}

/// Choose the newest full, image-bearing Wikipedia ZIM for a database name.
pub fn discover_latest(client: &Client, dbname: &str) -> Result<KiwixRelease, RemoteKiwixError> {
    let lang = dbname
        .strip_suffix("wiki")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RemoteKiwixError::Parse(format!("{dbname} is not a Wikipedia dbname")))?;
    let body = get_bytes(client, CATALOG_URL, Some(16 * 1024 * 1024))?;
    let prefix = format!("wikipedia_{lang}_all_maxi_");
    let mut newest: Option<String> = None;
    for href in html_hrefs(&body) {
        if !href.starts_with(&prefix) || !href.ends_with(".zim") {
            continue;
        }
        let date = &href[prefix.len()..href.len() - 4];
        if date.len() != 7
            || date.as_bytes()[4] != b'-'
            || !date[..4].bytes().all(|b| b.is_ascii_digit())
            || !date[5..].bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        if newest.as_ref().is_none_or(|current| date > current.as_str()) {
            newest = Some(date.to_string());
        }
    }
    let date = newest.ok_or_else(|| {
        RemoteKiwixError::Parse(format!(
            "no all-maxi image ZIM is listed for {dbname} at {CATALOG_URL}"
        ))
    })?;
    let name = format!("{prefix}{date}.zim");
    Ok(KiwixRelease {
        url: format!("{CATALOG_URL}{name}"),
        name,
    })
}

pub struct RemoteKiwixImageSource {
    client: Client,
    url: String,
    file_size: u64,
    checksum_pos: u64,
    cluster_offsets: Vec<u64>,
    entries: Vec<RemoteImageEntry>,
    open_stats: HttpStats,
}

#[derive(Debug, Clone, Copy, Default)]
struct HttpStats {
    bytes: u64,
    requests: u64,
    retries: u64,
}

impl HttpStats {
    fn add_response(&mut self, response: &RangeResponse) {
        self.bytes += response.bytes.len() as u64;
        self.requests += response.requests;
        self.retries += response.retries;
    }

    fn add(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.requests += other.requests;
        self.retries += other.retries;
    }
}

#[derive(Debug, Clone)]
struct RemoteImageEntry {
    key: String,
    hash: u64,
    legacy_hash: Option<u64>,
    file_type: String,
    preference: KiwixImagePreference,
    cluster: u32,
    blob: u32,
}

struct RemoteHeader {
    article_count: u32,
    cluster_count: u32,
    url_ptr_pos: u64,
    title_ptr_pos: u64,
    cluster_ptr_pos: u64,
    mime_list_pos: u64,
    checksum_pos: u64,
}

impl RemoteKiwixImageSource {
    pub fn open(client: Client, url: impl Into<String>) -> Result<Self, RemoteKiwixError> {
        let requested_url = url.into();
        let first = resolve_initial_range(&client, &requested_url, 0, 80)?;
        let mut open_stats = HttpStats::default();
        open_stats.add_response(&first);
        let url = first.final_url.clone();
        let requested = Url::parse(&requested_url)
            .map_err(|error| RemoteKiwixError::Parse(format!("invalid Kiwix URL: {error}")))?;
        let final_url = Url::parse(&url)
            .map_err(|error| RemoteKiwixError::Parse(format!("invalid resolved Kiwix URL: {error}")))?;
        if requested.scheme() == "https" && final_url.scheme() != "https" {
            return Err(RemoteKiwixError::Parse(
                "Kiwix HTTPS source resolved to a non-HTTPS URL".into(),
            ));
        }
        let file_size = first.total;
        let header = parse_header(&first.bytes, file_size)?;
        let pointer_end = [
            header.url_ptr_pos.checked_add(u64::from(header.article_count) * 8),
            header.cluster_ptr_pos.checked_add(u64::from(header.cluster_count) * 8),
            (header.title_ptr_pos > 0 && header.title_ptr_pos < file_size)
                .then_some(header.title_ptr_pos.checked_add(u64::from(header.article_count) * 4))
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .max()
        .ok_or_else(|| RemoteKiwixError::Parse("ZIM pointer tables are missing".into()))?;
        // The pointer tables are commonly stored near the end of the ZIM;
        // using their position as the MIME-list end would request gigabytes.
        // The MIME list is tiny and is terminated by zero padding, so one
        // bounded window is sufficient.
        let mime_end = header
            .mime_list_pos
            .checked_add(RANGE_WINDOW)
            .ok_or_else(|| RemoteKiwixError::Parse("ZIM MIME range overflows".into()))?
            .min(file_size);
        let (url_pointers, url_pointer_stats) = read_u64_table(
            &client,
            &url,
            header.url_ptr_pos,
            header.article_count as usize,
            file_size,
        )?;
        open_stats.add(url_pointer_stats);
        let (cluster_offsets, cluster_pointer_stats) = read_u64_table(
            &client,
            &url,
            header.cluster_ptr_pos,
            header.cluster_count as usize,
            file_size,
        )?;
        open_stats.add(cluster_pointer_stats);
        let mime_response = range(&client, &url, header.mime_list_pos, mime_end, Some(file_size))?;
        open_stats.add_response(&mime_response);
        let mimes = parse_mimes(&mime_response.bytes);
        cluster_offsets.first().ok_or_else(|| {
            RemoteKiwixError::Parse("ZIM has no clusters".into())
        })?;
        let directory_start = *url_pointers
            .first()
            .ok_or_else(|| RemoteKiwixError::Parse("ZIM URL pointer table is empty".into()))?;
        if pointer_end > file_size
            || directory_start < 80
            || header.url_ptr_pos <= directory_start
            || header.url_ptr_pos > header.checksum_pos
        {
            return Err(RemoteKiwixError::Parse("ZIM directory lies outside its pointer table".into()));
        }
        let (entries, directory_stats) = scan_image_entries(
            &client,
            &url,
            &url_pointers,
            header.url_ptr_pos,
            directory_start,
            &mimes,
            file_size,
        )?;
        open_stats.add(directory_stats);
        Ok(Self {
            client,
            url,
            file_size,
            checksum_pos: header.checksum_pos,
            cluster_offsets,
            entries,
            open_stats,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Pack directly from HTTP ranges; no ZIM range is written to disk.
    pub fn pack(&self, output_dir: impl AsRef<Path>) -> Result<KiwixPackStats, RemoteKiwixError> {
        let output_dir = output_dir.as_ref();
        if output_dir.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("packed media output already exists: {}", output_dir.display()),
            )
            .into());
        }
        match self.import_missing(output_dir) {
            Ok(stats) => Ok(stats),
            Err(error) => {
                remove_empty_output_dir(output_dir);
                Err(error)
            }
        }
    }

    /// Add only images absent from an existing immutable repository. Payload
    /// clusters are selected before any payload range is requested, and each
    /// selected cluster is fetched once and decoded once.
    pub fn import_missing(
        &self,
        repository_dir: impl AsRef<Path>,
    ) -> Result<KiwixPackStats, RemoteKiwixError> {
        let mut writer = MediaRepositoryWriter::open(repository_dir)?;
        writer.set_entries_seen(self.entries.len() as u64);
        let mut by_cluster: HashMap<u32, Vec<usize>> = HashMap::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if !matches!(
                writer.reserve_hash_with_legacy(entry.hash, entry.legacy_hash),
                Reservation::Reserved
            ) {
                continue;
            }
            if entry.cluster as usize >= self.cluster_offsets.len() {
                return Err(RemoteKiwixError::Parse(
                    "ZIM image points outside the cluster table".into(),
                ));
            }
            by_cluster.entry(entry.cluster).or_default().push(index);
        }
        let (http_bytes, http_requests, http_retries) = self.visit_clusters(&by_cluster, |cluster, bytes| {
            let decoded = decode_cluster(bytes)?;
            for &index in by_cluster.get(&cluster).into_iter().flatten() {
                let payload = blob(&decoded.bytes, &decoded.offsets, self.entries[index].blob)?;
                if payload.is_empty() {
                    continue;
                }
                writer.append_reserved(
                    &self.entries[index].file_type,
                    &self.entries[index].key,
                    payload,
                )?;
            }
            Ok(())
        })?;
        let mut http_stats = self.open_stats;
        http_stats.add(HttpStats {
            bytes: http_bytes,
            requests: http_requests,
            retries: http_retries,
        });
        writer.stats_mut().http_bytes = http_stats.bytes;
        writer.stats_mut().http_requests = http_stats.requests;
        writer.stats_mut().http_retries = http_stats.retries;
        Ok(writer.finish()?)
    }

    fn visit_clusters<F>(
        &self,
        by_cluster: &HashMap<u32, Vec<usize>>,
        mut visit: F,
    ) -> Result<(u64, u64, u64), RemoteKiwixError>
    where
        F: FnMut(u32, &[u8]) -> Result<(), RemoteKiwixError>,
    {
        let mut ids: Vec<u32> = by_cluster.keys().copied().collect();
        ids.sort_unstable();
        let mut batch = Vec::new();
        let mut start = 0;
        let mut end = 0;
        let mut http_bytes = 0;
        let mut http_requests = 0;
        let mut http_retries = 0;
        for id in ids {
            let cluster_start = self.cluster_offsets[id as usize];
            let cluster_end = if id as usize + 1 < self.cluster_offsets.len() {
                self.cluster_offsets[id as usize + 1]
            } else {
                self.checksum_pos
            };
            let split = !batch.is_empty()
                && (id != batch.last().copied().unwrap_or(id).saturating_add(1)
                    || cluster_end.saturating_sub(start) > CLUSTER_BATCH);
            if split {
                let stats = self.visit_cluster_batch(start, end, &batch, &mut visit)?;
                http_bytes += stats.0;
                http_requests += stats.1;
                http_retries += stats.2;
                batch.clear();
            }
            if batch.is_empty() {
                start = cluster_start;
            }
            end = cluster_end;
            batch.push(id);
        }
        if !batch.is_empty() {
            let stats = self.visit_cluster_batch(start, end, &batch, &mut visit)?;
            http_bytes += stats.0;
            http_requests += stats.1;
            http_retries += stats.2;
        }
        Ok((http_bytes, http_requests, http_retries))
    }

    fn visit_cluster_batch<F>(
        &self,
        start: u64,
        end: u64,
        ids: &[u32],
        visit: &mut F,
    ) -> Result<(u64, u64, u64), RemoteKiwixError>
    where
        F: FnMut(u32, &[u8]) -> Result<(), RemoteKiwixError>,
    {
        eprintln!(
            "kiwix: ranged cluster batch {}..{} ({} bytes)",
            ids.first().copied().unwrap_or_default(),
            ids.last().copied().unwrap_or_default(),
            end.saturating_sub(start)
        );
        let response = range(&self.client, &self.url, start, end, Some(self.file_size))?;
        let bytes_len = response.bytes.len() as u64;
        let bytes = response.bytes;
        for &id in ids {
            let local_start = (self.cluster_offsets[id as usize] - start) as usize;
            let local_end = if id as usize + 1 < self.cluster_offsets.len() {
                (self.cluster_offsets[id as usize + 1] - start) as usize
            } else {
                (self.checksum_pos - start) as usize
            };
            let cluster = bytes
                .get(local_start..local_end)
                .ok_or_else(|| RemoteKiwixError::Parse("cluster outside range response".into()))?;
            visit(id, cluster)?;
        }
        Ok((bytes_len, response.requests, response.retries))
    }
}

fn remove_empty_output_dir(path: &Path) {
    let empty = std::fs::read_dir(path)
        .ok()
        .is_some_and(|mut entries| entries.next().is_none());
    if empty {
        let _ = std::fs::remove_dir(path);
    }
}

struct DecodedCluster {
    bytes: Vec<u8>,
    offsets: Vec<u64>,
}

fn decode_cluster(bytes: &[u8]) -> Result<DecodedCluster, RemoteKiwixError> {
    let details = *bytes.first().ok_or_else(|| RemoteKiwixError::Parse("empty ZIM cluster".into()))?;
    let extended = details & 0x10 != 0;
    let payload = &bytes[1..];
    let decoded = match details & 0x0f {
        0 | 1 => payload.to_vec(),
        4 => {
            let mut out = Vec::new();
            xz2::read::XzDecoder::new(payload).read_to_end(&mut out)?;
            out
        }
        5 => zstd::stream::decode_all(payload).map_err(|error| {
            RemoteKiwixError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
        })?,
        value => return Err(RemoteKiwixError::Parse(format!("unsupported ZIM compression {value}"))),
    };
    let width = if extended { 8 } else { 4 };
    let first = read_offset(&decoded, 0, width)?;
    if first < width as u64 || first % width as u64 != 0 {
        return Err(RemoteKiwixError::Parse("invalid ZIM blob table".into()));
    }
    let count = usize::try_from(first / width as u64)
        .map_err(|_| RemoteKiwixError::Parse("ZIM blob table is too large".into()))?;
    let mut offsets = Vec::with_capacity(count);
    for index in 0..count {
        let offset = read_offset(&decoded, index * width, width)?;
        if offsets.last().is_some_and(|previous| *previous > offset) {
            return Err(RemoteKiwixError::Parse("ZIM blob offsets go backwards".into()));
        }
        offsets.push(offset);
    }
    Ok(DecodedCluster { bytes: decoded, offsets })
}

fn blob<'a>(
    bytes: &'a [u8],
    offsets: &[u64],
    index: u32,
) -> Result<&'a [u8], RemoteKiwixError> {
    let index = index as usize;
    let start = usize::try_from(*offsets.get(index).ok_or_else(|| {
        RemoteKiwixError::Parse("ZIM blob index is outside its cluster".into())
    })?)
    .map_err(|_| RemoteKiwixError::Parse("ZIM blob offset overflows".into()))?;
    let end = usize::try_from(*offsets.get(index + 1).ok_or_else(|| {
        RemoteKiwixError::Parse("ZIM blob end is outside its cluster".into())
    })?)
    .map_err(|_| RemoteKiwixError::Parse("ZIM blob end overflows".into()))?;
    bytes
        .get(start..end)
        .ok_or_else(|| RemoteKiwixError::Parse("ZIM blob is outside its cluster".into()))
}

fn read_offset(bytes: &[u8], start: usize, width: usize) -> Result<u64, RemoteKiwixError> {
    match width {
        4 => bytes
            .get(start..start + 4)
            .map(|value| u32::from_le_bytes(value.try_into().unwrap()) as u64),
        8 => bytes
            .get(start..start + 8)
            .map(|value| u64::from_le_bytes(value.try_into().unwrap())),
        _ => None,
    }
    .ok_or_else(|| RemoteKiwixError::Parse("ZIM offset table is truncated".into()))
}

fn parse_header(bytes: &[u8], file_size: u64) -> Result<RemoteHeader, RemoteKiwixError> {
    if bytes.len() < 80 || u32::from_le_bytes(bytes[0..4].try_into().unwrap()) != ZIM_MAGIC {
        return Err(RemoteKiwixError::Parse("invalid ZIM header".into()));
    }
    let get16 = |start| u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap());
    let get32 = |start| u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
    let get64 = |start| u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap());
    let version = get16(4);
    if version != 5 && version != 6 {
        return Err(RemoteKiwixError::Parse(format!("unsupported ZIM version {version}")));
    }
    let header = RemoteHeader {
        article_count: get32(24),
        cluster_count: get32(28),
        url_ptr_pos: get64(32),
        title_ptr_pos: get64(40),
        cluster_ptr_pos: get64(48),
        mime_list_pos: get64(56),
        checksum_pos: get64(72),
    };
    if header.mime_list_pos != 80 || header.checksum_pos + 16 != file_size {
        return Err(RemoteKiwixError::Parse("inconsistent ZIM file bounds".into()));
    }
    Ok(header)
}

fn parse_mimes(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

struct DirectoryEntry {
    namespace: u8,
    mime: String,
    cluster: u32,
    blob: u32,
    url: String,
}

fn scan_image_entries(
    client: &Client,
    url: &str,
    pointers: &[u64],
    directory_end: u64,
    directory_start: u64,
    mimes: &[String],
    file_size: u64,
) -> Result<(Vec<RemoteImageEntry>, HttpStats), RemoteKiwixError> {
    let mut entries = Vec::new();
    let mut http_stats = HttpStats::default();
    let mut window_start = 0;
    let mut window = Vec::new();
    for (index, &start) in pointers.iter().enumerate() {
        let end = pointers.get(index + 1).copied().unwrap_or(directory_end);
        if end <= start || start < directory_start || end > directory_end {
            return Err(RemoteKiwixError::Parse("invalid ZIM directory pointer".into()));
        }
        if start < window_start || end > window_start.saturating_add(window.len() as u64) {
            let fetch_end = start.saturating_add(RANGE_WINDOW).max(end).min(directory_end);
            window_start = start;
            let response = range(client, url, start, fetch_end, Some(file_size))?;
            http_stats.add_response(&response);
            window = response.bytes;
        }
        let local_start = usize::try_from(start - window_start)
            .map_err(|_| RemoteKiwixError::Parse("directory offset overflows".into()))?;
        let local_end = usize::try_from(end - window_start)
            .map_err(|_| RemoteKiwixError::Parse("directory end overflows".into()))?;
        let bytes = window
            .get(local_start..local_end)
            .ok_or_else(|| RemoteKiwixError::Parse("directory entry is outside range".into()))?;
        if (index + 1) % 100_000 == 0 {
            eprintln!(
                "kiwix: scanned {} of {} directory entries ({} images)",
                index + 1,
                pointers.len(),
                entries.len()
            );
        }
        let Some(entry) = parse_directory_entry(bytes, mimes)? else { continue };
        let mime = entry.mime.split(';').next().unwrap_or("");
        let is_image_mime = mime.starts_with("image/");
        let is_supported_media = mime_is_supported_media(&entry.mime);
        if !matches!(entry.namespace, b'C' | b'I')
            || (!is_supported_media && entry.namespace != b'I')
        {
            continue;
        }
        let (key, preference) = image_identity(&entry.url, is_supported_media);
        let legacy_key = legacy_image_key(&entry.url);
        if key.is_empty() {
            continue;
        }
        let legacy_hash = (legacy_key != key).then(|| media_title_hash(&legacy_key));
        entries.push(RemoteImageEntry {
            hash: media_title_hash(&key),
            key,
            legacy_hash,
            file_type: if is_image_mime {
                entry.mime[6..].split(';').next().unwrap_or("unknown").to_string()
            } else if is_supported_media {
                entry.mime
                    .split(';')
                    .next()
                    .and_then(|mime| mime.strip_prefix("audio/"))
                    .unwrap_or("unknown")
                    .to_string()
            } else {
                "unknown".to_string()
            },
            preference,
            cluster: entry.cluster,
            blob: entry.blob,
        });
    }
    entries.sort_by(|left, right| {
        left.hash
            .cmp(&right.hash)
            .then(left.key.cmp(&right.key))
            .then(right.preference.cmp(&left.preference))
            .then(left.file_type.cmp(&right.file_type))
    });
    eprintln!(
        "kiwix: indexed {} image entries from {} directory records",
        entries.len(),
        pointers.len()
    );
    Ok((entries, http_stats))
}

fn parse_directory_entry(bytes: &[u8], mimes: &[String]) -> Result<Option<DirectoryEntry>, RemoteKiwixError> {
    if bytes.len() < 8 {
        return Err(RemoteKiwixError::Parse("short ZIM directory entry".into()));
    }
    let mime_id = u16::from_le_bytes(bytes[0..2].try_into().unwrap());
    if mime_id >= 0xfffd {
        return Ok(None);
    }
    if bytes.len() < 16 {
        return Err(RemoteKiwixError::Parse("short ZIM directory entry".into()));
    }
    let namespace = bytes[3];
    let cluster = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let blob = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let mime = mimes
        .get(mime_id as usize)
        .cloned()
        .ok_or_else(|| RemoteKiwixError::Parse("ZIM MIME id is outside MIME table".into()))?;
    let url_start = 16;
    let url_len = bytes[url_start..]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| RemoteKiwixError::Parse("unterminated ZIM URL".into()))?;
    let title_start = url_start + url_len + 1;
    if bytes[title_start..].iter().position(|byte| *byte == 0).is_none() {
        return Err(RemoteKiwixError::Parse("unterminated ZIM title".into()));
    }
    Ok(Some(DirectoryEntry {
        namespace,
        mime,
        cluster,
        blob,
        url: String::from_utf8_lossy(&bytes[url_start..url_start + url_len]).into_owned(),
    }))
}

fn mime_is_supported_media(mime: &str) -> bool {
    mime.trim_start()
        .split(';')
        .next()
        .is_some_and(|value| {
            value.starts_with("image/") || matches!(value, "audio/ogg" | "audio/oga")
        })
}

fn read_u64_table(
    client: &Client,
    url: &str,
    start: u64,
    count: usize,
    file_size: u64,
) -> Result<(Vec<u64>, HttpStats), RemoteKiwixError> {
    let byte_count = (count as u64)
        .checked_mul(8)
        .ok_or_else(|| RemoteKiwixError::Parse("pointer table overflows".into()))?;
    let mut bytes = Vec::with_capacity(count.saturating_mul(8));
    let mut http_stats = HttpStats::default();
    let mut position = start;
    let end = start
        .checked_add(byte_count)
        .ok_or_else(|| RemoteKiwixError::Parse("pointer table overflows".into()))?;
    while position < end {
        let remaining = end - position;
        let mut request_bytes = remaining.min(RANGE_WINDOW);
        request_bytes -= request_bytes % 8;
        if request_bytes == 0 {
            request_bytes = remaining;
        }
        let response = range(
            client,
            url,
            position,
            position + request_bytes,
            Some(file_size),
        )?;
        http_stats.add_response(&response);
        bytes.extend_from_slice(&response.bytes);
        position += request_bytes;
    }
    if bytes.len() != count.saturating_mul(8) {
        return Err(RemoteKiwixError::Parse("short ZIM pointer table".into()));
    }
    Ok((
        bytes
            .chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
            .collect(),
        http_stats,
    ))
}

#[derive(Debug)]
struct RangeResponse {
    bytes: Vec<u8>,
    total: u64,
    final_url: String,
    requests: u64,
    retries: u64,
}

fn range(
    client: &Client,
    url: &str,
    start: u64,
    end: u64,
    expected_total: Option<u64>,
) -> Result<RangeResponse, RemoteKiwixError> {
    if end <= start {
        return Err(RemoteKiwixError::Parse("empty HTTP range".into()));
    }
    let mut requests = 0;
    let mut retries = 0;
    for attempt in 0..MAX_RETRIES {
        requests += 1;
        match send_range(client, url, start, end) {
            Ok(response) if response.status().as_u16() == 206 => {
                return validate_range_response(
                    response,
                    start,
                    end,
                    expected_total,
                    requests,
                    retries,
                );
            }
            Ok(response)
                if attempt + 1 < MAX_RETRIES
                    && (response.status().as_u16() == 429 || response.status().is_server_error()) =>
            {
                retries += 1;
                let delay = retry_delay(&response, attempt);
                drop(response);
                thread::sleep(delay);
            }
            Ok(response) => {
                return Err(RemoteKiwixError::Parse(format!(
                    "Kiwix range requires HTTP 206, got {} for bytes={start}-{end}",
                    response.status()
                )));
            }
            Err(error) if attempt + 1 < MAX_RETRIES => {
                retries += 1;
                thread::sleep(fallback_retry_delay(attempt));
                if attempt + 1 == MAX_RETRIES {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(RemoteKiwixError::Parse(format!(
        "Kiwix range retries exhausted for bytes={start}-{end}"
    )))
}

#[cfg(test)]
fn direct_client() -> Result<Client, RemoteKiwixError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .build()
        .map_err(RemoteKiwixError::Http)
}

fn resolve_initial_range(
    client: &Client,
    requested_url: &str,
    start: u64,
    end: u64,
) -> Result<RangeResponse, RemoteKiwixError> {
    let _ = Url::parse(requested_url)
        .map_err(|error| RemoteKiwixError::Parse(format!("invalid Kiwix URL: {error}")))?;
    range(client, requested_url, start, end, None)
}

fn send_range(
    client: &Client,
    url: &str,
    start: u64,
    end: u64,
) -> Result<reqwest::blocking::Response, reqwest::Error> {
    client
        .get(url)
        .header(RANGE, format!("bytes={start}-{}", end - 1))
        .send()
}

fn validate_range_response(
    response: reqwest::blocking::Response,
    start: u64,
    end: u64,
    expected_total: Option<u64>,
    requests: u64,
    retries: u64,
) -> Result<RangeResponse, RemoteKiwixError> {
    let (actual_start, actual_end, total) = response
        .headers()
        .get(CONTENT_RANGE)
        .ok_or_else(|| RemoteKiwixError::Parse("Kiwix 206 response has no Content-Range".into()))
        .and_then(|value| {
            let value = value
                .to_str()
                .map_err(|_| RemoteKiwixError::Parse("Kiwix Content-Range is not ASCII".into()))?;
            parse_content_range(value)
        })?;
    if actual_start != start || actual_end != end - 1 {
        return Err(RemoteKiwixError::Parse(format!(
            "Kiwix Content-Range mismatch: requested bytes={start}-{end}, got bytes {actual_start}-{actual_end}"
        )));
    }
    if expected_total.is_some_and(|expected| expected != total) {
        return Err(RemoteKiwixError::Parse(format!(
            "Kiwix Content-Range total changed: expected {}, got {total}",
            expected_total.unwrap_or_default()
        )));
    }
    if end > total {
        return Err(RemoteKiwixError::Parse(format!(
            "Kiwix Content-Range ends at {actual_end}, outside total {total}"
        )));
    }
    let expected = usize::try_from(end - start)
        .map_err(|_| RemoteKiwixError::Parse("HTTP range is too large".into()))?;
    let body_limit = u64::try_from(expected)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| RemoteKiwixError::Parse("HTTP range body limit overflows".into()))?;
    let final_url = response.url().to_string();
    let mut bytes = Vec::with_capacity(expected.min(1024 * 1024));
    response.take(body_limit).read_to_end(&mut bytes)?;
    if bytes.len() != expected {
        return Err(RemoteKiwixError::Parse(format!(
            "Kiwix range body length mismatch: expected {expected}, got {}",
            bytes.len()
        )));
    }
    Ok(RangeResponse {
        bytes,
        total,
        final_url,
        requests,
        retries,
    })
}

fn parse_content_range(value: &str) -> Result<(u64, u64, u64), RemoteKiwixError> {
    let value = value.trim();
    let rest = value
        .strip_prefix("bytes ")
        .ok_or_else(|| RemoteKiwixError::Parse(format!("invalid Kiwix Content-Range: {value}")))?;
    let (range, total) = rest
        .split_once('/')
        .ok_or_else(|| RemoteKiwixError::Parse(format!("invalid Kiwix Content-Range: {value}")))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| RemoteKiwixError::Parse(format!("invalid Kiwix Content-Range: {value}")))?;
    let start = start
        .parse::<u64>()
        .map_err(|_| RemoteKiwixError::Parse(format!("invalid Kiwix Content-Range: {value}")))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| RemoteKiwixError::Parse(format!("invalid Kiwix Content-Range: {value}")))?;
    let total = total
        .parse::<u64>()
        .map_err(|_| RemoteKiwixError::Parse(format!("invalid Kiwix Content-Range: {value}")))?;
    if end < start {
        return Err(RemoteKiwixError::Parse(format!("invalid Kiwix Content-Range: {value}")));
    }
    Ok((start, end, total))
}

fn retry_delay(response: &reqwest::blocking::Response, attempt: usize) -> Duration {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| fallback_retry_delay(attempt))
}

fn fallback_retry_delay(attempt: usize) -> Duration {
    let seconds = 2_u64.saturating_pow((attempt as u32).saturating_add(1));
    Duration::from_secs(seconds).min(MAX_FALLBACK_RETRY)
}

fn get_bytes(client: &Client, url: &str, limit: Option<u64>) -> Result<Vec<u8>, RemoteKiwixError> {
    let mut response = client
        .get(url)
        .send()?;
    if !response.status().is_success() {
        return Err(RemoteKiwixError::Parse(format!(
            "Kiwix HTTP status {} for {url}",
            response.status()
        )));
    }
    let mut bytes = Vec::new();
    if let Some(limit) = limit {
        response.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(RemoteKiwixError::Parse("Kiwix catalogue is unexpectedly large".into()));
        }
    } else {
        response.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

fn html_hrefs(body: &[u8]) -> impl Iterator<Item = &str> {
    let text = std::str::from_utf8(body).unwrap_or_default();
    text.split("href=\"").skip(1).filter_map(|tail| tail.split('"').next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiwix::image_key;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    struct MockResponse {
        status: &'static str,
        headers: Vec<String>,
        body: Vec<u8>,
    }

    fn mock_response(status: &'static str, body: &[u8], headers: &[String]) -> MockResponse {
        MockResponse {
            status,
            headers: headers.to_vec(),
            body: body.to_vec(),
        }
    }

    fn start_mock(responses: Vec<MockResponse>) -> (String, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_thread = requests.clone();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let count = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                requests_thread
                    .lock()
                    .unwrap()
                    .push(request.to_string());
                let mut wire = format!("HTTP/1.1 {}\r\n", response.status).into_bytes();
                for header in response.headers {
                    wire.extend_from_slice(header.as_bytes());
                    wire.extend_from_slice(b"\r\n");
                }
                wire.extend_from_slice(b"Connection: close\r\n\r\n");
                wire.extend_from_slice(&response.body);
                stream.write_all(&wire).unwrap();
            }
        });
        (format!("http://{address}/zim"), requests, handle)
    }

    fn start_range_mock(
        data: Vec<u8>,
        request_count: usize,
    ) -> (String, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_thread = requests.clone();
        let handle = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let count = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..count]).to_string();
                let range = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("range") {
                            value.trim().strip_prefix("bytes=")
                        } else {
                            None
                        }
                    })
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start = start.parse::<usize>().unwrap();
                let end = end.parse::<usize>().unwrap();
                let body = &data[start..=end];
                requests_thread.lock().unwrap().push(request);
                let mut wire = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len(),
                    body.len()
                )
                .into_bytes();
                wire.extend_from_slice(body);
                stream.write_all(&wire).unwrap();
            }
        });
        (format!("http://{address}/zim"), requests, handle)
    }

    fn uncompressed_cluster(blobs: &[&[u8]]) -> Vec<u8> {
        let table_bytes = (blobs.len() + 1) * 4;
        let mut cluster = vec![0_u8];
        cluster.extend_from_slice(&(table_bytes as u32).to_le_bytes());
        let mut offset = table_bytes as u32;
        for blob in blobs {
            offset += blob.len() as u32;
            cluster.extend_from_slice(&offset.to_le_bytes());
        }
        for blob in blobs {
            cluster.extend_from_slice(blob);
        }
        cluster
    }

    #[test]
    fn special_directory_entries_are_skipped_without_a_false_short_record_error() {
        assert!(parse_directory_entry(&[0xff, 0xff, 0, b'C', 0, 0, 0, 0], &[])
            .unwrap()
            .is_none());
    }

    #[test]
    fn scanner_accepts_ogg_and_oga_but_not_unrelated_audio() {
        assert!(mime_is_supported_media("audio/ogg"));
        assert!(mime_is_supported_media("audio/oga; codecs=vorbis"));
        assert!(!mime_is_supported_media("audio/mpeg"));
    }

    #[test]
    fn directory_entry_extracts_image_target_and_url() {
        let mut bytes = vec![0, 0, 0, b'C', 0, 0, 0, 0, 7, 0, 0, 0, 3, 0, 0, 0];
        bytes.extend_from_slice(b"File:foo.jpg\0Foo.jpg\0");
        let entry = parse_directory_entry(&bytes, &["image/jpeg".into()])
            .unwrap()
            .unwrap();
        assert_eq!(entry.namespace, b'C');
        assert_eq!(entry.cluster, 7);
        assert_eq!(entry.blob, 3);
        assert_eq!(entry.url, "File:foo.jpg");
        assert_eq!(entry.mime, "image/jpeg");
    }

    #[test]
    fn range_rejects_non_206_and_requires_exact_content_range() {
        let (url, _, handle) = start_mock(vec![mock_response("200 OK", b"abcd", &[])]);
        let error = range(&direct_client().unwrap(), &url, 0, 4, Some(4)).unwrap_err();
        assert!(error.to_string().contains("requires HTTP 206"));
        handle.join().unwrap();

        let (url, _, handle) = start_mock(vec![mock_response(
            "206 Partial Content",
            b"abcd",
            &["Content-Range: bytes 1-4/4".to_string()],
        )]);
        let error = range(&direct_client().unwrap(), &url, 0, 4, Some(4)).unwrap_err();
        assert!(error.to_string().contains("Content-Range mismatch"));
        handle.join().unwrap();
    }

    #[test]
    fn range_rejects_an_oversized_body() {
        let (url, _, handle) = start_mock(vec![mock_response(
            "206 Partial Content",
            b"abcde",
            &["Content-Range: bytes 0-3/5".to_string()],
        )]);
        let error = range(&direct_client().unwrap(), &url, 0, 4, Some(5)).unwrap_err();
        assert!(error.to_string().contains("body length mismatch"));
        handle.join().unwrap();
    }

    #[test]
    fn range_honors_zero_retry_after_and_reports_retry_counters() {
        let (url, requests, handle) = start_mock(vec![
            mock_response("503 Service Unavailable", b"", &["Retry-After: 0".to_string()]),
            mock_response(
                "206 Partial Content",
                b"abcd",
                &["Content-Range: bytes 0-3/4".to_string()],
            ),
        ]);
        let response = range(&direct_client().unwrap(), &url, 0, 4, Some(4)).unwrap();
        assert_eq!(response.bytes, b"abcd");
        assert_eq!(response.requests, 2);
        assert_eq!(response.retries, 1);
        assert_eq!(requests.lock().unwrap().len(), 2);
        handle.join().unwrap();
    }

    #[test]
    fn pointer_tables_are_fetched_in_bounded_windows() {
        let count = RANGE_WINDOW as usize / 8 + 2;
        let mut data = Vec::with_capacity(count * 8);
        for value in 0..count as u64 {
            data.extend_from_slice(&value.to_le_bytes());
        }
        let (url, requests, handle) = start_range_mock(data, 2);
        let (table, stats) = read_u64_table(
            &direct_client().unwrap(),
            &url,
            0,
            count,
            (count * 8) as u64,
        )
        .unwrap();
        assert_eq!(table.len(), count);
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.retries, 0);
        assert_eq!(requests.lock().unwrap().len(), 2);
        handle.join().unwrap();
    }

    #[test]
    fn redirect_chain_is_resolved_once_before_later_ranges() {
        let (final_url, final_requests, final_handle) = start_mock(vec![mock_response(
            "206 Partial Content",
            b"abcd",
            &["Content-Range: bytes 0-3/4".to_string()],
        )]);
        let (redirect_url, redirect_requests, redirect_handle) = start_mock(vec![mock_response(
            "302 Found",
            b"",
            &[format!("Location: {final_url}")],
        )]);
        let response = resolve_initial_range(&direct_client().unwrap(), &redirect_url, 0, 4)
            .unwrap();
        assert_eq!(response.final_url, final_url);
        assert_eq!(redirect_requests.lock().unwrap().len(), 1);
        assert_eq!(final_requests.lock().unwrap().len(), 1);
        redirect_handle.join().unwrap();
        final_handle.join().unwrap();
    }

    #[test]
    fn remote_import_skips_existing_clusters_and_duplicate_titles() {
        let cluster_existing = uncompressed_cluster(&[b"already-present"]);
        let cluster_missing = uncompressed_cluster(&[b"new-payload"]);
        let mut data = cluster_existing.clone();
        data.extend_from_slice(&cluster_missing);
        let (url, requests, handle) = start_range_mock(data.clone(), 1);

        let root = std::env::temp_dir().join(format!(
            "sarun-remote-import-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut existing = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(existing.reserve("Existing.jpg"), Reservation::Reserved);
        existing
            .append_reserved("jpg", "Existing.jpg", b"already-present")
            .unwrap();
        existing.finish().unwrap();

        let source = RemoteKiwixImageSource {
            client: direct_client().unwrap(),
            url,
            file_size: data.len() as u64,
            checksum_pos: data.len() as u64,
            cluster_offsets: vec![0, cluster_existing.len() as u64],
            open_stats: HttpStats::default(),
            entries: vec![
                RemoteImageEntry {
                    key: image_key("Existing.jpg"),
                    hash: media_title_hash(&image_key("Existing.jpg")),
                    legacy_hash: None,
                    file_type: "jpg".into(),
                    preference: image_identity("Existing.jpg", true).1,
                    cluster: 0,
                    blob: 0,
                },
                RemoteImageEntry {
                    key: image_key("Missing.jpg"),
                    hash: media_title_hash(&image_key("Missing.jpg")),
                    legacy_hash: None,
                    file_type: "jpg".into(),
                    preference: image_identity("Missing.jpg", true).1,
                    cluster: 1,
                    blob: 0,
                },
                RemoteImageEntry {
                    key: image_key("Missing.jpg"),
                    hash: media_title_hash(&image_key("Missing.jpg")),
                    legacy_hash: None,
                    file_type: "png".into(),
                    preference: image_identity("langru-500px-Missing.jpg", true).1,
                    cluster: 1,
                    blob: 0,
                },
            ],
        };
        let stats = source.import_missing(&root).unwrap();
        assert_eq!(stats.entries_seen, 3);
        assert_eq!(stats.entries_skipped_existing, 1);
        assert_eq!(stats.entries_skipped_duplicate, 1);
        assert_eq!(stats.entries_written, 1);
        assert_eq!(stats.http_requests, 1);
        assert_eq!(stats.http_retries, 0);
        assert_eq!(stats.http_bytes, cluster_missing.len() as u64);
        let request_log = requests.lock().unwrap().clone();
        assert_eq!(request_log.len(), 1);
        let request = request_log[0].to_ascii_lowercase();
        assert!(request.contains(&format!(
            "Range: bytes={}-{}",
            cluster_existing.len(),
            data.len() - 1
        ).to_ascii_lowercase()));
        assert!(!request.contains("range: bytes=0-"));
        let catalog = crate::PackedMediaCatalog::open_directory(&root).unwrap();
        assert_eq!(catalog.lookup("Existing.jpg", None).unwrap().unwrap(), b"already-present");
        assert_eq!(catalog.lookup("Missing.jpg", None).unwrap().unwrap(), b"new-payload");
        handle.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_import_publishes_alias_without_fetching_legacy_payload_again() {
        let root = std::env::temp_dir().join(format!(
            "sarun-remote-alias-import-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy = "langru-500px-Remote.jpg";
        let normalized = image_key("Remote.jpg");
        let mut existing = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(existing.reserve(legacy), Reservation::Reserved);
        existing
            .append_reserved("jpg", legacy, b"already-present")
            .unwrap();
        existing.finish().unwrap();

        let source = RemoteKiwixImageSource {
            client: direct_client().unwrap(),
            url: "http://127.0.0.1:1/never-requested".into(),
            file_size: 0,
            checksum_pos: 0,
            cluster_offsets: Vec::new(),
            open_stats: HttpStats::default(),
            entries: vec![RemoteImageEntry {
                key: normalized.clone(),
                hash: media_title_hash(&normalized),
                legacy_hash: Some(media_title_hash(legacy)),
                file_type: "jpg".into(),
                preference: image_identity("Remote.jpg", true).1,
                cluster: 99,
                blob: 0,
            }],
        };
        let stats = source.import_missing(&root).unwrap();
        assert_eq!(stats.aliases_added, 1);
        assert_eq!(stats.entries_written, 0);
        assert_eq!(stats.http_requests, 0);
        assert_eq!(
            crate::PackedMediaCatalog::open_directory(&root)
                .unwrap()
                .lookup_with_type(&normalized, None)
                .unwrap(),
            Some(("jpg".into(), b"already-present".to_vec()))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
