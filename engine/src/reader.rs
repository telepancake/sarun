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
                        links.push(LinkRef { line: li, span_range: (start, spans.len()), url: u });
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
            links.push(LinkRef { line: li, span_range: (start, spans.len()), url: u });
        }
        // Heading detection: the rich decorator prefixes `#`*level + ' ', and
        // heading lines are never Preformat-tagged (a leading # inside a code
        // block keeps its Preformat annotation, which styles Cyan — accepted).
        let hashes = text.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes)
            && text.as_bytes().get(hashes) == Some(&b' ')
            && !spans.first().is_some_and(|s| s.style.fg == Some(Color::Cyan))
        {
            headings.push(Heading { line: li, level: hashes, text: text[hashes + 1..].to_string() });
            let color = HEADING_COLORS[hashes.min(HEADING_COLORS.len()) - 1];
            for s in &mut spans {
                s.style = s.style.fg(color).add_modifier(Modifier::BOLD);
            }
        }
        plain.push(text);
        lines.push(Line::from(spans));
    }
    Doc { lines, links, headings, fragments, plain, width }
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

    /// Toggle the REVERSED (focus) modifier on one link's spans — O(spans of
    /// that link), never a document rebuild.
    fn set_link_focused(&mut self, link: usize, on: bool) {
        let Some(l) = self.links.get(link) else { return };
        let Some(line) = self.lines.get_mut(l.line) else { return };
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
    Wiki { root: PathBuf, title: String },
    Ietf { root: PathBuf, draft: Option<String> },
    Bytes { name: String },
}

