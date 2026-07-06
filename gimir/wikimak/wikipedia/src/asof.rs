//! asof-τ browsing helpers (browsing plan §2, the wayback contract).
//!
//! Two layers, split by dependency weight:
//!
//!   * [`resolve_at_with`] — redirect following at τ, generic over the
//!     redirect parser so the resolution loop compiles and tests under
//!     the default (`fetch`) feature set, where the renderer crate
//!     (`wikimak-wikitext`) is absent.
//!   * [`AsOfView`] + [`resolve_at`] — gated behind `serve`; they bind
//!     the real `wikimak_wikitext::parse_redirect` and adapt an
//!     [`Instance`] bound to one τ into a `wikimak_wikitext::PageStore`.
//!     Kept deliberately thin: every method is a one-line delegation to
//!     the τ read API on [`Instance`], so it cannot rot against the
//!     frozen trait.

use crate::error::Result;
use crate::instance::Instance;

/// Content languages that render right-to-left. Small built-in set
/// (browsing plan §7): siteinfo does not carry directionality, so the
/// renderer's `SiteConfig.rtl` is decided from the wiki's language code.
pub const RTL_LANGS: &[&str] = &[
    "ar", "arc", "arz", "azb", "bcc", "bqi", "ckb", "dv", "fa", "glk", "he", "ks", "ku-arab",
    "lrc", "mzn", "pnb", "ps", "sd", "ug", "ur", "yi",
];

/// Follow `#REDIRECT` at τ, loop-capped, returning the final page id.
///
/// `parse_redirect(text_bytes) -> Some(target_title)` decides whether a
/// revision is a redirect and to where. Each hop resolves its target
/// through [`Instance::page_id_by_title_at`] at the SAME τ, then reads
/// that revision's text with [`Instance::page_text_at`]. Termination:
///
///   * a title that does not resolve at τ → `Ok(None)` (red link);
///   * a page whose τ-revision is not a redirect → `Ok(Some(page_id))`;
///   * a page that resolves in the titles table but has no revision ≤ τ
///     → `Ok(Some(page_id))` (terminal: it exists, just no readable
///     text at τ — the renderer shows an empty/《no revision》page);
///   * revisiting a page id (redirect cycle) → `Ok(None)`;
///   * more than `max_hops` redirects → `Ok(None)`.
pub fn resolve_at_with<F>(
    inst: &Instance,
    title: &str,
    ts_micros: Option<i64>,
    max_hops: u32,
    parse_redirect: F,
) -> Result<Option<u64>>
where
    F: Fn(&[u8]) -> Option<String>,
{
    let mut current = title.trim().to_string();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // max_hops redirects → up to max_hops + 1 page resolutions.
    for _ in 0..=max_hops {
        let page_id = match inst.page_id_by_title_at(&current, ts_micros)? {
            Some(id) => id,
            None => return Ok(None),
        };
        if !seen.insert(page_id) {
            return Ok(None);
        }
        let text = match inst.page_text_at(page_id, ts_micros)? {
            Some(t) => t,
            None => return Ok(Some(page_id)),
        };
        match parse_redirect(&text) {
            Some(target) => current = target,
            None => return Ok(Some(page_id)),
        }
    }
    Ok(None)
}

/// Serve-layer redirect resolution: [`resolve_at_with`] bound to the
/// real `wikimak_wikitext::parse_redirect` (browsing plan §2 redirects).
#[cfg(feature = "serve")]
pub fn resolve_at(
    inst: &Instance,
    title: &str,
    ts_micros: Option<i64>,
    max_hops: u32,
) -> Result<Option<u64>> {
    resolve_at_with(inst, title, ts_micros, max_hops, |bytes| {
        wikimak_wikitext::parse_redirect(&String::from_utf8_lossy(bytes))
    })
}

/// An [`Instance`] pinned to one instant τ, presented to the renderer as
/// a `wikimak_wikitext::PageStore` (browsing plan §2). The τ is baked in
/// at construction: the renderer never sees a timestamp except through
/// `timestamp_micros`.
#[cfg(feature = "serve")]
pub struct AsOfView<'a> {
    inst: &'a Instance,
    /// The τ passed to the read API (`None` = current head).
    ts: Option<i64>,
    site: wikimak_wikitext::SiteConfig,
    /// τ in unix micros for `{{CURRENTYEAR}}` etc.: the caller's τ, or
    /// the construction-time wall clock when browsing "now".
    ts_micros: i64,
}

#[cfg(feature = "serve")]
impl<'a> AsOfView<'a> {
    /// Build a view of `inst` at τ = `ts` (`None` = current). The
    /// `SiteConfig` is assembled from the siteinfo snapshot chosen for τ
    /// ([`Instance::site_config_at`]); a missing snapshot yields a
    /// default (empty) config so rendering still proceeds.
    pub fn new(inst: &'a Instance, ts: Option<i64>) -> Result<Self> {
        let snapshot = inst.site_config_at(ts)?;
        let site = build_site_config(snapshot.as_ref());
        let ts_micros = ts.unwrap_or_else(|| chrono::Utc::now().timestamp_micros());
        Ok(Self {
            inst,
            ts,
            site,
            ts_micros,
        })
    }
}

