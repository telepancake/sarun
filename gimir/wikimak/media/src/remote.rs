//! Ranged Kiwix importer.
//!
//! ZIM archives are indexed containers rather than sequential streams. This
//! importer reads bounded HTTP ranges, keeps directory metadata in memory,
//! and writes the final hash/offset/data files directly. It never creates a
//! local ZIM copy.

use std::collections::{BTreeMap, HashMap};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{HeaderValue, RANGE, RETRY_AFTER, USER_AGENT};

use crate::kiwix::image_key;
use crate::packed::{media_title_hash, MAX_DATA_BYTES};
use crate::kiwix::KiwixPackStats;

const CATALOG_URL: &str = "https://download.kiwix.org/zim/wikipedia/";
const RANGE_WINDOW: u64 = 16 * 1024 * 1024;
const CLUSTER_BATCH: u64 = 32 * 1024 * 1024;
const MAX_RETRIES: usize = 5;
const USER_AGENT_VALUE: &str = "sarun-wikimak/0.1 (+https://github.com/telepancake/sarun)";
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
}

#[derive(Debug, Clone)]
struct RemoteImageEntry {
    key: String,
    file_type: String,
    cluster: u32,
    blob: u32,
    length: usize,
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
        let url = url.into();
        let first = range(&client, &url, 0, 80)?;
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
        let mimes = parse_mimes(&range(&client, &url, header.mime_list_pos, mime_end)?.bytes);
        let url_pointers = read_u64_table(&client, &url, header.url_ptr_pos, header.article_count as usize)?;
        let cluster_offsets = read_u64_table(&client, &url, header.cluster_ptr_pos, header.cluster_count as usize)?;
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
        let entries = scan_image_entries(
            &client,
            &url,
            &url_pointers,
            header.url_ptr_pos,
            directory_start,
            &mimes,
        )?;
        Ok(Self {
            client,
            url,
            file_size,
            checksum_pos: header.checksum_pos,
            cluster_offsets,
            entries,
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
        let parent = output_dir.parent().unwrap_or_else(|| Path::new("."));
        let name = output_dir
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output has no name"))?
            .to_string_lossy();
        let staging = parent.join(format!(".{name}.packing-{}", std::process::id()));
        create_dir_all(&staging)?;
        match self.pack_to(&staging) {
            Ok(stats) => {
                std::fs::rename(&staging, output_dir)?;
                Ok(stats)
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(error)
            }
        }
    }

    fn pack_to(&self, output_dir: &Path) -> Result<KiwixPackStats, RemoteKiwixError> {
        let mut entries = self.entries.clone();
        entries.sort_by(|left, right| {
            media_title_hash(&left.key)
                .cmp(&media_title_hash(&right.key))
                .then(left.key.cmp(&right.key))
                .then(left.file_type.cmp(&right.file_type))
        });
        entries.dedup_by(|left, right| {
            media_title_hash(&left.key) == media_title_hash(&right.key)
                && left.file_type == right.file_type
        });
        eprintln!("kiwix: reading image lengths from ranged clusters");
        let mut by_cluster: HashMap<u32, Vec<usize>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if entry.cluster as usize >= self.cluster_offsets.len() {
                return Err(RemoteKiwixError::Parse(
                    "ZIM image points outside the cluster table".into(),
                ));
            }
            by_cluster.entry(entry.cluster).or_default().push(index);
        }
        self.visit_clusters(&by_cluster, |cluster, bytes| {
            let decoded = decode_cluster(bytes)?;
            for &index in by_cluster.get(&cluster).into_iter().flatten() {
                entries[index].length = blob(&decoded.bytes, &decoded.offsets, entries[index].blob)?.len();
            }
            Ok(())
        })?;

        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            groups.entry(entry.file_type.clone()).or_default().push(index);
        }
        let mut outputs = Vec::new();
        let mut slots = vec![None; entries.len()];
        let mut stats = KiwixPackStats {
            entries_seen: self.entries.len() as u64,
            ..KiwixPackStats::default()
        };
        for (file_type, mut indices) in groups {
            indices.sort_by_key(|index| media_title_hash(&entries[*index].key));
            let mut part = 0_u32;
            let mut part_entries = Vec::new();
            let mut part_bytes = 0_u64;
            for index in indices {
                let length = entries[index].length as u64;
                if length == 0 {
                    continue;
                }
                if length >= MAX_DATA_BYTES {
                    return Err(RemoteKiwixError::Parse("one image exceeds 4 GiB".into()));
                }
                if !part_entries.is_empty() && part_bytes + length >= MAX_DATA_BYTES {
                    outputs.push(make_output(output_dir, &file_type, part, &part_entries, &entries, part_bytes)?);
                    stats.storages += 1;
                    part += 1;
                    part_entries.clear();
                    part_bytes = 0;
                }
                slots[index] = Some((outputs.len(), part_bytes));
                part_entries.push(index);
                part_bytes += length;
                stats.bytes_written += length;
                stats.entries_written += 1;
            }
            if !part_entries.is_empty() {
                outputs.push(make_output(output_dir, &file_type, part, &part_entries, &entries, part_bytes)?);
                stats.storages += 1;
            }
        }
        eprintln!("kiwix: writing image payloads to packed storage");
        self.visit_clusters(&by_cluster, |cluster, bytes| {
            let decoded = decode_cluster(bytes)?;
            for &index in by_cluster.get(&cluster).into_iter().flatten() {
                let Some((output, offset)) = slots[index] else { continue };
                let payload = blob(&decoded.bytes, &decoded.offsets, entries[index].blob)?;
                write_at(&outputs[output].data, payload, offset)?;
            }
            Ok(())
        })?;
        for output in outputs {
            output.finish()?;
        }
        Ok(stats)
    }

    fn visit_clusters<F>(
        &self,
        by_cluster: &HashMap<u32, Vec<usize>>,
        mut visit: F,
    ) -> Result<(), RemoteKiwixError>
    where
        F: FnMut(u32, &[u8]) -> Result<(), RemoteKiwixError>,
    {
        let mut ids: Vec<u32> = by_cluster.keys().copied().collect();
        ids.sort_unstable();
        let mut batch = Vec::new();
        let mut start = 0;
        let mut end = 0;
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
                self.visit_cluster_batch(start, end, &batch, &mut visit)?;
                batch.clear();
            }
            if batch.is_empty() {
                start = cluster_start;
            }
            end = cluster_end;
            batch.push(id);
        }
        if !batch.is_empty() {
            self.visit_cluster_batch(start, end, &batch, &mut visit)?;
        }
        Ok(())
    }

    fn visit_cluster_batch<F>(
        &self,
        start: u64,
        end: u64,
        ids: &[u32],
        visit: &mut F,
    ) -> Result<(), RemoteKiwixError>
    where
        F: FnMut(u32, &[u8]) -> Result<(), RemoteKiwixError>,
    {
        eprintln!(
            "kiwix: ranged cluster batch {}..{} ({} bytes)",
            ids.first().copied().unwrap_or_default(),
            ids.last().copied().unwrap_or_default(),
            end.saturating_sub(start)
        );
        let bytes = range(&self.client, &self.url, start, end)?.bytes;
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
        Ok(())
    }
}

