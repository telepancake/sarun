//! Publication of immutable, generation-addressed Wikipedia archives.
//!
//! The stable title index is the sole selector. Its embedded generation ID
//! names one immutable archive directory. Publication therefore has one
//! visibility boundary: atomically replacing the selector.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const LEGACY_INSTALL_SCHEMA: u32 = 1;
const LEGACY_INSTALL_SCHEMA_V2: u32 = 2;
const INSTALL_SCHEMA: u32 = 3;
const LEGACY_GENERATION_MANIFEST_SCHEMA: u32 = 2;
const GENERATION_MANIFEST_SCHEMA: u32 = 3;
const UPDATE_MAINTENANCE_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PublishReceipt {
    schema: u32,
    publication_id: String,
    candidate_generation_id: String,
    selected_before_publish: Option<String>,
    cleanup_generation_ids: Vec<String>,
    #[serde(default)]
    candidate_cleanup: Option<CandidateCleanupReceipt>,
    /// Metadata-only ownership captured before any destination-local candidate
    /// links are created. This closes the crash window between staging and the
    /// publication receipt without reading archive payloads.
    #[serde(default)]
    candidate_manifest: Option<GenerationManifest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CandidateCleanupReceipt {
    archive: PathBuf,
    title: PathBuf,
    title_identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstallOutcome {
    pub(crate) cleanup_pending: bool,
    pub(crate) candidate_cleanup_pending: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ServingPair {
    pub(crate) archive: PathBuf,
    pub(crate) title: PathBuf,
    /// The lease is acquired before the maintenance-marker check. Keeping it
    /// in the pair closes the check/open race: a maintenance writer cannot
    /// acquire its exclusive generation lease after this pair has crossed the
    /// marker check.
    _generation_lease: std::sync::Arc<crate::archive::ArchiveSharedLease>,
}

impl PartialEq for ServingPair {
    fn eq(&self, other: &Self) -> bool {
        self.archive == other.archive && self.title == other.title
    }
}

impl Eq for ServingPair {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct UpdateMaintenanceMarker {
    schema: u32,
    base_generation_id: String,
    new_generation_id: String,
    update_id: String,
}

/// A destination-local maintenance exclusion held across an update's
/// selector publication. Dropping this guard releases the kernel lease but
/// deliberately leaves the durable marker in place for crash recovery.
#[derive(Debug)]
pub(crate) struct UpdateMaintenanceGuard {
    destination: PathBuf,
    marker: UpdateMaintenanceMarker,
    _lease: crate::archive::ArchiveCleanupLease,
}

/// The begin operation distinguishes active readers from invalid durable
/// state so callers can retry without interpreting a safety failure as a
/// transient resource condition.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum UpdateMaintenanceError {
    Invalid(String),
    ReadersActive {
        generation_id: String,
        marker: PathBuf,
    },
    Io(String),
}

impl std::fmt::Display for UpdateMaintenanceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid update maintenance state: {message}"),
            Self::ReadersActive {
                generation_id,
                marker,
            } => write!(
                formatter,
                "update maintenance for generation {generation_id} is retryable because active readers hold the generation lease; marker remains at {}",
                marker.display()
            ),
            Self::Io(message) => write!(formatter, "update maintenance I/O failure: {message}"),
        }
    }
}

impl std::error::Error for UpdateMaintenanceError {}

