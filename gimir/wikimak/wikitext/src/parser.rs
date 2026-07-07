//! Parser core (plan §3.1): expanded wikitext → document tree → HTML.
//! Headings, lists, tables, [[links]] (ns/interwiki/File: dispatch),
//! external links, ''formatting'', <nowiki>/<pre>, HTML-in-wikitext
//! sanitization. Conformance corpus: MediaWiki parserTests (fetched at
//! test time, never vendored — GPL).
//!
//! OWNED BY: the parser-core agent.
//!
//! Pipeline shape. Everything that produces final HTML is stashed into a
//! [`Strip`] as an opaque marker (`\x7f…\x7f`); the text between markers
//! stays raw user text and is escaped exactly once, at [`Strip::resolve`].
//! That invariant keeps generated tags from being re-escaped and stray
//! user `<`/`&` from slipping through unescaped. Markers cannot collide
//! with input: `\x7f` is stripped from the input up front.
//!
//! Failure discipline (plan §3): unknown tags are escaped and counted in
//! `misses.unknown_tags`; missing media becomes a placeholder box counted
//! in `misses.missing_media`. Nothing panics; nothing is silently dropped.

use crate::title::{NS_CATEGORY, NS_FILE};
use crate::{
    html, InterwikiEntry, PageStore, RenderMisses, RenderOptions, RenderOutput, SiteConfig, Title,
};

// The Cite extension (<ref>/<references>) lives in a sibling source file.
// lib.rs is frozen, so it is attached here via #[path] rather than a crate
// root `mod` — it stays private to the parser (state never crosses lib.rs).
#[path = "cite.rs"]
mod cite;

const SEP: char = '\u{7f}';

/// Holds final HTML fragments behind opaque markers. `block` records which
/// items are block-level (a marker sitting alone on a line must not be
/// wrapped in `<p>`).
struct Strip {
    items: Vec<String>,
    block: Vec<bool>,
}

impl Strip {
    fn new() -> Self {
        Strip {
            items: Vec::new(),
            block: Vec::new(),
        }
    }

    fn stash(&mut self, html: String, block: bool) -> String {
        let idx = self.items.len();
        self.items.push(html);
        self.block.push(block);
        format!("{SEP}{idx}{SEP}")
    }

    fn stash_inline(&mut self, html: String) -> String {
        self.stash(html, false)
    }

    fn stash_block(&mut self, html: String) -> String {
        self.stash(html, true)
    }

    /// Reserve an empty slot (filled later via [`Strip::set`]) and return
    /// its index. Used by Cite: a `<ref>`'s inline `<sup>` and a group's
    /// `<references>` list are only knowable after the whole page is seen.
    fn reserve(&mut self, block: bool) -> usize {
        let idx = self.items.len();
        self.items.push(String::new());
        self.block.push(block);
        idx
    }

    fn set(&mut self, idx: usize, html: String) {
        self.items[idx] = html;
    }

    /// The marker text that resolves to slot `idx`.
    fn marker(idx: usize) -> String {
        format!("{SEP}{idx}{SEP}")
    }

    /// If `line` (trimmed) is exactly one block-level marker, its index.
    fn lone_block(&self, line: &str) -> Option<usize> {
        let t = line.trim();
        let inner = t.strip_prefix(SEP)?.strip_suffix(SEP)?;
        let idx: usize = inner.parse().ok()?;
        if *self.block.get(idx)? {
            Some(idx)
        } else {
            None
        }
    }

    /// Final pass: escape the non-marker text as body text, substitute
    /// each marker with its stored (already-final) HTML.
    fn resolve(&self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut run = String::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == 0x7f {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != 0x7f {
                    j += 1;
                }
                if j < b.len() {
                    if let Ok(idx) = s[start..j].parse::<usize>() {
                        if !run.is_empty() {
                            out.push_str(&html::escape_body(&run));
                            run.clear();
                        }
                        out.push_str(self.items.get(idx).map(String::as_str).unwrap_or(""));
                        i = j + 1;
                        continue;
                    }
                }
                run.push(SEP);
                i += 1;
            } else {
                let l = html::utf8_len(b[i]);
                run.push_str(&s[i..i + l]);
                i += l;
            }
        }
        if !run.is_empty() {
            out.push_str(&html::escape_body(&run));
        }
        out
    }
}

struct Ctx<'a> {
    store: &'a dyn PageStore,
    site: &'a SiteConfig,
    opts: &'a RenderOptions<'a>,
    misses: RenderMisses,
    categories: Vec<String>,
    strip: Strip,
    ext_counter: u32,
    depth: u32,
    cite: cite::CiteState,
}

pub fn to_html(
    store: &dyn PageStore,
    title: &Title,
    expanded: &str,
    opts: &RenderOptions<'_>,
    misses: RenderMisses,
) -> RenderOutput {
    let _ = title;
    let site = store.site();
    let mut ctx = Ctx {
        store,
        site,
        opts,
        misses,
        categories: Vec::new(),
        strip: Strip::new(),
        ext_counter: 0,
        depth: 0,
        cite: cite::CiteState::default(),
    };

    // Guard the marker namespace and normalize line endings.
    let text = expanded
        .replace(SEP, "")
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let text = ctx.strip_extension_tags(&text);
    // Cite is a two-pass extension: the strip pass collects every ref and
    // reserves markers; finalize backfills the inline <sup>s (now that use
    // counts are known) and each group's <references> list, auto-appending
    // where the page cited but never placed a list.
    let text = ctx.finalize_cite(text);
    let body = ctx.parse_blocks(&text);

    let mut open = String::from("<div class=\"mw-parser-output\"");
    if site.rtl {
        open.push_str(" dir=\"rtl\"");
        if !site.lang.is_empty() {
            open.push_str(&format!(" lang=\"{}\"", html::escape(&site.lang)));
        }
    }
    open.push('>');
    let html = format!("{open}{body}</div>");

    RenderOutput {
        html,
        categories: ctx.categories,
        misses: ctx.misses,
    }
}

impl<'a> Ctx<'a> {
    // ---- extension-tag strip (document level) ------------------------

