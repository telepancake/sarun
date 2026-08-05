//! Authoritative read-only interpretation and typed transitions for full builds.

use std::path::{Path, PathBuf};
use std::io::Write;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::direct::{DirectBuildPlan, PartialStats};
use crate::generation::{
    CompressionReferenceIdentity, GenerationId, GenerationIdentity,
};

pub(crate) const RECEIPT_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TargetKind {
    Content,
    History,
}

impl TargetKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::History => "history",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "content" => Some(Self::Content),
            "history" => Some(Self::History),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct StructuralSegment {
    pub(crate) name: String,
    pub(crate) virtual_start: u64,
    pub(crate) bytes: u64,
    pub(crate) role: u8,
    pub(crate) first_id: u64,
    pub(crate) last_id: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TargetReceipt {
    pub(crate) schema: u32,
    pub(crate) target_id: String,
    pub(crate) plan_id: String,
    pub(crate) kind: TargetKind,
    pub(crate) index: usize,
    pub(crate) source_id: String,
    pub(crate) archive_id: String,
    pub(crate) data_bytes: u64,
    pub(crate) siteinfo_bytes: Option<u64>,
    pub(crate) sample_bytes: Option<u64>,
    #[serde(default)]
    pub(crate) stats: PartialStats,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TargetCheckpointFile {
    pub(crate) name: String,
    pub(crate) bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TargetCheckpointReceipt {
    pub(crate) schema: u32,
    pub(crate) plan_id: String,
    pub(crate) kind: TargetKind,
    pub(crate) index: usize,
    pub(crate) source_id: String,
    pub(crate) files: Vec<TargetCheckpointFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AssemblyCheckpointReceipt {
    pub(crate) schema: u32,
    pub(crate) assembly_id: String,
    pub(crate) plan_id: String,
    pub(crate) generation_id: GenerationId,
    pub(crate) compression_reference: Option<CompressionReferenceIdentity>,
    pub(crate) segment_count: u64,
    pub(crate) segment_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ArchiveReceipt {
    pub(crate) schema: u32,
    pub(crate) assembly_id: String,
    pub(crate) plan_id: String,
    pub(crate) generation_id: GenerationId,
    pub(crate) compression_reference: CompressionReferenceIdentity,
    pub(crate) segments: Vec<StructuralSegment>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TitleProjectionReceipt {
    pub(crate) schema: u32,
    pub(crate) assembly_id: String,
    pub(crate) plan_id: String,
    pub(crate) generation_id: GenerationId,
    pub(crate) file_name: String,
    pub(crate) entries: u64,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct GenerationReceipt {
    pub(crate) schema: u32,
    pub(crate) plan_id: String,
    pub(crate) identity: GenerationIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TargetState {
    Missing,
    Partial(TargetCheckpointReceipt),
    Ready(TargetReceipt),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetEvent {
    Begin,
    Checkpoint,
    Publish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetTransition {
    Start,
    Resume,
    Reuse,
    PersistCheckpoint,
    Publish,
}

pub(crate) fn transition_target(
    state: &TargetState,
    event: TargetEvent,
) -> Result<TargetTransition, &'static str> {
    match (state, event) {
        (TargetState::Missing, TargetEvent::Begin) => Ok(TargetTransition::Start),
        (TargetState::Partial(_), TargetEvent::Begin) => Ok(TargetTransition::Resume),
        (TargetState::Ready(_), TargetEvent::Begin) => Ok(TargetTransition::Reuse),
        (TargetState::Missing | TargetState::Partial(_), TargetEvent::Checkpoint) => {
            Ok(TargetTransition::PersistCheckpoint)
        }
        (TargetState::Partial(_), TargetEvent::Publish) => Ok(TargetTransition::Publish),
        (TargetState::Ready(_), TargetEvent::Checkpoint | TargetEvent::Publish) => {
            Err("ready target is immutable")
        }
        (TargetState::Missing, TargetEvent::Publish) => {
            Err("target cannot publish without a durable checkpoint")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssemblyState {
    NotStarted,
    Partial,
    ArchiveRenamed,
    Projecting,
    Ready,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssemblyEvent {
    Begin,
    RetryRequested,
    Checkpoint,
    FinishAndRename,
    CommitArchiveReceipt,
    CommitGeneration,
    CleanupRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssemblyTransition {
    Start,
    Resume,
    PersistCheckpoint,
    RenameArchive,
    RecoverArchiveReceipt,
    CommitArchiveReceipt,
    ResumeProjection,
    CommitGeneration,
    Reuse,
    Cleanup,
}

pub(crate) fn transition_assembly(
    state: AssemblyState,
    event: AssemblyEvent,
) -> Result<AssemblyTransition, &'static str> {
    use AssemblyEvent as Event;
    use AssemblyState as State;
    use AssemblyTransition as Transition;
    match (state, event) {
        (State::NotStarted, Event::Begin | Event::RetryRequested) => Ok(Transition::Start),
        (State::Partial, Event::Begin | Event::RetryRequested) => Ok(Transition::Resume),
        (State::ArchiveRenamed, Event::Begin | Event::RetryRequested) => {
            Ok(Transition::RecoverArchiveReceipt)
        }
        (State::Projecting, Event::Begin | Event::RetryRequested) => {
            Ok(Transition::ResumeProjection)
        }
        (State::Ready, Event::Begin | Event::RetryRequested) => Ok(Transition::Reuse),
        (State::Partial, Event::Checkpoint) => Ok(Transition::PersistCheckpoint),
        (State::Partial, Event::FinishAndRename) => Ok(Transition::RenameArchive),
        (State::ArchiveRenamed, Event::CommitArchiveReceipt) => {
            Ok(Transition::CommitArchiveReceipt)
        }
        (State::Projecting, Event::CommitGeneration) => Ok(Transition::CommitGeneration),
        (State::Ready, Event::CleanupRequested) => Ok(Transition::Cleanup),
        _ => Err("assembly event is invalid in current state"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InspectedTarget {
    pub(crate) kind: TargetKind,
    pub(crate) index: usize,
    pub(crate) state: TargetState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssemblyCheckpoint {
    pub(crate) receipt: AssemblyCheckpointReceipt,
    pub(crate) attempt_tails: Vec<String>,
    pub(crate) unreceipted_segments: Vec<String>,
    pub(crate) unowned_entries: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum BuildState {
    Unplanned,
    Planned {
        plan: DirectBuildPlan,
        targets: Vec<InspectedTarget>,
    },
    ReadyForAssembly {
        plan: DirectBuildPlan,
        targets: Vec<InspectedTarget>,
    },
    Assembling {
        plan: DirectBuildPlan,
        targets: Vec<InspectedTarget>,
        checkpoint: AssemblyCheckpoint,
        title_projection: Option<TitleProjectionReceipt>,
    },
    Projecting {
        plan: DirectBuildPlan,
        archive: ArchiveReceipt,
        title_projection: TitleProjectionReceipt,
    },
    Ready {
        plan: DirectBuildPlan,
        generation: GenerationReceipt,
    },
}

impl BuildState {
    pub(crate) fn plan(&self) -> Option<&DirectBuildPlan> {
        match self {
            Self::Unplanned => None,
            Self::Planned { plan, .. }
            | Self::ReadyForAssembly { plan, .. }
            | Self::Assembling { plan, .. }
            | Self::Projecting { plan, .. }
            | Self::Ready { plan, .. } => Some(plan),
        }
    }

    pub(crate) fn targets(&self) -> &[InspectedTarget] {
        match self {
            Self::Planned { targets, .. }
            | Self::ReadyForAssembly { targets, .. }
            | Self::Assembling { targets, .. } => targets,
            Self::Unplanned | Self::Projecting { .. } | Self::Ready { .. } => &[],
        }
    }

    pub(crate) fn phase(&self) -> &'static str {
        match self {
            Self::Unplanned => "unplanned",
            Self::Planned { .. } => "materializing source targets",
            Self::ReadyForAssembly { .. } => "ready for assembly",
            Self::Assembling { .. } => "assembling",
            Self::Projecting { .. } => "projecting title and frame index",
            Self::Ready { .. } => "ready to install",
        }
    }
}

pub(crate) fn commit_title_projection(
    root: &Path,
    plan: &DirectBuildPlan,
    file_name: &str,
    entries: u64,
    sha256: &str,
) -> Result<TitleProjectionReceipt, InvalidBuildState> {
    let expected_name = format!("title-projection-{sha256}.entries");
    if file_name != expected_name
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            root,
            "title projection name is not its SHA-256 content identity",
        ));
    }
    let path = root.join(file_name);
    let bytes = std::fs::metadata(&path)
        .map_err(|error| {
            invalid(
                InvalidBuildKind::MissingArtifact,
                &path,
                format!("title projection is unavailable: {error}"),
            )
        })?
        .len();
    if bytes % 16 != 0 || bytes / 16 != entries {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &path,
            "title projection entry count does not match its fixed-width file",
        ));
    }
    crate::title_projection::ExternalTitleEntries::open_bound(&path, sha256, entries)
        .map_err(|error| {
            invalid(
                InvalidBuildKind::CorruptArtifact,
                &path,
                error.to_string(),
            )
        })?;
    let receipt = TitleProjectionReceipt {
        schema: RECEIPT_SCHEMA,
        assembly_id: assembly_id(plan),
        plan_id: plan.plan_id.clone(),
        generation_id: GenerationId::from_plan_id(&plan.plan_id),
        file_name: file_name.to_owned(),
        entries,
        bytes,
        sha256: sha256.to_owned(),
    };
    persist_receipt(&root.join("title-projection.receipt.json"), &receipt)?;
    Ok(receipt)
}

fn inspect_title_projection(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<Option<TitleProjectionReceipt>, InvalidBuildState> {
    let receipt_path = root.join("title-projection.receipt.json");
    let receipt: Option<TitleProjectionReceipt> = read_optional(&receipt_path)?;
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    if receipt.schema != RECEIPT_SCHEMA
        || receipt.assembly_id != assembly_id(plan)
        || receipt.plan_id != plan.plan_id
        || receipt.generation_id != GenerationId::from_plan_id(&plan.plan_id)
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &receipt_path,
            "title projection belongs to another plan or assembly",
        ));
    }
    let expected_name = format!("title-projection-{}.entries", receipt.sha256);
    if receipt.file_name != expected_name
        || receipt.sha256.len() != 64
        || !receipt.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &receipt_path,
            "title projection receipt has an invalid content-addressed name",
        ));
    }
    let path = root.join(&receipt.file_name);
    let bytes = std::fs::metadata(&path)
        .map_err(|error| {
            invalid(
                InvalidBuildKind::MissingArtifact,
                &path,
                format!("receipted title projection is unavailable: {error}"),
            )
        })?
        .len();
    if bytes != receipt.bytes || bytes % 16 != 0 || bytes / 16 != receipt.entries {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &path,
            "title projection does not match its receipt",
        ));
    }
    crate::title_projection::ExternalTitleEntries::open_bound(
        &path,
        &receipt.sha256,
        receipt.entries,
    )
        .map_err(|error| {
            invalid(
                InvalidBuildKind::CorruptArtifact,
                &path,
                error.to_string(),
            )
        })?;
    Ok(Some(receipt))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvalidBuildKind {
    Io,
    MalformedReceipt,
    UnsupportedSchema,
    MissingArtifact,
    ForeignIdentity,
    ContradictoryEvidence,
    CorruptArtifact,
    UnsupportedLayout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InvalidBuildState {
    pub(crate) kind: InvalidBuildKind,
    pub(crate) path: PathBuf,
    pub(crate) diagnostic: String,
}

/// Explicit recovery requested for a construction tree whose durable
/// evidence cannot be interpreted.  This is deliberately separate from a
/// normal resume: deleting a partial build is safe only after the inspector
/// has classified it, and never mutates an installed generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvalidBuildEvent {
    AbandonInvalidScratch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvalidBuildTransition {
    AbandonScratch,
}

impl std::fmt::Display for InvalidBuildState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.diagnostic)
    }
}

impl std::error::Error for InvalidBuildState {}

fn invalid(
    kind: InvalidBuildKind,
    path: impl Into<PathBuf>,
    diagnostic: impl Into<String>,
) -> InvalidBuildState {
    InvalidBuildState {
        kind,
        path: path.into(),
        diagnostic: diagnostic.into(),
    }
}

/// Decide whether an invalid construction tree may be explicitly abandoned.
///
/// I/O failures and contradictory evidence are not recoverable by deletion:
/// the inspector cannot establish what durable state is present.  A malformed
/// or foreign temporary receipt, on the other hand, can be discarded when no
/// complete candidate archive has been committed in the tree.  The caller
/// still owns the destination-local lock and performs the actual deletion.
pub(crate) fn transition_invalid_build(
    root: &Path,
    state: &InvalidBuildState,
    event: InvalidBuildEvent,
) -> Result<InvalidBuildTransition, InvalidBuildState> {
    if !matches!(event, InvalidBuildEvent::AbandonInvalidScratch) {
        return Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            "unsupported invalid-build recovery event",
        ));
    }
    if matches!(
        state.kind,
        InvalidBuildKind::Io
            | InvalidBuildKind::MissingArtifact
            | InvalidBuildKind::CorruptArtifact
            | InvalidBuildKind::ContradictoryEvidence
    ) {
        return Err(invalid(
            state.kind.clone(),
            state.path.clone(),
            format!(
                "cannot abandon ambiguous build state: {}",
                state.diagnostic
            ),
        ));
    }
    // A complete candidate has its own archive/index/receipt evidence.  It is
    // a recoverable construction result, not disposable scratch.
    const CANDIDATE_FILES: [&str; 4] = [
        "archive.swdump",
        "archive.swtitle",
        "archive.receipt.json",
        "archive.generation.json",
    ];
    if CANDIDATE_FILES
        .iter()
        .any(|name| root.join(name).exists())
    {
        return Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            "invalid build state has a candidate archive; preserve it for explicit repair",
        ));
    }
    Ok(InvalidBuildTransition::AbandonScratch)
}

