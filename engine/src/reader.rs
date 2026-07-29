//! Document reader: HTML/Markdown/plain-text → styled terminal lines with
//! link / heading / fragment indexes, plus the pane state (scroll, link
//! focus, search, follow history) the UI mounts as a right-pane view or
//! fullscreen.
//!
//! Rendering pipeline: HTML bytes → html2text rich `TaggedLine`s → one
//! ratatui `Line` per row plus side indexes (links, headings, anchor
//! fragments, per-line plain text for search). Markdown converts to HTML
//! via pulldown-cmark and takes the same path, so both formats share one
//! renderer. Anything else displays as plain text.
//!
//! Memory is bounded by the document: the raw source bytes are kept (so a
//! width change can re-render) alongside the built `Doc`; nothing else
//! accumulates. Link-focus changes patch only the affected spans in place —
//! no rebuild.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use html2text::render::{RichAnnotation, TaggedLine, TaggedLineElement};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// One focusable link occurrence: the styled spans `span_range` on `lines[line]`.
/// A link that wraps over several rows contributes one `LinkRef` per row (each
/// carries the same URL), so focus cycling walks strictly down the page.
#[derive(Debug, Clone)]
pub struct LinkRef {
    pub line: usize,
    /// Range of span indexes within `lines[line]` covered by this link run.
    pub span_range: (usize, usize),
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Heading {
    pub line: usize,
    pub level: usize,
    pub text: String,
}

#[derive(Debug, Clone, Copy)]
struct ScreenLink {
    link: usize,
    x0: u16,
    x1: u16,
    y: u16,
}

/// A rendered document at one specific width.
pub struct Doc {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<LinkRef>,
    pub headings: Vec<Heading>,
    /// HTML anchor (`id=` attribute) → first line it starts on.
    pub fragments: HashMap<String, usize>,
    /// Per-line plain text (concatenated span content) for search.
    pub plain: Vec<String>,
    /// Width the doc was rendered at — the re-render cache key.
    pub width: usize,
}

fn style_for(tags: &[RichAnnotation]) -> Style {
    let mut st = Style::default();
    for tag in tags {
        match tag {
            RichAnnotation::Link(_) => {
                st = st.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
            }
            RichAnnotation::Image(_) => st = st.fg(Color::Magenta),
            RichAnnotation::Emphasis => st = st.add_modifier(Modifier::ITALIC),
            RichAnnotation::Strong => st = st.add_modifier(Modifier::BOLD),
            RichAnnotation::Strikeout => st = st.add_modifier(Modifier::CROSSED_OUT),
            RichAnnotation::Code => st = st.fg(Color::Yellow),
            RichAnnotation::Preformat(_) => st = st.fg(Color::Cyan),
            _ => {}
        }
    }
    st
}

/// Heading palette by level (level 1 = brightest); deeper levels reuse the last.
const HEADING_COLORS: [Color; 3] = [Color::LightGreen, Color::Green, Color::Cyan];

/// Convert html2text rich lines into the Doc model.
fn build_doc(tagged: Vec<TaggedLine<Vec<RichAnnotation>>>, width: usize) -> Doc {
    let mut lines = Vec::with_capacity(tagged.len());
    let mut links: Vec<LinkRef> = Vec::new();
    let mut headings = Vec::new();
    let mut fragments = HashMap::new();
    let mut plain = Vec::with_capacity(tagged.len());
    for (li, tl) in tagged.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut text = String::new();
        // Merge consecutive same-URL spans on one row into one LinkRef
        // (an <a> whose content html2text split into several tagged strings).
        let mut run: Option<(usize, String)> = None; // (first span idx, url)
        for el in tl.iter() {
            let ts = match el {
                TaggedLineElement::Str(ts) => ts,
                TaggedLineElement::FragmentStart(name) => {
                    // First occurrence wins, as in HTML id resolution.
                    fragments.entry(name.clone()).or_insert(li);
                    continue;
                }
            };
            let url = ts.tag.iter().find_map(|t| match t {
                RichAnnotation::Link(u) => Some(u.as_str()),
                _ => None,
            });
            match (&run, url) {
                (Some((_, ru)), Some(u)) if ru == u => {} // run continues
                _ => {
                    if let Some((start, u)) = run.take() {
                        links.push(LinkRef {
                            line: li,
                            span_range: (start, spans.len()),
                            url: u,
                        });
                    }
                    if let Some(u) = url {
                        run = Some((spans.len(), u.to_string()));
                    }
                }
            }
            spans.push(Span::styled(ts.s.clone(), style_for(&ts.tag)));
            text.push_str(&ts.s);
        }
        if let Some((start, u)) = run.take() {
            links.push(LinkRef {
                line: li,
                span_range: (start, spans.len()),
                url: u,
            });
        }
        // Heading detection: the rich decorator prefixes `#`*level + ' ', and
        // heading lines are never Preformat-tagged (a leading # inside a code
        // block keeps its Preformat annotation, which styles Cyan — accepted).
        let hashes = text.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes)
            && text.as_bytes().get(hashes) == Some(&b' ')
            && !spans
                .first()
                .is_some_and(|s| s.style.fg == Some(Color::Cyan))
        {
            headings.push(Heading {
                line: li,
                level: hashes,
                text: text[hashes + 1..].to_string(),
            });
            let color = HEADING_COLORS[hashes.min(HEADING_COLORS.len()) - 1];
            for s in &mut spans {
                s.style = s.style.fg(color).add_modifier(Modifier::BOLD);
            }
        }
        plain.push(text);
        lines.push(Line::from(spans));
    }
    Doc {
        lines,
        links,
        headings,
        fragments,
        plain,
        width,
    }
}

impl Doc {
    /// Render HTML bytes at `width` columns.
    pub fn from_html(html: &[u8], width: usize) -> anyhow::Result<Doc> {
        let cfg = html2text::config::rich();
        let dom = cfg
            .parse_html(html)
            .map_err(|e| anyhow::anyhow!("reader: HTML parse failed: {e}"))?;
        let tree = cfg
            .dom_to_render_tree(&dom)
            .map_err(|e| anyhow::anyhow!("reader: render tree failed: {e}"))?;
        let tagged = cfg
            .render_to_lines(tree, width)
            .map_err(|e| anyhow::anyhow!("reader: render failed: {e}"))?;
        Ok(build_doc(tagged, width))
    }

    /// Render Markdown at `width` columns (pulldown-cmark → HTML → from_html).
    pub fn from_markdown(md: &[u8], width: usize) -> anyhow::Result<Doc> {
        let md = String::from_utf8_lossy(md);
        let parser = pulldown_cmark::Parser::new_ext(&md, pulldown_cmark::Options::all());
        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, parser);
        Doc::from_html(html.as_bytes(), width)
    }

    /// Plain-text fallback: no markup, no links; lines longer than `width`
    /// are hard-wrapped so horizontal content is never lost.
    pub fn from_text(raw: &[u8], width: usize) -> Doc {
        let text = String::from_utf8_lossy(raw);
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut plain = Vec::new();
        for l in text.lines() {
            let l = l.trim_end_matches('\r');
            let mut rest = l;
            loop {
                let cut = rest
                    .char_indices()
                    .nth(width)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                let (head, tail) = rest.split_at(cut);
                plain.push(head.to_string());
                lines.push(Line::from(head.to_string()));
                if tail.is_empty() {
                    break;
                }
                rest = tail;
            }
        }
        Doc {
            lines,
            links: Vec::new(),
            headings: Vec::new(),
            fragments: HashMap::new(),
            plain,
            width,
        }
    }

    fn from_diff(raw: &[u8], width: usize) -> Doc {
        let text = String::from_utf8_lossy(raw);
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut plain = Vec::new();
        for source in text.lines() {
            let source = source.trim_end_matches('\r');
            let style = if source.starts_with("+ ") || source.starts_with("+++") {
                Style::default().fg(Color::Green)
            } else if source.starts_with("- ") || source.starts_with("---") {
                Style::default().fg(Color::Red)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            let mut rest = source;
            loop {
                let cut = rest
                    .char_indices()
                    .nth(width)
                    .map(|(index, _)| index)
                    .unwrap_or(rest.len());
                let (head, tail) = rest.split_at(cut);
                plain.push(head.to_string());
                lines.push(Line::from(Span::styled(head.to_string(), style)));
                if tail.is_empty() {
                    break;
                }
                rest = tail;
            }
        }
        Doc {
            lines,
            links: Vec::new(),
            headings: Vec::new(),
            fragments: HashMap::new(),
            plain,
            width,
        }
    }

    /// Toggle the REVERSED (focus) modifier on one link's spans — O(spans of
    /// that link), never a document rebuild.
    fn set_link_focused(&mut self, link: usize, on: bool) {
        let Some(l) = self.links.get(link) else {
            return;
        };
        let Some(line) = self.lines.get_mut(l.line) else {
            return;
        };
        for si in l.span_range.0..l.span_range.1 {
            if let Some(sp) = line.spans.get_mut(si) {
                sp.style = if on {
                    sp.style.add_modifier(Modifier::REVERSED)
                } else {
                    sp.style.remove_modifier(Modifier::REVERSED)
                };
            }
        }
    }
}

// ── sources ─────────────────────────────────────────────────────────────────

/// Refuse documents past this size instead of chewing memory: the reader keeps
/// the raw bytes (for width re-render) plus the built Doc, so the bound is
/// ~2-3x this per open document (there is only ever one).
const MAX_DOC_BYTES: u64 = 16 << 20;

/// What the reader is showing. `File` is a host path (dispatch by extension);
/// `Wiki` is a page in an attached wikimak store, rendered in-process (the
/// same store-open + wikitext-render path `wikimak serve` uses — never a
/// network fetch). `Bytes` is content handed over by the caller (e.g. a box
/// file fetched over the control socket) — no follow targets on disk.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    File(PathBuf),
    Wiki {
        root: PathBuf,
        title: String,
        timestamp_micros: Option<i64>,
        page_id: Option<u64>,
    },
    WikiSearch {
        root: PathBuf,
        label: String,
        html: std::sync::Arc<[u8]>,
    },
    Ietf {
        root: PathBuf,
        draft: Option<String>,
        filter: Option<String>,
    },
    Bytes {
        name: String,
    },
}

impl Source {
    fn label(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::Wiki {
                title,
                timestamp_micros: None,
                ..
            } => format!("wiki:{title}"),
            Source::Wiki {
                title,
                timestamp_micros: Some(timestamp),
                ..
            } => format!("wiki:{title}@{timestamp}"),
            Source::WikiSearch { label, .. } => label.clone(),
            Source::Ietf {
                draft: None,
                filter: None,
                ..
            } => "ietf:drafts".into(),
            Source::Ietf {
                draft: None,
                filter: Some(f),
                ..
            } => format!("ietf:drafts:{f}"),
            Source::Ietf { draft: Some(d), .. } => format!("ietf:{d}"),
            Source::Bytes { name } => name.clone(),
        }
    }
}

/// How the raw bytes turn into a Doc. Decided once per source by extension
/// (wiki pages are always Html); the width re-render reuses it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Html,
    Markdown,
    Text,
    Diff,
}

fn kind_for_name(name: &str) -> Kind {
    let lower = name.to_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml") {
        Kind::Html
    } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
        Kind::Markdown
    } else {
        Kind::Text
    }
}

