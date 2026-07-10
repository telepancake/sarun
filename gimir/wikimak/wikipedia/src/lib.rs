//! # wikimak-wikipedia
//!
//! Wikipedia-specific glue. Per-instance depot binding, titles pool, page
//! importer that feeds a `mediawiki::PageStream` into a `depot` chain.
//!
//! Scope of this crate: the domain layer. It pulls together depot
//! (storage), mediawiki (dump I/O), and strpool (title bytes).
//!
//! See `SPEC.md` for the on-disk layout, per-revision codec, and
//! crash-safety contract.

pub mod asof;
#[cfg(feature = "fetch")]
mod cli;
#[cfg(feature = "fetch")]
pub use cli::cli_main;
pub mod error;
pub(crate) mod frames;
pub mod import;
pub mod instance;
pub mod readout;
pub mod revision;
#[cfg(feature = "serve")]
pub mod serve;
pub mod schema;
#[cfg(feature = "fetch")]
pub mod sync;

pub use error::{Error, Result};
pub use instance::{
    max_chain_id_for_root, ContributorMeta, HistoryEntry, HistoryIter, ImportStats, Instance,
    InstanceConfig, RevisionMeta, DEFAULT_MAX_CHAIN_ID,
};
#[cfg(feature = "fetch")]
pub use sync::{sync, SyncStats};
pub use revision::{
    FLAG_COMMENT_HIDDEN, FLAG_CONTRIBUTOR_HIDDEN, FLAG_SHA1_MISMATCH, FLAG_SUPPRESSED,
    FLAG_TEXT_HIDDEN, KIND_ANONYMOUS, KIND_HIDDEN, KIND_NAMED, REVISION_SCHEMA_VERSION,
};
