//! Streaming export-0.11 XML parser.
//!
//! Per SPEC §API: yields `Result<Page>` records, exposes `site_info`
//! for the dump-file header.
//!
//! Two granularities over one cursor:
//!
//!   * [`RevisionStream`] — the streaming core: `next_page()` yields a
//!     [`PageHeader`], then `next_revision()` yields that page's
//!     revisions ONE AT A TIME. At most one revision is resident;
//!     a full-history page's text never accumulates in RAM. Bulk
//!     consumers (the wikipedia importer) MUST use this.
//!   * [`PageStream`] — the compatibility collector over the core:
//!     `Iterator<Item = Result<Page>>`, one whole `<page>` element
//!     resident per item. Fine for small-scale consumers and tests;
//!     fatal for full-history enwiki (hot pages run to ~10^6 revisions
//!     ≈ 10^11 text bytes per page element).
//!
//! Elements are matched by local name, so default-namespaced exports
//! work without any namespace plumbing on the caller's side.

use std::io::{self, BufRead, BufReader, Read, Write};

use chrono::DateTime;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::Reader;

use crate::types::{
    Contributor, Error, Interwiki, Namespace, Page, PageHeader, Result, Revision, SiteInfo,
};

/// The streaming core: per-revision access to an export-0.11 document.
///
/// The `<siteinfo>` header is parsed lazily on the first `next_page()`.
/// Header fields of a `<page>` are everything before its first
/// `<revision>` (the fixed export-0.11 element order); a stray header
/// field AFTER a revision would be skipped, not folded into the
/// already-yielded [`PageHeader`].
struct ReplayBufReader<R: Read> {
    inner: BufReader<R>,
    replay: Vec<u8>,
    replay_position: usize,
}

impl<R: Read> ReplayBufReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            replay: Vec::new(),
            replay_position: 0,
        }
    }

    fn push_back(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.replay_position != self.replay.len() {
            return Err(io::Error::other("XML replay buffer is not empty"));
        }
        self.replay.clear();
        self.replay.extend_from_slice(bytes);
        self.replay_position = 0;
        Ok(())
    }

    fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

impl<R: Read> Read for ReplayBufReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let amount = available.len().min(output.len());
        output[..amount].copy_from_slice(&available[..amount]);
        self.consume(amount);
        Ok(amount)
    }
}

impl<R: Read> BufRead for ReplayBufReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.replay_position != self.replay.len() {
            Ok(&self.replay[self.replay_position..])
        } else {
            self.inner.fill_buf()
        }
    }

    fn consume(&mut self, amount: usize) {
        if self.replay_position != self.replay.len() {
            self.replay_position = (self.replay_position + amount).min(self.replay.len());
        } else {
            self.inner.consume(amount);
        }
    }
}

pub struct RevisionStream<R: Read> {
    reader: Reader<ReplayBufReader<R>>,
    buf: Vec<u8>,
    site_info: Option<SiteInfo>,
    header_parsed: bool,
    ended: bool,
    failed: bool,
    /// Between `next_page` (Some) and the `</page>` observed by
    /// `next_revision` (or the skip in the next `next_page`).
    in_page: bool,
    /// `next_page`'s header scan consumed a `<revision>` start tag;
    /// the next `next_revision` must parse it before reading further.
    pending_revision: bool,
}

/// Build a [`RevisionStream`] over `r`.
pub fn new_revision_stream<R: Read>(r: R) -> RevisionStream<R> {
    let mut reader = Reader::from_reader(ReplayBufReader::new(r));
    let cfg = reader.config_mut();
    cfg.trim_text(false);
    RevisionStream {
        reader,
        buf: Vec::new(),
        site_info: None,
        header_parsed: false,
        ended: false,
        failed: false,
        in_page: false,
        pending_revision: false,
    }
}

/// The revision fields available when the opening `<text>` tag is reached.
///
/// This is deliberately a value rather than a reference into the XML reader:
/// the caller may keep the prefix while it chooses the sink for the text
/// body.  Fields which the archive records before the body are all present;
/// `sha1` is completed by [`RevisionStart::finish`] because it is normally a
/// suffix field in export-0.11.
#[derive(Debug, Clone)]
pub struct RevisionPrefix {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub timestamp: DateTime<chrono::Utc>,
    pub contributor: Contributor,
    pub minor: bool,
    pub comment: String,
    pub origin: Option<i64>,
    pub model: String,
    pub format: String,
    pub text_bytes: Option<u64>,
    pub text_hidden: bool,
    pub comment_hidden: bool,
    pub contributor_hidden: bool,
    pub suppressed: bool,
}

enum RevisionBodyState {
    /// The reader has consumed `<text ...>` and the body is still pending.
    Streaming { qualified_end: Vec<u8> },
    /// `<text/>` was observed. There is no body to emit.
    Empty,
    /// A deleted text element was consumed without exposing its contents.
    Hidden,
    /// The body was consumed exactly once.
    Done,
}

/// A revision paused immediately after its opening `<text>` element.
///
/// The cursor is linear: inspect [`Self::prefix`], call
/// [`Self::stream_text_to`] exactly once, then call [`Self::finish`] to parse
/// the remainder of the revision. Dropping it before `finish` poisons the
/// parent stream, because continuing at an unconsumed revision body would be
/// ambiguous and could silently corrupt the following record.
pub struct RevisionStart<'a, R: Read> {
    stream: &'a mut RevisionStream<R>,
    prefix: RevisionPrefix,
    revision: Revision,
    body: RevisionBodyState,
    text_end_pending: bool,
    finished: bool,
}

