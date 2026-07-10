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

use std::io::{BufRead, BufReader, Read};

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
pub struct RevisionStream<R: Read> {
    reader: Reader<BufReader<R>>,
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
    let mut reader = Reader::from_reader(BufReader::new(r));
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
        if self.ended || self.failed || !self.in_page {
            return None;
        }
        if self.pending_revision {
            self.pending_revision = false;
            let r = parse_revision(&mut self.reader);
            if r.is_err() {
                self.failed = true;
            }
            return Some(r);
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
                        let r = parse_revision(&mut self.reader);
                        if r.is_err() {
                            self.failed = true;
                        }
                        return Some(r);
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

fn parse_revision<B: BufRead>(reader: &mut Reader<B>) -> Result<Revision> {
    let mut rev = Revision {
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
    // Track text-element attrs for the suppressed heuristic.
    let mut have_text = false;
    let mut text_has_bytes = false;
    let mut text_has_sha1 = false;
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
                        rev.id = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("rev id: {e}")))?
                    }
                    b"parentid" => {
                        let p: i64 = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("parentid: {e}")))?;
                        rev.parent_id = Some(p);
                    }
                    b"timestamp" => {
                        let raw = read_text(reader, &name)?;
                        rev.timestamp = DateTime::parse_from_rfc3339(raw.trim())
                            .map_err(|e| Error::Xml(format!("timestamp {raw:?}: {e}")))?
                            .with_timezone(&chrono::Utc);
                    }
                    b"contributor" => {
                        let (c, hidden) = parse_contributor(reader, &s)?;
                        rev.contributor = c;
                        rev.contributor_hidden = hidden;
                    }
                    b"comment" => {
                        if attr_string(&s, b"deleted") == "deleted" {
                            rev.comment_hidden = true;
                            skip_to_end(reader, QName(&name))?;
                        } else {
                            rev.comment = read_text(reader, &name)?;
                        }
                    }
                    b"origin" => {
                        let o: i64 = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("origin: {e}")))?;
                        rev.origin = Some(o);
                    }
                    b"model" => rev.model = read_text(reader, &name)?,
                    b"format" => rev.format = read_text(reader, &name)?,
                    b"text" => {
                        have_text = true;
                        text_has_bytes = attr_present(&s, b"bytes");
                        text_has_sha1 = attr_present(&s, b"sha1");
                        if attr_string(&s, b"deleted") == "deleted" {
                            rev.text_hidden = true;
                            skip_to_end(reader, QName(&name))?;
                        } else {
                            rev.text = read_text(reader, &name)?;
                        }
                    }
                    b"sha1" => rev.sha1 = read_text(reader, &name)?,
                    _ => skip_to_end(reader, QName(&name))?,
                }
            }
            Event::Empty(s) => {
                let name = local_name(&s);
                match name {
                    b"minor" => rev.minor = true,
                    b"comment" => {
                        if attr_string(&s, b"deleted") == "deleted" {
                            rev.comment_hidden = true;
                        }
                    }
                    b"contributor" => {
                        if attr_string(&s, b"deleted") == "deleted" {
                            rev.contributor_hidden = true;
                            rev.contributor = Contributor::Hidden;
                        }
                    }
                    b"text" => {
                        have_text = true;
                        text_has_bytes = attr_present(&s, b"bytes");
                        text_has_sha1 = attr_present(&s, b"sha1");
                        if attr_string(&s, b"deleted") == "deleted" {
                            rev.text_hidden = true;
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) if local_name_end(&e) == b"revision" => {
                // Suppressed heuristic: text deleted AND no bytes attr AND
                // no sha1 attr on the <text> element.
                if rev.text_hidden && have_text && !text_has_bytes && !text_has_sha1 {
                    rev.suppressed = true;
                }
                return Ok(rev);
            }
            Event::Eof => return Err(Error::Xml("EOF inside <revision>".into())),
            _ => {}
        }
    }
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