fn read_optional<T: DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, InvalidBuildState> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(invalid(
                InvalidBuildKind::Io,
                path,
                format!("cannot read receipt: {error}"),
            ))
        }
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        invalid(
            InvalidBuildKind::MalformedReceipt,
            path,
            format!("malformed receipt: {error}"),
        )
    })
}

fn sync_directory(path: &Path) -> Result<(), InvalidBuildState> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                invalid(
                    InvalidBuildKind::Io,
                    path,
                    format!("cannot sync receipt directory: {error}"),
                )
            })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub(crate) fn persist_receipt<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), InvalidBuildState> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| {
        invalid(
            InvalidBuildKind::Io,
            parent,
            format!("cannot create receipt directory: {error}"),
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        invalid(
            InvalidBuildKind::Io,
            parent,
            format!("cannot create receipt temporary: {error}"),
        )
    })?;
    serde_json::to_writer(&mut temporary, value).map_err(|error| {
        invalid(
            InvalidBuildKind::MalformedReceipt,
            path,
            format!("cannot encode receipt: {error}"),
        )
    })?;
    temporary.write_all(b"\n").map_err(|error| {
        invalid(
            InvalidBuildKind::Io,
            temporary.path(),
            error.to_string(),
        )
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        invalid(
            InvalidBuildKind::Io,
            temporary.path(),
            error.to_string(),
        )
    })?;
    temporary.persist(path).map_err(|error| {
        invalid(
            InvalidBuildKind::Io,
            path,
            error.error.to_string(),
        )
    })?;
    sync_directory(parent)
}