impl<'a, R: Read> RevisionStart<'a, R> {
    /// Return all archive-relevant fields parsed before `<text>`.
    pub fn prefix(&self) -> &RevisionPrefix {
        &self.prefix
    }

    /// Stream the decoded text body into `output` exactly once.
    ///
    /// For an empty or deleted text element this is a successful no-op. If a
    /// `bytes` attribute is present on visible text, it is compared with the
    /// number of decoded UTF-8 bytes delivered to `output`; a mismatch is an
    /// XML parse error. The parser never materializes the body.
    pub fn stream_text_to<W: Write>(&mut self, output: &mut W) -> Result<()> {
        self.stream_text_to_with_options(output, true)
    }

    fn stream_text_to_with_options<W: Write>(
        &mut self,
        output: &mut W,
        check_declared_bytes: bool,
    ) -> Result<()> {
        let body = std::mem::replace(&mut self.body, RevisionBodyState::Done);
        match body {
            RevisionBodyState::Streaming { qualified_end } => {
                let mut counted = CountingWriter {
                    output,
                    bytes: 0,
                };
                let stream_result = {
                    let mut validated = Utf8ValidatingWriter::new(&mut counted);
                    stream_text(&mut self.stream.reader, &qualified_end, &mut validated)
                        .and_then(|()| validated.finish())
                };
                let result = stream_result.and_then(|()| {
                    if check_declared_bytes {
                        if let Some(expected) = self.prefix.text_bytes {
                            if counted.bytes != expected {
                                return Err(Error::Parse(format!(
                                    "revision {} text bytes mismatch: declared {}, decoded {}",
                                    self.prefix.id, expected, counted.bytes
                                )));
                            }
                        }
                    }
                    Ok(())
                });
                if result.is_err() {
                    self.stream.failed = true;
                }
                result
            }
            RevisionBodyState::Empty => {
                if check_declared_bytes {
                    if let Some(expected) = self.prefix.text_bytes {
                        if expected != 0 {
                            self.stream.failed = true;
                            return Err(Error::Parse(format!(
                                "revision {} text bytes mismatch: declared {}, decoded 0",
                                self.prefix.id, expected
                            )));
                        }
                    }
                }
                Ok(())
            }
            RevisionBodyState::Hidden => Ok(()),
            RevisionBodyState::Done => {
                self.stream.failed = true;
                Err(Error::Parse(format!(
                    "revision {} text body was streamed more than once",
                    self.prefix.id
                )))
            }
        }
    }

    /// Finish parsing the revision suffix and return the complete compatible
    /// [`Revision`]. `stream_text_to` must have been called first.
    pub fn finish(mut self) -> Result<Revision> {
        if !matches!(&self.body, RevisionBodyState::Done) {
            self.stream.failed = true;
            self.finished = true;
            return Err(Error::Parse(format!(
                "revision {} text body was not streamed",
                self.prefix.id
            )));
        }
        let result = finish_streamed_revision(
            &mut self.stream.reader,
            &mut self.revision,
            self.text_end_pending,
        );
        self.finished = true;
        if result.is_err() {
            self.stream.failed = true;
        }
        result
    }
}

impl<R: Read> Drop for RevisionStart<'_, R> {
    fn drop(&mut self) {
        if !self.finished {
            self.stream.failed = true;
        }
    }
}

struct CountingWriter<'a, W: Write> {
    output: &'a mut W,
    bytes: u64,
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// Validate streamed text without buffering it. The raw text scanner cannot
/// rely on `quick_xml` here because it deliberately bypasses event decoding
/// for the unbounded body. At most three bytes of an incomplete UTF-8 scalar
/// are retained between writes.
struct Utf8ValidatingWriter<'a, W: Write> {
    output: &'a mut W,
    pending: [u8; 4],
    pending_len: usize,
    expected_len: usize,
}

impl<'a, W: Write> Utf8ValidatingWriter<'a, W> {
    fn new(output: &'a mut W) -> Self {
        Self {
            output,
            pending: [0; 4],
            pending_len: 0,
            expected_len: 0,
        }
    }

