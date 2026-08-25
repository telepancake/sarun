//! # wikimak-wikipedia
//!
//! Portable Wikipedia archive construction, update, lookup, and serving.
//! A `.swdump` mirror is a directory of bounded page-ID range files whose
//! lexical concatenation is one event stream. One generated `.swtitle` file
//! indexes titles, frames, and physical ranges.

pub mod asof;
pub mod archive;
pub mod archive_set;
pub mod backrefs;
mod backrefs_parse;
#[cfg(feature = "fetch")]
pub(crate) mod build_lifecycle;
#[cfg(feature = "fetch")]
pub(crate) mod installation_lifecycle;
#[cfg(feature = "serve")]
pub mod archive_browse;
#[cfg(feature = "fetch")]
pub mod direct;
#[cfg(feature = "fetch")]
mod progress_projection;
#[cfg(feature = "fetch")]
mod cli;
#[cfg(feature = "fetch")]
pub use cli::{
    cli_main, mirror_auxiliary_paths, mirror_has_installed_generation, mirror_scratch_path,
};
#[cfg(feature = "fetch")]
pub use installation_lifecycle::{GenerationCleanupReport, reclaim_installed_generation};
pub mod error;
pub(crate) mod frame_directory;
pub mod generation;
pub(crate) mod frames;
pub mod import;
pub mod instance;
pub mod readout;
pub mod revision;
pub(crate) mod revision_merge;
#[cfg(feature = "serve")]
pub mod serve;
pub mod schema;
pub mod title_slots;
pub mod title_index;
pub(crate) mod title_projection;
pub(crate) mod title_history;
#[cfg(feature = "fetch")]
pub mod sync;
#[cfg(feature = "fetch")]
pub mod siteinfo;
pub(crate) mod titles;

pub use error::{Error, Result};
#[cfg(feature = "fetch")]
pub use direct::{
    build_direct_archive, build_update_archive, DirectArchiveStats, MirrorBuildProgress,
    MirrorTargetProgress, UpdateArchiveStats,
};
#[cfg(feature = "fetch")]
pub use progress_projection::{mirror_build_progress, mirror_build_progress_for_run};
pub use instance::{
    max_chain_id_for_root, read_config, ContributorMeta, HistoryEntry, HistoryIter, ImportStats,
    Instance, InstanceConfig, PackedF0StorageStats, PageAction, RevisionCorrection, RevisionDictionaryStats,
    RevisionMeta, RevisionVisibility, SplitRevisionStorageStats, DEFAULT_MAX_CHAIN_ID,
};

/// The one packed image repository shared by mirrors stored in one library
/// directory.  Per-mirror `<dbname>.media` remains the blob/negative cache;
/// it is deliberately not this repository.
#[cfg(feature = "serve")]
pub fn shared_packed_media_path(archive: &std::path::Path) -> std::path::PathBuf {
    archive
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("wikimedia.media")
}

/// Return whether `path` is a usable packed-media catalogue.  Merely finding
/// a directory or a `.data` file is insufficient: all discovered storages
/// must have valid hash/offset companions and indexes.
#[cfg(feature = "serve")]
pub fn packed_media_directory_is_valid(path: &std::path::Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    wikimak_media::PackedMediaCatalog::open_directory(path)
        .map(|catalog| !catalog.storages().is_empty())
        .unwrap_or(false)
}

/// Select the packed catalogue for an archive.  An explicit CLI option is
/// authoritative even when it is invalid (the serving layer reports that
/// error).  Automatic selection uses the shared repository first, then the
/// pre-shared per-mirror packed layout for compatibility.
#[cfg(feature = "serve")]
pub fn resolve_packed_media_path(
    archive: &std::path::Path,
    explicit: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let shared = shared_packed_media_path(archive);
    if packed_media_directory_is_valid(&shared) {
        return Some(shared);
    }
    let legacy = archive.with_extension("media");
    packed_media_directory_is_valid(&legacy).then_some(legacy)
}

#[cfg(feature = "fetch")]
pub use sync::{maintain, reconcile_history, sync, SyncStats};
pub use revision::{
    FLAG_COMMENT_HIDDEN, FLAG_CONTRIBUTOR_HIDDEN, FLAG_SHA1_MISMATCH, FLAG_SUPPRESSED,
    FLAG_TEXT_HIDDEN, KIND_ANONYMOUS, KIND_HIDDEN, KIND_NAMED, REVISION_SCHEMA_VERSION,
};
