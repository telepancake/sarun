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
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct SiteInfo {
    pub site_name: String,
    pub db_name: String,
    pub base: String,
    pub generator: String,
    pub case: String,
    pub namespaces: BTreeMap<i32, Namespace>,
}