/// Assemble a renderer `SiteConfig` from a raw siteinfo snapshot JSON.
/// Tolerates snapshots written before the `namespaces` key existed
/// (browsing plan §2: additive keys, old snapshots degrade to an empty
/// namespace map). `lang` is derived from the db_name (`arwiki` → `ar`);
/// siteinfo carries no language field, and `rtl` follows [`RTL_LANGS`].
#[cfg(feature = "serve")]
fn build_site_config(snapshot: Option<&serde_json::Value>) -> wikimak_wikitext::SiteConfig {
    use std::collections::BTreeMap;
    let mut cfg = wikimak_wikitext::SiteConfig::default();
    let Some(v) = snapshot else {
        return cfg;
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    cfg.site_name = s("site_name");
    cfg.db_name = s("db_name");
    cfg.lang = lang_from_db_name(&cfg.db_name);
    cfg.rtl = RTL_LANGS.contains(&cfg.lang.as_str());
    // mw.site.server: scheme + host of the siteinfo <base> URL (the URL of
    // the wiki's main page), no path — e.g. "https://en.wikipedia.org".
    // Empty when no base is recorded.
    cfg.server = server_from_base(&s("base"));
    // Standard MediaWiki script path; siteinfo does not carry it.
    cfg.script_path = "/w".to_string();
    let mut namespaces: BTreeMap<i32, wikimak_wikitext::NamespaceInfo> = BTreeMap::new();
    if let Some(arr) = v.get("namespaces").and_then(|x| x.as_array()) {
        for ns in arr {
            let id = match ns.get("id").and_then(|x| x.as_i64()) {
                Some(id) => id as i32,
                None => continue,
            };
            let canonical = ns
                .get("canonical")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let case = ns.get("case").and_then(|x| x.as_str()).unwrap_or("");
            let aliases = ns
                .get("aliases")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            namespaces.insert(
                id,
                wikimak_wikitext::NamespaceInfo {
                    id,
                    canonical,
                    aliases,
                    case_first_letter: case == "first-letter",
                },
            );
        }
    }
    cfg.namespaces = namespaces;
    // interwiki left empty for now (browsing plan §2: interwikimap-at-τ
    // is a later item; the map is populated once siteinfo capture carries
    // <interwiki>).
    cfg
}

/// Extract `scheme://host` from a siteinfo `<base>` URL, dropping the
/// path (`http://x/Main_Page` → `http://x`). A base without `://` yields
/// an empty server (links stay relative). No allocation-heavy URL crate:
/// the base is a plain absolute URL in every dump.
#[cfg(feature = "serve")]
fn server_from_base(base: &str) -> String {
    let base = base.trim();
    let Some(scheme_end) = base.find("://") else {
        return String::new();
    };
    let after = scheme_end + 3;
    let host_end = base[after..]
        .find('/')
        .map(|i| after + i)
        .unwrap_or(base.len());
    base[..host_end].to_string()
}

/// `arwiki` → `ar`, `enwiki` → `en`; a db_name not ending in `wiki` is
/// used verbatim. Language code is not in the dump's siteinfo, so it is
/// inferred from the database name.
#[cfg(feature = "serve")]
fn lang_from_db_name(db_name: &str) -> String {
    db_name
        .strip_suffix("wiki")
        .filter(|s| !s.is_empty())
        .unwrap_or(db_name)
        .to_string()
}

#[cfg(feature = "serve")]
impl wikimak_wikitext::PageStore for AsOfView<'_> {
    fn page_text(&self, title: &wikimak_wikitext::Title) -> Option<String> {
        let key = title.prefixed(&self.site);
        let page_id = self.inst.page_id_by_title_at(&key, self.ts).ok().flatten()?;
        // Redirects are NOT auto-followed here (browsing plan §2: the
        // renderer decides whether/when to follow via `parse_redirect`).
        let bytes = self.inst.page_text_at(page_id, self.ts).ok().flatten()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn page_exists(&self, title: &wikimak_wikitext::Title) -> bool {
        let key = title.prefixed(&self.site);
        self.inst.exists_at(&key, self.ts).unwrap_or(false)
    }

    fn page_id(&self, title: &wikimak_wikitext::Title) -> Option<u64> {
        let key = title.prefixed(&self.site);
        self.inst.page_id_by_title_at(&key, self.ts).ok().flatten()
    }

    fn site(&self) -> &wikimak_wikitext::SiteConfig {
        &self.site
    }

    fn timestamp_micros(&self) -> i64 {
        self.ts_micros
    }
}