    /// Pull `<!-- -->`, `<nowiki>`, `<pre>`, `<ref>`, `<gallery>` out
    /// before any block/inline processing. Their bodies must never be
    /// interpreted as wiki markup, so each becomes a single marker.
    fn strip_extension_tags(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'<' {
                if s[i..].starts_with("<!--") {
                    if let Some(end) = s[i + 4..].find("-->") {
                        i = i + 4 + end + 3;
                    } else {
                        i = b.len();
                    }
                    continue;
                }
                if let Some(after) = match_open(s, i, "nowiki") {
                    let (inner, next) = read_to_close(s, after, "nowiki");
                    let m = self.strip.stash_inline(html::escape_body(inner));
                    out.push_str(&m);
                    i = next;
                    continue;
                }
                if let Some(after) = match_open(s, i, "pre") {
                    let (inner, next) = read_to_close(s, after, "pre");
                    let m = self.strip.stash_block(format!(
                        "<pre dir=\"ltr\">{}</pre>",
                        html::escape_body(inner)
                    ));
                    out.push_str(&m);
                    i = next;
                    continue;
                }
                // <references> before <ref>: "references" has "ref" as a
                // prefix, but match_ext_tag requires a name boundary so the
                // ref matcher never swallows it — order is for clarity only.
                if let Some((attrs, self_closing, past)) = match_ext_tag(s, i, "references") {
                    let next = self.handle_references(&mut out, attrs, self_closing, s, past);
                    i = next;
                    continue;
                }
                if let Some((attrs, self_closing, past)) = match_ext_tag(s, i, "ref") {
                    let next = self.handle_ref(&mut out, attrs, self_closing, s, past);
                    i = next;
                    continue;
                }
                if let Some(after) = match_open(s, i, "gallery") {
                    let (_inner, next) = read_to_close(s, after, "gallery");
                    let m = self.strip.stash_block(
                        "<div class=\"gallery-placeholder\">[gallery]</div>".to_string(),
                    );
                    out.push_str(&m);
                    i = next;
                    continue;
                }
                out.push('<');
                i += 1;
            } else {
                let l = html::utf8_len(b[i]);
                out.push_str(&s[i..i + l]);
                i += l;
            }
        }
        out
    }

    // ---- Cite: <ref> / <references> ----------------------------------

    /// Handle one `<ref …>…</ref>` or `<ref …/>`. Emits a reserved inline
    /// marker (backfilled in [`Ctx::finalize_cite`]) and returns the index
    /// in `s` just past the tag. `attrs`/`s` borrow the strip input; the
    /// name/group are copied out before any `&mut self` rendering.
    fn handle_ref(
        &mut self,
        out: &mut String,
        attrs: &str,
        self_closing: bool,
        s: &str,
        past: usize,
    ) -> usize {
        let (name, group) = ref_attrs(attrs);
        let gidx = self.cite.group_idx(&group);

        // Read the body (open form only) BEFORE touching cite/strip.
        let (raw, next) = if self_closing {
            ("", past)
        } else {
            read_to_close(s, past, "ref")
        };
        let has_body = !raw.trim().is_empty();

        // No name and no content is a hard Cite error — no note created.
        if name.is_empty() && !has_body {
            let m = self
                .strip
                .stash_inline(html::cite_error("<ref> with no content and no name"));
            out.push_str(&m);
            self.misses
                .failed_invokes
                .push("cite: empty <ref>".to_string());
            return next;
        }

        // Render the body through the inline path (escapes any HTML/markup
        // — this is the XSS boundary) before it becomes note content.
        let content = if has_body { Some(self.inline(raw)) } else { None };
        let name_opt = if name.is_empty() {
            None
        } else {
            Some(name.as_str())
        };

        let strip_idx = self.strip.reserve(false);
        let rec = self.cite.record_use(gidx, name_opt, content, strip_idx);
        if rec.redefinition {
            self.misses
                .failed_invokes
                .push(format!("cite: redefinition of ref name \"{name}\""));
        }
        out.push_str(&Strip::marker(strip_idx));
        next
    }

    /// Handle one `<references …/>` or `<references …>…</references>`.
    /// Reserves the group's list marker (first placement wins) and, for the
    /// body form, registers any list-defined references it carries.
    fn handle_references(
        &mut self,
        out: &mut String,
        attrs: &str,
        self_closing: bool,
        s: &str,
        past: usize,
    ) -> usize {
        let (_name, group) = ref_attrs(attrs);
        let gidx = self.cite.group_idx(&group);

        let next = if self_closing {
            past
        } else {
            let (inner, nxt) = read_to_close(s, past, "references");
            self.process_ldr(gidx, inner);
            nxt
        };

        if self.cite.groups[gidx].references_marker.is_none() {
            let idx = self.strip.reserve(true);
            self.cite.groups[gidx].references_marker = Some(idx);
            out.push_str(&Strip::marker(idx));
        } else {
            // A second <references/> for the same group renders nothing.
            self.misses
                .failed_invokes
                .push(format!("cite: duplicate <references/> for group \"{group}\""));
        }
        next
    }

    /// Scan a `<references>` body for `<ref name="x">…</ref>` definitions
    /// (list-defined references) and attach their content to the already-
    /// used named notes. Anonymous or unused entries are inert. Nested
    /// `<ref>` inside a body is not re-scanned (no recursion → no loop).
    fn process_ldr(&mut self, gidx: usize, inner: &str) {
        let b = inner.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'<' {
                if let Some((attrs, self_closing, past)) = match_ext_tag(inner, i, "ref") {
                    let (name, _group) = ref_attrs(attrs);
                    if self_closing {
                        i = past;
                        continue;
                    }
                    let (body, next) = read_to_close(inner, past, "ref");
                    if !name.is_empty() && !body.trim().is_empty() {
                        let rendered = self.inline(body);
                        self.cite.define(gidx, &name, rendered);
                    }
                    i = next;
                    continue;
                }
            }
            i += html::utf8_len(b[i]);
        }
    }

    /// Two-pass finalize (see [`to_html`]): backfill every inline `<sup>`
    /// now that use counts are known, then render each group's `<references>`
    /// list — auto-appending (with a miss) for a group that cited but never
    /// placed a list. Returns the (possibly extended) block text.
    fn finalize_cite(&mut self, mut text: String) -> String {
        let cite = std::mem::take(&mut self.cite);

        for g in &cite.groups {
            for note in &g.notes {
                let multi = note.uses.len() > 1;
                for (u, &strip_idx) in note.uses.iter().enumerate() {
                    self.strip
                        .set(strip_idx, cite::sup_html(&g.name, note.number, u, multi));
                }
            }
        }

        for g in &cite.groups {
            if g.notes.is_empty() {
                // A bare <references/> with no refs: leave its reserved slot
                // empty (renders nothing), matching MediaWiki.
                continue;
            }
            let list = cite::references_list_html(g);
            match g.references_marker {
                Some(idx) => self.strip.set(idx, list),
                None => {
                    self.misses.failed_invokes.push(format!(
                        "cite: <ref> in group \"{}\" with no <references/>",
                        g.name
                    ));
                    let idx = self.strip.reserve(true);
                    self.strip.set(idx, list);
                    text.push('\n');
                    text.push_str(&Strip::marker(idx));
                    text.push('\n');
                }
            }
        }
        text
    }

    // ---- block level -------------------------------------------------

    fn parse_blocks(&mut self, text: &str) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out = String::new();
        let mut para: Vec<String> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let trimmed_start = line.trim_start();

            if let Some(idx) = self.strip.lone_block(line) {
                self.flush_para(&mut para, &mut out);
                let block = self.strip.items[idx].clone();
                out.push_str(&block);
                i += 1;
                continue;
            }

            if line.trim().is_empty() {
                self.flush_para(&mut para, &mut out);
                i += 1;
                continue;
            }

            if trimmed_start.starts_with("{|") {
                self.flush_para(&mut para, &mut out);
                let (html, next) = self.parse_table(&lines, i);
                out.push_str(&html);
                i = next;
                continue;
            }

            if let Some((level, content)) = parse_heading(line) {
                self.flush_para(&mut para, &mut out);
                out.push_str(&self.render_heading(level, content));
                i += 1;
                continue;
            }

            if let Some(rest) = parse_hr(line) {
                self.flush_para(&mut para, &mut out);
                out.push_str("<hr />");
                if !rest.trim().is_empty() {
                    para.push(rest.to_string());
                }
                i += 1;
                continue;
            }

            let first = line.chars().next().unwrap_or(' ');
            if matches!(first, '*' | '#' | ':' | ';') {
                self.flush_para(&mut para, &mut out);
                let mut block: Vec<&str> = Vec::new();
                while i < lines.len() {
                    let c = lines[i].chars().next().unwrap_or(' ');
                    if matches!(c, '*' | '#' | ':' | ';') {
                        block.push(lines[i]);
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push_str(&self.render_list(&block));
                continue;
            }

            if line.starts_with(' ') || line.starts_with('\t') {
                self.flush_para(&mut para, &mut out);
                let mut block: Vec<&str> = Vec::new();
                while i < lines.len()
                    && (lines[i].starts_with(' ') || lines[i].starts_with('\t'))
                    && !lines[i].trim().is_empty()
                {
                    block.push(&lines[i][1..]);
                    i += 1;
                }
                let body: Vec<String> = block.iter().map(|l| self.inline(l)).collect();
                out.push_str(&format!("<pre dir=\"ltr\">{}</pre>", body.join("\n")));
                continue;
            }

            para.push(line.to_string());
            i += 1;
        }
        self.flush_para(&mut para, &mut out);
        out
    }

    fn flush_para(&mut self, para: &mut Vec<String>, out: &mut String) {
        if para.is_empty() {
            return;
        }
        let joined = para.join("\n");
        para.clear();
        let rendered = self.inline(&joined);
        if !rendered.trim().is_empty() {
            out.push_str("<p>");
            out.push_str(&rendered);
            out.push_str("</p>");
        }
    }

    fn render_heading(&mut self, level: usize, content: &str) -> String {
        let anchor = anchor_encode(content);
        let inner = self.inline(content.trim());
        format!(
            "<h{level} id=\"{}\">{}</h{level}>",
            html::escape(&anchor),
            inner
        )
    }

    // ---- lists -------------------------------------------------------

    fn render_list(&mut self, lines: &[&str]) -> String {
        let mut out = String::new();
        let mut stack: Vec<char> = Vec::new();
        for line in lines {
            let (pfx, rest) = split_list_prefix(line);
            let pfx: Vec<char> = pfx.chars().collect();
            let mut common = 0;
            while common < stack.len()
                && common < pfx.len()
                && list_type(stack[common]) == list_type(pfx[common])
            {
                common += 1;
            }
            while stack.len() > common {
                let c = stack.pop().unwrap();
                out.push_str(close_list(c));
            }
            if common == pfx.len() && common > 0 {
                let prev = stack[common - 1];
                let newc = pfx[common - 1];
                out.push_str(&next_item(prev, newc));
                stack[common - 1] = newc;
            } else {
                for &c in &pfx[common..] {
                    out.push_str(open_list(c));
                    stack.push(c);
                }
            }
            let last = *stack.last().unwrap_or(&'*');
            if last == ';' {
                if let Some((term, def)) = split_dt_dd(rest) {
                    out.push_str(&self.inline(term.trim()));
                    out.push_str("</dt><dd>");
                    out.push_str(&self.inline(def.trim()));
                    *stack.last_mut().unwrap() = ':';
                } else {
                    out.push_str(&self.inline(rest.trim()));
                }
            } else {
                out.push_str(&self.inline(rest.trim()));
            }
        }
        while let Some(c) = stack.pop() {
            out.push_str(close_list(c));
        }
        out
    }

    // ---- tables ------------------------------------------------------

    fn parse_table(&mut self, lines: &[&str], start: usize) -> (String, usize) {
        let first = lines[start].trim_start();
        let attrs = first.strip_prefix("{|").unwrap_or("").trim();
        let mut html = format!("<table{}>", html::sanitize_attrs(attrs));

        let mut caption: Option<String> = None;
        let mut rows: Vec<(String, Vec<Cell>)> = Vec::new();
        let mut cur_attrs = String::new();
        let mut cur_cells: Vec<Cell> = Vec::new();
        let mut started = false;
        let mut cur: Option<Cell> = None;

        let mut i = start + 1;
        while i < lines.len() {
            let line = lines[i];
            let t = line.trim_start();
            if t.starts_with("|}") {
                i += 1;
                break;
            } else if t.starts_with("|+") {
                if let Some(c) = cur.take() {
                    cur_cells.push(c);
                }
                let (a, content) = split_cell_attrs(t[2..].trim());
                caption = Some(format!(
                    "<caption{}>{}</caption>",
                    a.map(|x| html::sanitize_attrs(&x)).unwrap_or_default(),
                    self.inline(content.trim())
                ));
                i += 1;
            } else if t.starts_with("|-") {
                if let Some(c) = cur.take() {
                    cur_cells.push(c);
                }
                if started {
                    rows.push((cur_attrs.clone(), std::mem::take(&mut cur_cells)));
                }
                cur_attrs = t[2..].trim().to_string();
                started = true;
                cur_cells = Vec::new();
                i += 1;
            } else if t.starts_with('!') || (t.starts_with('|') && !t.starts_with("||")) {
                if let Some(c) = cur.take() {
                    cur_cells.push(c);
                }
                started = true;
                let header = t.starts_with('!');
                let body = &t[1..];
                let seps: &[&str] = if header { &["!!", "||"] } else { &["||"] };
                let segs = split_multi(body, seps);
                let n = segs.len();
                for (k, seg) in segs.into_iter().enumerate() {
                    let (a, content) = split_cell_attrs(&seg);
                    let cell = Cell {
                        header,
                        attrs: a.map(|x| html::sanitize_attrs(&x)).unwrap_or_default(),
                        content: content.to_string(),
                    };
                    if k + 1 == n {
                        cur = Some(cell);
                    } else {
                        cur_cells.push(cell);
                    }
                }
                i += 1;
            } else if t.starts_with("{|") {
                if self.depth < 24 {
                    self.depth += 1;
                    let (nested, next) = self.parse_table(lines, i);
                    self.depth -= 1;
                    let marker = self.strip.stash_inline(nested);
                    if let Some(c) = cur.as_mut() {
                        c.content.push_str(&marker);
                    } else {
                        cur = Some(Cell {
                            header: false,
                            attrs: String::new(),
                            content: marker,
                        });
                    }
                    i = next;
                } else {
                    i += 1;
                }
            } else {
                if let Some(c) = cur.as_mut() {
                    c.content.push('\n');
                    c.content.push_str(line);
                }
                i += 1;
            }
        }
        if let Some(c) = cur.take() {
            cur_cells.push(c);
        }
        if started || !cur_cells.is_empty() {
            rows.push((cur_attrs, cur_cells));
        }

        if let Some(cap) = caption {
            html.push_str(&cap);
        }
        for (attrs, cells) in rows {
            html.push_str(&format!("<tr{}>", html::sanitize_attrs(&attrs)));
            for cell in cells {
                let tag = if cell.header { "th" } else { "td" };
                let inner = self.inline(cell.content.trim());
                html.push_str(&format!("<{tag}{}>{}</{tag}>", cell.attrs, inner));
            }
            html.push_str("</tr>");
        }
        html.push_str("</table>");
        (html, i)
    }

    // ---- inline pipeline ---------------------------------------------

    /// Process one inline region and resolve it to final HTML. Order:
    /// internal links, external links, bare-URL autolinks, HTML tags
    /// (sanitizer), then quotes — each stashes final HTML behind markers
    /// so the last step (resolve) escapes only genuine body text.
    fn inline(&mut self, s: &str) -> String {
        let s = self.links_internal(s);
        let s = self.links_external(&s);
        let s = self.autolinks(&s);
        let s = self.html_tags(&s);
        let s = self.do_quotes(&s);
        self.strip.resolve(&s)
    }

    fn links_internal(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'[' && b.get(i + 1) == Some(&b'[') {
                if let Some((inner, after, trail)) = find_link(s, i) {
                    let html = self.render_internal_link(inner, trail);
                    out.push_str(&html);
                    i = after;
                    continue;
                }
            }
            let l = html::utf8_len(b[i]);
            out.push_str(&s[i..i + l]);
            i += l;
        }
        out
    }

    fn render_internal_link(&mut self, inner: &str, trail: &str) -> String {
        let parts = split_pipe(inner);
        let target_raw = parts[0].trim().to_string();
        let leading_colon = target_raw.starts_with(':');
        let target = if leading_colon {
            target_raw[1..].trim_start().to_string()
        } else {
            target_raw.clone()
        };

        if !leading_colon {
            if let Some(entry) = self.interwiki_of(&target) {
                return self.render_interwiki(&entry, &target, &parts, trail);
            }
        }

        let (title, frag) = Title::parse_parts(&target, self.site);

        if !leading_colon && title.ns == NS_CATEGORY {
            self.categories.push(title.text.clone());
            // A category emits no link, so any letters find_link took as a
            // "trail" are not a link trail — return them as plain text.
            return trail.to_string();
        }
        if !leading_colon && title.ns == NS_FILE {
            return self.render_image(&title, &parts);
        }

        let label = if parts.len() >= 2 {
            let explicit = parts[1..].join("|");
            if explicit.trim().is_empty() {
                pipe_trick(&title.text)
            } else {
                explicit
            }
        } else {
            display_target(&target)
        };
        self.render_page_link(&title, frag.as_deref(), &label, trail)
    }

    fn render_page_link(
        &mut self,
        title: &Title,
        frag: Option<&str>,
        label: &str,
        trail: &str,
    ) -> String {
        let path = html::encode_path(&title.prefixed(self.site));
        let mut href = format!("{}{}{}", self.opts.link_prefix, path, self.opts.asof_query);
        if let Some(f) = frag {
            href.push('#');
            href.push_str(&html::encode_frag(f));
        }
        let class = if self.store.page_exists(title) {
            String::new()
        } else {
            " class=\"new\"".to_string()
        };
        let open = self
            .strip
            .stash_inline(format!("<a href=\"{}\"{}>", html::escape(&href), class));
        let close = self.strip.stash_inline("</a>".to_string());
        format!("{open}{label}{trail}{close}")
    }

    fn interwiki_of(&self, target: &str) -> Option<InterwikiEntry> {
        let idx = target.find(':')?;
        let prefix = target[..idx].trim();
        if prefix.is_empty() {
            return None;
        }
        // A namespace prefix wins over an interwiki prefix.
        if crate::title::resolve_ns(prefix, self.site).is_some() {
            return None;
        }
        let key = prefix.to_lowercase();
        self.site
            .interwiki
            .get(&key)
            .or_else(|| self.site.interwiki.get(prefix))
            .cloned()
    }

    fn render_interwiki(
        &mut self,
        entry: &InterwikiEntry,
        target: &str,
        parts: &[String],
        trail: &str,
    ) -> String {
        let idx = target.find(':').unwrap();
        let sub = &target[idx + 1..];
        let href = entry.url.replace("$1", &html::encode_path(sub));
        let class = if entry.local_instance.is_some() {
            "extiw"
        } else {
            "external extiw"
        };
        let label = if parts.len() >= 2 && !parts[1].trim().is_empty() {
            parts[1..].join("|")
        } else {
            display_target(target)
        };
        let open = self.strip.stash_inline(format!(
            "<a href=\"{}\" class=\"{}\">",
            html::escape(&href),
            class
        ));
        let close = self.strip.stash_inline("</a>".to_string());
        format!("{open}{label}{trail}{close}")
    }

    fn render_image(&mut self, title: &Title, parts: &[String]) -> String {
        let mut width: Option<u32> = None;
        let mut format = "";
        let mut align = "";
        let mut alt: Option<String> = None;
        let mut caption: Option<String> = None;
        for p in &parts[1..] {
            let pt = p.trim();
            let low = pt.to_lowercase();
            match low.as_str() {
                "thumb" | "thumbnail" => format = "thumb",
                "frame" | "framed" => format = "frame",
                "frameless" => format = "frameless",
                "border" => {}
                "left" => align = "left",
                "right" => align = "right",
                "center" | "centre" => align = "center",
                "none" => align = "none",
                _ => {
                    if let Some(px) = parse_px(pt) {
                        width = Some(px);
                    } else if low.starts_with("alt=") {
                        alt = Some(pt[4..].to_string());
                    } else if low.ends_with("px")
                        || low.starts_with("link=")
                        || low.starts_with("upright")
                        || low.starts_with("page=")
                        || low.starts_with("class=")
                        || low.starts_with("lang=")
                    {
                        // Recognized-but-unhandled keyword (incl. height-only
                        // `xNNpx` sizing): not a caption.
                    } else {
                        caption = Some(p.to_string());
                    }
                }
            }
        }
        let req_width = width.or(if format == "thumb" || format == "frameless" {
            Some(220)
        } else {
            None
        });
        let src = self.opts.media.and_then(|m| m.image_url(title, req_width));

        let alt_text = alt
            .clone()
            .or_else(|| caption.clone())
            .unwrap_or_else(|| title.text.clone());
        let cap_html = caption.as_deref().map(|c| self.inline(c));
        let name = title.prefixed(self.site);

        let is_boxed = format == "thumb" || format == "frame";
        let align_class = match align {
            "left" => "tleft",
            "center" | "none" => "tnone",
            _ => "tright",
        };

        let inner_media = match &src {
            Some(url) => {
                let w = width.map(|n| format!(" width=\"{n}\"")).unwrap_or_default();
                format!(
                    "<img src=\"{}\" alt=\"{}\"{}/>",
                    html::escape(url),
                    html::escape(&alt_text),
                    w
                )
            }
            None => {
                self.misses.missing_media.push(name.clone());
                format!(
                    "<span class=\"image-placeholder\">[File: {}]</span>",
                    html::escape(&title.text)
                )
            }
        };

        let final_html = if is_boxed {
            let cap = cap_html.unwrap_or_default();
            format!(
                "<div class=\"thumb {align_class}\"><div class=\"thumbinner\">{inner_media}<div class=\"thumbcaption\">{cap}</div></div></div>"
            )
        } else if !align.is_empty() {
            format!("<span class=\"float{align}\">{inner_media}</span>")
        } else {
            inner_media
        };
        self.strip.stash_inline(final_html)
    }

    // ---- external links & autolinks ----------------------------------

    fn links_external(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'[' && b.get(i + 1) != Some(&b'[') {
                if let Some(close) = find_byte(s, i + 1, b']') {
                    let inner = &s[i + 1..close];
                    if let Some(html) = self.try_external(inner) {
                        out.push_str(&html);
                        i = close + 1;
                        continue;
                    }
                }
            }
            let l = html::utf8_len(b[i]);
            out.push_str(&s[i..i + l]);
            i += l;
        }
        out
    }

    fn try_external(&mut self, inner: &str) -> Option<String> {
        let inner = inner.trim_start();
        if !has_url_scheme(inner) {
            return None;
        }
        let (url, label) = match inner.find(char::is_whitespace) {
            Some(sp) => (&inner[..sp], inner[sp..].trim_start()),
            None => (inner, ""),
        };
        let open = self.strip.stash_inline(format!(
            "<a href=\"{}\" class=\"external {}\">",
            html::escape(url),
            if label.is_empty() { "autonumber" } else { "text" }
        ));
        let close = self.strip.stash_inline("</a>".to_string());
        if label.is_empty() {
            self.ext_counter += 1;
            Some(format!("{open}[{}]{close}", self.ext_counter))
        } else {
            Some(format!("{open}{label}{close}"))
        }
    }

    fn autolinks(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'h' || b[i] == b'f' {
                if let Some(scheme_len) = autolink_scheme(&s[i..]) {
                    let before_ok = i == 0 || {
                        let pc = out.chars().last().unwrap_or(' ');
                        !pc.is_alphanumeric()
                    };
                    if before_ok {
                        let end = consume_url(s, i + scheme_len);
                        let url = &s[i..end];
                        let open = self.strip.stash_inline(format!(
                            "<a href=\"{}\" class=\"external free\">",
                            html::escape(url)
                        ));
                        let close = self.strip.stash_inline("</a>".to_string());
                        out.push_str(&format!("{open}{url}{close}"));
                        i = end;
                        continue;
                    }
                }
            }
            let l = html::utf8_len(b[i]);
            out.push_str(&s[i..i + l]);
            i += l;
        }
        out
    }

    // ---- HTML-in-wikitext sanitizer ----------------------------------

    fn html_tags(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'<' {
                if let Some((name, close, selfclose, attrs, end)) = parse_tag(s, i) {
                    let lname = name.to_lowercase();
                    if html::tag_allowed(&lname) {
                        let html = self.emit_tag(&lname, close, selfclose, attrs);
                        out.push_str(&html);
                        i = end;
                        continue;
                    } else {
                        // Unknown/disallowed: count once, leave the raw
                        // '<' to be escaped at resolve time.
                        self.misses.unknown_tags.push(lname);
                        out.push('<');
                        i += 1;
                        continue;
                    }
                }
                out.push('<');
                i += 1;
            } else {
                let l = html::utf8_len(b[i]);
                out.push_str(&s[i..i + l]);
                i += l;
            }
        }
        out
    }

    fn emit_tag(&mut self, name: &str, close: bool, selfclose: bool, attrs: &str) -> String {
        // poem is remapped to a plain div (no HTML5 <poem> element).
        let out_name = if name == "poem" { "div" } else { name };
        let extra = if name == "poem" && !close {
            " class=\"poem\""
        } else {
            ""
        };
        let html = if html::tag_void(name) {
            format!("<{out_name}{} />", html::sanitize_attrs(attrs))
        } else if close {
            format!("</{out_name}>")
        } else if selfclose {
            format!("<{out_name}{}{} />", extra, html::sanitize_attrs(attrs))
        } else {
            format!("<{out_name}{}{}>", extra, html::sanitize_attrs(attrs))
        };
        self.strip.stash_inline(html)
    }

    // ---- quotes (MediaWiki doQuotes) ---------------------------------

    /// Faithful port of MediaWiki `Parser::doQuotes` apostrophe balancing:
    /// the 4-apostrophe and >5-apostrophe reinterpretation, the
    /// odd-bold/odd-italic single-letter-word fixup, and the stateful tag
    /// emission. Emitted `<i>`/`<b>` tags are stashed as markers so the
    /// resolve pass does not re-escape them.
    fn do_quotes(&mut self, text: &str) -> String {
        let mut arr = split_apostrophe_runs(text);
        if arr.len() == 1 {
            return text.to_string();
        }
        let mut numbold = 0;
        let mut numitalics = 0;
        let mut i = 1;
        while i < arr.len() {
            let mut len = arr[i].len();
            if len == 4 {
                arr[i - 1].push('\'');
                arr[i] = "'''".to_string();
                len = 3;
            } else if len > 5 {
                let extra = len - 5;
                arr[i - 1].push_str(&"'".repeat(extra));
                arr[i] = "'''''".to_string();
                len = 5;
            }
            match len {
                2 => numitalics += 1,
                3 => numbold += 1,
                5 => {
                    numitalics += 1;
                    numbold += 1;
                }
                _ => {}
            }
            i += 2;
        }

        if numbold % 2 == 1 && numitalics % 2 == 1 {
            let mut first_single: i64 = -1;
            let mut first_multi: i64 = -1;
            let mut first_space: i64 = -1;
            let mut j = 1;
            while j < arr.len() {
                if arr[j].len() == 3 {
                    let prev = &arr[j - 1];
                    let x1 = prev.chars().last();
                    let x2 = {
                        let mut it = prev.chars().rev();
                        it.next();
                        it.next()
                    };
                    if x1 == Some(' ') {
                        if first_space == -1 {
                            first_space = j as i64;
                        }
                    } else if x2 == Some(' ') {
                        first_single = j as i64;
                        break;
                    } else if first_multi == -1 {
                        first_multi = j as i64;
                    }
                }
                j += 2;
            }
            let pick = if first_single > -1 {
                first_single
            } else if first_multi > -1 {
                first_multi
            } else {
                first_space
            };
            if pick > -1 {
                let p = pick as usize;
                arr[p] = "''".to_string();
                arr[p - 1].push('\'');
            }
        }

        let mut output = String::new();
        let mut buffer = String::new();
        let mut state = "";
        for idx in 0..arr.len() {
            if idx % 2 == 0 {
                if state == "both" {
                    buffer.push_str(&arr[idx]);
                } else {
                    output.push_str(&arr[idx]);
                }
                continue;
            }
            match arr[idx].len() {
                2 => match state {
                    "i" => {
                        output.push_str(&self.q("</i>"));
                        state = "";
                    }
                    "bi" => {
                        output.push_str(&self.q("</i>"));
                        state = "b";
                    }
                    "ib" => {
                        output.push_str(&self.q("</b></i><b>"));
                        state = "b";
                    }
                    "both" => {
                        output.push_str(&self.q("<b><i>"));
                        output.push_str(&buffer);
                        output.push_str(&self.q("</i>"));
                        buffer.clear();
                        state = "b";
                    }
                    "b" => {
                        output.push_str(&self.q("<i>"));
                        state = "bi";
                    }
                    _ => {
                        output.push_str(&self.q("<i>"));
                        state = "i";
                    }
                },
                3 => match state {
                    "b" => {
                        output.push_str(&self.q("</b>"));
                        state = "";
                    }
                    "bi" => {
                        output.push_str(&self.q("</b></i><i>"));
                        state = "i";
                    }
                    "ib" => {
                        output.push_str(&self.q("</b>"));
                        state = "i";
                    }
                    "both" => {
                        output.push_str(&self.q("<i><b>"));
                        output.push_str(&buffer);
                        output.push_str(&self.q("</b>"));
                        buffer.clear();
                        state = "i";
                    }
                    "i" => {
                        output.push_str(&self.q("<b>"));
                        state = "ib";
                    }
                    _ => {
                        output.push_str(&self.q("<b>"));
                        state = "b";
                    }
                },
                5 => match state {
                    "b" => {
                        output.push_str(&self.q("</b><i>"));
                        state = "i";
                    }
                    "i" => {
                        output.push_str(&self.q("</i><b>"));
                        state = "b";
                    }
                    "bi" => {
                        output.push_str(&self.q("</i></b>"));
                        state = "";
                    }
                    "ib" => {
                        output.push_str(&self.q("</b></i>"));
                        state = "";
                    }
                    "both" => {
                        output.push_str(&self.q("<i><b>"));
                        output.push_str(&buffer);
                        output.push_str(&self.q("</b></i>"));
                        buffer.clear();
                        state = "";
                    }
                    _ => {
                        buffer.clear();
                        state = "both";
                    }
                },
                _ => {}
            }
        }
        if state == "b" || state == "ib" {
            output.push_str(&self.q("</b>"));
        }
        if state == "i" || state == "bi" || state == "ib" {
            output.push_str(&self.q("</i>"));
        }
        if state == "bi" {
            output.push_str(&self.q("</b>"));
        }
        if state == "both" && !buffer.is_empty() {
            output.push_str(&self.q("<b><i>"));
            output.push_str(&buffer);
            output.push_str(&self.q("</i></b>"));
        }
        output
    }

    /// Stash a literal formatting-tag string as a marker.
    fn q(&mut self, tag: &str) -> String {
        self.strip.stash_inline(tag.to_string())
    }
}