/// Abstract reader/writer exclusion state for an incremental update.
///
/// This is deliberately separate from `UpdatePhase`: publishing the durable
/// marker may precede acquisition of the exclusive generation lease while
/// existing readers drain, and a crash releases the live lease without
/// removing that marker. New readers are admitted only in `Available`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateMaintenancePhase {
    Available,
    MarkerPublished,
    WriterExclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateMaintenanceEvent {
    PublishMarker,
    ExistingReaderObserved,
    AcquireWriter,
    OpenNewReader,
    ProcessCrashed,
    FinishAfterCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateMaintenanceRejection {
    MarkerRequired,
    MaintenanceActive,
    CommitRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateMaintenanceImpossibility {
    ReaderWhileWriterExclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateMaintenanceDecision {
    Advance(UpdateMaintenancePhase),
    NoOp,
    Reject(UpdateMaintenanceRejection),
    Impossible(UpdateMaintenanceImpossibility),
}

/// Pure maintenance transition relation. Identity equality, reader count, and
/// the committed selector are guards supplied by the concrete operations;
/// this table defines the state effect once those guards hold.
fn update_maintenance_transition(
    phase: UpdateMaintenancePhase,
    event: UpdateMaintenanceEvent,
) -> UpdateMaintenanceDecision {
    use UpdateMaintenanceDecision::{Advance, Impossible, NoOp, Reject};
    use UpdateMaintenanceEvent::*;
    use UpdateMaintenancePhase::*;

    match (phase, event) {
        (Available, PublishMarker) => Advance(MarkerPublished),
        (MarkerPublished | WriterExclusive, PublishMarker) => NoOp,
        (Available | MarkerPublished, ExistingReaderObserved) => NoOp,
        (WriterExclusive, ExistingReaderObserved) => Impossible(
            UpdateMaintenanceImpossibility::ReaderWhileWriterExclusive,
        ),
        (Available, AcquireWriter) => Reject(UpdateMaintenanceRejection::MarkerRequired),
        (MarkerPublished, AcquireWriter) => Advance(WriterExclusive),
        (WriterExclusive, AcquireWriter) => NoOp,
        (Available, OpenNewReader) => NoOp,
        (MarkerPublished | WriterExclusive, OpenNewReader) => {
            Reject(UpdateMaintenanceRejection::MaintenanceActive)
        }
        (Available | MarkerPublished, ProcessCrashed) => NoOp,
        (WriterExclusive, ProcessCrashed) => Advance(MarkerPublished),
        (WriterExclusive, FinishAfterCommit) => Advance(Available),
        (Available | MarkerPublished, FinishAfterCommit) => {
            Reject(UpdateMaintenanceRejection::CommitRequired)
        }
    }
}

/// Result of reclaiming one unselected installed generation. The caller owns
/// the writer/reader exclusion protocol; this operation only uses the durable
/// manifest and no-replace claims to decide what it may unlink.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GenerationCleanupReport {
    pub reclaimed_segments: u64,
    pub reclaimed_bytes: u64,
    /// Paths which prevented the generation directory from becoming empty.
    /// They are left in place for inspection and retry; normal cleanup never
    /// moves them into a private quarantine.
    pub pending_paths: Vec<PathBuf>,
    /// Kept for the engine's explicit-deletion compatibility surface. Normal
    /// installation cleanup no longer populates this field. The engine must
    /// handle `pending_paths` and its own explicit quarantine policy separately.
    pub quarantined_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct GenerationManifest {
    schema: u32,
    generation_id: String,
    segments: Vec<GenerationManifestSegment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GenerationManifestState {
    Current(GenerationManifest),
    MetadataMigration { schema: u32 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct GenerationManifestSegment {
    name: String,
    bytes: u64,
    identity: FileIdentity,
}

/// Platform file identity captured with an ownership receipt. Device/inode,
/// modification time, and length form the fail-closed replacement
/// check. Keeping this identity avoids rereading unchanged multi-gigabyte
/// segments on every reopen/publication and remains stable when an unrelated
/// hard link is removed after publication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileIdentity {
    device: u64,
    inode: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    bytes: u64,
}

pub(crate) fn generation_root(destination: &Path) -> PathBuf {
    destination.with_extension("generations")
}

fn selector_path(destination: &Path) -> PathBuf {
    destination.with_extension("swtitle")
}

fn valid_generation_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn receipt_path(destination: &Path) -> PathBuf {
    destination.with_extension("install.json")
}

fn generation_path(destination: &Path, generation_id: &str) -> Result<PathBuf, String> {
    if !valid_generation_id(generation_id) {
        return Err(format!("invalid generation ID {generation_id:?}"));
    }
    Ok(generation_root(destination).join(generation_id))
}

fn pending_selector_path(destination: &Path, generation_id: &str) -> Result<PathBuf, String> {
    Ok(generation_root(destination).join(format!("{generation_id}.swtitle.pending")))
}

fn candidate_archive_pending_path(destination: &Path, generation_id: &str) -> Result<PathBuf, String> {
    generation_path(destination, generation_id)?;
    Ok(generation_root(destination).join(format!(
        ".candidate-archive-{generation_id}.pending"
    )))
}

fn candidate_title_pending_path(destination: &Path, generation_id: &str) -> Result<PathBuf, String> {
    generation_path(destination, generation_id)?;
    Ok(generation_root(destination).join(format!(
        ".candidate-title-{generation_id}.pending"
    )))
}

fn candidate_generation_pending_path(
    destination: &Path,
    generation_id: &str,
) -> Result<PathBuf, String> {
    generation_path(destination, generation_id)?;
    Ok(generation_root(destination).join(format!(".generation-{generation_id}.pending")))
}

fn generation_manifest_path(destination: &Path, generation_id: &str) -> Result<PathBuf, String> {
    generation_path(destination, generation_id)
        .map(|_| generation_root(destination).join(format!("{generation_id}.manifest.json")))
}

fn path_exists(path: &Path) -> Result<bool, String> {
    path.try_exists()
        .map_err(|error| format!("inspect {}: {error}", path.display()))
}

fn namespace_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect {} without following links: {error}", path.display())),
    }
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    Ok(())
}

fn file_identity(metadata: &std::fs::Metadata, path: &Path) -> Result<FileIdentity, String> {
    if !metadata.file_type().is_file() {
        return Err(format!("owned file {} is not regular", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
            bytes: metadata.len(),
        });
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(format!(
            "cannot establish fail-closed device/inode identity for {} on this platform",
            path.display()
        ))
    }
}

fn open_regular_file(path: &Path) -> Result<(std::fs::File, FileIdentity), String> {
    #[cfg(not(unix))]
    return Err(format!(
        "cannot open owned file {} without a no-follow primitive",
        path.display()
    ));

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| format!("open owned file {} without following links: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect owned file {}: {error}", path.display()))?;
    let identity = file_identity(&metadata, path)?;
    Ok((file, identity))
}

fn inspect_file_identity(path: &Path) -> Result<FileIdentity, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect owned file {} without following links: {error}", path.display()))?;
    file_identity(&metadata, path)
}

fn same_regular_file(left: &FileIdentity, right: &FileIdentity) -> bool {
    left.device == right.device && left.inode == right.inode && left.bytes == right.bytes
}

fn validate_generation_manifest(
    manifest: &GenerationManifest,
    generation_id: &str,
    path: &Path,
) -> Result<(), String> {
    if !matches!(
        manifest.schema,
        LEGACY_GENERATION_MANIFEST_SCHEMA | GENERATION_MANIFEST_SCHEMA
    )
        || manifest.generation_id != generation_id
        || manifest.segments.is_empty()
    {
        return Err(format!("{} has an invalid generation manifest", path.display()));
    }
    let mut previous = None;
    for segment in &manifest.segments {
        let segment_path = Path::new(&segment.name);
        if segment.name.is_empty()
            || segment.name != segment_path.to_string_lossy()
            || segment_path.file_name().and_then(|name| name.to_str()) != Some(segment.name.as_str())
            || !segment.name.ends_with(crate::archive_set::PART_SUFFIX)
            || segment.bytes == 0
            || segment.identity.bytes != segment.bytes
            || previous.is_some_and(|name: &str| name >= segment.name.as_str())
        {
            return Err(format!("{} has an invalid generation segment", path.display()));
        }
        previous = Some(segment.name.as_str());
    }
    Ok(())
}

fn generation_manifest_from_segments(
    generation_id: &str,
    generation: &Path,
    segments: &[crate::archive_set::ArchiveSetSegment],
) -> Result<GenerationManifest, String> {
    let mut owned = Vec::with_capacity(segments.len());
    for segment in segments {
        let identity = inspect_file_identity(&generation.join(&segment.name))?;
        if identity.bytes != segment.bytes {
            return Err(format!(
                "owned file {} has length {}, expected {}",
                generation.join(&segment.name).display(),
                identity.bytes,
                segment.bytes
            ));
        }
        owned.push(GenerationManifestSegment {
            name: segment.name.clone(),
            bytes: identity.bytes,
            identity,
        });
    }
    let manifest = GenerationManifest {
        schema: GENERATION_MANIFEST_SCHEMA,
        generation_id: generation_id.to_owned(),
        segments: owned,
    };
    validate_generation_manifest(&manifest, generation_id, generation)?;
    Ok(manifest)
}

fn read_generation_manifest_state(
    destination: &Path,
    generation_id: &str,
) -> Result<Option<GenerationManifestState>, String> {
    let path = generation_manifest_path(destination, generation_id)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(format!("generation manifest {} is not a regular file", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut file = open_regular_file(&path)?.0;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let schema = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get("schema").and_then(serde_json::Value::as_u64));
    if schema == Some(1) {
        return Ok(Some(GenerationManifestState::MetadataMigration {
            schema: 1,
        }));
    }
    if schema == Some(LEGACY_GENERATION_MANIFEST_SCHEMA as u64) {
        // Schema 2 exists in two forms. Identity-bearing manifests are already
        // sufficient ownership evidence: `digest` and legacy `birth_*` fields
        // are ignored while the stable current identity fields are retained.
        // Digest-only manifests cannot establish ownership after displacement;
        // only a still-selected generation may recapture their metadata.
        let Ok(mut manifest) = serde_json::from_slice::<GenerationManifest>(&bytes) else {
            return Ok(Some(GenerationManifestState::MetadataMigration {
                schema: LEGACY_GENERATION_MANIFEST_SCHEMA,
            }));
        };
        validate_generation_manifest(&manifest, generation_id, &path)?;
        manifest.schema = GENERATION_MANIFEST_SCHEMA;
        return Ok(Some(GenerationManifestState::Current(manifest)));
    }
    let manifest: GenerationManifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode {}: {error}", path.display()))?;
    validate_generation_manifest(&manifest, generation_id, &path)?;
    Ok(Some(GenerationManifestState::Current(manifest)))
}

fn read_generation_manifest(
    destination: &Path,
    generation_id: &str,
) -> Result<Option<GenerationManifest>, String> {
    match read_generation_manifest_state(destination, generation_id)? {
        Some(GenerationManifestState::Current(manifest)) => Ok(Some(manifest)),
        Some(GenerationManifestState::MetadataMigration { .. }) | None => Ok(None),
    }
}

fn persist_generation_manifest(
    destination: &Path,
    generation_id: &str,
    manifest: &GenerationManifest,
) -> Result<(), String> {
    let root = generation_root(destination);
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create {}: {error}", root.display()))?;
    let path = generation_manifest_path(destination, generation_id)?;
    validate_generation_manifest(manifest, generation_id, &path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&root)
        .map_err(|error| format!("stage {}: {error}", path.display()))?;
    serde_json::to_writer(temporary.as_file_mut(), manifest)
        .map_err(|error| format!("encode {}: {error}", path.display()))?;
    temporary
        .write_all(b"\n")
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    temporary
        .persist(&path)
        .map_err(|error| format!("publish {}: {}", path.display(), error.error))?;
    sync_directory(&root)
}

fn ensure_generation_manifest(
    destination: &Path,
    generation_id: &str,
    generation: &Path,
) -> Result<(), String> {
    match read_generation_manifest_state(destination, generation_id)? {
        Some(GenerationManifestState::Current(found)) => {
            let reader = crate::archive_set::ArchiveSetReader::open(generation)
                .map_err(|error| format!("validate generation {}: {error}", generation.display()))?;
            if found.segments.len() != reader.segments().len() {
                return Err(format!(
                    "generation manifest disagrees with {}",
                    generation.display()
                ));
            }
            for (owned, actual) in found.segments.iter().zip(reader.segments()) {
                if owned.name != actual.name
                    || owned.bytes != actual.bytes
                    || inspect_file_identity(&generation.join(&owned.name))? != owned.identity
                {
                    return Err(format!(
                        "generation manifest disagrees with {}",
                        generation.display()
                    ));
                }
            }
            Ok(())
        }
        Some(GenerationManifestState::MetadataMigration { .. }) | None => {
            let reader = crate::archive_set::ArchiveSetReader::open_with_residue(generation)
                .map_err(|error| format!("validate generation {}: {error}", generation.display()))?;
            let expected = generation_manifest_from_segments(generation_id, generation, reader.segments())?;
            let expected = GenerationManifest {
                schema: GENERATION_MANIFEST_SCHEMA,
                ..expected
            };
            persist_generation_manifest(destination, generation_id, &expected)
        }
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn selected_generation(destination: &Path) -> Result<Option<String>, String> {
    let selector = selector_path(destination);
    if !path_exists(&selector)? {
        return Ok(None);
    }
    crate::title_index::TitleIndex::open(&selector)
        .map(|titles| Some(titles.generation_id().as_str().to_owned()))
        .map_err(|error| format!("open selector {}: {error}", selector.display()))
}

fn update_maintenance_marker_path(destination: &Path) -> PathBuf {
    destination.with_extension("maintenance.json")
}

fn validate_update_maintenance_marker(
    marker: &UpdateMaintenanceMarker,
    path: &Path,
) -> Result<(), String> {
    if marker.schema != UPDATE_MAINTENANCE_SCHEMA
        || !valid_generation_id(&marker.base_generation_id)
        || !valid_generation_id(&marker.new_generation_id)
        || marker.base_generation_id == marker.new_generation_id
        || marker.update_id.is_empty()
        || marker.update_id.bytes().any(|byte| byte == 0)
    {
        return Err(format!("{} has an invalid update-maintenance marker", path.display()));
    }
    Ok(())
}

fn read_update_maintenance_marker(
    destination: &Path,
) -> Result<Option<UpdateMaintenanceMarker>, String> {
    let path = update_maintenance_marker_path(destination);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(format!(
                "update-maintenance marker {} is not a regular file",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    }
    let file = open_regular_file(&path)?.0;
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() > 16 * 1024 {
        return Err(format!("{} is too large to be an update-maintenance marker", path.display()));
    }
    let marker: UpdateMaintenanceMarker = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode {}: {error}", path.display()))?;
    validate_update_maintenance_marker(&marker, &path)?;
    Ok(Some(marker))
}

/// Publish a complete marker under a no-replace name. A concurrent creator
/// reports `Ok(false)` and the caller must inspect the now-authoritative
/// marker rather than replacing it.
fn publish_update_maintenance_marker(
    destination: &Path,
    marker: &UpdateMaintenanceMarker,
) -> Result<bool, String> {
    let path = update_maintenance_marker_path(destination);
    validate_update_maintenance_marker(marker, &path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent(&path))
        .map_err(|error| format!("stage {}: {error}", path.display()))?;
    serde_json::to_writer(temporary.as_file_mut(), marker)
        .map_err(|error| format!("encode {}: {error}", path.display()))?;
    temporary
        .write_all(b"\n")
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    match crate::instance::rename_without_replacing(temporary.path(), &path) {
        Ok(()) => {
            sync_directory(parent(&path))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!("publish {}: {error}", path.display())),
    }
}

fn ensure_no_update_maintenance(destination: &Path) -> Result<(), String> {
    if let Some(marker) = read_update_maintenance_marker(destination)? {
        return Err(format!(
            "serving is unavailable while update {} maintains generation {} -> {} (marker {})",
            marker.update_id,
            marker.base_generation_id,
            marker.new_generation_id,
            update_maintenance_marker_path(destination).display()
        ));
    }
    Ok(())
}

pub(crate) fn update_maintenance_active(destination: &Path) -> Result<bool, String> {
    read_update_maintenance_marker(destination).map(|marker| marker.is_some())
}

/// Begin or resume destination-local update maintenance.
///
/// The marker is published and directory-synced before the nonblocking
/// exclusive lease attempt. `ReadersActive` is therefore retryable, while the
/// marker remains durable and prevents new serving opens. A guard drop only
/// releases the kernel lease; the marker is removed solely by `finish` after
/// the exact new generation is selected.
pub(crate) fn begin_update_maintenance(
    destination: &Path,
    base_generation_id: &str,
    new_generation_id: &str,
    update_id: &str,
) -> Result<UpdateMaintenanceGuard, UpdateMaintenanceError> {
    let marker_path = update_maintenance_marker_path(destination);
    let requested = UpdateMaintenanceMarker {
        schema: UPDATE_MAINTENANCE_SCHEMA,
        base_generation_id: base_generation_id.to_owned(),
        new_generation_id: new_generation_id.to_owned(),
        update_id: update_id.to_owned(),
    };
    validate_update_maintenance_marker(&requested, &marker_path)
        .map_err(UpdateMaintenanceError::Invalid)?;

    let selected = selected_generation(destination).map_err(UpdateMaintenanceError::Io)?;
    let base_generation = generation_path(destination, base_generation_id)
        .map_err(UpdateMaintenanceError::Invalid)?;
    match std::fs::symlink_metadata(&base_generation) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(UpdateMaintenanceError::Invalid(format!(
                "base generation {} is not a directory",
                base_generation.display()
            )));
        }
        Err(error) => {
            return Err(UpdateMaintenanceError::Io(format!(
                "inspect base generation {}: {error}",
                base_generation.display()
            )));
        }
    }

    let marker = match read_update_maintenance_marker(destination)
        .map_err(UpdateMaintenanceError::Invalid)?
    {
        Some(existing) if existing == requested => {
            if !matches!(
                selected.as_deref(),
                Some(found) if found == base_generation_id || found == new_generation_id
            ) {
                return Err(UpdateMaintenanceError::Invalid(format!(
                    "selector names {:?}, expected maintenance generation {} or {}",
                    selected, base_generation_id, new_generation_id
                )));
            }
            existing
        }
        Some(existing) => {
            return Err(UpdateMaintenanceError::Invalid(format!(
                "{} names update {} generation {} -> {}, not requested update {} generation {} -> {}",
                marker_path.display(),
                existing.update_id,
                existing.base_generation_id,
                existing.new_generation_id,
                requested.update_id,
                requested.base_generation_id,
                requested.new_generation_id
            )));
        }
        None => {
            if selected.as_deref() != Some(base_generation_id) {
                return Err(UpdateMaintenanceError::Invalid(format!(
                    "selector names {:?}, expected base generation {} before maintenance begins",
                    selected, base_generation_id
                )));
            }
            ensure_generation_manifest(destination, base_generation_id, &base_generation)
                .map_err(UpdateMaintenanceError::Invalid)?;
            if publish_update_maintenance_marker(destination, &requested)
                .map_err(UpdateMaintenanceError::Io)?
            {
                requested.clone()
            } else {
                match read_update_maintenance_marker(destination)
                    .map_err(UpdateMaintenanceError::Invalid)?
                {
                    Some(existing) if existing == requested => existing,
                    Some(existing) => {
                        return Err(UpdateMaintenanceError::Invalid(format!(
                            "{} was concurrently published for update {} generation {} -> {}",
                            marker_path.display(),
                            existing.update_id,
                            existing.base_generation_id,
                            existing.new_generation_id
                        )));
                    }
                    None => {
                        return Err(UpdateMaintenanceError::Io(format!(
                            "{} disappeared while beginning maintenance",
                            marker_path.display()
                        )));
                    }
                }
            }
        }
    };

    let lease = match crate::archive::try_acquire_archive_cleanup_lease(&base_generation)
        .map_err(|error| UpdateMaintenanceError::Io(error.to_string()))?
    {
        Some(lease) => lease,
        None => {
            return Err(UpdateMaintenanceError::ReadersActive {
                generation_id: base_generation_id.to_owned(),
                marker: marker_path,
            });
        }
    };

    let selected_after_lease = selected_generation(destination)
        .map_err(UpdateMaintenanceError::Io)?;
    if selected_after_lease != selected {
        return Err(UpdateMaintenanceError::Invalid(format!(
            "selector changed from {:?} to {:?} while beginning maintenance for base generation {}",
            selected, selected_after_lease, base_generation_id
        )));
    }

    Ok(UpdateMaintenanceGuard {
        destination: destination.to_path_buf(),
        marker,
        _lease: lease,
    })
}

impl UpdateMaintenanceGuard {
    /// Atomically replace one hard-linked preserved-base segment, then retire
    /// the exact old installed link while serving remains closed.
    ///
    /// The replacement is first linked beside the preserved base, so the
    /// subsequent rename is a same-directory atomic swap. After that directory
    /// is durable, the old installed segment is unlinked only if its complete
    /// manifest identity still matches. Replaying after a crash accepts the
    /// already-swapped replacement and completes a pending installed unlink.
    pub(crate) fn replace_preserved_segment(
        &self,
        preserved: &Path,
        segment_name: &str,
        replacement: &Path,
        replacement_name: &str,
    ) -> Result<bool, String> {
        if selected_generation(&self.destination)?.as_deref()
            != Some(&self.marker.base_generation_id)
        {
            return Err(format!(
                "cannot replace base segment {segment_name}: selector no longer names generation {}",
                self.marker.base_generation_id
            ));
        }
        let marker_path = update_maintenance_marker_path(&self.destination);
        match read_update_maintenance_marker(&self.destination)? {
            Some(found) if found == self.marker => {}
            Some(_) => return Err(format!("{} no longer names this update", marker_path.display())),
            None => return Err(format!("{} disappeared during maintenance", marker_path.display())),
        }
        if preserved.file_name().and_then(|name| name.to_str()) != Some(segment_name) {
            return Err(format!(
                "preserved segment path {} does not end in {segment_name}",
                preserved.display()
            ));
        }
        if Path::new(replacement_name)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(replacement_name)
        {
            return Err(format!("replacement segment name {replacement_name:?} is not canonical"));
        }
        let generation = generation_path(&self.destination, &self.marker.base_generation_id)?;
        let manifest = read_generation_manifest(
            &self.destination,
            &self.marker.base_generation_id,
        )?
        .ok_or_else(|| format!("base generation {} has no ownership manifest", generation.display()))?;
        let owned = manifest
            .segments
            .iter()
            .find(|segment| segment.name == segment_name)
            .ok_or_else(|| {
                format!(
                    "base generation manifest has no segment named {segment_name}"
                )
            })?;
        let replacement_identity = inspect_file_identity(replacement)?;
        let preserved_parent = parent(preserved);
        let selected_replacement = preserved_parent.join(replacement_name);
        let replacement_is_selected = match std::fs::symlink_metadata(&selected_replacement) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let found = inspect_file_identity(&selected_replacement)?;
                if same_regular_file(&found, &replacement_identity) {
                    true
                } else if selected_replacement == preserved && found == owned.identity {
                    false
                } else {
                    return Err(format!(
                        "preserved replacement path {} names an unexpected file",
                        selected_replacement.display()
                    ));
                }
            }
            Ok(_) => {
                return Err(format!(
                    "preserved replacement path {} is not a regular file",
                    selected_replacement.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(format!("inspect {}: {error}", selected_replacement.display()));
            }
        };
        if !replacement_is_selected {
            match std::fs::symlink_metadata(preserved) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    if inspect_file_identity(preserved)? != owned.identity {
                        return Err(format!(
                            "preserved base segment {} changed from its manifest identity",
                            preserved.display()
                        ));
                    }
                }
                Ok(_) => {
                    return Err(format!(
                        "preserved base segment {} is not a regular file",
                        preserved.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(format!(
                        "preserved base segment {} disappeared before its replacement was durable",
                        preserved.display()
                    ));
                }
                Err(error) => return Err(format!("inspect {}: {error}", preserved.display())),
            }
            let pending_id = hex::encode(sha2::Sha256::digest(
                serde_json::to_vec(&(
                    "wikipedia-hdd-segment-replacement",
                    &self.marker.update_id,
                    segment_name,
                    replacement_name,
                ))
                .expect("maintenance segment identity is serializable"),
            ));
            let pending = preserved_parent.join(format!(".maintenance-{pending_id}.pending"));
            match std::fs::symlink_metadata(&pending) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    let found = inspect_file_identity(&pending)?;
                    if !same_regular_file(&found, &replacement_identity) {
                        return Err(format!(
                            "pending replacement {} does not name {}",
                            pending.display(),
                            replacement.display()
                        ));
                    }
                }
                Ok(_) => {
                    return Err(format!(
                        "pending replacement {} is not a regular file",
                        pending.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::hard_link(replacement, &pending).map_err(|error| {
                        format!(
                            "stage replacement {} at {}: {error}",
                            replacement.display(),
                            pending.display()
                        )
                    })?;
                    sync_directory(preserved_parent)?;
                }
                Err(error) => return Err(format!("inspect {}: {error}", pending.display())),
            }
            if selected_replacement == preserved {
                std::fs::rename(&pending, preserved).map_err(|error| {
                    format!(
                        "replace preserved segment {} from {}: {error}",
                        preserved.display(),
                        replacement.display()
                    )
                })?;
            } else {
                crate::instance::rename_without_replacing(&pending, &selected_replacement)
                    .map_err(|error| {
                        format!(
                            "publish renamed preserved segment {}: {error}",
                            selected_replacement.display()
                        )
                    })?;
                std::fs::remove_file(preserved).map_err(|error| {
                    format!("retire preserved base segment {}: {error}", preserved.display())
                })?;
            }
            sync_directory(preserved_parent)?;
        }
        if selected_replacement != preserved {
            match std::fs::symlink_metadata(preserved) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    if inspect_file_identity(preserved)? != owned.identity {
                        return Err(format!(
                            "preserved old segment {} changed before retirement",
                            preserved.display()
                        ));
                    }
                    std::fs::remove_file(preserved).map_err(|error| {
                        format!("retire preserved base segment {}: {error}", preserved.display())
                    })?;
                    sync_directory(preserved_parent)?;
                }
                Ok(_) => {
                    return Err(format!(
                        "preserved old segment {} is not a regular file",
                        preserved.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("inspect {}: {error}", preserved.display())),
            }
        }
        let installed = generation.join(segment_name);
        match std::fs::symlink_metadata(&installed) {
            Ok(metadata) if metadata.file_type().is_file() => {
                if inspect_file_identity(&installed)? != owned.identity {
                    return Err(format!(
                        "installed base segment {} changed from its manifest identity",
                        installed.display()
                    ));
                }
                std::fs::remove_file(&installed).map_err(|error| {
                    format!("retire installed base segment {}: {error}", installed.display())
                })?;
                sync_directory(&generation)?;
                Ok(true)
            }
            Ok(_) => Err(format!(
                "installed base segment {} is not a regular file",
                installed.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("inspect {}: {error}", installed.display())),
        }
    }

    /// Finish only after the exact new generation is the durable selector.
    /// Marker removal and its parent-directory sync happen while the base
    /// generation's exclusive lease is still held.
    pub(crate) fn finish(self) -> Result<(), String> {
        let selected = selected_generation(&self.destination)?;
        if selected.as_deref() != Some(&self.marker.new_generation_id) {
            return Err(format!(
                "cannot finish update {}: selector names {:?}, expected new generation {}",
                self.marker.update_id, selected, self.marker.new_generation_id
            ));
        }
        let path = update_maintenance_marker_path(&self.destination);
        match read_update_maintenance_marker(&self.destination)? {
            Some(found) if found == self.marker => {}
            Some(_) => return Err(format!("{} no longer names this update", path.display())),
            None => return Err(format!("{} disappeared before maintenance finish", path.display())),
        }
        std::fs::remove_file(&path)
            .map_err(|error| format!("remove update-maintenance marker {}: {error}", path.display()))?;
        sync_directory(parent(&path))
    }
}

fn legacy_publication_id(
    candidate_generation_id: &str,
    selected_before_publish: Option<&str>,
    cleanup_generation_ids: &[String],
) -> String {
    hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&(
            "wikipedia-generation-publication",
            candidate_generation_id,
            selected_before_publish,
            cleanup_generation_ids,
        ))
        .expect("publication identity is serializable"),
    ))
}

fn legacy_publication_id_v2(
    candidate_generation_id: &str,
    selected_before_publish: Option<&str>,
    cleanup_generation_ids: &[String],
    candidate_cleanup: Option<&CandidateCleanupReceipt>,
) -> String {
    hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&(
            "wikipedia-generation-publication-v2",
            candidate_generation_id,
            selected_before_publish,
            cleanup_generation_ids,
            candidate_cleanup,
        ))
        .expect("publication identity is serializable"),
    ))
}

