//! Small, deliberately boring packed image storage.
//!
//! Each data file contains concatenated image payloads.  Its companion index
//! has two arrays: sorted `u64` title hashes and same-ordinal `u32` data
//! offsets.  Payload length is the next offset (or the data-file length for
//! the final entry), so length and MIME are not duplicated in the index.  A
//! data file is capped below 4 GiB, keeping offsets 32-bit as specified by
//! the bootstrap image format.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{create_dir_all, read_dir, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::url::normalize_filename;

pub const MAX_DATA_BYTES: u64 = u32::MAX as u64 + 1;
const PART_FORMAT: &[u8] = b"sarun-packed-media-v2 lengths=u32\n";
const STAGING_MANIFEST_NAME: &str = ".sarun-media-staging-v1";
const STAGING_MANIFEST: &[u8] = b"sarun-media-import-v1\n";
const ALIAS_PREFIX: &str = "media-alias-";
const ALIAS_SUFFIX: &str = ".aliases";
const ALIAS_FORMAT: &[u8] = b"sarun-packed-media-alias-part-v1\n";
const ALIAS_RECORD_BYTES: usize = 16;

/// Counts produced by a local or ranged Kiwix import.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct KiwixPackStats {
    pub entries_seen: u64,
    pub entries_skipped_existing: u64,
    pub entries_skipped_duplicate: u64,
    pub entries_written: u64,
    pub aliases_added: u64,
    pub bytes_written: u64,
    pub storages: u64,
    pub http_bytes: u64,
    pub http_requests: u64,
    pub http_retries: u64,
}

impl KiwixPackStats {
    pub fn entries_skipped(&self) -> u64 {
        self.entries_skipped_existing + self.entries_skipped_duplicate
    }
}
/// The same hash family as the Wikipedia title index's hashed-title branch,
/// with namespace 6 (File) as the seed.  The high bit marks the hash form.
pub fn media_title_hash(title: &str) -> u64 {
    let title = title.trim();
    let title = title
        .strip_prefix("File:")
        .or_else(|| title.strip_prefix("file:"))
        .unwrap_or(title);
    let title = normalize_filename(title);
    let hash = xxhash_rust::xxh3::xxh3_64_with_seed(title.as_bytes(), 6);
    (1_u64 << 63) | (hash & ((1_u64 << 63) - 1))
}

#[derive(Clone, Debug)]
pub struct MediaStorageSpec {
    pub data: PathBuf,
    pub hashes: PathBuf,
    pub offsets: PathBuf,
    /// New parts may carry lengths because payload order is not required to
    /// match hash order. `None` is the old format: length is the next offset.
    pub lengths: Option<PathBuf>,
    /// File type is a storage property, not an index field (`jpg`, `png`, …).
    pub file_type: String,
    /// A storage may contain originals (`None`) or one known rendition width.
    pub width: Option<u32>,
}

/// One packed data/index pair. The two index arrays are independently mmap'ed
/// and joined by ordinal, so neither array needs a packed struct or alignment
/// padding.
pub struct MediaStorage {
    spec: MediaStorageSpec,
    data: Arc<File>,
    hashes: memmap2::Mmap,
    offsets: memmap2::Mmap,
    lengths: Option<memmap2::Mmap>,
    data_len: u64,
    #[cfg(test)]
    binary_search_probes: AtomicU64,
}

impl MediaStorage {
    pub fn open(spec: MediaStorageSpec) -> io::Result<Self> {
        let hash_file = File::open(&spec.hashes)?;
        let offset_file = File::open(&spec.offsets)?;
        let hash_len = hash_file.metadata()?.len();
        let offset_len = offset_file.metadata()?.len();
        if hash_len % 8 != 0 || offset_len % 4 != 0 || hash_len / 8 != offset_len / 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media hash and offset arrays have incompatible lengths",
            ));
        }
        let count = usize::try_from(hash_len / 8)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "media index too large"))?;
        // Mmap the independent arrays.  Reads use byte slices and
        // from_le_bytes, so no host alignment or struct layout is assumed.
        let hashes = unsafe { memmap2::MmapOptions::new().map(&hash_file)? };
        let offsets = unsafe { memmap2::MmapOptions::new().map(&offset_file)? };
        let lengths = match &spec.lengths {
            Some(path) if path.exists() => {
                let file = File::open(path)?;
                if file.metadata()?.len() != hash_len / 2 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "media length array has incompatible length",
                    ));
                }
                Some(unsafe { memmap2::MmapOptions::new().map(&file)? })
            }
            Some(path) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("media length companion is missing: {}", path.display()),
                ));
            }
            None => None,
        };
        let mut previous = None;
        for position in 0..count {
            let hash = read_hash(&hashes, position);
            if previous.is_some_and(|value| value >= hash) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media index is not strictly sorted",
                ));
            }
            previous = Some(hash);
        }
        let data = Arc::new(OpenOptions::new().read(true).open(&spec.data)?);
        let data_len = data.metadata()?.len();
        if data_len >= MAX_DATA_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media storage exceeds 4 GiB",
            ));
        }
        for position in 0..count {
            let start = u64::from(read_offset(&offsets, position));
            let end = if let Some(lengths) = &lengths {
                start.checked_add(u64::from(read_length(lengths, position)))
            } else if position + 1 < count {
                Some(u64::from(read_offset(&offsets, position + 1)))
            } else {
                Some(data_len)
            };
            if end.is_none_or(|end| start > end || end > data_len) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media index points past data",
                ));
            }
        }
        Ok(Self {
            spec,
            data,
            hashes,
            offsets,
            lengths,
            data_len,
            #[cfg(test)]
            binary_search_probes: AtomicU64::new(0),
        })
    }

    pub fn spec(&self) -> &MediaStorageSpec {
        &self.spec
    }

    pub fn lookup(&self, title: &str) -> io::Result<Option<Vec<u8>>> {
        self.lookup_hash(media_title_hash(title))
    }

    fn lookup_hash(&self, hash: u64) -> io::Result<Option<Vec<u8>>> {
        let Ok(position) = self.binary_search_hash(hash) else {
            return Ok(None);
        };
        let start = u64::from(read_offset(&self.offsets, position));
        let end = if let Some(lengths) = &self.lengths {
            start + u64::from(read_length(lengths, position))
        } else {
            (position + 1 < self.hashes.len() / 8)
                .then(|| u64::from(read_offset(&self.offsets, position + 1)))
                .unwrap_or(self.data_len)
        };
        if end < start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media index offsets go backwards",
            ));
        }
        let mut file = self.data.try_clone()?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0_u8; usize::try_from(end - start).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "media payload too large")
        })?];
        file.read_exact(&mut bytes)?;
        Ok(Some(bytes))
    }

    fn binary_search_hash(&self, needle: u64) -> Result<usize, usize> {
        #[cfg(test)]
        self.binary_search_probes.fetch_add(1, Ordering::Relaxed);
        let mut low = 0;
        let mut high = self.hashes.len() / 8;
        while low < high {
            let middle = low + (high - low) / 2;
            match read_hash(&self.hashes, middle).cmp(&needle) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Equal => return Ok(middle),
                std::cmp::Ordering::Greater => high = middle,
            }
        }
        Err(low)
    }

    fn hash_values(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.hashes.len() / 8).map(|position| read_hash(&self.hashes, position))
    }
}