    fn invalid_utf8() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid UTF-8 in streamed revision text",
        )
    }

    fn invalid_xml_character() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid XML character in streamed revision text",
        )
    }

    fn validate(&mut self, bytes: &[u8]) -> io::Result<()> {
        for &byte in bytes {
            if self.pending_len == 0 {
                self.expected_len = match byte {
                    0x00..=0x08 | 0x0b..=0x0c | 0x0e..=0x1f => {
                        return Err(Self::invalid_xml_character())
                    }
                    0x09 | 0x0a | 0x0d | 0x20..=0x7f => 0,
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(Self::invalid_utf8()),
                };
                if self.expected_len != 0 {
                    self.pending[0] = byte;
                    self.pending_len = 1;
                }
                continue;
            }
            if !(0x80..=0xbf).contains(&byte) {
                return Err(Self::invalid_utf8());
            }
            self.pending[self.pending_len] = byte;
            self.pending_len += 1;
            if self.pending_len == self.expected_len {
                let scalar = std::str::from_utf8(&self.pending[..self.pending_len])
                    .map_err(|_| Self::invalid_utf8())?;
                let codepoint = scalar
                    .chars()
                    .next()
                    .map(|character| character as u32)
                    .expect("a complete UTF-8 scalar is non-empty");
                if !matches!(
                    codepoint,
                    0x0009
                        | 0x000a
                        | 0x000d
                        | 0x0020..=0xd7ff
                        | 0xe000..=0xfffd
                        | 0x10000..=0x10ffff
                ) {
                    return Err(Self::invalid_xml_character());
                }
                self.pending_len = 0;
                self.expected_len = 0;
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        if self.pending_len != 0 {
            return Err(Error::Xml(
                "invalid UTF-8 in streamed revision text: truncated scalar".into(),
            ));
        }
        Ok(())
    }
}

impl<W: Write> Write for Utf8ValidatingWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.validate(bytes)?;
        self.output.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

impl<R: Read> RevisionStream<R> {
    /// Consume the stream, returning the underlying reader. The parser
    /// stops at `</mediawiki>`; callers that need end-of-stream effects
    /// on the source (e.g. `VerifyingReader`'s on-EOF checksum) drain
    /// the returned reader.
    pub fn into_inner(self) -> R {
        self.reader.into_inner().into_inner()
    }

    /// The parsed `<siteinfo>` header, or `None` if not yet observed
    /// (it is observed by the first `next_page()`).
    pub fn site_info(&self) -> Option<&SiteInfo> {
        self.site_info.as_ref()
    }

    /// Advance to the next `<page>` and return its header. Any
    /// unconsumed revisions of the current page are skipped (without
    /// materializing them). `None` at end of document; after any
    /// `Err`, the stream is dead and every call returns `None`.
    pub fn next_page(&mut self) -> Option<Result<PageHeader>> {
        if self.ended || self.failed {
            return None;
        }
        if !self.header_parsed {
            self.header_parsed = true;
            if let Err(e) = self.parse_header() {
                self.failed = true;
                return Some(Err(e));
            }
        }
        if self.in_page {
            // Abandoned page: skip to its matching end tag wholesale.
            self.pending_revision = false;
            self.in_page = false;
            if let Err(e) = skip_to_end(&mut self.reader, QName(b"page")) {
                self.failed = true;
                return Some(Err(e));
            }
        }
        loop {
            self.buf.clear();
            let ev = match self.reader.read_event_into(&mut self.buf) {
                Ok(e) => e,
                Err(e) => {
                    self.failed = true;
                    return Some(Err(Error::Xml(e.to_string())));
                }
            };
            match ev {
                Event::Start(s) => {
                    let is_page = local_name(&s) == b"page";
                    if is_page {
                        self.in_page = true;
                        let h = self.parse_page_header();
                        if h.is_err() {
                            self.failed = true;
                            self.in_page = false;
                        }
                        return Some(h);
                    }
                }
                Event::Eof => {
                    self.ended = true;
                    return None;
                }
                _ => {}
            }
        }
    }

    /// The current page's next revision, or `None` at `</page>` (the
    /// signal to call `next_page` again). At most ONE revision is ever
    /// resident. After any `Err`, the stream is dead.
    pub fn next_revision(&mut self) -> Option<Result<Revision>> {
        let mut started = match self.next_revision_stream()? {
            Ok(started) => started,
            Err(error) => return Some(Err(error)),
        };
        let mut text = Vec::new();
        if let Err(error) = started.stream_text_to_with_options(&mut text, false) {
            return Some(Err(error));
        }
        let text = match String::from_utf8(text) {
            Ok(text) => text,
            Err(error) => return Some(Err(Error::Xml(error.to_string()))),
        };
        let mut revision = match started.finish() {
            Ok(revision) => revision,
            Err(error) => return Some(Err(error)),
        };
        revision.text = text;
        Some(Ok(revision))
    }

    /// Begin the current page's next revision and pause immediately after
    /// its opening `<text>` tag. The returned [`RevisionStart`] exposes the
    /// parsed prefix, then streams the body and parses the suffix through one
    /// explicit code path. A prefix field encountered after `<text>` is an
    /// error instead of being silently accepted with incomplete metadata.
    pub fn next_revision_stream(&mut self) -> Option<Result<RevisionStart<'_, R>>> {
        if self.ended || self.failed || !self.in_page {
            return None;
        }
        if self.pending_revision {
            self.pending_revision = false;
            return Some(self.begin_revision());
        }
        loop {
            self.buf.clear();
            let ev = match self.reader.read_event_into(&mut self.buf) {
                Ok(e) => e,
                Err(e) => {
                    self.failed = true;
                    return Some(Err(Error::Xml(e.to_string())));
                }
            };
            match ev {
                Event::Start(s) => {
                    let name = local_name(&s).to_vec();
                    if name == b"revision" {
                        return Some(self.begin_revision());
                    }
                    if let Err(e) = skip_to_end(&mut self.reader, QName(&name)) {
                        self.failed = true;
                        return Some(Err(e));
                    }
                }
                Event::End(e) if local_name_end(&e) == b"page" => {
                    self.in_page = false;
                    return None;
                }
                Event::Eof => {
                    self.failed = true;
                    return Some(Err(Error::Xml("EOF inside <page>".into())));
                }
                _ => {}
            }
        }
    }

    fn begin_revision(&mut self) -> Result<RevisionStart<'_, R>> {
        match parse_revision_prefix(&mut self.reader) {
            Ok((revision, prefix, body, text_end_pending)) => Ok(RevisionStart {
                stream: self,
                prefix,
                revision,
                body,
                text_end_pending,
                finished: false,
            }),
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    /// Parse the current page's next revision while emitting its visible
    /// `<text>` content to `text`.  The compatibility [`Revision`] returned
    /// by this method has an empty `text` field: the content has already been
    /// delivered to the sink.  All other revision fields retain their normal
    /// meaning.
    ///
    /// This is the import boundary for large dumps.  In particular, callers
    /// must not use `next_revision()` for a bulk import and then convert its
    /// `String` into another buffer.
    pub fn next_revision_to<W: Write>(&mut self, text: &mut W) -> Option<Result<Revision>> {
        let mut started = match self.next_revision_stream()? {
            Ok(started) => started,
            Err(error) => return Some(Err(error)),
        };
        if let Err(error) = started.stream_text_to_with_options(text, false) {
            return Some(Err(error));
        }
        Some(started.finish())
    }

    /// Parse one `<page>`'s header fields, stopping at the first
    /// `<revision>` (leaving it pending for `next_revision`) or at
    /// `</page>` (a page with no revisions).
    fn parse_page_header(&mut self) -> Result<PageHeader> {
        let mut h = PageHeader {
            title: String::new(),
            namespace: 0,
            id: 0,
            redirect_title: None,
        };
        loop {
            self.buf.clear();
            let ev = self
                .reader
                .read_event_into(&mut self.buf)
                .map_err(|e| Error::Xml(e.to_string()))?;
            match ev {
                Event::Start(s) => {
                    let name = local_name(&s).to_vec();
                    match name.as_slice() {
                        b"title" => h.title = read_text(&mut self.reader, &name)?,
                        b"ns" => {
                            h.namespace = read_text(&mut self.reader, &name)?
                                .trim()
                                .parse()
                                .map_err(|e| Error::Xml(format!("ns: {e}")))?
                        }
                        b"id" => {
                            h.id = read_text(&mut self.reader, &name)?
                                .trim()
                                .parse()
                                .map_err(|e| Error::Xml(format!("id: {e}")))?
                        }
                        b"redirect" => {
                            // Defensive: redirect usually arrives as Empty,
                            // but in case it has a body, skip its end.
                            h.redirect_title = Some(attr_string(&s, b"title"));
                            skip_to_end(&mut self.reader, QName(&name))?;
                        }
                        b"revision" => {
                            self.pending_revision = true;
                            return Ok(h);
                        }
                        _ => skip_to_end(&mut self.reader, QName(&name))?,
                    }
                }
                Event::Empty(s) => {
                    if local_name(&s) == b"redirect" {
                        h.redirect_title = Some(attr_string(&s, b"title"));
                    }
                }
                Event::End(e) if local_name_end(&e) == b"page" => {
                    self.in_page = false;
                    return Ok(h);
                }
                Event::Eof => return Err(Error::Xml("EOF inside <page>".into())),
                _ => {}
            }
        }
    }

    fn parse_header(&mut self) -> Result<()> {
        // Walk tokens until we see <siteinfo>, decode it, leave the
        // cursor positioned at the next sibling.
        loop {
            self.buf.clear();
            let ev = self
                .reader
                .read_event_into(&mut self.buf)
                .map_err(|e| Error::Xml(e.to_string()))?;
            match ev {
                Event::Start(s) => {
                    let name = local_name(&s).to_vec();
                    if name == b"mediawiki" {
                        continue;
                    }
                    if name == b"siteinfo" {
                        self.site_info = Some(parse_site_info(&mut self.reader)?);
                        return Ok(());
                    }
                    // Unknown — skip to its end.
                    skip_to_end(&mut self.reader, QName(&name))?;
                }
                Event::Eof => {
                    return Err(Error::Xml("unexpected EOF before <siteinfo>".into()));
                }
                _ => {}
            }
        }
    }
}

/// The compatibility collector over [`RevisionStream`]: an iterator of
/// whole [`Page`]s, ONE `<page>` element fully resident per item. For
/// small-scale consumers and tests only — bulk import must stream
/// revisions via [`PageStream::revisions_mut`] / [`RevisionStream`].
pub struct PageStream<R: Read> {
    inner: RevisionStream<R>,
}

/// Build a `PageStream` over `r`.
pub fn new_page_stream<R: Read>(r: R) -> PageStream<R> {
    PageStream {
        inner: new_revision_stream(r),
    }
}

impl<R: Read> PageStream<R> {
    /// Consume the stream, returning the underlying reader. The parser
    /// stops at `</mediawiki>`; callers that need end-of-stream effects
    /// on the source (e.g. `VerifyingReader`'s on-EOF checksum) drain
    /// the returned reader.
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }

    /// The streaming core sharing this stream's cursor: per-revision
    /// access without materializing a whole page. Pages/revisions
    /// consumed through it advance this stream too.
    pub fn revisions_mut(&mut self) -> &mut RevisionStream<R> {
        &mut self.inner
    }
}

