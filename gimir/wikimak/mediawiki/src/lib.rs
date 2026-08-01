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
#[cfg(feature = "fetch")]
pub(crate) mod politeness;
pub mod parser;
pub mod sha1;
pub mod types;

pub use bz2::{new_bz2_reader, Bz2Options, Bz2Reader};
#[cfg(feature = "fetch")]
pub use discover::{discover, discover_incremental_with, discover_with, Config, DUMPS_BASE_URL};
#[cfg(feature = "fetch")]
pub use fetch::{fetch, FetchStats, FetchStatsHandle, VerifyingReader};
#[cfg(feature = "fetch")]
pub fn prepare_robots(client: &reqwest::blocking::Client, url: &str) -> Result<()> {
    politeness::ensure_robots(client, url)
}
pub use parser::{new_page_stream, new_revision_stream, site_info, PageStream, RevisionStream};
pub use sha1::verify_rev_sha1;
pub use types::{
    Contributor, Error, Interwiki, Namespace, Page, PageHeader, Part, Result, Revision, Run,
    RunSource, SiteInfo,
};
