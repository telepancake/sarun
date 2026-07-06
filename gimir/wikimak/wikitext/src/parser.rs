//! Parser core (plan §3.1): expanded wikitext → document tree → HTML.
//! Headings, lists, tables, [[links]] (ns/interwiki/File: dispatch),
//! external links, ''formatting'', <nowiki>/<pre>, HTML-in-wikitext
//! sanitization. Conformance corpus: MediaWiki parserTests (fetched at
//! test time, never vendored — GPL).
//!
//! OWNED BY: the parser-core agent. The skeleton escapes text into <p>.

use crate::{html, PageStore, RenderMisses, RenderOptions, RenderOutput, Title};

pub fn to_html(
    store: &dyn PageStore,
    title: &Title,
    expanded: &str,
    opts: &RenderOptions<'_>,
    misses: RenderMisses,
) -> RenderOutput {
    let _ = (store, title, opts);
    RenderOutput {
        html: format!("<p>{}</p>", html::escape(expanded)),
        categories: Vec::new(),
        misses,
    }
}