/// A sorted catalogue-level map from a title hash to every storage containing
/// it.  Storage indices are sorted as a secondary key, so a duplicate hash's
/// candidates retain the catalog's original order for stable rendition ties.
/// The flat representation avoids a per-key allocation or hash-table bucket.
struct PackedMediaLookupIndex {
    entries: Vec<(u64, u32)>,
}

impl PackedMediaLookupIndex {
    fn build(storages: &[MediaStorage]) -> io::Result<Self> {
        let entry_count = storages.iter().try_fold(0_usize, |total, storage| {
            total.checked_add(storage.hashes.len() / 8).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "media catalogue is too large")
            })
        })?;
        let mut entries = Vec::with_capacity(entry_count);
        for (storage_index, storage) in storages.iter().enumerate() {
            let storage_index = u32::try_from(storage_index).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "media catalogue has too many parts")
            })?;
            entries.extend(
                storage
                    .hash_values()
                    .map(|hash| (hash, storage_index)),
            );
        }
        entries.sort_unstable();
        Ok(Self { entries })
    }

    fn matching_range(&self, hash: u64) -> std::ops::Range<usize> {
        let first = self.entries.partition_point(|(entry_hash, _)| *entry_hash < hash);
        let end = self.entries[first..]
            .partition_point(|(entry_hash, _)| *entry_hash == hash)
            + first;
        first..end
    }

    fn contains_hash(&self, hash: u64) -> bool {
        let range = self.matching_range(hash);
        range.start != range.end
    }
}

/// Open several candidate storages and choose the rendition nearest to the
/// requested width.  Storage type and rendition width live in filenames/
/// descriptors, never in each 12-byte index entry. Opening also builds a
/// sorted in-memory hash-to-storage index, so part indexes are not touched
/// during candidate discovery.
pub struct PackedMediaCatalog {
    storages: Vec<MediaStorage>,
    lookup_index: PackedMediaLookupIndex,
    aliases: Vec<MediaAlias>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct MediaAlias {
    source: u64,
    target: u64,
}

impl PackedMediaCatalog {
    pub fn open(specs: impl IntoIterator<Item = MediaStorageSpec>) -> io::Result<Self> {
        let storages = specs
            .into_iter()
            .map(MediaStorage::open)
            .collect::<io::Result<Vec<_>>>()?;
        let lookup_index = PackedMediaLookupIndex::build(&storages)?;
        Ok(Self {
            storages,
            lookup_index,
            aliases: Vec::new(),
        })
    }

    pub fn storages(&self) -> &[MediaStorage] {
        &self.storages
    }

    /// Discover the `media-<type>-<part>.{data,hashes,offsets[,lengths]}` files emitted
    /// by the Kiwix packer.  The directory itself is the only catalogue; no
    /// per-image metadata sidecar is needed.
    pub fn open_directory(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let mut specs = Vec::new();
        for item in read_dir(root)? {
            let path = item?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("data") {
                continue;
            }
            let Some(stem) = path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(rest) = stem.strip_prefix("media-") else {
                continue;
            };
            let Some((file_type, part)) = rest.rsplit_once('-') else {
                continue;
            };
            if part.parse::<u32>().is_err() || file_type.is_empty() {
                continue;
            }
            let format = root.join(format!("{stem}.format"));
            let lengths = if format.exists() {
                if std::fs::read(&format)? != PART_FORMAT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown packed-media format marker: {}", format.display()),
                    ));
                }
                Some(root.join(format!("{stem}.lengths")))
            } else {
                None
            };
            specs.push(MediaStorageSpec {
                data: path,
                hashes: root.join(format!("{stem}.hashes")),
                offsets: root.join(format!("{stem}.offsets")),
                lengths,
                file_type: file_type.to_string(),
                width: None,
            });
        }
        specs.sort_by(|left, right| left.data.cmp(&right.data));
        let mut catalog = Self::open(specs)?;
        catalog.aliases = read_alias_parts(root, &catalog.lookup_index)?;
        Ok(catalog)
    }

    pub fn lookup(&self, title: &str, width: Option<u32>) -> io::Result<Option<Vec<u8>>> {
        self.lookup_with_type(title, width)
            .map(|value| value.map(|(_, bytes)| bytes))
    }

    pub fn lookup_with_type(
        &self,
        title: &str,
        width: Option<u32>,
    ) -> io::Result<Option<(String, Vec<u8>)>> {
        let requested_hash = media_title_hash(title);
        let hash = self
            .aliases
            .binary_search_by_key(&requested_hash, |alias| alias.source)
            .ok()
            .map(|position| self.aliases[position].target)
            .unwrap_or(requested_hash);
        let mut candidates = self
            .lookup_index
            .matching_range(hash)
            .map(|position| {
                &self.storages[self.lookup_index.entries[position].1 as usize]
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|storage| match (width, storage.spec.width) {
            (None, None) => 0,
            (None, Some(value)) => value,
            (Some(_requested), None) => u32::MAX / 2,
            (Some(requested), Some(value)) => value.abs_diff(requested),
        });
        candidates.first().map_or(Ok(None), |storage| {
            storage
                .lookup_hash(hash)
                .map(|bytes| bytes.map(|bytes| (storage.spec.file_type.clone(), bytes)))
        })
    }

    pub fn contains_hash(&self, hash: u64) -> bool {
        self.lookup_index.contains_hash(hash)
            || self
                .aliases
                .binary_search_by_key(&hash, |alias| alias.source)
                .is_ok()
    }

    fn aliases(&self) -> &[MediaAlias] {
        &self.aliases
    }
}

