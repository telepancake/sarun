//! Block structure: paragraphs, headings, hr, lists (nested/mixed/dl),
//! preformatted. Exact HTML assertions.

mod common;
use common::*;

#[test]
fn single_paragraph() {
    assert_eq!(render_inner("just text"), "<p>just text</p>");
}

#[test]
fn multiline_paragraph_joins_with_newline() {
    assert_eq!(
        render_inner("line one\nline two"),
        "<p>line one\nline two</p>"
    );
}

#[test]
fn blank_line_separates_paragraphs() {
    assert_eq!(
        render_inner("para one\n\npara two"),
        "<p>para one</p><p>para two</p>"
    );
}

#[test]
fn heading_levels_and_anchors() {
    assert_eq!(
        render_inner("== H2 ==\n=== H3 ==="),
        "<h2 id=\"H2\">H2</h2><h3 id=\"H3\">H3</h3>"
    );
}

#[test]
fn heading_level_one_through_six() {
    assert_eq!(render_inner("= T ="), "<h1 id=\"T\">T</h1>");
    assert_eq!(
        render_inner("====== D ======"),
        "<h6 id=\"D\">D</h6>"
    );
}

#[test]
fn heading_beyond_six_caps_and_keeps_extra_equals() {
    // Seven '=' each side → h6, one stray '=' stays inside the content.
    assert_eq!(
        render_inner("======= seven ======="),
        "<h6 id=\"=_seven_=\">= seven =</h6>"
    );
}

#[test]
fn heading_with_spaces_gets_underscore_anchor() {
    assert_eq!(
        render_inner("== Early life =="),
        "<h2 id=\"Early_life\">Early life</h2>"
    );
}

#[test]
fn horizontal_rule() {
    assert_eq!(render_inner("----"), "<hr />");
}

#[test]
fn horizontal_rule_with_trailing_text() {
    assert_eq!(
        render_inner("----\nafter"),
        "<hr /><p>after</p>"
    );
}

#[test]
fn unordered_list_nested() {
    assert_eq!(
        render_inner("* a\n* b\n** c\n* d"),
        "<ul><li>a</li><li>b<ul><li>c</li></ul></li><li>d</li></ul>"
    );
}

#[test]
fn ordered_list_nested() {
    assert_eq!(
        render_inner("# one\n# two\n## sub"),
        "<ol><li>one</li><li>two<ol><li>sub</li></ol></li></ol>"
    );
}

#[test]
fn mixed_list_types_switch_closes_and_reopens() {
    assert_eq!(
        render_inner("* a\n# b"),
        "<ul><li>a</li></ul><ol><li>b</li></ol>"
    );
}

#[test]
fn definition_list_dt_dd_same_line() {
    assert_eq!(
        render_inner("; term : definition"),
        "<dl><dt>term</dt><dd>definition</dd></dl>"
    );
}

#[test]
fn definition_list_dt_then_dd_lines_share_one_dl() {
    assert_eq!(
        render_inner("; term\n: def"),
        "<dl><dt>term</dt><dd>def</dd></dl>"
    );
}

#[test]
fn indent_colon_is_dd() {
    assert_eq!(render_inner(": indented"), "<dl><dd>indented</dd></dl>");
}

#[test]
fn deeply_mixed_nesting() {
    assert_eq!(
        render_inner("* a\n*# b\n*# c"),
        "<ul><li>a<ol><li>b</li><li>c</li></ol></li></ul>"
    );
}

#[test]
fn leading_space_is_preformatted() {
    assert_eq!(
        render_inner(" code line\n more code"),
        "<pre dir=\"ltr\">code line\nmore code</pre>"
    );
}

#[test]
fn list_interrupts_paragraph() {
    assert_eq!(
        render_inner("intro\n* item"),
        "<p>intro</p><ul><li>item</li></ul>"
    );
}

#[test]
fn category_only_line_produces_no_paragraph() {
    assert_eq!(render_inner("[[Category:X]]"), "");
}