/// Return the parsed `<siteinfo>` header, or `None` if it has not yet
/// been observed.
pub fn site_info<R: Read>(stream: &PageStream<R>) -> Option<&SiteInfo> {
    stream.inner.site_info()
}

impl<R: Read> Iterator for PageStream<R> {
    type Item = Result<Page>;
    fn next(&mut self) -> Option<Self::Item> {
        let header = match self.inner.next_page()? {
            Ok(h) => h,
            Err(e) => return Some(Err(e)),
        };
        let mut revisions = Vec::new();
        while let Some(rev) = self.inner.next_revision() {
            match rev {
                Ok(r) => revisions.push(r),
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Ok(Page {
            title: header.title,
            namespace: header.namespace,
            id: header.id,
            redirect_title: header.redirect_title,
            revisions,
        }))
    }
}

fn local_name<'a>(s: &'a BytesStart<'a>) -> &'a [u8] {
    s.local_name().into_inner()
}

fn skip_to_end<B: BufRead>(reader: &mut Reader<B>, end: QName) -> Result<()> {
    let mut tmp = Vec::new();
    reader
        .read_to_end_into(end, &mut tmp)
        .map_err(|e| Error::Xml(e.to_string()))?;
    Ok(())
}

fn parse_site_info<B: BufRead>(reader: &mut Reader<B>) -> Result<SiteInfo> {
    let mut si = SiteInfo {
        site_name: String::new(),
        db_name: String::new(),
        base: String::new(),
        generator: String::new(),
        case: String::new(),
        namespaces: Default::default(),
        interwiki: Vec::new(),
    };
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?;
        match ev {
            Event::Start(s) => {
                let name = local_name(&s).to_vec();
                match name.as_slice() {
                    b"sitename" => si.site_name = read_text(reader, &name)?,
                    b"dbname" => si.db_name = read_text(reader, &name)?,
                    b"base" => si.base = read_text(reader, &name)?,
                    b"generator" => si.generator = read_text(reader, &name)?,
                    b"case" => si.case = read_text(reader, &name)?,
                    b"namespaces" => parse_namespaces(reader, &mut si)?,
                    // Not part of export-0.11's header, but a snapshot may
                    // embed the API interwikimap; parse it if present.
                    b"interwikimap" | b"interwiki" => {
                        parse_interwiki(reader, &mut si, &name)?
                    }
                    _ => skip_to_end(reader, QName(&name))?,
                }
            }
            Event::End(e) if local_name_end(&e) == b"siteinfo" => return Ok(si),
            Event::Eof => return Err(Error::Xml("EOF inside <siteinfo>".into())),
            _ => {}
        }
    }
}