fn read_alias_parts(root: &Path, lookup_index: &PackedMediaLookupIndex) -> io::Result<Vec<MediaAlias>> {
    let mut paths = Vec::new();
    for item in read_dir(root)? {
        let path = item?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !name.starts_with(ALIAS_PREFIX) || !name.ends_with(ALIAS_SUFFIX) {
            continue;
        }
        let number = name
            .strip_prefix(ALIAS_PREFIX)
            .and_then(|value| value.strip_suffix(ALIAS_SUFFIX))
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid media alias part name: {}", path.display()),
                )
            })?;
        paths.push((number, path));
    }
    paths.sort_by_key(|(number, path)| (*number, path.clone()));
    if paths.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media alias part numbers are duplicated",
        ));
    }

    // The files are the durable representation. This flat vector is only a
    // compact in-memory lookup map built from mmap-validated immutable parts;
    // importing a new batch never rewrites it or any prior part.
    let mut aliases = Vec::new();
    for (_, path) in paths {
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("media alias part is not a regular file: {}", path.display()),
            ));
        }
        let length = usize::try_from(metadata.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "media alias part is too large")
        })?;
        let payload_length = length.checked_sub(ALIAS_FORMAT.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "media alias part is truncated")
        })?;
        if payload_length % ALIAS_RECORD_BYTES != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media alias part has a partial record",
            ));
        }
        let file = File::open(&path)?;
        let mapped = unsafe { memmap2::MmapOptions::new().map(&file)? };
        if mapped.get(..ALIAS_FORMAT.len()) != Some(ALIAS_FORMAT) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown media alias part format",
            ));
        }
        let count = payload_length / ALIAS_RECORD_BYTES;
        let mut previous_source = None;
        for position in 0..count {
            let start = ALIAS_FORMAT.len() + position * ALIAS_RECORD_BYTES;
            let record = &mapped[start..start + ALIAS_RECORD_BYTES];
            let source = u64::from_le_bytes(record[..8].try_into().unwrap());
            let target = u64::from_le_bytes(record[8..].try_into().unwrap());
            if source & (1_u64 << 63) == 0
                || target & (1_u64 << 63) == 0
                || source == target
                || previous_source.is_some_and(|previous| previous >= source)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media aliases are not sorted or contain an invalid hash",
                ));
            }
            if lookup_index.contains_hash(source) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media alias shadows an existing payload hash",
                ));
            }
            if !lookup_index.contains_hash(target) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media alias target is not an existing payload hash",
                ));
            }
            aliases.push(MediaAlias { source, target });
            previous_source = Some(source);
        }
    }
    aliases.sort_unstable_by_key(|alias| alias.source);
    if aliases
        .windows(2)
        .any(|pair| pair[0].source == pair[1].source)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media alias source appears in more than one part",
        ));
    }
    Ok(aliases)
}

/// Streaming writer. Callers sort title hashes and feed records in that order;
/// the writer never needs a large temporary image tree.
pub struct MediaStorageWriter {
    data: File,
    hashes: File,
    offsets: File,
    lengths: Option<File>,
    pending: Vec<PendingRecord>,
    next_hash: Option<u64>,
    offset: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingRecord {
    hash: u64,
    offset: u32,
    length: u32,
}

impl MediaStorageWriter {
    pub fn create(
        data: impl AsRef<Path>,
        hashes: impl AsRef<Path>,
        offsets: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Ok(Self {
            data: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(data)?,
            hashes: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(hashes)?,
            offsets: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(offsets)?,
            lengths: None,
            pending: Vec::new(),
            next_hash: None,
            offset: 0,
        })
    }

    /// Create a writer for the length-indexed format. Payloads may be
    /// appended in any order; hash, offset, and length arrays are sorted by
    /// hash when `finish` is called.
    pub fn create_with_lengths(
        data: impl AsRef<Path>,
        hashes: impl AsRef<Path>,
        offsets: impl AsRef<Path>,
        lengths: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Ok(Self {
            data: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(data)?,
            hashes: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(hashes)?,
            offsets: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(offsets)?,
            lengths: Some(
                OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(lengths)?,
            ),
            pending: Vec::new(),
            next_hash: None,
            offset: 0,
        })
    }

    pub fn append(&mut self, title: &str, bytes: &[u8]) -> io::Result<()> {
        let hash = media_title_hash(title);
        if self.lengths.is_none() && self.next_hash.is_some_and(|previous| previous >= hash) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media records must be appended in title-hash order",
            ));
        }
        let end = self
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "media size overflow"))?;
        if end >= MAX_DATA_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media storage would exceed 4 GiB",
            ));
        }
        let record = PendingRecord {
            hash,
            offset: u32::try_from(self.offset).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "media offset exceeds u32")
            })?,
            length: u32::try_from(bytes.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "media payload exceeds u32")
            })?,
        };
        self.data.write_all(bytes)?;
        if self.lengths.is_some() {
            self.pending.push(record);
        } else {
            self.hashes.write_all(&hash.to_le_bytes())?;
            self.offsets.write_all(&record.offset.to_le_bytes())?;
        }
        self.offset = end;
        self.next_hash = Some(hash);
        Ok(())
    }

    pub(crate) fn can_append(&self, bytes: usize) -> bool {
        self.offset
            .checked_add(bytes as u64)
            .is_some_and(|end| end < MAX_DATA_BYTES)
    }

    pub fn finish(mut self) -> io::Result<()> {
        if self.lengths.is_some() {
            self.pending.sort_unstable_by_key(|record| record.hash);
            for pair in self.pending.windows(2) {
                if pair[0].hash >= pair[1].hash {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "media records contain duplicate or unsorted title hashes",
                    ));
                }
            }
            for record in &self.pending {
                self.hashes.write_all(&record.hash.to_le_bytes())?;
                self.offsets.write_all(&record.offset.to_le_bytes())?;
                self.lengths
                    .as_mut()
                    .expect("lengths is present")
                    .write_all(&record.length.to_le_bytes())?;
            }
        }
        self.data.sync_all()?;
        self.hashes.sync_all()?;
        self.offsets.sync_all()?;
        if let Some(lengths) = self.lengths {
            lengths.sync_all()?;
        }
        Ok(())
    }
}

fn read_hash(bytes: &[u8], position: usize) -> u64 {
    let start = position * 8;
    u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap())
}

fn read_offset(bytes: &[u8], position: usize) -> u32 {
    let start = position * 4;
    u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap())
}

fn read_length(bytes: &[u8], position: usize) -> u32 {
    let start = position * 4;
    u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap())
}

static IMPORT_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Reservation {
    Existing,
    Aliased,
    Duplicate,
    Reserved,
}

