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
/// check. When `Part::size_bytes == 0`, the source streams until protocol EOF
/// unless the user sets a positive `SARUN_WIKIMEDIA_MAX_SOURCE_BYTES` receive-
/// time ceiling. An absent or empty value means no ceiling; an invalid/zero
/// value is an error. The ceiling is a per-fetch bound, not an aggregate worker
/// budget.
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
- Content History discovery reads `SHA256SUMS` as the completion fence and
  authoritative part list, then reads the bounded `xml/bzip2/` directory index
  once for exact byte counts. Every manifest part must have exactly one
  positive listed size, and every listed XML part must be in the manifest.
  Discovery does not issue per-part HEAD or Range requests.
- Legacy fallback path: `<dbname>/<YYYYMMDD>/dumpstatus.json`.
- Part filenames: sorted by the leading page-range integer parsed from
  `-p(\d+)`, NOT lexicographically.
- Bz2 history files: single-stream multi-block. Multi-stream exists only for
  pages-articles.
- sha1 hash field: SHA-1 of UTF-8 text, base-36, left-padded to 31 chars.
- `<text deleted="deleted" />` form: text/comment/contributor independently
  carry the attribute; `Suppressed` heuristic: text deleted AND no `bytes=`
  AND no `sha1=` attribute on the text element.

## Fetch contract

The caller remains responsible for the durable import plan, target receipts,
and publication. Fetch streams the response on the fly and verifies an
advertised checksum when the returned reader reaches EOF.

- an unknown-size source (`size_bytes == 0`) has no implicit size ceiling;
  a positive `SARUN_WIKIMEDIA_MAX_SOURCE_BYTES` value adds a per-source
  receive-time bound, while an absent or empty value leaves it unbounded;
  invalid or zero values are rejected before the request;
- the explicit bound is enforced while reading, including a one-byte probe at
  the ceiling so an oversized body cannot be mistaken for an exact-bound body;
- advertised exact sizes remain strict: EOF before the advertised byte count
  and any extra byte after it are rejected, independently of checksum
  availability;
- sync and atomic rename are not part of fetch state or recovery; interrupted
  input is not persisted by this package.

On macOS, small metadata requests to official HTTPS Wikimedia hosts may use
Sarun's bounded `/usr/bin/curl` adapter. It enforces HTTPS-only redirects,
connect/total/low-speed deadlines, a redirect count, and a response-size
ceiling. The adapter passes `--disable` first so a user's curl configuration
  cannot add options to the production request. Range validation, digest
  verification, request spacing, cooldown, and retry decisions remain owned by
  this crate rather than delegated to curl defaults.
Non-official and loopback origins remain on the injected reqwest client so
tests exercise the same policy seams.

Every small reqwest metadata request has a 300-second request deadline and a
64 MiB body ceiling, including robots.txt and directory indexes.

## Out of scope

- Import target receipts, archive/index publication, and database state.
- A universal power-loss guarantee independent of filesystem and mount
  semantics.