/// Parse an `<interwikimap>`/`<interwiki>` wrapper of `<iw>` entries in the
/// `action=query&meta=siteinfo&siprop=interwikimap` XML shape
/// (`<iw prefix="w" url="https://…/$1" local="" />`). A plain dump header
/// has no such element, so this is normally never reached.
///
/// The `local` attribute is MediaWiki's same-farm flag; it is recorded on
/// [`Interwiki::is_local`] but the wikipedia layer treats a foreign wiki as
/// external regardless (it only turns a prefix into a local link when the
/// prefix maps to an instance WE mirror).
fn parse_interwiki<B: BufRead>(reader: &mut Reader<B>, si: &mut SiteInfo, end: &[u8]) -> Result<()> {
    // Pull the (prefix, url, is_local) out of an `<iw>` start tag.
    fn push_iw(si: &mut SiteInfo, s: &BytesStart<'_>) {
        let prefix = attr_string(s, b"prefix");
        if prefix.is_empty() {
            return;
        }
        si.interwiki.push(Interwiki {
            prefix,
            url: attr_string(s, b"url"),
            is_local: attr_present(s, b"local"),
        });
    }
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?;
        match ev {
            // `<iw .../>` — the API-XML shape (always empty in practice).
            Event::Empty(s) if local_name(&s) == b"iw" => push_iw(si, &s),
            // `<iw ...>…</iw>` — defensive; consume the body.
            Event::Start(s) => {
                let n = local_name(&s).to_vec();
                if n == b"iw" {
                    push_iw(si, &s);
                }
                skip_to_end(reader, QName(&n))?;
            }
            Event::End(e) if local_name_end(&e) == end => return Ok(()),
            Event::Eof => return Err(Error::Xml("EOF inside <interwikimap>".into())),
            _ => {}
        }
    }
}

fn parse_namespaces<B: BufRead>(reader: &mut Reader<B>, si: &mut SiteInfo) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?;
        match ev {
            Event::Start(s) => {
                if local_name(&s) == b"namespace" {
                    let key = attr_i32(&s, b"key")?;
                    let case = attr_string(&s, b"case");
                    let name = read_text(reader, b"namespace")?;
                    si.namespaces.insert(
                        key,
                        Namespace {
                            id: key,
                            case,
                            name,
                            aliases: Vec::new(),
                        },
                    );
                } else {
                    let n = local_name(&s).to_vec();
                    skip_to_end(reader, QName(&n))?;
                }
            }
            Event::Empty(s) => {
                if local_name(&s) == b"namespace" {
                    let key = attr_i32(&s, b"key")?;
                    let case = attr_string(&s, b"case");
                    si.namespaces.insert(
                        key,
                        Namespace {
                            id: key,
                            case,
                            name: String::new(),
                            aliases: Vec::new(),
                        },
                    );
                }
            }
            Event::End(e) if local_name_end(&e) == b"namespaces" => return Ok(()),
            Event::Eof => return Err(Error::Xml("EOF inside <namespaces>".into())),
            _ => {}
        }
    }
}

fn local_name_end<'a>(e: &'a quick_xml::events::BytesEnd<'a>) -> &'a [u8] {
    e.local_name().into_inner()
}

fn read_text<B: BufRead>(reader: &mut Reader<B>, end: &[u8]) -> Result<String> {
    let mut out = String::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?;
        match ev {
            Event::Text(t) => {
                let raw = t.decode().map_err(|e| Error::Xml(e.to_string()))?;
                let unescaped =
                    quick_xml::escape::unescape(&raw).map_err(|e| Error::Xml(e.to_string()))?;
                out.push_str(&unescaped);
            }
            Event::CData(c) => {
                out.push_str(
                    std::str::from_utf8(c.as_ref()).map_err(|e| Error::Xml(e.to_string()))?,
                );
            }
            Event::GeneralRef(reference) => {
                if let Some(ch) = reference
                    .resolve_char_ref()
                    .map_err(|e| Error::Xml(e.to_string()))?
                {
                    out.push(ch);
                } else {
                    let name = reference.decode().map_err(|e| Error::Xml(e.to_string()))?;
                    let value = quick_xml::escape::resolve_predefined_entity(&name)
                        .ok_or_else(|| Error::Xml(format!("unknown XML entity &{name};")))?;
                    out.push_str(value);
                }
            }
            Event::End(e) if local_name_end(&e) == end => return Ok(out),
            Event::Eof => {
                return Err(Error::Xml(format!(
                    "EOF inside <{}>",
                    String::from_utf8_lossy(end)
                )))
            }
            _ => {}
        }
    }
}

