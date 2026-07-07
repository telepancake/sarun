//! HTML output helpers: escaping, the sanitizer allowlist for
//! HTML-in-wikitext, inline error boxes, RTL/dir attributes.
//! OWNED BY: the parser-core agent.
//!
//! Two escapes coexist on purpose. `escape` is total (`&`→`&amp;`
//! unconditionally) and is for attribute values and known-literal text
//! (nowiki/pre bodies). `escape_body` preserves already-valid HTML
//! entities (`&nbsp;`, `&#39;`) so body text passes them through the way
//! MediaWiki does, escaping only bare `&`.

/// Total escape: every `&` becomes `&amp;`. For attribute values and
/// literal blocks where entities must NOT be interpreted.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Body-text escape: leaves a valid entity reference (`&name;`,
/// `&#123;`, `&#x1F;`) intact, escapes every other `&`. Does not touch
/// `"` (legal in body text).
pub fn escape_body(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => {
                out.push_str("&lt;");
                i += 1;
            }
            b'>' => {
                out.push_str("&gt;");
                i += 1;
            }
            b'&' => {
                if let Some(len) = entity_len(&s[i..]) {
                    out.push_str(&s[i..i + len]);
                    i += len;
                } else {
                    out.push_str("&amp;");
                    i += 1;
                }
            }
            _ => {
                let l = utf8_len(b[i]);
                out.push_str(&s[i..i + l]);
                i += l;
            }
        }
    }
    out
}

/// Attribute-value escape: like [`escape_body`] (valid entities kept)
/// but also escapes `"` and `'`. Matches MediaWiki's decode-then-encode
/// result for both bare `&` and pre-existing `&amp;`.
pub fn escape_attr(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => {
                out.push_str("&lt;");
                i += 1;
            }
            b'>' => {
                out.push_str("&gt;");
                i += 1;
            }
            b'"' => {
                out.push_str("&quot;");
                i += 1;
            }
            b'\'' => {
                out.push_str("&#039;");
                i += 1;
            }
            b'&' => {
                if let Some(len) = entity_len(&s[i..]) {
                    out.push_str(&s[i..i + len]);
                    i += len;
                } else {
                    out.push_str("&amp;");
                    i += 1;
                }
            }
            _ => {
                let l = utf8_len(b[i]);
                out.push_str(&s[i..i + l]);
                i += l;
            }
        }
    }
    out
}

/// Length in bytes of a valid entity reference starting at `s[0]=='&'`,
/// including the trailing `;`, or None.
fn entity_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.first() != Some(&b'&') {
        return None;
    }
    let mut i = 1;
    if b.get(i) == Some(&b'#') {
        i += 1;
        let hex = b.get(i) == Some(&b'x') || b.get(i) == Some(&b'X');
        if hex {
            i += 1;
        }
        let start = i;
        while i < b.len()
            && ((hex && b[i].is_ascii_hexdigit()) || (!hex && b[i].is_ascii_digit()))
        {
            i += 1;
        }
        if i == start || b.get(i) != Some(&b';') {
            return None;
        }
        return Some(i + 1);
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == start || b.get(i) != Some(&b';') {
        return None;
    }
    Some(i + 1)
}

pub(crate) fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// MediaWiki-style inline error box (script errors, failed invokes).
pub fn error_box(msg: &str) -> String {
    format!(r#"<span class="error">{}</span>"#, escape(msg))
}

// ---- Cite (<ref>/<references>) presentation ---------------------------
//
// All ids/hrefs/labels these take are RAW strings, escaped here (single
// choke point); `content`/`backlinks` are already-final HTML from the
// parser and pass through verbatim.

/// Inline citation marker: `<sup class="reference" id=…><a href=…>[n]</a></sup>`.
pub fn ref_marker(ref_id: &str, note_id: &str, label: &str) -> String {
    format!(
        "<sup class=\"reference\" id=\"{}\"><a href=\"#{}\">{}</a></sup>",
        escape(ref_id),
        escape(note_id),
        escape(label)
    )
}

/// Inline Cite error (empty ref, malformed tag, undefined named ref).
pub fn cite_error(msg: &str) -> String {
    format!(
        "<span class=\"error mw-ext-cite-error\">Cite error: {}</span>",
        escape(msg)
    )
}

/// Single-use back-link: `^` pointing at the sole use.
pub fn ref_backlink_single(ref_id: &str) -> String {
    format!(
        "<span class=\"mw-cite-backlink\"><a href=\"#{}\">^</a></span>",
        escape(ref_id)
    )
}

/// Multi-use back-links: `^ a b c …`, one anchor per (ref_id, label) use.
pub fn ref_backlink_multi(entries: &[(String, String)]) -> String {
    let mut s = String::from("<span class=\"mw-cite-backlink\">^");
    for (rid, label) in entries {
        s.push_str(&format!(" <a href=\"#{}\">{}</a>", escape(rid), escape(label)));
    }
    s.push_str("</span>");
    s
}

/// One `<li>` in the references list: back-link(s) then the note text.
pub fn reference_item(note_id: &str, backlinks: &str, content: &str) -> String {
    format!(
        "<li id=\"{}\">{} <span class=\"reference-text\">{}</span></li>",
        escape(note_id),
        backlinks,
        content
    )
}

/// Wrap rendered `<li>` items in the ordered references list.
pub fn references_wrap(items: &str) -> String {
    format!("<ol class=\"references\">{}</ol>", items)
}

// ---- URL/anchor encoding ---------------------------------------------

/// Percent-encode a title for use as a link path. Spaces → `_`, then the
/// characters MediaWiki keeps in a pretty path pass through and the rest
/// (`? # % [ ] { } | " < > & = +`, space, non-ASCII) become `%XX`.
pub fn encode_path(s: &str) -> String {
    let underscored = s.replace(' ', "_");
    let mut out = String::with_capacity(underscored.len());
    for &byte in underscored.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'(' | b')' | b',' | b'!' | b'*' | b'\'' | b';' | b'@' | b'~')
        {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", byte));
        }
    }
    out
}

