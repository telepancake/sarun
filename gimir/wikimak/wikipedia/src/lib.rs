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
#[cfg(feature = "fetch")]
pub use sync::{maintain, reconcile_history, sync, SyncStats};
pub use revision::{
    FLAG_COMMENT_HIDDEN, FLAG_CONTRIBUTOR_HIDDEN, FLAG_SHA1_MISMATCH, FLAG_SUPPRESSED,
    FLAG_TEXT_HIDDEN, KIND_ANONYMOUS, KIND_HIDDEN, KIND_NAMED, REVISION_SCHEMA_VERSION,
};