struct OutputPart {
    data: File,
}

impl OutputPart {
    fn finish(self) -> io::Result<()> {
        self.data.sync_all()
    }
}

fn make_output(
    root: &Path,
    file_type: &str,
    part: u32,
    indices: &[usize],
    entries: &[RemoteImageEntry],
    bytes: u64,
) -> Result<OutputPart, RemoteKiwixError> {
    let stem = format!("media-{file_type}-{part:04}");
    let data = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(root.join(format!("{stem}.data")))?;
    data.set_len(bytes)?;
    let mut hashes = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(root.join(format!("{stem}.hashes")))?;
    let mut offsets = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(root.join(format!("{stem}.offsets")))?;
    let mut offset = 0_u64;
    for &index in indices {
        hashes.write_all(&media_title_hash(&entries[index].key).to_le_bytes())?;
        offsets.write_all(&(offset as u32).to_le_bytes())?;
        offset += entries[index].length as u64;
    }
    hashes.sync_all()?;
    offsets.sync_all()?;
    Ok(OutputPart { data })
}

fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        let mut written = 0;
        while written < bytes.len() {
            let count = file.write_at(&bytes[written..], offset + written as u64)?;
            if count == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short media write"));
            }
            written += count;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        use std::io::{Seek, SeekFrom};
        let mut file = file.try_clone()?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)
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
) -> Result<Vec<RemoteImageEntry>, RemoteKiwixError> {
    let mut entries = Vec::new();
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
            window = range(client, url, start, fetch_end)?.bytes;
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
        let is_image_mime = entry.mime.starts_with("image/");
        if !matches!(entry.namespace, b'C' | b'I') || (!is_image_mime && entry.namespace != b'I') {
            continue;
        }
        let key = image_key(&entry.url);
        if key.is_empty() {
            continue;
        }
        entries.push(RemoteImageEntry {
            key,
            file_type: if is_image_mime {
                entry.mime[6..].split(';').next().unwrap_or("unknown").to_string()
            } else {
                "unknown".to_string()
            },
            cluster: entry.cluster,
            blob: entry.blob,
            length: 0,
        });
    }
    entries.sort_by(|left, right| {
        media_title_hash(&left.key)
            .cmp(&media_title_hash(&right.key))
            .then(left.key.cmp(&right.key))
            .then(left.file_type.cmp(&right.file_type))
    });
    entries.dedup_by(|left, right| {
        media_title_hash(&left.key) == media_title_hash(&right.key)
            && left.file_type == right.file_type
    });
    eprintln!(
        "kiwix: indexed {} image entries from {} directory records",
        entries.len(),
        pointers.len()
    );
    Ok(entries)
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

