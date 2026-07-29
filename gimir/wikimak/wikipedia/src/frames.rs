//! Frame compression — the discipline the depot expects and the whole
//! design exists for (tiered-VBF doc §8: "a depot whose on-disk size
//! matches its uncompressed input has not rendered this design").
//!
//! * f0 holds the newest revision's record, standalone zstd. After
//!   seed training it uses the instance's pretrained revision
//!   dictionary.
//! * f1 holds the older records (newest-first, concatenated), zstd with
//!   `ZSTD_CCtx_refPrefix` anchored on f0's RECORD — successive
//!   revisions are ~99% identical, so the frame costs ~the delta.
//! * A sealed cold frame keeps its f1 bytes verbatim; its anchor is the
//!   oldest record of the next-newer frame (depot SPEC chain walk).
//!
//! Dictionary scope is per (Wikipedia instance, lane), not per page.
//! Archive initialization samples newest page revisions in a read-only
//! prepass and publishes the dictionary before writing any f0 frame.
//! Importers that cannot make a prepass may explicitly finalize and
//! repack afterward. Later updates keep using the active dictionary
//! until an explicit retraining. f1/cold remain refPrefix frames and
//! never use the dictionary.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const LEVEL: i32 = 3;

/// Compress `raw`, optionally refPrefix-anchored on `prefix`.
pub(crate) fn compress(raw: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>> {
    wikimak_depot::compress_frame(raw, prefix, LEVEL)
        .map_err(|_| Error::Codec("zstd compress"))
}

/// Decompress a frame produced by [`compress`] with the same `prefix`.
pub(crate) fn decompress(frame: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>> {
    wikimak_depot::decompress_frame(frame, prefix)
        .map_err(|_| Error::Codec("zstd decompress"))
}

/// Durable dictionaries for one Wikipedia instance.
///
/// Files are immutable and named by `(lane, zstd dict_id)`. `persist`
/// fsyncs the bytes before publishing the name, then fsyncs the
/// directory, so a subsequently committed frame may safely refer to
/// the id. Reusing an id for different bytes is a loud error.
#[derive(Clone)]
pub(crate) struct DictionaryStore {
    root: PathBuf,
}

impl DictionaryStore {
    pub(crate) fn open(instance_root: &Path) -> Result<Self> {
        let root = instance_root.join("dictionaries");
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Read-side handle. The directory may legitimately be absent on a
    /// pre-training instance; opening it must not mutate the mirror.
    pub(crate) fn open_existing(instance_root: &Path) -> Self {
        Self { root: instance_root.join("dictionaries") }
    }

    pub(crate) fn persist(&self, lane: &str, bytes: &[u8]) -> Result<u32> {
        validate_lane(lane)?;
        let dict_id = dictionary_id(bytes)?;
        let final_path = self.path(lane, dict_id);
        match std::fs::read(&final_path) {
            Ok(existing) if existing == bytes => return Ok(dict_id),
            Ok(_) => {
                return Err(Error::DictionaryCollision {
                    lane: lane.to_owned(),
                    dict_id,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let (tmp, mut file) = self.create_temp(lane, dict_id, "bytes")?;
        if let Err(e) = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        })() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        match std::fs::rename(&tmp, &final_path) {
            Ok(()) => {}
            Err(_) if final_path.exists() => {
                let _ = std::fs::remove_file(&tmp);
                let existing = std::fs::read(&final_path)?;
                if existing != bytes {
                    return Err(Error::DictionaryCollision {
                        lane: lane.to_owned(),
                        dict_id,
                    });
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
        std::fs::File::open(&self.root)?.sync_all()?;
        Ok(dict_id)
    }

    pub(crate) fn load(&self, lane: &str, dict_id: u32) -> Result<Vec<u8>> {
        validate_lane(lane)?;
        match std::fs::read(self.path(lane, dict_id)) {
            Ok(bytes) if dictionary_id(&bytes)? == dict_id => Ok(bytes),
            Ok(_) => Err(Error::InvalidDictionary),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::MissingFrameDictionary {
                    lane: lane.to_owned(),
                    dict_id,
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Select the dictionary future f0 writes should use. The immutable
    /// dictionary is verified before the pointer is atomically
    /// published, so a crash cannot leave `current` naming absent bytes.
    pub(crate) fn activate(&self, lane: &str, dict_id: u32) -> Result<()> {
        self.load(lane, dict_id)?;
        let final_path = self.root.join(format!("{lane}.current"));
        let (tmp, mut file) = self.create_temp(lane, dict_id, "current")?;
        if let Err(e) = (|| -> std::io::Result<()> {
            writeln!(file, "{dict_id:08x}")?;
            file.sync_all()
        })() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, &final_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        std::fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn current(&self, lane: &str) -> Result<Option<u32>> {
        validate_lane(lane)?;
        let path = self.root.join(format!("{lane}.current"));
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let dict_id = u32::from_str_radix(text.trim(), 16)
            .map_err(|_| Error::FrameEnvelope("invalid current dictionary id"))?;
        self.load(lane, dict_id)?;
        Ok(Some(dict_id))
    }

    fn path(&self, lane: &str, dict_id: u32) -> PathBuf {
        self.root.join(format!("{lane}-{dict_id:08x}.zdict"))
    }

    fn create_temp(
        &self,
        lane: &str,
        dict_id: u32,
        purpose: &str,
    ) -> Result<(PathBuf, std::fs::File)> {
        (0..1000)
            .find_map(|attempt| {
                let path = self.root.join(format!(
                    ".{lane}-{dict_id:08x}.{purpose}.{}.{attempt}.tmp",
                    std::process::id()
                ));
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(file) => Some(Ok((path, file))),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .transpose()?
            .ok_or(Error::FrameEnvelope("too many stale dictionary temp files"))
    }
}

/// Train one per-instance dictionary from bounded head samples.
pub(crate) fn train_dictionary(samples: &[Vec<u8>], capacity: usize) -> Result<Vec<u8>> {
    if samples.is_empty() || capacity == 0 {
        return Err(Error::InvalidDictionary);
    }
    let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
    zstd::dict::from_samples(&refs, capacity).map_err(|_| Error::InvalidDictionary)
}

/// Provisional first-pass f0 without a trained dictionary.
pub(crate) fn compress_head_plain(raw: &[u8]) -> Result<Vec<u8>> {
    wikimak_depot::compress_frame(raw, None, LEVEL)
        .map_err(|_| Error::Codec("zstd compress"))
}

/// f0 using a persisted per-instance dictionary. Zstd writes the
/// dictionary's native id into its frame header; no side envelope is
/// needed.
pub(crate) fn compress_head_dictionary(raw: &[u8], dictionary: &[u8]) -> Result<Vec<u8>> {
    dictionary_id(dictionary)?;
    let mut cctx = zstd::zstd_safe::CCtx::create();
    cctx
        .set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(LEVEL))
        .map_err(|_| Error::Codec("zstd compression parameter"))?;
    cctx
        .load_dictionary(dictionary)
        .map_err(|_| Error::Codec("zstd dictionary load"))?;
    let mut zstd = Vec::with_capacity(zstd::zstd_safe::compress_bound(raw.len()));
    cctx.compress2(&mut zstd, raw)
        .map_err(|_| Error::Codec("zstd dictionary compress"))?;
    Ok(zstd)
}

/// Encode a head using the instance's active dictionary, or as a plain
/// provisional frame before seed finalization.
pub(crate) fn compress_head(raw: &[u8], dictionaries: &DictionaryStore) -> Result<Vec<u8>> {
    match dictionaries.current("revision")? {
        Some(dict_id) => {
            let dictionary = dictionaries.load("revision", dict_id)?;
            compress_head_dictionary(raw, &dictionary)
        }
        None => compress_head_plain(raw),
    }
}

/// Decode f0, resolving its native zstd dictionary id when present.
/// Dict id zero means a provisional plain head.
pub(crate) fn decompress_head(
    frame: &[u8],
    dictionaries: &DictionaryStore,
    lane: &str,
) -> Result<Vec<u8>> {
    let Some(dict_id) = zstd::zstd_safe::get_dict_id_from_frame(frame).map(u32::from) else {
        return wikimak_depot::decompress_frame(frame, None)
            .map_err(|_| Error::Codec("zstd decompress"));
    };
    let dictionary = dictionaries.load(lane, dict_id)?;
    let raw_len = zstd::zstd_safe::get_frame_content_size(frame)
        .map_err(|_| Error::Codec("zstd frame content size"))?
        .ok_or(Error::Codec("zstd frame without content size"))?
        as usize;
    let mut dctx = zstd::zstd_safe::DCtx::create();
    dctx.load_dictionary(&dictionary)
        .map_err(|_| Error::Codec("zstd dictionary load"))?;
    let mut raw = Vec::with_capacity(raw_len);
    dctx.decompress(&mut raw, frame)
        .map_err(|_| Error::Codec("zstd dictionary decompress"))?;
    Ok(raw)
}

/// f1/cold remain refPrefix frames. A nonzero native dict id here is
/// corruption or a tier mixup and is rejected before decode.
pub(crate) fn compress_history(raw: &[u8], prefix: &[u8]) -> Result<Vec<u8>> {
    wikimak_depot::compress_frame(raw, Some(prefix), LEVEL)
        .map_err(|_| Error::Codec("zstd compress"))
}

pub(crate) fn decompress_history(frame: &[u8], prefix: &[u8]) -> Result<Vec<u8>> {
    if zstd::zstd_safe::get_dict_id_from_frame(frame).is_some() {
        return Err(Error::FrameEnvelope("history frame carries a dictionary id"));
    }
    wikimak_depot::decompress_frame(frame, Some(prefix))
        .map_err(|_| Error::Codec("zstd decompress"))
}

pub(crate) fn frame_dictionary_id(frame: &[u8]) -> Option<u32> {
    zstd::zstd_safe::get_dict_id_from_frame(frame).map(u32::from)
}

fn dictionary_id(bytes: &[u8]) -> Result<u32> {
    zstd::zstd_safe::get_dict_id_from_dict(bytes)
        .map(u32::from)
        .ok_or(Error::InvalidDictionary)
}

fn validate_lane(lane: &str) -> Result<()> {
    if lane.is_empty()
        || !lane
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(Error::FrameEnvelope("invalid dictionary lane"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Vec<u8>> {
        (0..500)
            .map(|i| {
                format!(
                    "== Shared heading ==\nA representative current wiki page {} with common markup \
                     [[Target|label]] and {{{{template|value={}}}}}.",
                    i % 31,
                    i
                )
                .into_bytes()
            })
            .collect()
    }

    #[test]
    fn native_dictionary_identity_and_history_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = DictionaryStore::open(tmp.path()).unwrap();
        let samples = samples();
        let dictionary = train_dictionary(&samples, 4096).unwrap();
        let dict_id = store.persist("revision", &dictionary).unwrap();
        assert_eq!(store.persist("revision", &dictionary).unwrap(), dict_id);

        let head = &samples[17];
        let plain = compress_head_plain(head).unwrap();
        assert!(zstd::zstd_safe::get_dict_id_from_frame(&plain).is_none());
        assert_eq!(decompress_head(&plain, &store, "revision").unwrap(), *head);

        let encoded = compress_head_dictionary(head, &dictionary).unwrap();
        assert_eq!(
            zstd::zstd_safe::get_dict_id_from_frame(&encoded).map(u32::from),
            Some(dict_id)
        );
        assert_eq!(decompress_head(&encoded, &store, "revision").unwrap(), *head);

        let older = &samples[16];
        let prefixed = compress_history(older, head).unwrap();
        assert!(zstd::zstd_safe::get_dict_id_from_frame(&prefixed).is_none());
        assert_eq!(decompress_history(&prefixed, head).unwrap(), *older);
    }

    #[test]
    fn dictionary_store_is_durable_and_missing_is_loud() {
        let tmp = tempfile::TempDir::new().unwrap();
        let samples = samples();
        let dictionary = train_dictionary(&samples, 4096).unwrap();
        let id = {
            let store = DictionaryStore::open(tmp.path()).unwrap();
            let id = store.persist("revision", &dictionary).unwrap();
            store.activate("revision", id).unwrap();
            id
        };
        let reopened = DictionaryStore::open(tmp.path()).unwrap();
        assert_eq!(reopened.current("revision").unwrap(), Some(id));
        assert_eq!(reopened.current("comment").unwrap(), None);
        assert_eq!(reopened.load("revision", id).unwrap(), dictionary);
        assert!(matches!(
            reopened.load("comment", id),
            Err(Error::MissingFrameDictionary { dict_id, .. }) if dict_id == id
        ));
    }

    #[test]
    fn dictionary_id_collision_is_loud() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = DictionaryStore::open(tmp.path()).unwrap();
        let dictionary = train_dictionary(&samples(), 4096).unwrap();
        let id = store.persist("revision", &dictionary).unwrap();
        let mut different = dictionary;
        let last = different.len() - 1;
        different[last] ^= 1;
        assert_eq!(dictionary_id(&different).unwrap(), id);
        assert!(matches!(
            store.persist("revision", &different),
            Err(Error::DictionaryCollision { dict_id, .. }) if dict_id == id
        ));
    }

    #[test]
    fn dictionary_frame_is_rejected_in_history_context() {
        let dictionary = train_dictionary(&samples(), 4096).unwrap();
        let head = compress_head_dictionary(b"head", &dictionary).unwrap();
        assert!(matches!(
            decompress_history(&head, b"prefix"),
            Err(Error::FrameEnvelope("history frame carries a dictionary id"))
        ));
    }
}
