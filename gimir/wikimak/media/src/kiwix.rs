//! Direct read-only access to images in a Kiwix/ZIM archive.
//!
//! ZIM is already a packed, indexed container.  The mirror therefore keeps a
//! selected ZIM path as an image source instead of unpacking it into one file
//! per image or creating a second giant temporary archive.  Opening a source
//! scans directory entries only; image cluster bytes are decompressed on the
//! first request for that image.  The `zim` reader also presents split files
//! (`foo.zimaa`, `foo.zimab`, ...) as one logical archive.

use std::collections::BTreeMap;
use std::fs::create_dir_all;
use std::io;
use std::path::{Path, PathBuf};

use crate::url::normalize_filename;
use crate::{media_title_hash, MediaStorageWriter};

#[derive(Debug, thiserror::Error)]
pub enum KiwixError {
    #[error("kiwix zim: {0}")]
    Zim(#[from] zim::Error),
    #[error("kiwix image pack: {0}")]
    Io(#[from] io::Error),
    #[error("kiwix source key is too long: {0} bytes")]
    KeyTooLong(usize),
}

/// A directory-only index over image entries in a local ZIM archive.
///
/// `zim::Zim` memory-maps the archive and does not read cluster payloads at
/// open.  The small sorted vector here contains only normalized image names
/// and their directory-entry indices, so lookup is binary-searchable without
/// retaining image bytes in RAM.
pub struct KiwixImageSource {
    path: PathBuf,
    zim: zim::Zim,
    entries: Vec<ImageEntry>,
}

#[derive(Debug, Clone)]
struct ImageEntry {
    key: String,
    directory_index: u32,
    file_type: String,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct KiwixPackStats {
    pub entries_seen: u64,
    pub entries_written: u64,
    pub bytes_written: u64,
    pub storages: u64,
}

impl KiwixImageSource {
    /// Open a `.zim` file or the first `.zimaa` split file.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, KiwixError> {
        let path = path.into();
        let zim = zim::Zim::new(&path)?;
        let mut entries = Vec::new();
        for (directory_index, item) in zim.iterate_by_urls().enumerate() {
            let directory_index = u32::try_from(directory_index).map_err(|_| {
                KiwixError::Zim(zim::Error::OutOfBounds)
            })?;
            let entry = item?;
            let file_type = match &entry.mime_type {
                zim::MimeType::Type(mime) if mime_is_image(mime) => mime_file_type(mime),
                _ if entry.namespace == zim::Namespace::ImagesFile => "unknown".to_string(),
                _ => continue,
            };
            if file_type.is_empty() {
                continue;
            }
            let key = image_key(&entry.url);
            if key.is_empty() {
                continue;
            }
            if key.len() > 1024 * 1024 {
                return Err(KiwixError::KeyTooLong(key.len()));
            }
            entries.push(ImageEntry {
                key,
                directory_index,
                file_type,
            });
        }
        entries.sort_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then(left.file_type.cmp(&right.file_type))
                .then(left.directory_index.cmp(&right.directory_index))
        });
        // Duplicate image names are not useful to a MediaWiki resolver.  Keep
        // one directory entry per name and payload type deterministically.
        entries.dedup_by(|left, right| {
            left.key == right.key && left.file_type == right.file_type
        });
        Ok(Self {
            path,
            zim,
            entries,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, filename: &str) -> bool {
        self.find(filename).is_some()
    }

    /// Read one original image.  Only its containing ZIM cluster is touched;
    /// no extracted image file is created.
    pub fn get(&self, filename: &str) -> Result<Option<Vec<u8>>, KiwixError> {
        self.get_with_type(filename)
            .map(|value| value.map(|(_, bytes)| bytes))
    }

    pub fn get_with_type(
        &self,
        filename: &str,
    ) -> Result<Option<(String, Vec<u8>)>, KiwixError> {
        let Some(image) = self.find(filename) else {
            return Ok(None);
        };
        let entry = self.zim.get_by_url_index(image.directory_index)?;
        let Some(content) = self.zim.entry_content(&entry)? else {
            return Ok(None);
        };
        Ok(Some((image.file_type.clone(), content.to_vec()?)))
    }

    /// Pack the selected image payloads into the simple SoA storage format.
    ///
    /// Directory metadata is kept in memory, while each image cluster is
    /// decompressed and written once.  Payloads are grouped by MIME-derived
    /// storage type, sorted by the 63-bit title hash, and split before the
    /// data file reaches 4 GiB.  No extracted one-file-per-image tree is
    /// created.
    pub fn pack(&self, output_dir: impl AsRef<Path>) -> Result<KiwixPackStats, KiwixError> {
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
        if staging.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("staging path already exists: {}", staging.display()),
            )
            .into());
        }
        create_dir_all(&staging)?;
        match self.pack_to(&staging) {
            Ok(stats) => {
                if let Err(error) = std::fs::rename(&staging, output_dir) {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(error.into());
                }
                Ok(stats)
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(error)
            }
        }
    }

    fn pack_to(&self, output_dir: &Path) -> Result<KiwixPackStats, KiwixError> {
        let mut groups = BTreeMap::<String, Vec<&ImageEntry>>::new();
        for entry in &self.entries {
            groups
                .entry(entry.file_type.clone())
                .or_default()
                .push(entry);
        }

        let mut stats = KiwixPackStats {
            entries_seen: self.entries.len() as u64,
            ..KiwixPackStats::default()
        };
        for (file_type, mut entries) in groups {
            let total_entries = entries.len();
            eprintln!("wikimak kiwix-pack: {} entries of {}", total_entries, file_type);
            entries.sort_by(|left, right| {
                media_title_hash(&left.key)
                    .cmp(&media_title_hash(&right.key))
                    .then(left.key.cmp(&right.key))
                    .then(left.directory_index.cmp(&right.directory_index))
            });
            let mut part = 0_u32;
            let mut writer = None;
            let mut last_hash = None;
            for (entry_number, entry) in entries.into_iter().enumerate() {
                if entry_number % 1000 == 0 {
                    eprintln!(
                        "wikimak kiwix-pack: {} {}/{}",
                        file_type,
                        entry_number,
                        total_entries
                    );
                }
                let hash = media_title_hash(&entry.key);
                // A 63-bit collision is intentionally tolerated by the
                // bootstrap format. Keep the lexicographically first title
                // selected by the ordering above.
                if last_hash == Some(hash) {
                    continue;
                }
                let zim_entry = self.zim.get_by_url_index(entry.directory_index)?;
                let Some(content) = self.zim.entry_content(&zim_entry)? else {
                    continue;
                };
                let bytes = content.to_vec()?;
                if bytes.is_empty() {
                    continue;
                }
                if writer
                    .as_ref()
                    .is_some_and(|writer: &MediaStorageWriter| !writer.can_append(bytes.len()))
                {
                    writer.take().expect("writer was present").finish()?;
                    part = part.checked_add(1).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "too many media parts")
                    })?;
                }
                if writer.is_none() {
                    writer = Some(MediaStorageWriter::create(
                        output_dir.join(format!("media-{file_type}-{part:04}.data")),
                        output_dir.join(format!("media-{file_type}-{part:04}.hashes")),
                        output_dir.join(format!("media-{file_type}-{part:04}.offsets")),
                    )?);
                    stats.storages += 1;
                }
                writer
                    .as_mut()
                    .expect("writer was just created")
                    .append(&entry.key, &bytes)?;
                stats.entries_written += 1;
                stats.bytes_written += bytes.len() as u64;
                last_hash = Some(hash);
            }
            if let Some(writer) = writer.take() {
                writer.finish()?;
            }
        }
        Ok(stats)
    }

    fn find(&self, filename: &str) -> Option<&ImageEntry> {
        let key = image_key(filename);
        self.entries
            .binary_search_by(|entry| entry.key.cmp(&key))
            .ok()
            .map(|index| &self.entries[index])
    }
}

