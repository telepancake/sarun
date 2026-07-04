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

pub mod error;
pub mod import;
pub mod instance;
pub mod revision;
pub mod schema;

pub use error::{Error, Result};
pub use instance::{
    ContributorMeta, HistoryEntry, HistoryIter, ImportStats, Instance, InstanceConfig, RevisionMeta,
};
pub use revision::{
    FLAG_COMMENT_HIDDEN, FLAG_CONTRIBUTOR_HIDDEN, FLAG_SHA1_MISMATCH, FLAG_SUPPRESSED,
    FLAG_TEXT_HIDDEN, KIND_ANONYMOUS, KIND_HIDDEN, KIND_NAMED, REVISION_SCHEMA_VERSION,
};
