//! HTML-in-wikitext sanitizer: tag allowlist, attribute allowlist, style
//! scrubbing, unknown-tag escaping + counting, poem/gallery handling, RTL
//! content wrapper.

mod common;
use common::*;

#[test]
fn allowed_tag_with_allowed_attr() {
    assert_eq!(
        render_inner("<span class=\"hl\">t</span>"),
        "<p><span class=\"hl\">t</span></p>"
    );
}

#[test]
fn disallowed_attribute_dropped() {
    assert_eq!(
        render_inner("<b class=\"a\" onclick=\"evil()\">t</b>"),
        "<p><b class=\"a\">t</b></p>"
    );
}

#[test]
fn style_scrubs_dangerous_declaration_keeps_safe() {
    assert_eq!(
        render_inner("<span style=\"color:red; behavior:url(x)\">s</span>"),
        "<p><span style=\"color:red\">s</span></p>"
    );
}

#[test]
fn style_strips_expression() {
    // <div> is block-level: MediaWiki's BlockLevelPass hoists it out of a
    // paragraph, so it is not wrapped in <p>.
    assert_eq!(
        render_inner("<div style=\"width:expression(alert(1)); height:2px\">d</div>"),
        "<div style=\"height:2px\">d</div>"
    );
}

#[test]
fn unknown_tag_is_escaped_and_counted() {
    let out = render_out("<blink>x</blink>");
    assert_eq!(out.html, "<div class=\"mw-parser-output\"><p>&lt;blink&gt;x&lt;/blink&gt;</p></div>");
    assert_eq!(out.misses.unknown_tags, vec!["blink".to_string(), "blink".to_string()]);
}

#[test]
fn script_tag_fully_neutralized() {
    assert_eq!(
        render_inner("<script>alert(1)</script>"),
        "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>"
    );
}

#[test]
fn img_tag_in_wikitext_is_not_allowed() {
    // Raw <img> is never emitted from wikitext (only the File: pipeline).
    let out = render_out("<img src=\"x\">");
    assert!(out.misses.unknown_tags.contains(&"img".to_string()));
    assert!(out.html.contains("&lt;img"));
}

#[test]
fn poem_is_remapped_to_div() {
    assert_eq!(
        render_inner("<poem>line</poem>"),
        "<p><div class=\"poem\">line</div></p>"
    );
}

#[test]
fn gallery_is_placeholder() {
    assert_eq!(
        render_inner("<gallery>\nFile:A.jpg\n</gallery>"),
        "<div class=\"gallery-placeholder\">[gallery]</div>"
    );
}

#[test]
fn colspan_rowspan_kept() {
    // <td> is block-level (BlockLevelPass), so a bare cell is not <p>-wrapped;
    // the colspan/rowspan attributes survive the sanitizer.
    assert_eq!(
        render_inner("<td colspan=\"2\" rowspan=\"3\">x</td>"),
        "<td colspan=\"2\" rowspan=\"3\">x</td>"
    );
}

#[test]
fn attribute_value_is_escaped() {
    assert_eq!(
        render_inner("<span title=\"a &amp; b\">t</span>"),
        "<p><span title=\"a &amp; b\">t</span></p>"
    );
}

#[test]
fn rtl_wrapper_gets_dir_and_lang() {
    let full = render_inner_opts("مرحبا", "", true);
    assert_eq!(
        full,
        "<div class=\"mw-parser-output\" dir=\"rtl\" lang=\"ar\"><p>مرحبا</p></div>"
    );
}

#[test]
fn ltr_wrapper_has_no_dir() {
    let full = render_inner_opts("hi", "", false);
    assert_eq!(full, "<div class=\"mw-parser-output\"><p>hi</p></div>");
}

#[test]
fn pre_and_code_get_ltr_dir() {
    // Leading-space pre block carries dir="ltr" even under an RTL site.
    let full = render_inner_opts(" code", "", true);
    assert!(full.contains("<pre dir=\"ltr\">code</pre>"), "{full}");
}
