//! wikitext → HTML renderer (browsing plan B1–B3, §3 architecture).
//!
//! Layered: preprocessor (template/parser-function/magic-word expansion,
//! all lookups through [`PageStore`]) feeds the parser core (wikitext →
//! document tree → HTML). Scribunto and media are dependency-INVERTED:
//! this crate stays pure Rust (no C, no network) and calls out through
//! [`ModuleInvoker`] / [`MediaResolver`] traits; the serve layer wires
//! the real implementations (wikimak-scribunto, wikimak-media).
//!
//! The asof-τ contract (plan §2) lives OUTSIDE this crate: a
//! [`PageStore`] is already bound to one instant τ by its constructor.
//! The renderer never sees a timestamp except through
//! [`PageStore::timestamp_micros`] (which drives `{{CURRENTYEAR}}` and
//! friends — τ, not wall-clock).
//!
//! Failure discipline (plan §3): a template/module that errors renders
//! an inline error box, never aborts the page. Unknown extension tags
//! render as visible labeled placeholders and are counted in
//! [`RenderOutput::misses`] — never silently dropped.

use std::collections::BTreeMap;

pub mod html;
pub mod magic;
pub mod parser;
pub mod preprocess;
pub mod title;

pub use title::Title;

/// One namespace from siteinfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceInfo {
    pub id: i32,
    /// Canonical name, e.g. "Template". Empty for ns 0.
    pub canonical: String,
    /// Localized name + aliases, normalized (underscores → spaces).
    pub aliases: Vec<String>,
    /// true = first-letter case-insensitive (the usual MediaWiki rule).
    pub case_first_letter: bool,
}

/// One interwiki prefix from the interwikimap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterwikiEntry {
    pub prefix: String,
    /// URL pattern with `$1` for the target title.
    pub url: String,
    /// Set when the prefix resolves to a locally mirrored instance —
    /// rendered as a local link (cross-instance browsing) instead of an
    /// external one (plan §5 "Interwiki").
    pub local_instance: Option<String>,
}

/// Site configuration the renderer consults constantly — namespaces,
/// case rules, language/direction, interwiki. Sourced from the
/// instance's siteinfo snapshot at τ.
#[derive(Debug, Clone, Default)]
pub struct SiteConfig {
    pub site_name: String,
    pub db_name: String,
    /// Content language code, e.g. "en", "ar".
    pub lang: String,
    /// true for right-to-left content languages (ar, he, fa, ur…):
    /// the HTML gets dir="rtl" on the content root and the parser keeps
    /// bidi-neutral punctuation inside directional runs.
    pub rtl: bool,
    /// Canonical server URL, scheme + host, no trailing slash — e.g.
    /// "https://en.wikipedia.org". Drives `mw.site.server` and the
    /// absolute half of `mw.uri.fullUrl`. Empty when siteinfo carries no
    /// base URL (the serve layer's links stay relative regardless).
    pub server: String,
    /// Wiki script path, e.g. "/w" — drives `mw.site.scriptPath` and the
    /// path half of `mw.uri.localUrl`/`fullUrl`. Empty by default.
    pub script_path: String,
    pub namespaces: BTreeMap<i32, NamespaceInfo>,
    pub interwiki: BTreeMap<String, InterwikiEntry>,
    /// Localized magic-word aliases from siteinfo (`siteinfo.magicwords`):
    /// each alias token (leading `#` and trailing `:` removed) mapped to its
    /// canonical magic-word id — e.g. "תנאי" → "if", "שם הדף" → "pagename".
    /// Case-insensitive words are additionally keyed by their lowercased
    /// alias at build time. Empty ⇒ only the built-in English names resolve.
    pub magic_aliases: BTreeMap<String, String>,
}