/// Commit a newly discovered immutable plan.
///
/// Cost: one small receipt write and two fsync operations; no archive bytes,
/// source bytes, network requests, or directory walks.
pub(crate) fn commit_plan(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<BuildState, InvalidBuildState> {
    match inspect_build(root, None)? {
        BuildState::Unplanned => {}
        state => {
            return Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root.join("plan.json"),
                format!("cannot commit a plan while build is {}", state.phase()),
            ))
        }
    }
    if plan.plan_id
        != crate::direct::canonical_direct_plan_id(plan).map_err(|error| {
            invalid(
                InvalidBuildKind::MalformedReceipt,
                root.join("plan.json"),
                error.to_string(),
            )
        })?
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            root.join("plan.json"),
            "plan ID does not match its canonical encoding",
        ));
    }
    persist_receipt(&root.join("plan.json"), plan)?;
    inspect_build(root, Some(&plan.plan_id))
}

pub(crate) fn target_path(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
) -> Result<PathBuf, InvalidBuildState> {
    let name = plan
        .target_name(kind.as_str(), index)
        .ok_or_else(|| {
            invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root.join("plan.json"),
                format!("plan does not contain {} target {index}", kind.as_str()),
            )
        })?;
    Ok(root.join("nodes").join(format!("{name}.done")))
}

pub(crate) fn target_partial_path(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
) -> Result<PathBuf, InvalidBuildState> {
    let name = plan
        .target_name(kind.as_str(), index)
        .ok_or_else(|| {
            invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root.join("plan.json"),
                format!("plan does not contain {} target {index}", kind.as_str()),
            )
        })?;
    Ok(root.join("nodes").join(format!("{name}.partial")))
}

fn target_source_id(
    plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
) -> Result<String, InvalidBuildState> {
    let bytes = plan
        .target_source_identity(kind.as_str(), index)
        .map_err(|error| {
            invalid(
                InvalidBuildKind::ContradictoryEvidence,
                PathBuf::from("plan.json"),
                error.to_string(),
            )
        })?;
    Ok(bytes)
}

pub(crate) fn target_frame_directory_identity(
    plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
) -> Result<[u8; 32], InvalidBuildState> {
    let identity = (
        "wikipedia-target-frame-directory",
        &plan.plan_id,
        kind,
        index,
        target_source_id(plan, kind, index)?,
    );
    Ok(sha2::Sha256::digest(
        serde_json::to_vec(&identity).expect("frame-directory identity is serializable"),
    )
    .into())
}

fn target_archive_id(
    plan_id: &str,
    kind: TargetKind,
    index: usize,
    source_id: &str,
    data_bytes: u64,
    siteinfo_bytes: Option<u64>,
    sample_bytes: Option<u64>,
) -> String {
    let identity = (
        "wikipedia-target-archive",
        plan_id,
        kind,
        index,
        source_id,
        data_bytes,
        siteinfo_bytes,
        sample_bytes,
    );
    hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&identity).expect("target identity is serializable"),
    ))
}

fn target_id(receipt: &TargetReceipt) -> String {
    let identity = (
        "wikipedia-target",
        &receipt.plan_id,
        receipt.kind,
        receipt.index,
        &receipt.source_id,
        &receipt.archive_id,
    );
    hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&identity).expect("target identity is serializable"),
    ))
}

pub(crate) fn make_target_receipt(
    plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
    data_bytes: u64,
    siteinfo_bytes: Option<u64>,
    sample_bytes: Option<u64>,
    stats: PartialStats,
) -> Result<TargetReceipt, InvalidBuildState> {
    let source_id = target_source_id(plan, kind, index)?;
    let archive_id = target_archive_id(
        &plan.plan_id,
        kind,
        index,
        &source_id,
        data_bytes,
        siteinfo_bytes,
        sample_bytes,
    );
    let mut receipt = TargetReceipt {
        schema: RECEIPT_SCHEMA,
        target_id: String::new(),
        plan_id: plan.plan_id.clone(),
        kind,
        index,
        source_id,
        archive_id,
        data_bytes,
        siteinfo_bytes,
        sample_bytes,
        stats,
    };
    receipt.target_id = target_id(&receipt);
    Ok(receipt)
}

pub(crate) fn make_target_checkpoint(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
) -> Result<TargetCheckpointReceipt, InvalidBuildState> {
    let partial = target_partial_path(root, plan, kind, index)?;
    let mut files = std::fs::read_dir(&partial)
        .map_err(|error| {
            invalid(
                InvalidBuildKind::Io,
                &partial,
                format!("cannot inspect target checkpoint: {error}"),
            )
        })?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.ends_with(".swdump")
                && !name.ends_with(".swframe")
                && !name.ends_with(".samples")
                && !name.ends_with(".receipt.json")
            {
                return None;
            }
            entry.metadata().ok().filter(|metadata| metadata.is_file()).map(
                |metadata| TargetCheckpointFile {
                    name,
                    bytes: metadata.len(),
                },
            )
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    if files.is_empty() {
        return Err(invalid(
            InvalidBuildKind::MissingArtifact,
            &partial,
            "target has no independently durable checkpoint files",
        ));
    }
    for file in files.iter().filter(|file| file.name.ends_with(".swdump")) {
        let path = partial.join(&file.name);
        if !crate::archive::has_clean_completion_marker(&path).map_err(|error| {
            invalid(
                InvalidBuildKind::CorruptArtifact,
                &path,
                error.to_string(),
            )
        })? {
            return Err(invalid(
                InvalidBuildKind::CorruptArtifact,
                &path,
                "target checkpoint archive has no clean completion marker",
            ));
        }
    }
    Ok(TargetCheckpointReceipt {
        schema: RECEIPT_SCHEMA,
        plan_id: plan.plan_id.clone(),
        kind,
        index,
        source_id: target_source_id(plan, kind, index)?,
        files,
    })
}