/// Canonical key shared by ZIM directory entries and MediaWiki image URLs.
pub fn image_key(filename: &str) -> String {
    let filename = filename.trim().trim_start_matches('/');
    let filename = filename
        .strip_prefix("_assets_/")
        .and_then(|rest| rest.split_once('/').map(|(_, name)| name))
        .unwrap_or(filename);
    let filename = percent_decode(filename);
    let filename = filename.trim_matches('"');
    let filename = filename
        .strip_prefix("File:")
        .or_else(|| filename.strip_prefix("file:"))
        .unwrap_or(filename);
    normalize_filename(filename)
}

fn mime_is_image(mime: &str) -> bool {
    mime.trim_start()
        .split(';')
        .next()
        .is_some_and(|value| value.starts_with("image/"))
}

fn mime_file_type(mime: &str) -> String {
    mime.trim()
        .split(';')
        .next()
        .and_then(|value| value.strip_prefix("image/"))
        .unwrap_or("unknown")
        .to_string()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'%'
            && position + 2 < bytes.len()
            && hex(bytes[position + 1]).is_some()
            && hex(bytes[position + 2]).is_some()
        {
            decoded.push((hex(bytes[position + 1]).unwrap() << 4) | hex(bytes[position + 2]).unwrap());
            position += 3;
        } else {
            decoded.push(bytes[position]);
            position += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_mediawiki_file_names() {
        assert_eq!(image_key("File:foo bar.jpg"), "Foo_bar.jpg");
        assert_eq!(image_key("/file:foo%20bar.jpg"), "Foo_bar.jpg");
        assert_eq!(
            image_key("_assets_/0123456789abcdef0123456789abcdef/Foo%20bar.jpg"),
            "Foo_bar.jpg"
        );
    }

    #[test]
    fn rejects_missing_source_without_writing_anything() {
        let path = std::env::temp_dir().join(format!(
            "sarun-missing-kiwix-{}",
            std::process::id()
        ));
        assert!(KiwixImageSource::open(&path).is_err());
        assert!(!path.exists());
    }

}
