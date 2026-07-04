# wikimak-mediawiki — spec

Port of the Go `internal/mediawiki` package to Rust. Same shape, same names.
See `internal/mediawiki/` for the reference behavior.

## API

```rust
pub struct Run {
    pub source: RunSource,        // ContentHistory | Legacy
    pub date: chrono::NaiveDate,
    pub parts: Vec<Part>,         // sorted by parsed page-range start
}

pub struct Part {
    pub url: String,
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,   // hex
    pub sha1: Option<String>,     // hex
}

pub fn discover(client: &reqwest::blocking::Client, dbname: &str) -> Result<Run>;

/// Streaming HTTP fetch. The returned reader verifies the part's checksum on
/// EOF; calling `into_inner()` or dropping without reading to EOF skips the
/// check.
pub struct VerifyingReader<R: Read> { /* opaque */ }
pub fn fetch(client: &reqwest::blocking::Client, part: &Part) -> Result<VerifyingReader<Box<dyn Read>>>;

/// Block-parallel bz2 decoder. Pure Rust on top of `bzip2` crate's C backend
/// for per-block decode. Accepts single-stream multi-block (history dumps)
/// and multi-stream (pages-articles-multistream).
pub struct Bz2Options { pub workers: usize }
pub fn new_bz2_reader<R: Read + Send>(r: R, opts: Bz2Options) -> impl Read;

/// Streaming export-0.11 XML parser. Yields `Page` records.
pub struct PageStream<R: Read> { /* opaque */ }
impl<R: Read> Iterator for PageStream<R> {
    type Item = Result<Page>;
}
pub fn new_page_stream<R: Read>(r: R) -> PageStream<R>;
pub fn site_info<R: Read>(stream: &PageStream<R>) -> Option<&SiteInfo>;

pub struct Page {
    pub title: String,
    pub namespace: i32,
    pub id: i64,
    pub redirect_title: Option<String>,
    pub revisions: Vec<Revision>,
}

pub struct Revision {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub contributor: Contributor,
    pub minor: bool,
    pub comment: String,
    pub origin: Option<i64>,
    pub model: String,
    pub format: String,
    pub text: String,
    pub sha1: String,              // base-36 as stored
    pub text_hidden: bool,
    pub comment_hidden: bool,
    pub contributor_hidden: bool,
    pub suppressed: bool,
}

pub enum Contributor {
    Anonymous { ip: String },
    Named { username: String, user_id: i64 },
    Hidden,
}

pub struct SiteInfo {
    pub site_name: String,
    pub db_name: String,
    pub base: String,
    pub generator: String,
    pub case: String,
    pub namespaces: BTreeMap<i32, Namespace>,
}

pub struct Namespace {
    pub id: i32,
    pub case: String,
    pub name: String,
}

/// Verify a revision's text against its dump-stored base-36 sha1. Returns
/// (matched, normalized_text, tried_variants).
pub fn verify_rev_sha1(text: &str, sha1_base36: &str) -> (bool, String, Vec<&'static str>);
```

## Wire facts (verified live; do not deviate)

- Content History layout: parts live under
  `<date>/xml/bzip2/` together with `SHA256SUMS` and `_SUCCESS`. NOT at the
  date directory's top level. There is no per-date `readme.html`.
- Legacy fallback path: `<dbname>/<YYYYMMDD>/dumpstatus.json`.
- Part filenames: sorted by the leading page-range integer parsed from
  `-p(\d+)`, NOT lexicographically.
- Bz2 history files: single-stream multi-block. Multi-stream exists only for
  pages-articles.
- sha1 hash field: SHA-1 of UTF-8 text, base-36, left-padded to 31 chars.
- `<text deleted="deleted" />` form: text/comment/contributor independently
  carry the attribute; `Suppressed` heuristic: text deleted AND no `bytes=`
  AND no `sha1=` attribute on the text element.

## Crash-safety contract

There is none. This crate does I/O over HTTP and produces records; the
caller (wikimak-wikipedia) decides what's durable.

## Out of scope

- Local caching of dumps.
- Retry/backoff policies (caller's job; this crate fails fast).
- Any database, file format, or storage logic.