fn inspect_target(
    root: &Path,
    plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
) -> Result<InspectedTarget, InvalidBuildState> {
    let node = target_path(root, plan, kind, index)?;
    let receipt_path = node.join("receipt.json");
    if !node.exists() {
        let partial = target_partial_path(root, plan, kind, index)?;
        let checkpoint_path = partial.join("checkpoint.json");
        if partial.is_dir() && checkpoint_path.exists() {
            let receipt: TargetCheckpointReceipt =
                read_optional(&checkpoint_path)?.ok_or_else(|| {
                    invalid(
                        InvalidBuildKind::MissingArtifact,
                        &checkpoint_path,
                        "target checkpoint receipt disappeared",
                    )
                })?;
            let observed = make_target_checkpoint(root, plan, kind, index)?;
            if receipt != observed {
                return Err(invalid(
                    InvalidBuildKind::ForeignIdentity,
                    &checkpoint_path,
                    "target checkpoint inventory or identity does not match",
                ));
            }
            return Ok(InspectedTarget {
                kind,
                index,
                state: TargetState::Partial(receipt),
            });
        }
        return Ok(InspectedTarget {
            kind,
            index,
            state: TargetState::Missing,
        });
    }
    if !node.is_dir() {
        return Err(invalid(
            InvalidBuildKind::UnsupportedLayout,
            &node,
            "target path is not a directory",
        ));
    }
    let receipt: TargetReceipt = read_optional(&receipt_path)?.ok_or_else(|| {
        invalid(
            InvalidBuildKind::MissingArtifact,
            &receipt_path,
            "target directory has no receipt",
        )
    })?;
    if receipt.schema != RECEIPT_SCHEMA {
        return Err(invalid(
            InvalidBuildKind::UnsupportedSchema,
            &receipt_path,
            format!("unsupported target receipt schema {}", receipt.schema),
        ));
    }
    let expected_source = target_source_id(plan, kind, index)?;
    if receipt.plan_id != plan.plan_id
        || receipt.kind != kind
        || receipt.index != index
        || receipt.source_id != expected_source
        || receipt.archive_id
            != target_archive_id(
                &plan.plan_id,
                kind,
                index,
                &expected_source,
                receipt.data_bytes,
                receipt.siteinfo_bytes,
                receipt.sample_bytes,
            )
        || receipt.target_id != target_id(&receipt)
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &receipt_path,
            "target receipt does not identify this plan target",
        ));
    }
    let data = node.join("data.swdump");
    let bytes = std::fs::metadata(&data)
        .map_err(|error| {
            invalid(
                InvalidBuildKind::MissingArtifact,
                &data,
                format!("target archive is unavailable: {error}"),
            )
        })?
        .len();
    if bytes != receipt.data_bytes
        || !crate::archive::has_clean_completion_marker(&data).map_err(|error| {
            invalid(
                InvalidBuildKind::CorruptArtifact,
                &data,
                error.to_string(),
            )
        })?
    {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &data,
            "target archive does not match its receipt",
        ));
    }
    let frame_directory_path = node.join("data.swframe");
    let frame_directory = crate::frame_directory::FrameDirectory::open_bound(
        &frame_directory_path,
        target_frame_directory_identity(plan, kind, index)?,
    )
    .map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &frame_directory_path,
            error.to_string(),
        )
    })?;
    frame_directory.require_archive_bounds(bytes).map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &frame_directory_path,
            error.to_string(),
        )
    })?;
    let title_records = node.join("title-records.swdump");
    if !crate::archive::has_clean_completion_marker(&title_records).map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &title_records,
            error.to_string(),
        )
    })? {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &title_records,
            "target title-record sidecar has no clean completion marker",
        ));
    }
    let observed_siteinfo = if kind == TargetKind::Content && index == 0 {
        let siteinfo = node.join("siteinfo.swdump");
        let bytes = std::fs::metadata(&siteinfo)
            .map_err(|error| {
                invalid(
                    InvalidBuildKind::MissingArtifact,
                    &siteinfo,
                    format!("siteinfo archive is unavailable: {error}"),
                )
            })?
            .len();
        if !crate::archive::has_clean_completion_marker(&siteinfo).map_err(|error| {
            invalid(
                InvalidBuildKind::CorruptArtifact,
                &siteinfo,
                error.to_string(),
            )
        })? {
            return Err(invalid(
                InvalidBuildKind::CorruptArtifact,
                &siteinfo,
                "siteinfo archive has no clean completion marker",
            ));
        }
        Some(bytes)
    } else {
        None
    };
    if observed_siteinfo != receipt.siteinfo_bytes {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &receipt_path,
            "target siteinfo identity does not match its receipt",
        ));
    }
    let observed_samples = if kind == TargetKind::Content {
        let samples = node.join("newest.samples");
        Some(
            std::fs::metadata(&samples)
                .map_err(|error| {
                    invalid(
                        InvalidBuildKind::MissingArtifact,
                        &samples,
                        format!("newest-revision samples are unavailable: {error}"),
                    )
                })?
                .len(),
        )
    } else {
        None
    };
    if observed_samples != receipt.sample_bytes {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &receipt_path,
            "target sample inventory does not match its receipt",
        ));
    }
    Ok(InspectedTarget {
        kind,
        index,
        state: TargetState::Ready(receipt),
    })
}

/// Inspect exactly one source target before a worker mutates it.
///
/// This is the worker-side authority. It validates the durable plan identity,
/// the requested target, and the constant-size set of artifacts that close
/// source materialization. It deliberately does not inspect any sibling
/// target. Across N workers the target receipt/stat cost is therefore O(N),
/// rather than O(N²).
pub(crate) fn inspect_target_for_materialization(
    root: &Path,
    supplied_plan: &DirectBuildPlan,
    kind: TargetKind,
    index: usize,
) -> Result<InspectedTarget, InvalidBuildState> {
    let plan_path = root.join("plan.json");
    let durable_plan = crate::direct::read_direct_build_plan(&plan_path).map_err(|error| {
        invalid(
            InvalidBuildKind::MalformedReceipt,
            &plan_path,
            error.to_string(),
        )
    })?;
    if durable_plan.plan_id != supplied_plan.plan_id
        || durable_plan.plan_id
            != crate::direct::canonical_direct_plan_id(&durable_plan).map_err(|error| {
                invalid(
                    InvalidBuildKind::MalformedReceipt,
                    &plan_path,
                    error.to_string(),
                )
            })?
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &plan_path,
            "worker plan does not identify the durable full-build plan",
        ));
    }
    for name in [
        "assembly.partial",
        "assembly.checkpoint.json",
        "archive.swdump",
        "archive.receipt.json",
        "archive.swtitle",
        "archive.generation.json",
    ] {
        let path = root.join(name);
        if path.exists() {
            return Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                path,
                "source target cannot be materialized after assembly has started",
            ));
        }
    }
    inspect_target(root, &durable_plan, kind, index)
}

fn structural_segments(
    segments: &[crate::archive_set::ArchiveSetSegment],
) -> Vec<StructuralSegment> {
    segments
        .iter()
        .map(|segment| StructuralSegment {
            name: segment.name.clone(),
            virtual_start: segment.virtual_start,
            bytes: segment.bytes,
            role: match segment.kind {
                Some(crate::archive::EntityKind::Page) => 1,
                Some(crate::archive::EntityKind::User) => 2,
                Some(crate::archive::EntityKind::Global) => 3,
                None if segment.name.starts_with("0000-") => 0,
                None if segment.name.starts_with("9999-") => 4,
                None => u8::MAX,
            },
            first_id: segment.first_id,
            last_id: segment.last_id,
        })
        .collect()
}

fn update_segment_digest(hasher: &mut sha2::Sha256, segment: &StructuralSegment) {
    let bytes = serde_json::to_vec(segment).expect("structural segment is serializable");
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn segment_digest(segments: &[StructuralSegment]) -> String {
    let mut hasher = sha2::Sha256::new();
    for segment in segments {
        update_segment_digest(&mut hasher, segment);
    }
    hex::encode(hasher.finalize())
}

pub(crate) fn assembly_id(plan: &DirectBuildPlan) -> String {
    let generation_id = GenerationId::from_plan_id(&plan.plan_id);
    let identity = (
        "wikipedia-full-assembly",
        &plan.plan_id,
        generation_id.as_str(),
        plan.frame_target,
        plan.range_target,
        plan.compression_level,
        plan.ref_prefix_sample_bytes,
        plan.ref_prefix_bytes,
    );
    hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&identity).expect("assembly identity is serializable"),
    ))
}

pub(crate) fn make_assembly_checkpoint(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<AssemblyCheckpointReceipt, InvalidBuildState> {
    let assembly = root.join("assembly.partial");
    let inspected = crate::archive_set::inspect_partial_archive_set(&assembly).map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &assembly,
            error.to_string(),
        )
    })?;
    let compression_reference = if inspected.segments.is_empty() {
        None
    } else {
        Some(
            crate::archive::archive_compression_reference_identity(&assembly).map_err(|error| {
                invalid(
                    InvalidBuildKind::CorruptArtifact,
                    &assembly,
                    error.to_string(),
                )
            })?,
        )
    };
    Ok(AssemblyCheckpointReceipt {
        schema: RECEIPT_SCHEMA,
        assembly_id: assembly_id(plan),
        plan_id: plan.plan_id.clone(),
        generation_id: GenerationId::from_plan_id(&plan.plan_id),
        compression_reference,
        segment_count: inspected.segments.len() as u64,
        segment_digest: segment_digest(&structural_segments(&inspected.segments)),
    })
}

