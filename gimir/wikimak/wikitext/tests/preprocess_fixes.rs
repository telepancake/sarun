//! Regression tests for the preprocessor / magic-word fixes the real-page
//! corpus straightedge surfaced (tests/corpus.rs). Each pins an exact
//! input → output:
//!
//!  1. subst/safesubst/msg/raw prefixes strip FIRST, then the remainder is
//!     dispatched (`#invoke:` → invoke, `#if:` → parser function), including
//!     when the name is dynamic (`{{ {{{|safesubst:}}}#invoke:… }}`).
//!  2. localized parser functions/variables resolve via siteinfo magic-word
//!     aliases (`#תנאי:` → `#if:`), with the built-in English names intact.
//!  3. self-closing inclusion tags in a function name (`{{<noinclude/>#if:…}}`).
//!  4. the `{{!}}` / `{{=}}` builtins.
//!  5. includeonly/noinclude/onlyinclude/templatestyles fully consumed — never
//!     left as tags for the parser to flag.

#[path = "preprocess_common/mod.rs"]
mod common;

use std::collections::BTreeMap;

use common::*;
use wikimak_wikitext::preprocess::expand;
use wikimak_wikitext::{Frame, RenderOptions, SiteConfig};

/// Expanded text on a given render title (mainspace, no invoker).
fn xt_on(store: &MockStore, t: &wikimak_wikitext::Title, text: &str) -> String {
    ex_on(store, t, text).text
}

// --- helpers ---------------------------------------------------------------

/// Expand a mainspace page with a #invoke handler that echoes module/function.
fn xt_invoke(store: &MockStore, text: &str) -> String {
    let inv = FnInvoker(|m: &str, f: &str, _: &Frame| Ok(format!("[{m}/{f}]")));
    let opts = RenderOptions { invoker: Some(&inv), ..Default::default() };
    expand(store, &title(0, "Test"), text, &opts).text
}

/// Mirror of corpus.rs build_site's alias index: (canonical, aliases,
/// case_sensitive) → token map, with case-insensitive words also keyed
/// lowercased (trailing `:` on subst-family aliases stripped).
fn alias_map(entries: &[(&str, &[&str], bool)]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (name, aliases, cs) in entries {
        for a in *aliases {
            let token = a.strip_suffix(':').unwrap_or(a).to_string();
            m.entry(token.clone()).or_insert_with(|| name.to_string());
            if !cs {
                m.entry(token.to_lowercase()).or_insert_with(|| name.to_string());
            }
        }
    }
    m
}

/// A site with a realistic (localized + English) magic-word alias index for
/// he/fa parser functions and one localized variable.
fn localized_site() -> SiteConfig {
    let mut s = standard_site();
    s.magic_aliases = alias_map(&[
        ("if", &["תנאי", "اگر", "if"], false),
        ("ifeq", &["שווה", "ifeq"], false),
        ("switch", &["בחר", "switch"], false),
        ("invoke", &["invoke"], false),
        ("safesubst", &["ס בטוח:", "SAFESUBST:"], false),
        ("pagename", &["שם הדף", "PAGENAME"], true),
        ("!", &["!"], true),
    ]);
    s
}

// --- bug 1: subst/safesubst prefix + inner dispatch ------------------------

#[test]
fn safesubst_invoke_static_and_dynamic() {
    let s = MockStore::new();
    // Static: subst prefix strips, `#invoke:String` dispatches (String is the
    // module, the next part the function).
    assert_eq!(xt_invoke(&s, "{{safesubst:#invoke:String|len|abc}}"), "[String/len]");
    // Dynamic name via `{{{|safesubst:}}}` — the exact top corpus miss. The
    // head must be expanded, THEN the prefix stripped and `#invoke` dispatched,
    // not transcluded as a literal title "Safesubst:#invoke:String".
    assert_eq!(xt_invoke(&s, "{{ {{{|safesubst:}}}#invoke:String|len|abc}}"), "[String/len]");
}

#[test]
fn subst_prefix_is_transparent_for_templates_and_functions() {
    let mut s = MockStore::new();
    s.template("Foo", "BODY");
    // subst: on a plain template transcludes the template, not "subst:Foo".
    assert_eq!(xt(&s, "{{subst:Foo}}"), "BODY");
    // subst: on a parser function dispatches the function.
    assert_eq!(xt(&s, "{{subst:#if:x|yes|no}}"), "yes");
    // A genuinely dynamic template name still transcludes correctly.
    assert_eq!(xt(&s, "{{ {{{|Foo}}} }}"), "BODY");
}

// --- bug 3: self-closing inclusion tags inside a function name -------------

#[test]
fn self_closing_noinclude_in_function_name() {
    let s = MockStore::new();
    // `<noinclude/>` must be resolved before template-vs-function detection so
    // the call is `#if`, not template "<noinclude/>#if:".
    assert_eq!(xt(&s, "{{<noinclude/>#if:x|yes|no}}"), "yes");
    assert_eq!(xt(&s, "{{<noinclude />#if:x|yes|no}}"), "yes");
    // safesubst prefix with an embedded self-closing noinclude.
    assert_eq!(xt(&s, "{{safesubst<noinclude/>:#if:x|yes|no}}"), "yes");
    assert_eq!(xt(&s, "{{safesubst:<noinclude/>#if:x|yes|no}}"), "yes");
}

