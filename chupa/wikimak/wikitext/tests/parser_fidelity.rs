//! parserTests-fidelity pins (browsing plan §6). Each block below locks in
//! the GENERAL MediaWiki behavior behind a measured parserTests core-subset
//! gap — not just the one corpus case — so an adversarial variation of the
//! same construct is covered. Exact-HTML assertions throughout.
//!
//!   1. internal links carry `title="<prefixed page>"`
//!   2. red links carry `class="new"` + `"(page does not exist)"` title
//!      (serve-route href kept — see the render_page_link note)
//!   3. `[[URL …]]` is NOT a wikilink — literal brackets + external link
//!   4. character-reference normalization (Sanitizer::normalizeCharReferences)
//!   5. tables get an implicit `<tbody>` + last-wins duplicate-attr dedup
//!   6. block-level HTML tags break paragraphs; inline tags do not

mod common;
use common::*;

// ---------------------------------------------------------------------------
// Gap 1 — internal links carry a title attribute (the prefixed page title).
// ---------------------------------------------------------------------------

#[test]
fn title_on_plain_blue_link() {
    assert_eq!(
        render_inner("[[Berlin]]"),
        "<p><a href=\"/wiki/Berlin\" title=\"Berlin\">Berlin</a></p>"
    );
}

#[test]
fn title_on_piped_link_is_the_page_not_the_label() {
    assert_eq!(
        render_inner("[[Foo bar|see here]]"),
        "<p><a href=\"/wiki/Foo_bar\" title=\"Foo bar\">see here</a></p>"
    );
}

#[test]
fn title_on_namespaced_link_keeps_prefix_with_spaces() {
    assert_eq!(
        render_inner("[[Help:Contents]]"),
        "<p><a href=\"/wiki/Help:Contents\" title=\"Help:Contents\">Help:Contents</a></p>"
    );
}

#[test]
fn title_is_the_page_even_with_a_fragment_and_trail() {
    // The title attribute is the PAGE alone ("Cat"); the fragment stays in
    // the href and in the (unpiped) visible text, and trail letters join it.
    assert_eq!(
        render_inner("[[Cat#Body]]s"),
        "<p><a href=\"/wiki/Cat#Body\" title=\"Cat\">Cat#Bodys</a></p>"
    );
}

// ---------------------------------------------------------------------------
// Gap 2 — red links: class="new" + "(page does not exist)" title. The href
// deliberately stays on the serve layer's link_prefix route (not MediaWiki's
// /index.php?...&redlink=1 form) so serve's /wiki/<title> routing keeps
// working; see the render_page_link comment.
// ---------------------------------------------------------------------------

#[test]
fn red_link_class_and_title() {
    assert_eq!(
        render_inner("[[Nonexistent page]]"),
        "<p><a href=\"/wiki/Nonexistent_page\" class=\"new\" title=\"Nonexistent page (page does not exist)\">Nonexistent page</a></p>"
    );
}

#[test]
fn red_link_title_on_a_piped_link_uses_the_page_name() {
    assert_eq!(
        render_inner("[[Absent thing|display]]"),
        "<p><a href=\"/wiki/Absent_thing\" class=\"new\" title=\"Absent thing (page does not exist)\">display</a></p>"
    );
}

#[test]
fn red_link_href_stays_on_serve_route() {
    // The href routes through the serve layer's /wiki/ prefix, NOT an
    // index.php edit URL — so red-link navigation still resolves in serve.
    let html = render_inner("[[Absent thing|display]]");
    assert!(html.contains("href=\"/wiki/Absent_thing\""), "{html}");
    assert!(!html.contains("index.php"), "{html}");
    assert!(!html.contains("redlink=1"), "{html}");
}

// ---------------------------------------------------------------------------
// Gap 3 — `[[URL …]]` is not a wikilink: the brackets stay literal and the
// inner `[URL …]` becomes an external link.
// ---------------------------------------------------------------------------

#[test]
fn double_bracket_url_with_label_is_literal_bracket_plus_external() {
    assert_eq!(
        render_inner("[[http://example.com Link text]]"),
        "<p>[<a rel=\"nofollow\" class=\"external text\" href=\"http://example.com\">Link text</a>]</p>"
    );
}

