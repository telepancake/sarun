//! Direct read-only access to images in a Kiwix/ZIM archive.
//!
//! ZIM is already a packed, indexed container.  The mirror therefore keeps a
//! selected ZIM path as an image source instead of unpacking it into one file
//! per image or creating a second giant temporary archive.  Opening a source
//! scans directory entries only; image cluster bytes are decompressed on the
//! first request for that image.  The `zim` reader also presents split files
//! (`foo.zimaa`, `foo.zimab`, ...) as one logical archive.

use std::io;
use std::path::{Path, PathBuf};

use crate::url::normalize_filename;
pub use crate::packed::KiwixPackStats;
use crate::packed::{media_title_hash, MediaRepositoryWriter, Reservation};

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
    legacy_hash: Option<u64>,
    directory_index: u32,
    file_type: String,
    preference: KiwixImagePreference,
}

/// Preference among Kiwix directory entries that resolve to one Wikimedia
/// file title. A direct asset is the best representation; when Kiwix only
/// carries generated renditions, retain the largest one rather than whichever
/// MIME name happens to sort first.
#[derive(Debug, Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct KiwixImagePreference {
    known_image_type: bool,
    direct_asset: bool,
    rendition_width: u32,
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
                zim::MimeType::Type(mime) if mime_is_supported_media(mime) => mime_file_type(mime),
                _ if entry.namespace == zim::Namespace::ImagesFile => "unknown".to_string(),
                _ => continue,
            };
            if file_type.is_empty() {
                continue;
            }
            let (key, preference) = image_identity(&entry.url, file_type != "unknown");
            let legacy_key = legacy_image_key(&entry.url);
            if key.is_empty() {
                continue;
            }
            if key.len() > 1024 * 1024 {
                return Err(KiwixError::KeyTooLong(key.len()));
            }
            let legacy_hash = (legacy_key != key).then(|| media_title_hash(&legacy_key));
            entries.push(ImageEntry {
                key,
                legacy_hash,
                directory_index,
                file_type,
                preference,
            });
        }
        entries.sort_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then(right.preference.cmp(&left.preference))
                .then(left.file_type.cmp(&right.file_type))
                .then(left.directory_index.cmp(&right.directory_index))
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

    /// Create a new packed repository from this source.
    pub fn pack(&self, output_dir: impl AsRef<Path>) -> Result<KiwixPackStats, KiwixError> {
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

    /// Add only titles absent from the existing repository. Existing parts
    /// remain immutable, and all newly selected payloads are staged and
    /// published as fresh parts.
    pub fn import_missing(
        &self,
        repository_dir: impl AsRef<Path>,
    ) -> Result<KiwixPackStats, KiwixError> {
        let mut writer = MediaRepositoryWriter::open(repository_dir)?;
        writer.set_entries_seen(self.entries.len() as u64);
        for entry in &self.entries {
            if !matches!(
                writer.reserve_with_legacy(&entry.key, entry.legacy_hash),
                Reservation::Reserved
            ) {
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
            writer.append_reserved(&entry.file_type, &entry.key, &bytes)?;
        }
        Ok(writer.finish()?)
    }

    fn find(&self, filename: &str) -> Option<&ImageEntry> {
        let key = image_key(filename);
        let index = self
            .entries
            .partition_point(|entry| entry.key.as_str() < key.as_str());
        self.entries
            .get(index)
            .filter(|entry| entry.key == key)
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

/// Canonical key shared by ZIM directory entries and MediaWiki image URLs.
pub fn image_key(filename: &str) -> String {
    image_identity(filename, true).0
}

pub(crate) fn image_identity(
    filename: &str,
    known_image_type: bool,
) -> (String, KiwixImagePreference) {
    let filename = decoded_media_filename(filename);
    let (filename, rendition_width) = strip_kiwix_rendition_prefix(&filename);
    (
        normalize_filename(filename),
        KiwixImagePreference {
            known_image_type,
            direct_asset: rendition_width.is_none(),
            rendition_width: rendition_width.unwrap_or(0),
        },
    )
}

/// The identity used by the pre-normalization importer. This is retained
/// only to bridge repositories that already contain Kiwix rendition names.
pub(crate) fn legacy_image_key(filename: &str) -> String {
    normalize_filename(&decoded_media_filename(filename))
}

fn decoded_media_filename(filename: &str) -> String {
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
    filename.to_owned()
}

/// Kiwix rewrites a Wikimedia thumbnail into an asset name such as
/// `langru-500px-GDP_PPP_per_capita_CIS.svg.png`.  The language and width
/// belong to the rendition, while the filename after that prefix is the
/// MediaWiki identity used by Sarun's media route.  Keep the source identity
/// when importing so the packed repository can be queried by the page's
/// original `File:` name.
fn strip_kiwix_rendition_prefix(filename: &str) -> (&str, Option<u32>) {
    let Some(rest) = filename.strip_prefix("lang") else {
        return (filename, None);
    };
    let Some((language, rest)) = rest.split_once('-') else {
        return (filename, None);
    };
    if !(2..=8).contains(&language.len())
        || !language.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return (filename, None);
    }
    let Some((width, original)) = rest.split_once("px-") else {
        return (filename, None);
    };
    if width.is_empty()
        || !width.bytes().all(|byte| byte.is_ascii_digit())
        || original.is_empty()
    {
        return (filename, None);
    }
    let Ok(width) = width.parse::<u32>() else {
        return (filename, None);
    };
    let original = if has_ascii_suffix(original, ".svg.png")
        || has_ascii_suffix(original, ".tif.jpg")
    {
        &original[..original.len() - 4]
    } else {
        original
    };
    (original, Some(width))
}

fn has_ascii_suffix(value: &str, suffix: &str) -> bool {
    value.len() >= suffix.len()
        && value[value.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn mime_is_supported_media(mime: &str) -> bool {
    mime.trim_start()
        .split(';')
        .next()
        .is_some_and(|value| {
            value.starts_with("image/") || matches!(value, "audio/ogg" | "audio/oga")
        })
}

fn mime_file_type(mime: &str) -> String {
    mime.trim()
        .split(';')
        .next()
        .and_then(|value| {
            value
                .strip_prefix("image/")
                .or_else(|| value.strip_prefix("audio/"))
        })
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
    fn canonicalizes_kiwix_renditions_to_the_original_media_title() {
        assert_eq!(
            image_key(
                concat!(
                    "_assets_/0c70a452f799bfe840676ee341124611/",
                    "langru-500px-GDP_PPP_per_capita_CIS.svg.png"
                )
            ),
            "GDP_PPP_per_capita_CIS.svg"
        );
        assert_eq!(
            image_key(
                concat!(
                    "_assets_/0c70a452f799bfe840676ee341124611/",
                    "langru-500px-Crime_and_incarceration_rates_in_Russia.svg.png"
                )
            ),
            "Crime_and_incarceration_rates_in_Russia.svg"
        );
        assert_eq!(
            image_key(
                concat!(
                    "_assets_/0c70a452f799bfe840676ee341124611/",
                    "langru-250px-RUSMARKA-3541-3544list.jpg"
                )
            ),
            "RUSMARKA-3541-3544list.jpg"
        );
    }

    #[test]
    fn prefers_direct_assets_then_larger_renditions() {
        let (_, direct) = image_identity("GDP_PPP_per_capita_CIS.svg", true);
        let (_, small) = image_identity(
            "langru-250px-GDP_PPP_per_capita_CIS.svg.png",
            true,
        );
        let (_, large) = image_identity(
            "langru-500px-GDP_PPP_per_capita_CIS.svg.png",
            true,
        );
        let (_, unknown_direct) = image_identity("GDP_PPP_per_capita_CIS.svg", false);
        assert!(direct > large);
        assert!(large > small);
        assert!(small > unknown_direct);
    }

    #[test]
    fn accepts_only_the_audio_types_used_by_wikimedia_anthem_assets() {
        assert!(mime_is_supported_media("audio/ogg"));
        assert!(mime_is_supported_media("audio/oga; codecs=vorbis"));
        assert!(!mime_is_supported_media("audio/mpeg"));
        assert_eq!(mime_file_type("audio/ogg"), "ogg");
        assert_eq!(mime_file_type("audio/oga"), "oga");
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