fn publication_id(
    candidate_generation_id: &str,
    selected_before_publish: Option<&str>,
    cleanup_generation_ids: &[String],
    candidate_cleanup: Option<&CandidateCleanupReceipt>,
) -> String {
    publication_id_with_manifest(
        candidate_generation_id,
        selected_before_publish,
        cleanup_generation_ids,
        candidate_cleanup,
        None,
    )
}

fn publication_id_with_manifest(
    candidate_generation_id: &str,
    selected_before_publish: Option<&str>,
    cleanup_generation_ids: &[String],
    candidate_cleanup: Option<&CandidateCleanupReceipt>,
    candidate_manifest: Option<&GenerationManifest>,
) -> String {
    hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&(
            "wikipedia-generation-publication-v3",
            candidate_generation_id,
            selected_before_publish,
            cleanup_generation_ids,
            candidate_cleanup,
            candidate_manifest,
        ))
        .expect("publication identity is serializable"),
    ))
}

fn validate_receipt(receipt: &PublishReceipt, path: &Path) -> Result<(), String> {
    if !matches!(
        receipt.schema,
        LEGACY_INSTALL_SCHEMA | LEGACY_INSTALL_SCHEMA_V2 | INSTALL_SCHEMA
    ) {
        return Err(format!(
            "{} has unsupported schema {}",
            path.display(),
            receipt.schema
        ));
    }
    generation_path(Path::new("mirror.swdump"), &receipt.candidate_generation_id)?;
    if let Some(selected) = receipt.selected_before_publish.as_deref() {
        generation_path(Path::new("mirror.swdump"), selected)?;
    }
    for generation in &receipt.cleanup_generation_ids {
        generation_path(Path::new("mirror.swdump"), generation)?;
    }
    if let Some(cleanup) = receipt.candidate_cleanup.as_ref() {
        if cleanup.archive.as_os_str().is_empty()
            || cleanup.title.as_os_str().is_empty()
            || cleanup.archive == cleanup.title
        {
            return Err(format!("{} has invalid candidate cleanup paths", path.display()));
        }
    }
    if let Some(manifest) = receipt.candidate_manifest.as_ref() {
        validate_generation_manifest(
            manifest,
            &receipt.candidate_generation_id,
            path,
        )?;
    }
    let expected_publication_id = if receipt.schema == LEGACY_INSTALL_SCHEMA {
        if receipt.candidate_cleanup.is_some() || receipt.candidate_manifest.is_some() {
            return Err(format!(
                "{} has candidate cleanup state under the legacy schema",
                path.display()
            ));
        }
        legacy_publication_id(
            &receipt.candidate_generation_id,
            receipt.selected_before_publish.as_deref(),
            &receipt.cleanup_generation_ids,
        )
    } else if receipt.schema == LEGACY_INSTALL_SCHEMA_V2 {
        if receipt.candidate_manifest.is_some() {
            return Err(format!(
                "{} has candidate manifest state under the legacy schema",
                path.display()
            ));
        }
        legacy_publication_id_v2(
            &receipt.candidate_generation_id,
            receipt.selected_before_publish.as_deref(),
            &receipt.cleanup_generation_ids,
            receipt.candidate_cleanup.as_ref(),
        )
    } else {
        publication_id_with_manifest(
            &receipt.candidate_generation_id,
            receipt.selected_before_publish.as_deref(),
            &receipt.cleanup_generation_ids,
            receipt.candidate_cleanup.as_ref(),
            receipt.candidate_manifest.as_ref(),
        )
    };
    if receipt.publication_id != expected_publication_id {
        return Err(format!("{} has a foreign publication identity", path.display()));
    }
    Ok(())
}

fn read_receipt(destination: &Path) -> Result<Option<PublishReceipt>, String> {
    let path = receipt_path(destination);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let receipt: PublishReceipt = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode {}: {error}", path.display()))?;
            validate_receipt(&receipt, &path)?;
            Ok(Some(receipt))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn persist_receipt(destination: &Path, receipt: &PublishReceipt) -> Result<(), String> {
    let path = receipt_path(destination);
    let mut temporary = tempfile::NamedTempFile::new_in(parent(&path))
        .map_err(|error| format!("create receipt beside {}: {error}", path.display()))?;
    serde_json::to_writer(&mut temporary, receipt)
        .map_err(|error| format!("encode {}: {error}", path.display()))?;
    temporary
        .write_all(b"\n")
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    temporary
        .persist(&path)
        .map_err(|error| format!("publish {}: {}", path.display(), error.error))?;
    sync_directory(parent(&path))
}

fn generation_identity(
    archive: &Path,
    title: &Path,
) -> Result<crate::generation::GenerationIdentity, String> {
    crate::generation::generation_identity(archive, title).map_err(|error| error.to_string())
}

fn generation_manifest_for_archive(
    generation_id: &str,
    archive_path: &Path,
) -> Result<GenerationManifest, String> {
    let archive = crate::archive_set::ArchiveSetReader::open(archive_path)
        .map_err(|error| format!("open candidate {}: {error}", archive_path.display()))?;
    let manifest = generation_manifest_from_segments(
        generation_id,
        archive_path,
        archive.segments(),
    )?;
    Ok(GenerationManifest {
        schema: GENERATION_MANIFEST_SCHEMA,
        ..manifest
    })
}

fn generation_manifest_matches(
    expected: &GenerationManifest,
    found: &GenerationManifest,
    path: &Path,
) -> Result<(), String> {
    if expected.generation_id != found.generation_id
        || expected.segments != found.segments
    {
        return Err(format!(
            "generation manifest {} disagrees with the pre-publication ownership inventory",
            path.display()
        ));
    }
    Ok(())
}

fn populate_generation(
    candidate_archive: &Path,
    candidate_title: &Path,
    destination: &Path,
    generation_id: &str,
) -> Result<PathBuf, String> {
    let expected = generation_manifest_for_archive(generation_id, candidate_archive)?;
    populate_generation_with_manifest(
        candidate_archive,
        candidate_title,
        destination,
        generation_id,
        &expected,
    )
}

fn populate_generation_with_manifest(
    candidate_archive: &Path,
    candidate_title: &Path,
    destination: &Path,
    generation_id: &str,
    expected: &GenerationManifest,
) -> Result<PathBuf, String> {
    let root = generation_root(destination);
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create {}: {error}", root.display()))?;
    let generation = generation_path(destination, generation_id)?;
    if path_exists(&generation)? {
        let identity = generation_identity(&generation, candidate_title)?;
        if identity.generation_id.as_str() != generation_id {
            return Err(format!(
                "{} does not contain generation {}",
                generation.display(),
                generation_id
            ));
        }
        ensure_generation_manifest(destination, generation_id, &generation)?;
        let found = read_generation_manifest(destination, generation_id)?
            .ok_or_else(|| format!("generation {} has no ownership manifest", generation.display()))?;
        generation_manifest_matches(expected, &found, &generation)?;
        return Ok(generation);
    }

    let archive = crate::archive_set::ArchiveSetReader::open(candidate_archive)
        .map_err(|error| format!("open candidate {}: {error}", candidate_archive.display()))?;
    let temporary = candidate_generation_pending_path(destination, generation_id)?;
    match std::fs::symlink_metadata(&temporary) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "generation staging path {} is not a directory",
                temporary.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&temporary)
                .map_err(|error| format!("stage generation in {}: {error}", root.display()))?;
            sync_directory(&root)?;
        }
        Err(error) => return Err(format!("inspect generation staging path {}: {error}", temporary.display())),
    }
    for segment in archive.segments() {
        let source = candidate_archive.join(&segment.name);
        let target = temporary.join(&segment.name);
        let expected_segment = expected
            .segments
            .iter()
            .find(|owned| owned.name == segment.name)
            .ok_or_else(|| format!("candidate manifest has no segment named {}", segment.name))?;
        let (source_file, source_identity) = open_regular_file(&source)?;
        if source_identity != expected_segment.identity {
            return Err(format!(
                "candidate segment {} changed after ownership capture",
                source.display()
            ));
        }
        source_file
            .sync_all()
            .map_err(|error| format!("sync candidate segment {}: {error}", source.display()))?;
        match inspect_file_identity(&target) {
            Ok(target_identity) if target_identity == expected_segment.identity => continue,
            Ok(_) => {
                return Err(format!(
                    "staged candidate segment {} has an unexpected identity",
                    target.display()
                ));
            }
            Err(error) if namespace_exists(&target)? => {
                return Err(format!(
                    "staged candidate segment {} cannot be used: {error}",
                    target.display()
                ));
            }
            Err(_) => {}
        }
        std::fs::hard_link(&source, &target).map_err(|error| {
            format!(
                "hard-link candidate segment {} into destination-local generation: {error}",
                source.display()
            )
        })?;
        let target_identity = inspect_file_identity(&target)?;
        if target_identity != expected_segment.identity {
            return Err(format!(
                "staged candidate segment {} does not retain its captured identity",
                target.display()
            ));
        }
    }
    let residual = std::fs::read_dir(&temporary)
        .map_err(|error| format!("inspect generation staging path {}: {error}", temporary.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| format!("inspect generation staging path {}: {error}", temporary.display()))?;
    if residual.len() != archive.segments().len() {
        return Err(format!(
            "generation staging path {} contains unexpected entries",
            temporary.display()
        ));
    }
    sync_directory(&temporary)?;
    std::fs::rename(&temporary, &generation)
        .map_err(|error| format!("publish generation {}: {error}", generation.display()))?;
    sync_directory(&root)?;
    persist_generation_manifest(destination, generation_id, expected)?;
    generation_identity(&generation, candidate_title)?;
    Ok(generation)
}

