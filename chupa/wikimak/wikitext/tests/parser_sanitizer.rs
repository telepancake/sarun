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
fn adjacent_quoted_attributes_are_accepted() {
    assert_eq!(
        render_inner(
            "<th colspan=\"3\" align=\"center\" class=\"mergedtoprow\"style=\"padding:0.25em\">x</th>"
        ),
        "<th colspan=\"3\" align=\"center\" class=\"mergedtoprow\" style=\"padding:0.25em\">x</th>"
    );
}

#[test]
fn allowed_tag_may_span_source_lines() {
    assert_eq!(
        render_inner(
            "<tr class=\"adr\">\n <th colspan=\"3\" align=\"center\" class=\"mergedtoprow\"\n style=\"padding:0.25em\">x</th></tr>"
        ),
        "<tr class=\"adr\"><th colspan=\"3\" align=\"center\" class=\"mergedtoprow\" style=\"padding:0.25em\">x</th></tr>"
    );
}

#[test]
fn quotation_tag_is_allowed() {
    assert_eq!(render_inner("<q>quoted</q>"), "<p><q>quoted</q></p>");
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
fn indented_block_html_is_not_preformatted() {
    assert_eq!(
        render_inner(" <div>one</div>\n <div>two</div>"),
        "<div>one</div><div>two</div>"
    );
}

#[test]
fn unknown_tag_is_escaped_and_counted() {
    let out = render_out("<blink>x</blink>");
    assert_eq!(out.html, "<div class=\"mw-parser-output\"><p>&lt;blink&gt;x&lt;/blink&gt;</p></div>");
    assert_eq!(out.misses.unknown_tags, vec!["blink ×2".to_string()]);
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
fn gallery_renders_two_entries_with_caption_links_and_width_hint() {
    assert_eq!(
        render_inner(
            "<gallery widths=180 heights=120>\nA.jpg|Caption with [[Berlin|Berlin link]]\nFile:B.jpg|Second ''caption''\n</gallery>"
        ),
        "<ul class=\"gallery mw-gallery-default\"><li class=\"gallerybox\"><div class=\"thumb\"><img src=\"https://media.example/A.jpg?w=180\" alt=\"A.jpg\"/></div><div class=\"gallerytext\">Caption with <a href=\"/wiki/Berlin\" title=\"Berlin\">Berlin link</a></div></li><li class=\"gallerybox\"><div class=\"thumb\"><img src=\"https://media.example/B.jpg?w=180\" alt=\"B.jpg\"/></div><div class=\"gallerytext\">Second <i>caption</i></div></li></ul>"
    );
}

#[test]
fn gallery_without_dimensions_uses_default_thumbnail_hint() {
    assert_eq!(
        render_inner("<gallery>\nA.jpg\n</gallery>"),
        "<ul class=\"gallery mw-gallery-default\"><li class=\"gallerybox\"><div class=\"thumb\"><img src=\"https://media.example/A.jpg?w=220\" alt=\"A.jpg\"/></div><div class=\"gallerytext\"></div></li></ul>"
    );
}

#[test]
fn gallery_uses_height_hint_and_counts_only_valid_missing_media() {
    let out = render_out(
        "<gallery heights=96>\nPresent.jpg|Shown\nMissing.jpg|Unavailable\n|orphan caption\n[[Not a file]]\n</gallery>",
    );
    assert_eq!(
        out.html,
        "<div class=\"mw-parser-output\"><ul class=\"gallery mw-gallery-default\"><li class=\"gallerybox\"><div class=\"thumb\"><img src=\"https://media.example/Present.jpg?w=96\" alt=\"Present.jpg\"/></div><div class=\"gallerytext\">Shown</div></li><li class=\"gallerybox\"><div class=\"thumb\"><span class=\"image-placeholder\">[File: Missing.jpg]</span></div><div class=\"gallerytext\">Unavailable</div></li></ul></div>"
    );
    assert_eq!(
        out.misses.missing_media,
        vec!["File:Missing.jpg".to_string()]
    );
}

#[test]
fn inputbox_is_replaced_for_the_read_only_archive() {
    let out = render_out("<inputbox>\ntype=create\n</inputbox>");
    assert!(out.html.contains("Page creation is unavailable"), "{}", out.html);
    assert!(out.misses.unknown_tags.is_empty(), "{:?}", out.misses);
}

#[test]
fn indicator_is_metadata_not_visible_body_text() {
    let out = render_out("<indicator name=\"featured\">[[File:Star.svg]]</indicator>Body");
    assert_eq!(out.html, "<div class=\"mw-parser-output\"><p>Body</p></div>");
    assert!(out.misses.unknown_tags.is_empty());
}

#[test]
fn graph_and_imagemap_have_safe_deduplicated_fallbacks() {
    let out = render_out(
        "<graph>{\"data\":[]}</graph>\n<graph>x</graph>\n<imagemap>File:Map.png</imagemap>",
    );
    assert_eq!(
        out.misses.unknown_tags,
        vec!["graph ×2".to_string(), "imagemap".to_string()]
    );
    assert_eq!(out.html.matches("graph-fallback").count(), 2);
    assert_eq!(out.html.matches("imagemap-fallback").count(), 1);
    assert!(!out.html.contains("&lt;graph"));
    assert!(!out.html.contains("&lt;imagemap"));
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