fn build(kind: Kind, raw: &[u8], width: usize) -> anyhow::Result<Doc> {
    let width = width.max(10);
    match kind {
        Kind::Html => Doc::from_html(raw, width),
        Kind::Markdown => Doc::from_markdown(raw, width),
        Kind::Text => Ok(Doc::from_text(raw, width)),
        Kind::Diff => Ok(Doc::from_diff(raw, width)),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_title(title: &str) -> String {
    let mut encoded = String::with_capacity(title.len());
    for byte in title.replace(' ', "_").bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b':' | b'/')
        {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn format_timestamp(timestamp_micros: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(timestamp_micros.div_euclid(1_000_000))
        .map(|value| {
            format!(
                "{} {:02}:{:02}:{:02} UTC",
                value.date(),
                value.hour(),
                value.minute(),
                value.second()
            )
        })
        .unwrap_or_else(|_| timestamp_micros.to_string())
}

// ── wiki page rendering (in-process `wikimak serve` page path) ──────────────

/// Follow `#REDIRECT` chains at head, loop-capped — the same contract as
/// serve.rs `resolve_page` (which is private to that module).
const MAX_REDIRECT_HOPS: usize = 10;
const WIKI_REVISION_TEXT_CACHE_BYTES: usize = 64 << 20;

fn resolve_wiki_page(
    archive: &wikimak_wikipedia::archive_browse::ArchiveBrowseIndex,
    raw: &str,
    timestamp_micros: i64,
) -> anyhow::Result<(u64, String)> {
    let original = raw.replace('_', " ").trim().to_string();
    let mut current = original.clone();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..=MAX_REDIRECT_HOPS {
        let pid = archive
            .page_id_by_title(&current, timestamp_micros)
            .ok_or_else(|| anyhow::anyhow!("wiki: no page titled {current:?}"))?;
        if !seen.insert(pid) {
            return Ok((pid, current));
        }
        let text = archive
            .page_text_at(pid, timestamp_micros)
            .map_err(|e| anyhow::anyhow!("wiki page text {current:?}: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("wiki: no text at {current:?}"))?;
        match wikimak_wikitext::parse_redirect(&String::from_utf8_lossy(&text)) {
            Some(target) => current = target.replace('_', " ").trim().to_string(),
            None => return Ok((pid, current)),
        }
    }
    anyhow::bail!("wiki: redirect loop from {original:?}")
}

/// Render one wiki page to HTML directly from the archive, resolve redirects,
/// and run the same wikitext→HTML renderer `wikimak
/// serve` uses, with `/wiki/` hrefs so link-follow can recognize internal
/// targets. Returns (html, resolved display title).
fn wiki_page_html(
    archive: &wikimak_wikipedia::archive_browse::ArchiveBrowseIndex,
    title: &str,
    timestamp_micros: Option<i64>,
    page_id: Option<u64>,
) -> anyhow::Result<(String, String, u64)> {
    let at = timestamp_micros.unwrap_or(i64::MAX);
    let (pid, resolved) = match page_id {
        Some(page_id) => (
            page_id,
            archive
                .page_title_at(page_id, at)
                .map_err(|error| anyhow::anyhow!("wiki page title: {error}"))?
                .unwrap_or_else(|| title.replace('_', " ")),
        ),
        None => resolve_wiki_page(archive, title, at)?,
    };
    let text = archive
        .page_text_at(pid, at)
        .map_err(|e| anyhow::anyhow!("wiki page text: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("wiki: no text at {resolved:?}"))?;
    let (html, display) =
        wiki_wikitext_html(archive, &resolved, timestamp_micros, &text);
    Ok((html, display, pid))
}

fn wiki_wikitext_html(
    archive: &wikimak_wikipedia::archive_browse::ArchiveBrowseIndex,
    resolved: &str,
    timestamp_micros: Option<i64>,
    text: &[u8],
) -> (String, String) {
    use wikimak_wikitext::PageStore;
    let view = archive.view(timestamp_micros);
    let wikitext = String::from_utf8_lossy(&text);
    let site = view.site();
    let title_obj = wikimak_wikitext::Title::parse(resolved, site);
    let display = title_obj.prefixed(site);
    let invoker = wikimak_scribunto::LuaInvoker::new().ok();
    let opts = wikimak_wikitext::RenderOptions {
        invoker: invoker
            .as_ref()
            .map(|i| i as &dyn wikimak_wikitext::ModuleInvoker),
        media: None,
        link_prefix: "/wiki/".into(),
        asof_query: timestamp_micros
            .map(|timestamp| format!("?at={timestamp}"))
            .unwrap_or_default(),
    };
    let out = wikimak_wikitext::render(&view, &title_obj, &wikitext, &opts);
    let html = format!(
        "<h1>{}</h1>\n{}",
        wikimak_wikitext::html::escape(&display),
        out.html
    );
    (html, display)
}

fn raw_wikitext_html(title: &str, text: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(text);
    let mut html = format!(
        "<h1>{} · raw wikitext</h1><pre>",
        wikimak_wikitext::html::escape(title)
    );
    let mut rest = text.as_ref();
    while let Some(open) = rest.find("[[") {
        html.push_str(&wikimak_wikitext::html::escape(&rest[..open]));
        let link = &rest[open..];
        let Some(close) = link.find("]]") else {
            html.push_str(&wikimak_wikitext::html::escape(link));
            rest = "";
            break;
        };
        let original = &link[..close + 2];
        let target = link[2..close]
            .split('|')
            .next()
            .unwrap_or_default()
            .trim();
        if target.is_empty() {
            html.push_str(&wikimak_wikitext::html::escape(original));
        } else {
            html.push_str(&format!(
                "<a href=\"/wiki/{}\">{}</a>",
                percent_encode_title(target),
                wikimak_wikitext::html::escape(original)
            ));
        }
        rest = &link[close + 2..];
    }
    html.push_str(&wikimak_wikitext::html::escape(rest));
    html.push_str("</pre>");
    html.into_bytes()
}

fn revision_diff(previous: &[u8], current: &[u8]) -> Vec<u8> {
    use similar::ChangeTag;
    let previous = String::from_utf8_lossy(previous);
    let current = String::from_utf8_lossy(current);
    let diff = similar::TextDiff::from_lines(previous.as_ref(), current.as_ref());
    let mut output = String::from("--- previous\n+++ selected\n");
    for change in diff.iter_all_changes() {
        let marker = match change.tag() {
            ChangeTag::Delete => "- ",
            ChangeTag::Insert => "+ ",
            ChangeTag::Equal => "  ",
        };
        output.push_str(marker);
        output.push_str(change.value());
        if !change.value().ends_with('\n') {
            output.push('\n');
        }
    }
    output.into_bytes()
}

/// Pick a page to land on when a wiki mirror is opened without a title:
/// "Main Page" when the store has one, else the first page in title order.
pub fn wiki_default_title(root: &Path) -> anyhow::Result<String> {
    let archive = wikimak_wikipedia::archive_browse::ArchiveBrowseIndex::open(
        root,
        root.with_extension("swtitle"),
    )
    .map_err(|e| anyhow::anyhow!("wiki open {}: {e}", root.display()))?;
    wiki_default_title_from(&archive, root)
}

fn wiki_default_title_from(
    archive: &wikimak_wikipedia::archive_browse::ArchiveBrowseIndex,
    root: &Path,
) -> anyhow::Result<String> {
    if let Some(title) = archive
        .site_info()
        .base
        .split_once("/wiki/")
        .map(|(_, title)| percent_decode(title.split(['?', '#']).next().unwrap_or(title)))
        .filter(|title| !title.is_empty())
    {
        if archive.page_id_by_title(&title, i64::MAX).is_some() {
            return Ok(title);
        }
    }
    archive
        .first_page()
        .map_err(|e| anyhow::anyhow!("wiki page listing: {e}"))?
        .map(|(_, t)| t)
        .ok_or_else(|| anyhow::anyhow!("wiki archive at {} has no pages", root.display()))
}

fn ietf_draft_list_html(root: &Path, filter: &str) -> anyhow::Result<(String, String)> {
    let cfg = ietf_mirror::MirrorConfig::new(root.to_path_buf());
    let m = ietf_mirror::Mirror::open_read(cfg)
        .map_err(|e| anyhow::anyhow!("ietf open {}: {e}", root.display()))?;
    let all_drafts = m
        .drafts()
        .map_err(|e| anyhow::anyhow!("ietf drafts: {e}"))?;
    let drafts: Vec<String> = if filter.is_empty() {
        all_drafts
    } else {
        all_drafts
            .into_iter()
            .filter(|d| d.to_lowercase().contains(&filter.to_lowercase()))
            .collect()
    };
    if drafts.is_empty() {
        return Ok((
            "<h1>IETF Drafts</h1>\n<p>No drafts mirrored yet.</p>\n".into(),
            "0 drafts".into(),
        ));
    }
    // Group drafts by working group: "draft-ietf-<wg>-..." → <wg>.
    // Non-ietf drafts go into "other".
    let mut groups: std::collections::BTreeMap<String, Vec<&str>> =
        std::collections::BTreeMap::new();
    for name in &drafts {
        let wg = ietf_wg_of(name);
        groups.entry(wg.to_string()).or_default().push(name);
    }
    let mut html = String::from(&format!("<h1>IETF Drafts</h1>\n<p>{} drafts", drafts.len()));
    if !filter.is_empty() {
        html.push_str(&format!(" matching '{}'", filter));
    }
    html.push_str(&format!(" in {} groups</p>\n", groups.len()));
    for (wg, names) in &groups {
        html.push_str(&format!("<h2>{wg} ({})</h2>\n<ul>\n", names.len()));
        // Show first 50 drafts per group, with a note if truncated.
        for name in names.iter().take(50) {
            html.push_str(&format!("<li><a href=\"/ietf/{name}\">{name}</a></li>\n"));
        }
        if names.len() > 50 {
            html.push_str(&format!("<li>... and {} more</li>\n", names.len() - 50));
        }
        html.push_str("</ul>\n");
    }
    Ok((html, format!("{} drafts", drafts.len())))
}

/// Extract the working group from a draft name: `draft-ietf-<wg>-...` → `<wg>`.
/// Non-ietf drafts (e.g. `draft-ietf-ace-...` → "ace") and non-grouped drafts
/// go into "other".
fn ietf_wg_of(name: &str) -> &str {
    let parts: Vec<&str> = name.splitn(4, '-').collect();
    // draft-ietf-<wg>-... → <wg>
    if parts.len() >= 4 && parts[0] == "draft" && parts[1] == "ietf" {
        return parts[2];
    }
    // draft-<author>-... → "individual"
    if parts.len() >= 3 && parts[0] == "draft" {
        return "individual";
    }
    "other"
}

fn ietf_draft_text(root: &Path, draft: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let cfg = ietf_mirror::MirrorConfig::new(root.to_path_buf());
    let m = ietf_mirror::Mirror::open_read(cfg)
        .map_err(|e| anyhow::anyhow!("ietf open {}: {e}", root.display()))?;
    let entry = m
        .head(draft)
        .map_err(|e| anyhow::anyhow!("ietf head {draft}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no draft {draft}"))?;
    let display = format!(
        "{} rev {} {}",
        draft,
        entry.rev,
        entry.date.as_deref().unwrap_or("-")
    );
    Ok((entry.text, display))
}

fn load_source(source: &Source) -> anyhow::Result<(Vec<u8>, Kind, String)> {
    match source {
        Source::File(p) => {
            let md = std::fs::metadata(p)
                .map_err(|e| anyhow::anyhow!("reader: {}: {e}", p.display()))?;
            if md.len() > MAX_DOC_BYTES {
                anyhow::bail!(
                    "reader: {} is {} bytes (cap {MAX_DOC_BYTES})",
                    p.display(),
                    md.len()
                );
            }
            let raw =
                std::fs::read(p).map_err(|e| anyhow::anyhow!("reader: {}: {e}", p.display()))?;
            let kind = kind_for_name(&p.display().to_string());
            Ok((raw, kind, p.display().to_string()))
        }
        Source::Wiki { .. } => {
            anyhow::bail!("reader: wiki source requires an open archive session")
        }
        Source::WikiSearch { html, label, .. } => {
            Ok((html.to_vec(), Kind::Html, label.clone()))
        }
        Source::Ietf {
            root,
            draft,
            filter,
        } => match draft {
            None => {
                let f = filter.as_deref().unwrap_or("");
                let (html, display) = ietf_draft_list_html(root, f)?;
                Ok((html.into_bytes(), Kind::Html, display))
            }
            Some(name) => {
                let (text, display) = ietf_draft_text(root, name)?;
                Ok((text, Kind::Text, display))
            }
        },
        Source::Bytes { .. } => {
            anyhow::bail!("reader: byte sources are loaded by the caller")
        }
    }
}

// ── the reader pane ─────────────────────────────────────────────────────────

/// What a key did — the UI acts on the non-`Consumed` results (leave the
/// pane, toggle fullscreen, open the path prompt); everything else stays
/// inside the reader.
#[derive(PartialEq, Debug)]
pub enum KeyResult {
    Consumed,
    NotHandled,
    Close,
    ToggleFull,
    OpenPrompt,
    ArchiveSearch {
        pattern: String,
        kind: wikimak_wikipedia::archive_browse::ArchiveSearchKind,
    },
    ContributorEdits {
        contributor: wikimak_wikipedia::ContributorMeta,
        label: String,
    },
}

struct WikiReader {
    archive: std::sync::Arc<wikimak_wikipedia::archive_browse::ArchiveBrowseIndex>,
    page_id: u64,
    page_title: String,
    revisions: Vec<wikimak_wikipedia::archive_browse::PageRevisionSummary>,
    revision_texts: wikimak_wikipedia::archive_browse::PageRevisionTextCursor,
    view_mode: WikiViewMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchMode {
    Document,
    ArchiveTitle,
    ArchiveFullText,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WikiViewMode {
    Rendered,
    Raw,
    Diff,
}

impl WikiViewMode {
    fn next(self) -> Self {
        match self {
            Self::Rendered => Self::Raw,
            Self::Raw => Self::Diff,
            Self::Diff => Self::Rendered,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Rendered => "rendered",
            Self::Raw => "raw wikitext",
            Self::Diff => "revision diff",
        }
    }
}

/// The document reader pane state: one open document, its scroll / link
/// focus / search, and the follow history. The SAME `render` draws the
/// right-pane and fullscreen mounts — only the target Rect differs.
pub struct Reader {
    source: Source,
    raw: Vec<u8>,
    kind: Kind,
    doc: Doc,
    display: String,
    pub scroll: usize,
    focus_link: Option<usize>,
    searching: bool,
    search_mode: SearchMode,
    query: String,
    matches: Vec<usize>,
    /// Follow history: the source we came from + its scroll position.
    history: Vec<(Source, usize)>,
    /// Entries left by Back. A fresh follow or search clears this stack.
    future: Vec<(Source, usize)>,
    /// One archive/index instance for the entire wiki-reading session.
    wiki: Option<WikiReader>,
    /// Which half of the split Reader has keyboard focus.
    history_focused: bool,
    history_selected: usize,
    history_scroll: usize,
    history_view_h: usize,
    history_origin: Option<(Source, usize)>,
    /// Link rectangles from the most recently rendered terminal grid.
    screen_links: Vec<ScreenLink>,
    pub status: String,
    /// Last rendered viewport height (page size for PgUp/PgDn and clamping).
    view_h: usize,
}

impl Reader {
    fn new(source: Source, raw: Vec<u8>, kind: Kind, display: String) -> anyhow::Result<Reader> {
        let doc = build(kind, &raw, 78)?;
        Ok(Reader {
            source,
            raw,
            kind,
            doc,
            display,
            scroll: 0,
            focus_link: None,
            searching: false,
            search_mode: SearchMode::Document,
            query: String::new(),
            matches: Vec::new(),
            history: Vec::new(),
            future: Vec::new(),
            wiki: None,
            history_focused: false,
            history_selected: 0,
            history_scroll: 0,
            history_view_h: 1,
            history_origin: None,
            screen_links: Vec::new(),
            status: "arrows links · h history · v rendered/raw/diff · [/] back/forward · j/k scroll · / page search · T title regexp · F full-text regexp".into(),
            view_h: 20,
        })
    }

    /// Open a host file (html/md by extension, plain text otherwise).
    pub fn open_file(path: PathBuf) -> anyhow::Result<Reader> {
        let source = Source::File(path);
        let (raw, kind, display) = load_source(&source)?;
        Reader::new(source, raw, kind, display)
    }

    /// Open a wikimak store page; `title: None` lands on the default page.
    pub fn open_wiki(root: PathBuf, title: Option<String>) -> anyhow::Result<Reader> {
        let archive = std::sync::Arc::new(
            wikimak_wikipedia::archive_browse::ArchiveBrowseIndex::open(
                &root,
                root.with_extension("swtitle"),
            )
            .map_err(|e| anyhow::anyhow!("wiki open {}: {e}", root.display()))?,
        );
        let title = match title {
            Some(t) => t,
            None => wiki_default_title_from(&archive, &root)?,
        };
        let source = Source::Wiki {
            root,
            title,
            timestamp_micros: None,
            page_id: None,
        };
        let Source::Wiki { title, .. } = &source else {
            unreachable!()
        };
        let (html, display, page_id) = wiki_page_html(&archive, title, None, None)?;
        let revisions = archive
            .page_revisions(page_id)
            .map_err(|e| anyhow::anyhow!("wiki page history: {e}"))?;
        let revision_texts = archive
            .page_revision_text_cursor(page_id, WIKI_REVISION_TEXT_CACHE_BYTES)
            .map_err(|e| anyhow::anyhow!("wiki page revision stream: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("wiki page frame is missing"))?;
        let mut reader = Reader::new(source, html.into_bytes(), Kind::Html, display.clone())?;
        reader.wiki = Some(WikiReader {
            archive,
            page_id,
            page_title: display,
            revisions,
            revision_texts,
            view_mode: WikiViewMode::Rendered,
        });
        Ok(reader)
    }

    /// Open an IETF mirror: `draft: None` lands on the draft list,
    /// `Some(name)` opens that draft's latest revision as text.
    pub fn open_ietf(root: PathBuf, draft: Option<String>) -> anyhow::Result<Reader> {
        let source = Source::Ietf {
            root,
            draft,
            filter: None,
        };
        let (raw, kind, display) = load_source(&source)?;
        Reader::new(source, raw, kind, display)
    }

    /// Set a filter on an IETF draft list and reload. Only works when
    /// the source is an IETF draft list (draft: None).
    #[allow(dead_code)]
    pub fn set_ietf_filter(&mut self, filter: &str) -> anyhow::Result<()> {
        if let Source::Ietf {
            root, draft: None, ..
        } = &self.source
        {
            let source = Source::Ietf {
                root: root.clone(),
                draft: None,
                filter: if filter.is_empty() {
                    None
                } else {
                    Some(filter.to_string())
                },
            };
            self.load_into(source)?;
        }
        Ok(())
    }

    /// Open caller-supplied bytes (e.g. a box file fetched over the control
    /// socket); `name` decides html/md/text dispatch and titles the pane.
    pub fn open_bytes(name: String, raw: Vec<u8>) -> anyhow::Result<Reader> {
        if raw.len() as u64 > MAX_DOC_BYTES {
            anyhow::bail!(
                "reader: {name} is {} bytes (cap {MAX_DOC_BYTES})",
                raw.len()
            );
        }
        let kind = kind_for_name(&name);
        Reader::new(Source::Bytes { name: name.clone() }, raw, kind, name)
    }

    pub fn source_label(&self) -> String {
        self.source.label()
    }

    /// Swap in a new source (link follow / back), keeping the render width.
    /// On failure the current document stays and the error is LOUD on the
    /// status line at the caller.
    fn load_into(&mut self, source: Source) -> anyhow::Result<()> {
        let (raw, kind, display, wiki_page) = match &source {
            Source::Wiki {
                title,
                timestamp_micros,
                page_id,
                ..
            } => {
                let wiki = self
                    .wiki
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("reader: wiki session is not open"))?;
                let (html, display, page_id) =
                    wiki_page_html(&wiki.archive, title, *timestamp_micros, *page_id)?;
                let revisions = wiki
                    .archive
                    .page_revisions(page_id)
                    .map_err(|e| anyhow::anyhow!("wiki page history: {e}"))?;
                let revision_texts = wiki
                    .archive
                    .page_revision_text_cursor(page_id, WIKI_REVISION_TEXT_CACHE_BYTES)
                    .map_err(|e| anyhow::anyhow!("wiki page revision stream: {e}"))?
                    .ok_or_else(|| anyhow::anyhow!("wiki page frame is missing"))?;
                (
                    html.into_bytes(),
                    Kind::Html,
                    display,
                    Some((page_id, revisions, revision_texts)),
                )
            }
            _ => {
                let (raw, kind, display) = load_source(&source)?;
                (raw, kind, display, None)
            }
        };
        let doc = build(kind, &raw, self.doc.width)?;
        self.source = source;
        self.raw = raw;
        self.kind = kind;
        self.display = display;
        self.doc = doc;
        self.scroll = 0;
        self.focus_link = None;
        self.matches.clear();
        self.query.clear();
        self.screen_links.clear();
        if let Some((page_id, revisions, revision_texts)) = wiki_page {
            let wiki = self
                .wiki
                .as_mut()
                .expect("wiki source was loaded through an open wiki session");
            wiki.page_id = page_id;
            wiki.page_title = self.display.clone();
            wiki.revisions = revisions;
            wiki.revision_texts = revision_texts;
            self.history_selected = match &self.source {
                Source::Wiki {
                    timestamp_micros: Some(timestamp),
                    ..
                } => wiki
                    .revisions
                    .iter()
                    .position(|revision| revision.timestamp_micros <= *timestamp)
                    .unwrap_or(0),
                _ => 0,
            };
            self.history_scroll = self.history_selected;
            if wiki.view_mode != WikiViewMode::Rendered {
                self.apply_wiki_view(self.history_selected)?;
            }
        } else if matches!(self.source, Source::WikiSearch { .. }) {
            if let Some(wiki) = self.wiki.as_mut() {
                wiki.revisions.clear();
            }
            self.history_focused = false;
            self.history_selected = 0;
            self.history_scroll = 0;
        }
        Ok(())
    }

    pub fn archive_search_index(
        &self,
    ) -> Option<std::sync::Arc<wikimak_wikipedia::archive_browse::ArchiveBrowseIndex>> {
        self.wiki.as_ref().map(|wiki| wiki.archive.clone())
    }

    pub fn show_archive_search(
        &mut self,
        pattern: &str,
        kind: wikimak_wikipedia::archive_browse::ArchiveSearchKind,
        results: &wikimak_wikipedia::archive_browse::ArchiveSearchResults,
        elapsed: std::time::Duration,
    ) -> anyhow::Result<()> {
        use wikimak_wikipedia::archive_browse::ArchiveSearchKind;
        let root = match &self.source {
            Source::Wiki { root, .. } | Source::WikiSearch { root, .. } => root.clone(),
            _ => anyhow::bail!("archive search is only available in a wiki reader"),
        };
        let escaped_pattern = wikimak_wikitext::html::escape(pattern);
        let noun = match kind {
            ArchiveSearchKind::Title => "title",
            ArchiveSearchKind::FullText => "full-text",
        };
        let shown = results.hits.len();
        let mut html = format!(
            "<h1>Wikipedia {noun} regexp</h1><p><code>{escaped_pattern}</code>: \
             {} matches; showing {shown}. Scanned {} frames with {} workers in {:.2?}.</p><ol>",
            results.match_count, results.searched_frames, results.workers, elapsed
        );
        for hit in &results.hits {
            let title = wikimak_wikitext::html::escape(&hit.title);
            let encoded = percent_encode_title(&hit.title);
            let at = hit.timestamp_micros.map_or_else(
                String::new,
                |timestamp| format!("?pageid={}&at={timestamp}", hit.page_id),
            );
            html.push_str(&format!(
                "<li><a href=\"/wiki/{encoded}{at}\">{title}</a>"
            ));
            if let Some(revision_id) = hit.revision_id {
                html.push_str(&format!(" <small>revision {revision_id}"));
                if let Some(timestamp) = hit.timestamp_micros {
                    html.push_str(&format!(" · {}</small>", format_timestamp(timestamp)));
                } else {
                    html.push_str("</small>");
                }
            }
            if let Some(snippet) = &hit.snippet {
                html.push_str(&format!(
                    "<blockquote>{}</blockquote>",
                    wikimak_wikitext::html::escape(snippet)
                ));
            }
            html.push_str("</li>");
        }
        html.push_str("</ol>");
        let label = format!("wiki:{noun} /{pattern}/");
        let from = (self.source.clone(), self.scroll);
        self.load_into(Source::WikiSearch {
            root,
            label,
            html: std::sync::Arc::from(html.into_bytes()),
        })?;
        self.history.push(from);
        self.future.clear();
        self.status = format!(
            "{} matches · {shown} shown · {:.2?} · Backspace goes back",
            results.match_count, elapsed
        );
        Ok(())
    }

    pub fn show_contributor_edits(
        &mut self,
        contributor: &str,
        results: &wikimak_wikipedia::archive_browse::ArchiveSearchResults,
        elapsed: std::time::Duration,
    ) -> anyhow::Result<()> {
        let root = match &self.source {
            Source::Wiki { root, .. } | Source::WikiSearch { root, .. } => root.clone(),
            _ => anyhow::bail!("contributor edits require an open wiki archive"),
        };
        let escaped_contributor = wikimak_wikitext::html::escape(contributor);
        let shown = results.hits.len();
        let mut html = format!(
            "<h1>Edits by {escaped_contributor}</h1><p>{} edits; showing {shown}. \
             Scanned {} frames with {} workers in {:.2?}.</p><ol>",
            results.match_count, results.searched_frames, results.workers, elapsed
        );
        for hit in &results.hits {
            let title = wikimak_wikitext::html::escape(&hit.title);
            let encoded = percent_encode_title(&hit.title);
            let timestamp = hit.timestamp_micros.unwrap_or(i64::MAX);
            html.push_str(&format!(
                "<li><a href=\"/wiki/{encoded}?pageid={}&at={timestamp}\">{title}</a>",
                hit.page_id
            ));
            if let Some(revision_id) = hit.revision_id {
                html.push_str(&format!(
                    " <small>revision {revision_id} · {}</small>",
                    format_timestamp(timestamp)
                ));
            }
            if let Some(comment) = &hit.snippet {
                html.push_str(&format!(
                    "<blockquote>{}</blockquote>",
                    wikimak_wikitext::html::escape(comment)
                ));
            }
            html.push_str("</li>");
        }
        html.push_str("</ol>");
        let label = format!("wiki:edits:{contributor}");
        let from = (self.source.clone(), self.scroll);
        self.load_into(Source::WikiSearch {
            root,
            label,
            html: std::sync::Arc::from(html.into_bytes()),
        })?;
        self.history.push(from);
        self.future.clear();
        self.status = format!(
            "{} edits · {shown} shown · {:.2?} · Backspace goes back",
            results.match_count, elapsed
        );
        Ok(())
    }

    /// Width-keyed render cache: rebuild the doc only when the viewport
    /// width actually changed. Focus and search matches index into the old
    /// wrap, so they are recomputed / dropped.
    fn ensure_width(&mut self, width: usize) {
        let width = width.max(10);
        if self.doc.width == width {
            return;
        }
        match build(self.kind, &self.raw, width) {
            Ok(doc) => {
                self.doc = doc;
                self.focus_link = None;
                if !self.query.is_empty() {
                    self.matches = find_matches(&self.doc.plain, &self.query);
                }
                self.scroll = self.scroll.min(self.doc.lines.len().saturating_sub(1));
            }
            Err(e) => self.status = format!("re-render: {e}"),
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.doc.lines.len().saturating_sub(1));
    }

    /// Put `line` in view (with a little context above) unless it already is.
    fn scroll_to(&mut self, line: usize) {
        if line < self.scroll || line >= self.scroll + self.view_h.max(1) {
            self.scroll = line.saturating_sub(2);
        }
    }

    fn focus_next(&mut self, dir: isize) {
        if self.doc.links.is_empty() {
            self.status = "no links in this document".into();
            return;
        }
        let n = self.doc.links.len() as isize;
        let cur = self.focus_link.map(|f| f as isize).unwrap_or(-1);
        let next = ((cur + dir).rem_euclid(n)) as usize;
        self.set_focus(next);
        let line = self.doc.links[next].line;
        self.scroll_to(line);
        self.status = format!(
            "link {}/{}: {}",
            next + 1,
            self.doc.links.len(),
            self.doc.links[next].url
        );
    }

    fn set_focus(&mut self, next: usize) {
        if self.focus_link == Some(next) {
            return;
        }
        if let Some(old) = self.focus_link {
            self.doc.set_link_focused(old, false);
        }
        self.doc.set_link_focused(next, true);
        self.focus_link = Some(next);
    }

    /// Move through the links as objects on the most recently rendered
    /// terminal grid. Only links actually visible on that grid participate.
    fn focus_spatial(&mut self, dx: i32, dy: i32) {
        if self.screen_links.is_empty() {
            self.spatial_edge(dx, dy);
            return;
        }
        let current = self
            .focus_link
            .and_then(|link| self.screen_links.iter().find(|item| item.link == link))
            .copied();
        let next = match current {
            None => {
                let key = |item: &&ScreenLink| match (dx, dy) {
                    (-1, 0) | (0, -1) => {
                        (u16::MAX - item.y, u16::MAX - item.x1, item.link)
                    }
                    _ => (item.y, item.x0, item.link),
                };
                self.screen_links.iter().min_by_key(key).copied()
            }
            Some(current) => {
                let cx = (i32::from(current.x0) + i32::from(current.x1)) / 2;
                let cy = i32::from(current.y);
                self.screen_links
                    .iter()
                    .filter(|item| item.link != current.link)
                    .filter_map(|item| {
                        let x = (i32::from(item.x0) + i32::from(item.x1)) / 2;
                        let y = i32::from(item.y);
                        let along = (x - cx) * dx + (y - cy) * dy;
                        if along <= 0 {
                            return None;
                        }
                        let across = (x - cx) * dy - (y - cy) * dx;
                        let distance = i64::from(along).pow(2) + i64::from(across).pow(2);
                        Some((distance, along, across.abs(), item))
                    })
                    .min_by_key(|(distance, along, across, item)| {
                        (*distance, *along, *across, item.link)
                    })
                    .map(|(_, _, _, item)| *item)
            }
        };
        let Some(next) = next else {
            self.spatial_edge(dx, dy);
            return;
        };
        self.set_focus(next.link);
        self.status = format!(
            "link {}/{}: {}",
            next.link + 1,
            self.doc.links.len(),
            self.doc.links[next.link].url
        );
    }

    fn spatial_edge(&mut self, dx: i32, dy: i32) {
        if dy < 0 && self.scroll > 0 {
            self.scroll = self.scroll.saturating_sub(1);
            self.status = "scrolled up".into();
        } else if dy > 0
            && self.scroll + self.view_h.max(1) < self.doc.lines.len()
        {
            self.scroll += 1;
            self.status = "scrolled down".into();
        } else if dx < 0 && self.wiki.as_ref().is_some_and(|wiki| !wiki.revisions.is_empty()) {
            self.focus_history();
            self.status =
                "page history · Up/Down select · Enter revision · u user page · e edits".into();
        } else {
            self.status = "no link or content in that direction".into();
        }
    }

    fn focus_history(&mut self) {
        if !self.history_focused {
            self.history_origin = Some((self.source.clone(), self.scroll));
        }
        self.history_focused = true;
    }

    fn finish_history_focus(&mut self) {
        if let Some(origin) = self.history_origin.take() {
            if origin.0 != self.source || origin.1 != self.scroll {
                self.history.push(origin);
                self.future.clear();
            }
        }
        self.history_focused = false;
    }

    fn move_history(&mut self, amount: isize) {
        let count = self.revision_count();
        if count == 0 {
            self.status = "this page has no revisions".into();
            return;
        }
        self.history_selected = self
            .history_selected
            .saturating_add_signed(amount)
            .min(count - 1);
        if self.history_selected < self.history_scroll {
            self.history_scroll = self.history_selected;
        } else if self.history_selected >= self.history_scroll + self.history_view_h.max(1) {
            self.history_scroll = self
                .history_selected
                .saturating_add(1)
                .saturating_sub(self.history_view_h.max(1));
        }
        self.status = format!(
            "revision {}/{} · Enter opens · u user page · e edits · Right returns",
            self.history_selected + 1,
            count
        );
        if let Err(error) = self.apply_wiki_view(self.history_selected) {
            self.status = error.to_string();
        }
    }

    fn apply_wiki_view(&mut self, revision_index: usize) -> anyhow::Result<()> {
        let width = self.doc.width;
        let (archive, page_title, timestamp_micros, page_id, mode, current, previous) = {
            let wiki = self
                .wiki
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("wiki session is not open"))?;
            let revision = wiki
                .revisions
                .get(revision_index)
                .ok_or_else(|| anyhow::anyhow!("revision is not cached"))?;
            if !revision.has_text {
                anyhow::bail!("selected revision has no retained wikitext");
            }
            let timestamp_micros = revision.timestamp_micros;
            let has_previous = wiki.view_mode == WikiViewMode::Diff
                && wiki
                    .revisions
                    .get(revision_index + 1)
                    .is_some_and(|previous| previous.has_text);
            let current = wiki
                .revision_texts
                .text(revision_index)
                .map_err(|error| anyhow::anyhow!("wiki revision stream: {error}"))?
                .ok_or_else(|| anyhow::anyhow!("selected revision has no retained wikitext"))?;
            let previous = if has_previous {
                wiki.revision_texts
                    .text(revision_index + 1)
                    .map_err(|error| anyhow::anyhow!("wiki revision stream: {error}"))?
            } else {
                None
            };
            (
                wiki.archive.clone(),
                wiki.page_title.clone(),
                timestamp_micros,
                wiki.page_id,
                wiki.view_mode,
                current,
                previous,
            )
        };
        let (raw, kind) = match mode {
            WikiViewMode::Rendered => {
                let (html, _) = wiki_wikitext_html(
                    &archive,
                    &page_title,
                    Some(timestamp_micros),
                    &current,
                );
                (html.into_bytes(), Kind::Html)
            }
            WikiViewMode::Raw => (raw_wikitext_html(&page_title, &current), Kind::Html),
            WikiViewMode::Diff => (
                previous.map_or_else(
                    || b"(no earlier retained wikitext to compare)\n".to_vec(),
                    |previous| revision_diff(&previous, &current),
                ),
                Kind::Diff,
            ),
        };
        let display = format!("{} · {}", page_title, mode.label());
        let doc = build(kind, &raw, width)?;
        if let Source::Wiki {
            timestamp_micros: timestamp,
            page_id: source_page_id,
            ..
        } = &mut self.source
        {
            *timestamp = Some(timestamp_micros);
            *source_page_id = Some(page_id);
        }
        self.raw = raw;
        self.kind = kind;
        self.doc = doc;
        self.display = display;
        self.scroll = 0;
        self.focus_link = None;
        self.matches.clear();
        self.query.clear();
        self.screen_links.clear();
        self.history_selected = revision_index;
        self.status = format!(
            "{} · revision {}/{} · v cycles rendered/raw/diff",
            mode.label(),
            revision_index + 1,
            self.revision_count()
        );
        Ok(())
    }

    fn set_wiki_view_mode(&mut self, mode: WikiViewMode) {
        let Some(wiki) = self.wiki.as_mut() else {
            self.status = "wikitext views are only available for wiki pages".into();
            return;
        };
        wiki.view_mode = mode;
        if let Err(error) = self.apply_wiki_view(self.history_selected) {
            self.status = error.to_string();
        }
    }

    fn open_history_revision(&mut self) {
        let Some(revision_id) = self
            .wiki
            .as_ref()
            .and_then(|wiki| wiki.revisions.get(self.history_selected))
            .map(|revision| revision.revision_id)
        else {
            self.status = "no revision selected".into();
            return;
        };
        if let Err(error) = self.apply_wiki_view(self.history_selected) {
            self.status = error.to_string();
            return;
        }
        self.finish_history_focus();
        self.status = format!(
            "revision {} · Left at edge returns to page history",
            revision_id
        );
    }

    fn selected_contributor(&self) -> Option<wikimak_wikipedia::ContributorMeta> {
        self.wiki
            .as_ref()?
            .revisions
            .get(self.history_selected)
            .map(|revision| revision.contributor.clone())
    }

    fn contributor_label(contributor: &wikimak_wikipedia::ContributorMeta) -> Option<String> {
        match contributor {
            wikimak_wikipedia::ContributorMeta::Named { username, .. } => {
                Some(username.clone())
            }
            wikimak_wikipedia::ContributorMeta::Anonymous { ip } => Some(ip.clone()),
            wikimak_wikipedia::ContributorMeta::Hidden => None,
        }
    }

    fn open_contributor_page(&mut self) {
        let Some(contributor) = self.selected_contributor() else {
            self.status = "no contributor selected".into();
            return;
        };
        let Some(name) = Self::contributor_label(&contributor) else {
            self.status = "this revision's contributor is hidden".into();
            return;
        };
        let Some(wiki) = self.wiki.as_ref() else {
            return;
        };
        let namespace = wiki
            .archive
            .site_info()
            .namespaces
            .iter()
            .find(|namespace| namespace.id == 2)
            .map(|namespace| namespace.localized_name.as_str())
            .filter(|namespace| !namespace.is_empty())
            .unwrap_or("User");
        let title = format!("{namespace}:{name}");
        let Some(page_id) = wiki.archive.page_id_by_title(&title, i64::MAX) else {
            self.status = format!("{name} has no local user page");
            return;
        };
        let root = match &self.source {
            Source::Wiki { root, .. } => root.clone(),
            _ => return,
        };
        let target = Source::Wiki {
            root,
            title,
            timestamp_micros: None,
            page_id: Some(page_id),
        };
        self.finish_history_focus();
        let from = (self.source.clone(), self.scroll);
        match self.load_into(target) {
            Ok(()) => {
                self.history.push(from);
                self.future.clear();
                self.history_focused = false;
                self.status = format!("user page for {name} · Backspace goes back");
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    pub fn handle_mouse(
        &mut self,
        column: u16,
        row: u16,
        kind: crossterm::event::MouseEventKind,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                self.clamp_scroll();
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
                self.clamp_scroll();
            }
            MouseEventKind::Moved => {
                if let Some(link) = self
                    .screen_links
                    .iter()
                    .find(|link| link.y == row && link.x0 <= column && column < link.x1)
                    .map(|link| link.link)
                {
                    self.set_focus(link);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(link) = self
                    .screen_links
                    .iter()
                    .find(|link| link.y == row && link.x0 <= column && column < link.x1)
                    .map(|link| link.link)
                {
                    self.set_focus(link);
                    self.follow();
                }
            }
            _ => {}
        }
    }

    /// n/p: next/previous search match while a query is live, else heading.
    fn jump(&mut self, dir: isize) {
        if !self.matches.is_empty() {
            // The "current" match sits at scroll+2 (jump lands matches there),
            // so n/p move strictly past it, wrapping at the ends.
            let cur = self.scroll + 2;
            let next = if dir > 0 {
                self.matches
                    .iter()
                    .find(|&&l| l > cur)
                    .or(self.matches.first())
            } else {
                self.matches
                    .iter()
                    .rev()
                    .find(|&&l| l < cur)
                    .or(self.matches.last())
            };
            if let Some(&l) = next {
                self.scroll = l.saturating_sub(2);
                let at = self.matches.iter().position(|&m| m == l).unwrap_or(0);
                self.status = format!("match {}/{} · Esc clears", at + 1, self.matches.len());
            }
            return;
        }
        // Jumps land the heading at the top (scroll == heading line), so
        // "next" skips anything already in the first rows and "previous" is
        // strictly above the viewport top.
        let target = if dir > 0 {
            self.doc.headings.iter().find(|h| h.line > self.scroll + 2)
        } else {
            self.doc
                .headings
                .iter()
                .rev()
                .find(|h| h.line < self.scroll)
        };
        match target {
            Some(h) => {
                self.scroll = h.line;
                self.status = format!("{} {}", "#".repeat(h.level), h.text);
            }
            None => self.status = "no more headings".into(),
        }
    }

    /// Jump to an HTML anchor (`id=` fragment). Tries the exact name, then
    /// its percent-decoded and underscore-folded forms.
    pub fn jump_fragment(&mut self, frag: &str) -> bool {
        let candidates = [
            frag.to_string(),
            percent_decode(frag),
            percent_decode(frag).replace('_', " "),
        ];
        for c in &candidates {
            if let Some(&line) = self.doc.fragments.get(c) {
                self.scroll = line;
                self.status = format!("#{frag}");
                return true;
            }
        }
        self.status = format!("no anchor #{frag} in this document");
        false
    }

    fn commit_search(&mut self) {
        self.searching = false;
        if self.query.is_empty() {
            self.matches.clear();
            return;
        }
        self.matches = find_matches(&self.doc.plain, &self.query);
        if self.matches.is_empty() {
            self.status = format!("no match: {}", self.query);
            self.query.clear();
        } else {
            // Land on the first match at/after the current position.
            self.jump(1);
        }
    }

    fn follow(&mut self) {
        let Some(fi) = self.focus_link else {
            self.status = "no link focused — Tab cycles links".into();
            return;
        };
        let url = self.doc.links[fi].url.clone();
        // Same-document anchor.
        if let Some(frag) = url.strip_prefix('#') {
            self.jump_fragment(&frag.to_string());
            return;
        }
        // External URLs are show-only: the reader never dials out.
        if url.contains("://") || url.starts_with("mailto:") {
            self.status = format!("external link (not followed): {url}");
            return;
        }
        let (path_part, frag) = match url.split_once('#') {
            Some((p, f)) => (p.to_string(), Some(f.to_string())),
            None => (url.clone(), None),
        };
        let (path_part, query) = match path_part.split_once('?') {
            Some((path, query)) => (path.to_string(), Some(query.to_string())),
            None => (path_part, None),
        };
        let timestamp_micros = query.as_deref().and_then(|query| {
            query
                .split('&')
                .find_map(|field| field.strip_prefix("at=")?.parse().ok())
        });
        let page_id = query.as_deref().and_then(|query| {
            query
                .split('&')
                .find_map(|field| field.strip_prefix("pageid=")?.parse().ok())
        });
        let target = match &self.source {
            Source::Wiki { root, .. } | Source::WikiSearch { root, .. } => {
                match path_part.strip_prefix("/wiki/") {
                    Some(t) => {
                        let title = percent_decode(t).replace('_', " ");
                        Some(Source::Wiki {
                            root: root.clone(),
                            title,
                            timestamp_micros,
                            page_id,
                        })
                    }
                    None => None,
                }
            }
            Source::File(p) => {
                let resolved = if path_part.starts_with('/') {
                    PathBuf::from(&path_part)
                } else {
                    p.parent().unwrap_or(Path::new(".")).join(&path_part)
                };
                Some(Source::File(resolved))
            }
            Source::Ietf { root, .. } => match path_part.strip_prefix("/ietf/") {
                Some(d) => {
                    let draft = percent_decode(d);
                    Some(Source::Ietf {
                        root: root.clone(),
                        draft: Some(draft),
                        filter: None,
                    })
                }
                None => None,
            },
            Source::Bytes { .. } => None,
        };
        let Some(target) = target else {
            self.status = format!("cannot follow {url} from this document");
            return;
        };
        let from = (self.source.clone(), self.scroll);
        match self.load_into(target) {
            Ok(()) => {
                self.history.push(from);
                self.future.clear();
                if let Some(f) = frag {
                    self.jump_fragment(&f);
                }
                self.status = format!("{} · Backspace goes back", self.display);
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    fn back(&mut self) {
        let Some((source, scroll)) = self.history.pop() else {
            self.status = "history is empty".into();
            return;
        };
        let current = (self.source.clone(), self.scroll);
        match self.load_into(source.clone()) {
            Ok(()) => {
                self.scroll = scroll;
                self.future.push(current);
                self.status = format!("back to {}", source.label());
            }
            Err(e) => {
                self.history.push((source, scroll));
                self.status = e.to_string();
            }
        }
    }

    fn forward(&mut self) {
        let Some((source, scroll)) = self.future.pop() else {
            self.status = "forward history is empty".into();
            return;
        };
        let current = (self.source.clone(), self.scroll);
        match self.load_into(source.clone()) {
            Ok(()) => {
                self.scroll = scroll;
                self.history.push(current);
                self.status = format!("forward to {}", source.label());
            }
            Err(e) => {
                self.future.push((source, scroll));
                self.status = e.to_string();
            }
        }
    }

    #[cfg(test)]
    fn history_len(&self) -> usize {
        self.history.len()
    }

    #[cfg(test)]
    fn future_len(&self) -> usize {
        self.future.len()
    }

    /// Handle one key. The caller (ui.rs) has already taken the F-keys; the
    /// pane accelerators come back as `NotHandled` so pane switching works.
    pub fn handle_key_with_modifiers(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> KeyResult {
        use crossterm::event::{KeyCode, KeyModifiers};
        let code = match code {
            KeyCode::Char(character)
                if self.searching && modifiers.contains(KeyModifiers::ALT) =>
            {
                KeyCode::Char(self.localized_alt_character(
                    character,
                    modifiers.contains(KeyModifiers::SHIFT),
                ))
            }
            other => other,
        };
        self.handle_key(code)
    }

    fn localized_alt_character(&self, character: char, shifted: bool) -> char {
        let language = self
            .wiki
            .as_ref()
            .map(|wiki| wiki.archive.site_info().language.as_str());
        if language != Some("lv") {
            return character;
        }
        let mapped = match character.to_ascii_lowercase() {
            'a' => 'ā',
            'c' => 'č',
            'e' => 'ē',
            'g' => 'ģ',
            'i' => 'ī',
            'k' => 'ķ',
            'l' => 'ļ',
            'n' => 'ņ',
            's' => 'š',
            'u' => 'ū',
            'z' => 'ž',
            _ => return character,
        };
        if shifted || character.is_uppercase() {
            mapped.to_uppercase().next().unwrap_or(mapped)
        } else {
            mapped
        }
    }

    pub fn handle_key(&mut self, code: crossterm::event::KeyCode) -> KeyResult {
        use crossterm::event::KeyCode;
        if self.searching {
            match code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.search_mode = SearchMode::Document;
                    self.query.clear();
                    self.status.clear();
                }
                KeyCode::Enter => {
                    if self.query.is_empty() {
                        self.searching = false;
                    } else {
                        match self.search_mode {
                            SearchMode::Document => self.commit_search(),
                            SearchMode::ArchiveTitle => {
                                self.searching = false;
                                return KeyResult::ArchiveSearch {
                                    pattern: std::mem::take(&mut self.query),
                                    kind: wikimak_wikipedia::archive_browse::ArchiveSearchKind::Title,
                                };
                            }
                            SearchMode::ArchiveFullText => {
                                self.searching = false;
                                return KeyResult::ArchiveSearch {
                                    pattern: std::mem::take(&mut self.query),
                                    kind: wikimak_wikipedia::archive_browse::ArchiveSearchKind::FullText,
                                };
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) => self.query.push(c),
                _ => {}
            }
            return KeyResult::Consumed;
        }
        if self.history_focused {
            match code {
                KeyCode::Up | KeyCode::Char('k') => self.move_history(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_history(1),
                KeyCode::PageUp => self.move_history(-(self.history_view_h.max(1) as isize)),
                KeyCode::PageDown => self.move_history(self.history_view_h.max(1) as isize),
                KeyCode::Home | KeyCode::Char('g') => {
                    self.history_selected = 0;
                    self.history_scroll = 0;
                    self.move_history(0);
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.history_selected = self.revision_count().saturating_sub(1);
                    self.move_history(0);
                }
                KeyCode::Enter => self.open_history_revision(),
                KeyCode::Char('u') => self.open_contributor_page(),
                KeyCode::Char('e') => {
                    let Some(contributor) = self.selected_contributor() else {
                        self.status = "no contributor selected".into();
                        return KeyResult::Consumed;
                    };
                    let Some(label) = Self::contributor_label(&contributor) else {
                        self.status = "this revision's contributor is hidden".into();
                        return KeyResult::Consumed;
                    };
                    self.finish_history_focus();
                    return KeyResult::ContributorEdits { contributor, label };
                }
                KeyCode::Char('v') => {
                    let next = self
                        .wiki
                        .as_ref()
                        .map(|wiki| wiki.view_mode.next())
                        .unwrap_or(WikiViewMode::Rendered);
                    self.set_wiki_view_mode(next);
                }
                KeyCode::Char('1') => self.set_wiki_view_mode(WikiViewMode::Rendered),
                KeyCode::Char('2') => self.set_wiki_view_mode(WikiViewMode::Raw),
                KeyCode::Char('3') => self.set_wiki_view_mode(WikiViewMode::Diff),
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab | KeyCode::Esc => {
                    self.finish_history_focus();
                    self.status = "document · arrows navigate links".into();
                }
                KeyCode::Char('z') => {
                    self.finish_history_focus();
                    return KeyResult::ToggleFull;
                }
                KeyCode::Backspace | KeyCode::Char('[') => {
                    self.finish_history_focus();
                    self.back();
                }
                KeyCode::Char(']') => {
                    self.finish_history_focus();
                    self.forward();
                }
                _ => return KeyResult::NotHandled,
            }
            return KeyResult::Consumed;
        }
        match code {
            KeyCode::Char('j') => self.scroll += 1,
            KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Char('h') => {
                if self.wiki.as_ref().is_some_and(|wiki| !wiki.revisions.is_empty()) {
                    self.focus_history();
                    self.status =
                        "page history · v view · Enter revision · u user page · e edits".into();
                }
            }
            KeyCode::Char('v') => {
                let next = self
                    .wiki
                    .as_ref()
                    .map(|wiki| wiki.view_mode.next())
                    .unwrap_or(WikiViewMode::Rendered);
                self.set_wiki_view_mode(next);
            }
            KeyCode::Char('1') => self.set_wiki_view_mode(WikiViewMode::Rendered),
            KeyCode::Char('2') => self.set_wiki_view_mode(WikiViewMode::Raw),
            KeyCode::Char('3') => self.set_wiki_view_mode(WikiViewMode::Diff),
            KeyCode::Left => self.focus_spatial(-1, 0),
            KeyCode::Right => self.focus_spatial(1, 0),
            KeyCode::Up => self.focus_spatial(0, -1),
            KeyCode::Down => self.focus_spatial(0, 1),
            KeyCode::PageDown => self.scroll += self.view_h.max(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.view_h.max(1)),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = self.doc.lines.len().saturating_sub(self.view_h.max(1))
            }
            KeyCode::Tab => self.focus_next(1),
            KeyCode::BackTab => self.focus_next(-1),
            KeyCode::Enter => self.follow(),
            KeyCode::Backspace => self.back(),
            KeyCode::Char('[') => self.back(),
            KeyCode::Char(']') => self.forward(),
            KeyCode::Char('n') => self.jump(1),
            KeyCode::Char('p') => self.jump(-1),
            KeyCode::Char('/') => {
                self.searching = true;
                self.search_mode = SearchMode::Document;
                self.query.clear();
            }
            KeyCode::Char('T') | KeyCode::Char('F') => {
                if self.wiki.is_none() {
                    self.status = "archive regexp search is only available for wiki mirrors".into();
                } else {
                    self.searching = true;
                    self.search_mode = if code == KeyCode::Char('T') {
                        SearchMode::ArchiveTitle
                    } else {
                        SearchMode::ArchiveFullText
                    };
                    self.query.clear();
                }
            }
            KeyCode::Char('z') => return KeyResult::ToggleFull,
            KeyCode::Char('o') => return KeyResult::OpenPrompt,
            KeyCode::Esc => {
                if !self.query.is_empty() || !self.matches.is_empty() {
                    self.query.clear();
                    self.matches.clear();
                    self.status = "search cleared".into();
                } else {
                    return KeyResult::Close;
                }
            }
            _ => return KeyResult::NotHandled,
        }
        self.clamp_scroll();
        KeyResult::Consumed
    }

    pub fn page_history_lines(&mut self, limit: usize) -> Vec<Line<'static>> {
        self.history_view_h = limit.max(1);
        let Some(wiki) = &self.wiki else {
            return vec![Line::from(Span::styled(
                "(page history is available for wiki pages)",
                Style::default().add_modifier(Modifier::DIM),
            ))];
        };
        if wiki.revisions.is_empty() {
            return vec![Line::from(Span::styled(
                "(no revisions)",
                Style::default().add_modifier(Modifier::DIM),
            ))];
        }
        self.history_selected = self.history_selected.min(wiki.revisions.len() - 1);
        self.history_scroll = self
            .history_scroll
            .min(wiki.revisions.len().saturating_sub(self.history_view_h));
        if self.history_selected < self.history_scroll {
            self.history_scroll = self.history_selected;
        } else if self.history_selected >= self.history_scroll + self.history_view_h {
            self.history_scroll = self.history_selected + 1 - self.history_view_h;
        }
        let displayed = match &self.source {
            Source::Wiki {
                timestamp_micros: Some(timestamp),
                ..
            } => wiki
                .revisions
                .iter()
                .position(|revision| revision.timestamp_micros <= *timestamp),
            Source::Wiki { .. } => (!wiki.revisions.is_empty()).then_some(0),
            _ => None,
        };
        wiki.revisions
            .iter()
            .enumerate()
            .skip(self.history_scroll)
            .take(self.history_view_h)
            .map(|(index, revision)| {
                let timestamp = time::OffsetDateTime::from_unix_timestamp(
                    revision.timestamp_micros.div_euclid(1_000_000),
                )
                .map(|value| {
                    format!(
                        "{} {:02}:{:02}",
                        value.date(),
                        value.hour(),
                        value.minute()
                    )
                })
                .unwrap_or_else(|_| revision.timestamp_micros.to_string());
                let contributor = match &revision.contributor {
                    wikimak_wikipedia::ContributorMeta::Named { username, .. } => {
                        username.as_str()
                    }
                    wikimak_wikipedia::ContributorMeta::Anonymous { ip } => ip.as_str(),
                    wikimak_wikipedia::ContributorMeta::Hidden => "(hidden)",
                };
                let mut flags = String::new();
                if revision.minor == Some(true) {
                    flags.push_str(" m");
                }
                if revision.visibility.is_some() {
                    flags.push_str(" hidden");
                }
                if !revision.has_text {
                    flags.push_str(" no-text");
                }
                let comment = revision.comment.replace(['\r', '\n'], " ");
                let is_displayed = displayed == Some(index);
                let line = Line::from(vec![
                    Span::styled(
                        if is_displayed { "▶ " } else { "  " },
                        if is_displayed {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(
                        format!("{timestamp}  "),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("r{} ", revision.revision_id),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{contributor}{flags}")),
                    Span::styled(
                        if comment.is_empty() {
                            String::new()
                        } else {
                            format!(" · {comment}")
                        },
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]);
                if self.history_focused && index == self.history_selected {
                    line.style(Style::default().add_modifier(Modifier::REVERSED))
                } else if is_displayed {
                    line.style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    line
                }
            })
            .collect()
    }

    pub fn revision_count(&self) -> usize {
        self.wiki
            .as_ref()
            .map_or(0, |wiki| wiki.revisions.len())
    }

    pub fn history_focused(&self) -> bool {
        self.history_focused
    }

    pub fn handle_history_mouse(
        &mut self,
        row: u16,
        area: ratatui::layout::Rect,
        kind: crossterm::event::MouseEventKind,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        if row <= area.y || row >= area.y.saturating_add(area.height).saturating_sub(1) {
            return;
        }
        let index = self.history_scroll + usize::from(row - area.y - 1);
        if index >= self.revision_count() {
            return;
        }
        match kind {
            MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left) => {
                self.focus_history();
                self.history_selected = index;
                if matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.open_history_revision();
                } else if let Err(error) = self.apply_wiki_view(index) {
                    self.status = error.to_string();
                }
            }
            MouseEventKind::ScrollUp => self.move_history(-3),
            MouseEventKind::ScrollDown => self.move_history(3),
            _ => {}
        }
    }

    /// Draw the document into `area` — the ONE widget both the right-pane
    /// and fullscreen mounts use. Rebuilds the doc only on width change.
    pub fn render(&mut self, f: &mut ratatui::Frame, area: ratatui::layout::Rect, focused: bool) {
        use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
        let inner_w = area.width.saturating_sub(2) as usize;
        let inner_h = area.height.saturating_sub(2) as usize;
        self.ensure_width(inner_w);
        self.view_h = inner_h.max(1);
        self.clamp_scroll();
        self.screen_links.clear();
        for (index, link) in self.doc.links.iter().enumerate() {
            if link.line < self.scroll || link.line >= self.scroll + inner_h {
                continue;
            }
            let Some(line) = self.doc.lines.get(link.line) else {
                continue;
            };
            let x0 = line.spans[..link.span_range.0]
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            let width = line.spans[link.span_range.0..link.span_range.1]
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            if width == 0 || x0 >= inner_w {
                continue;
            }
            let x0 = x0.min(inner_w);
            let x1 = (x0 + width).min(inner_w);
            self.screen_links.push(ScreenLink {
                link: index,
                x0: area.x.saturating_add(1).saturating_add(x0 as u16),
                x1: area.x.saturating_add(1).saturating_add(x1 as u16),
                y: area
                    .y
                    .saturating_add(1)
                    .saturating_add((link.line - self.scroll) as u16),
            });
        }
        let end = (self.scroll + inner_h).min(self.doc.lines.len());
        let visible: Vec<Line> = self.doc.lines[self.scroll.min(end)..end].to_vec();
        let title = format!(
            " {} · {}/{} · {} links ",
            self.display,
            self.scroll,
            self.doc.lines.len(),
            self.doc.links.len()
        );
        let bottom = if self.searching {
            let prefix = match self.search_mode {
                SearchMode::Document => "/",
                SearchMode::ArchiveTitle => "title regexp /",
                SearchMode::ArchiveFullText => "full-text regexp /",
            };
            format!("{prefix}{}_", self.query)
        } else {
            self.status.clone()
        };
        let (bstyle, btype) = if focused {
            (
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                BorderType::Double,
            )
        } else {
            (Style::default().fg(Color::Gray), BorderType::Plain)
        };
        let para = Paragraph::new(visible).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(btype)
                .border_style(bstyle)
                .title(title)
                .title_bottom(
                    Line::from(bottom)
                        .right_aligned()
                        .style(Style::default().fg(Color::DarkGray)),
                ),
        );
        f.render_widget(para, area);
    }
}

fn find_matches(plain: &[String], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    plain
        .iter()
        .enumerate()
        .filter(|(_, l)| l.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const HTML: &str = r##"
        <h1>Alpha</h1>
        <p>intro text with a <a href="#sec2">jump link</a> here.</p>
        <p>filler one</p><p>filler two</p><p>filler three</p>
        <h2 id="sec2">Beta section</h2>
        <p>body with <a href="https://example.com/x">external link</a> and
           <em>emphasis</em>.</p>
        <p>needle alpha</p>
        <p id="deep">needle beta</p>
    "##;

    fn html_reader() -> Reader {
        Reader::open_bytes("fixture.html".into(), HTML.as_bytes().to_vec()).unwrap()
    }

    /// Render into a TestBackend and return the buffer for style asserts.
    fn frame(r: &mut Reader, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| r.render(f, f.area(), true)).unwrap();
        term.backend().buffer().clone()
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    /// Cells whose style carries the given check, joined as text.
    fn styled_text(buf: &ratatui::buffer::Buffer, pred: impl Fn(&Style) -> bool) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let c = &buf[(x, y)];
                if pred(&c.style()) {
                    s.push_str(c.symbol());
                }
            }
        }
        s
    }

    #[test]
    fn doc_indexes_links_headings_fragments() {
        let r = html_reader();
        let urls: Vec<&str> = r.doc.links.iter().map(|l| l.url.as_str()).collect();
        assert!(urls.contains(&"#sec2"), "anchor link indexed: {urls:?}");
        assert!(
            urls.contains(&"https://example.com/x"),
            "external link indexed: {urls:?}"
        );
        let names: Vec<&str> = r.doc.headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(names, ["Alpha", "Beta section"], "heading index");
        assert_eq!(r.doc.headings[0].level, 1);
        assert_eq!(r.doc.headings[1].level, 2);
        // The id= anchor starts where its element's block starts — at or
        // just above the heading text (html2text attaches the zero-width
        // marker before the block's leading spacing).
        let sec2 = r.doc.fragments["sec2"];
        let hline = r.doc.headings[1].line;
        assert!(
            sec2 <= hline && hline - sec2 <= 3,
            "fragment ({sec2}) points at its heading line ({hline})"
        );
        assert!(
            r.doc.fragments.contains_key("deep"),
            "non-heading id indexed"
        );
    }

    #[test]
    fn link_styling_and_focus_patch() {
        let mut r = html_reader();
        let buf = frame(&mut r, 60, 20);
        let links = styled_text(&buf, |s| {
            s.fg == Some(Color::Blue) && s.add_modifier.contains(Modifier::UNDERLINED)
        });
        assert!(
            links.contains("jump link"),
            "link styled blue+underline: {links:?}"
        );
        // No REVERSED cells before any focus.
        assert_eq!(
            styled_text(&buf, |s| s.add_modifier.contains(Modifier::REVERSED)),
            ""
        );
        // Tab focuses link 0: exactly its text goes REVERSED (span patch).
        assert_eq!(r.handle_key(KeyCode::Tab), KeyResult::Consumed);
        let buf = frame(&mut r, 60, 20);
        let rev = styled_text(&buf, |s| s.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(rev, "jump link", "focused link REVERSED");
        // Cycling on moves the highlight and clears the old one.
        r.handle_key(KeyCode::Tab);
        let buf = frame(&mut r, 60, 20);
        let rev = styled_text(&buf, |s| s.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(rev, "external link", "focus moved to the next link");
        // BackTab returns.
        r.handle_key(KeyCode::BackTab);
        let buf = frame(&mut r, 60, 20);
        let rev = styled_text(&buf, |s| s.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(rev, "jump link");
    }

    #[test]
    fn arrows_and_mouse_use_rendered_link_positions() {
        let html = br#"<p><a href="https://left.test">left</a> gap
                         <a href="https://right.test">right</a></p>
                       <p><a href="https://below.test">below</a></p>"#;
        let mut r = Reader::open_bytes("grid.html".into(), html.to_vec()).unwrap();
        frame(&mut r, 60, 15);
        assert!(!r.screen_links.is_empty());

        r.handle_key(KeyCode::Down);
        let first = r.focus_link.expect("Down focuses a visible link");
        assert_eq!(r.doc.links[first].url, "https://left.test");
        r.handle_key(KeyCode::Right);
        let right = r.focus_link.expect("Right keeps a link focused");
        assert_eq!(r.doc.links[right].url, "https://right.test");
        r.handle_key(KeyCode::Down);
        let below = r.focus_link.expect("Down chooses the nearest lower link");
        assert_eq!(r.doc.links[below].url, "https://below.test");

        let left = r
            .screen_links
            .iter()
            .find(|item| r.doc.links[item.link].url == "https://left.test")
            .copied()
            .unwrap();
        r.handle_mouse(left.x0, left.y, crossterm::event::MouseEventKind::Moved);
        assert_eq!(r.focus_link, Some(left.link), "hover follows screen cells");
        r.handle_mouse(
            left.x0,
            left.y,
            crossterm::event::MouseEventKind::Down(
                crossterm::event::MouseButton::Left,
            ),
        );
        assert!(r.status.contains("external link"), "click follows: {}", r.status);
    }

    #[test]
    fn heading_jump_lands_on_heading_line() {
        let mut r = html_reader();
        assert_eq!(r.scroll, 0);
        r.handle_key(KeyCode::Char('n'));
        assert_eq!(
            r.scroll, r.doc.headings[1].line,
            "n jumps to the next heading line"
        );
        r.handle_key(KeyCode::Char('p'));
        assert_eq!(r.scroll, r.doc.headings[0].line, "p jumps back");
    }

    #[test]
    fn anchor_jump_via_fragment_map() {
        let mut r = html_reader();
        assert!(r.jump_fragment("sec2"));
        assert_eq!(r.scroll, r.doc.fragments["sec2"]);
        assert!(!r.jump_fragment("missing"), "unknown anchor refuses loudly");
        assert!(r.status.contains("missing"));
        // Following the in-document '#sec2' link scrolls, no history entry.
        r.scroll = 0;
        r.handle_key(KeyCode::Tab); // focus "#sec2"
        r.handle_key(KeyCode::Enter);
        assert_eq!(r.scroll, r.doc.fragments["sec2"]);
        assert_eq!(r.history_len(), 0, "same-doc anchor is not a navigation");
    }

    #[test]
    fn search_match_navigation() {
        let mut r = html_reader();
        r.handle_key(KeyCode::Char('/'));
        for c in "needle".chars() {
            r.handle_key(KeyCode::Char(c));
        }
        r.handle_key(KeyCode::Enter);
        assert_eq!(r.matches.len(), 2, "two matching lines");
        let first = r.matches[0];
        let second = r.matches[1];
        assert_eq!(
            r.scroll,
            first.saturating_sub(2),
            "committed search lands on match 1"
        );
        r.handle_key(KeyCode::Char('n'));
        assert_eq!(r.scroll, second.saturating_sub(2), "n advances to match 2");
        r.handle_key(KeyCode::Char('p'));
        assert_eq!(r.scroll, first.saturating_sub(2), "p returns to match 1");
        // Esc clears the query; n/p fall back to headings.
        r.handle_key(KeyCode::Esc);
        assert!(r.matches.is_empty());
        // no match: loud status, query dropped.
        r.handle_key(KeyCode::Char('/'));
        r.handle_key(KeyCode::Char('q'));
        r.handle_key(KeyCode::Char('z'));
        r.handle_key(KeyCode::Enter);
        assert!(r.status.contains("no match"), "{}", r.status);
    }

    #[test]
    fn external_links_are_show_only() {
        let mut r = html_reader();
        r.handle_key(KeyCode::Tab);
        r.handle_key(KeyCode::Tab); // "https://example.com/x"
        r.handle_key(KeyCode::Enter);
        assert!(r.status.contains("external link"), "{}", r.status);
        assert!(r.status.contains("https://example.com/x"));
        assert_eq!(r.history_len(), 0);
        assert_eq!(
            r.source_label(),
            "fixture.html",
            "still on the same document"
        );
    }

    #[test]
    fn file_follow_updates_history_and_back_restores_scroll() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.md");
        let mut body = String::from("# A\n\nsee [the other](b.md)\n");
        body.push_str(&"filler\n\n".repeat(40));
        std::fs::write(&a, body).unwrap();
        std::fs::write(tmp.path().join("b.md"), "# B\n\nback via [a](a.md)\n").unwrap();
        let mut r = Reader::open_file(a.clone()).unwrap();
        frame(&mut r, 40, 10); // set a real viewport
        r.handle_key(KeyCode::Char('j'));
        r.handle_key(KeyCode::Char('j'));
        let scroll_before = r.scroll;
        r.handle_key(KeyCode::Tab);
        r.handle_key(KeyCode::Enter);
        assert_eq!(r.history_len(), 1, "follow pushed history: {}", r.status);
        assert!(
            matches!(&r.source, Source::File(p) if p.ends_with("b.md")),
            "{}",
            r.status
        );
        let buf = frame(&mut r, 40, 10);
        assert!(buffer_text(&buf).contains("# B"), "target doc rendered");
        r.handle_key(KeyCode::Backspace);
        assert_eq!(r.history_len(), 0);
        assert!(matches!(&r.source, Source::File(p) if p == &a));
        assert_eq!(r.scroll, scroll_before, "back restores the scroll position");
        // A dangling link refuses loudly and stays put.
        std::fs::write(&a, "[gone](missing.md)\n").unwrap();
        let mut r = Reader::open_file(a).unwrap();
        r.handle_key(KeyCode::Tab);
        r.handle_key(KeyCode::Enter);
        assert!(r.status.contains("missing.md"), "loud error: {}", r.status);
        assert_eq!(r.history_len(), 0);
    }

    #[test]
    fn width_change_rerenders_and_keeps_search() {
        let mut r = html_reader();
        r.handle_key(KeyCode::Char('/'));
        for c in "needle".chars() {
            r.handle_key(KeyCode::Char(c));
        }
        r.handle_key(KeyCode::Enter);
        let w60 = r.doc.width;
        frame(&mut r, 30, 12);
        assert_ne!(r.doc.width, w60, "narrower frame re-rendered the doc");
        assert_eq!(r.matches.len(), 2, "matches recomputed for the new wrap");
        let before = r.doc.width;
        frame(&mut r, 30, 12);
        assert_eq!(r.doc.width, before, "same width → cache hit");
    }

    #[test]
    fn plain_text_fallback_wraps() {
        let long = format!("short\n{}\n", "x".repeat(200));
        let r = Reader::open_bytes("notes.txt".into(), long.into_bytes()).unwrap();
        assert_eq!(r.kind, Kind::Text);
        assert!(r.doc.links.is_empty());
        assert!(
            r.doc.lines.len() >= 3,
            "long line hard-wrapped at doc width"
        );
    }

    // ── wiki store ──────────────────────────────────────────────────────────

    fn build_wiki_store(root: &Path) -> PathBuf {
        use wikimak_wikipedia::archive::{
            ArchiveWriter, CompressionSettings, ManifestRecord, Record, RevisionRecord, SiteInfoRecord,
            SiteNamespaceRecord,
        };
        use wikimak_wikipedia::{ContributorMeta, RevisionMeta};
        let archive = root.join("readertest.swdump");
        let output = wikimak_wikipedia::archive_set::ArchiveSetOutput::new_in(
            root,
            1 << 20,
        )
        .unwrap();
        let mut writer = ArchiveWriter::with_ref_prefix(
            output,
            1024,
            CompressionSettings::default(),
            b"reader wiki test reference prefix",
        )
        .unwrap();
        let timestamp = chrono::DateTime::parse_from_rfc3339("2022-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        for (page_id, revision_id, title, namespace, text) in [
            (
                1,
                11,
                "Alpha Article",
                0,
                "== Overview ==\nAlpha body linking [[Beta Article]] and [https://example.org outside].",
            ),
            (
                2,
                21,
                "Beta Article",
                0,
                "Beta body, back to [[Alpha Article]].",
            ),
            (3, 31, "Ed", 2, "Ed's local user page."),
        ] {
            writer
                .write(&Record::PageState {
                    page_id,
                    timestamp_micros: timestamp.timestamp_micros(),
                    title: title.into(),
                    namespace: Some(namespace),
                    deleted: false,
                })
                .unwrap();
            writer
                .write(&Record::Revision {
                    page_id,
                    revision: RevisionRecord {
                        meta: RevisionMeta {
                            rev_id: revision_id,
                            parent_id: 0,
                            ts: timestamp,
                            contributor: ContributorMeta::Named {
                                username: "Ed".into(),
                                user_id: 1,
                            },
                            comment: String::new(),
                            sha1: String::new(),
                            flags: 0,
                            text_len: text.len() as u64,
                        },
                        has_text: true,
                        text: text.as_bytes().to_vec(),
                        visibility: None,
                        history: None,
                    },
                })
                .unwrap();
            if page_id == 1 {
                let older_text = "Alpha old body linking [[Beta Article]].";
                writer
                    .write(&Record::Revision {
                        page_id,
                        revision: RevisionRecord {
                            meta: RevisionMeta {
                                rev_id: 10,
                                parent_id: 0,
                                ts: timestamp - chrono::Duration::days(1),
                                contributor: ContributorMeta::Named {
                                    username: "Ed".into(),
                                    user_id: 1,
                                },
                                comment: "initial version".into(),
                                sha1: String::new(),
                                flags: 0,
                                text_len: older_text.len() as u64,
                            },
                            has_text: true,
                            text: older_text.as_bytes().to_vec(),
                            visibility: None,
                            history: None,
                        },
                    })
                    .unwrap();
            }
        }
        writer
            .write(&Record::Manifest {
                timestamp_micros: timestamp.timestamp_micros(),
                manifest: ManifestRecord {
                    wiki_db: "readertest".into(),
                    content_snapshot: "2022-01-01".into(),
                    metadata_snapshot: "2022-01-01".into(),
                    source_files: Vec::new(),
                },
            })
            .unwrap();
        writer
            .write(&Record::SiteInfo {
                timestamp_micros: timestamp.timestamp_micros(),
                site_info: SiteInfoRecord {
                    site_name: "Reader Test Wiki".into(),
                    db_name: "readertest".into(),
                    base: "http://reader.test/wiki/Main_Page".into(),
                    generator: "g".into(),
                    case: "first-letter".into(),
                    language: "lv".into(),
                    rtl: false,
                    server: "http://reader.test".into(),
                    script_path: "/w".into(),
                    namespaces: vec![
                        SiteNamespaceRecord {
                            id: 0,
                            case: "first-letter".into(),
                            localized_name: String::new(),
                            aliases: Vec::new(),
                        },
                        SiteNamespaceRecord {
                            id: 10,
                            case: "first-letter".into(),
                            localized_name: "Template".into(),
                            aliases: Vec::new(),
                        },
                        SiteNamespaceRecord {
                            id: 2,
                            case: "first-letter".into(),
                            localized_name: "Dalībnieks".into(),
                            aliases: vec!["User".into()],
                        },
                    ],
                    interwiki: Vec::new(),
                    magic_words: Vec::new(),
                },
            })
            .unwrap();
        let (output, _) = writer.finish().unwrap();
        output.finish().unwrap().persist(&archive).unwrap();
        wikimak_wikipedia::title_index::build(
            &archive,
            archive.with_extension("swtitle"),
        )
        .unwrap();
        archive
    }

    #[test]
    fn wiki_page_renders_and_internal_links_follow() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = build_wiki_store(tmp.path());
        // No title → the default-page pick (no Main Page here: first title).
        let picked = wiki_default_title(&archive).unwrap();
        assert_eq!(picked, "Alpha Article");
        let mut r =
            Reader::open_wiki(archive.clone(), Some("Alpha Article".into())).unwrap();
        let archive_address = r
            .wiki
            .as_ref()
            .map(|wiki| std::ptr::from_ref(&wiki.archive))
            .unwrap();
        assert!(r.revision_count() > 0);
        assert!(
            buffer_text(&frame(&mut r, 70, 16)).contains("Alpha Article"),
            "initial page renders before persistence check"
        );
        let buf = frame(&mut r, 70, 16);
        let text = buffer_text(&buf);
        assert!(
            text.contains("Alpha Article"),
            "page title displayed:\n{text}"
        );
        assert!(
            text.contains("Alpha body"),
            "wikitext body rendered:\n{text}"
        );
        assert!(
            text.contains("Overview"),
            "section heading rendered:\n{text}"
        );
        // The [[Beta Article]] link is indexed with a /wiki/ href.
        assert!(
            r.doc
                .links
                .iter()
                .any(|l| l.url.starts_with("/wiki/") && l.url.contains("Beta")),
            "internal link indexed: {:?}",
            r.doc.links.iter().map(|l| &l.url).collect::<Vec<_>>()
        );
        // Focus the internal link (skip any earlier ones) and follow it.
        loop {
            r.handle_key(KeyCode::Tab);
            let f = r.focus_link.expect("some link focused");
            if r.doc.links[f].url.starts_with("/wiki/") {
                break;
            }
        }
        r.handle_key(KeyCode::Enter);
        assert_eq!(
            r.wiki
                .as_ref()
                .map(|wiki| std::ptr::from_ref(&wiki.archive))
                .unwrap(),
            archive_address,
            "link following retains the open archive/index"
        );
        assert_eq!(
            r.history_len(),
            1,
            "wiki follow pushed history: {}",
            r.status
        );
        assert!(
            matches!(&r.source, Source::Wiki { title, .. } if title == "Beta Article"),
            "landed on Beta: {} / {:?}",
            r.status,
            r.source
        );
        let text = buffer_text(&frame(&mut r, 70, 16));
        assert!(text.contains("Beta body"), "target page rendered:\n{text}");
        // Back returns to Alpha.
        r.handle_key(KeyCode::Backspace);
        assert!(matches!(&r.source, Source::Wiki { title, .. } if title == "Alpha Article"));
        // External URL from a wiki page: show-only.
        let ext = r
            .doc
            .links
            .iter()
            .position(|l| l.url.starts_with("https://example.org"))
            .expect("external link present");
        r.focus_link = Some(ext);
        r.handle_key(KeyCode::Enter);
        assert!(r.status.contains("external link"), "{}", r.status);
        // A dead wiki title refuses loudly.
        let e = match Reader::open_wiki(archive, Some("No Such Page".into())) {
            Err(e) => e,
            Ok(_) => panic!("opening a missing wiki page must fail"),
        };
        assert!(e.to_string().contains("No Such Page"), "{e}");
    }

    #[test]
    fn wiki_regexp_results_are_navigable() {
        use wikimak_wikipedia::archive_browse::ArchiveSearchKind;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = build_wiki_store(tmp.path());
        let mut reader =
            Reader::open_wiki(archive_path, Some("Alpha Article".into())).unwrap();

        assert_eq!(reader.handle_key(KeyCode::Char('T')), KeyResult::Consumed);
        for character in "Beta".chars() {
            reader.handle_key(KeyCode::Char(character));
        }
        assert_eq!(
            reader.handle_key(KeyCode::Enter),
            KeyResult::ArchiveSearch {
                pattern: "Beta".into(),
                kind: ArchiveSearchKind::Title,
            }
        );

        let archive = reader.archive_search_index().unwrap();
        let results = archive
            .search_regex(
                &regex::Regex::new("Beta body").unwrap(),
                ArchiveSearchKind::FullText,
                500,
            )
            .unwrap();
        assert_eq!(results.match_count, 1);
        reader
            .show_archive_search(
                "Beta body",
                ArchiveSearchKind::FullText,
                &results,
                std::time::Duration::from_millis(12),
            )
            .unwrap();
        let search_payload = match &reader.source {
            Source::WikiSearch { html, .. } => std::sync::Arc::as_ptr(html),
            source => panic!("expected search result source, got {source:?}"),
        };
        assert_eq!(reader.doc.links.len(), 1);
        reader.handle_key(KeyCode::Tab);
        reader.handle_key(KeyCode::Enter);
        assert!(matches!(
            &reader.source,
            Source::Wiki {
                title,
                timestamp_micros: Some(_),
                page_id: Some(2),
                ..
            } if title == "Beta Article"
        ));
        assert!(reader.doc.plain.iter().any(|line| line.contains("Beta body")));
        reader.handle_key(KeyCode::Char('['));
        assert_eq!(reader.future_len(), 1);
        assert!(matches!(
            &reader.source,
            Source::WikiSearch { html, .. }
                if std::ptr::addr_eq(std::sync::Arc::as_ptr(html), search_payload)
        ));
        reader.handle_key(KeyCode::Char(']'));
        assert!(matches!(
            &reader.source,
            Source::Wiki {
                page_id: Some(2),
                ..
            }
        ));
    }

    #[test]
    fn history_pane_focuses_and_opens_selected_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_path = build_wiki_store(tmp.path());
        let mut reader =
            Reader::open_wiki(archive_path, Some("Alpha Article".into())).unwrap();

        assert!(!reader.history_focused());
        reader.handle_key(KeyCode::Char('h'));
        assert!(reader.history_focused());
        let lines = reader.page_history_lines(5);
        assert!(
            lines[0].style.add_modifier.contains(Modifier::REVERSED),
            "selected history row is visibly focused"
        );
        assert!(
            lines[0].to_string().starts_with('▶'),
            "displayed revision has a persistent marker"
        );
        reader.handle_key(KeyCode::Enter);
        assert!(!reader.history_focused());
        assert!(matches!(
            reader.source,
            Source::Wiki {
                timestamp_micros: Some(_),
                page_id: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn history_opens_contributor_page_and_requests_edit_list() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_path = build_wiki_store(tmp.path());
        let mut reader =
            Reader::open_wiki(archive_path, Some("Alpha Article".into())).unwrap();

        reader.handle_key(KeyCode::Char('h'));
        let contributor = wikimak_wikipedia::ContributorMeta::Named {
            username: "Ed".into(),
            user_id: 1,
        };
        assert_eq!(
            reader.handle_key(KeyCode::Char('e')),
            KeyResult::ContributorEdits {
                contributor: contributor.clone(),
                label: "Ed".into(),
            }
        );
        let edits = reader
            .archive_search_index()
            .unwrap()
            .contributor_edits(&contributor, 10)
            .unwrap();
        assert_eq!(edits.match_count, 4);
        assert_eq!(edits.hits.len(), 4);
        reader.handle_key(KeyCode::Char('h'));
        reader.handle_key(KeyCode::Char('u'));
        assert!(matches!(
            &reader.source,
            Source::Wiki { title, .. } if title == "Dalībnieks:Ed"
        ));
    }

    #[test]
    fn wiki_raw_and_diff_views_use_streaming_history_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_path = build_wiki_store(tmp.path());
        let mut reader =
            Reader::open_wiki(archive_path, Some("Alpha Article".into())).unwrap();
        assert_eq!(reader.wiki.as_ref().unwrap().revisions.len(), 2);
        assert!(
            reader
                .wiki
                .as_ref()
                .unwrap()
                .revisions
                .iter()
                .all(|revision| revision.has_text),
            "revision summaries retain availability without retaining text"
        );

        reader.handle_key(KeyCode::Char('h'));
        reader.handle_key(KeyCode::Char('2'));
        assert!(
            String::from_utf8_lossy(&reader.raw).contains("[[Beta Article]]"),
            "raw mode preserves original link markup"
        );
        assert!(!reader.doc.links.is_empty(), "raw links remain navigable");

        reader.handle_key(KeyCode::Char('3'));
        let diff = String::from_utf8_lossy(&reader.raw);
        assert!(diff.contains("--- previous"));
        assert!(diff.contains("- Alpha old body"));
        assert!(diff.contains("+ == Overview =="));

        reader.handle_key(KeyCode::Down);
        assert!(
            String::from_utf8_lossy(&reader.raw)
                .contains("(no earlier retained wikitext to compare)"),
            "moving through history refreshes the document immediately"
        );
        assert!(matches!(
            reader.source,
            Source::Wiki {
                timestamp_micros: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn vertical_spatial_navigation_scrolls_at_viewport_edge() {
        let html = format!(
            "<p><a href=\"first\">first</a></p>{}",
            "<p>more text</p>".repeat(40)
        );
        let mut reader = Reader::open_bytes("long.html".into(), html.into_bytes()).unwrap();
        frame(&mut reader, 50, 8);
        reader.handle_key(KeyCode::Tab);
        assert_eq!(reader.scroll, 0);
        reader.handle_key(KeyCode::Down);
        assert_eq!(reader.scroll, 1, "Down scrolls past the last visible link");
    }

    #[test]
    fn latvian_alt_letters_are_preserved_in_search_input() {
        use crossterm::event::KeyModifiers;
        use wikimak_wikipedia::archive_browse::ArchiveSearchKind;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = build_wiki_store(tmp.path());
        let mut reader =
            Reader::open_wiki(archive_path, Some("Alpha Article".into())).unwrap();
        reader.handle_key(KeyCode::Char('T'));
        reader.handle_key_with_modifiers(KeyCode::Char('r'), KeyModifiers::NONE);
        reader.handle_key_with_modifiers(KeyCode::Char('i'), KeyModifiers::ALT);
        reader.handle_key_with_modifiers(KeyCode::Char('g'), KeyModifiers::ALT);
        reader.handle_key_with_modifiers(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(
            reader.handle_key(KeyCode::Enter),
            KeyResult::ArchiveSearch {
                pattern: "rīģā".into(),
                kind: ArchiveSearchKind::Title,
            }
        );
    }
}