fn stage_selector(
    candidate_title: &Path,
    destination: &Path,
    generation_id: &str,
) -> Result<PathBuf, String> {
    let pending = pending_selector_path(destination, generation_id)?;
    if path_exists(&pending)? {
        let titles = crate::title_index::TitleIndex::open(&pending)
            .map_err(|error| format!("open pending selector {}: {error}", pending.display()))?;
        if titles.generation_id().as_str() != generation_id {
            return Err(format!(
                "{} names generation {}, expected {}",
                pending.display(),
                titles.generation_id().as_str(),
                generation_id
            ));
        }
        return Ok(pending);
    }
    std::fs::hard_link(candidate_title, &pending).map_err(|error| {
        format!(
            "stage selector {} by same-filesystem hard link: {error}",
            pending.display()
        )
    })?;
    std::fs::File::open(&pending)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync pending selector {}: {error}", pending.display()))?;
    sync_directory(&generation_root(destination))?;
    Ok(pending)
}

fn bound_selected_manifest(
    destination: &Path,
    generation_id: &str,
) -> Result<(PathBuf, GenerationManifest), String> {
    let generation = generation_path(destination, generation_id)?;
    ensure_generation_manifest(destination, generation_id, &generation)?;
    let manifest = read_generation_manifest(destination, generation_id)?
        .ok_or_else(|| format!("selected generation {} has no manifest", generation.display()))?;
    for segment in &manifest.segments {
        let selected = generation.join(&segment.name);
        let actual = inspect_file_identity(&selected)?;
        if actual != segment.identity {
            return Err(format!(
                "selected generation segment {} disagrees with its manifest",
                selected.display()
            ));
        }
    }
    Ok((generation, manifest))
}

fn retire_candidate_archive(
    destination: &Path,
    generation_id: &str,
    cleanup: &CandidateCleanupReceipt,
) -> Result<(), String> {
    let (_generation, manifest) = bound_selected_manifest(destination, generation_id)?;
    let pending = candidate_archive_pending_path(destination, generation_id)?;

    let source_present = namespace_exists(&cleanup.archive)?;
    let pending_present = namespace_exists(&pending)?;
    if source_present && pending_present {
        return Err(format!(
            "candidate archive cleanup has both source {} and pending {}",
            cleanup.archive.display(),
            pending.display()
        ));
    }
    if !pending_present {
        if !source_present {
            // A crash after the last unlink and before receipt retirement is
            // indistinguishable from an already-complete cleanup. The selected
            // counterparts above still prove every expected manifest identity.
            return Ok(());
        }
        let metadata = std::fs::symlink_metadata(&cleanup.archive)
            .map_err(|error| format!("inspect candidate archive {}: {error}", cleanup.archive.display()))?;
        if !metadata.file_type().is_dir() {
            return Err(format!(
                "candidate archive {} is not a regular directory; cleanup remains pending",
                cleanup.archive.display()
            ));
        }
        crate::instance::rename_without_replacing(&cleanup.archive, &pending).map_err(|error| {
            format!(
                "claim candidate archive {} at {}: {error}",
                cleanup.archive.display(),
                pending.display()
            )
        })?;
        sync_directory(parent(&cleanup.archive))?;
        sync_directory(&generation_root(destination))?;
    }

    let metadata = std::fs::symlink_metadata(&pending)
        .map_err(|error| format!("inspect pending candidate archive {}: {error}", pending.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "pending candidate archive {} is not a regular directory; cleanup remains pending",
            pending.display()
        ));
    }
    let mut pending_paths = Vec::new();
    for segment in &manifest.segments {
        let candidate = pending.join(&segment.name);
        let candidate_identity = match inspect_file_identity(&candidate) {
            Ok(identity) => identity,
            Err(_error) if !namespace_exists(&candidate)? => continue,
            Err(_) => {
                pending_paths.push(candidate);
                continue;
            }
        };
        if candidate_identity == segment.identity {
            match std::fs::remove_file(&candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "unlink redundant candidate segment {}: {error}",
                        candidate.display()
                    ));
                }
            }
            sync_directory(&pending)?;
        } else {
            pending_paths.push(candidate);
        }
    }

    let residue = std::fs::read_dir(&pending)
        .map_err(|error| format!("inspect candidate archive residue {}: {error}", pending.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| format!("inspect candidate archive residue {}: {error}", pending.display()))?;
    if residue.is_empty() {
        std::fs::remove_dir(&pending)
            .map_err(|error| format!("remove empty candidate archive {}: {error}", pending.display()))?;
        sync_directory(&generation_root(destination))?;
    } else {
        pending_paths.extend(residue.into_iter().map(|entry| entry.path()));
    }
    if !pending_paths.is_empty() {
        return Err(format!(
            "candidate archive cleanup remains pending at {}",
            pending_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

fn retire_candidate_title(
    destination: &Path,
    generation_id: &str,
    cleanup: &CandidateCleanupReceipt,
) -> Result<(), String> {
    let selector = selector_path(destination);
    let selected_identity = inspect_file_identity(&selector)?;
    let pending = candidate_title_pending_path(destination, generation_id)?;

    let source_present = namespace_exists(&cleanup.title)?;
    let pending_present = namespace_exists(&pending)?;
    if source_present && pending_present {
        return Err(format!(
            "candidate title cleanup has both source {} and pending {}",
            cleanup.title.display(),
            pending.display()
        ));
    }
    if !pending_present {
        if !source_present {
            if same_regular_file(&selected_identity, &cleanup.title_identity) {
                return Ok(());
            }
            return Err(format!(
                "candidate title {} is absent but selector {} does not prove its expected identity",
                cleanup.title.display(),
                selector.display()
            ));
        }
        crate::instance::rename_without_replacing(&cleanup.title, &pending).map_err(|error| {
            format!(
                "claim candidate title {} at {}: {error}",
                cleanup.title.display(),
                pending.display()
            )
        })?;
        sync_directory(parent(&cleanup.title))?;
        sync_directory(&generation_root(destination))?;
    }

    let candidate_identity = inspect_file_identity(&pending).map_err(|error| {
        format!(
            "candidate title {} cannot be reclaimed without changing it: {error}",
            pending.display()
        )
    })?;
    if candidate_identity != cleanup.title_identity || candidate_identity != selected_identity {
        return Err(format!(
            "candidate title {} does not identify the selected selector; cleanup remains pending",
            pending.display()
        ));
    }
    std::fs::remove_file(&pending)
        .map_err(|error| format!("unlink redundant candidate title {}: {error}", pending.display()))?;
    sync_directory(&generation_root(destination))?;
    Ok(())
}

fn retire_candidate_links(
    destination: &Path,
    generation_id: &str,
    cleanup: &CandidateCleanupReceipt,
) -> bool {
    let mut pending = false;
    if let Err(error) = retire_candidate_archive(destination, generation_id, cleanup) {
        eprintln!(
            "selected generation is live; candidate archive link cleanup remains pending: {error}"
        );
        pending = true;
    }
    if let Err(error) = retire_candidate_title(destination, generation_id, cleanup) {
        eprintln!(
            "selected generation is live; candidate title link cleanup remains pending: {error}"
        );
        pending = true;
    }
    pending
}

fn generation_segments_for_cleanup(
    destination: &Path,
    generation_id: &str,
    generation: &Path,
    _lease: &crate::archive::ArchiveCleanupLease,
) -> Result<Vec<GenerationManifestSegment>, String> {
    // A current manifest is already the durable ownership authority. Do not
    // reopen the archive set here: a crash may have removed one expected
    // segment, and that is precisely the idempotent missing-path case cleanup
    // must be able to finish.
    match read_generation_manifest_state(destination, generation_id)? {
        Some(GenerationManifestState::Current(found)) => Ok(found.segments),
        Some(GenerationManifestState::MetadataMigration { schema }) => Err(format!(
            "generation {} has legacy ownership manifest schema {}; ownership is unknown after selector replacement",
            generation.display(),
            schema
        )),
        None => Err(format!(
            "generation {} has no ownership manifest; ownership is unknown after selector replacement",
            generation.display()
        )),
    }
}

fn retire_displaced_generation(
    destination: &Path,
    generation_id: &str,
    generation: &Path,
    lease: &crate::archive::ArchiveCleanupLease,
) -> Result<GenerationCleanupReport, String> {
    let segments =
        generation_segments_for_cleanup(destination, generation_id, generation, lease)?;
    let root = generation_root(destination);
    let mut report = GenerationCleanupReport::default();
    for segment in &segments {
        let path = generation.join(&segment.name);
        let identity = match inspect_file_identity(&path) {
            Ok(identity) => identity,
            Err(_error) if !namespace_exists(&path)? => continue,
            Err(_) => {
                report.pending_paths.push(path);
                continue;
            }
        };
        if identity != segment.identity {
            report.pending_paths.push(path);
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.reclaimed_segments += 1;
                report.reclaimed_bytes += segment.bytes;
                sync_directory(&generation)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("unlink owned segment {}: {error}", path.display()));
            }
        }
    }

    let residual = std::fs::read_dir(generation)
        .map_err(|error| format!("inspect generation residue {}: {error}", generation.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| format!("inspect generation residue {}: {error}", generation.display()))?;
    for entry in residual {
        report.pending_paths.push(entry.path());
    }
    if !report.pending_paths.is_empty() {
        return Ok(report);
    }
    std::fs::remove_dir(generation)
        .map_err(|error| format!("retire empty generation {}: {error}", generation.display()))?;
    sync_directory(&root)?;

    let manifest = generation_manifest_path(destination, generation_id)?;
    match std::fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&manifest)
                .map_err(|error| format!("remove {}: {error}", manifest.display()))?;
            sync_directory(&root)?;
        }
        Ok(_) => {
            return Err(format!(
                "generation manifest {} is not a regular file",
                manifest.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect {}: {error}", manifest.display())),
    }
    Ok(report)
}

fn retire_missing_generation_manifest(
    destination: &Path,
    generation_id: &str,
) -> Result<(), String> {
    match read_generation_manifest_state(destination, generation_id)? {
        None => return Ok(()),
        Some(GenerationManifestState::MetadataMigration { schema }) => {
            return Err(format!(
                "generation {} has legacy ownership manifest schema {}; it is retained",
                generation_id, schema
            ));
        }
        Some(GenerationManifestState::Current(_)) => {}
    }
    let manifest = generation_manifest_path(destination, generation_id)?;
    match std::fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&manifest)
                .map_err(|error| format!("remove {}: {error}", manifest.display()))?;
            sync_directory(&generation_root(destination))?;
            Ok(())
        }
        Ok(_) => Err(format!(
            "generation manifest {} is not a regular file",
            manifest.display()
        )),
        Err(error) => Err(format!("inspect {}: {error}", manifest.display())),
    }
}