fn attr_string(s: &BytesStart<'_>, key: &[u8]) -> String {
    for a in s.attributes().flatten() {
        if a.key.local_name().into_inner() == key {
            #[allow(deprecated)]
            return a
                .unescape_value()
                .map(|c| c.into_owned())
                .unwrap_or_default();
        }
    }
    String::new()
}

fn attr_present(s: &BytesStart<'_>, key: &[u8]) -> bool {
    s.attributes()
        .flatten()
        .any(|a| a.key.local_name().into_inner() == key)
}

fn attr_i32(s: &BytesStart<'_>, key: &[u8]) -> Result<i32> {
    attr_string(s, key)
        .parse::<i32>()
        .map_err(|e| Error::Xml(format!("attr {}: {e}", String::from_utf8_lossy(key))))
}

// ---- revision parsing ------------------------------------------------

fn parse_revision_prefix<R: Read>(
    reader: &mut Reader<ReplayBufReader<R>>,
) -> Result<(
    Revision,
    RevisionPrefix,
    RevisionBodyState,
    bool,
)> {
    let mut revision = Revision {
        id: 0,
        parent_id: None,
        timestamp: DateTime::<chrono::Utc>::UNIX_EPOCH,
        contributor: Contributor::Hidden,
        minor: false,
        comment: String::new(),
        origin: None,
        model: String::new(),
        format: String::new(),
        text: String::new(),
        sha1: String::new(),
        text_hidden: false,
        comment_hidden: false,
        contributor_hidden: false,
        suppressed: false,
    };
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?;
        match ev {
            Event::Start(s) => {
                let name = local_name(&s).to_vec();
                match name.as_slice() {
                    b"id" => {
                        revision.id = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("rev id: {e}")))?;
                    }
                    b"parentid" => {
                        let parent_id: i64 = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("parentid: {e}")))?;
                        revision.parent_id = Some(parent_id);
                    }
                    b"timestamp" => {
                        let raw = read_text(reader, &name)?;
                        revision.timestamp = DateTime::parse_from_rfc3339(raw.trim())
                            .map_err(|e| Error::Xml(format!("timestamp {raw:?}: {e}")))?
                            .with_timezone(&chrono::Utc);
                    }
                    b"contributor" => {
                        let (contributor, hidden) = parse_contributor(reader, &s)?;
                        revision.contributor = contributor;
                        revision.contributor_hidden = hidden;
                    }
                    b"comment" => {
                        if attr_string(&s, b"deleted") == "deleted" {
                            revision.comment_hidden = true;
                            skip_to_end(reader, QName(&name))?;
                        } else {
                            revision.comment = read_text(reader, &name)?;
                        }
                    }
                    b"minor" => {
                        revision.minor = true;
                        skip_to_end(reader, QName(&name))?;
                    }
                    b"origin" => {
                        let origin: i64 = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("origin: {e}")))?;
                        revision.origin = Some(origin);
                    }
                    b"model" => revision.model = read_text(reader, &name)?,
                    b"format" => revision.format = read_text(reader, &name)?,
                    b"sha1" => revision.sha1 = read_text(reader, &name)?,
                    b"text" => {
                        let text_bytes = text_bytes_attr(&s)?;
                        let text_sha1 = attr_present(&s, b"sha1");
                        let text_hidden = attr_string(&s, b"deleted") == "deleted";
                        revision.text_hidden = text_hidden;
                        revision.suppressed =
                            text_hidden && text_bytes.is_none() && !text_sha1;
                        let prefix = revision_prefix(&revision, text_bytes);
                        if text_hidden {
                            skip_to_end(reader, QName(&name))?;
                            return Ok((
                                revision,
                                prefix,
                                RevisionBodyState::Hidden,
                                false,
                            ));
                        }
                        return Ok((
                            revision,
                            prefix,
                            RevisionBodyState::Streaming {
                                qualified_end: s.name().as_ref().to_vec(),
                            },
                            true,
                        ));
                    }
                    b"revision" => {
                        return Err(Error::Xml(
                            "nested <revision> before its <text> field".into(),
                        ));
                    }
                    _ => skip_to_end(reader, QName(&name))?,
                }
            }
            Event::Empty(s) => {
                let name = local_name(&s);
                match name {
                    b"minor" => revision.minor = true,
                    b"comment" => {
                        if attr_string(&s, b"deleted") == "deleted" {
                            revision.comment_hidden = true;
                        }
                    }
                    b"contributor" => {
                        if attr_string(&s, b"deleted") == "deleted" {
                            revision.contributor_hidden = true;
                            revision.contributor = Contributor::Hidden;
                        }
                    }
                    b"text" => {
                        let text_bytes = text_bytes_attr(&s)?;
                        let text_sha1 = attr_present(&s, b"sha1");
                        let text_hidden = attr_string(&s, b"deleted") == "deleted";
                        revision.text_hidden = text_hidden;
                        revision.suppressed =
                            text_hidden && text_bytes.is_none() && !text_sha1;
                        let prefix = revision_prefix(&revision, text_bytes);
                        return Ok((
                            revision,
                            prefix,
                            if text_hidden {
                                RevisionBodyState::Hidden
                            } else {
                                RevisionBodyState::Empty
                            },
                            false,
                        ));
                    }
                    _ => {}
                }
            }
            Event::End(e) if local_name_end(&e) == b"revision" => {
                return Err(Error::Xml(
                    "revision ended before its <text> field".into(),
                ));
            }
            Event::Eof => return Err(Error::Xml("EOF inside <revision> before <text>".into())),
            _ => {}
        }
    }
}

