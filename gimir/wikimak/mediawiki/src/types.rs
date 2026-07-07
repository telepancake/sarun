//! Shared types and error enum.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};

pub type Result<T> = std::result::Result<T, Error>;

/// Errors surfaced by the mediawiki crate.
///
/// Per SPEC: errors use `thiserror`, no `anyhow`. Variants are intentionally
/// coarse — callers either retry the whole pipeline or surface the message.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[cfg(feature = "fetch")]
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("http status {status} for {url}")]
    HttpStatus { status: u16, url: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("checksum mismatch for {part}: expected {expected}, got {got}")]
    ChecksumMismatch {
        part: String,
        expected: String,
        got: String,
    },
    #[error("no complete run found for {dbname}")]
    NoCompleteRun { dbname: String },
    #[error("xml error: {0}")]
    Xml(String),
    #[error("bz2 error: {0}")]
    Bz2(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSource {
    ContentHistory,
    Legacy,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub source: RunSource,
    pub date: NaiveDate,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone)]
pub struct Part {
    pub url: String,
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contributor {
    Anonymous { ip: String },
    Named { username: String, user_id: i64 },
    Hidden,
}

#[derive(Debug, Clone)]
pub struct Revision {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub timestamp: DateTime<Utc>,
    pub contributor: Contributor,
    pub minor: bool,
    pub comment: String,
    pub origin: Option<i64>,
    pub model: String,
    pub format: String,
    pub text: String,
    pub sha1: String,
    pub text_hidden: bool,
    pub comment_hidden: bool,
    pub contributor_hidden: bool,
    pub suppressed: bool,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub title: String,
    pub namespace: i32,
    pub id: i64,
    pub redirect_title: Option<String>,
    pub revisions: Vec<Revision>,
}

#[derive(Debug, Clone)]
pub struct Namespace {
    pub id: i32,
    pub case: String,
    /// The localized namespace name — the element text of `<namespace>` in
    /// the dump's `<siteinfo>` (content-language name, e.g. "Vorlage" on
    /// dewiki, "Template" on enwiki). Empty for the main namespace (id 0).
    pub name: String,
    /// Additional resolvable names for this namespace. The export-0.11
    /// `<siteinfo>` header carries NO aliases (only the single localized
    /// name above), so the parser leaves this empty; it exists so a richer
    /// source (`action=query&meta=siteinfo&siprop=namespacealiases`) can
    /// populate it without another struct change. Never fabricated.
    pub aliases: Vec<String>,
}

/// One interwiki-map prefix. Export-0.11 `<siteinfo>` normally carries no
/// interwiki data (it lives in `action=query&meta=siteinfo`); the parser
/// fills this only if a snapshot embeds an `<interwikimap>`/`<interwiki>`
/// element (API-XML shape). Otherwise it stays empty and the wikipedia
/// layer seeds a built-in map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interwiki {
    pub prefix: String,
    /// URL pattern with `$1` standing in for the target title.
    pub url: String,
    /// MediaWiki's own `local` flag for the prefix (same-farm interwiki).
    /// This is NOT "mirrored by us" — the wikipedia layer never derives a
    /// local-link decision from it.
    pub is_local: bool,
}

#[derive(Debug, Clone)]
pub struct SiteInfo {
    pub site_name: String,
    pub db_name: String,
    pub base: String,
    pub generator: String,
    pub case: String,
    pub namespaces: BTreeMap<i32, Namespace>,
    /// Interwiki map, empty for a plain dump header (see [`Interwiki`]).
    pub interwiki: Vec<Interwiki>,
}