fn inspect_assembly(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<Option<AssemblyCheckpoint>, InvalidBuildState> {
    let assembly = root.join("assembly.partial");
    let receipt_path = root.join("assembly.checkpoint.json");
    let receipt: Option<AssemblyCheckpointReceipt> = read_optional(&receipt_path)?;
    if !assembly.exists() && receipt.is_none() {
        return Ok(None);
    }
    if !assembly.is_dir() {
        return Err(invalid(
            InvalidBuildKind::MissingArtifact,
            &assembly,
            "assembly receipt exists without its partial archive directory",
        ));
    }
    let receipt = receipt.ok_or_else(|| {
        invalid(
            InvalidBuildKind::MissingArtifact,
            &receipt_path,
            "partial assembly has no checkpoint receipt",
        )
    })?;
    if receipt.schema != RECEIPT_SCHEMA
        || receipt.plan_id != plan.plan_id
        || receipt.assembly_id != assembly_id(plan)
        || receipt.generation_id != GenerationId::from_plan_id(&plan.plan_id)
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &receipt_path,
            "assembly checkpoint belongs to another plan or assembly",
        ));
    }
    let inspected = crate::archive_set::inspect_partial_archive_set(&assembly).map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &assembly,
            error.to_string(),
        )
    })?;
    let observed_segments = structural_segments(&inspected.segments);
    let receipt_count =
        usize::try_from(receipt.segment_count).map_err(|_| {
            invalid(
                InvalidBuildKind::CorruptArtifact,
                &receipt_path,
                "assembly checkpoint segment count is too large",
            )
        })?;
    if receipt_count > observed_segments.len()
        || receipt.segment_digest != segment_digest(&observed_segments[..receipt_count])
        || (receipt_count != 0
            && receipt.compression_reference
                != Some(
                    crate::archive::archive_compression_reference_identity(&assembly).map_err(
                        |error| {
                            invalid(
                                InvalidBuildKind::CorruptArtifact,
                                &assembly,
                                error.to_string(),
                            )
                        },
                    )?,
                ))
    {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &receipt_path,
            "receipted assembly prefix does not match sealed ranges",
        ));
    }
    let unreceipted_segments = inspected.segments[receipt_count..]
        .iter()
        .map(|segment| segment.name.clone())
        .collect();
    Ok(Some(AssemblyCheckpoint {
        receipt,
        attempt_tails: inspected.attempt_tails,
        unreceipted_segments,
        unowned_entries: inspected.unowned_entries,
    }))
}

/// Enter or explicitly resume assembly from a validated state.
///
/// Resume removes only attempt tails and sealed segments newer than the last
/// receipt.  Cost is O(number of assembly directory entries), with no source
/// or archive payload reads and O(1) open descriptors.
pub(crate) fn prepare_assembly(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<(), InvalidBuildState> {
    match inspect_build(root, Some(&plan.plan_id))? {
        BuildState::ReadyForAssembly { .. } => {
            if transition_assembly(AssemblyState::NotStarted, AssemblyEvent::Begin)
                != Ok(AssemblyTransition::Start)
            {
                return Err(invalid(
                    InvalidBuildKind::ContradictoryEvidence,
                    root,
                    "assembly state table rejected a new assembly",
                ));
            }
            let assembly = root.join("assembly.partial");
            std::fs::create_dir(&assembly).map_err(|error| {
                invalid(
                    InvalidBuildKind::Io,
                    &assembly,
                    format!("cannot create assembly checkpoint: {error}"),
                )
            })?;
            let receipt = make_assembly_checkpoint(root, plan)?;
            persist_receipt(&root.join("assembly.checkpoint.json"), &receipt)
        }
        BuildState::Assembling { checkpoint, .. } => {
            if transition_assembly(AssemblyState::Partial, AssemblyEvent::Begin)
                != Ok(AssemblyTransition::Resume)
            {
                return Err(invalid(
                    InvalidBuildKind::ContradictoryEvidence,
                    root,
                    "assembly state table rejected a resumable assembly",
                ));
            }
            if !checkpoint.unowned_entries.is_empty() {
                return Err(invalid(
                    InvalidBuildKind::UnsupportedLayout,
                    root.join("assembly.partial"),
                    format!(
                        "assembly contains unowned entries: {}",
                        checkpoint.unowned_entries.join(", ")
                    ),
                ));
            }
            for name in checkpoint
                .attempt_tails
                .iter()
                .chain(checkpoint.unreceipted_segments.iter())
            {
                let path = root.join("assembly.partial").join(name);
                std::fs::remove_file(&path).map_err(|error| {
                    invalid(
                        InvalidBuildKind::Io,
                        &path,
                        format!("cannot discard uncommitted assembly output: {error}"),
                    )
                })?;
            }
            sync_directory(&root.join("assembly.partial"))?;
            let observed = make_assembly_checkpoint(root, plan)?;
            if observed != checkpoint.receipt {
                return Err(invalid(
                    InvalidBuildKind::CorruptArtifact,
                    root.join("assembly.checkpoint.json"),
                    "assembly changed while preparing its resume boundary",
                ));
            }
            Ok(())
        }
        state => Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            format!("cannot begin assembly while build is {}", state.phase()),
        )),
    }
}

/// Publish the exact currently sealed assembly prefix.
///
/// Cost is one directory inventory plus bounded reference-header reads and one
/// small receipt write; it never decodes revision payloads.
pub(crate) struct AssemblyCheckpointTracker {
    count: usize,
    digest: sha2::Sha256,
}

impl AssemblyCheckpointTracker {
    pub(crate) fn new(segments: &[crate::archive_set::ArchiveSetSegment]) -> Self {
        let structural = structural_segments(segments);
        let mut digest = sha2::Sha256::new();
        for segment in &structural {
            update_segment_digest(&mut digest, segment);
        }
        Self {
            count: structural.len(),
            digest,
        }
    }

    /// Persist a constant-size receipt after hashing only newly sealed ranges.
    ///
    /// Across R range seals this writes O(R) receipt bytes and hashes each
    /// structural segment once. Resume validation may scan the R filenames
    /// once; normal per-seal work is O(1).
    pub(crate) fn checkpoint(
        &mut self,
        root: &Path,
        plan: &DirectBuildPlan,
        segments: &[crate::archive_set::ArchiveSetSegment],
    ) -> Result<AssemblyCheckpointReceipt, InvalidBuildState> {
        if transition_assembly(AssemblyState::Partial, AssemblyEvent::Checkpoint)
            != Ok(AssemblyTransition::PersistCheckpoint)
        {
            return Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root,
                "assembly state table rejected checkpoint publication",
            ));
        }
        if segments.len() < self.count {
            return Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root.join("assembly.partial"),
                "sealed assembly segment inventory moved backwards",
            ));
        }
        let new = structural_segments(&segments[self.count..]);
        for segment in &new {
            update_segment_digest(&mut self.digest, segment);
        }
        self.count = segments.len();
        let compression_reference = if self.count == 0 {
            None
        } else {
            Some(
                crate::archive::archive_compression_reference_identity(
                    root.join("assembly.partial"),
                )
                .map_err(|error| {
                    invalid(
                        InvalidBuildKind::CorruptArtifact,
                        root.join("assembly.partial"),
                        error.to_string(),
                    )
                })?,
            )
        };
        let receipt = AssemblyCheckpointReceipt {
            schema: RECEIPT_SCHEMA,
            assembly_id: assembly_id(plan),
            plan_id: plan.plan_id.clone(),
            generation_id: GenerationId::from_plan_id(&plan.plan_id),
            compression_reference,
            segment_count: self.count as u64,
            segment_digest: hex::encode(self.digest.clone().finalize()),
        };
        persist_receipt(&root.join("assembly.checkpoint.json"), &receipt)?;
        Ok(receipt)
    }
}

pub(crate) fn make_archive_receipt(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<ArchiveReceipt, InvalidBuildState> {
    let archive = root.join("archive.swdump");
    let set = crate::archive_set::ArchiveSetReader::open(&archive).map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &archive,
            error.to_string(),
        )
    })?;
    Ok(ArchiveReceipt {
        schema: RECEIPT_SCHEMA,
        assembly_id: assembly_id(plan),
        plan_id: plan.plan_id.clone(),
        generation_id: GenerationId::from_plan_id(&plan.plan_id),
        compression_reference:
            crate::archive::archive_compression_reference_identity(&archive).map_err(|error| {
                invalid(
                    InvalidBuildKind::CorruptArtifact,
                    &archive,
                    error.to_string(),
                )
            })?,
        segments: structural_segments(set.segments()),
    })
}