fn revision_prefix(revision: &Revision, text_bytes: Option<u64>) -> RevisionPrefix {
    RevisionPrefix {
        id: revision.id,
        parent_id: revision.parent_id,
        timestamp: revision.timestamp,
        contributor: revision.contributor.clone(),
        minor: revision.minor,
        comment: revision.comment.clone(),
        origin: revision.origin,
        model: revision.model.clone(),
        format: revision.format.clone(),
        text_bytes,
        text_hidden: revision.text_hidden,
        comment_hidden: revision.comment_hidden,
        contributor_hidden: revision.contributor_hidden,
        suppressed: revision.suppressed,
    }
}

fn text_bytes_attr(s: &BytesStart<'_>) -> Result<Option<u64>> {
    if !attr_present(s, b"bytes") {
        return Ok(None);
    }
    let raw = attr_string(s, b"bytes");
    raw.parse::<u64>().map(Some).map_err(|e| {
        Error::Xml(format!(
            "text bytes attribute {raw:?} is not an unsigned byte count: {e}"
        ))
    })
}

fn finish_streamed_revision<B: BufRead>(
    reader: &mut Reader<B>,
    revision: &mut Revision,
    text_end_pending: bool,
) -> Result<Revision> {
    let mut buf = Vec::new();
    if text_end_pending {
        buf.clear();
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?
        {
            Event::End(e) if local_name_end(&e) == b"text" => {}
            _ => {
                return Err(Error::Xml(
                    "streamed text body was not followed by </text>".into(),
                ))
            }
        }
    }
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?;
        match ev {
            Event::Start(s) => {
                let name = local_name(&s).to_vec();
                match name.as_slice() {
                    b"sha1" => revision.sha1 = read_text(reader, &name)?,
                    b"id" | b"parentid" | b"timestamp" | b"contributor" | b"comment"
                    | b"minor" | b"origin" | b"model" | b"format" | b"text" => {
                        return Err(late_revision_prefix_field(&name))
                    }
                    _ => skip_to_end(reader, QName(&name))?,
                }
            }
            Event::Empty(s) => {
                let name = local_name(&s);
                match name {
                    b"sha1" => {}
                    b"id" | b"parentid" | b"timestamp" | b"contributor" | b"comment"
                    | b"minor" | b"origin" | b"model" | b"format" | b"text" => {
                        return Err(late_revision_prefix_field(name))
                    }
                    _ => {}
                }
            }
            Event::End(e) if local_name_end(&e) == b"revision" => return Ok(revision.clone()),
            Event::Eof => return Err(Error::Xml("EOF inside streamed <revision>".into())),
            _ => {}
        }
    }
}

fn late_revision_prefix_field(name: &[u8]) -> Error {
    Error::Xml(format!(
        "revision field <{}> appears after <text>; prefix metadata must precede the text body",
        String::from_utf8_lossy(name)
    ))
}

/// Consume an XML text element without asking quick-xml to materialize the
/// complete text event.  `Reader::read_event_into` is excellent for ordinary
/// fields, but a single `<text>` event can be as large as the entire wiki
/// revision.  The dump format permits only character data or CDATA in this
/// element, so scanning that body directly from the reader's `BufRead` keeps
/// the caller's write path bounded by the input buffer and sink.
fn stream_text<R: Read>(
    reader: &mut Reader<ReplayBufReader<R>>,
    qualified_end: &[u8],
    output: &mut dyn Write,
) -> Result<()> {
    let input = reader.get_mut();
    loop {
        let chunk = input
            .fill_buf()
            .map_err(|e| Error::Xml(e.to_string()))?;
        if chunk.is_empty() {
            return Err(Error::Xml(format!(
                "EOF inside <{}>",
                String::from_utf8_lossy(qualified_end)
            )));
        }
        let special = chunk
            .iter()
            .position(|byte| matches!(*byte, b'<' | b'&'));
        match special {
            Some(0) if chunk[0] == b'&' => {
                input.consume(1);
                stream_entity(input, output)?;
            }
            Some(0) => {
                input.consume(1);
                let kind = read_xml_byte(input)?;
                match kind {
                    b'/' => {
                        let closing = consume_text_end(input, qualified_end)?;
                        // quick-xml observed the opening tag and owns the XML
                        // nesting stack. Replay only this bounded grammar token
                        // so its next event consumes the matching end tag; the
                        // unbounded body itself has already gone to `output`.
                        input.push_back(&closing).map_err(Error::Io)?;
                        return Ok(());
                    }
                    b'!' => {
                        expect_xml_bytes(input, b"[CDATA[")?;
                        stream_cdata(input, output)?;
                    }
                    _ => {
                        return Err(Error::Xml(
                            "nested markup inside revision text is unsupported".into(),
                        ))
                    }
                }
            }
            Some(position) => {
                output
                    .write_all(&chunk[..position])
                    .map_err(Error::Io)?;
                input.consume(position);
            }
            None => {
                output.write_all(chunk).map_err(Error::Io)?;
                let length = chunk.len();
                input.consume(length);
            }
        }
    }
}

fn read_xml_byte<B: BufRead>(input: &mut B) -> Result<u8> {
    let chunk = input
        .fill_buf()
        .map_err(|e| Error::Xml(e.to_string()))?;
    let Some(byte) = chunk.first().copied() else {
        return Err(Error::Xml("unexpected EOF in revision text markup".into()));
    };
    input.consume(1);
    Ok(byte)
}