// --- bug 4: {{!}} / {{=}} builtins -----------------------------------------

#[test]
fn pipe_and_equals_builtins() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{!}}"), "|");
    assert_eq!(xt(&s, "{{=}}"), "=");
    assert_eq!(xt(&s, "a{{!}}b{{=}}c"), "a|b=c");
    // Works with a populated (case-sensitive `!`) alias index too.
    let mut ls = MockStore::new();
    ls.site = localized_site();
    assert_eq!(xt(&ls, "{{!}}"), "|");
}

// --- bug 2: localized parser functions + variables -------------------------

#[test]
fn localized_parser_functions() {
    let mut s = MockStore::new();
    s.site = localized_site();
    // Hebrew #if / #ifeq / #switch.
    assert_eq!(xt(&s, "{{#תנאי:x|yes|no}}"), "yes");
    assert_eq!(xt(&s, "{{#תנאי:|yes|no}}"), "no");
    assert_eq!(xt(&s, "{{#שווה:1|1|eq|ne}}"), "eq");
    assert_eq!(xt(&s, "{{#בחר:b|a=1|b=2|c=3}}"), "2");
    // Persian #if.
    assert_eq!(xt(&s, "{{#اگر:x|yes|no}}"), "yes");
    // English names still resolve when the alias index is populated.
    assert_eq!(xt(&s, "{{#if:x|yes|no}}"), "yes");
    assert_eq!(xt(&s, "{{#switch:b|a=1|b=2}}"), "2");
}

#[test]
fn localized_variable_and_no_alias_fallback() {
    let mut s = MockStore::new();
    s.site = localized_site();
    // Localized {{PAGENAME}} alias resolves to the render title.
    assert_eq!(xt_on(&s, &title(0, "Test"), "{{שם הדף}}"), "Test");
    assert_eq!(xt_on(&s, &title(0, "Test"), "{{PAGENAME}}"), "Test");
    // With NO alias index, English built-ins still work (empty map path).
    let plain = MockStore::new();
    assert_eq!(xt(&plain, "{{#if:x|yes|no}}"), "yes");
    assert_eq!(xt_on(&plain, &title(0, "Test"), "{{PAGENAME}}"), "Test");
    // A localized token with no index is just an (unknown) template name.
    assert_eq!(xt(&plain, "{{#תנאי:x|yes|no}}").contains("תנאי"), true);
}

// --- bug 5: inclusion + templatestyles fully consumed ----------------------

/// Convenience for a whole `xt` string helper (mainspace, no invoker).
fn top(store: &MockStore, text: &str) -> String {
    xt(store, text)
}

#[test]
fn inclusion_tags_consumed_on_page_and_transclusion() {
    let mut s = MockStore::new();
    s.template("T", "<includeonly>IN</includeonly><noinclude>DOC</noinclude>");
    // Transcluded: includeonly body kept, noinclude dropped, no tags leak.
    assert_eq!(xt(&s, "{{T}}"), "IN");
    // On the page itself: includeonly dropped, noinclude unwrapped.
    let out = top(&s, "<includeonly>X</includeonly>Y<noinclude>Z</noinclude>");
    assert_eq!(out, "YZ");
    for tag in ["<includeonly", "<noinclude", "<onlyinclude"] {
        assert!(!xt(&s, "{{T}}").contains(tag), "leaked {tag}");
        assert!(!out.contains(tag), "leaked {tag} on page");
    }
}

#[test]
fn onlyinclude_keeps_region_and_processes_inner_tags() {
    let mut s = MockStore::new();
    s.template(
        "T2",
        "before<onlyinclude>A<includeonly>B</includeonly><noinclude>C</noinclude></onlyinclude>after",
    );
    // Only the onlyinclude region transcludes; inside it includeonly is kept
    // and noinclude dropped; nothing outside survives; no tags leak.
    let out = xt(&s, "{{T2}}");
    assert_eq!(out, "AB");
    assert!(!out.contains('<'));
}

#[test]
fn templatestyles_consumed() {
    let s = MockStore::new();
    assert_eq!(
        xt(&s, "<templatestyles src=\"Foo/styles.css\" />after"),
        "after"
    );
    assert_eq!(
        xt(&s, "x<templatestyles src=\"A/styles.css\"/>y"),
        "xy"
    );
}

#[test]
fn self_closing_nowiki_disappears_without_corrupting_later_nowiki() {
    let s = MockStore::new();
    // A self-closing <nowiki/> produces nothing and must NOT swallow a later
    // paired nowiki (that would corrupt it into the parser as an unknown tag).
    let out = xt(&s, "a<nowiki/>b <nowiki>{{x}}</nowiki> c");
    // The paired nowiki is protected verbatim (restored after expansion);
    // the self-closing one vanished.
    assert_eq!(out, "ab <nowiki>{{x}}</nowiki> c");
}