fn cleanup_displaced(
    destination: &Path,
    receipt: &mut PublishReceipt,
) -> Result<bool, String> {
    let maintenance = read_update_maintenance_marker(destination)?;
    let mut pending = Vec::new();
    for generation_id in &receipt.cleanup_generation_ids {
        if generation_id == &receipt.candidate_generation_id {
            continue;
        }
        if maintenance
            .as_ref()
            .is_some_and(|marker| marker.base_generation_id == *generation_id)
        {
            pending.push(generation_id.clone());
            continue;
        }
        let generation = generation_path(destination, generation_id)?;
        match std::fs::symlink_metadata(&generation) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                eprintln!(
                    "generation {} is not a real directory; cleanup remains pending",
                    generation.display()
                );
                pending.push(generation_id.clone());
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = retire_missing_generation_manifest(destination, generation_id) {
                    eprintln!(
                        "generation {} is absent; manifest cleanup remains pending: {error}",
                        generation.display()
                    );
                    pending.push(generation_id.clone());
                }
                continue;
            }
            Err(error) => {
                eprintln!(
                    "generation {} cannot be inspected; cleanup remains pending: {error}",
                    generation.display()
                );
                pending.push(generation_id.clone());
                continue;
            }
        }
        match crate::archive::try_acquire_archive_cleanup_lease(&generation) {
            Ok(Some(lease)) => {
                match retire_displaced_generation(destination, generation_id, &generation, &lease) {
                    Ok(report) if report.pending_paths.is_empty() => {}
                    Ok(report) => {
                        eprintln!(
                            "generation {} cleanup remains pending for {} retained paths",
                            generation.display(),
                            report.pending_paths.len()
                        );
                        pending.push(generation_id.clone());
                    }
                    Err(error) => {
                        eprintln!(
                            "generation {} is no longer selected; cleanup remains pending: {error}",
                            generation.display()
                        );
                        pending.push(generation_id.clone());
                    }
                }
            }
            Ok(None) => pending.push(generation_id.clone()),
            Err(error) => {
                eprintln!(
                    "generation {} is no longer selected; lease check remains pending: {error}",
                    generation.display()
                );
                pending.push(generation_id.clone());
            }
        }
    }
    receipt.cleanup_generation_ids = pending;
    Ok(!receipt.cleanup_generation_ids.is_empty())
}

fn finish_cleanup_receipt(destination: &Path, receipt: &mut PublishReceipt) -> bool {
    if receipt.cleanup_generation_ids.is_empty() && receipt.candidate_cleanup.is_none() {
        match std::fs::remove_file(receipt_path(destination)) {
            Ok(()) => match sync_directory(parent(destination)) {
                Ok(()) => false,
                Err(error) => {
                    eprintln!(
                        "generation is selected; cleanup receipt directory sync remains pending: {error}"
                    );
                    true
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                eprintln!(
                    "generation is selected; cleanup receipt removal remains pending: {error}"
                );
                true
            }
        }
    } else {
        receipt.schema = INSTALL_SCHEMA;
        receipt.publication_id = publication_id_with_manifest(
            &receipt.candidate_generation_id,
            receipt.selected_before_publish.as_deref(),
            &receipt.cleanup_generation_ids,
            receipt.candidate_cleanup.as_ref(),
            receipt.candidate_manifest.as_ref(),
        );
        if let Err(error) = persist_receipt(destination, receipt) {
            eprintln!(
                "generation is selected; cleanup receipt update remains pending: {error}"
            );
        }
        true
    }
}

/// Reclaim one named, unselected installed generation.
///
/// This is intentionally narrower than mirror-root deletion. It never
/// recursively removes a generation, uses only the metadata ownership
/// manifest, and unlinks a segment only while its exact captured identity is
/// still present at the expected path. Mismatches and unknown entries remain
/// in place and are reported as pending. The engine must hold its writer lock
/// and pass the exact generation cleanup lease acquired before mutation.
pub fn reclaim_installed_generation(
    destination: &Path,
    generation_id: &str,
    lease: &crate::archive::ArchiveCleanupLease,
) -> Result<GenerationCleanupReport, String> {
    let generation = generation_path(destination, generation_id)?;
    if selected_generation(destination)?.as_deref() == Some(generation_id) {
        return Err(format!("refusing to reclaim selected generation {}", generation.display()));
    }
    match std::fs::symlink_metadata(&generation) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            if !lease.covers(&generation).map_err(|error| error.to_string())? {
                return Err(format!(
                    "cleanup lease does not cover generation {}",
                    generation.display()
                ));
            }
            retire_displaced_generation(destination, generation_id, &generation, lease)
        }
        Ok(_) => Err(format!("generation {} is not a real directory", generation.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            retire_missing_generation_manifest(destination, generation_id)?;
            Ok(GenerationCleanupReport::default())
        }
        Err(error) => Err(format!("inspect generation {}: {error}", generation.display())),
    }
}

/// Complete a receipted selector publication and its independent old-generation cleanup.
pub(crate) fn recover(destination: &Path) -> Result<Option<InstallOutcome>, String> {
    let Some(mut receipt) = read_receipt(destination)? else {
        return Ok(None);
    };
    let selected = selected_generation(destination)?;
    if selected.as_deref() != Some(&receipt.candidate_generation_id) {
        if selected.as_deref() != receipt.selected_before_publish.as_deref() {
            return Err(format!(
                "selector changed outside publication {}",
                receipt.publication_id
            ));
        }
        let generation =
            generation_path(destination, &receipt.candidate_generation_id)?;
        let pending =
            pending_selector_path(destination, &receipt.candidate_generation_id)?;
        let generation_present = path_exists(&generation)?;
        let pending_present = path_exists(&pending)?;
        if !generation_present {
            if pending_present {
                return Err(
                    "publication receipt has a pending selector without its immutable generation".into(),
                );
            }
            let cleanup = receipt.candidate_cleanup.as_ref().ok_or_else(|| {
                "publication receipt lacks candidate inputs for staging recovery".to_owned()
            })?;
            let manifest = receipt.candidate_manifest.as_ref().ok_or_else(|| {
                "publication receipt lacks metadata ownership for staging recovery".to_owned()
            })?;
            populate_generation_with_manifest(
                &cleanup.archive,
                &cleanup.title,
                destination,
                &receipt.candidate_generation_id,
                manifest,
            )?;
        } else {
            ensure_generation_manifest(destination, &receipt.candidate_generation_id, &generation)?;
            if let Some(expected) = receipt.candidate_manifest.as_ref() {
                let found = read_generation_manifest(destination, &receipt.candidate_generation_id)?
                    .ok_or_else(|| format!("generation {} has no ownership manifest", generation.display()))?;
                generation_manifest_matches(expected, &found, &generation)?;
            }
        }
        if !pending_present {
            let cleanup = receipt.candidate_cleanup.as_ref().ok_or_else(|| {
                "publication receipt lacks candidate title for selector recovery".to_owned()
            })?;
            stage_selector(&cleanup.title, destination, &receipt.candidate_generation_id)?;
        }
        let pending = pending_selector_path(destination, &receipt.candidate_generation_id)?;
        let identity = generation_identity(&generation, &pending)?;
        if identity.generation_id.as_str() != receipt.candidate_generation_id {
            return Err("pending selector and immutable generation disagree".into());
        }
        let selector = selector_path(destination);
        std::fs::rename(&pending, &selector)
            .map_err(|error| format!("publish selector {}: {error}", selector.display()))?;
        sync_directory(parent(&selector))?;
    }
    let candidate_cleanup_pending = receipt.candidate_cleanup.as_ref().is_some_and(|cleanup| {
        retire_candidate_links(destination, &receipt.candidate_generation_id, cleanup)
    });
    if !candidate_cleanup_pending {
        receipt.candidate_cleanup = None;
    }
    let displaced_cleanup_pending = match cleanup_displaced(destination, &mut receipt) {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!(
                "selected generation is live; previous generation cleanup remains pending: {error}"
            );
            true
        }
    };
    let receipt_pending = finish_cleanup_receipt(destination, &mut receipt);
    Ok(Some(InstallOutcome {
        cleanup_pending: candidate_cleanup_pending
            || displaced_cleanup_pending
            || receipt_pending,
        candidate_cleanup_pending,
    }))
}

