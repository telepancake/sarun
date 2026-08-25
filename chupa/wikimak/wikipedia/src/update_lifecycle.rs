//! Durable state model for one incremental Wikipedia update.
//!
//! This module deliberately contains no update execution logic.  It defines
//! the typed receipts and the one read-only inspector which interprets them.
//! Execution code may advance the machine only by durably publishing the next
//! receipt described here.

use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub(crate) const UPDATE_SCHEMA: u32 = 1;

/// Durable phases of one update, independent of the receipt payloads.
///
/// `Unplanned` and `Cleaned` have no update-root representation: they are the
/// states immediately before publishing the exact plan and immediately after
/// removing a committed update root.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum UpdatePhase {
    Unplanned,
    Planned,
    TailReady,
    BasePreserved,
    BaseSiteInfoReady,
    ApplyingRanges,
    RangesApplied,
    CandidateComplete,
    IndexReady,
    Installed,
    Committed,
    Cleaned,
}

/// The complete event alphabet for the incremental-update machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateEvent {
    PublishPlan,
    PublishTail,
    PreserveBase,
    PublishBaseSiteInfo,
    PublishRangePlan,
    PublishRange,
    PublishFinalRange,
    PublishInventory,
    PublishIndex,
    InstallGeneration,
    PublishCommit,
    Cleanup,
    DiscoveryFailed,
    SourceGapDetected,
    WorkerFailed,
    CancelRequested,
    ProcessCrashed,
    ResumeRequested,
    RetryRequested,
    DuplicateReceipt,
    StaleReceipt,
    ForeignReceipt,
    InstallInterrupted,
    CleanupFailed,
}