/// Commit a complete archive after its assembly directory was atomically
/// renamed to `archive.swdump`.
///
/// Cost is O(segment count) metadata plus the compression-reference header;
/// no revision payload is read.
pub(crate) fn commit_archive(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<ArchiveReceipt, InvalidBuildState> {
    if transition_assembly(
        AssemblyState::ArchiveRenamed,
        AssemblyEvent::CommitArchiveReceipt,
    ) != Ok(AssemblyTransition::CommitArchiveReceipt)
    {
        return Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            "assembly state table rejected archive receipt publication",
        ));
    }
    let receipt = make_archive_receipt(root, plan)?;
    persist_receipt(&root.join("archive.receipt.json"), &receipt)?;
    let checkpoint = root.join("assembly.checkpoint.json");
    match std::fs::remove_file(&checkpoint) {
        Ok(()) => sync_directory(root)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(invalid(
                InvalidBuildKind::Io,
                checkpoint,
                format!("cannot retire assembly checkpoint: {error}"),
            ))
        }
    }
    match inspect_build(root, Some(&plan.plan_id))? {
        BuildState::Projecting { archive, .. } if archive == receipt => Ok(receipt),
        state => Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            format!("archive commit produced unexpected state {}", state.phase()),
        )),
    }
}

/// Recover only the interruption window between the atomic assembly-directory
/// rename and the archive-receipt commit.  No other unreceipted archive is
/// adopted.
pub(crate) fn recover_archive_commit(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<Option<ArchiveReceipt>, InvalidBuildState> {
    let archive = root.join("archive.swdump");
    let archive_kind = std::fs::symlink_metadata(&archive);
    match archive_kind {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(invalid(
                InvalidBuildKind::Io,
                &archive,
                format!("cannot inspect candidate archive: {error}"),
            ))
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid(
                InvalidBuildKind::UnsupportedLayout,
                &archive,
                "candidate archive is not an archive-set directory",
            ))
        }
        Ok(_) => {}
    }
    if root.join("archive.receipt.json").is_file() {
        if root.join("assembly.checkpoint.json").is_file() {
            std::fs::remove_file(root.join("assembly.checkpoint.json")).map_err(|error| {
                invalid(
                    InvalidBuildKind::Io,
                    root.join("assembly.checkpoint.json"),
                    format!("cannot retire committed assembly checkpoint: {error}"),
                )
            })?;
            sync_directory(root)?;
        }
        return match inspect_build(root, Some(&plan.plan_id))? {
            BuildState::Projecting { archive, .. } => Ok(Some(archive)),
            BuildState::Ready { .. } => Ok(Some(make_archive_receipt(root, plan)?)),
            state => Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root,
                format!("archive receipt recovery observed {}", state.phase()),
            )),
        };
    }
    if transition_assembly(AssemblyState::ArchiveRenamed, AssemblyEvent::RetryRequested)
        != Ok(AssemblyTransition::RecoverArchiveReceipt)
    {
        return Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            "assembly state table rejected archive receipt recovery",
        ));
    }
    let checkpoint_path = root.join("assembly.checkpoint.json");
    let checkpoint: AssemblyCheckpointReceipt = read_optional(&checkpoint_path)?
        .ok_or_else(|| {
            invalid(
                InvalidBuildKind::MissingArtifact,
                &archive,
                "unreceipted archive has no assembly checkpoint",
            )
        })?;
    if checkpoint.schema != RECEIPT_SCHEMA
        || checkpoint.plan_id != plan.plan_id
        || checkpoint.assembly_id != assembly_id(plan)
        || checkpoint.generation_id != GenerationId::from_plan_id(&plan.plan_id)
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &checkpoint_path,
            "assembly checkpoint does not authorize this archive commit",
        ));
    }
    let observed = make_archive_receipt(root, plan)?;
    let non_completion = observed
        .segments
        .iter()
        .filter(|segment| segment.role != 4)
        .cloned()
        .collect::<Vec<_>>();
    if checkpoint.segment_count != non_completion.len() as u64
        || checkpoint.segment_digest != segment_digest(&non_completion)
        || checkpoint.compression_reference.as_ref()
            != Some(&observed.compression_reference)
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &archive,
            "completed archive does not match its last assembly checkpoint",
        ));
    }
    commit_archive(root, plan).map(Some)
}

/// Commit the generation only after archive and index mutually validate.
///
/// Cost is O(index bytes + compression reference header + bounded compressed
/// global metadata), O(1) descriptors, bounded memory, and zero network.
pub(crate) fn commit_generation(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<GenerationReceipt, InvalidBuildState> {
    if transition_assembly(AssemblyState::Projecting, AssemblyEvent::CommitGeneration)
        != Ok(AssemblyTransition::CommitGeneration)
    {
        return Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            "assembly state table rejected generation publication",
        ));
    }
    let archive = root.join("archive.swdump");
    let titles = root.join("archive.swtitle");
    let identity = crate::generation::generation_identity(&archive, &titles).map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &titles,
            error.to_string(),
        )
    })?;
    if identity.generation_id != GenerationId::from_plan_id(&plan.plan_id)
        || identity.wiki_db != plan.wiki_db
        || identity.content_frontier != plan.content_snapshot
        || identity.metadata_frontier != plan.metadata_snapshot
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &titles,
            "projected generation does not match its full-build plan",
        ));
    }
    let receipt = GenerationReceipt {
        schema: RECEIPT_SCHEMA,
        plan_id: plan.plan_id.clone(),
        identity,
    };
    persist_receipt(&root.join("archive.generation.json"), &receipt)?;
    match inspect_build(root, Some(&plan.plan_id))? {
        BuildState::Ready { generation, .. } if generation.identity == receipt.identity => {
            Ok(receipt)
        }
        state => Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            format!("generation commit produced unexpected state {}", state.phase()),
        )),
    }
}

fn inspect_complete_archive(
    root: &Path,
    plan: &DirectBuildPlan,
) -> Result<Option<ArchiveReceipt>, InvalidBuildState> {
    let archive = root.join("archive.swdump");
    let receipt_path = root.join("archive.receipt.json");
    let receipt: Option<ArchiveReceipt> = read_optional(&receipt_path)?;
    if !archive.exists() && receipt.is_none() {
        return Ok(None);
    }
    if !archive.exists() {
        return Err(invalid(
            InvalidBuildKind::MissingArtifact,
            &archive,
            "archive receipt exists without its archive",
        ));
    }
    let receipt = receipt.ok_or_else(|| {
        invalid(
            InvalidBuildKind::MissingArtifact,
            &receipt_path,
            "complete archive has no archive receipt",
        )
    })?;
    if receipt.schema != RECEIPT_SCHEMA
        || receipt.plan_id != plan.plan_id
        || receipt.assembly_id != assembly_id(plan)
        || receipt.generation_id != GenerationId::from_plan_id(&plan.plan_id)
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &receipt_path,
            "archive receipt belongs to another assembly or plan",
        ));
    }
    let set = crate::archive_set::ArchiveSetReader::open(&archive).map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &archive,
            error.to_string(),
        )
    })?;
    let segments = structural_segments(set.segments());
    let compression_reference =
        crate::archive::archive_compression_reference_identity(&archive).map_err(|error| {
            invalid(
                InvalidBuildKind::CorruptArtifact,
                &archive,
                error.to_string(),
            )
        })?;
    if receipt.segments != segments || receipt.compression_reference != compression_reference {
        return Err(invalid(
            InvalidBuildKind::CorruptArtifact,
            &receipt_path,
            "archive structure does not match its receipt",
        ));
    }
    Ok(Some(receipt))
}