/// Install a validated candidate, recording its metadata ownership before any
/// destination-local generation objects are staged. Candidate inputs are
/// reclaimed only after selector publication and exact identity comparison.
pub(crate) fn install(
    candidate_archive: PathBuf,
    candidate_title: PathBuf,
    destination: &Path,
) -> Result<InstallOutcome, String> {
    std::fs::create_dir_all(parent(destination))
        .map_err(|error| format!("create {}: {error}", parent(destination).display()))?;

    let previous_cleanup = if let Some(outcome) = recover(destination)? {
        if outcome.candidate_cleanup_pending {
            return Err(
                "previous selected candidate link cleanup is still pending; retry recovery before installing another generation"
                    .into(),
            );
        }
        if outcome.cleanup_pending {
            read_receipt(destination)?
                .map(|receipt| receipt.cleanup_generation_ids)
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let identity = generation_identity(&candidate_archive, &candidate_title)?;
    let candidate_generation_id = identity.generation_id.as_str().to_owned();
    let selected_before_publish = selected_generation(destination)?;
    if let Some(previous_id) = selected_before_publish.as_deref() {
        let previous_generation = generation_path(destination, previous_id)?;
        // Upgrade the selected generation's legacy ownership receipt while it
        // is still authoritative. This is metadata-only and happens before
        // the generation can enter the displaced-cleanup list.
        match read_generation_manifest_state(destination, previous_id)? {
            Some(GenerationManifestState::Current(_)) => {}
            Some(GenerationManifestState::MetadataMigration { .. }) | None => {
                ensure_generation_manifest(destination, previous_id, &previous_generation)?;
            }
        }
    }
    let candidate_manifest = generation_manifest_for_archive(
        &candidate_generation_id,
        &candidate_archive,
    )?;
    let candidate_title_identity = inspect_file_identity(&candidate_title)?;

    let mut cleanup_generation_ids = previous_cleanup;
    if let Some(previous) = selected_before_publish.as_ref() {
        if previous != &candidate_generation_id
            && !cleanup_generation_ids.iter().any(|value| value == previous)
        {
            cleanup_generation_ids.push(previous.clone());
        }
    }
    cleanup_generation_ids.sort();
    cleanup_generation_ids.dedup();
    let candidate_cleanup = CandidateCleanupReceipt {
        archive: candidate_archive,
        title: candidate_title,
        title_identity: candidate_title_identity,
    };
    let receipt = PublishReceipt {
        schema: INSTALL_SCHEMA,
        publication_id: publication_id_with_manifest(
            &candidate_generation_id,
            selected_before_publish.as_deref(),
            &cleanup_generation_ids,
            Some(&candidate_cleanup),
            Some(&candidate_manifest),
        ),
        candidate_generation_id: candidate_generation_id.clone(),
        selected_before_publish: selected_before_publish.clone(),
        cleanup_generation_ids,
        candidate_cleanup: Some(candidate_cleanup),
        candidate_manifest: Some(candidate_manifest.clone()),
    };
    persist_receipt(destination, &receipt)?;
    populate_generation_with_manifest(
        receipt
            .candidate_cleanup
            .as_ref()
            .expect("candidate cleanup is present during install")
            .archive
            .as_path(),
        receipt
            .candidate_cleanup
            .as_ref()
            .expect("candidate cleanup is present during install")
            .title
            .as_path(),
        destination,
        &candidate_generation_id,
        receipt
            .candidate_manifest
            .as_ref()
            .expect("candidate manifest is present during install"),
    )?;
    if selected_before_publish.as_deref() != Some(&candidate_generation_id) {
        let cleanup = receipt
            .candidate_cleanup
            .as_ref()
            .expect("candidate cleanup is present during install");
        stage_selector(&cleanup.title, destination, &candidate_generation_id)?;
    }
    recover(destination)?
        .ok_or_else(|| "publication receipt disappeared before selector commit".into())
}

pub(crate) fn candidate_cleanup_owns_path(
    destination: &Path,
    path: &Path,
) -> Result<bool, String> {
    Ok(read_receipt(destination)?
        .and_then(|receipt| receipt.candidate_cleanup)
        .is_some_and(|cleanup| cleanup.archive == path || cleanup.title == path))
}

/// Resolve the selected immutable generation without acquiring a serving
/// lease or admitting a reader. Recovery and update-maintenance callers may
/// use this only after taking their destination writer authority.
pub(crate) fn selected_generation_paths(
    destination: &Path,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    let Some(generation_id) = selected_generation(destination)? else {
        return Ok(None);
    };
    let archive = generation_path(destination, &generation_id)?;
    if !path_exists(&archive)? {
        return Err(format!(
            "selector names unavailable generation {}",
            archive.display()
        ));
    }
    Ok(Some((archive, selector_path(destination))))
}

/// Select one immutable archive without scanning the generation directory.
pub(crate) fn serving_pair(destination: &Path) -> Result<Option<ServingPair>, String> {
    let Some(generation_id) = selected_generation(destination)? else {
        return Ok(None);
    };
    let archive = generation_path(destination, &generation_id)?;
    if !path_exists(&archive)? {
        return Err(format!(
            "selector names unavailable generation {}",
            archive.display()
        ));
    }
    // Acquire the shared generation lease before checking the marker. If the
    // marker is published after this point, its writer cannot acquire the
    // exclusive lease and must return a retryable reader-active result.
    let generation_lease = crate::archive::try_acquire_archive_shared_lease(&archive)
        .map_err(|error| format!("acquire shared generation lease {}: {error}", archive.display()))?
        .ok_or_else(|| {
            format!(
                "serving is unavailable while generation {} is under exclusive maintenance",
                archive.display()
            )
        })?;
    ensure_no_update_maintenance(destination)?;
    Ok(Some(ServingPair {
        archive,
        title: selector_path(destination),
        _generation_lease: std::sync::Arc::new(generation_lease),
    }))
}

/// Retry a read-only open only when atomic selector replacement changed the selection.
pub(crate) fn with_serving_pair<T>(
    destination: &Path,
    mut open: impl FnMut(&ServingPair) -> Result<T, String>,
) -> Result<T, String> {
    let first = serving_pair(destination)?
        .ok_or_else(|| format!("{} has no committed generation", destination.display()))?;
    match open(&first) {
        Ok(value) => Ok(value),
        Err(first_error) => {
            let second = serving_pair(destination)?
                .ok_or_else(|| format!("{} has no committed generation", destination.display()))?;
            if second == first {
                Err(first_error)
            } else {
                open(&second)
            }
        }
    }
}

/// All persistent paths owned by one logical Wikipedia mirror.
pub(crate) fn auxiliary_paths(destination: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(vec![
        selector_path(destination),
        generation_root(destination),
        receipt_path(destination),
        destination.with_extension("swrefs"),
        update_maintenance_marker_path(destination),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{
        ArchiveWriter, CompressionSettings, ManifestRecord, Record, SiteInfoRecord,
    };

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn generation(parent: &Path, name: &str) -> (PathBuf, PathBuf, String) {
        let archive = parent.join(format!("{name}.swdump"));
        let title = archive.with_extension("swtitle");
        let output = crate::archive_set::ArchiveSetOutput::new_in(parent, 1 << 20).unwrap();
        let mut writer = ArchiveWriter::with_ref_prefix(
            output,
            128,
            CompressionSettings::default(),
            b"generation fixture reference",
        )
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
        let id = crate::generation::GenerationId::from_plan_bytes(name.as_bytes());
        crate::title_index::build(&archive, &title, &id).unwrap();
        (archive, title, id.as_str().to_owned())
    }

    fn publish_candidate_without_cleanup(
        candidate_archive: &Path,
        candidate_title: &Path,
        destination: &Path,
        generation_id: &str,
    ) -> CandidateCleanupReceipt {
        assert!(selected_generation(destination).unwrap().is_none());
        populate_generation(
            candidate_archive,
            candidate_title,
            destination,
            generation_id,
        )
        .unwrap();
        let title_identity = inspect_file_identity(candidate_title).unwrap();
        let pending = stage_selector(candidate_title, destination, generation_id).unwrap();
        let cleanup = CandidateCleanupReceipt {
            archive: candidate_archive.to_path_buf(),
            title: candidate_title.to_path_buf(),
            title_identity,
        };
        persist_receipt(
            destination,
            &PublishReceipt {
                schema: INSTALL_SCHEMA,
                publication_id: publication_id(
                    generation_id,
                    None,
                    &[],
                    Some(&cleanup),
                ),
                candidate_generation_id: generation_id.to_owned(),
                selected_before_publish: None,
                cleanup_generation_ids: Vec::new(),
                candidate_cleanup: Some(cleanup.clone()),
                candidate_manifest: None,
            },
        )
        .unwrap();
        std::fs::rename(pending, selector_path(destination)).unwrap();
        sync_directory(parent(&selector_path(destination))).unwrap();
        cleanup
    }

    fn publish_replacement_without_recovery(
        destination: &Path,
        selected_id: &str,
        candidate_archive: &Path,
        candidate_title: &Path,
        candidate_id: &str,
    ) -> PathBuf {
        let generation = populate_generation(
            candidate_archive,
            candidate_title,
            destination,
            candidate_id,
        )
        .unwrap();
        let pending = stage_selector(candidate_title, destination, candidate_id).unwrap();
        let cleanup = vec![selected_id.to_owned()];
        persist_receipt(
            destination,
            &PublishReceipt {
                schema: INSTALL_SCHEMA,
                publication_id: publication_id(candidate_id, Some(selected_id), &cleanup, None),
                candidate_generation_id: candidate_id.to_owned(),
                selected_before_publish: Some(selected_id.to_owned()),
                cleanup_generation_ids: cleanup,
                candidate_cleanup: None,
                candidate_manifest: None,
            },
        )
        .unwrap();
        std::fs::rename(pending, selector_path(destination)).unwrap();
        sync_directory(parent(&selector_path(destination))).unwrap();
        generation
    }

    #[test]
    fn successful_install_retires_redundant_candidate_links() {
        let temporary = tempfile::tempdir().unwrap();
        let scratch = temporary.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, generation_id) = generation(&scratch, "archive");

        let outcome = install(archive.clone(), title.clone(), &destination).unwrap();

        assert!(!outcome.cleanup_pending);
        assert!(!outcome.candidate_cleanup_pending);
        assert!(!archive.exists());
        assert!(!title.exists());
        assert!(!candidate_archive_pending_path(&destination, &generation_id)
            .unwrap()
            .exists());
        assert!(!candidate_title_pending_path(&destination, &generation_id)
            .unwrap()
            .exists());
        let selected_generation = generation_path(&destination, &generation_id).unwrap();
        assert!(selected_generation.is_dir());
        assert!(selector_path(&destination).is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let manifest = read_generation_manifest(&destination, &generation_id)
                .unwrap()
                .unwrap();
            for segment in manifest.segments {
                assert_eq!(
                    std::fs::metadata(selected_generation.join(segment.name))
                        .unwrap()
                        .nlink(),
                    1,
                    "no candidate link pins a selected archive segment"
                );
            }
            assert_eq!(
                std::fs::metadata(selector_path(&destination)).unwrap().nlink(),
                1,
                "no candidate link pins the selected title index"
            );
        }
        assert!(recover(&destination).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn selected_digest_only_schema_two_manifest_migrates_without_payload_read() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (archive, title, generation_id) = generation(temporary.path(), "legacy-selected");
        install(archive, title, &destination).unwrap();

        let generation_path = generation_path(&destination, &generation_id).unwrap();
        let current = read_generation_manifest(&destination, &generation_id)
            .unwrap()
            .unwrap();
        let legacy = serde_json::json!({
            "schema": LEGACY_GENERATION_MANIFEST_SCHEMA,
            "generation_id": &generation_id,
            "segments": current
                .segments
                .iter()
                .map(|segment| serde_json::json!({
                    "name": segment.name,
                    "bytes": segment.bytes,
                    "digest": "0".repeat(64),
                }))
                .collect::<Vec<_>>(),
        });
        std::fs::write(
            generation_manifest_path(&destination, &generation_id).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let original_modes = current
            .segments
            .iter()
            .map(|segment| {
                let path = generation_path.join(&segment.name);
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0)).unwrap();
                (path, mode)
            })
            .collect::<Vec<_>>();

        ensure_generation_manifest(&destination, &generation_id, &generation_path).unwrap();

        for (path, mode) in original_modes {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let migrated = read_generation_manifest(&destination, &generation_id)
            .unwrap()
            .unwrap();
        assert_eq!(migrated.schema, GENERATION_MANIFEST_SCHEMA);
        assert_eq!(migrated.segments.len(), current.segments.len());
        assert!(migrated
            .segments
            .iter()
            .all(|segment| segment.identity.bytes == segment.bytes));
    }

    #[test]
    fn identity_bearing_schema_two_cleans_unselected_generation_idempotently() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "legacy-owned");
        install(first_archive, first_title, &destination).unwrap();

        let old_generation = generation_path(&destination, &first_id).unwrap();
        let current = read_generation_manifest(&destination, &first_id)
            .unwrap()
            .unwrap();
        assert!(current.segments.len() > 1);
        let legacy = serde_json::json!({
            "schema": LEGACY_GENERATION_MANIFEST_SCHEMA,
            "generation_id": &first_id,
            "segments": current
                .segments
                .iter()
                .map(|segment| serde_json::json!({
                    "name": segment.name,
                    "bytes": segment.bytes,
                    "digest": "1".repeat(64),
                    "identity": {
                        "device": segment.identity.device,
                        "inode": segment.identity.inode,
                        "birth_seconds": 123_i64,
                        "birth_nanos": 456_i64,
                        "modified_seconds": segment.identity.modified_seconds,
                        "modified_nanos": segment.identity.modified_nanos,
                        "bytes": segment.identity.bytes,
                    },
                }))
                .collect::<Vec<_>>(),
        });
        std::fs::write(
            generation_manifest_path(&destination, &first_id).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let decoded = read_generation_manifest(&destination, &first_id)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.schema, GENERATION_MANIFEST_SCHEMA);
        assert_eq!(decoded.segments, current.segments);

        let (second_archive, second_title, second_id) = generation(temporary.path(), "replacement");
        let second_generation = publish_replacement_without_recovery(
            &destination,
            &first_id,
            &second_archive,
            &second_title,
            &second_id,
        );
        let already_reclaimed = old_generation.join(&current.segments[0].name);
        std::fs::remove_file(&already_reclaimed).unwrap();
        sync_directory(&old_generation).unwrap();

        let outcome = recover(&destination).unwrap().unwrap();

        assert!(!outcome.cleanup_pending);
        assert!(!old_generation.exists());
        assert!(!generation_manifest_path(&destination, &first_id).unwrap().exists());
        assert!(second_generation.is_dir());
        assert!(read_receipt(&destination).unwrap().is_none());
    }

    #[test]
    fn digest_only_schema_two_unselected_generation_remains_unknown_and_untouched() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "legacy-unknown");
        install(first_archive, first_title, &destination).unwrap();

        let old_generation = generation_path(&destination, &first_id).unwrap();
        let current = read_generation_manifest(&destination, &first_id)
            .unwrap()
            .unwrap();
        let legacy = serde_json::json!({
            "schema": LEGACY_GENERATION_MANIFEST_SCHEMA,
            "generation_id": &first_id,
            "segments": current
                .segments
                .iter()
                .map(|segment| serde_json::json!({
                    "name": segment.name,
                    "bytes": segment.bytes,
                    "digest": "2".repeat(64),
                }))
                .collect::<Vec<_>>(),
        });
        let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
        let manifest_path = generation_manifest_path(&destination, &first_id).unwrap();
        std::fs::write(&manifest_path, &legacy_bytes).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "replacement");
        publish_replacement_without_recovery(
            &destination,
            &first_id,
            &second_archive,
            &second_title,
            &second_id,
        );

        let outcome = recover(&destination).unwrap().unwrap();

        assert!(outcome.cleanup_pending);
        assert!(old_generation.is_dir());
        assert!(current
            .segments
            .iter()
            .all(|segment| old_generation.join(&segment.name).is_file()));
        assert_eq!(std::fs::read(&manifest_path).unwrap(), legacy_bytes);
        assert!(read_receipt(&destination).unwrap().is_some());
        assert!(recover(&destination).unwrap().unwrap().cleanup_pending);
    }

    #[test]
    fn candidate_cleanup_resumes_after_archive_namespace_claim() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, generation_id) = generation(temporary.path(), "candidate");
        publish_candidate_without_cleanup(&archive, &title, &destination, &generation_id);
        let pending = candidate_archive_pending_path(&destination, &generation_id).unwrap();
        crate::instance::rename_without_replacing(&archive, &pending).unwrap();
        sync_directory(parent(&archive)).unwrap();
        sync_directory(&generation_root(&destination)).unwrap();

        let outcome = recover(&destination).unwrap().unwrap();

        assert!(!outcome.cleanup_pending);
        assert!(!outcome.candidate_cleanup_pending);
        assert!(!archive.exists());
        assert!(!pending.exists());
        assert!(!title.exists());
        assert!(recover(&destination).unwrap().is_none());
    }

    #[test]
    fn candidate_cleanup_resumes_after_one_segment_unlink() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, generation_id) = generation(temporary.path(), "candidate");
        publish_candidate_without_cleanup(&archive, &title, &destination, &generation_id);
        let pending = candidate_archive_pending_path(&destination, &generation_id).unwrap();
        crate::instance::rename_without_replacing(&archive, &pending).unwrap();
        let manifest = read_generation_manifest(&destination, &generation_id)
            .unwrap()
            .unwrap();
        let removed = pending.join(&manifest.segments[0].name);
        std::fs::remove_file(&removed).unwrap();
        sync_directory(&pending).unwrap();

        let outcome = recover(&destination).unwrap().unwrap();

        assert!(!outcome.cleanup_pending);
        assert!(!pending.exists());
        assert!(!title.exists());
        assert!(generation_path(&destination, &generation_id)
            .unwrap()
            .join(&manifest.segments[0].name)
            .is_file());
        assert!(recover(&destination).unwrap().is_none());
    }

    #[test]
    fn replaced_candidate_segment_stays_pending_without_quarantine() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, generation_id) = generation(temporary.path(), "candidate");
        publish_candidate_without_cleanup(&archive, &title, &destination, &generation_id);
        let pending = candidate_archive_pending_path(&destination, &generation_id).unwrap();
        crate::instance::rename_without_replacing(&archive, &pending).unwrap();
        let manifest = read_generation_manifest(&destination, &generation_id)
            .unwrap()
            .unwrap();
        let segment_name = &manifest.segments[0].name;
        let replacement = pending.join(segment_name);
        let selected = generation_path(&destination, &generation_id)
            .unwrap()
            .join(segment_name);
        std::fs::remove_file(&replacement).unwrap();
        std::fs::copy(&selected, &replacement).unwrap();
        std::fs::write(pending.join("foreign-sentinel"), b"unknown candidate residue").unwrap();
        let replacement_identity = inspect_file_identity(&replacement).unwrap();
        assert!(!same_regular_file(
            &replacement_identity,
            &manifest.segments[0].identity
        ));

        let outcome = recover(&destination).unwrap().unwrap();

        assert!(outcome.cleanup_pending);
        assert!(outcome.candidate_cleanup_pending);
        assert!(pending.exists());
        assert!(pending.join(segment_name).is_file());
        assert_eq!(std::fs::read(pending.join(segment_name)).unwrap(), std::fs::read(selected).unwrap());
        assert_eq!(
            std::fs::read(pending.join("foreign-sentinel")).unwrap(),
            b"unknown candidate residue"
        );
        assert!(!title.exists());
        assert!(recover(&destination).unwrap().unwrap().candidate_cleanup_pending);
        assert!(pending.join(segment_name).is_file());
    }

    #[test]
    fn replaced_candidate_title_stays_pending_without_quarantine() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, generation_id) = generation(temporary.path(), "candidate");
        publish_candidate_without_cleanup(&archive, &title, &destination, &generation_id);
        std::fs::remove_file(&title).unwrap();
        std::fs::copy(selector_path(&destination), &title).unwrap();

        let outcome = recover(&destination).unwrap().unwrap();

        assert!(outcome.cleanup_pending);
        assert!(outcome.candidate_cleanup_pending);
        assert!(!archive.exists());
        assert!(!title.exists());
        let pending = candidate_title_pending_path(&destination, &generation_id).unwrap();
        assert!(pending.is_file());
        assert_eq!(std::fs::read(&pending).unwrap(), std::fs::read(selector_path(&destination)).unwrap());
        assert!(recover(&destination).unwrap().unwrap().candidate_cleanup_pending);
        assert!(pending.is_file());
    }

    #[test]
    fn candidate_retirement_does_not_mutate_other_build_state() {
        let temporary = tempfile::tempdir().unwrap();
        let scratch = temporary.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, _) = generation(&scratch, "archive");
        for (path, bytes) in [
            (scratch.join("input-cache/source.bz2"), b"source".as_slice()),
            (scratch.join("nodes/content-000000.done/receipt.json"), b"receipt".as_slice()),
            (scratch.join("plan.json"), b"plan".as_slice()),
            (scratch.join("target-logs/content-000000.log"), b"log".as_slice()),
            (scratch.join("foreign-sentinel"), b"unknown".as_slice()),
        ] {
            std::fs::create_dir_all(parent(&path)).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        install(archive, title, &destination).unwrap();

        assert_eq!(std::fs::read(scratch.join("input-cache/source.bz2")).unwrap(), b"source");
        assert_eq!(
            std::fs::read(scratch.join("nodes/content-000000.done/receipt.json")).unwrap(),
            b"receipt"
        );
        assert_eq!(std::fs::read(scratch.join("plan.json")).unwrap(), b"plan");
        assert_eq!(
            std::fs::read(scratch.join("target-logs/content-000000.log")).unwrap(),
            b"log"
        );
        assert_eq!(std::fs::read(scratch.join("foreign-sentinel")).unwrap(), b"unknown");
    }

    #[test]
    fn selector_is_the_only_visibility_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(first_id.as_str())
        );

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        populate_generation(&second_archive, &second_title, &destination, &second_id).unwrap();
        let pending = stage_selector(&second_title, &destination, &second_id).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(first_id.as_str()),
            "generation population and pending selector are invisible"
        );
        let cleanup = vec![first_id.clone()];
        let receipt = PublishReceipt {
            schema: INSTALL_SCHEMA,
            publication_id: publication_id(&second_id, Some(&first_id), &cleanup, None),
            candidate_generation_id: second_id.clone(),
            selected_before_publish: Some(first_id),
            cleanup_generation_ids: cleanup,
            candidate_cleanup: None,
            candidate_manifest: None,
        };
        persist_receipt(&destination, &receipt).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            receipt.selected_before_publish.as_deref(),
            "durable intent is not publication"
        );
        std::fs::rename(pending, selector_path(&destination)).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(second_id.as_str()),
            "one selector rename publishes the complete generation"
        );
    }

    #[test]
    fn recovery_commits_receipted_candidate_but_preserves_unreceipted_candidate() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let (orphan_archive, orphan_title, orphan_id) = generation(temporary.path(), "orphan");
        let orphan =
            populate_generation(&orphan_archive, &orphan_title, &destination, &orphan_id).unwrap();

        let (next_archive, next_title, next_id) = generation(temporary.path(), "next");
        populate_generation(&next_archive, &next_title, &destination, &next_id).unwrap();
        stage_selector(&next_title, &destination, &next_id).unwrap();
        let cleanup = vec![first_id.clone()];
        let receipt = PublishReceipt {
            schema: INSTALL_SCHEMA,
            publication_id: publication_id(&next_id, Some(&first_id), &cleanup, None),
            candidate_generation_id: next_id.clone(),
            selected_before_publish: Some(first_id),
            cleanup_generation_ids: cleanup,
            candidate_cleanup: None,
            candidate_manifest: None,
        };
        persist_receipt(&destination, &receipt).unwrap();

        recover(&destination).unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(next_id.as_str())
        );
        assert!(
            orphan.exists(),
            "cleanup must not infer that every unselected generation is disposable"
        );
    }

    #[test]
    fn old_generation_cleanup_waits_for_reader_lease() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();
        let selected = serving_pair(&destination).unwrap().unwrap();
        let titles = crate::title_index::TitleIndex::open(&selected.title).unwrap();
        let reader = crate::archive::IndexedArchiveSet::open(&selected.archive, &titles).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        let outcome = install(second_archive, second_title, &destination).unwrap();
        assert!(outcome.cleanup_pending);
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(second_id.as_str())
        );
        assert!(generation_path(&destination, &first_id).unwrap().exists());

        drop(reader);
        drop(titles);
        drop(selected);
        assert!(!recover(&destination).unwrap().unwrap().cleanup_pending);
        assert!(!generation_path(&destination, &first_id).unwrap().exists());
    }

    #[test]
    fn old_generation_cleanup_unlinks_owned_segments_and_retains_foreign_residue() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let old_generation = generation_path(&destination, &first_id).unwrap();
        let foreign = old_generation.join("foreign/nested");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("sentinel"), b"user-owned").unwrap();
        // Exercise the legacy cleanup path: the generation payload is valid,
        // but it predates the compact ownership sidecar.
        std::fs::remove_file(generation_manifest_path(&destination, &first_id).unwrap()).unwrap();
        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        let outcome = install(second_archive, second_title, &destination).unwrap();
        assert!(outcome.cleanup_pending);
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(second_id.as_str())
        );
        assert!(old_generation.exists(), "foreign residue keeps generation pending");
        assert!(generation_manifest_path(&destination, &first_id).unwrap().exists());
        assert_eq!(
            std::fs::read(old_generation.join("foreign/nested/sentinel")).unwrap(),
            b"user-owned"
        );
        assert!(recover(&destination).unwrap().unwrap().cleanup_pending);
    }

    #[test]
    fn cleanup_resume_removes_manifest_after_generation_was_already_retired() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        let second_generation =
            populate_generation(&second_archive, &second_title, &destination, &second_id).unwrap();
        let pending = stage_selector(&second_title, &destination, &second_id).unwrap();
        let cleanup = vec![first_id.clone()];
        let receipt = PublishReceipt {
            schema: INSTALL_SCHEMA,
            publication_id: publication_id(&second_id, Some(&first_id), &cleanup, None),
            candidate_generation_id: second_id.clone(),
            selected_before_publish: Some(first_id.clone()),
            cleanup_generation_ids: cleanup,
            candidate_cleanup: None,
            candidate_manifest: None,
        };
        persist_receipt(&destination, &receipt).unwrap();
        std::fs::rename(pending, selector_path(&destination)).unwrap();

        // Simulate the crash cut after all owned segment unlinks and the
        // generation-directory removal, but before the sidecar unlink.
        let old_generation = generation_path(&destination, &first_id).unwrap();
        let old_reader = crate::archive_set::ArchiveSetReader::open(&old_generation).unwrap();
        for segment in old_reader.segments() {
            std::fs::remove_file(old_generation.join(&segment.name)).unwrap();
        }
        std::fs::remove_dir(&old_generation).unwrap();
        assert!(generation_manifest_path(&destination, &first_id).unwrap().exists());

        let outcome = recover(&destination).unwrap().unwrap();
        assert!(!outcome.cleanup_pending);
        assert!(!generation_manifest_path(&destination, &first_id).unwrap().exists());
        assert!(second_generation.is_dir());
        assert!(recover(&destination).unwrap().is_none(), "retry is idempotently complete");
    }

    #[test]
    fn cleanup_resume_reclaims_a_durably_claimed_segment() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        let second_generation =
            populate_generation(&second_archive, &second_title, &destination, &second_id).unwrap();
        let pending = stage_selector(&second_title, &destination, &second_id).unwrap();
        let cleanup = vec![first_id.clone()];
        let receipt = PublishReceipt {
            schema: INSTALL_SCHEMA,
            publication_id: publication_id(&second_id, Some(&first_id), &cleanup, None),
            candidate_generation_id: second_id.clone(),
            selected_before_publish: Some(first_id.clone()),
            cleanup_generation_ids: cleanup,
            candidate_cleanup: None,
            candidate_manifest: None,
        };
        persist_receipt(&destination, &receipt).unwrap();
        std::fs::rename(pending, selector_path(&destination)).unwrap();

        let old_generation = generation_path(&destination, &first_id).unwrap();
        let segment_name = {
            let reader = crate::archive_set::ArchiveSetReader::open(&old_generation).unwrap();
            reader.segments()[0].name.clone()
        };
        std::fs::remove_file(old_generation.join(&segment_name)).unwrap();

        let outcome = recover(&destination).unwrap().unwrap();
        assert!(!outcome.cleanup_pending);
        assert!(!old_generation.exists());
        assert!(second_generation.is_dir());
    }

    #[test]
    fn same_size_generation_replacement_stays_pending() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        populate_generation(&second_archive, &second_title, &destination, &second_id).unwrap();
        let pending = stage_selector(&second_title, &destination, &second_id).unwrap();
        let cleanup = vec![first_id.clone()];
        persist_receipt(
            &destination,
            &PublishReceipt {
                schema: INSTALL_SCHEMA,
                publication_id: publication_id(&second_id, Some(&first_id), &cleanup, None),
                candidate_generation_id: second_id.clone(),
                selected_before_publish: Some(first_id.clone()),
                cleanup_generation_ids: cleanup,
                candidate_cleanup: None,
                candidate_manifest: None,
            },
        )
        .unwrap();
        std::fs::rename(pending, selector_path(&destination)).unwrap();

        let old_generation = generation_path(&destination, &first_id).unwrap();
        let segment_name = {
            let reader = crate::archive_set::ArchiveSetReader::open(&old_generation).unwrap();
            reader.segments()[0].name.clone()
        };
        let segment = old_generation.join(&segment_name);
        let mut replacement = std::fs::read(&segment).unwrap();
        replacement[0] ^= 0xff;
        std::fs::remove_file(&segment).unwrap();
        std::fs::write(&segment, &replacement).unwrap();

        let outcome = recover(&destination).unwrap().unwrap();
        assert!(outcome.cleanup_pending);
        assert!(old_generation.exists());
        assert_eq!(std::fs::read(&segment).unwrap(), replacement);
        assert!(read_receipt(&destination).unwrap().is_some());
    }

    #[test]
    fn malformed_generation_manifest_blocks_cleanup_and_is_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        populate_generation(&second_archive, &second_title, &destination, &second_id).unwrap();
        let pending = stage_selector(&second_title, &destination, &second_id).unwrap();
        let cleanup = vec![first_id.clone()];
        persist_receipt(
            &destination,
            &PublishReceipt {
                schema: INSTALL_SCHEMA,
                publication_id: publication_id(&second_id, Some(&first_id), &cleanup, None),
                candidate_generation_id: second_id.clone(),
                selected_before_publish: Some(first_id.clone()),
                cleanup_generation_ids: cleanup,
                candidate_cleanup: None,
                candidate_manifest: None,
            },
        )
        .unwrap();
        std::fs::rename(pending, selector_path(&destination)).unwrap();

        let manifest = generation_manifest_path(&destination, &first_id).unwrap();
        std::fs::write(&manifest, b"{\"schema\":2}").unwrap();
        let old_generation = generation_path(&destination, &first_id).unwrap();
        let outcome = recover(&destination).unwrap().unwrap();
        assert!(outcome.cleanup_pending);
        assert!(old_generation.exists());
        assert_eq!(std::fs::read(manifest).unwrap(), b"{\"schema\":2}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_generation_segment_is_claimed_without_following_and_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, first_id) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();

        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        populate_generation(&second_archive, &second_title, &destination, &second_id).unwrap();
        let pending = stage_selector(&second_title, &destination, &second_id).unwrap();
        let cleanup = vec![first_id.clone()];
        persist_receipt(
            &destination,
            &PublishReceipt {
                schema: INSTALL_SCHEMA,
                publication_id: publication_id(&second_id, Some(&first_id), &cleanup, None),
                candidate_generation_id: second_id.clone(),
                selected_before_publish: Some(first_id.clone()),
                cleanup_generation_ids: cleanup,
                candidate_cleanup: None,
                candidate_manifest: None,
            },
        )
        .unwrap();
        std::fs::rename(pending, selector_path(&destination)).unwrap();

        let old_generation = generation_path(&destination, &first_id).unwrap();
        let segment_name = {
            let reader = crate::archive_set::ArchiveSetReader::open(&old_generation).unwrap();
            reader.segments()[0].name.clone()
        };
        let segment = old_generation.join(&segment_name);
        let target = temporary.path().join("owned-target");
        std::fs::write(&target, std::fs::read(&segment).unwrap()).unwrap();
        std::fs::remove_file(&segment).unwrap();
        symlink(&target, &segment).unwrap();

        let outcome = recover(&destination).unwrap().unwrap();
        assert!(outcome.cleanup_pending);
        assert!(old_generation.exists());
        assert!(target.exists(), "symlink target was not followed or removed");
        assert_eq!(std::fs::read_link(&segment).unwrap(), target);
    }

    #[test]
    fn selector_open_retries_only_after_selection_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("wiki.swdump");
        let (first_archive, first_title, _) = generation(temporary.path(), "first");
        install(first_archive, first_title, &destination).unwrap();
        let (second_archive, second_title, second_id) = generation(temporary.path(), "second");
        let mut calls = 0;
        let opened = with_serving_pair(&destination, |pair| {
            calls += 1;
            if calls == 1 {
                install(second_archive.clone(), second_title.clone(), &destination).unwrap();
                Err("old generation lost its cleanup race".into())
            } else {
                Ok(pair.archive.clone())
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(opened, generation_path(&destination, &second_id).unwrap());
    }

    #[test]
    fn maintenance_transition_classifies_every_reader_writer_interaction() {
        use UpdateMaintenanceDecision::{Advance, Impossible, NoOp, Reject};
        use UpdateMaintenanceEvent::*;
        use UpdateMaintenancePhase::*;

        let phases = [Available, MarkerPublished, WriterExclusive];
        let events = [
            PublishMarker,
            ExistingReaderObserved,
            AcquireWriter,
            OpenNewReader,
            ProcessCrashed,
            FinishAfterCommit,
        ];
        let marker_required = Reject(UpdateMaintenanceRejection::MarkerRequired);
        let active = Reject(UpdateMaintenanceRejection::MaintenanceActive);
        let commit_required = Reject(UpdateMaintenanceRejection::CommitRequired);
        let reader_exclusive = Impossible(
            UpdateMaintenanceImpossibility::ReaderWhileWriterExclusive,
        );
        let expected = [
            [
                Advance(MarkerPublished),
                NoOp,
                marker_required,
                NoOp,
                NoOp,
                commit_required,
            ],
            [
                NoOp,
                NoOp,
                Advance(WriterExclusive),
                active,
                NoOp,
                commit_required,
            ],
            [
                NoOp,
                reader_exclusive,
                NoOp,
                active,
                Advance(MarkerPublished),
                Advance(Available),
            ],
        ];
        for (phase_index, phase) in phases.into_iter().enumerate() {
            for (event_index, event) in events.into_iter().enumerate() {
                assert_eq!(
                    update_maintenance_transition(phase, event),
                    expected[phase_index][event_index],
                    "{phase:?} + {event:?}"
                );
            }
        }
    }

    #[test]
    fn update_maintenance_begin_blocks_new_serving_and_drop_keeps_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "maintenance-base");
        install(archive, title, &destination).unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"maintenance-new");

        let guard = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "maintenance-update",
        )
        .unwrap();
        assert!(serving_pair(&destination)
            .unwrap_err()
            .contains("serving is unavailable"));
        assert!(update_maintenance_marker_path(&destination).is_file());

        // Drop is the crash/restart path. The marker remains authoritative and
        // keeps serving closed until an exact resumed finish succeeds.
        drop(guard);
        assert!(read_update_maintenance_marker(&destination).unwrap().is_some());
        assert!(serving_pair(&destination).is_err());
    }

    #[test]
    fn preexisting_reader_makes_maintenance_begin_retryable() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "reader-base");
        install(archive, title, &destination).unwrap();
        let generation = generation_path(&destination, &base_id).unwrap();
        let reader = crate::archive_set::ArchiveSetReader::open(&generation).unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"reader-new");

        let error = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "reader-update",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateMaintenanceError::ReadersActive { .. }));
        assert!(update_maintenance_marker_path(&destination).is_file());

        drop(reader);
        let resumed = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "reader-update",
        )
        .unwrap();
        drop(resumed);
    }

    #[test]
    fn serving_reader_crossing_marker_check_pins_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "race-base");
        install(archive, title, &destination).unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"race-new");

        // serving_pair has already crossed its marker check, and its retained
        // shared lease is the synchronization witness for that fact.
        let reader = serving_pair(&destination).unwrap().unwrap();
        let error = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "race-update",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateMaintenanceError::ReadersActive { .. }));
        assert!(serving_pair(&destination).is_err());
        drop(reader);
    }

    #[test]
    fn exact_marker_is_accepted_for_crash_resume() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "resume-base");
        install(archive, title, &destination).unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"resume-new");
        let marker = UpdateMaintenanceMarker {
            schema: UPDATE_MAINTENANCE_SCHEMA,
            base_generation_id: base_id.clone(),
            new_generation_id: new_id.as_str().to_owned(),
            update_id: "resume-update".into(),
        };
        assert!(publish_update_maintenance_marker(&destination, &marker).unwrap());

        let first = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "resume-update",
        )
        .unwrap();
        drop(first);
        let second = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "resume-update",
        )
        .unwrap();
        drop(second);
    }

    #[test]
    fn mismatched_and_malformed_markers_fail_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "marker-base");
        install(archive, title, &destination).unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"marker-new");
        let wrong_id = crate::generation::GenerationId::from_plan_bytes(b"marker-other");
        let marker = UpdateMaintenanceMarker {
            schema: UPDATE_MAINTENANCE_SCHEMA,
            base_generation_id: base_id.clone(),
            new_generation_id: new_id.as_str().to_owned(),
            update_id: "different-update".into(),
        };
        assert!(publish_update_maintenance_marker(&destination, &marker).unwrap());
        let error = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "requested-update",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateMaintenanceError::Invalid(_)));
        assert!(serving_pair(&destination).is_err());

        std::fs::write(
            update_maintenance_marker_path(&destination),
            format!(
                "{{\"schema\":1,\"base_generation_id\":\"{}\"",
                wrong_id.as_str()
            )
            .as_bytes(),
        )
        .unwrap();
        let error = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "different-update",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateMaintenanceError::Invalid(_)));
        assert!(serving_pair(&destination).is_err());
    }

    #[test]
    fn finish_before_selector_commit_rejected_and_marker_survives() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "finish-base");
        install(archive, title, &destination).unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"finish-new");

        let guard = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "finish-update",
        )
        .unwrap();
        let error = guard.finish().unwrap_err();
        assert!(error.contains("cannot finish"));
        assert!(read_update_maintenance_marker(&destination).unwrap().is_some());

        let resumed = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "finish-update",
        )
        .unwrap();
        drop(resumed);
    }

    #[test]
    fn maintenance_replaces_one_preserved_piece_then_reclaims_the_installed_old_link() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "piece-base");
        install(archive, title, &destination).unwrap();
        let installed_generation = generation_path(&destination, &base_id).unwrap();
        let set = crate::archive_set::ArchiveSetReader::open(&installed_generation).unwrap();
        let segment = set
            .segments()
            .iter()
            .find(|segment| segment.kind.is_some())
            .unwrap()
            .clone();
        drop(set);
        let installed = installed_generation.join(&segment.name);
        let preserved_root = temporary.path().join("preserved");
        std::fs::create_dir(&preserved_root).unwrap();
        let preserved = preserved_root.join(&segment.name);
        std::fs::hard_link(&installed, &preserved).unwrap();
        let replacement = temporary.path().join("replacement.part");
        std::fs::write(&replacement, b"replacement piece bytes").unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"piece-new");
        let guard = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "piece-update",
        )
        .unwrap();

        assert!(guard
            .replace_preserved_segment(
                &preserved,
                &segment.name,
                &replacement,
                &segment.name,
            )
            .unwrap());
        assert!(!installed.exists());
        assert!(same_regular_file(
            &inspect_file_identity(&preserved).unwrap(),
            &inspect_file_identity(&replacement).unwrap(),
        ));
        assert!(!guard
            .replace_preserved_segment(
                &preserved,
                &segment.name,
                &replacement,
                &segment.name,
            )
            .unwrap());
    }

    #[test]
    fn maintenance_resume_finishes_a_renamed_piece_swap() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "rename-piece-base");
        install(archive, title, &destination).unwrap();
        let installed_generation = generation_path(&destination, &base_id).unwrap();
        let set = crate::archive_set::ArchiveSetReader::open(&installed_generation).unwrap();
        let segment = set
            .segments()
            .iter()
            .find(|segment| segment.kind.is_some())
            .unwrap()
            .clone();
        drop(set);
        let installed = installed_generation.join(&segment.name);
        let preserved_root = temporary.path().join("preserved-renamed");
        std::fs::create_dir(&preserved_root).unwrap();
        let preserved = preserved_root.join(&segment.name);
        std::fs::hard_link(&installed, &preserved).unwrap();
        let replacement = temporary.path().join("renamed-replacement.part");
        std::fs::write(&replacement, b"renamed replacement piece bytes").unwrap();
        let replacement_name = "1000-p00000000000000000001-p00000000000000000002.swdump-part";
        let selected_replacement = preserved_root.join(replacement_name);
        std::fs::hard_link(&replacement, &selected_replacement).unwrap();
        let new_id = crate::generation::GenerationId::from_plan_bytes(b"rename-piece-new");
        let guard = begin_update_maintenance(
            &destination,
            &base_id,
            new_id.as_str(),
            "rename-piece-update",
        )
        .unwrap();

        assert!(guard
            .replace_preserved_segment(
                &preserved,
                &segment.name,
                &replacement,
                replacement_name,
            )
            .unwrap());
        assert!(!preserved.exists());
        assert!(!installed.exists());
        assert!(selected_replacement.exists());
    }

    #[test]
    fn finish_after_exact_selector_commit_removes_marker_and_resumes_serving() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("installed/wiki.swdump");
        let (archive, title, base_id) = generation(temporary.path(), "commit-base");
        install(archive, title, &destination).unwrap();
        let (next_archive, next_title, new_id) = generation(temporary.path(), "commit-new");

        let guard = begin_update_maintenance(
            &destination,
            &base_id,
            &new_id,
            "commit-update",
        )
        .unwrap();
        let outcome = install(next_archive, next_title, &destination).unwrap();
        assert!(outcome.cleanup_pending);
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(new_id.as_str())
        );

        guard.finish().unwrap();
        assert!(!update_maintenance_marker_path(&destination).exists());
        let selected = serving_pair(&destination).unwrap().unwrap();
        assert_eq!(
            selected_generation(&destination).unwrap().as_deref(),
            Some(new_id.as_str())
        );
        assert_eq!(selected.title, selector_path(&destination));
    }
}
