//! Small, deliberately boring packed image storage.
//!
//! Each data file contains concatenated image payloads.  Its companion index
//! has two arrays: sorted `u64` title hashes and same-ordinal `u32` data
//! offsets.  Payload length is the next offset (or the data-file length for
//! the final entry), so length and MIME are not duplicated in the index.  A
//! data file is capped below 4 GiB, keeping offsets 32-bit as specified by
//! the bootstrap image format.

use std::fs::{File, OpenOptions, read_dir};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::url::normalize_filename;

pub const MAX_DATA_BYTES: u64 = u32::MAX as u64 + 1;
/// The same hash family as the Wikipedia title index's hashed-title branch,
/// with namespace 6 (File) as the seed.  The high bit marks the hash form.
pub fn media_title_hash(title: &str) -> u64 {
    let title = normalize_filename(title.trim().trim_start_matches("File:"));
    let hash = xxhash_rust::xxh3::xxh3_64_with_seed(title.as_bytes(), 6);
    (1_u64 << 63) | (hash & ((1_u64 << 63) - 1))
}

#[derive(Clone, Debug)]
pub struct MediaStorageSpec {
    pub data: PathBuf,
    pub hashes: PathBuf,
    pub offsets: PathBuf,
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
    data_len: u64,
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
        if (0..count).any(|position| u64::from(read_offset(&offsets, position)) >= data_len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media index points past data",
            ));
        }
        Ok(Self {
            spec,
            data,
            hashes,
            offsets,
            data_len,
        })
    }

    pub fn spec(&self) -> &MediaStorageSpec {
        &self.spec
    }

    pub fn lookup(&self, title: &str) -> io::Result<Option<Vec<u8>>> {
        let hash = media_title_hash(title);
        let Ok(position) = self.binary_search_hash(hash) else {
            return Ok(None);
        };
        let start = u64::from(read_offset(&self.offsets, position));
        let end = (position + 1 < self.hashes.len() / 8)
            .then(|| u64::from(read_offset(&self.offsets, position + 1)))
            .unwrap_or(self.data_len);
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
}

/// Open several candidate storages and choose the rendition nearest to the
/// requested width.  Storage type and rendition width live in filenames/
/// descriptors, never in each 12-byte index entry.
pub struct PackedMediaCatalog {
    storages: Vec<MediaStorage>,
}

impl PackedMediaCatalog {
    pub fn open(specs: impl IntoIterator<Item = MediaStorageSpec>) -> io::Result<Self> {
        let storages = specs
            .into_iter()
            .map(MediaStorage::open)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { storages })
    }

    pub fn storages(&self) -> &[MediaStorage] {
        &self.storages
    }

    /// Discover the `media-<type>-<part>.{data,hashes,offsets}` files emitted
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
            specs.push(MediaStorageSpec {
                data: path,
                hashes: root.join(format!("{stem}.hashes")),
                offsets: root.join(format!("{stem}.offsets")),
                file_type: file_type.to_string(),
                width: None,
            });
        }
        specs.sort_by(|left, right| left.data.cmp(&right.data));
        Self::open(specs)
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
        let mut candidates = self
            .storages
            .iter()
            .filter(|storage| storage.lookup_exists(title))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|storage| match (width, storage.spec.width) {
            (None, None) => 0,
            (None, Some(value)) => value,
            (Some(_requested), None) => u32::MAX / 2,
            (Some(requested), Some(value)) => value.abs_diff(requested),
        });
        candidates.first().map_or(Ok(None), |storage| {
            storage
                .lookup(title)
                .map(|bytes| bytes.map(|bytes| (storage.spec.file_type.clone(), bytes)))
        })
    }
}

impl MediaStorage {
    fn lookup_exists(&self, title: &str) -> bool {
        let hash = media_title_hash(title);
        self.binary_search_hash(hash).is_ok()
    }
}

/// Streaming writer. Callers sort title hashes and feed records in that order;
/// the writer never needs a large temporary image tree.
pub struct MediaStorageWriter {
    data: File,
    hashes: File,
    offsets: File,
    next_hash: Option<u64>,
    offset: u64,
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
            next_hash: None,
            offset: 0,
        })
    }

    pub fn append(&mut self, title: &str, bytes: &[u8]) -> io::Result<()> {
        let hash = media_title_hash(title);
        if self.next_hash.is_some_and(|previous| previous >= hash) {
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
        self.data.write_all(bytes)?;
        self.hashes.write_all(&hash.to_le_bytes())?;
        self.offsets.write_all(&(self.offset as u32).to_le_bytes())?;
        self.offset = end;
        self.next_hash = Some(hash);
        Ok(())
    }

    pub(crate) fn can_append(&self, bytes: usize) -> bool {
        self.offset
            .checked_add(bytes as u64)
            .is_some_and(|end| end < MAX_DATA_BYTES)
    }

    pub fn finish(self) -> io::Result<()> {
        self.data.sync_all()?;
        self.hashes.sync_all()?;
        self.offsets.sync_all()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sarun-media-packed-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
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
                file_type: "jpg".into(),
                width: Some(if label == "small" { 120 } else { 960 }),
            });
        }
        let catalog = PackedMediaCatalog::open(specs).unwrap();
        assert_eq!(catalog.lookup("A.jpg", Some(100)).unwrap().unwrap(), b"small");
        assert_eq!(catalog.lookup("A.jpg", Some(800)).unwrap().unwrap(), b"large");
        std::fs::remove_dir_all(root).unwrap();
    }

}
