//! Links — internal, pipe/pipe-trick, trail letters, fragments,
//! namespace/category/file/interwiki dispatch, external links, autolinks.

mod common;
use common::*;

#[test]
fn internal_link_blue() {
    // MediaWiki tags every internal link with the prefixed page title.
    assert_eq!(
        render_inner("[[Berlin]]"),
        "<p><a href=\"/wiki/Berlin\" title=\"Berlin\">Berlin</a></p>"
    );
}

#[test]
fn internal_link_red_when_missing() {
    // First-letter is uppercased for the target; display keeps source case.
    // A red link carries class="new" and the "(page does not exist)" title.
    assert_eq!(
        render_inner("[[missingpage]]"),
        "<p><a href=\"/wiki/Missingpage\" class=\"new\" title=\"Missingpage (page does not exist)\">missingpage</a></p>"
    );
}

#[test]
fn piped_label() {
    assert_eq!(
        render_inner("[[Foo bar|the label]]"),
        "<p><a href=\"/wiki/Foo_bar\" title=\"Foo bar\">the label</a></p>"
    );
}

#[test]
fn link_trail_letters_join_inside_anchor() {
    assert_eq!(
        render_inner("[[cat]]s"),
        "<p><a href=\"/wiki/Cat\" title=\"Cat\">cats</a></p>"
    );
}

#[test]
fn fragment_in_href() {
    // The title attribute is the page (no fragment); the fragment stays in href.
    assert_eq!(
        render_inner("[[Berlin#History|hist]]"),
        "<p><a href=\"/wiki/Berlin#History\" title=\"Berlin\">hist</a></p>"
    );
}

#[test]
fn namespaced_link_display_keeps_prefix() {
    assert_eq!(
        render_inner("[[Help:Contents]]"),
        "<p><a href=\"/wiki/Help:Contents\" title=\"Help:Contents\">Help:Contents</a></p>"
    );
}

#[test]
fn pipe_trick_strips_namespace_and_parenthetical() {
    assert_eq!(
        render_inner("[[Help:Foo (bar)|]]"),
        "<p><a href=\"/wiki/Help:Foo_(bar)\" class=\"new\" title=\"Help:Foo (bar) (page does not exist)\">Foo</a></p>"
    );
}

#[test]
fn pipe_trick_strips_after_comma() {
    assert_eq!(
        render_inner("[[Berlin, Germany|]]"),
        "<p><a href=\"/wiki/Berlin,_Germany\" class=\"new\" title=\"Berlin, Germany (page does not exist)\">Berlin</a></p>"
    );
}

#[test]
fn category_collected_not_rendered() {
    let out = render_out("[[Category:Animals]][[Category:Pets|k]] rest");
    assert_eq!(out.categories, vec!["Animals".to_string(), "Pets".to_string()]);
    assert_eq!(render_inner("[[Category:Animals]]text"), "<p>text</p>");
}

#[test]
fn leading_colon_category_renders_as_link() {
    assert_eq!(
        render_inner("[[:Category:Animals]]"),
        "<p><a href=\"/wiki/Category:Animals\" class=\"new\" title=\"Category:Animals (page does not exist)\">Category:Animals</a></p>"
    );
    // ...and does not collect a category.
    assert!(render_out("[[:Category:Animals]]").categories.is_empty());
}

#[test]
fn leading_colon_file_renders_as_link_not_image() {
    assert_eq!(
        render_inner("[[:File:Pic.jpg]]"),
        "<p><a href=\"/wiki/File:Pic.jpg\" class=\"new\" title=\"File:Pic.jpg (page does not exist)\">File:Pic.jpg</a></p>"
    );
}

#[test]
fn interwiki_external_marked() {
    assert_eq!(
        render_inner("[[fr:Paris]]"),
        "<p><a href=\"https://fr.wikipedia.org/wiki/Paris\" class=\"external extiw\">fr:Paris</a></p>"
    );
}

#[test]
fn interwiki_with_label() {
    assert_eq!(
        render_inner("[[fr:Paris|Paris FR]]"),
        "<p><a href=\"https://fr.wikipedia.org/wiki/Paris\" class=\"external extiw\">Paris FR</a></p>"
    );
}

#[test]
fn interwiki_local_instance_is_not_external() {
    assert_eq!(
        render_inner("[[meta:Help]]"),
        "<p><a href=\"https://meta.example/wiki/Help\" class=\"extiw\">meta:Help</a></p>"
    );
}

#[test]
fn external_link_with_label() {
    assert_eq!(
        render_inner("[http://example.com Label]"),
        "<p><a rel=\"nofollow\" class=\"external text\" href=\"http://example.com\">Label</a></p>"
    );
}

#[test]
fn external_link_bare_autonumbered() {
    assert_eq!(
        render_inner("[http://example.com] [https://b.example]"),
        "<p><a rel=\"nofollow\" class=\"external autonumber\" href=\"http://example.com\">[1]</a> \
         <a rel=\"nofollow\" class=\"external autonumber\" href=\"https://b.example\">[2]</a></p>"
    );
}

#[test]
fn bare_url_autolinked_with_trailing_punctuation_trim() {
    assert_eq!(
        render_inner("see http://x.example/a, ok"),
        "<p>see <a rel=\"nofollow\" class=\"external free\" href=\"http://x.example/a\">http://x.example/a</a>, ok</p>"
    );
}

#[test]
fn asof_query_carried_through_internal_links() {
    let full = render_inner_opts("[[Berlin]]", "?asof=2005-01-01", false);
    assert!(full.contains("href=\"/wiki/Berlin?asof=2005-01-01\""), "{full}");
}

#[test]
fn two_links_and_trail() {
    assert_eq!(
        render_inner("[[cat]]s and [[Dog]]gy"),
        "<p><a href=\"/wiki/Cat\" title=\"Cat\">cats</a> and <a href=\"/wiki/Dog\" title=\"Dog\">Doggy</a></p>"
    );
}
