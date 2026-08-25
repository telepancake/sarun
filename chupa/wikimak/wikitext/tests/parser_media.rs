//! File/image markup: plain, thumb, sizing, alignment, caption, alt,
//! resolver miss → placeholder + counted media miss.

mod common;
use common::*;

#[test]
fn plain_file_is_inline_image() {
    assert_eq!(
        render_inner("[[File:Pic.jpg]]"),
        "<p><img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Pic.jpg\"/></p>"
    );
}

#[test]
fn ogg_file_is_an_audio_control_not_an_image() {
    assert_eq!(
        render_inner("[[File:National Anthem.ogg]]"),
        "<p><audio controls src=\"https://media.example/National Anthem.ogg?w=0\"></audio></p>"
    );
}

#[test]
fn image_alias_namespace_resolves() {
    // "Image:" is an alias of the File namespace.
    assert_eq!(
        render_inner("[[Image:Pic.jpg]]"),
        "<p><img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Pic.jpg\"/></p>"
    );
}

#[test]
fn percent_encoded_file_target_is_decoded_before_resolution() {
    assert_eq!(
        render_inner("[[File:R%C4%ABga%20view.jpg]]"),
        "<p><img src=\"https://media.example/Rīga view.jpg?w=0\" alt=\"Rīga view.jpg\"/></p>"
    );
}

#[test]
fn thumb_with_size_align_and_caption() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|thumb|left|120px|A ''caption'' here]]"),
        "<p><div class=\"thumb tleft\"><div class=\"thumbinner\">\
<img src=\"https://media.example/Pic.jpg?w=120\" alt=\"A caption here\" width=\"120\"/>\
<div class=\"thumbcaption\">A <i>caption</i> here</div></div></div></p>"
    );
}

#[test]
fn thumb_default_width_is_requested() {
    // No explicit px on a thumb → the 220px render bucket is requested,
    // visible in the resolver's echoed `?w=`.
    assert_eq!(
        render_inner("[[File:Pic.jpg|thumb]]"),
        "<p><div class=\"thumb tright\"><div class=\"thumbinner\">\
<img src=\"https://media.example/Pic.jpg?w=220\" alt=\"Pic.jpg\"/>\
<div class=\"thumbcaption\"></div></div></div></p>"
    );
}

#[test]
fn explicit_alt_used() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|alt=Alt text|caption words]]"),
        "<p><img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Alt text\" title=\"caption words\"/></p>"
    );
}

#[test]
fn missing_media_is_placeholder_and_counted() {
    let out = render_out("[[File:Missing.jpg|thumb|Nope]]");
    assert_eq!(
        out.html,
        "<div class=\"mw-parser-output\"><p><div class=\"thumb tright\"><div class=\"thumbinner\">\
<span class=\"image-placeholder\">[File: Missing.jpg]</span>\
<div class=\"thumbcaption\">Nope</div></div></div></p></div>"
    );
    assert_eq!(out.misses.missing_media, vec!["File:Missing.jpg".to_string()]);
}

#[test]
fn repeated_missing_media_is_aggregated_with_count() {
    let out = render_out("[[File:Missing.jpg]][[File:Missing.jpg]]");
    assert_eq!(
        out.misses.missing_media,
        vec!["File:Missing.jpg ×2".to_string()]
    );
}

#[test]
fn framed_image_is_boxed_without_default_width() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|frame|Cap]]"),
        "<p><div class=\"thumb tright\"><div class=\"thumbinner\">\
<img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Cap\"/>\
<div class=\"thumbcaption\">Cap</div></div></div></p>"
    );
}

#[test]
fn right_aligned_inline_image() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|right]]"),
        "<p><span class=\"floatright\"><img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Pic.jpg\"/></span></p>"
    );
}

#[test]
fn image_caption_markup_is_rendered_and_used_as_plain_alt() {
    assert_eq!(
        render_inner(
            "[[File:Pic.jpg|thumb|Caption with [[Berlin|Berlin link]] and [https://example.test label] and ''emphasis'']]"
        ),
        "<p><div class=\"thumb tright\"><div class=\"thumbinner\">\
<img src=\"https://media.example/Pic.jpg?w=220\" alt=\"Caption with Berlin link and label and emphasis\"/>\
<div class=\"thumbcaption\">Caption with <a href=\"/wiki/Berlin\" title=\"Berlin\">Berlin link</a> and <a rel=\"nofollow\" class=\"external text\" href=\"https://example.test\">label</a> and <i>emphasis</i></div></div></div></p>"
    );
}

#[test]
fn image_alt_markup_does_not_close_the_outer_image_link() {
    assert_eq!(
        render_inner(
            "[[File:Pic.jpg|frameless|border|160x160px|alt=[[Berlin|Berlin link]] and [https://example.test label]]]"
        ),
        "<p><img src=\"https://media.example/Pic.jpg?w=160\" alt=\"Berlin link and label\" width=\"160\"/></p>"
    );
}

#[test]
fn frameless_caption_becomes_plain_image_title() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|frameless|border|Caption [[Berlin|Berlin link]]]]"),
        "<p><img src=\"https://media.example/Pic.jpg?w=220\" alt=\"Caption Berlin link\" title=\"Caption Berlin link\"/></p>"
    );
}

#[test]
fn pipe_inside_external_caption_link_is_not_an_image_option() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|frameless|[https://example.test label|with pipe]]]"),
        "<p><img src=\"https://media.example/Pic.jpg?w=220\" alt=\"label|with pipe\" title=\"label|with pipe\"/></p>"
    );
}

#[test]
fn image_attributes_preserve_entities_without_double_escaping_ampersands() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|frameless|A &amp; B & C &eacute;]]"),
        "<p><img src=\"https://media.example/Pic.jpg?w=220\" alt=\"A &amp; B &amp; C &#233;\" title=\"A &amp; B &amp; C &#233;\"/></p>"
    );
}

#[test]
fn explicit_alt_and_caption_are_rendered_independently() {
    assert_eq!(
        render_inner(
            "[[File:Pic.jpg|frameless|alt=Alt ''text''|Caption <b>shown</b> & details]]"
        ),
        "<p><img src=\"https://media.example/Pic.jpg?w=220\" alt=\"Alt text\" title=\"Caption shown &amp; details\"/></p>"
    );
}

#[test]
fn nested_internal_link_inside_external_caption_keeps_pipes_and_brackets_scoped() {
    assert_eq!(
        render_inner(
            "[[File:Pic.jpg|frameless|[https://example.test [[Berlin|Berlin link]]|tail]]]"
        ),
        "<p><img src=\"https://media.example/Pic.jpg?w=220\" alt=\"Berlin link|tail\" title=\"Berlin link|tail\"/></p>"
    );
}

#[test]
fn malformed_image_brackets_do_not_swallow_a_later_image() {
    let out = render_inner("[[File:Broken.jpg|frameless|broken [caption [[File:Pic.jpg]]");
    assert!(out.contains("[[File:Broken.jpg|frameless|broken [caption "), "{out}");
    assert!(
        out.contains("<img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Pic.jpg\"/>")
    );
}
