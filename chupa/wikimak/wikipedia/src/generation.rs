//! Stable identity and publication boundary for a servable Wikipedia generation.
//!
//! A generation ID is assigned from the small immutable plan that constructs
//! the generation.  Validation is structural: it checks the archive, its
//! ordered segment inventory, compression-reference ID, and title index
//! without rereading archive payloads merely to checksum immutable revisions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;
use thiserror::Error;

const GENERATION_ID_BYTES: usize = 32;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct GenerationId(String);

impl GenerationId {
    /// Assign an ID from a canonical, immutable construction-plan encoding.
    pub fn from_plan_bytes(plan: &[u8]) -> Self {
        Self(hex::encode(sha2::Sha256::digest(plan)))
    }

    pub fn from_plan_id(plan_id: &str) -> Self {
        let mut input = b"wikipedia-full-generation\0".to_vec();
        input.extend_from_slice(plan_id.as_bytes());
        Self::from_plan_bytes(&input)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: &str) -> Result<Self> {
        let id = Self(value.to_owned());
        let _ = id.to_bytes()?;
        Ok(id)
    }

    pub(crate) fn to_bytes(&self) -> Result<[u8; GENERATION_ID_BYTES]> {
        let decoded = hex::decode(&self.0).map_err(|_| GenerationError::MalformedId)?;
        decoded
            .try_into()
            .map_err(|_| GenerationError::MalformedId)
    }

    pub(crate) fn from_bytes(bytes: [u8; GENERATION_ID_BYTES]) -> Self {
        Self(hex::encode(bytes))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CompressionReferenceIdentity {
    Dictionary {
        dictionary_id: u32,
        raw_bytes: u64,
        compressed_bytes: u64,
    },
    RefPrefix {
        xxh3_64: u64,
        raw_bytes: u64,
        compressed_bytes: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenerationSegmentIdentity {
    pub role: u8,
    pub first_id: u64,
    pub last_id: u64,
    pub virtual_start: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenerationIdentity {
    pub generation_id: GenerationId,
    pub wiki_db: String,
    pub content_frontier: String,
    pub metadata_frontier: String,
    pub compression_reference: CompressionReferenceIdentity,
    pub segments: Vec<GenerationSegmentIdentity>,
}

#[derive(Debug, Error)]
pub enum GenerationError {
    #[error("io at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("archive: {0}")]
    Archive(#[from] crate::archive::ArchiveError),
    #[error("generation contains no manifest")]
    MissingManifest,
    #[error("generation contains manifests for more than one wiki")]
    MixedWiki,
    #[error("malformed generation ID")]
    MalformedId,
    #[error("generation identity mismatch: expected {expected}, observed {observed}")]
    IdentityMismatch { expected: String, observed: String },
}

pub type Result<T> = std::result::Result<T, GenerationError>;

fn io(path: &Path, source: std::io::Error) -> GenerationError {
    GenerationError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn generation_frontiers(
    archive: &Path,
    titles: &crate::title_index::TitleIndex,
) -> Result<(String, String, String)> {
    use crate::archive::{EntityKind, Record};

    let indexed = crate::archive::IndexedArchiveSet::open(archive, titles)?;
    let mut wiki_db = None;
    let mut content_frontier = None;
    let mut metadata_frontier = None;
    let mut left = 0;
    let mut right = titles.frame_count();
    while left < right {
        let middle = left + (right - left) / 2;
        if titles.frame(middle)?.info.first_entity.kind < EntityKind::Global {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    for position in left..titles.frame_count() {
        let frame = titles.frame(position)?;
        if frame.info.first_entity.kind != EntityKind::Global {
            continue;
        }
        let location = indexed.location(frame)?;
        let mut input = indexed.open_file(&location)?;
        crate::archive::visit_frame_while_file(&mut input, &location, |record| {
            if let Record::Manifest { manifest, .. } = record {
                if wiki_db
                    .as_ref()
                    .is_some_and(|current| current != &manifest.wiki_db)
                {
                    return Err(crate::archive::ArchiveError::Invalid(
                        "archive contains manifests for more than one wiki",
                    ));
                }
                wiki_db = Some(manifest.wiki_db);
                content_frontier = Some(
                    content_frontier
                        .take()
                        .map_or(manifest.content_snapshot.clone(), |current: String| {
                            current.max(manifest.content_snapshot)
                        }),
                );
                metadata_frontier = Some(
                    metadata_frontier
                        .take()
                        .map_or(manifest.metadata_snapshot.clone(), |current: String| {
                            current.max(manifest.metadata_snapshot)
                        }),
                );
            }
            Ok(true)
        })?;
    }
    match (wiki_db, content_frontier, metadata_frontier) {
        (Some(wiki), Some(content), Some(metadata)) => Ok((wiki, content, metadata)),
        _ => Err(GenerationError::MissingManifest),
    }
}

/// Validate an archive/index pair and read its construction-assigned identity.
pub fn generation_identity(
    archive: impl AsRef<Path>,
    title_index: impl AsRef<Path>,
) -> Result<GenerationIdentity> {
    let archive = archive.as_ref();
    let title_index = title_index.as_ref();
    let titles = crate::title_index::TitleIndex::open(title_index)?;
    crate::archive::IndexedArchiveSet::open(archive, &titles)?;
    let (wiki_db, content_frontier, metadata_frontier) =
        generation_frontiers(archive, &titles)?;
    let compression_reference =
        crate::archive::archive_compression_reference_identity(archive)?;
    let segments = (0..titles.segment_count())
        .map(|position| {
            titles
                .segment(position)
                .map(|segment| GenerationSegmentIdentity {
                    role: segment.role,
                    first_id: segment.first_id,
                    last_id: segment.last_id,
                    virtual_start: segment.virtual_start,
                    bytes: segment.bytes,
                })
        })
        .collect::<crate::archive::Result<Vec<_>>>()?;
    Ok(GenerationIdentity {
        generation_id: titles.generation_id().clone(),
        wiki_db,
        content_frontier,
        metadata_frontier,
        compression_reference,
        segments,
    })
}

/// Validate the pair and require it to be the named generation.
pub fn validate_generation(
    archive: impl AsRef<Path>,
    title_index: impl AsRef<Path>,
    expected: &GenerationIdentity,
) -> Result<GenerationIdentity> {
    let observed = generation_identity(archive, title_index)?;
    if observed != *expected {
        return Err(GenerationError::IdentityMismatch {
            expected: expected.generation_id.as_str().to_owned(),
            observed: observed.generation_id.as_str().to_owned(),
        });
    }
    Ok(observed)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| io(path, error))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Atomically publish a fully built index, then sync its directory.
///
/// The prepared index must already be in the installed index's destination
/// filesystem. The archive must already be at its final location. Renaming
/// this index is the generation switch observed by new readers.
pub fn publish_index_atomically(
    prepared_index: impl AsRef<Path>,
    installed_index: impl AsRef<Path>,
) -> Result<()> {
    let prepared_index = prepared_index.as_ref();
    let installed_index = installed_index.as_ref();
    let parent = installed_index
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    std::fs::File::open(prepared_index)
        .map_err(|error| io(prepared_index, error))?
        .sync_all()
        .map_err(|error| io(prepared_index, error))?;
    std::fs::rename(prepared_index, installed_index)
        .map_err(|error| io(installed_index, error))?;
    sync_directory(parent)
}