struct Cell {
    header: bool,
    attrs: String,
    content: String,
}

// ---- free helpers ----------------------------------------------------

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_end();
    if !t.starts_with('=') {
        return None;
    }
    let a = t.chars().take_while(|&c| c == '=').count();
    let b = t.chars().rev().take_while(|&c| c == '=').count();
    let n = a.min(b);
    if n == 0 || t.len() < 2 * n + 1 {
        return None;
    }
    let level = n.min(6);
    let content = &t[level..t.len() - level];
    Some((level, content))
}

fn parse_hr(line: &str) -> Option<&str> {
    let dashes = line.chars().take_while(|&c| c == '-').count();
    if dashes >= 4 {
        Some(&line[dashes..])
    } else {
        None
    }
}

/// Anchor id from heading source: trimmed, spaces → `_`.
fn anchor_encode(s: &str) -> String {
    s.trim().replace(' ', "_")
}

fn list_type(c: char) -> u8 {
    match c {
        '*' => 0,
        '#' => 1,
        ':' | ';' => 2,
        _ => 3,
    }
}

fn open_list(c: char) -> &'static str {
    match c {
        '*' => "<ul><li>",
        '#' => "<ol><li>",
        ';' => "<dl><dt>",
        ':' => "<dl><dd>",
        _ => "",
    }
}