/// Everything the renderer asks the wiki. Implementations are already
/// bound to one τ — the renderer is a pure function of this trait.
pub trait PageStore {
    /// Wikitext of the page at τ, by normalized title. None = red link.
    fn page_text(&self, title: &Title) -> Option<String>;
    /// Existence at τ (red/blue links, #ifexist) — MUST be cheaper than
    /// `page_text` (titles-table point lookup, no frame decode).
    fn page_exists(&self, title: &Title) -> bool;
    /// Numeric page id at τ (drives `mw.title.id`). Default `None` keeps
    /// the trait cheap for stores that don't expose ids; the serve-layer
    /// [`AsOfView`](../wikimak_wikipedia/asof/struct.AsOfView.html) fills
    /// it from the titles table.
    fn page_id(&self, title: &Title) -> Option<u64> {
        let _ = title;
        None
    }
    fn site(&self) -> &SiteConfig;
    /// τ in unix micros — drives {{CURRENTYEAR}} etc. (plan §3.2: τ!).
    fn timestamp_micros(&self) -> i64;
}

/// `{{#invoke:Module|fn|args}}` boundary (Scribunto). The implementation
/// (wikimak-scribunto) owns the Lua sandbox; expansion depth/loop limits
/// stay in the preprocessor.
pub trait ModuleInvoker {
    /// Returns the wikitext produced by the invocation, or Err(message)
    /// which the renderer shows as an inline script-error box.
    fn invoke(
        &self,
        module: &str,
        function: &str,
        frame: &Frame,
        store: &dyn PageStore,
    ) -> Result<String, String>;
}

/// `[[File:…]]` / thumb URL boundary. None = render an offline
/// placeholder box (counted in misses), never a broken external dep.
pub trait MediaResolver {
    fn image_url(&self, file: &Title, width_px: Option<u32>) -> Option<String>;
}

/// Template/invoke call frame: named + positional args, parent access.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    /// Positional args as "1", "2", … plus named args. Values are
    /// UNEXPANDED wikitext; expansion is lazy (frame semantics).
    pub args: BTreeMap<String, String>,
    pub parent: Option<Box<Frame>>,
    /// Title of the page/template this frame is expanding.
    pub title: String,
}

/// Counters for what the render could not do faithfully — the accuracy
/// harness (plan §6) aggregates these; the UI can badge them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderMisses {
    pub unknown_tags: Vec<String>,
    pub failed_invokes: Vec<String>,
    pub missing_templates: Vec<String>,
    pub missing_media: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    pub html: String,
    /// Direct [[Category:…]] declarations found during parse.
    pub categories: Vec<String>,
    pub misses: RenderMisses,
}

/// Rendering options carried through the whole pipeline.
pub struct RenderOptions<'a> {
    pub invoker: Option<&'a dyn ModuleInvoker>,
    pub media: Option<&'a dyn MediaResolver>,
    /// Link-href prefix for internal links, e.g. "/wiki/enwiki/".
    /// The serve layer appends `?asof=` itself via `asof_query`.
    pub link_prefix: String,
    /// Query-string suffix appended to every internal link (carries the
    /// date picker through navigation), e.g. "?asof=2005-01-01".
    pub asof_query: String,
}

impl Default for RenderOptions<'_> {
    fn default() -> Self {
        RenderOptions {
            invoker: None,
            media: None,
            link_prefix: "./".into(),
            asof_query: String::new(),
        }
    }
}

/// The facade: preprocess (expand templates/parser functions/magic
/// words through `store`) then parse to HTML.
pub fn render(
    store: &dyn PageStore,
    title: &Title,
    text: &str,
    opts: &RenderOptions<'_>,
) -> RenderOutput {
    let expanded = preprocess::expand(store, title, text, opts);
    parser::to_html(store, title, &expanded.text, opts, expanded.misses)
}

/// `#REDIRECT [[Target]]` detection on RAW wikitext (pre-expansion) —
/// a property of the revision text, followed at τ by the caller,
/// loop-capped there (plan §2 "Redirects").
pub fn parse_redirect(text: &str) -> Option<String> {
    preprocess::parse_redirect(text)
}
