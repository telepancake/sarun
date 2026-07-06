//! Inline formatting: doQuotes apostrophe balancing, nowiki/code/br,
//! HTML entity preservation. Every assertion is exact HTML so a stub that
//! only escapes text cannot pass.

mod common;
use common::*;

#[test]
fn bold_and_italic_basic() {
    assert_eq!(
        render_inner("Hello '''world''' and ''italics''."),
        "<p>Hello <b>world</b> and <i>italics</i>.</p>"
    );
}

#[test]
fn bold_italic_combined_five_apostrophes() {
    assert_eq!(render_inner("'''''both'''''"), "<p><i><b>both</b></i></p>");
}

#[test]
fn italic_wrapping_bold() {
    assert_eq!(
        render_inner("''Italic and '''bold''' inside''"),
        "<p><i>Italic and <b>bold</b> inside</i></p>"
    );
}

#[test]
fn doquotes_single_letter_word_fixup() {
    // MediaWiki's odd-bold/odd-italic rule turns a leading single-letter
    // word's bold-open into an apostrophe + italics.
    assert_eq!(render_inner("L'''''Word"), "<p>L<b><i>Word</i></b></p>");
}

#[test]
fn doquotes_interleaved_state_machine() {
    // Exercises the bi/ib transitions of doQuotes.
    assert_eq!(
        render_inner("a ''b '''c'' d''' e"),
        "<p>a <i>b <b>c</b></i><b> d</b> e</p>"
    );
}

#[test]
fn lone_double_apostrophe_pair_toggles_italic() {
    assert_eq!(render_inner("''x''"), "<p><i>x</i></p>");
}

#[test]
fn triple_apostrophe_toggles_bold() {
    assert_eq!(render_inner("'''x'''"), "<p><b>x</b></p>");
}

#[test]
fn unbalanced_italic_closes_at_line_end() {
    // One unmatched '' opens italics and is auto-closed.
    assert_eq!(render_inner("''open only"), "<p><i>open only</i></p>");
}

#[test]
fn four_apostrophes_are_one_literal_plus_bold() {
    // ''''x''' → apostrophe then bold x.
    assert_eq!(render_inner("''''x'''"), "<p>'<b>x</b></p>");
}

#[test]
fn nowiki_is_literal_no_markup() {
    assert_eq!(
        render_inner("<nowiki>'''not bold''' [[nope]]</nowiki>"),
        "<p>'''not bold''' [[nope]]</p>"
    );
}

#[test]
fn code_tag_passes_through_and_content_is_parsed() {
    assert_eq!(render_inner("<code>x=1</code>"), "<p><code>x=1</code></p>");
}

#[test]
fn br_is_normalized_to_void() {
    assert_eq!(render_inner("line<br>break"), "<p>line<br />break</p>");
}

#[test]
fn br_self_closed_variants() {
    assert_eq!(render_inner("a<br/>b<br />c"), "<p>a<br />b<br />c</p>");
}

#[test]
fn html_entities_are_preserved_bare_amp_escaped() {
    assert_eq!(
        render_inner("A &amp; B &nbsp; C & D"),
        "<p>A &amp; B &nbsp; C &amp; D</p>"
    );
}

#[test]
fn numeric_entity_preserved() {
    assert_eq!(render_inner("x &#39; &#x2764; y"), "<p>x &#39; &#x2764; y</p>");
}

#[test]
fn angle_brackets_that_are_not_tags_are_escaped() {
    assert_eq!(render_inner("a < b and c > d"), "<p>a &lt; b and c &gt; d</p>");
}

#[test]
fn html_comment_removed() {
    assert_eq!(render_inner("<!-- hidden -->shown"), "<p>shown</p>");
}

#[test]
fn ref_tag_becomes_placeholder_not_dropped() {
    assert_eq!(
        render_inner("text <ref>cite here</ref> more"),
        "<p>text <sup class=\"reference\">[ref]</sup> more</p>"
    );
}

#[test]
fn bold_inside_link_label_is_processed() {
    assert_eq!(
        render_inner("[[Berlin|'''big''']]"),
        "<p><a href=\"/wiki/Berlin\"><b>big</b></a></p>"
    );
}
