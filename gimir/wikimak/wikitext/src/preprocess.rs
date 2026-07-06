//! Preprocessor/transclusion engine (plan §3.2): template expansion in
//! MediaWiki's exact order — <includeonly>/<noinclude>/<onlyinclude>,
//! {{{param|default}}}, parser functions (core + ParserFunctions),
//! magic words/variables (τ-resolved via PageStore::timestamp_micros),
//! depth/loop limits. Reference: Preprocessor_Hash.php + PPFrame
//! semantics (GPL — behavior reference only, no code reuse).
//!
//! OWNED BY: the preprocessor agent. The skeleton passes text through.

use crate::{PageStore, RenderMisses, RenderOptions, Title};

pub struct Expanded {
    pub text: String,
    pub misses: RenderMisses,
}

pub fn expand(
    store: &dyn PageStore,
    title: &Title,
    text: &str,
    opts: &RenderOptions<'_>,
) -> Expanded {
    let _ = (store, title, opts);
    Expanded { text: text.to_string(), misses: RenderMisses::default() }
}

/// `#REDIRECT [[Target]]` (localized synonyms come from magicwords
/// later; the English form first).
pub fn parse_redirect(text: &str) -> Option<String> {
    let t = text.trim_start();
    let lower = t.get(..9)?.to_ascii_lowercase();
    if lower != "#redirect" {
        return None;
    }
    let rest = &t[9..];
    let open = rest.find("[[")?;
    let close = rest[open + 2..].find("]]")?;
    let inner = &rest[open + 2..open + 2 + close];
    let target = inner.split('|').next().unwrap_or(inner).split('#').next().unwrap_or(inner);
    Some(target.trim().to_string())
}
