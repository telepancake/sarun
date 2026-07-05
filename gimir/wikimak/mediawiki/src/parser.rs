//! Streaming export-0.11 XML parser.
//!
//! Per SPEC §API: yields `Result<Page>` records, exposes `site_info`
//! for the dump-file header.
//!
//! Elements are matched by local name, so default-namespaced exports
//! work without any namespace plumbing on the caller's side.

use std::io::{BufRead, BufReader, Read};

use chrono::DateTime;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::Reader;

use crate::types::{Contributor, Error, Namespace, Page, Result, Revision, SiteInfo};

/// Streaming iterator over `<page>` elements in an export-0.11 document.
///
/// The `<siteinfo>` header is parsed lazily on the first `next()` (or
/// the first `site_info` query that triggers a backing `next()` — but
/// the public `site_info()` is just a peek; tests call `next()` first).
pub struct PageStream<R: Read> {
    reader: Reader<BufReader<R>>,
    buf: Vec<u8>,
    site_info: Option<SiteInfo>,
    header_parsed: bool,
    ended: bool,
    failed: bool,
}

/// Build a `PageStream` over `r`.
pub fn new_page_stream<R: Read>(r: R) -> PageStream<R> {
    let mut reader = Reader::from_reader(BufReader::new(r));
    let cfg = reader.config_mut();
    cfg.trim_text(false);
    PageStream {
        reader,
        buf: Vec::new(),
        site_info: None,
        header_parsed: false,
        ended: false,
        failed: false,
    }
}

impl<R: Read> PageStream<R> {
    /// Consume the stream, returning the underlying reader. The parser
    /// stops at `</mediawiki>`; callers that need end-of-stream effects
    /// on the source (e.g. `VerifyingReader`'s on-EOF checksum) drain
    /// the returned reader.
    pub fn into_inner(self) -> R {
        self.reader.into_inner().into_inner()
    }
}

/// Return the parsed `<siteinfo>` header, or `None` if it has not yet
/// been observed.
pub fn site_info<R: Read>(stream: &PageStream<R>) -> Option<&SiteInfo> {
    stream.site_info.as_ref()
}

impl<R: Read> Iterator for PageStream<R> {
    type Item = Result<Page>;
    fn next(&mut self) -> Option<Self::Item> {
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
                Event::Start(s) if local_name(&s) == b"page" => {
                    return Some(parse_page(&mut self.reader));
                }
                Event::Eof => {
                    self.ended = true;
                    return None;
                }
                Event::End(_) => {
                    // </mediawiki> — keep looping to EOF.
                }
                _ => {}
            }
        }
    }
}

impl<R: Read> PageStream<R> {
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
                    _ => skip_to_end(reader, QName(&name))?,
                }
            }
            Event::End(e) if local_name_end(&e) == b"siteinfo" => return Ok(si),
            Event::Eof => return Err(Error::Xml("EOF inside <siteinfo>".into())),
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

// ---- page / revision parsing ----------------------------------------

fn parse_page<B: BufRead>(reader: &mut Reader<B>) -> Result<Page> {
    let mut page = Page {
        title: String::new(),
        namespace: 0,
        id: 0,
        redirect_title: None,
        revisions: Vec::new(),
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
                    b"title" => page.title = read_text(reader, &name)?,
                    b"ns" => {
                        page.namespace = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("ns: {e}")))?
                    }
                    b"id" => {
                        page.id = read_text(reader, &name)?
                            .trim()
                            .parse()
                            .map_err(|e| Error::Xml(format!("id: {e}")))?
                    }
                    b"redirect" => {
                        // Defensive: redirect usually arrives as Empty,
                        // but in case it has a body, skip its end.
                        page.redirect_title = Some(attr_string(&s, b"title"));
                        skip_to_end(reader, QName(&name))?;
                    }
                    b"revision" => page.revisions.push(parse_revision(reader)?),
                    _ => skip_to_end(reader, QName(&name))?,
                }
            }
            Event::Empty(s) => {
                if local_name(&s) == b"redirect" {
                    page.redirect_title = Some(attr_string(&s, b"title"));
                }
            }
            Event::End(e) if local_name_end(&e) == b"page" => return Ok(page),
            Event::Eof => return Err(Error::Xml("EOF inside <page>".into())),
            _ => {}
        }
    }
}

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