fn read_u64_table(
    client: &Client,
    url: &str,
    start: u64,
    count: usize,
) -> Result<Vec<u64>, RemoteKiwixError> {
    let bytes = range(
        client,
        url,
        start,
        start
            .checked_add(count as u64 * 8)
            .ok_or_else(|| RemoteKiwixError::Parse("pointer table overflows".into()))?,
    )?
    .bytes;
    if bytes.len() != count * 8 {
        return Err(RemoteKiwixError::Parse("short ZIM pointer table".into()));
    }
    Ok(bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect())
}

struct RangeResponse {
    bytes: Vec<u8>,
    total: u64,
}

fn range(
    client: &Client,
    url: &str,
    start: u64,
    end: u64,
) -> Result<RangeResponse, RemoteKiwixError> {
    if end <= start {
        return Err(RemoteKiwixError::Parse("empty HTTP range".into()));
    }
    let expected = usize::try_from(end - start)
        .map_err(|_| RemoteKiwixError::Parse("HTTP range is too large".into()))?;
    for attempt in 0..MAX_RETRIES {
        let response = client
            .get(url)
            .header(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE))
            .header(RANGE, format!("bytes={}-{}", start, end - 1))
            .send();
        match response {
            Ok(response) if response.status().is_success() => {
                if start != 0 && response.status().as_u16() != 206 {
                    return Err(RemoteKiwixError::Parse(
                        "Kiwix server ignored a nonzero range request".into(),
                    ));
                }
                let total = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.rsplit('/').next())
                    .and_then(|value| value.parse().ok())
                    .or_else(|| response.content_length().map(|length| start + length))
                    .unwrap_or(end);
                let mut bytes = Vec::with_capacity(expected.min(1024 * 1024));
                response.take((expected + 1) as u64).read_to_end(&mut bytes)?;
                if bytes.len() != expected {
                    return Err(RemoteKiwixError::Parse(format!(
                        "short Kiwix range: expected {expected}, got {}",
                        bytes.len()
                    )));
                }
                return Ok(RangeResponse { bytes, total });
            }
            Ok(response)
                if attempt + 1 < MAX_RETRIES
                    && (response.status().as_u16() == 429 || response.status().is_server_error()) =>
            {
                let delay = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| Duration::from_secs(2_u64.saturating_pow(attempt as u32 + 1)));
                drop(response);
                thread::sleep(delay);
            }
            Ok(response) => {
                return Err(RemoteKiwixError::Parse(format!(
                    "Kiwix HTTP status {} for {url}",
                    response.status()
                )))
            }
            Err(_error) if attempt + 1 < MAX_RETRIES => {
                thread::sleep(Duration::from_secs(2_u64.saturating_pow(attempt as u32 + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(RemoteKiwixError::Parse("Kiwix range retries exhausted".into()))
}

fn get_bytes(client: &Client, url: &str, limit: Option<u64>) -> Result<Vec<u8>, RemoteKiwixError> {
    let mut response = client
        .get(url)
        .header(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE))
        .send()?;
    if !response.status().is_success() {
        return Err(RemoteKiwixError::Parse(format!(
            "Kiwix HTTP status {} for {url}",
            response.status()
        )));
    }
    let mut bytes = Vec::new();
    response.read_to_end(&mut bytes)?;
    if limit.is_some_and(|limit| bytes.len() as u64 > limit) {
        return Err(RemoteKiwixError::Parse("Kiwix catalogue is unexpectedly large".into()));
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

    #[test]
    fn special_directory_entries_are_skipped_without_a_false_short_record_error() {
        assert!(parse_directory_entry(&[0xff, 0xff, 0, b'C', 0, 0, 0, 0], &[])
            .unwrap()
            .is_none());
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
}