struct RepositoryWriteLock {
    _file: File,
}

impl RepositoryWriteLock {
    fn acquire(root: &Path) -> io::Result<Self> {
        let path = root.join(".media-write.lock");
        #[cfg(unix)]
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        #[cfg(not(unix))]
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("media repository is already being written: {}", root.display()),
                    )
                } else {
                    error
                }
            })?;

        #[cfg(unix)]
        {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("media repository is already being written: {}", root.display()),
                    ));
                }
                return Err(error);
            }
        }

        if let Err(error) = file.set_len(0).and_then(|()| {
            writeln!(file, "pid={}", std::process::id()).and_then(|()| file.sync_all())
        }) {
            return Err(error);
        }
        Ok(Self { _file: file })
    }
}

struct PendingPart {
    stem: String,
    writer: Option<MediaStorageWriter>,
}

/// Exclusive writer for an immutable packed-media repository.
///
/// Existing parts are opened read-only and never rewritten. New parts are
/// built below a private staging directory; their companions are published
/// before the `.data` name, which is the only name discovered as a part.
pub struct MediaRepositoryWriter {
    root: PathBuf,
    staging: PathBuf,
    _lock: RepositoryWriteLock,
    existing: HashSet<u64>,
    existing_aliases: Vec<u64>,
    reserved: HashSet<u64>,
    pending_aliases: Vec<MediaAlias>,
    pending_alias_keys: HashSet<u64>,
    next_parts: HashMap<String, u32>,
    parts: BTreeMap<String, Vec<PendingPart>>,
    stats: KiwixPackStats,
}

impl MediaRepositoryWriter {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        create_dir_all(&root)?;
        let lock = RepositoryWriteLock::acquire(&root)?;
        reclaim_abandoned_staging(&root)?;
        reclaim_unpublished_parts(&root)?;
        let catalog = PackedMediaCatalog::open_directory(&root)?;
        let existing = catalog
            .storages()
            .iter()
            .flat_map(MediaStorage::hash_values)
            .collect();
        let existing_aliases = catalog
            .aliases()
            .iter()
            .map(|alias| alias.source)
            .collect();
        let sequence = IMPORT_SEQ.fetch_add(1, Ordering::Relaxed);
        let staging = root.join(format!(
            ".media-import-{}-{sequence}",
            std::process::id()
        ));
        create_dir_all(&staging)?;
        let mut manifest = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(staging.join(STAGING_MANIFEST_NAME))?;
        manifest.write_all(STAGING_MANIFEST)?;
        manifest.sync_all()?;
        sync_directory(&staging)?;
        sync_directory(&root)?;
        Ok(Self {
            root,
            staging,
            _lock: lock,
            existing,
            existing_aliases,
            reserved: HashSet::new(),
            pending_aliases: Vec::new(),
            pending_alias_keys: HashSet::new(),
            next_parts: HashMap::new(),
            parts: BTreeMap::new(),
            stats: KiwixPackStats::default(),
        })
    }

    pub fn set_entries_seen(&mut self, entries: u64) {
        self.stats.entries_seen = entries;
    }

    pub fn stats_mut(&mut self) -> &mut KiwixPackStats {
        &mut self.stats
    }

    pub fn reserve(&mut self, title: &str) -> Reservation {
        self.reserve_hash(media_title_hash(title))
    }

    pub(crate) fn reserve_with_legacy(
        &mut self,
        title: &str,
        legacy_hash: Option<u64>,
    ) -> Reservation {
        self.reserve_hash_with_legacy(media_title_hash(title), legacy_hash)
    }

    pub(crate) fn reserve_hash(&mut self, hash: u64) -> Reservation {
        self.reserve_hash_with_legacy(hash, None)
    }

    pub(crate) fn reserve_hash_with_legacy(
        &mut self,
        hash: u64,
        legacy_hash: Option<u64>,
    ) -> Reservation {
        if self.existing.contains(&hash) {
            self.stats.entries_skipped_existing += 1;
            return Reservation::Existing;
        }
        if self.existing_aliases.binary_search(&hash).is_ok() {
            self.stats.entries_skipped_existing += 1;
            return Reservation::Existing;
        }
        if !self.reserved.insert(hash) {
            self.stats.entries_skipped_duplicate += 1;
            return Reservation::Duplicate;
        }
        if let Some(target) = legacy_hash.filter(|target| {
            *target != hash && self.existing.contains(target)
        }) {
            self.pending_aliases.push(MediaAlias {
                source: hash,
                target,
            });
            self.pending_alias_keys.insert(hash);
            self.stats.aliases_added += 1;
            return Reservation::Aliased;
        }
        Reservation::Reserved
    }

    pub fn append_reserved(
        &mut self,
        file_type: &str,
        title: &str,
        bytes: &[u8],
    ) -> io::Result<()> {
        let hash = media_title_hash(title);
        if !self.reserved.contains(&hash) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media payload was not reserved",
            ));
        }
        if self.pending_alias_keys.contains(&hash) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media payload cannot be appended for an aliased title",
            ));
        }
        validate_file_type(file_type)?;
        let needs_part = self.parts
            .get(file_type)
            .and_then(|parts| parts.last())
            .and_then(|part| part.writer.as_ref())
            .is_none_or(|writer| !writer.can_append(bytes.len()));
        if needs_part {
            let part = self.next_part(file_type)?;
            let stem = format!("media-{file_type}-{part:04}");
            let writer = MediaStorageWriter::create_with_lengths(
                self.staging.join(format!("{stem}.data")),
                self.staging.join(format!("{stem}.hashes")),
                self.staging.join(format!("{stem}.offsets")),
                self.staging.join(format!("{stem}.lengths")),
            )?;
            let mut format = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(self.staging.join(format!("{stem}.format")))?;
            format.write_all(PART_FORMAT)?;
            format.sync_all()?;
            self.parts
                .entry(file_type.to_string())
                .or_default()
                .push(PendingPart {
                    stem,
                    writer: Some(writer),
                });
            self.stats.storages += 1;
        }
        let parts = self.parts.get_mut(file_type).expect("media part exists");
        parts
            .last_mut()
            .expect("a part was just created or already existed")
            .writer
            .as_mut()
            .expect("pending part writer is live")
            .append(title, bytes)?;
        self.stats.entries_written += 1;
        self.stats.bytes_written += bytes.len() as u64;
        Ok(())
    }

    fn next_part(&mut self, file_type: &str) -> io::Result<u32> {
        let mut part = *self.next_parts.entry(file_type.to_string()).or_insert_with(|| {
            highest_part(&self.root, file_type).unwrap_or(0)
        });
        loop {
            let stem = format!("media-{file_type}-{part:04}");
            let occupied = ["data", "hashes", "offsets", "lengths", "format"]
                .into_iter()
                .any(|extension| self.root.join(format!("{stem}.{extension}")).exists());
            if !occupied {
                self.next_parts
                    .insert(file_type.to_string(), part.saturating_add(1));
                return Ok(part);
            }
            part = part.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "too many media parts")
            })?;
        }
    }

    pub fn finish(mut self) -> io::Result<KiwixPackStats> {
        for parts in self.parts.values_mut() {
            for part in parts {
                part.writer
                    .take()
                    .expect("pending part writer is live")
                    .finish()?;
            }
        }

        let staged_aliases = self.stage_aliases()?;

        let mut published_companions = Vec::new();
        let mut published_data = HashSet::new();
        let result = (|| {
            for parts in self.parts.values() {
                for part in parts {
                    // The marker moves first. If the process dies before the
                    // data name is published, the next writer can identify
                    // and remove exactly these new-format companions.
                    for extension in ["format", "hashes", "offsets", "lengths"] {
                        let source = self.staging.join(format!("{}.{}", part.stem, extension));
                        let target = self.root.join(format!("{}.{}", part.stem, extension));
                        if target.exists() {
                            return Err(io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                format!("media part companion appeared during import: {}", target.display()),
                            ));
                        }
                        std::fs::rename(&source, &target)?;
                        published_companions.push(target);
                    }
                    sync_directory(&self.root)?;
                    let source = self.staging.join(format!("{}.data", part.stem));
                    let target = self.root.join(format!("{}.data", part.stem));
                    if target.exists() {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("media part appeared during import: {}", target.display()),
                        ));
                    }
                    std::fs::rename(&source, &target)?;
                    published_data.insert(part.stem.clone());
                    sync_directory(&self.root)?;
                }
            }
            if let Some(source) = staged_aliases {
                let file_name = source.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "staged alias part has no name")
                })?;
                let target = self.root.join(file_name);
                if target.exists() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("media alias part appeared during import: {}", target.display()),
                    ));
                }
                std::fs::rename(source, target)?;
                sync_directory(&self.root)?;
            }
            Ok(())
        })();
        if result.is_err() {
            for path in published_companions {
                let keep = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .is_some_and(|stem| published_data.contains(stem));
                if !keep {
                    let _ = std::fs::remove_file(path);
                }
            }
            let _ = sync_directory(&self.root);
        }
        result.map(|()| self.stats.clone())
    }

    fn next_alias_part(&self) -> io::Result<u32> {
        let mut part = highest_alias_part(&self.root).unwrap_or(0);
        loop {
            let path = self.root.join(alias_part_name(part));
            if !path.exists() {
                return Ok(part);
            }
            part = part.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "too many media alias parts")
            })?;
        }
    }

    fn stage_aliases(&mut self) -> io::Result<Option<PathBuf>> {
        if self.pending_aliases.is_empty() {
            return Ok(None);
        }
        self.pending_aliases.sort_unstable_by_key(|alias| alias.source);
        if self
            .pending_aliases
            .windows(2)
            .any(|pair| pair[0].source == pair[1].source)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media alias was added twice",
            ));
        }
        if self
            .pending_aliases
            .iter()
            .any(|alias| self.existing_aliases.binary_search(&alias.source).is_ok())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media alias already exists in the repository",
            ));
        }
        let part = self.next_alias_part()?;
        let path = self.staging.join(alias_part_name(part));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        file.write_all(ALIAS_FORMAT)?;
        for alias in &self.pending_aliases {
            file.write_all(&alias.source.to_le_bytes())?;
            file.write_all(&alias.target.to_le_bytes())?;
        }
        file.sync_all()?;
        Ok(Some(path))
    }
}