fn close_list(c: char) -> &'static str {
    match c {
        '*' => "</li></ul>",
        '#' => "</li></ol>",
        ';' => "</dt></dl>",
        ':' => "</dd></dl>",
        _ => "",
    }
}

fn next_item(prev: char, newc: char) -> String {
    match list_type(newc) {
        0 | 1 => "</li><li>".to_string(),
        2 => {
            let close = if prev == ';' { "</dt>" } else { "</dd>" };
            let open = if newc == ';' { "<dt>" } else { "<dd>" };
            format!("{close}{open}")
        }
        _ => String::new(),
    }
}

fn split_list_prefix(line: &str) -> (&str, &str) {
    let end = line
        .char_indices()
        .take_while(|&(_, c)| matches!(c, '*' | '#' | ':' | ';'))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    (&line[..end], &line[end..])
}

/// First top-level `:` (not inside `[[…]]`/`[…]`/`{{…}}`) splits a `;`
/// definition line into term and definition.
fn split_dt_dd(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let mut depth_sq = 0i32;
    let mut depth_cu = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' => depth_sq += 1,
            b']' => depth_sq -= 1,
            b'{' => depth_cu += 1,
            b'}' => depth_cu -= 1,
            b':' if depth_sq <= 0 && depth_cu <= 0 => {
                return Some((&s[..i], &s[i + 1..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Locate the matching `]]` for a `[[` at `open`, counting nested `[[`.
/// Returns (inner, index-after-`]]`-and-trail, trail-letters).
fn find_link(s: &str, open: usize) -> Option<(&str, usize, &str)> {
    let b = s.as_bytes();
    let mut depth = 1;
    let mut i = open + 2;
    while i + 1 < b.len() {
        if b[i] == b'[' && b[i + 1] == b'[' {
            depth += 1;
            i += 2;
        } else if b[i] == b']' && b[i + 1] == b']' {
            depth -= 1;
            if depth == 0 {
                let inner = &s[open + 2..i];
                let after = i + 2;
                let trail_end = after
                    + s[after..]
                        .bytes()
                        .take_while(|c| c.is_ascii_lowercase())
                        .count();
                return Some((inner, trail_end, &s[after..trail_end]));
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

/// Split link inner on top-level `|` (respecting nested `[[…]]`/`{{…}}`).
fn split_pipe(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth_sq = 0i32;
    let mut depth_cu = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' => depth_sq += 1,
            b']' => depth_sq -= 1,
            b'{' => depth_cu += 1,
            b'}' => depth_cu -= 1,
            b'|' if depth_sq <= 0 && depth_cu <= 0 => {
                parts.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(s[start..].to_string());
    parts
}

fn display_target(target: &str) -> String {
    target.trim().replace('_', " ")
}

/// Pipe trick: from a namespace-stripped page name, drop a trailing
/// parenthetical `(disambig)` or everything after the first comma.
fn pipe_trick(page: &str) -> String {
    let s = page.trim();
    if let Some(open) = s.rfind('(') {
        if s.ends_with(')') {
            return s[..open].trim().to_string();
        }
    }
    if let Some(comma) = s.find(',') {
        return s[..comma].trim().to_string();
    }
    s.to_string()
}

fn find_byte(s: &str, from: usize, target: u8) -> Option<usize> {
    s.as_bytes()[from..]
        .iter()
        .position(|&c| c == target)
        .map(|p| p + from)
}

fn has_url_scheme(s: &str) -> bool {
    let low = s.to_lowercase();
    low.starts_with("http://")
        || low.starts_with("https://")
        || low.starts_with("ftp://")
        || low.starts_with("ftps://")
        || low.starts_with("//")
        || low.starts_with("mailto:")
        || low.starts_with("irc://")
        || low.starts_with("news:")
        || low.starts_with("gopher://")
}

fn autolink_scheme(s: &str) -> Option<usize> {
    let low = s.to_lowercase();
    for sc in ["https://", "http://", "ftps://", "ftp://"] {
        if low.starts_with(sc) {
            return Some(sc.len());
        }
    }
    None
}

/// Consume URL body characters, then trim trailing punctuation and an
/// unbalanced closing paren (MediaWiki free-link boundary heuristic).
fn consume_url(s: &str, from: usize) -> usize {
    let b = s.as_bytes();
    let mut i = from;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace()
            || matches!(c, b'<' | b'>' | b'[' | b']' | b'"' | b'{' | b'}' | b'|' | 0x7f)
        {
            break;
        }
        i += 1;
    }
    while i > from {
        let c = b[i - 1];
        if matches!(c, b'.' | b',' | b';' | b':' | b'!' | b'?') {
            i -= 1;
        } else if c == b')' {
            let opens = s[from..i].bytes().filter(|&x| x == b'(').count();
            let closes = s[from..i].bytes().filter(|&x| x == b')').count();
            if closes > opens {
                i -= 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    i
}

/// Parse a size keyword to a pixel width: `250px` → 250. `x120px`
/// (height-only) yields no width; `250x120px` uses the width part.
fn parse_px(s: &str) -> Option<u32> {
    let low = s.trim().to_lowercase();
    let core = low.strip_suffix("px")?;
    if core.starts_with('x') {
        return None;
    }
    let width_part = core.split('x').next().unwrap_or(&core);
    width_part.parse::<u32>().ok()
}

/// Split text into [text, run, text, run, …] where runs are 2+
/// apostrophes (MediaWiki `preg_split("/(''+)/")`).
fn split_apostrophe_runs(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            let mut j = i;
            while j < b.len() && b[j] == b'\'' {
                j += 1;
            }
            let run = j - i;
            if run >= 2 {
                parts.push(std::mem::take(&mut cur));
                parts.push("'".repeat(run));
                i = j;
                continue;
            } else {
                cur.push('\'');
                i += 1;
            }
        } else {
            let l = html::utf8_len(b[i]);
            cur.push_str(&s[i..i + l]);
            i += l;
        }
    }
    parts.push(cur);
    parts
}

/// Match an opening tag `<name` at `pos` (case-insensitive, name followed
/// by whitespace/`>`/`/`). Returns the byte index just past the `>`. Not a
/// match for a self-closing `<name … />`.
fn match_open(s: &str, pos: usize, name: &str) -> Option<usize> {
    let rest = &s[pos..];
    if !rest.starts_with('<') {
        return None;
    }
    let after_lt = &rest[1..];
    // Byte compare, not `after_lt[..name.len()]`: `name` is ASCII but
    // `after_lt` can have a multibyte char straddling that offset, and a
    // str-slice on a non-char-boundary panics — aborting the whole render
    // on any `<` near a non-ASCII char. `get` + byte compare is
    // boundary-safe.
    match after_lt.as_bytes().get(..name.len()) {
        Some(head) if head.eq_ignore_ascii_case(name.as_bytes()) => {}
        _ => return None,
    }
    match after_lt.as_bytes().get(name.len()).copied() {
        Some(c) if c.is_ascii_whitespace() || c == b'>' || c == b'/' => {}
        _ => return None,
    }
    let gt = rest.find('>')?;
    if rest.as_bytes()[gt - 1] == b'/' {
        return None;
    }
    Some(pos + gt + 1)
}

/// Match an extension tag `<name …>` or `<name …/>` at `pos`, returning
/// (raw attributes, is_self_closing, index-just-past-`>`). The `>` scan is
/// quote-aware so a `>` inside an attribute value does not end the tag.
/// Requires a name boundary (whitespace/`>`/`/`) so `<references>` is not
/// matched as `<ref>`. Unterminated (`<ref` with no `>`) yields None.
fn match_ext_tag<'b>(s: &'b str, pos: usize, name: &str) -> Option<(&'b str, bool, usize)> {
    let rest = &s[pos..];
    if !rest.starts_with('<') {
        return None;
    }
    let after_lt = &rest[1..];
    // Boundary-safe byte compare (see match_open): a str-slice at
    // `name.len()` panics when a multibyte char straddles the offset.
    match after_lt.as_bytes().get(..name.len()) {
        Some(head) if head.eq_ignore_ascii_case(name.as_bytes()) => {}
        _ => return None,
    }
    match after_lt.as_bytes().get(name.len()).copied() {
        Some(c) if c.is_ascii_whitespace() || c == b'/' || c == b'>' => {}
        _ => return None,
    }
    let attrs_start = pos + 1 + name.len();
    let b = s.as_bytes();
    let mut j = attrs_start;
    let mut quote = 0u8;
    while j < b.len() {
        let c = b[j];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            break;
        }
        j += 1;
    }
    if j >= b.len() {
        return None; // unterminated
    }
    let self_closing = j > attrs_start && b[j - 1] == b'/';
    let attrs_end = if self_closing { j - 1 } else { j };
    Some((&s[attrs_start..attrs_end], self_closing, j + 1))
}

/// Extract the (name, group) of a `<ref>`/`<references>` tag, trimmed.
/// Empty string = absent. Other attributes are ignored.
fn ref_attrs(attrs: &str) -> (String, String) {
    let mut name = String::new();
    let mut group = String::new();
    for (k, v) in html::parse_attrs(attrs) {
        match k.to_ascii_lowercase().as_str() {
            "name" => name = v,
            "group" => group = v,
            _ => {}
        }
    }
    (name.trim().to_string(), group.trim().to_string())
}

/// From `after` (just past an opening tag), read to the matching
/// `</name>` (case-insensitive). Returns (inner, index-past-close); with
/// no close, consumes to end of string.
fn read_to_close<'b>(s: &'b str, after: usize, name: &str) -> (&'b str, usize) {
    // `</` + an ASCII tag name; search bytes case-insensitively. Indexing
    // `s` with a `to_lowercase()` offset was wrong whenever the body held
    // a case-length-changing char (İ→i̇, ẞ→ß, …): the offset no longer
    // aligned with the original bytes, corrupting the slice or panicking.
    // `rel` here indexes the start of `</` (ASCII), so `after + rel` is a
    // valid char boundary.
    let needle = format!("</{}", name);
    let nb = needle.as_bytes();
    let hay = s[after..].as_bytes();
    let rel = if hay.len() >= nb.len() {
        (0..=hay.len() - nb.len()).find(|&i| hay[i..i + nb.len()].eq_ignore_ascii_case(nb))
    } else {
        None
    };
    if let Some(rel) = rel {
        let inner = &s[after..after + rel];
        let close_gt = s[after + rel..].find('>').map(|g| after + rel + g + 1);
        (inner, close_gt.unwrap_or(s.len()))
    } else {
        (&s[after..], s.len())
    }
}

/// Parse an HTML tag at `pos` (`b[pos]=='<'`). Returns
/// (name, is_close, is_selfclose, attrs, index-past-`>`). None if it does
/// not look like a tag (a bare `<`).
fn parse_tag(s: &str, pos: usize) -> Option<(String, bool, bool, &str, usize)> {
    let b = s.as_bytes();
    if b.get(pos) != Some(&b'<') {
        return None;
    }
    let mut i = pos + 1;
    let close = b.get(i) == Some(&b'/');
    if close {
        i += 1;
    }
    let name_start = i;
    if !b.get(i).map_or(false, |c| c.is_ascii_alphabetic()) {
        return None;
    }
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-') {
        i += 1;
    }
    let name = s[name_start..i].to_string();
    let mut j = i;
    let mut quote = 0u8;
    while j < b.len() {
        let c = b[j];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            break;
        }
        j += 1;
    }
    if j >= b.len() {
        return None;
    }
    let mut attr_end = j;
    let selfclose = attr_end > i && b[attr_end - 1] == b'/';
    if selfclose {
        attr_end -= 1;
    }
    let attrs = &s[i..attr_end];
    Some((name, close, selfclose, attrs, j + 1))
}

/// Split `s` on any of the top-level separators in `seps` (respecting
/// `[[…]]`/`{{…}}` depth). Used for inline `||`/`!!` cell separation.
fn split_multi(s: &str, seps: &[&str]) -> Vec<String> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth_sq = 0i32;
    let mut depth_cu = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' => {
                depth_sq += 1;
                i += 1;
                continue;
            }
            b']' => {
                depth_sq -= 1;
                i += 1;
                continue;
            }
            b'{' => {
                depth_cu += 1;
                i += 1;
                continue;
            }
            b'}' => {
                depth_cu -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth_sq <= 0 && depth_cu <= 0 {
            let mut matched = None;
            for sep in seps {
                if s[i..].starts_with(sep) {
                    matched = Some(sep.len());
                    break;
                }
            }
            if let Some(l) = matched {
                parts.push(s[start..i].to_string());
                i += l;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    parts.push(s[start..].to_string());
    parts
}

/// Split a table cell into (optional attributes, content). Only splits
/// when the part before the first top-level single `|` looks like real
/// attributes (`=` present, no wiki-markup openers).
fn split_cell_attrs(s: &str) -> (Option<String>, &str) {
    let b = s.as_bytes();
    let mut depth_sq = 0i32;
    let mut depth_cu = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' => depth_sq += 1,
            b']' => depth_sq -= 1,
            b'{' => depth_cu += 1,
            b'}' => depth_cu -= 1,
            b'|' if depth_sq <= 0 && depth_cu <= 0 => {
                if b.get(i + 1) == Some(&b'|') {
                    return (None, s);
                }
                let left = &s[..i];
                if left.contains('=')
                    && !left.contains('[')
                    && !left.contains('{')
                    && !left.contains('<')
                {
                    return (Some(left.trim().to_string()), &s[i + 1..]);
                }
                return (None, s);
            }
            _ => {}
        }
        i += 1;
    }
    (None, s)
}