impl Source {
    fn label(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::Wiki { title, .. } => format!("wiki:{title}"),
            Source::Ietf { draft: None, .. } => "ietf:drafts".into(),
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

// ── wiki page rendering (in-process `wikimak serve` page path) ──────────────

/// Follow `#REDIRECT` chains at head, loop-capped — the same contract as
/// serve.rs `resolve_page` (which is private to that module).
const MAX_REDIRECT_HOPS: usize = 10;

fn resolve_wiki_page(
    inst: &wikimak_wikipedia::Instance,
    raw: &str,
) -> anyhow::Result<(u64, String)> {
    let original = raw.replace('_', " ").trim().to_string();
    let mut current = original.clone();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..=MAX_REDIRECT_HOPS {
        let pid = inst
            .page_id_by_title_at(&current, None)
            .map_err(|e| anyhow::anyhow!("wiki page lookup {current:?}: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("wiki: no page titled {current:?}"))?;
        if !seen.insert(pid) {
            return Ok((pid, current));
        }
        let text = inst
            .page_text_at(pid, None)
            .map_err(|e| anyhow::anyhow!("wiki page text {current:?}: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("wiki: no text at {current:?}"))?;
        match wikimak_wikitext::parse_redirect(&String::from_utf8_lossy(&text)) {
            Some(target) => current = target.replace('_', " ").trim().to_string(),
            None => return Ok((pid, current)),
        }
    }
    anyhow::bail!("wiki: redirect loop from {original:?}")
}

/// Render one wiki page to HTML: open the store read-side (shared flock,
/// DROPPED on return — holding it would block a mirror import elsewhere),
/// resolve redirects, and run the same wikitext→HTML renderer `wikimak
/// serve` uses, with `/wiki/` hrefs so link-follow can recognize internal
/// targets. Returns (html, resolved display title).
fn wiki_page_html(root: &Path, title: &str) -> anyhow::Result<(String, String)> {
    use wikimak_wikitext::PageStore;
    let inst = wikimak_wikipedia::Instance::open_read(wikimak_wikipedia::read_config(
        root.to_path_buf(),
    ))
    .map_err(|e| anyhow::anyhow!("wiki open {}: {e}", root.display()))?;
    let view = wikimak_wikipedia::asof::AsOfView::new(&inst, None)
        .map_err(|e| anyhow::anyhow!("wiki site config: {e}"))?;
    let (pid, resolved) = resolve_wiki_page(&inst, title)?;
    let site = view.site();
    let title_obj = wikimak_wikitext::Title::parse(&resolved, site);
    let display = title_obj.prefixed(site);
    let text = inst
        .page_text_at(pid, None)
        .map_err(|e| anyhow::anyhow!("wiki page text: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("wiki: no text at {resolved:?}"))?;
    let wikitext = String::from_utf8_lossy(&text);
    let invoker = wikimak_scribunto::LuaInvoker::new().ok();
    let opts = wikimak_wikitext::RenderOptions {
        invoker: invoker
            .as_ref()
            .map(|i| i as &dyn wikimak_wikitext::ModuleInvoker),
        media: None,
        link_prefix: "/wiki/".into(),
        asof_query: String::new(),
    };
    let out = wikimak_wikitext::render(&view, &title_obj, &wikitext, &opts);
    let html = format!(
        "<h1>{}</h1>\n{}",
        wikimak_wikitext::html::escape(&display),
        out.html
    );
    Ok((html, display))
}

/// Pick a page to land on when a wiki mirror is opened without a title:
/// "Main Page" when the store has one, else the first page in title order.
pub fn wiki_default_title(root: &Path) -> anyhow::Result<String> {
    let inst = wikimak_wikipedia::Instance::open_read(wikimak_wikipedia::read_config(
        root.to_path_buf(),
    ))
    .map_err(|e| anyhow::anyhow!("wiki open {}: {e}", root.display()))?;
    if inst
        .page_id_by_title_at("Main Page", None)
        .map_err(|e| anyhow::anyhow!("wiki lookup: {e}"))?
        .is_some()
    {
        return Ok("Main Page".into());
    }
    let pages = inst
        .pages(None, 1)
        .map_err(|e| anyhow::anyhow!("wiki page listing: {e}"))?;
    pages
        .into_iter()
        .next()
        .map(|(_, t)| t)
        .ok_or_else(|| anyhow::anyhow!("wiki store at {} has no pages", root.display()))
}

fn ietf_draft_list_html(root: &Path) -> anyhow::Result<(String, String)> {
    let cfg = ietf_mirror::MirrorConfig::new(root.to_path_buf());
    let m = ietf_mirror::Mirror::open_read(cfg)
        .map_err(|e| anyhow::anyhow!("ietf open {}: {e}", root.display()))?;
    let drafts = m.drafts()
        .map_err(|e| anyhow::anyhow!("ietf drafts: {e}"))?;
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
    let mut html = String::from(
        &format!("<h1>IETF Drafts</h1>\n<p>{} drafts in {} groups</p>\n",
            drafts.len(), groups.len()));
    for (wg, names) in &groups {
        html.push_str(&format!(
            "<h2>{wg} ({})</h2>\n<ul>\n", names.len()));
        // Show first 50 drafts per group, with a note if truncated.
        for name in names.iter().take(50) {
            html.push_str(&format!(
                "<li><a href=\"/ietf/{name}\">{name}</a></li>\n"));
        }
        if names.len() > 50 {
            html.push_str(&format!(
                "<li>... and {} more</li>\n", names.len() - 50));
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
    let entry = m.head(draft)
        .map_err(|e| anyhow::anyhow!("ietf head {draft}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no draft {draft}"))?;
    let display = format!("{} rev {} {}", draft, entry.rev,
        entry.date.as_deref().unwrap_or("-"));
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
        Source::Wiki { root, title } => {
            let (html, display) = wiki_page_html(root, title)?;
            Ok((html.into_bytes(), Kind::Html, display))
        }
        Source::Ietf { root, draft } => {
            match draft {
                None => {
                    let (html, display) = ietf_draft_list_html(root)?;
                    Ok((html.into_bytes(), Kind::Html, display))
                }
                Some(name) => {
                    let (text, display) = ietf_draft_text(root, name)?;
                    Ok((text, Kind::Text, display))
                }
            }
        }
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
    query: String,
    matches: Vec<usize>,
    /// Follow history: the source we came from + its scroll position.
    history: Vec<(Source, usize)>,
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
            query: String::new(),
            matches: Vec::new(),
            history: Vec::new(),
            status: "j/k scroll · Tab links · Enter follow · n/p headings · / search · z zoom · o open · Esc close".into(),
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
        let title = match title {
            Some(t) => t,
            None => wiki_default_title(&root)?,
        };
        let source = Source::Wiki { root, title };
        let (raw, kind, display) = load_source(&source)?;
        Reader::new(source, raw, kind, display)
    }

    /// Open an IETF mirror: `draft: None` lands on the draft list,
    /// `Some(name)` opens that draft's latest revision as text.
    pub fn open_ietf(root: PathBuf, draft: Option<String>) -> anyhow::Result<Reader> {
        let source = Source::Ietf { root, draft };
        let (raw, kind, display) = load_source(&source)?;
        Reader::new(source, raw, kind, display)
    }

    /// Open caller-supplied bytes (e.g. a box file fetched over the control
    /// socket); `name` decides html/md/text dispatch and titles the pane.
    pub fn open_bytes(name: String, raw: Vec<u8>) -> anyhow::Result<Reader> {
        if raw.len() as u64 > MAX_DOC_BYTES {
            anyhow::bail!("reader: {name} is {} bytes (cap {MAX_DOC_BYTES})", raw.len());
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
        let (raw, kind, display) = load_source(&source)?;
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
        if let Some(old) = self.focus_link {
            self.doc.set_link_focused(old, false);
        }
        self.doc.set_link_focused(next, true);
        self.focus_link = Some(next);
        let line = self.doc.links[next].line;
        self.scroll_to(line);
        self.status = format!(
            "link {}/{}: {}",
            next + 1,
            self.doc.links.len(),
            self.doc.links[next].url
        );
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
            self.doc.headings.iter().rev().find(|h| h.line < self.scroll)
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
        let target = match &self.source {
            Source::Wiki { root, .. } => match path_part.strip_prefix("/wiki/") {
                Some(t) => {
                    let title = percent_decode(t).replace('_', " ");
                    Some(Source::Wiki { root: root.clone(), title })
                }
                None => None,
            },
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
                    Some(Source::Ietf { root: root.clone(), draft: Some(draft) })
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
        match self.load_into(source.clone()) {
            Ok(()) => {
                self.scroll = scroll;
                self.status = format!("back to {}", source.label());
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    #[cfg(test)]
    fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Handle one key. The caller (ui.rs) has already taken the F-keys; the
    /// pane accelerators come back as `NotHandled` so pane switching works.
    pub fn handle_key(&mut self, code: crossterm::event::KeyCode) -> KeyResult {
        use crossterm::event::KeyCode;
        if self.searching {
            match code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                    self.status.clear();
                }
                KeyCode::Enter => self.commit_search(),
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) => self.query.push(c),
                _ => {}
            }
            return KeyResult::Consumed;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
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
            KeyCode::Char('n') => self.jump(1),
            KeyCode::Char('p') => self.jump(-1),
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
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

    /// Outline for the left column: one row per heading, indented by level,
    /// the current position's section highlighted.
    pub fn outline_lines(&self) -> Vec<Line<'static>> {
        if self.doc.headings.is_empty() {
            return vec![Line::from(Span::styled(
                "(no headings)",
                Style::default().add_modifier(Modifier::DIM),
            ))];
        }
        // The heading the viewport is inside: last one at/above scroll+2.
        let here = self
            .doc
            .headings
            .iter()
            .rev()
            .find(|h| h.line <= self.scroll + 2)
            .map(|h| h.line);
        self.doc
            .headings
            .iter()
            .map(|h| {
                let pad = "  ".repeat(h.level.saturating_sub(1));
                let mut st = Style::default().fg(
                    HEADING_COLORS[h.level.min(HEADING_COLORS.len()) - 1],
                );
                if Some(h.line) == here {
                    st = st.add_modifier(Modifier::REVERSED);
                }
                Line::from(Span::styled(format!("{pad}{}", h.text), st))
            })
            .collect()
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
            format!("/{}_", self.query)
        } else {
            self.status.clone()
        };
        let (bstyle, btype) = if focused {
            (
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
        assert!(urls.contains(&"https://example.com/x"), "external link indexed: {urls:?}");
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
        assert!(r.doc.fragments.contains_key("deep"), "non-heading id indexed");
    }

    #[test]
    fn link_styling_and_focus_patch() {
        let mut r = html_reader();
        let buf = frame(&mut r, 60, 20);
        let links = styled_text(&buf, |s| {
            s.fg == Some(Color::Blue) && s.add_modifier.contains(Modifier::UNDERLINED)
        });
        assert!(links.contains("jump link"), "link styled blue+underline: {links:?}");
        // No REVERSED cells before any focus.
        assert_eq!(styled_text(&buf, |s| s.add_modifier.contains(Modifier::REVERSED)), "");
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
    fn heading_jump_lands_on_heading_line() {
        let mut r = html_reader();
        assert_eq!(r.scroll, 0);
        r.handle_key(KeyCode::Char('n'));
        assert_eq!(r.scroll, r.doc.headings[1].line, "n jumps to the next heading line");
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
        assert_eq!(r.scroll, first.saturating_sub(2), "committed search lands on match 1");
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
        assert_eq!(r.source_label(), "fixture.html", "still on the same document");
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
        assert!(matches!(&r.source, Source::File(p) if p.ends_with("b.md")), "{}", r.status);
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
        assert!(r.doc.lines.len() >= 3, "long line hard-wrapped at doc width");
    }

    // ── wiki store ──────────────────────────────────────────────────────────

    const WIKI_XML: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>Reader Test Wiki</sitename><dbname>readertest</dbname>
    <base>http://reader.test/wiki/Main_Page</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces>
      <namespace key="0" case="first-letter"/>
      <namespace key="10" case="first-letter">Template</namespace>
    </namespaces>
  </siteinfo>
  <page>
    <title>Alpha Article</title><ns>0</ns><id>1</id>
    <revision>
      <id>11</id><timestamp>2022-01-01T00:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">== Overview ==
Alpha body linking [[Beta Article]] and [https://example.org outside].</text><sha1>a1</sha1>
    </revision>
  </page>
  <page>
    <title>Beta Article</title><ns>0</ns><id>2</id>
    <revision>
      <id>21</id><timestamp>2022-01-01T00:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">Beta body, back to [[Alpha Article]].</text><sha1>b1</sha1>
    </revision>
  </page>
</mediawiki>"#;

    /// Build a tiny wikimak store the same way the crate's own acceptance
    /// tests do (Instance::open + import + flush) — with dbname "wiki" so
    /// the reader's `read_config` open-read path (the same one the engine's
    /// wiki_attach uses) finds it.
    fn build_wiki_store(root: &Path) {
        let cfg = wikimak_wikipedia::InstanceConfig {
            root: root.to_path_buf(),
            dbname: "wiki".into(),
            max_chain_id: 4096,
            depot: wikimak_depot::DepotConfig {
                root: root.join("depot"),
                max_chain_id: 4096,
                file_size_threshold: 1 << 30,
                eviction_dead_ratio: 0.5,
            },
            title_shard_count: 1,
            title_seal_threshold_bytes: 1 << 20,
            f1_seal_threshold_bytes: 0,
        };
        let inst = wikimak_wikipedia::Instance::open(cfg).expect("create test wiki store");
        let mut stream =
            wikimak_mediawiki::new_page_stream(std::io::Cursor::new(WIKI_XML.as_bytes().to_vec()));
        inst.import(&mut stream).expect("import fixture");
        inst.flush().expect("flush");
    }

    #[test]
    fn wiki_page_renders_and_internal_links_follow() {
        let tmp = tempfile::tempdir().unwrap();
        build_wiki_store(tmp.path());
        // No title → the default-page pick (no Main Page here: first title).
        let picked = wiki_default_title(tmp.path()).unwrap();
        assert_eq!(picked, "Alpha Article");
        let mut r = Reader::open_wiki(tmp.path().to_path_buf(), Some("Alpha Article".into()))
            .unwrap();
        let buf = frame(&mut r, 70, 16);
        let text = buffer_text(&buf);
        assert!(text.contains("Alpha Article"), "page title displayed:\n{text}");
        assert!(text.contains("Alpha body"), "wikitext body rendered:\n{text}");
        assert!(text.contains("Overview"), "section heading rendered:\n{text}");
        // The [[Beta Article]] link is indexed with a /wiki/ href.
        assert!(
            r.doc.links.iter().any(|l| l.url.starts_with("/wiki/") && l.url.contains("Beta")),
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
        assert_eq!(r.history_len(), 1, "wiki follow pushed history: {}", r.status);
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
        let e = match Reader::open_wiki(tmp.path().to_path_buf(), Some("No Such Page".into())) {
            Err(e) => e,
            Ok(_) => panic!("opening a missing wiki page must fail"),
        };
        assert!(e.to_string().contains("No Such Page"), "{e}");
    }
}