fn alias_part_name(part: u32) -> String {
    format!("{ALIAS_PREFIX}{part:08}{ALIAS_SUFFIX}")
}

fn parse_alias_part_name(name: &str) -> Option<u32> {
    name.strip_prefix(ALIAS_PREFIX)
        .and_then(|value| value.strip_suffix(ALIAS_SUFFIX))
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse().ok())
}

fn highest_alias_part(root: &Path) -> Option<u32> {
    read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|item| {
            item.file_name()
                .to_str()
                .and_then(parse_alias_part_name)
        })
        .max()
}

/// A killed writer cannot resume its private files: none of their names are
/// published and the index is finalized only by `finish`. Once the repository
/// lock is held, no live writer can own one of these directories, so reclaim
/// only direct, non-symlink children with the writer's exact private prefix.
fn reclaim_abandoned_staging(root: &Path) -> io::Result<()> {
    let mut removed = false;
    for item in read_dir(root)? {
        let item = item?;
        if !owned_staging_name(&item.file_name()) || !item.file_type()?.is_dir() {
            continue;
        }
        let manifest = item.path().join(STAGING_MANIFEST_NAME);
        let Ok(metadata) = manifest.symlink_metadata() else {
            continue;
        };
        if !metadata.file_type().is_file()
            || !matches!(
                std::fs::read(&manifest).as_deref(),
                Ok(bytes) if bytes == STAGING_MANIFEST
            )
        {
            continue;
        }
        std::fs::remove_dir_all(item.path())?;
        removed = true;
    }
    if removed {
        sync_directory(root)?;
    }
    Ok(())
}

/// Remove companions from an interrupted publication only when the exact
/// new-format marker proves that Sarun created them and no discoverable data
/// part was published. Unmarked files are never inferred to be disposable.
fn reclaim_unpublished_parts(root: &Path) -> io::Result<()> {
    let mut removed = false;
    for item in read_dir(root)? {
        let item = item?;
        if !item.file_type()?.is_file()
            || item.path().extension().and_then(|value| value.to_str()) != Some("format")
            || !matches!(
                std::fs::read(item.path()).as_deref(),
                Ok(bytes) if bytes == PART_FORMAT
            )
        {
            continue;
        }
        let Some(stem) = item
            .path()
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if !valid_part_stem(&stem) || root.join(format!("{stem}.data")).exists() {
            continue;
        }
        for extension in ["format", "hashes", "offsets", "lengths"] {
            let path = root.join(format!("{stem}.{extension}"));
            match std::fs::remove_file(&path) {
                Ok(()) => removed = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    if removed {
        sync_directory(root)?;
    }
    Ok(())
}

fn valid_part_stem(stem: &str) -> bool {
    let Some(rest) = stem.strip_prefix("media-") else {
        return false;
    };
    let Some((file_type, part)) = rest.rsplit_once('-') else {
        return false;
    };
    !file_type.is_empty() && validate_file_type(file_type).is_ok() && part.parse::<u32>().is_ok()
}

fn owned_staging_name(name: &std::ffi::OsStr) -> bool {
    let Some(rest) = name.to_str().and_then(|name| name.strip_prefix(".media-import-")) else {
        return false;
    };
    let Some((pid, sequence)) = rest.split_once('-') else {
        return false;
    };
    pid.parse::<u32>().is_ok() && sequence.parse::<u64>().is_ok()
}

impl Drop for MediaRepositoryWriter {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.staging);
    }
}

fn validate_file_type(file_type: &str) -> io::Result<()> {
    if file_type.is_empty()
        || file_type == "."
        || file_type == ".."
        || file_type.contains('/')
        || file_type.contains('\\')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid media file type: {file_type:?}"),
        ));
    }
    Ok(())
}