fn expect_xml_bytes<B: BufRead>(input: &mut B, expected: &[u8]) -> Result<()> {
    for want in expected {
        if read_xml_byte(input)? != *want {
            return Err(Error::Xml("malformed CDATA opener".into()));
        }
    }
    Ok(())
}

fn consume_text_end<B: BufRead>(input: &mut B, qualified_end: &[u8]) -> Result<Vec<u8>> {
    let mut closing = Vec::with_capacity(qualified_end.len() + 3);
    closing.extend_from_slice(b"</");
    for expected in qualified_end {
        let actual = read_xml_byte(input)?;
        if actual != *expected {
            return Err(Error::Xml(format!(
                "unexpected end tag inside <{}>",
                String::from_utf8_lossy(qualified_end)
            )));
        }
        closing.push(actual);
    }
    loop {
        let byte = read_xml_byte(input)?;
        closing.push(byte);
        if byte == b'>' {
            return Ok(closing);
        }
        if !byte.is_ascii_whitespace() {
            return Err(Error::Xml(format!(
                "malformed end tag for <{}>",
                String::from_utf8_lossy(qualified_end)
            )));
        }
    }
}

fn stream_cdata<B: BufRead>(input: &mut B, output: &mut dyn Write) -> Result<()> {
    let mut carry = [0_u8; 2];
    let mut carry_len = 0_usize;
    loop {
        let chunk = input
            .fill_buf()
            .map_err(|e| Error::Xml(e.to_string()))?;
        if chunk.is_empty() {
            return Err(Error::Xml("EOF inside revision CDATA".into()));
        }
        let mut emitted = Vec::with_capacity(chunk.len());
        let mut consumed = 0;
        let mut close_at = None;
        for &byte in chunk {
            if carry_len == 2 && carry[0] == b']' && carry[1] == b']' && byte == b'>' {
                close_at = Some(consumed + 1);
                break;
            }
            if carry_len < 2 {
                carry[carry_len] = byte;
                carry_len += 1;
            } else {
                emitted.push(carry[0]);
                carry[0] = carry[1];
                carry[1] = byte;
            }
            consumed += 1;
        }
        if let Some(consumed) = close_at {
            input.consume(consumed);
            output.write_all(&emitted).map_err(Error::Io)?;
            return Ok(());
        }
        input.consume(consumed);
        output.write_all(&emitted).map_err(Error::Io)?;
    }
}

fn stream_entity<B: BufRead>(input: &mut B, output: &mut dyn Write) -> Result<()> {
    let mut entity = [0_u8; 64];
    let mut length = 0;
    loop {
        let byte = read_xml_byte(input)?;
        if byte == b';' {
            break;
        }
        if length == entity.len() {
            return Err(Error::Xml("XML entity in revision text is too long".into()));
        }
        entity[length] = byte;
        length += 1;
    }
    let entity = &entity[..length];
    let value = match entity {
        b"amp" => "&",
        b"lt" => "<",
        b"gt" => ">",
        b"apos" => "'",
        b"quot" => "\"",
        _ if entity.starts_with(b"#x") || entity.starts_with(b"#X") => {
            let digits = std::str::from_utf8(&entity[2..])
                .map_err(|e| Error::Xml(e.to_string()))?;
            let value = u32::from_str_radix(digits, 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| Error::Xml("invalid hexadecimal XML character reference".into()))?;
            let mut encoded = [0_u8; 4];
            output
                .write_all(value.encode_utf8(&mut encoded).as_bytes())
                .map_err(Error::Io)?;
            return Ok(());
        }
        _ if entity.starts_with(b"#") => {
            let digits = std::str::from_utf8(&entity[1..])
                .map_err(|e| Error::Xml(e.to_string()))?;
            let value = digits
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| Error::Xml("invalid decimal XML character reference".into()))?;
            let mut encoded = [0_u8; 4];
            output
                .write_all(value.encode_utf8(&mut encoded).as_bytes())
                .map_err(Error::Io)?;
            return Ok(());
        }
        _ => return Err(Error::Xml("unknown XML entity in revision text".into())),
    };
    output.write_all(value.as_bytes()).map_err(Error::Io)
}

fn parse_contributor<B: BufRead>(
    reader: &mut Reader<B>,
    start: &BytesStart<'_>,
) -> Result<(Contributor, bool)> {
    let deleted = attr_string(start, b"deleted") == "deleted";
    if deleted {
        skip_to_end(reader, QName(b"contributor"))?;
        return Ok((Contributor::Hidden, true));
    }
    let mut username: Option<String> = None;
    let mut user_id: Option<i64> = None;
    let mut ip: Option<String> = None;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Xml(e.to_string()))?;
        match ev {
            Event::Start(s) => {
                let name = local_name(&s).to_vec();
                match name.as_slice() {
                    b"username" => username = Some(read_text(reader, &name)?),
                    b"id" => {
                        let v: i64 = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("contributor id: {e}")))?;
                        user_id = Some(v);
                    }
                    b"ip" => ip = Some(read_text(reader, &name)?),
                    _ => skip_to_end(reader, QName(&name))?,
                }
            }
            Event::End(e) if local_name_end(&e) == b"contributor" => {
                let c = if let Some(ip) = ip {
                    Contributor::Anonymous { ip }
                } else if let (Some(u), Some(id)) = (username, user_id) {
                    Contributor::Named {
                        username: u,
                        user_id: id,
                    }
                } else {
                    Contributor::Hidden
                };
                return Ok((c, false));
            }
            Event::Eof => return Err(Error::Xml("EOF inside <contributor>".into())),
            _ => {}
        }
    }
}