#[test]
fn double_bracket_bare_url_is_bracket_plus_autonumber() {
    assert_eq!(
        render_inner("[[https://a.example]]"),
        "<p>[<a rel=\"nofollow\" class=\"external autonumber\" href=\"https://a.example\">[1]</a>]</p>"
    );
}

// ---------------------------------------------------------------------------
// Gap 4 — character-reference normalization.
// ---------------------------------------------------------------------------

#[test]
fn defined_named_entities_become_decimal_numeric() {
    assert_eq!(
        render_inner("&eacute; &aacute; &copy;"),
        "<p>&#233; &#225; &#169;</p>"
    );
}

#[test]
fn undefined_named_entity_is_escaped() {
    assert_eq!(
        render_inner("&xacute; here"),
        "<p>&amp;xacute; here</p>"
    );
}

#[test]
fn semicolonless_run_is_escaped_not_decoded() {
    // `&ampamp;` reads as the single name "ampamp;" (undefined), not `&amp`
    // + "amp;" — MediaWiki requires the whole semicolon-terminated name.
    assert_eq!(render_inner("&ampamp;"), "<p>&amp;ampamp;</p>");
    // A name with no trailing semicolon is likewise not decoded.
    assert_eq!(render_inner("&copy x"), "<p>&amp;copy x</p>");
}

#[test]
fn xml_predefined_entities_stay_in_word_form() {
    assert_eq!(
        render_inner("&amp; &lt; &gt; &quot;"),
        "<p>&amp; &lt; &gt; &quot;</p>"
    );
}

#[test]
fn numeric_references_are_renormalized() {
    // Decimal stays decimal (leading zeros dropped); hex stays lowercase hex.
    assert_eq!(
        render_inner("&#0233; &#xE9; &#x2764;"),
        "<p>&#233; &#xe9; &#x2764;</p>"
    );
}

#[test]
fn out_of_range_numeric_reference_is_escaped() {
    // U+0000 is not a valid HTML5/XML character → the `&` is escaped.
    assert_eq!(render_inner("&#0; x"), "<p>&amp;#0; x</p>");
}

// ---------------------------------------------------------------------------
// Gap 5 — implicit <tbody> and last-wins duplicate-attribute dedup.
// ---------------------------------------------------------------------------

#[test]
fn tbody_wraps_every_row_of_a_multirow_table() {
    assert_eq!(
        render_inner("{|\n! head\n|-\n| one\n|-\n| two\n|}"),
        "<table><tbody><tr><th>head</th></tr><tr><td>one</td></tr><tr><td>two</td></tr></tbody></table>"
    );
}

#[test]
fn duplicate_cell_attribute_keeps_the_last_value() {
    assert_eq!(
        render_inner("{|\n| class=\"error\" class=\"awesome\" | x\n|}"),
        "<table><tbody><tr><td class=\"awesome\">x</td></tr></tbody></table>"
    );
}

#[test]
fn duplicate_table_attribute_keeps_the_last_value() {
    assert_eq!(
        render_inner("{| class=\"a\" class=\"b\"\n| x\n|}"),
        "<table class=\"b\"><tbody><tr><td>x</td></tr></tbody></table>"
    );
}

// ---------------------------------------------------------------------------
// Gap 6 — block-level HTML tags break paragraphs; inline tags do not.
// ---------------------------------------------------------------------------

#[test]
fn div_breaks_a_paragraph_midtext() {
    assert_eq!(
        render_inner("before\n<div>block</div>\nafter"),
        "<p>before</p><div>block</div><p>after</p>"
    );
}

#[test]
fn center_is_block_but_big_is_inline() {
    // A lone <center> line is hoisted (no <p>); a lone <big> line is wrapped.
    assert_eq!(render_inner("<center>x</center>"), "<center>x</center>");
    assert_eq!(render_inner("<big>x</big>"), "<p><big>x</big></p>");
}

#[test]
fn consecutive_inline_tag_lines_share_one_paragraph() {
    assert_eq!(
        render_inner("<big>a</big>\n<tt>b</tt>"),
        "<p><big>a</big>\n<tt>b</tt></p>"
    );
}