/// Fragment (`#…`) encoding: spaces → `_`, minimal percent-encoding for
/// characters unsafe in an id reference.
pub fn encode_frag(s: &str) -> String {
    let underscored = s.replace(' ', "_");
    let mut out = String::with_capacity(underscored.len());
    for &byte in underscored.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b':' | b'(' | b')' | b',' | b'!' | b'*' | b'\'' | b';' | b'@')
        {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", byte));
        }
    }
    out
}

// ---- HTML-in-wikitext sanitizer --------------------------------------

/// Tags the sanitizer permits (rendered) vs. everything else (escaped and
/// counted). `poem` is remapped to a `div`; `img` is deliberately absent
/// (only the File: pipeline emits images, never raw wikitext).
pub(crate) fn tag_allowed(name: &str) -> bool {
    matches!(
        name,
        "div" | "span" | "sup" | "sub" | "small" | "big" | "center" | "blockquote"
            | "table" | "tr" | "td" | "th" | "caption" | "code" | "pre" | "tt" | "b"
            | "i" | "u" | "s" | "strike" | "em" | "strong" | "abbr" | "font" | "hr"
            | "br" | "wbr" | "dl" | "dt" | "dd" | "ul" | "ol" | "li" | "h1" | "h2"
            | "h3" | "h4" | "h5" | "h6" | "p" | "poem"
    )
}

/// Void tags: emitted self-closed regardless of the source form.
pub(crate) fn tag_void(name: &str) -> bool {
    matches!(name, "br" | "hr" | "wbr")
}

fn attr_allowed(name: &str) -> bool {
    matches!(
        name,
        "class" | "id" | "style" | "title" | "dir" | "lang" | "colspan" | "rowspan"
            | "align" | "valign" | "width" | "height"
    )
}

/// Sanitize a raw attribute string (the text between the tag name and the
/// closing `>`). Returns allowlisted attributes serialized with a leading
/// space each, e.g. ` class="x" id="y"`. Disallowed attributes are
/// dropped; `style` values are scrubbed of `expression`/`url`/`javascript`.
pub fn sanitize_attrs(raw: &str) -> String {
    let mut out = String::new();
    for (name, value) in parse_attrs(raw) {
        let name = name.to_ascii_lowercase();
        if !attr_allowed(&name) {
            continue;
        }
        let value = if name == "style" {
            sanitize_style(&value)
        } else {
            value
        };
        out.push(' ');
        out.push_str(&name);
        out.push_str("=\"");
        out.push_str(&escape_attr(&value));
        out.push('"');
    }
    out
}

/// Drop any CSS declaration whose text contains a scripting/expression
/// vector (`expression`, `javascript:`, `url(`). Blunt on purpose (plan
/// §3.1 sanitizer): a dropped declaration is safer than a clever one.
pub fn sanitize_style(value: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for decl in value.split(';') {
        let low: String = decl
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if low.contains("expression")
            || low.contains("javascript:")
            || low.contains("url(")
            || low.contains("behavior:")
            || low.contains("-moz-binding")
        {
            continue;
        }
        if !decl.trim().is_empty() {
            kept.push(decl.trim());
        }
    }
    kept.join("; ")
}

/// Parse attributes into (name, value) pairs. Handles double/single
/// quoted and bare values; tolerates junk between attributes.
pub(crate) fn parse_attrs(raw: &str) -> Vec<(String, String)> {
    let b = raw.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len()
            && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'-' | b'_' | b':'))
        {
            i += 1;
        }
        if i == start {
            // Not an attribute-name character; skip it and continue.
            i += 1;
            continue;
        }
        let name = raw[start..i].to_string();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            out.push((name, String::new()));
            continue;
        }
        i += 1; // consume '='
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
            let q = b[i];
            i += 1;
            let vstart = i;
            while i < b.len() && b[i] != q {
                i += 1;
            }
            let v = raw[vstart..i.min(raw.len())].to_string();
            if i < b.len() {
                i += 1; // closing quote
            }
            v
        } else {
            let vstart = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                i += 1;
            }
            raw[vstart..i].to_string()
        };
        out.push((name, value));
    }
    out
}
