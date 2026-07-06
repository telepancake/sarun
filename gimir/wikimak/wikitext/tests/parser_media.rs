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
fn image_alias_namespace_resolves() {
    // "Image:" is an alias of the File namespace.
    assert_eq!(
        render_inner("[[Image:Pic.jpg]]"),
        "<p><img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Pic.jpg\"/></p>"
    );
}

#[test]
fn thumb_with_size_align_and_caption() {
    assert_eq!(
        render_inner("[[File:Pic.jpg|thumb|left|120px|A ''caption'' here]]"),
        "<p><div class=\"thumb tleft\"><div class=\"thumbinner\">\
<img src=\"https://media.example/Pic.jpg?w=120\" alt=\"A ''caption'' here\" width=\"120\"/>\
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
        "<p><img src=\"https://media.example/Pic.jpg?w=0\" alt=\"Alt text\"/></p>"
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
