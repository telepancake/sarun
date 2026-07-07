//! # wikimak-mediawiki
//!
//! MediaWiki dump plumbing: discover runs on dumps.wikimedia.org, fetch parts
//! with checksum verification, decompress bz2, stream-parse export-0.11 XML,
//! verify revision sha1 (base-36) with newline-fudge tolerance. See
//! `wikimak/mediawiki/SPEC.md`.
//!
//! Scope of this crate: dump-format I/O. It does NOT know about the depot,
//! storage tiers, or rendering — it produces `Page { id, title, ns,
//! revisions: [Revision...] }` records and walks away.

pub mod bz2;
#[cfg(feature = "fetch")]
pub mod discover;
#[cfg(feature = "fetch")]
pub mod fetch;
pub mod parser;
pub mod sha1;
pub mod types;

pub use bz2::{new_bz2_reader, Bz2Options, Bz2Reader};
#[cfg(feature = "fetch")]
pub use discover::{discover, discover_with, Config, DUMPS_BASE_URL};
#[cfg(feature = "fetch")]
pub use fetch::{fetch, VerifyingReader};
pub use parser::{new_page_stream, site_info, PageStream};
pub use sha1::verify_rev_sha1;
pub use types::{
    Contributor, Error, Interwiki, Namespace, Page, Part, Result, Revision, Run, RunSource,
    SiteInfo,
};