fn inspect_generation(
    root: &Path,
    plan: &DirectBuildPlan,
    archive: &ArchiveReceipt,
) -> Result<Option<GenerationReceipt>, InvalidBuildState> {
    let path = root.join("archive.generation.json");
    let receipt: Option<GenerationReceipt> = read_optional(&path)?;
    let titles = root.join("archive.swtitle");
    if receipt.is_none() {
        return if titles.exists() {
            let index = crate::title_index::TitleIndex::open(&titles).map_err(|error| {
                invalid(
                    InvalidBuildKind::CorruptArtifact,
                    &titles,
                    error.to_string(),
                )
            })?;
            if index.generation_id() != &archive.generation_id {
                Err(invalid(
                    InvalidBuildKind::ForeignIdentity,
                    &titles,
                    "candidate title index names another generation",
                ))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        };
    }
    let receipt = receipt.expect("checked");
    if !titles.exists() {
        return Err(invalid(
            InvalidBuildKind::MissingArtifact,
            &titles,
            "generation receipt exists without its title index",
        ));
    }
    if receipt.schema != RECEIPT_SCHEMA
        || receipt.plan_id != plan.plan_id
        || receipt.identity.generation_id != archive.generation_id
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &path,
            "generation receipt belongs to another plan or archive",
        ));
    }
    let observed = crate::generation::validate_generation(
        root.join("archive.swdump"),
        &titles,
        &receipt.identity,
    )
    .map_err(|error| {
        invalid(
            InvalidBuildKind::CorruptArtifact,
            &path,
            error.to_string(),
        )
    })?;
    if observed.wiki_db != plan.wiki_db
        || observed.content_frontier != plan.content_snapshot
        || observed.metadata_frontier != plan.metadata_snapshot
    {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &path,
            "generation metadata does not match its full-build plan",
        ));
    }
    Ok(Some(receipt))
}

fn has_authoritative_artifact(root: &Path) -> bool {
    [
        "assembly.partial",
        "assembly.checkpoint.json",
        "archive.swdump",
        "archive.swtitle",
        "archive.receipt.json",
        "archive.generation.json",
        "title-projection.entries",
        "title-projection.receipt.json",
    ]
    .iter()
    .any(|name| root.join(name).exists())
        || root
            .join("nodes")
            .read_dir()
            .ok()
            .and_then(|mut entries| entries.next())
            .is_some()
}