fn highest_part(root: &Path, file_type: &str) -> Option<u32> {
    let mut highest = None;
    for item in read_dir(root).ok()?.flatten() {
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name
            .strip_suffix(".data")
            .or_else(|| name.strip_suffix(".hashes"))
            .or_else(|| name.strip_suffix(".offsets"))
            .or_else(|| name.strip_suffix(".lengths"))
            .or_else(|| name.strip_suffix(".format")) else { continue };
        let Some(rest) = stem.strip_prefix("media-") else { continue };
        let Some((kind, part)) = rest.rsplit_once('-') else { continue };
        if kind == file_type {
            let Ok(part) = part.parse::<u32>() else { continue };
            highest = Some(highest.map_or(part, |old: u32| old.max(part)));
        }
    }
    highest
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn tempdir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sarun-media-packed-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn sorted_index_and_next_offset_define_length() {
        let root = tempdir();
        let data = root.join("jpg.data");
        let hashes = root.join("jpg.hashes");
        let offsets = root.join("jpg.offsets");
        let mut writer = MediaStorageWriter::create(&data, &hashes, &offsets).unwrap();
        let mut records = vec![("A.jpg", b"one".as_slice()), ("B.jpg", b"two-two".as_slice())];
        records.sort_by_key(|(title, _)| media_title_hash(title));
        for (title, bytes) in records {
            writer.append(title, bytes).unwrap();
        }
        writer.finish().unwrap();
        let storage = MediaStorage::open(MediaStorageSpec {
            data,
            hashes,
            offsets,
            lengths: None,
            file_type: "jpg".into(),
            width: None,
        })
        .unwrap();
        assert_eq!(storage.lookup("A.jpg").unwrap().unwrap(), b"one");
        assert_eq!(storage.lookup("B.jpg").unwrap().unwrap(), b"two-two");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn different_storages_can_supply_different_renditions() {
        let root = tempdir();
        let mut specs = Vec::new();
        for (label, bytes) in [("small", b"small".as_slice()), ("large", b"large".as_slice())] {
            let data = root.join(format!("{label}.data"));
            let hashes = root.join(format!("{label}.hashes"));
            let offsets = root.join(format!("{label}.offsets"));
            let mut writer = MediaStorageWriter::create(&data, &hashes, &offsets).unwrap();
            writer.append("A.jpg", bytes).unwrap();
            writer.finish().unwrap();
            specs.push(MediaStorageSpec {
                data,
                hashes,
                offsets,
                lengths: None,
                file_type: "jpg".into(),
                width: Some(if label == "small" { 120 } else { 960 }),
            });
        }
        let catalog = PackedMediaCatalog::open(specs).unwrap();
        assert_eq!(catalog.lookup("A.jpg", Some(100)).unwrap().unwrap(), b"small");
        assert_eq!(catalog.lookup("A.jpg", Some(800)).unwrap().unwrap(), b"large");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn catalogue_index_probes_only_matching_parts() {
        let root = tempdir();
        let mut specs = Vec::new();
        for part in 0..64 {
            let (title, bytes, width) = match part {
                6 => ("Rendition.jpg".to_owned(), b"small-first".to_vec(), Some(120)),
                7 => ("Rendition.jpg".to_owned(), b"small-second".to_vec(), Some(120)),
                55 => ("Rendition.jpg".to_owned(), b"large".to_vec(), Some(960)),
                _ => (
                    format!("Part-{part:04}.jpg"),
                    format!("payload-{part}").into_bytes(),
                    None,
                ),
            };
            let stem = format!("media-jpg-{part:04}");
            let data = root.join(format!("{stem}.data"));
            let hashes = root.join(format!("{stem}.hashes"));
            let offsets = root.join(format!("{stem}.offsets"));
            let mut writer = MediaStorageWriter::create(&data, &hashes, &offsets).unwrap();
            writer.append(&title, &bytes).unwrap();
            writer.finish().unwrap();
            specs.push(MediaStorageSpec {
                data,
                hashes,
                offsets,
                lengths: None,
                file_type: "jpg".into(),
                width,
            });
        }

        let catalog = PackedMediaCatalog::open(specs).unwrap();
        let mut expected_probes = vec![0_u64; 64];
        assert_eq!(catalog.lookup("Not-present.jpg", None).unwrap(), None);
        assert_eq!(
            catalog
                .storages
                .iter()
                .map(|storage| storage.binary_search_probes.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            expected_probes
        );

        assert_eq!(
            catalog.lookup("Part-0042.jpg", None).unwrap().unwrap(),
            b"payload-42"
        );
        expected_probes[42] = 1;
        assert_eq!(
            catalog
                .storages
                .iter()
                .map(|storage| storage.binary_search_probes.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            expected_probes
        );

        assert_eq!(
            catalog
                .lookup_with_type("Rendition.jpg", Some(100))
                .unwrap(),
            Some(("jpg".to_owned(), b"small-first".to_vec()))
        );
        expected_probes[6] += 1;
        assert_eq!(
            catalog
                .storages
                .iter()
                .map(|storage| storage.binary_search_probes.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            expected_probes
        );

        assert_eq!(
            catalog
                .lookup_with_type("Rendition.jpg", Some(800))
                .unwrap(),
            Some(("jpg".to_owned(), b"large".to_vec()))
        );
        expected_probes[55] += 1;
        assert_eq!(
            catalog
                .storages
                .iter()
                .map(|storage| storage.binary_search_probes.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            expected_probes
        );

        assert_eq!(catalog.lookup("Still-missing.jpg", None).unwrap(), None);
        assert_eq!(
            catalog
                .storages
                .iter()
                .map(|storage| storage.binary_search_probes.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            expected_probes
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_length_index_allows_data_order_to_differ_from_hash_order() {
        let root = tempdir();
        let data = root.join("media-jpg-0000.data");
        let hashes = root.join("media-jpg-0000.hashes");
        let offsets = root.join("media-jpg-0000.offsets");
        let lengths = root.join("media-jpg-0000.lengths");
        let mut writer = MediaStorageWriter::create_with_lengths(&data, &hashes, &offsets, &lengths)
            .unwrap();
        writer.append("B.jpg", b"payload-b").unwrap();
        writer.append("A.jpg", b"payload-a").unwrap();
        writer.finish().unwrap();

        let storage = MediaStorage::open(MediaStorageSpec {
            data,
            hashes,
            offsets,
            lengths: Some(lengths),
            file_type: "jpg".into(),
            width: None,
        })
        .unwrap();
        assert_eq!(storage.lookup("A.jpg").unwrap().unwrap(), b"payload-a");
        assert_eq!(storage.lookup("B.jpg").unwrap().unwrap(), b"payload-b");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_discovery_keeps_old_parts_without_lengths() {
        let root = tempdir();
        let data = root.join("media-png-0000.data");
        let hashes = root.join("media-png-0000.hashes");
        let offsets = root.join("media-png-0000.offsets");
        let mut writer = MediaStorageWriter::create(&data, &hashes, &offsets).unwrap();
        writer.append("Old.png", b"old-format").unwrap();
        writer.finish().unwrap();
        let catalog = PackedMediaCatalog::open_directory(&root).unwrap();
        assert_eq!(catalog.lookup("Old.png", None).unwrap().unwrap(), b"old-format");
        assert_eq!(catalog.storages()[0].spec().lengths, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn marked_new_part_requires_its_length_index() {
        let root = tempdir();
        let stem = "media-png-0000";
        std::fs::write(root.join(format!("{stem}.data")), b"payload").unwrap();
        std::fs::write(
            root.join(format!("{stem}.hashes")),
            media_title_hash("Marked.png").to_le_bytes(),
        )
        .unwrap();
        std::fs::write(root.join(format!("{stem}.offsets")), 0_u32.to_le_bytes()).unwrap();
        std::fs::write(root.join(format!("{stem}.format")), PART_FORMAT).unwrap();
        assert!(matches!(
            PackedMediaCatalog::open_directory(&root),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_import_is_globally_deduplicated_and_second_import_is_empty() {
        let root = tempdir();
        let mut first = MediaRepositoryWriter::open(&root).unwrap();
        first.set_entries_seen(2);
        assert_eq!(first.reserve("Shared.jpg"), Reservation::Reserved);
        first
            .append_reserved("jpg", "Shared.jpg", b"first")
            .unwrap();
        assert_eq!(first.reserve("Shared.jpg"), Reservation::Duplicate);
        let first_stats = first.finish().unwrap();
        assert_eq!(first_stats.entries_written, 1);
        assert_eq!(first_stats.entries_skipped_duplicate, 1);

        let before: Vec<_> = read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let mut second = MediaRepositoryWriter::open(&root).unwrap();
        second.set_entries_seen(1);
        assert_eq!(second.reserve("Shared.jpg"), Reservation::Existing);
        let second_stats = second.finish().unwrap();
        assert_eq!(second_stats.entries_written, 0);
        assert_eq!(second_stats.bytes_written, 0);
        assert_eq!(second_stats.entries_skipped_existing, 1);
        let after: Vec<_> = read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before.len(), after.len());
        assert!(PackedMediaCatalog::open_directory(&root)
            .unwrap()
            .contains_hash(media_title_hash("Shared.jpg")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn alias_lookup_returns_existing_payload_type_and_survives_reopen() {
        let root = tempdir();
        let legacy = "langru-500px-GDP_PPP_per_capita_CIS.svg.png";
        let normalized = "GDP_PPP_per_capita_CIS.svg";
        let legacy_hash = media_title_hash(legacy);
        let mut first = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(first.reserve(legacy), Reservation::Reserved);
        first
            .append_reserved("png", legacy, b"legacy-rendering")
            .unwrap();
        first.finish().unwrap();

        let before: Vec<_> = read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let mut second = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(
            second.reserve_with_legacy(normalized, Some(legacy_hash)),
            Reservation::Aliased
        );
        assert!(second
            .append_reserved("svg", normalized, b"must-not-be-written")
            .is_err());
        let stats = second.finish().unwrap();
        assert_eq!(stats.aliases_added, 1);
        assert_eq!(stats.entries_written, 0);

        let after: Vec<_> = read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            after
                .iter()
                .filter(|name| name.to_string_lossy().ends_with(".data"))
                .count(),
            before
                .iter()
                .filter(|name| name.to_string_lossy().ends_with(".data"))
                .count()
        );
        let catalog = PackedMediaCatalog::open_directory(&root).unwrap();
        assert_eq!(
            catalog.lookup_with_type(normalized, None).unwrap(),
            Some(("png".to_owned(), b"legacy-rendering".to_vec()))
        );
        drop(catalog);
        let reopened = PackedMediaCatalog::open_directory(&root).unwrap();
        assert_eq!(
            reopened.lookup_with_type(normalized, None).unwrap(),
            Some(("png".to_owned(), b"legacy-rendering".to_vec()))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dropped_alias_import_is_not_published_and_repeat_is_a_noop() {
        let root = tempdir();
        let legacy = "langru-500px-Repeat.jpg";
        let normalized = "Repeat.jpg";
        let legacy_hash = media_title_hash(legacy);
        let mut first = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(first.reserve(legacy), Reservation::Reserved);
        first.append_reserved("jpg", legacy, b"payload").unwrap();
        first.finish().unwrap();

        let mut interrupted = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(
            interrupted.reserve_with_legacy(normalized, Some(legacy_hash)),
            Reservation::Aliased
        );
        drop(interrupted);
        assert!(!read_dir(&root)
            .unwrap()
            .any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(ALIAS_PREFIX)));
        assert_eq!(
            PackedMediaCatalog::open_directory(&root)
                .unwrap()
                .lookup(normalized, None)
                .unwrap(),
            None
        );

        let mut repeat = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(
            repeat.reserve_with_legacy(normalized, Some(legacy_hash)),
            Reservation::Aliased
        );
        let stats = repeat.finish().unwrap();
        assert_eq!(stats.aliases_added, 1);

        let before = read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(ALIAS_PREFIX))
            .count();
        let mut second = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(
            second.reserve_with_legacy(normalized, Some(legacy_hash)),
            Reservation::Existing
        );
        let stats = second.finish().unwrap();
        assert_eq!(stats.aliases_added, 0);
        assert_eq!(stats.entries_written, 0);
        let after = read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(ALIAS_PREFIX))
            .count();
        assert_eq!(before, after);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_alias_part_rejects_non_payload_target() {
        let root = tempdir();
        let data = root.join("media-jpg-0000.data");
        let hashes = root.join("media-jpg-0000.hashes");
        let offsets = root.join("media-jpg-0000.offsets");
        let mut writer = MediaStorageWriter::create(&data, &hashes, &offsets).unwrap();
        writer.append("Real.jpg", b"real").unwrap();
        writer.finish().unwrap();

        let source = media_title_hash("Alias.jpg");
        let target = media_title_hash("Missing.jpg");
        let mut alias = File::create(root.join(alias_part_name(0))).unwrap();
        alias.write_all(ALIAS_FORMAT).unwrap();
        alias.write_all(&source.to_le_bytes()).unwrap();
        alias.write_all(&target.to_le_bytes()).unwrap();
        alias.sync_all().unwrap();
        assert!(matches!(
            PackedMediaCatalog::open_directory(&root),
            Err(error) if error.kind() == io::ErrorKind::InvalidData
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overlap_and_duplicate_across_file_types_store_one_payload() {
        let root = tempdir();
        let mut first = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(first.reserve("Overlap.png"), Reservation::Reserved);
        first
            .append_reserved("png", "Overlap.png", b"png")
            .unwrap();
        first.finish().unwrap();

        let mut second = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(second.reserve("Overlap.png"), Reservation::Existing);
        assert_eq!(second.reserve("Missing.jpg"), Reservation::Reserved);
        second
            .append_reserved("jpg", "Missing.jpg", b"jpg")
            .unwrap();
        let stats = second.finish().unwrap();
        assert_eq!(stats.entries_written, 1);
        assert_eq!(stats.bytes_written, 3);
        let catalog = PackedMediaCatalog::open_directory(&root).unwrap();
        assert_eq!(catalog.lookup("Overlap.png", None).unwrap().unwrap(), b"png");
        assert_eq!(catalog.lookup("Missing.jpg", None).unwrap().unwrap(), b"jpg");
        assert_eq!(catalog.storages().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_parts_skip_existing_part_names() {
        let root = tempdir();
        let mut first = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(first.reserve("First.jpg"), Reservation::Reserved);
        first
            .append_reserved("jpg", "First.jpg", b"first")
            .unwrap();
        first.finish().unwrap();

        let mut second = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(second.reserve("Second.jpg"), Reservation::Reserved);
        second
            .append_reserved("jpg", "Second.jpg", b"second")
            .unwrap();
        second.finish().unwrap();
        assert!(root.join("media-jpg-0000.data").is_file());
        assert!(root.join("media-jpg-0001.data").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn advisory_lock_recovers_from_stale_inode_and_releases_after_writer_drop() {
        let root = tempdir();
        let lock_path = root.join(".media-write.lock");
        std::fs::write(&lock_path, b"pid=999999\n").unwrap();

        let first = MediaRepositoryWriter::open(&root).unwrap();
        assert!(matches!(
            MediaRepositoryWriter::open(&root),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        drop(first);
        assert!(lock_path.is_file(), "the lock inode is not deletion-based state");
        let second = MediaRepositoryWriter::open(&root).unwrap();
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn repository_lock_does_not_follow_a_symlink() {
        let root = tempdir();
        let outside = root.join("outside");
        std::fs::write(&outside, b"not a lock").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".media-write.lock")).unwrap();
        assert!(MediaRepositoryWriter::open(&root).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"not a lock");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn next_writer_reclaims_only_abandoned_owned_staging() {
        let root = tempdir();
        let abandoned = root.join(".media-import-999999-42");
        std::fs::create_dir(&abandoned).unwrap();
        std::fs::write(abandoned.join(STAGING_MANIFEST_NAME), STAGING_MANIFEST).unwrap();
        std::fs::write(abandoned.join("media-webp-0000.data"), b"partial").unwrap();

        let unproven = root.join(".media-import-999999-44");
        std::fs::create_dir(&unproven).unwrap();
        std::fs::write(unproven.join("media-webp-0000.data"), b"not ours").unwrap();

        let foreign = root.join(".media-import-not-owned");
        std::fs::create_dir(&foreign).unwrap();
        let symlink_target = root.join("foreign-target");
        std::fs::create_dir(&symlink_target).unwrap();
        let symlink = root.join(".media-import-999999-43");
        std::os::unix::fs::symlink(&symlink_target, &symlink).unwrap();

        let writer = MediaRepositoryWriter::open(&root).unwrap();
        assert!(!abandoned.exists());
        assert!(unproven.is_dir());
        assert!(foreign.is_dir());
        assert!(symlink.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(symlink_target.is_dir());
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn next_writer_reclaims_only_marked_unpublished_companions() {
        let root = tempdir();
        let owned = "media-webp-0007";
        std::fs::write(root.join(format!("{owned}.format")), PART_FORMAT).unwrap();
        std::fs::write(root.join(format!("{owned}.hashes")), b"partial").unwrap();
        let unmarked = "media-webp-0008";
        std::fs::write(root.join(format!("{unmarked}.hashes")), b"foreign").unwrap();

        let writer = MediaRepositoryWriter::open(&root).unwrap();
        assert!(!root.join(format!("{owned}.format")).exists());
        assert!(!root.join(format!("{owned}.hashes")).exists());
        assert_eq!(
            std::fs::read(root.join(format!("{unmarked}.hashes"))).unwrap(),
            b"foreign"
        );
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_index_staging_never_publishes_a_data_part() {
        let root = tempdir();
        let mut writer = MediaRepositoryWriter::open(&root).unwrap();
        assert_eq!(writer.reserve("Duplicate.jpg"), Reservation::Reserved);
        writer
            .append_reserved("jpg", "Duplicate.jpg", b"one")
            .unwrap();
        writer
            .append_reserved("jpg", "Duplicate.jpg", b"two")
            .unwrap();
        assert!(writer.finish().is_err());
        assert!(!read_dir(&root)
            .unwrap()
            .any(|entry| entry.unwrap().path().extension().and_then(|value| value.to_str()) == Some("data")));
        std::fs::remove_dir_all(root).unwrap();
    }

}