pub(crate) const ALL_UPDATE_EVENTS: [UpdateEvent; 24] = [
    UpdateEvent::PublishPlan,
    UpdateEvent::PublishTail,
    UpdateEvent::PreserveBase,
    UpdateEvent::PublishBaseSiteInfo,
    UpdateEvent::PublishRangePlan,
    UpdateEvent::PublishRange,
    UpdateEvent::PublishFinalRange,
    UpdateEvent::PublishInventory,
    UpdateEvent::PublishIndex,
    UpdateEvent::InstallGeneration,
    UpdateEvent::PublishCommit,
    UpdateEvent::Cleanup,
    UpdateEvent::DiscoveryFailed,
    UpdateEvent::SourceGapDetected,
    UpdateEvent::WorkerFailed,
    UpdateEvent::CancelRequested,
    UpdateEvent::ProcessCrashed,
    UpdateEvent::ResumeRequested,
    UpdateEvent::RetryRequested,
    UpdateEvent::DuplicateReceipt,
    UpdateEvent::StaleReceipt,
    UpdateEvent::ForeignReceipt,
    UpdateEvent::InstallInterrupted,
    UpdateEvent::CleanupFailed,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Rejection {
    OutOfOrder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Impossibility {
    RangeAfterFinalRange,
    EventAfterCleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransitionDecision {
    Advance(UpdatePhase),
    NoOp,
    Reject(Rejection),
    Impossible(Impossibility),
}

/// Pure transition relation. Every event is deliberately classified for every
/// phase: advancing the machine, replaying an already durable event, rejecting
/// an out-of-order request, or exposing an executor invariant violation.
pub(crate) fn transition(state: UpdatePhase, event: UpdateEvent) -> TransitionDecision {
    debug_assert!(ALL_UPDATE_EVENTS.contains(&event));
    use TransitionDecision::{Advance, Impossible, NoOp, Reject};
    use UpdateEvent::*;
    use UpdatePhase::*;

    if state == Cleaned {
        return match event {
            Cleanup => NoOp,
            StaleReceipt | ForeignReceipt | ResumeRequested | RetryRequested | DuplicateReceipt => {
                Reject(Rejection::OutOfOrder)
            }
            _ => Impossible(Impossibility::EventAfterCleanup),
        };
    }

    match event {
        PublishPlan => match state {
            Unplanned => Advance(Planned),
            Planned | TailReady | BasePreserved | BaseSiteInfoReady | ApplyingRanges | RangesApplied
            | CandidateComplete | IndexReady | Installed | Committed => NoOp,
            Cleaned => unreachable!(),
        },
        PublishTail => linear_event(state, Planned, TailReady),
        PreserveBase => linear_event(state, TailReady, BasePreserved),
        PublishBaseSiteInfo => linear_event(state, BasePreserved, BaseSiteInfoReady),
        PublishRangePlan => linear_event(state, BaseSiteInfoReady, ApplyingRanges),
        PublishRange => match state {
            Unplanned | Planned | TailReady | BasePreserved | BaseSiteInfoReady => {
                Reject(Rejection::OutOfOrder)
            }
            ApplyingRanges => NoOp,
            RangesApplied | CandidateComplete | IndexReady | Installed | Committed => {
                Impossible(Impossibility::RangeAfterFinalRange)
            }
            Cleaned => unreachable!(),
        },
        PublishFinalRange => linear_event(state, ApplyingRanges, RangesApplied),
        PublishInventory => linear_event(state, RangesApplied, CandidateComplete),
        PublishIndex => linear_event(state, CandidateComplete, IndexReady),
        InstallGeneration => linear_event(state, IndexReady, Installed),
        PublishCommit => linear_event(state, Installed, Committed),
        Cleanup => linear_event(state, Committed, Cleaned),
        DiscoveryFailed | SourceGapDetected | WorkerFailed | CancelRequested | ProcessCrashed => {
            NoOp
        }
        ResumeRequested | RetryRequested => match state {
            Unplanned => Reject(Rejection::OutOfOrder),
            Planned | TailReady | BasePreserved | BaseSiteInfoReady | ApplyingRanges | RangesApplied
            | CandidateComplete | IndexReady | Installed | Committed => NoOp,
            Cleaned => unreachable!(),
        },
        DuplicateReceipt => match state {
            Unplanned => Reject(Rejection::OutOfOrder),
            Planned | TailReady | BasePreserved | BaseSiteInfoReady | ApplyingRanges | RangesApplied
            | CandidateComplete | IndexReady | Installed | Committed => NoOp,
            Cleaned => unreachable!(),
        },
        StaleReceipt | ForeignReceipt => Reject(Rejection::OutOfOrder),
        InstallInterrupted => match state {
            IndexReady | Installed => NoOp,
            Unplanned | Planned | TailReady | BasePreserved | BaseSiteInfoReady | ApplyingRanges | RangesApplied
            | CandidateComplete | Committed => Reject(Rejection::OutOfOrder),
            Cleaned => unreachable!(),
        },
        CleanupFailed => match state {
            Committed => NoOp,
            Unplanned | Planned | TailReady | BasePreserved | BaseSiteInfoReady | ApplyingRanges | RangesApplied
            | CandidateComplete | IndexReady | Installed => Reject(Rejection::OutOfOrder),
            Cleaned => unreachable!(),
        },
    }
}

fn linear_event(
    state: UpdatePhase,
    predecessor: UpdatePhase,
    target: UpdatePhase,
) -> TransitionDecision {
    use TransitionDecision::{Advance, NoOp, Reject};
    if state == predecessor {
        Advance(target)
    } else if state < predecessor {
        Reject(Rejection::OutOfOrder)
    } else {
        NoOp
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateAction {
    PublishTail,
    PreserveBase,
    PublishBaseSiteInfo,
    PublishRangePlan,
    PublishRange,
    PublishInventory,
    PublishIndex,
    InstallGeneration,
    PublishCommit,
    Cleanup,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ActiveUpdate {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct UpdatePlanReceipt {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
    pub new_generation_id: String,
    pub source_plan_id: String,
    pub wiki_db: String,
    pub base_content_frontier: String,
    pub base_metadata_frontier: String,
    pub result_content_frontier: String,
    pub result_metadata_frontier: String,
    pub overlap_days: u64,
    pub frame_target: usize,
    pub compression: CompressionReceipt,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CompressionReceipt {
    pub level: i32,
    pub checksum: bool,
    pub long_distance_matching: bool,
    pub window_log: Option<u32>,
    pub target_block_size: Option<u32>,
    pub workers: u32,
}

impl From<crate::archive::CompressionSettings> for CompressionReceipt {
    fn from(settings: crate::archive::CompressionSettings) -> Self {
        Self {
            level: settings.level,
            checksum: settings.checksum,
            long_distance_matching: settings.long_distance_matching,
            window_log: settings.window_log,
            target_block_size: settings.target_block_size,
            workers: settings.workers,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TailReceipt {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
    pub source_plan_id: String,
    pub tail_id: String,
    pub file_name: String,
    pub bytes: u64,
    pub frame_directory_name: String,
    pub frame_directory_format: u32,
    pub frame_directory_bytes: u64,
    pub frames: u64,
    pub records: u64,
    pub first_entity: Option<EntityBound>,
    pub last_entity: Option<EntityBound>,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct EntityBound {
    pub kind: u8,
    pub id: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PreservedBaseReceipt {
    pub schema: u32,
    pub update_id: String,
    pub generation: crate::generation::GenerationIdentity,
    pub archive_name: String,
    pub index_name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct BaseSiteInfoCheckpoint {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
    pub source_plan_id: String,
    pub site_info: SiteInfoReceipt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SiteInfoReceipt {
    site_name: String,
    db_name: String,
    base: String,
    generator: String,
    case: String,
    language: String,
    rtl: bool,
    server: String,
    script_path: String,
    namespaces: Vec<SiteNamespaceReceipt>,
    interwiki: Vec<SiteInterwikiReceipt>,
    magic_words: Vec<SiteMagicWordReceipt>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SiteNamespaceReceipt {
    id: i32,
    case: String,
    localized_name: String,
    aliases: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SiteInterwikiReceipt {
    prefix: String,
    url: String,
    is_local: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SiteMagicWordReceipt {
    canonical_name: String,
    aliases: Vec<String>,
    case_sensitive: bool,
}

impl BaseSiteInfoCheckpoint {
    pub(crate) fn new(plan: &UpdatePlanReceipt, site_info: crate::archive::SiteInfoRecord) -> Self {
        Self {
            schema: UPDATE_SCHEMA,
            update_id: plan.update_id.clone(),
            base_generation_id: plan.base_generation_id.clone(),
            source_plan_id: plan.source_plan_id.clone(),
            site_info: site_info.into(),
        }
    }

    pub(crate) fn site_info(&self) -> crate::archive::SiteInfoRecord {
        self.site_info.clone().into()
    }
}

impl From<crate::archive::SiteInfoRecord> for SiteInfoReceipt {
    fn from(site_info: crate::archive::SiteInfoRecord) -> Self {
        Self {
            site_name: site_info.site_name,
            db_name: site_info.db_name,
            base: site_info.base,
            generator: site_info.generator,
            case: site_info.case,
            language: site_info.language,
            rtl: site_info.rtl,
            server: site_info.server,
            script_path: site_info.script_path,
            namespaces: site_info
                .namespaces
                .into_iter()
                .map(|namespace| SiteNamespaceReceipt {
                    id: namespace.id,
                    case: namespace.case,
                    localized_name: namespace.localized_name,
                    aliases: namespace.aliases,
                })
                .collect(),
            interwiki: site_info
                .interwiki
                .into_iter()
                .map(|entry| SiteInterwikiReceipt {
                    prefix: entry.prefix,
                    url: entry.url,
                    is_local: entry.is_local,
                })
                .collect(),
            magic_words: site_info
                .magic_words
                .into_iter()
                .map(|word| SiteMagicWordReceipt {
                    canonical_name: word.canonical_name,
                    aliases: word.aliases,
                    case_sensitive: word.case_sensitive,
                })
                .collect(),
        }
    }
}

impl From<SiteInfoReceipt> for crate::archive::SiteInfoRecord {
    fn from(site_info: SiteInfoReceipt) -> Self {
        Self {
            site_name: site_info.site_name,
            db_name: site_info.db_name,
            base: site_info.base,
            generator: site_info.generator,
            case: site_info.case,
            language: site_info.language,
            rtl: site_info.rtl,
            server: site_info.server,
            script_path: site_info.script_path,
            namespaces: site_info
                .namespaces
                .into_iter()
                .map(|namespace| crate::archive::SiteNamespaceRecord {
                    id: namespace.id,
                    case: namespace.case,
                    localized_name: namespace.localized_name,
                    aliases: namespace.aliases,
                })
                .collect(),
            interwiki: site_info
                .interwiki
                .into_iter()
                .map(|entry| crate::archive::SiteInterwikiRecord {
                    prefix: entry.prefix,
                    url: entry.url,
                    is_local: entry.is_local,
                })
                .collect(),
            magic_words: site_info
                .magic_words
                .into_iter()
                .map(|word| crate::archive::SiteMagicWordRecord {
                    canonical_name: word.canonical_name,
                    aliases: word.aliases,
                    case_sensitive: word.case_sensitive,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RangePlanReceipt {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
    pub tail_id: String,
    pub slots: Vec<RangeSlot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RangeSlot {
    pub index: usize,
    pub kind: u8,
    pub first_id: u64,
    pub last_id: u64,
    pub base_segment_id: String,
    pub base_name: String,
    pub base_bytes: u64,
    pub candidate_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum RangeSelection {
    Unchanged {
        segment_id: String,
        name: String,
        bytes: u64,
    },
    Replaced {
        segment_id: String,
        name: String,
        bytes: u64,
        frames: u64,
        records: u64,
        frame_directory_name: String,
        frame_directory_format: u32,
        frame_directory_bytes: u64,
        first_entity: EntityBound,
        last_entity: EntityBound,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RangeCandidateReceipt {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
    pub tail_id: String,
    pub slot_index: usize,
    pub candidate_id: String,
    pub kind: u8,
    pub first_id: u64,
    pub last_id: u64,
    pub base_segment_id: String,
    pub selection: RangeSelection,
    pub consumed_first: Option<EntityBound>,
    pub consumed_last: Option<EntityBound>,
    pub tail_bytes_read: u64,
    pub base_bytes_read: u64,
    pub base_frame_bytes_copied: u64,
    pub base_frame_bytes_decoded: u64,
    pub candidate_bytes_written: u64,
    pub title_projection_name: Option<String>,
    pub title_projection_bytes: u64,
    pub title_projection_records: u64,
    #[serde(default)]
    pub backref_delta_name: Option<String>,
    #[serde(default)]
    pub backref_delta_bytes: u64,
    #[serde(default)]
    pub backref_delta_records: u64,
    pub tail_cursor: TailCursorReceipt,
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TailCursorReceipt {
    pub frame_offset: Option<u64>,
    pub record_ordinal: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SelectedSegment {
    pub slot_index: usize,
    pub segment_id: String,
    pub name: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CandidateInventoryReceipt {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
    pub tail_id: String,
    pub segments: Vec<SelectedSegment>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PreparedGenerationReceipt {
    pub schema: u32,
    pub update_id: String,
    pub base_generation_id: String,
    pub generation_id: String,
    pub archive_name: String,
    pub index_name: String,
    pub index_bytes: u64,
    #[serde(default)]
    pub backrefs_name: String,
    #[serde(default)]
    pub backrefs_bytes: u64,
    #[serde(default)]
    pub backrefs_records: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CommitReceipt {
    pub schema: u32,
    pub update_id: String,
    pub old_generation_id: String,
    pub new_generation_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UpdateState {
    Planned(UpdatePlanReceipt),
    TailReady(UpdatePlanReceipt, TailReceipt),
    BasePreserved(UpdatePlanReceipt, TailReceipt, PreservedBaseReceipt),
    BaseSiteInfoReady {
        plan: UpdatePlanReceipt,
        tail: TailReceipt,
        preserved_base: PreservedBaseReceipt,
        site_info: BaseSiteInfoCheckpoint,
    },
    ApplyingRanges {
        plan: UpdatePlanReceipt,
        tail: TailReceipt,
        preserved_base: PreservedBaseReceipt,
        site_info: BaseSiteInfoCheckpoint,
        ranges: RangePlanReceipt,
        completed: usize,
    },
    CandidateComplete {
        plan: UpdatePlanReceipt,
        tail: TailReceipt,
        preserved_base: PreservedBaseReceipt,
        site_info: BaseSiteInfoCheckpoint,
        ranges: RangePlanReceipt,
        inventory: CandidateInventoryReceipt,
    },
    IndexReady {
        plan: UpdatePlanReceipt,
        tail: TailReceipt,
        preserved_base: PreservedBaseReceipt,
        site_info: BaseSiteInfoCheckpoint,
        inventory: CandidateInventoryReceipt,
        generation: PreparedGenerationReceipt,
    },
    Installed {
        plan: UpdatePlanReceipt,
        generation: PreparedGenerationReceipt,
    },
    Committed(CommitReceipt),
}

impl UpdateState {
    pub(crate) fn phase(&self) -> UpdatePhase {
        match self {
            Self::Planned(_) => UpdatePhase::Planned,
            Self::TailReady(..) => UpdatePhase::TailReady,
            Self::BasePreserved(..) => UpdatePhase::BasePreserved,
            Self::BaseSiteInfoReady { .. } => UpdatePhase::BaseSiteInfoReady,
            Self::ApplyingRanges {
                ranges, completed, ..
            } if *completed == ranges.slots.len() => UpdatePhase::RangesApplied,
            Self::ApplyingRanges { .. } => UpdatePhase::ApplyingRanges,
            Self::CandidateComplete { .. } => UpdatePhase::CandidateComplete,
            Self::IndexReady { .. } => UpdatePhase::IndexReady,
            Self::Installed { .. } => UpdatePhase::Installed,
            Self::Committed(_) => UpdatePhase::Committed,
        }
    }

    pub(crate) fn next_action(&self) -> UpdateAction {
        match self.phase() {
            UpdatePhase::Planned => UpdateAction::PublishTail,
            UpdatePhase::TailReady => UpdateAction::PreserveBase,
            UpdatePhase::BasePreserved => UpdateAction::PublishBaseSiteInfo,
            UpdatePhase::BaseSiteInfoReady => UpdateAction::PublishRangePlan,
            UpdatePhase::ApplyingRanges => UpdateAction::PublishRange,
            UpdatePhase::RangesApplied => UpdateAction::PublishInventory,
            UpdatePhase::CandidateComplete => UpdateAction::PublishIndex,
            UpdatePhase::IndexReady => UpdateAction::InstallGeneration,
            UpdatePhase::Installed => UpdateAction::PublishCommit,
            UpdatePhase::Committed => UpdateAction::Cleanup,
            UpdatePhase::Unplanned | UpdatePhase::Cleaned => {
                unreachable!("non-durable update phase has no inspected state")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InvalidUpdateState {
    pub path: PathBuf,
    pub diagnostic: String,
}

impl std::fmt::Display for InvalidUpdateState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.diagnostic)
    }
}

impl std::error::Error for InvalidUpdateState {}

#[derive(Clone, Debug)]
pub(crate) struct UpdatePaths {
    pub root: PathBuf,
}

impl UpdatePaths {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(crate) fn plan(&self) -> PathBuf {
        self.root.join("plan.json")
    }

    pub(crate) fn source_plan(&self) -> PathBuf {
        self.root.join("source-plan.json")
    }

    pub(crate) fn tail_archive(&self) -> PathBuf {
        self.root.join("tail").join("records.swdump")
    }

    pub(crate) fn tail_receipt(&self) -> PathBuf {
        self.root.join("tail").join("receipt.json")
    }

    pub(crate) fn tail_frame_directory(&self) -> PathBuf {
        self.root.join("tail").join("frames.swframe")
    }

    pub(crate) fn base_archive(&self) -> PathBuf {
        self.root.join("base").join("archive.swdump")
    }

    pub(crate) fn base_index(&self) -> PathBuf {
        self.root.join("base").join("archive.swtitle")
    }

    pub(crate) fn base_receipt(&self) -> PathBuf {
        self.root.join("base").join("receipt.json")
    }

    pub(crate) fn base_site_info(&self) -> PathBuf {
        self.root.join("base").join("site-info.json")
    }

    pub(crate) fn range_plan(&self) -> PathBuf {
        self.root.join("ranges").join("plan.json")
    }

    pub(crate) fn range_receipt(&self, index: usize) -> PathBuf {
        self.root
            .join("ranges")
            .join("receipts")
            .join(format!("{index:06}.json"))
    }

    pub(crate) fn range_object(&self, candidate_id: &str) -> PathBuf {
        self.root
            .join("ranges")
            .join("objects")
            .join(format!("{candidate_id}.swdump-part"))
    }

    pub(crate) fn range_projection(&self, candidate_id: &str) -> PathBuf {
        self.root
            .join("ranges")
            .join("projections")
            .join(format!("{candidate_id}.swdump"))
    }

    pub(crate) fn range_frame_directory(&self, candidate_id: &str) -> PathBuf {
        self.root
            .join("ranges")
            .join("frame-directories")
            .join(format!("{candidate_id}.swframe"))
    }

    pub(crate) fn range_backref_delta(&self, candidate_id: &str) -> PathBuf {
        self.root
            .join("ranges")
            .join("backref-deltas")
            .join(format!("{candidate_id}.swrefdelta"))
    }

    pub(crate) fn candidate_archive(&self) -> PathBuf {
        self.root.join("candidate").join("archive.swdump")
    }

    pub(crate) fn candidate_index(&self) -> PathBuf {
        self.root.join("candidate").join("archive.swtitle")
    }

    pub(crate) fn candidate_backrefs(&self) -> PathBuf {
        self.root.join("candidate").join("backrefs.swrefs")
    }

    pub(crate) fn candidate_inventory(&self) -> PathBuf {
        self.root.join("candidate").join("inventory.json")
    }

    pub(crate) fn prepared_generation(&self) -> PathBuf {
        self.root.join("candidate").join("generation.json")
    }

    pub(crate) fn commit_receipt(&self) -> PathBuf {
        self.root.join("commit.json")
    }
}

fn invalid(path: impl Into<PathBuf>, diagnostic: impl Into<String>) -> InvalidUpdateState {
    InvalidUpdateState {
        path: path.into(),
        diagnostic: diagnostic.into(),
    }
}

pub(crate) fn read_receipt<T: DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, InvalidUpdateState> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(invalid(path, error.to_string())),
    };
    if !metadata.file_type().is_file() {
        return Err(invalid(path, "receipt is not a regular file"));
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return Err(invalid(path, error.to_string())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| invalid(path, format!("malformed receipt: {error}")))
}

fn require_schema(path: &Path, schema: u32) -> Result<(), InvalidUpdateState> {
    if schema != UPDATE_SCHEMA {
        return Err(invalid(
            path,
            format!("unsupported update receipt schema {schema}"),
        ));
    }
    Ok(())
}

fn require_link(
    path: &Path,
    actual: &str,
    expected: &str,
    field: &str,
) -> Result<(), InvalidUpdateState> {
    if actual != expected {
        return Err(invalid(
            path,
            format!("{field} is {actual:?}, expected {expected:?}"),
        ));
    }
    Ok(())
}

fn require_file(path: &Path, expected_bytes: Option<u64>) -> Result<(), InvalidUpdateState> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| invalid(path, format!("required artifact is unavailable: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(invalid(path, "required artifact is not a regular file"));
    }
    if let Some(expected) = expected_bytes {
        if metadata.len() != expected {
            return Err(invalid(
                path,
                format!(
                    "artifact has {} bytes, receipt requires {expected}",
                    metadata.len()
                ),
            ));
        }
    }
    Ok(())
}

fn validate_plan(paths: &UpdatePaths, plan: &UpdatePlanReceipt) -> Result<(), InvalidUpdateState> {
    let path = paths.plan();
    require_schema(&path, plan.schema)?;
    if plan.update_id.is_empty()
        || plan.base_generation_id.is_empty()
        || plan.new_generation_id.is_empty()
        || plan.source_plan_id.is_empty()
        || plan.wiki_db.is_empty()
    {
        return Err(invalid(&path, "plan contains an empty identity"));
    }
    if plan.frame_target == 0 {
        return Err(invalid(&path, "plan has a zero frame target"));
    }
    let source_path = paths.source_plan();
    let source: crate::direct::UpdateSourcePlan = read_receipt(&source_path)?
        .ok_or_else(|| invalid(&source_path, "update plan has no exact source plan"))?;
    crate::direct::validate_update_source_plan(&source)
        .map_err(|error| invalid(&source_path, error.to_string()))?;
    let source_compression: crate::archive::CompressionSettings = source.compression.into();
    if source.source_plan_id != plan.source_plan_id
        || source.source_plan_id != plan.update_id
        || source.base_generation_id.as_str() != plan.base_generation_id
        || source.generation_id.as_str() != plan.new_generation_id
        || source.wiki_db != plan.wiki_db
        || source.base_content_frontier != plan.base_content_frontier
        || source.base_metadata_frontier != plan.base_metadata_frontier
        || source.resulting_content_frontier != plan.result_content_frontier
        || source.resulting_metadata_frontier != plan.result_metadata_frontier
        || source.overlap_days != plan.overlap_days
        || source.frame_target != plan.frame_target
        || CompressionReceipt::from(source_compression) != plan.compression
    {
        return Err(invalid(
            &source_path,
            "exact source plan does not match its lifecycle plan",
        ));
    }
    Ok(())
}

fn validate_tail(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    tail: &TailReceipt,
) -> Result<crate::frame_directory::FrameDirectory, InvalidUpdateState> {
    let path = paths.tail_receipt();
    require_schema(&path, tail.schema)?;
    require_link(&path, &tail.update_id, &plan.update_id, "update_id")?;
    require_link(
        &path,
        &tail.base_generation_id,
        &plan.base_generation_id,
        "base_generation_id",
    )?;
    require_link(
        &path,
        &tail.source_plan_id,
        &plan.source_plan_id,
        "source_plan_id",
    )?;
    if tail.tail_id.is_empty() {
        return Err(invalid(&path, "tail_id is empty"));
    }
    if !tail.complete {
        return Err(invalid(
            &path,
            "tail receipt does not record clean completion",
        ));
    }
    if tail.file_name != "records.swdump" {
        return Err(invalid(
            &path,
            format!("unexpected tail artifact {:?}", tail.file_name),
        ));
    }
    if tail.frame_directory_name != "frames.swframe"
        || tail.frame_directory_format != crate::frame_directory::FORMAT_VERSION
    {
        return Err(invalid(
            &path,
            "tail frame-directory format is not canonical",
        ));
    }
    require_file(&paths.tail_archive(), Some(tail.bytes))?;
    require_file(
        &paths.tail_frame_directory(),
        Some(tail.frame_directory_bytes),
    )?;
    let identity = crate::generation::GenerationId::parse(&tail.tail_id)
        .and_then(|identity| identity.to_bytes())
        .map_err(|error| invalid(&path, error.to_string()))?;
    let directory =
        crate::frame_directory::FrameDirectory::open_bound(paths.tail_frame_directory(), identity)
            .map_err(|error| invalid(paths.tail_frame_directory(), error.to_string()))?;
    let summary = directory.summary();
    if summary.bytes != tail.frame_directory_bytes
        || summary.frames != tail.frames
        || summary.records != tail.records
        || summary.first_entity.map(EntityBound::from) != tail.first_entity
        || summary.last_entity.map(EntityBound::from) != tail.last_entity
    {
        return Err(invalid(
            paths.tail_frame_directory(),
            "tail frame-directory summary disagrees with its receipt",
        ));
    }
    if !crate::archive::has_clean_completion_marker(paths.tail_archive())
        .map_err(|error| invalid(paths.tail_archive(), error.to_string()))?
    {
        return Err(invalid(
            paths.tail_archive(),
            "tail has no clean completion marker",
        ));
    }
    Ok(directory)
}

impl From<crate::archive::EntityKey> for EntityBound {
    fn from(entity: crate::archive::EntityKey) -> Self {
        Self {
            kind: entity.kind as u8,
            id: entity.id,
        }
    }
}

pub(crate) fn validate_preserved_base(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    base: &PreservedBaseReceipt,
) -> Result<(), InvalidUpdateState> {
    validate_preserved_base_receipt(paths, plan, base)?;
    crate::generation::validate_generation(
        paths.base_archive(),
        paths.base_index(),
        &base.generation,
    )
    .map(|_| ())
    .map_err(|error| invalid(paths.base_receipt(), error.to_string()))
}

fn validate_preserved_base_receipt(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    base: &PreservedBaseReceipt,
) -> Result<(), InvalidUpdateState> {
    let path = paths.base_receipt();
    require_schema(&path, base.schema)?;
    require_link(&path, &base.update_id, &plan.update_id, "update_id")?;
    require_link(
        &path,
        base.generation.generation_id.as_str(),
        &plan.base_generation_id,
        "generation_id",
    )?;
    if base.archive_name != "archive.swdump" || base.index_name != "archive.swtitle" {
        return Err(invalid(&path, "preserved base names are not canonical"));
    }
    Ok(())
}

fn validate_preserved_base_index(
    paths: &UpdatePaths,
    base: &PreservedBaseReceipt,
) -> Result<(), InvalidUpdateState> {
    let path = paths.base_index();
    require_file(&path, None)?;
    let titles = crate::title_index::TitleIndex::open(&path)
        .map_err(|error| invalid(&path, error.to_string()))?;
    if titles.generation_id() != &base.generation.generation_id
        || titles.segment_count() != base.generation.segments.len()
    {
        return Err(invalid(
            &path,
            "preserved base index has the wrong generation identity",
        ));
    }
    for (position, expected) in base.generation.segments.iter().enumerate() {
        let observed = titles
            .segment(position)
            .map_err(|error| invalid(&path, error.to_string()))?;
        if observed.role != expected.role
            || observed.first_id != expected.first_id
            || observed.last_id != expected.last_id
            || observed.virtual_start != expected.virtual_start
            || observed.bytes != expected.bytes
        {
            return Err(invalid(
                &path,
                format!("preserved base index segment {position} changed"),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_base_site_info(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    checkpoint: &BaseSiteInfoCheckpoint,
) -> Result<(), InvalidUpdateState> {
    let path = paths.base_site_info();
    require_schema(&path, checkpoint.schema)?;
    require_link(&path, &checkpoint.update_id, &plan.update_id, "update_id")?;
    require_link(
        &path,
        &checkpoint.base_generation_id,
        &plan.base_generation_id,
        "base_generation_id",
    )?;
    require_link(
        &path,
        &checkpoint.source_plan_id,
        &plan.source_plan_id,
        "source_plan_id",
    )?;
    require_link(
        &path,
        &checkpoint.site_info.db_name,
        &plan.wiki_db,
        "site_info.db_name",
    )?;
    Ok(())
}

fn validate_range_plan(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    tail: &TailReceipt,
    ranges: &RangePlanReceipt,
) -> Result<(), InvalidUpdateState> {
    let path = paths.range_plan();
    require_schema(&path, ranges.schema)?;
    require_link(&path, &ranges.update_id, &plan.update_id, "update_id")?;
    require_link(
        &path,
        &ranges.base_generation_id,
        &plan.base_generation_id,
        "base_generation_id",
    )?;
    require_link(&path, &ranges.tail_id, &tail.tail_id, "tail_id")?;
    if ranges.slots.is_empty() {
        return Err(invalid(&path, "range plan has no slots"));
    }
    let mut previous = None;
    for (position, slot) in ranges.slots.iter().enumerate() {
        if slot.index != position {
            return Err(invalid(
                &path,
                format!("slot {} has index {}", position, slot.index),
            ));
        }
        if slot.first_id > slot.last_id
            || slot.base_segment_id.is_empty()
            || slot.base_name.is_empty()
            || slot.candidate_id.is_empty()
        {
            return Err(invalid(&path, format!("slot {position} is malformed")));
        }
        let first = EntityBound {
            kind: slot.kind,
            id: slot.first_id,
        };
        if previous.is_some_and(|last| first <= last) {
            return Err(invalid(&path, "range slots are not strictly ordered"));
        }
        previous = Some(EntityBound {
            kind: slot.kind,
            id: slot.last_id,
        });
    }
    Ok(())
}

fn validate_range_receipt(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    tail: &TailReceipt,
    tail_directory: &crate::frame_directory::FrameDirectory,
    slot: &RangeSlot,
    receipt: &RangeCandidateReceipt,
) -> Result<SelectedSegment, InvalidUpdateState> {
    let path = paths.range_receipt(slot.index);
    require_schema(&path, receipt.schema)?;
    require_link(&path, &receipt.update_id, &plan.update_id, "update_id")?;
    require_link(
        &path,
        &receipt.base_generation_id,
        &plan.base_generation_id,
        "base_generation_id",
    )?;
    require_link(&path, &receipt.tail_id, &tail.tail_id, "tail_id")?;
    if receipt.slot_index != slot.index
        || receipt.candidate_id != slot.candidate_id
        || receipt.kind != slot.kind
        || receipt.first_id != slot.first_id
        || receipt.last_id != slot.last_id
        || receipt.base_segment_id != slot.base_segment_id
    {
        return Err(invalid(&path, "receipt does not describe its range slot"));
    }
    if !receipt.complete {
        return Err(invalid(&path, "range candidate is not complete"));
    }
    validate_range_backref_delta(paths, slot, receipt)?;
    match receipt.tail_cursor.frame_offset {
        None if receipt.tail_cursor.record_ordinal == 0 => {}
        Some(offset) => {
            let Some(position) = tail_directory.index_of_offset(offset) else {
                return Err(invalid(&path, "tail cursor points between frames"));
            };
            let frame = tail_directory
                .get(position)
                .map_err(|error| invalid(&path, error.to_string()))?;
            if receipt.tail_cursor.record_ordinal >= frame.records {
                return Err(invalid(
                    &path,
                    "tail cursor record ordinal lies outside its frame",
                ));
            }
        }
        None => return Err(invalid(&path, "EOF tail cursor has a record ordinal")),
    }
    match &receipt.selection {
        RangeSelection::Unchanged {
            segment_id,
            name,
            bytes,
        } => {
            if segment_id != &slot.base_segment_id
                || name != &slot.base_name
                || *bytes != slot.base_bytes
            {
                return Err(invalid(
                    &path,
                    "unchanged selection does not name the base segment",
                ));
            }
            if receipt.consumed_first.is_some() || receipt.consumed_last.is_some() {
                return Err(invalid(
                    &path,
                    "unchanged range claims to have consumed tail records",
                ));
            }
            if receipt.base_bytes_read != 0
                || receipt.base_frame_bytes_copied != 0
                || receipt.base_frame_bytes_decoded != 0
                || receipt.candidate_bytes_written != 0
                || receipt.title_projection_name.is_some()
                || receipt.title_projection_bytes != 0
                || receipt.title_projection_records != 0
                || receipt.backref_delta_name.is_some()
                || receipt.backref_delta_bytes != 0
                || receipt.backref_delta_records != 0
            {
                return Err(invalid(&path, "unchanged range records archive I/O"));
            }
            Ok(SelectedSegment {
                slot_index: slot.index,
                segment_id: segment_id.clone(),
                name: name.clone(),
                bytes: *bytes,
            })
        }
        RangeSelection::Replaced {
            segment_id,
            name,
            bytes,
            frames,
            records,
            frame_directory_name,
            frame_directory_format,
            frame_directory_bytes,
            first_entity,
            last_entity,
            ..
        } => {
            if segment_id != &slot.candidate_id || name.is_empty() {
                return Err(invalid(
                    &path,
                    "replacement selection has the wrong candidate identity",
                ));
            }
            if receipt.consumed_first.is_none() || receipt.consumed_last.is_none() {
                return Err(invalid(
                    &path,
                    "replacement does not identify its consumed tail interval",
                ));
            }
            let expected_directory_name = format!("{}.swframe", slot.candidate_id);
            if frame_directory_name != &expected_directory_name
                || *frame_directory_format != crate::frame_directory::FORMAT_VERSION
            {
                return Err(invalid(
                    &path,
                    "replacement frame-directory name or format is not canonical",
                ));
            }
            let directory_path = paths.range_frame_directory(&slot.candidate_id);
            require_file(&directory_path, Some(*frame_directory_bytes))?;
            let identity = crate::generation::GenerationId::parse(segment_id)
                .and_then(|identity| identity.to_bytes())
                .map_err(|error| invalid(&path, error.to_string()))?;
            let directory =
                crate::frame_directory::FrameDirectory::open_bound(&directory_path, identity)
                    .map_err(|error| invalid(&directory_path, error.to_string()))?;
            let summary = directory.summary();
            if summary.bytes != *frame_directory_bytes
                || summary.frames != *frames
                || summary.records != *records
                || summary.first_entity.map(EntityBound::from) != Some(*first_entity)
                || summary.last_entity.map(EntityBound::from) != Some(*last_entity)
                || first_entity.kind != slot.kind
                || last_entity.kind != slot.kind
            {
                return Err(invalid(
                    &directory_path,
                    "replacement frame-directory summary is inconsistent",
                ));
            }
            if receipt.base_bytes_read
                != receipt
                    .base_frame_bytes_copied
                    .checked_add(receipt.base_frame_bytes_decoded)
                    .ok_or_else(|| invalid(&path, "base frame byte telemetry overflows"))?
                || receipt.base_bytes_read > slot.base_bytes
                || receipt.candidate_bytes_written != *bytes
            {
                return Err(invalid(
                    &path,
                    "replacement I/O counters do not match its range objects",
                ));
            }
            let object = paths.range_object(&slot.candidate_id);
            require_file(&object, Some(*bytes))?;
            match (
                receipt.title_projection_name.as_deref(),
                receipt.title_projection_records,
            ) {
                (None, 0) if receipt.title_projection_bytes == 0 => {}
                (Some(name), records) if records != 0 => {
                    let expected = format!("{}.swdump", slot.candidate_id);
                    if name != expected {
                        return Err(invalid(
                            &path,
                            "title projection name does not match candidate identity",
                        ));
                    }
                    let projection = paths.range_projection(&slot.candidate_id);
                    require_file(&projection, Some(receipt.title_projection_bytes))?;
                    if !crate::archive::has_clean_completion_marker(&projection)
                        .map_err(|error| invalid(&projection, error.to_string()))?
                    {
                        return Err(invalid(
                            &projection,
                            "title projection has no clean completion marker",
                        ));
                    }
                }
                _ => return Err(invalid(&path, "title projection receipt is inconsistent")),
            }
            Ok(SelectedSegment {
                slot_index: slot.index,
                segment_id: segment_id.clone(),
                name: name.clone(),
                bytes: *bytes,
            })
        }
    }
}

pub(crate) fn validate_range_backref_delta(
    paths: &UpdatePaths,
    slot: &RangeSlot,
    receipt: &RangeCandidateReceipt,
) -> Result<(), InvalidUpdateState> {
    let path = paths.range_receipt(slot.index);
    let is_page = slot.kind == crate::archive::EntityKind::Page as u8;
    let Some(name) = receipt.backref_delta_name.as_deref() else {
        if receipt.backref_delta_bytes != 0 || receipt.backref_delta_records != 0 {
            return Err(invalid(
                &path,
                "backref delta has counters but no durable artifact name",
            ));
        }
        return Ok(());
    };
    if !is_page {
        return Err(invalid(
            &path,
            "non-Page range carries a backref delta",
        ));
    }
    let expected = format!("{}.swrefdelta", slot.candidate_id);
    if name != expected || receipt.backref_delta_bytes == 0 {
        return Err(invalid(
            &path,
            "backref delta name or byte count is not canonical",
        ));
    }
    let delta = paths.range_backref_delta(&slot.candidate_id);
    require_file(&delta, Some(receipt.backref_delta_bytes))?;
    let records = crate::backrefs::projection_delta_file_records(&delta)
    .map_err(|error| invalid(&delta, error.to_string()))?;
    if records != receipt.backref_delta_records {
        return Err(invalid(
            &delta,
            format!(
                "backref delta contains {records} records, receipt requires {}",
                receipt.backref_delta_records
            ),
        ));
    }
    Ok(())
}

fn tail_cursor_order(
    directory: &crate::frame_directory::FrameDirectory,
    cursor: &TailCursorReceipt,
) -> Option<(usize, u64)> {
    match cursor.frame_offset {
        Some(offset) => directory
            .index_of_offset(offset)
            .map(|position| (position, cursor.record_ordinal)),
        None => Some((directory.len(), 0)),
    }
}

fn validate_inventory(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    tail: &TailReceipt,
    expected: &[SelectedSegment],
    inventory: &CandidateInventoryReceipt,
    require_candidate_archive: bool,
) -> Result<(), InvalidUpdateState> {
    let path = paths.candidate_inventory();
    require_schema(&path, inventory.schema)?;
    require_link(&path, &inventory.update_id, &plan.update_id, "update_id")?;
    require_link(
        &path,
        &inventory.base_generation_id,
        &plan.base_generation_id,
        "base_generation_id",
    )?;
    require_link(&path, &inventory.tail_id, &tail.tail_id, "tail_id")?;
    if inventory.segments != expected {
        return Err(invalid(
            &path,
            "candidate inventory is not the exact range selection",
        ));
    }
    if require_candidate_archive {
        let archive = crate::archive_set::ArchiveSetReader::open(paths.candidate_archive())
            .map_err(|error| invalid(paths.candidate_archive(), error.to_string()))?;
        let data = archive
            .segments()
            .iter()
            .filter(|segment| segment.kind.is_some())
            .collect::<Vec<_>>();
        if data.len() != inventory.segments.len()
            || data
                .iter()
                .zip(&inventory.segments)
                .any(|(segment, selected)| {
                    segment.name != selected.name || segment.bytes != selected.bytes
                })
        {
            return Err(invalid(
                paths.candidate_archive(),
                "candidate paths do not match the selected range inventory",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_prepared_generation(
    paths: &UpdatePaths,
    plan: &UpdatePlanReceipt,
    generation: &PreparedGenerationReceipt,
    candidate_is_installed: bool,
) -> Result<(), InvalidUpdateState> {
    let path = paths.prepared_generation();
    require_schema(&path, generation.schema)?;
    require_link(&path, &generation.update_id, &plan.update_id, "update_id")?;
    require_link(
        &path,
        &generation.base_generation_id,
        &plan.base_generation_id,
        "base_generation_id",
    )?;
    if generation.generation_id.is_empty()
        || generation.archive_name != "archive.swdump"
        || generation.index_name != "archive.swtitle"
    {
        return Err(invalid(&path, "prepared generation is malformed"));
    }
    require_link(
        &path,
        &generation.generation_id,
        &plan.new_generation_id,
        "generation_id",
    )?;
    let legacy_backrefs = generation.backrefs_name.is_empty()
        && generation.backrefs_bytes == 0
        && generation.backrefs_records == 0;
    if !legacy_backrefs
        && (generation.backrefs_name != "backrefs.swrefs" || generation.backrefs_bytes == 0)
    {
        return Err(invalid(&path, "prepared backref sidecar is malformed"));
    }
    if !candidate_is_installed && !legacy_backrefs {
        let backrefs = paths.candidate_backrefs();
        require_file(&backrefs, Some(generation.backrefs_bytes))?;
        let index = crate::backrefs::BackrefIndex::open_for_title_index(
            &backrefs,
            paths.candidate_index(),
        )
        .map_err(|error| invalid(&backrefs, error.to_string()))?;
        if !index.has_raw_postings() {
            return Err(invalid(
                &backrefs,
                "prepared backref sidecar lacks raw postings",
            ));
        }
        if index.logical_count() != generation.backrefs_records {
            return Err(invalid(
                &backrefs,
                "prepared backref sidecar logical-count metadata disagrees",
            ));
        }
    }
    Ok(())
}

/// Interpret the last durable update state without mutating any artifact.
pub(crate) fn inspect_update(
    paths: &UpdatePaths,
    installed_generation_id: &str,
) -> Result<UpdateState, InvalidUpdateState> {
    let mut phase = UpdatePhase::Unplanned;
    let plan_path = paths.plan();
    let plan: UpdatePlanReceipt = read_receipt(&plan_path)?
        .ok_or_else(|| invalid(&plan_path, "update root has no plan receipt"))?;
    validate_plan(paths, &plan)?;
    phase = observed_transition(&plan_path, phase, UpdateEvent::PublishPlan)?;

    let Some(tail) = read_receipt::<TailReceipt>(&paths.tail_receipt())? else {
        require_link(
            &plan_path,
            installed_generation_id,
            &plan.base_generation_id,
            "installed generation",
        )?;
        return Ok(UpdateState::Planned(plan));
    };
    let tail_directory = validate_tail(paths, &plan, &tail)?;
    phase = observed_transition(&paths.tail_receipt(), phase, UpdateEvent::PublishTail)?;
    let Some(base) = read_receipt::<PreservedBaseReceipt>(&paths.base_receipt())? else {
        require_link(
            &paths.tail_receipt(),
            installed_generation_id,
            &plan.base_generation_id,
            "installed generation",
        )?;
        return Ok(UpdateState::TailReady(plan, tail));
    };
    validate_preserved_base_receipt(paths, &plan, &base)?;
    phase = observed_transition(&paths.base_receipt(), phase, UpdateEvent::PreserveBase)?;
    let Some(site_info) = read_receipt::<BaseSiteInfoCheckpoint>(&paths.base_site_info())? else {
        if paths
            .range_plan()
            .parent()
            .is_some_and(|ranges| ranges.exists())
        {
            return Err(invalid(
                paths.base_site_info(),
                "range state exists without the required base SiteInfo checkpoint",
            ));
        }
        validate_preserved_base(paths, &plan, &base)?;
        require_link(
            &paths.base_receipt(),
            installed_generation_id,
            &plan.base_generation_id,
            "installed generation",
        )?;
        return Ok(UpdateState::BasePreserved(plan, tail, base));
    };
    validate_base_site_info(paths, &plan, &site_info)?;
    phase = observed_transition(
        &paths.base_site_info(),
        phase,
        UpdateEvent::PublishBaseSiteInfo,
    )?;
    let Some(ranges) = read_receipt::<RangePlanReceipt>(&paths.range_plan())? else {
        validate_preserved_base(paths, &plan, &base)?;
        require_link(
            &paths.base_site_info(),
            installed_generation_id,
            &plan.base_generation_id,
            "installed generation",
        )?;
        return Ok(UpdateState::BaseSiteInfoReady {
            plan,
            tail,
            preserved_base: base,
            site_info,
        });
    };
    validate_range_plan(paths, &plan, &tail, &ranges)?;
    if read_receipt::<RangeCandidateReceipt>(&paths.range_receipt(0))?.is_none() {
        validate_preserved_base(paths, &plan, &base)?;
    } else {
        validate_preserved_base_index(paths, &base)?;
    }
    phase = observed_transition(&paths.range_plan(), phase, UpdateEvent::PublishRangePlan)?;

    let mut selected = Vec::with_capacity(ranges.slots.len());
    let mut previous_cursor = (0_usize, 0_u64);
    for slot in &ranges.slots {
        let Some(receipt) =
            read_receipt::<RangeCandidateReceipt>(&paths.range_receipt(slot.index))?
        else {
            require_link(
                &paths.range_plan(),
                installed_generation_id,
                &plan.base_generation_id,
                "installed generation",
            )?;
            return Ok(UpdateState::ApplyingRanges {
                plan,
                tail,
                preserved_base: base,
                site_info,
                ranges,
                completed: selected.len(),
            });
        };
        selected.push(validate_range_receipt(
            paths,
            &plan,
            &tail,
            &tail_directory,
            slot,
            &receipt,
        )?);
        let cursor = tail_cursor_order(&tail_directory, &receipt.tail_cursor)
            .ok_or_else(|| invalid(paths.range_receipt(slot.index), "invalid tail cursor"))?;
        if cursor < previous_cursor {
            return Err(invalid(
                paths.range_receipt(slot.index),
                "range receipts move the tail cursor backwards",
            ));
        }
        previous_cursor = cursor;
        let event = if selected.len() == ranges.slots.len() {
            UpdateEvent::PublishFinalRange
        } else {
            UpdateEvent::PublishRange
        };
        phase = observed_transition(&paths.range_receipt(slot.index), phase, event)?;
    }
    let Some(inventory) = read_receipt::<CandidateInventoryReceipt>(&paths.candidate_inventory())?
    else {
        require_link(
            &paths.range_plan(),
            installed_generation_id,
            &plan.base_generation_id,
            "installed generation",
        )?;
        return Ok(UpdateState::ApplyingRanges {
            plan,
            tail,
            preserved_base: base,
            site_info,
            ranges,
            completed: selected.len(),
        });
    };
    // Generic installation may retire its candidate links after the immutable
    // generation is selected. Receipts still bind the exact range inventory;
    // the caller validates the selected generation itself before commit.
    let candidate_is_installed = installed_generation_id == plan.new_generation_id;
    validate_inventory(
        paths,
        &plan,
        &tail,
        &selected,
        &inventory,
        !candidate_is_installed,
    )?;
    phase = observed_transition(
        &paths.candidate_inventory(),
        phase,
        UpdateEvent::PublishInventory,
    )?;
    let Some(generation) = read_receipt::<PreparedGenerationReceipt>(&paths.prepared_generation())?
    else {
        require_link(
            &paths.candidate_inventory(),
            installed_generation_id,
            &plan.base_generation_id,
            "installed generation",
        )?;
        return Ok(UpdateState::CandidateComplete {
            plan,
            tail,
            preserved_base: base,
            site_info,
            ranges,
            inventory,
        });
    };
    validate_prepared_generation(
        paths,
        &plan,
        &generation,
        candidate_is_installed,
    )?;
    if !candidate_is_installed {
        require_file(&paths.candidate_index(), Some(generation.index_bytes))?;
    }
    phase = observed_transition(
        &paths.prepared_generation(),
        phase,
        UpdateEvent::PublishIndex,
    )?;
    if installed_generation_id == plan.base_generation_id {
        if read_receipt::<CommitReceipt>(&paths.commit_receipt())?.is_some() {
            return Err(invalid(
                paths.commit_receipt(),
                "commit receipt exists while the base generation is installed",
            ));
        }
        return Ok(UpdateState::IndexReady {
            plan,
            tail,
            preserved_base: base,
            site_info,
            inventory,
            generation,
        });
    }
    require_link(
        &paths.prepared_generation(),
        installed_generation_id,
        &generation.generation_id,
        "installed generation",
    )?;
    phase = observed_transition(
        &paths.prepared_generation(),
        phase,
        UpdateEvent::InstallGeneration,
    )?;
    let Some(commit) = read_receipt::<CommitReceipt>(&paths.commit_receipt())? else {
        return Ok(UpdateState::Installed { plan, generation });
    };
    require_schema(&paths.commit_receipt(), commit.schema)?;
    require_link(
        &paths.commit_receipt(),
        &commit.update_id,
        &plan.update_id,
        "update_id",
    )?;
    require_link(
        &paths.commit_receipt(),
        &commit.old_generation_id,
        &plan.base_generation_id,
        "old_generation_id",
    )?;
    require_link(
        &paths.commit_receipt(),
        &commit.new_generation_id,
        &plan.new_generation_id,
        "new_generation_id",
    )?;
    let _ = observed_transition(&paths.commit_receipt(), phase, UpdateEvent::PublishCommit)?;
    Ok(UpdateState::Committed(commit))
}

fn observed_transition(
    path: &Path,
    state: UpdatePhase,
    event: UpdateEvent,
) -> Result<UpdatePhase, InvalidUpdateState> {
    match transition(state, event) {
        TransitionDecision::Advance(next) => Ok(next),
        TransitionDecision::NoOp => Ok(state),
        TransitionDecision::Reject(reason) => Err(invalid(
            path,
            format!("out-of-order lifecycle event {event:?}: {reason:?}"),
        )),
        TransitionDecision::Impossible(reason) => Err(invalid(
            path,
            format!("impossible lifecycle event {event:?}: {reason:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    fn write(path: &Path, value: &impl Serialize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn source_and_plan() -> (crate::direct::UpdateSourcePlan, UpdatePlanReceipt) {
        let base_generation_id = crate::generation::GenerationId::from_plan_bytes(b"test base");
        let mut source = crate::direct::UpdateSourcePlan {
            schema: UPDATE_SCHEMA,
            source_plan_id: String::new(),
            generation_id: crate::generation::GenerationId::from_plan_bytes(&[]),
            base_generation_id: base_generation_id.clone(),
            wiki_db: "lvwiki".into(),
            base_content_frontier: "2026-07-01".into(),
            base_metadata_frontier: "2026-07".into(),
            overlap_days: 3,
            frame_target: 131_072,
            compression: crate::archive::CompressionSettings::default().into(),
            content_runs: Vec::new(),
            history_snapshot: "2026-07".into(),
            history_files: Vec::new(),
            resulting_content_frontier: "2026-07-03".into(),
            resulting_metadata_frontier: "2026-07".into(),
        };
        let bytes = serde_json::to_vec(&source).unwrap();
        source.source_plan_id = hex::encode(sha2::Sha256::digest(bytes));
        let mut generation = b"wikipedia-update-generation\0".to_vec();
        generation.extend_from_slice(base_generation_id.as_str().as_bytes());
        generation.push(0);
        generation.extend_from_slice(source.source_plan_id.as_bytes());
        source.generation_id = crate::generation::GenerationId::from_plan_bytes(&generation);
        let compression: crate::archive::CompressionSettings = source.compression.into();
        let plan = UpdatePlanReceipt {
            schema: UPDATE_SCHEMA,
            update_id: source.source_plan_id.clone(),
            base_generation_id: source.base_generation_id.as_str().into(),
            new_generation_id: source.generation_id.as_str().into(),
            source_plan_id: source.source_plan_id.clone(),
            wiki_db: source.wiki_db.clone(),
            base_content_frontier: source.base_content_frontier.clone(),
            base_metadata_frontier: source.base_metadata_frontier.clone(),
            result_content_frontier: source.resulting_content_frontier.clone(),
            result_metadata_frontier: source.resulting_metadata_frontier.clone(),
            overlap_days: source.overlap_days,
            frame_target: source.frame_target,
            compression: CompressionReceipt::from(compression),
        };
        (source, plan)
    }

    fn write_plan(paths: &UpdatePaths) -> UpdatePlanReceipt {
        let (source, plan) = source_and_plan();
        write(&paths.source_plan(), &source);
        write(&paths.plan(), &plan);
        plan
    }

    fn base_id() -> String {
        source_and_plan().0.base_generation_id.as_str().to_owned()
    }

    fn update_id() -> String {
        source_and_plan().0.source_plan_id
    }

    fn new_id() -> String {
        source_and_plan().0.generation_id.as_str().to_owned()
    }

    fn plan() -> UpdatePlanReceipt {
        source_and_plan().1
    }

    fn make_complete_archive(path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        crate::archive::ArchiveWriter::new(file, 1024)
            .unwrap()
            .finish()
            .unwrap();
    }

    fn make_base_generation(paths: &UpdatePaths) -> crate::generation::GenerationIdentity {
        std::fs::create_dir_all(paths.base_archive().parent().unwrap()).unwrap();
        let output =
            crate::archive_set::ArchiveSetOutput::new_in(paths.root.as_path(), 1 << 20).unwrap();
        let prefix = vec![b'x'; 256];
        let mut writer = crate::archive::ArchiveWriter::with_ref_prefix(
            output,
            1024,
            crate::archive::CompressionSettings::default(),
            &prefix,
        )
        .unwrap();
        writer
            .write(&crate::archive::Record::PageState {
                page_id: 1,
                timestamp_micros: 100,
                title: "One".into(),
                namespace: None,
                deleted: false,
            })
            .unwrap();
        writer
            .write(&crate::archive::Record::Manifest {
                timestamp_micros: 100,
                manifest: crate::archive::ManifestRecord {
                    wiki_db: "lvwiki".into(),
                    content_snapshot: "2026-07-01".into(),
                    metadata_snapshot: "2026-07".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&crate::archive::Record::SiteInfo {
                timestamp_micros: 100,
                site_info: crate::archive::SiteInfoRecord {
                    site_name: "Test".into(),
                    db_name: "lvwiki".into(),
                    base: String::new(),
                    generator: String::new(),
                    case: "first-letter".into(),
                    language: "lv".into(),
                    rtl: false,
                    server: String::new(),
                    script_path: String::new(),
                    namespaces: Vec::new(),
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (output, _) = writer.finish().unwrap();
        output
            .finish()
            .unwrap()
            .persist(paths.base_archive())
            .unwrap();
        crate::title_index::build(
            paths.base_archive(),
            paths.base_index(),
            &source_and_plan().0.base_generation_id,
        )
        .unwrap();
        crate::generation::generation_identity(paths.base_archive(), paths.base_index()).unwrap()
    }

    fn base_site_info(plan: &UpdatePlanReceipt) -> BaseSiteInfoCheckpoint {
        BaseSiteInfoCheckpoint::new(
            plan,
            crate::archive::SiteInfoRecord {
                site_name: "Test".into(),
                db_name: "lvwiki".into(),
                base: String::new(),
                generator: String::new(),
                case: "first-letter".into(),
                language: "lv".into(),
                rtl: false,
                server: String::new(),
                script_path: String::new(),
                namespaces: Vec::new(),
                interwiki: Vec::new(),
                magic_words: Vec::new(),
            },
        )
    }

    fn hard_link_set(source: &Path, destination: &Path) {
        std::fs::create_dir_all(destination).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            std::fs::hard_link(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }

    fn tail(paths: &UpdatePaths) -> TailReceipt {
        let plan = plan();
        std::fs::create_dir_all(paths.tail_archive().parent().unwrap()).unwrap();
        make_complete_archive(&paths.tail_archive());
        let tail_id = crate::generation::GenerationId::from_plan_bytes(b"tail-a");
        let frame_directory = crate::frame_directory::write_from_archive(
            paths.tail_archive(),
            paths.tail_frame_directory(),
            tail_id.to_bytes().unwrap(),
        )
        .unwrap();
        TailReceipt {
            schema: UPDATE_SCHEMA,
            update_id: plan.update_id,
            base_generation_id: plan.base_generation_id,
            source_plan_id: plan.source_plan_id,
            tail_id: tail_id.as_str().into(),
            file_name: "records.swdump".into(),
            bytes: std::fs::metadata(paths.tail_archive()).unwrap().len(),
            frame_directory_name: "frames.swframe".into(),
            frame_directory_format: crate::frame_directory::FORMAT_VERSION,
            frame_directory_bytes: frame_directory.bytes,
            frames: 0,
            records: 0,
            first_entity: None,
            last_entity: None,
            complete: true,
        }
    }

    #[test]
    fn inspector_advances_only_at_receipt_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(root.path());
        write_plan(&paths);
        assert!(matches!(
            inspect_update(&paths, &base_id()).unwrap(),
            UpdateState::Planned(_)
        ));

        let tail = tail(&paths);
        write(&paths.tail_receipt(), &tail);
        assert!(matches!(
            inspect_update(&paths, &base_id()).unwrap(),
            UpdateState::TailReady(_, _)
        ));
    }

    #[test]
    fn foreign_tail_is_invalid_not_recovered() {
        let root = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(root.path());
        write_plan(&paths);
        let mut tail = tail(&paths);
        tail.update_id = "another-update".into();
        write(&paths.tail_receipt(), &tail);
        assert!(inspect_update(&paths, &base_id())
            .unwrap_err()
            .diagnostic
            .contains("update_id"));
    }

    #[test]
    fn active_update_cannot_change_its_installed_base() {
        let root = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(root.path());
        write_plan(&paths);
        assert!(inspect_update(&paths, "generation-b")
            .unwrap_err()
            .diagnostic
            .contains("installed generation"));
    }

    #[test]
    fn commit_without_its_predecessor_receipts_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(root.path());
        write_plan(&paths);
        write(
            &paths.commit_receipt(),
            &CommitReceipt {
                schema: UPDATE_SCHEMA,
                update_id: update_id(),
                old_generation_id: base_id(),
                new_generation_id: new_id(),
            },
        );
        assert!(matches!(
            inspect_update(&paths, &base_id()).unwrap(),
            UpdateState::Planned(_)
        ));
        assert!(inspect_update(&paths, &new_id()).is_err());
    }

    #[test]
    fn inspector_transition_table_has_no_path_inference() {
        let root = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(root.path());
        let source_plan = write_plan(&paths);
        let tail = tail(&paths);
        write(&paths.tail_receipt(), &tail);
        let generation = make_base_generation(&paths);
        let preserved = PreservedBaseReceipt {
            schema: UPDATE_SCHEMA,
            update_id: source_plan.update_id.clone(),
            generation: generation.clone(),
            archive_name: "archive.swdump".into(),
            index_name: "archive.swtitle".into(),
        };
        write(&paths.base_receipt(), &preserved);
        assert!(matches!(
            inspect_update(&paths, generation.generation_id.as_str()).unwrap(),
            UpdateState::BasePreserved(..)
        ));

        let site_info = base_site_info(&source_plan);
        write(&paths.base_site_info(), &site_info);
        assert!(matches!(
            inspect_update(&paths, generation.generation_id.as_str()).unwrap(),
            UpdateState::BaseSiteInfoReady { .. }
        ));

        let set = crate::archive_set::ArchiveSetReader::open(paths.base_archive()).unwrap();
        let slots = set
            .segments()
            .iter()
            .filter_map(|segment| {
                segment.kind.map(|kind| {
                    let index = 0;
                    RangeSlot {
                        index,
                        kind: kind as u8,
                        first_id: segment.first_id,
                        last_id: segment.last_id,
                        base_segment_id: format!("base-{}", kind as u8),
                        base_name: segment.name.clone(),
                        base_bytes: segment.bytes,
                        candidate_id: format!("candidate-{}", kind as u8),
                    }
                })
            })
            .enumerate()
            .map(|(index, mut slot)| {
                slot.index = index;
                slot
            })
            .collect::<Vec<_>>();
        let ranges = RangePlanReceipt {
            schema: UPDATE_SCHEMA,
            update_id: source_plan.update_id.clone(),
            base_generation_id: source_plan.base_generation_id.clone(),
            tail_id: tail.tail_id.clone(),
            slots,
        };
        write(&paths.range_plan(), &ranges);
        assert!(matches!(
            inspect_update(&paths, generation.generation_id.as_str()).unwrap(),
            UpdateState::ApplyingRanges { completed: 0, .. }
        ));

        let mut selected = Vec::new();
        for slot in &ranges.slots {
            let receipt = RangeCandidateReceipt {
                schema: UPDATE_SCHEMA,
                update_id: ranges.update_id.clone(),
                base_generation_id: ranges.base_generation_id.clone(),
                tail_id: ranges.tail_id.clone(),
                slot_index: slot.index,
                candidate_id: slot.candidate_id.clone(),
                kind: slot.kind,
                first_id: slot.first_id,
                last_id: slot.last_id,
                base_segment_id: slot.base_segment_id.clone(),
                selection: RangeSelection::Unchanged {
                    segment_id: slot.base_segment_id.clone(),
                    name: slot.base_name.clone(),
                    bytes: slot.base_bytes,
                },
                consumed_first: None,
                consumed_last: None,
                tail_bytes_read: 0,
                base_bytes_read: 0,
                base_frame_bytes_copied: 0,
                base_frame_bytes_decoded: 0,
                candidate_bytes_written: 0,
                title_projection_name: None,
                title_projection_bytes: 0,
                title_projection_records: 0,
                backref_delta_name: None,
                backref_delta_bytes: 0,
                backref_delta_records: 0,
                tail_cursor: TailCursorReceipt {
                    frame_offset: None,
                    record_ordinal: 0,
                },
                complete: true,
            };
            write(&paths.range_receipt(slot.index), &receipt);
            selected.push(SelectedSegment {
                slot_index: slot.index,
                segment_id: slot.base_segment_id.clone(),
                name: slot.base_name.clone(),
                bytes: slot.base_bytes,
            });
            assert!(matches!(
                inspect_update(&paths, generation.generation_id.as_str()).unwrap(),
                UpdateState::ApplyingRanges { completed, .. }
                    if completed == slot.index + 1
            ));
        }
        assert!(matches!(
            inspect_update(&paths, generation.generation_id.as_str()).unwrap(),
            UpdateState::ApplyingRanges { completed, .. } if completed == ranges.slots.len()
        ));

        hard_link_set(&paths.base_archive(), &paths.candidate_archive());
        let inventory = CandidateInventoryReceipt {
            schema: UPDATE_SCHEMA,
            update_id: ranges.update_id.clone(),
            base_generation_id: ranges.base_generation_id.clone(),
            tail_id: ranges.tail_id.clone(),
            segments: selected,
        };
        write(&paths.candidate_inventory(), &inventory);
        assert!(matches!(
            inspect_update(&paths, generation.generation_id.as_str()).unwrap(),
            UpdateState::CandidateComplete { .. }
        ));

        crate::title_index::build(
            paths.candidate_archive(),
            paths.candidate_index(),
            &source_and_plan().0.generation_id,
        )
        .unwrap();
        crate::backrefs::build(
            paths.candidate_archive(),
            paths.candidate_index(),
            paths.candidate_backrefs(),
        )
        .unwrap();
        let prepared_backrefs =
            crate::backrefs::BackrefIndex::open_for_title_index(
                paths.candidate_backrefs(),
                paths.candidate_index(),
            )
            .unwrap();
        let prepared = PreparedGenerationReceipt {
            schema: UPDATE_SCHEMA,
            update_id: source_plan.update_id.clone(),
            base_generation_id: source_plan.base_generation_id.clone(),
            generation_id: source_plan.new_generation_id.clone(),
            archive_name: "archive.swdump".into(),
            index_name: "archive.swtitle".into(),
            index_bytes: std::fs::metadata(paths.candidate_index()).unwrap().len(),
            backrefs_name: "backrefs.swrefs".into(),
            backrefs_bytes: std::fs::metadata(paths.candidate_backrefs()).unwrap().len(),
            backrefs_records: prepared_backrefs.logical_count(),
        };
        write(&paths.prepared_generation(), &prepared);
        assert!(matches!(
            inspect_update(&paths, generation.generation_id.as_str()).unwrap(),
            UpdateState::IndexReady { .. }
        ));
        assert!(matches!(
            inspect_update(&paths, &prepared.generation_id).unwrap(),
            UpdateState::Installed { .. }
        ));
        let commit = CommitReceipt {
            schema: UPDATE_SCHEMA,
            update_id: source_plan.update_id.clone(),
            old_generation_id: source_plan.base_generation_id.clone(),
            new_generation_id: source_plan.new_generation_id.clone(),
        };
        write(&paths.commit_receipt(), &commit);
        assert!(matches!(
            inspect_update(&paths, &prepared.generation_id).unwrap(),
            UpdateState::Committed(_)
        ));
    }

    #[test]
    fn range_state_without_site_info_checkpoint_fails_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(root.path());
        let plan = write_plan(&paths);
        let tail = tail(&paths);
        write(&paths.tail_receipt(), &tail);
        let generation = make_base_generation(&paths);
        write(
            &paths.base_receipt(),
            &PreservedBaseReceipt {
                schema: UPDATE_SCHEMA,
                update_id: plan.update_id,
                generation: generation.clone(),
                archive_name: "archive.swdump".into(),
                index_name: "archive.swtitle".into(),
            },
        );
        std::fs::create_dir_all(paths.range_plan().parent().unwrap()).unwrap();
        let sentinel = paths.range_plan().parent().unwrap().join("restart-cut");
        std::fs::write(&sentinel, b"preserve").unwrap();

        let error = inspect_update(&paths, generation.generation_id.as_str()).unwrap_err();

        assert_eq!(error.path, paths.base_site_info());
        assert!(error.diagnostic.contains("without the required base SiteInfo checkpoint"));
        assert_eq!(std::fs::read(sentinel).unwrap(), b"preserve");
        assert!(paths.base_receipt().is_file());
    }

    #[test]
    fn stale_site_info_checkpoint_is_rejected_at_its_identity_boundary() {
        let root = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(root.path());
        let plan = write_plan(&paths);
        let tail = tail(&paths);
        write(&paths.tail_receipt(), &tail);
        let generation = make_base_generation(&paths);
        write(
            &paths.base_receipt(),
            &PreservedBaseReceipt {
                schema: UPDATE_SCHEMA,
                update_id: plan.update_id.clone(),
                generation: generation.clone(),
                archive_name: "archive.swdump".into(),
                index_name: "archive.swtitle".into(),
            },
        );
        let mut checkpoint = base_site_info(&plan);
        checkpoint.source_plan_id = "another-source-plan".into();
        write(&paths.base_site_info(), &checkpoint);

        let error = inspect_update(&paths, generation.generation_id.as_str()).unwrap_err();

        assert_eq!(error.path, paths.base_site_info());
        assert!(error.diagnostic.contains("source_plan_id"));
        assert_eq!(read_receipt::<BaseSiteInfoCheckpoint>(&paths.base_site_info()).unwrap(), Some(checkpoint));
    }

    #[test]
    fn every_phase_event_pair_has_an_explicit_transition_classification() {
        use Impossibility::{EventAfterCleanup, RangeAfterFinalRange};
        use TransitionDecision::{Advance, Impossible, NoOp, Reject};
        use UpdateEvent::*;
        use UpdatePhase::*;

        let phases = [
            Unplanned,
            Planned,
            TailReady,
            BasePreserved,
            BaseSiteInfoReady,
            ApplyingRanges,
            RangesApplied,
            CandidateComplete,
            IndexReady,
            Installed,
            Committed,
            Cleaned,
        ];
        let events = [
            PublishPlan,
            PublishTail,
            PreserveBase,
            PublishBaseSiteInfo,
            PublishRangePlan,
            PublishRange,
            PublishFinalRange,
            PublishInventory,
            PublishIndex,
            InstallGeneration,
            PublishCommit,
            Cleanup,
        ];
        let rejected = Reject(Rejection::OutOfOrder);
        let after_final = Impossible(RangeAfterFinalRange);
        let after_cleanup = Impossible(EventAfterCleanup);
        let expected = [
            [Advance(Planned), rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected],
            [NoOp, Advance(TailReady), rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected],
            [NoOp, NoOp, Advance(BasePreserved), rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected],
            [NoOp, NoOp, NoOp, Advance(BaseSiteInfoReady), rejected, rejected, rejected, rejected, rejected, rejected, rejected, rejected],
            [NoOp, NoOp, NoOp, NoOp, Advance(ApplyingRanges), rejected, rejected, rejected, rejected, rejected, rejected, rejected],
            [NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, Advance(RangesApplied), rejected, rejected, rejected, rejected, rejected],
            [NoOp, NoOp, NoOp, NoOp, NoOp, after_final, NoOp, Advance(CandidateComplete), rejected, rejected, rejected, rejected],
            [NoOp, NoOp, NoOp, NoOp, NoOp, after_final, NoOp, NoOp, Advance(IndexReady), rejected, rejected, rejected],
            [NoOp, NoOp, NoOp, NoOp, NoOp, after_final, NoOp, NoOp, NoOp, Advance(Installed), rejected, rejected],
            [NoOp, NoOp, NoOp, NoOp, NoOp, after_final, NoOp, NoOp, NoOp, NoOp, Advance(Committed), rejected],
            [NoOp, NoOp, NoOp, NoOp, NoOp, after_final, NoOp, NoOp, NoOp, NoOp, NoOp, Advance(Cleaned)],
            [after_cleanup, after_cleanup, after_cleanup, after_cleanup, after_cleanup, after_cleanup, after_cleanup, after_cleanup, after_cleanup, after_cleanup, after_cleanup, NoOp],
        ];
        for (state_index, state) in phases.into_iter().enumerate() {
            for (event_index, event) in events.into_iter().enumerate() {
                assert_eq!(
                    transition(state, event),
                    expected[state_index][event_index],
                    "{state:?} + {event:?}"
                );
            }
        }

        let events = [
            DiscoveryFailed,
            SourceGapDetected,
            WorkerFailed,
            CancelRequested,
            ProcessCrashed,
            ResumeRequested,
            RetryRequested,
            DuplicateReceipt,
            StaleReceipt,
            ForeignReceipt,
            InstallInterrupted,
            CleanupFailed,
        ];
        let r = Reject(Rejection::OutOfOrder);
        let x = Impossible(EventAfterCleanup);
        let ordinary = [
            NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, r, r, r, r,
        ];
        let expected = [
            [NoOp, NoOp, NoOp, NoOp, NoOp, r, r, r, r, r, r, r],
            ordinary,
            ordinary,
            ordinary,
            ordinary,
            ordinary,
            ordinary,
            ordinary,
            [NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, r, r, NoOp, r],
            [NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, r, r, NoOp, r],
            [NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, NoOp, r, r, r, NoOp],
            [x, x, x, x, x, r, r, r, r, r, x, x],
        ];
        for (state_index, state) in phases.into_iter().enumerate() {
            for (event_index, event) in events.into_iter().enumerate() {
                assert_eq!(
                    transition(state, event),
                    expected[state_index][event_index],
                    "{state:?} + {event:?}"
                );
            }
        }
    }

    #[test]
    fn tail_cursor_resume_replays_only_its_single_bounded_frame() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("tail.swdump");
        let directory_path = root.path().join("tail.swframe");
        let file = std::fs::File::create(&archive).unwrap();
        let mut writer = crate::archive::ArchiveWriter::new(file, 1).unwrap();
        for (page_id, timestamp_micros) in
            [(1, 300), (1, 200), (1, 100), (2, 300), (2, 200), (3, 100)]
        {
            writer
                .write(&crate::archive::Record::PageState {
                    page_id,
                    timestamp_micros,
                    title: format!("Page {page_id}"),
                    namespace: None,
                    deleted: false,
                })
                .unwrap();
        }
        writer.finish().unwrap();
        let identity = crate::generation::GenerationId::from_plan_bytes(b"cursor-test");
        crate::frame_directory::write_from_archive(
            &archive,
            &directory_path,
            identity.to_bytes().unwrap(),
        )
        .unwrap();
        let directory = std::sync::Arc::new(
            crate::frame_directory::FrameDirectory::open_bound(
                &directory_path,
                identity.to_bytes().unwrap(),
            )
            .unwrap(),
        );
        assert!(directory.len() >= 2);

        let mut first = crate::archive::ArchiveRecordReader::open_frame_directory(
            &archive,
            std::sync::Arc::clone(&directory),
            0,
        )
        .unwrap();
        first.next_record().unwrap().unwrap();
        first.next_record().unwrap().unwrap();
        let frame_offset = first.current_frame_offset().unwrap();
        let ordinal = first.current_frame_records_read();
        let position = directory.index_of_offset(frame_offset).unwrap();
        let frame = directory.get(position).unwrap();
        assert!(ordinal < frame.records);
        let expected = first.next_record().unwrap().unwrap();

        let mut resumed = crate::archive::ArchiveRecordReader::open_frame_directory(
            &archive,
            std::sync::Arc::clone(&directory),
            position,
        )
        .unwrap();
        let mut replayed = 0_u64;
        while replayed < ordinal {
            resumed.next_record().unwrap().unwrap();
            replayed += 1;
        }
        assert_eq!(replayed, ordinal);
        assert!(replayed < frame.records);
        assert_eq!(resumed.current_frame_offset(), Some(frame_offset));
        assert_eq!(
            resumed.remaining_frame_count(),
            directory.len() - position - 1
        );
        let actual = resumed.next_record().unwrap().unwrap();
        assert_eq!(actual.entity(), expected.entity());
    }
}