/// The only read-only authority for interpreting a full-build tree.
pub(crate) fn inspect_build(
    root: &Path,
    expected_plan_id: Option<&str>,
) -> Result<BuildState, InvalidBuildState> {
    let plan_path = root.join("plan.json");
    if !plan_path.exists() {
        return if has_authoritative_artifact(root) {
            Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                &plan_path,
                "build artifacts exist without a plan receipt",
            ))
        } else {
            Ok(BuildState::Unplanned)
        };
    }
    let plan = crate::direct::read_direct_build_plan(&plan_path).map_err(|error| {
        invalid(
            InvalidBuildKind::MalformedReceipt,
            &plan_path,
            error.to_string(),
        )
    })?;
    if expected_plan_id.is_some_and(|expected| expected != plan.plan_id) {
        return Err(invalid(
            InvalidBuildKind::ForeignIdentity,
            &plan_path,
            format!(
                "expected plan {}, observed {}",
                expected_plan_id.expect("checked"),
                plan.plan_id
            ),
        ));
    }

    if let Some(archive) = inspect_complete_archive(root, &plan)? {
        if root.join("assembly.partial").exists()
            || root.join("assembly.checkpoint.json").exists()
        {
            return Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root,
                "complete archive and partial assembly coexist",
            ));
        }
        return match inspect_generation(root, &plan, &archive)? {
            Some(generation) => Ok(BuildState::Ready { plan, generation }),
            None => {
                let title_projection = inspect_title_projection(root, &plan)?.ok_or_else(|| {
                    invalid(
                        InvalidBuildKind::MissingArtifact,
                        root.join("title-projection.receipt.json"),
                        "committed archive has no durable title projection",
                    )
                })?;
                Ok(BuildState::Projecting {
                    plan,
                    archive,
                    title_projection,
                })
            }
        };
    }
    if root.join("archive.swtitle").exists() || root.join("archive.generation.json").exists() {
        return Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root,
            "generation metadata exists without a complete archive",
        ));
    }
    let title_projection = inspect_title_projection(root, &plan)?;

    let mut targets = Vec::new();
    for (kind, count) in [
        (TargetKind::Content, plan.content_target_count()),
        (TargetKind::History, plan.history_files.len()),
    ] {
        for index in 0..count {
            targets.push(inspect_target(root, &plan, kind, index)?);
        }
    }
    let expected_names = targets
        .iter()
        .flat_map(|target| {
            [
                target_path(root, &plan, target.kind, target.index),
                target_partial_path(root, &plan, target.kind, target.index),
            ]
        })
        .map(|path| {
            path.map(|path| path.file_name().expect("target node name").to_owned())
        })
        .collect::<Result<std::collections::HashSet<_>, _>>()?;
    match std::fs::read_dir(root.join("nodes")) {
        Ok(entries) => {
        for entry in entries {
            let entry = entry.map_err(|error| {
                invalid(
                    InvalidBuildKind::Io,
                    root.join("nodes"),
                    error.to_string(),
                )
            })?;
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with('.') && !expected_names.contains(&name) {
                return Err(invalid(
                    InvalidBuildKind::UnsupportedLayout,
                    entry.path(),
                    "unrecognized finalized target layout",
                ));
            }
        }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(invalid(
                InvalidBuildKind::Io,
                root.join("nodes"),
                format!("cannot inspect target directory: {error}"),
            ))
        }
    }
    let all_ready = targets
        .iter()
        .all(|target| matches!(target.state, TargetState::Ready(_)));
    if let Some(checkpoint) = inspect_assembly(root, &plan)? {
        if !all_ready {
            return Err(invalid(
                InvalidBuildKind::ContradictoryEvidence,
                root.join("assembly.checkpoint.json"),
                "assembly exists before every source target is ready",
            ));
        }
        return Ok(BuildState::Assembling {
            plan,
            targets,
            checkpoint,
            title_projection,
        });
    }
    if title_projection.is_some() {
        return Err(invalid(
            InvalidBuildKind::ContradictoryEvidence,
            root.join("title-projection.receipt.json"),
            "title projection exists without assembly or committed archive",
        ));
    }
    if all_ready {
        Ok(BuildState::ReadyForAssembly { plan, targets })
    } else {
        Ok(BuildState::Planned { plan, targets })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with_content_targets(count: usize) -> DirectBuildPlan {
        let mut plan = DirectBuildPlan {
            schema: 1,
            plan_id: String::new(),
            wiki_db: "testwiki".into(),
            content_snapshot: "2026-08-01".into(),
            metadata_snapshot: "2026-08".into(),
            observed_at_micros: 1,
            frame_target: 128 << 10,
            range_target: 1 << 20,
            compression_level: 9,
            ref_prefix_sample_bytes: 2,
            ref_prefix_bytes: 1,
            content_groups: (0..count)
                .map(|index| {
                    vec![crate::direct::PlannedPart {
                        url: format!("https://example.invalid/{index}"),
                        filename: format!("testwiki-p{index}p{index}.xml.bz2"),
                        size_bytes: 1,
                        sha256: None,
                        sha1: None,
                        md5: None,
                    }]
                })
                .collect(),
            history_files: Vec::new(),
        };
        plan.plan_id = crate::direct::canonical_direct_plan_id(&plan).unwrap();
        plan
    }

    #[test]
    fn worker_inspector_does_not_read_sibling_target_receipts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nodes")).unwrap();
        let plan = plan_with_content_targets(3);
        persist_receipt(&root.path().join("plan.json"), &plan).unwrap();

        let sibling =
            target_path(root.path(), &plan, TargetKind::Content, 2).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("receipt.json"), b"not json").unwrap();

        let target = inspect_target_for_materialization(
            root.path(),
            &plan,
            TargetKind::Content,
            0,
        )
        .unwrap();
        assert_eq!(target.state, TargetState::Missing);
        assert_eq!(
            inspect_build(root.path(), Some(&plan.plan_id))
                .unwrap_err()
                .kind,
            InvalidBuildKind::MalformedReceipt
        );
    }

    #[test]
    fn malformed_temporary_build_can_be_abandoned() {
        let root = tempfile::tempdir().unwrap();
        let state = InvalidBuildState {
            kind: InvalidBuildKind::MalformedReceipt,
            path: root.path().join("plan.json"),
            diagnostic: "unsupported direct build plan".into(),
        };
        assert_eq!(
            transition_invalid_build(
                root.path(),
                &state,
                InvalidBuildEvent::AbandonInvalidScratch,
            )
            .unwrap(),
            InvalidBuildTransition::AbandonScratch
        );
    }

    #[test]
    fn contradictory_or_candidate_build_is_not_auto_abandoned() {
        let root = tempfile::tempdir().unwrap();
        let contradictory = InvalidBuildState {
            kind: InvalidBuildKind::ContradictoryEvidence,
            path: root.path().join("archive.swdump"),
            diagnostic: "complete archive and partial assembly coexist".into(),
        };
        assert!(transition_invalid_build(
            root.path(),
            &contradictory,
            InvalidBuildEvent::AbandonInvalidScratch,
        )
        .is_err());

        std::fs::write(root.path().join("archive.swdump"), b"candidate").unwrap();
        let foreign = InvalidBuildState {
            kind: InvalidBuildKind::ForeignIdentity,
            path: root.path().join("nodes/receipt.json"),
            diagnostic: "target belongs to another plan".into(),
        };
        assert!(transition_invalid_build(
            root.path(),
            &foreign,
            InvalidBuildEvent::AbandonInvalidScratch,
        )
        .is_err());
    }

    #[test]
    fn target_state_event_table_is_exhaustive() {
        let plan = plan_with_content_targets(1);
        let checkpoint = TargetCheckpointReceipt {
            schema: RECEIPT_SCHEMA,
            plan_id: plan.plan_id.clone(),
            kind: TargetKind::Content,
            index: 0,
            source_id: target_source_id(&plan, TargetKind::Content, 0).unwrap(),
            files: vec![TargetCheckpointFile {
                name: "data.swdump".into(),
                bytes: 1,
            }],
        };
        let ready = make_target_receipt(
            &plan,
            TargetKind::Content,
            0,
            1,
            Some(1),
            Some(1),
            PartialStats::default(),
        )
        .unwrap();
        let states = [
            TargetState::Missing,
            TargetState::Partial(checkpoint),
            TargetState::Ready(ready),
        ];
        let events = [
            TargetEvent::Begin,
            TargetEvent::Checkpoint,
            TargetEvent::Publish,
        ];
        let expected = [
            [
                Ok(TargetTransition::Start),
                Ok(TargetTransition::PersistCheckpoint),
                Err("target cannot publish without a durable checkpoint"),
            ],
            [
                Ok(TargetTransition::Resume),
                Ok(TargetTransition::PersistCheckpoint),
                Ok(TargetTransition::Publish),
            ],
            [
                Ok(TargetTransition::Reuse),
                Err("ready target is immutable"),
                Err("ready target is immutable"),
            ],
        ];
        for (state_index, state) in states.iter().enumerate() {
            for (event_index, event) in events.iter().copied().enumerate() {
                assert_eq!(
                    transition_target(state, event),
                    expected[state_index][event_index]
                );
            }
        }
    }

    #[test]
    fn title_projection_publish_boundaries_have_one_authoritative_interpretation() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan_with_content_targets(0);
        let entry = [7_u8; 16];
        let identity = hex::encode(sha2::Sha256::digest(entry));
        let file_name = format!("title-projection-{identity}.entries");
        std::fs::write(root.path().join(&file_name), entry).unwrap();

        // Crash after the payload rename but before receipt publication: the
        // content-addressed payload is orphaned attempt output, not evidence
        // that the build advanced.
        assert_eq!(inspect_title_projection(root.path(), &plan).unwrap(), None);

        let receipt = commit_title_projection(
            root.path(),
            &plan,
            &file_name,
            1,
            &identity,
        )
        .unwrap();
        assert_eq!(
            inspect_title_projection(root.path(), &plan).unwrap(),
            Some(receipt.clone())
        );

        // A later attempt may have renamed another payload before crashing.
        // The atomic receipt still selects exactly the earlier projection.
        let orphan_identity = hex::encode(sha2::Sha256::digest([8_u8; 16]));
        std::fs::write(
            root.path()
                .join(format!("title-projection-{orphan_identity}.entries")),
            [8_u8; 16],
        )
        .unwrap();
        assert_eq!(
            inspect_title_projection(root.path(), &plan).unwrap(),
            Some(receipt)
        );
    }

    #[test]
    fn title_projection_receipt_rejects_structural_extent_change() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan_with_content_targets(0);
        let entry = [3_u8; 16];
        let identity = hex::encode(sha2::Sha256::digest(entry));
        let file_name = format!("title-projection-{identity}.entries");
        std::fs::write(root.path().join(&file_name), entry).unwrap();
        commit_title_projection(root.path(), &plan, &file_name, 1, &identity).unwrap();

        std::fs::write(root.path().join(&file_name), [4_u8; 15]).unwrap();
        assert_eq!(
            inspect_title_projection(root.path(), &plan)
                .unwrap_err()
                .kind,
            InvalidBuildKind::CorruptArtifact
        );
    }

    #[test]
    fn assembly_state_event_matrix_is_exhaustive() {
        let states = [
            AssemblyState::NotStarted,
            AssemblyState::Partial,
            AssemblyState::ArchiveRenamed,
            AssemblyState::Projecting,
            AssemblyState::Ready,
        ];
        let events = [
            AssemblyEvent::Begin,
            AssemblyEvent::RetryRequested,
            AssemblyEvent::Checkpoint,
            AssemblyEvent::FinishAndRename,
            AssemblyEvent::CommitArchiveReceipt,
            AssemblyEvent::CommitGeneration,
            AssemblyEvent::CleanupRequested,
        ];
        const INVALID: Result<AssemblyTransition, &'static str> =
            Err("assembly event is invalid in current state");
        let expected = [
            [
                Ok(AssemblyTransition::Start),
                Ok(AssemblyTransition::Start),
                INVALID,
                INVALID,
                INVALID,
                INVALID,
                INVALID,
            ],
            [
                Ok(AssemblyTransition::Resume),
                Ok(AssemblyTransition::Resume),
                Ok(AssemblyTransition::PersistCheckpoint),
                Ok(AssemblyTransition::RenameArchive),
                INVALID,
                INVALID,
                INVALID,
            ],
            [
                Ok(AssemblyTransition::RecoverArchiveReceipt),
                Ok(AssemblyTransition::RecoverArchiveReceipt),
                INVALID,
                INVALID,
                Ok(AssemblyTransition::CommitArchiveReceipt),
                INVALID,
                INVALID,
            ],
            [
                Ok(AssemblyTransition::ResumeProjection),
                Ok(AssemblyTransition::ResumeProjection),
                INVALID,
                INVALID,
                INVALID,
                Ok(AssemblyTransition::CommitGeneration),
                INVALID,
            ],
            [
                Ok(AssemblyTransition::Reuse),
                Ok(AssemblyTransition::Reuse),
                INVALID,
                INVALID,
                INVALID,
                INVALID,
                Ok(AssemblyTransition::Cleanup),
            ],
        ];
        for (state_index, state) in states.into_iter().enumerate() {
            for (event_index, event) in events.into_iter().enumerate() {
                assert_eq!(
                    transition_assembly(state, event),
                    expected[state_index][event_index],
                    "{state:?} + {event:?}"
                );
            }
        }
    }
}
