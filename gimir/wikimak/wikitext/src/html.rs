//! HTML output helpers: escaping, the sanitizer allowlist for
//! HTML-in-wikitext, inline error boxes, RTL/dir attributes.
//! OWNED BY: the parser-core agent.

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

/// MediaWiki-style inline error box (script errors, failed invokes).
pub fn error_box(msg: &str) -> String {
    format!(r#"<span class="error">{}</span>"#, escape(msg))
}
