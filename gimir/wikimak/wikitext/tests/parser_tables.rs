//! Tables: {| |+ |- ! | syntax, attributes, inline !!/|| separators,
//! cell attributes, nesting.

mod common;
use common::*;

#[test]
fn simple_table_one_cell() {
    assert_eq!(
        render_inner("{|\n| x\n|}"),
        "<table><tr><td>x</td></tr></table>"
    );
}

#[test]
fn table_attributes_sanitized() {
    assert_eq!(
        render_inner("{| class=\"wikitable\" onmouseover=\"x\"\n| a\n|}"),
        "<table class=\"wikitable\"><tr><td>a</td></tr></table>"
    );
}

#[test]
fn caption_headers_rows_and_inline_cells() {
    assert_eq!(
        render_inner("{| class=\"wikitable\"\n|+ Cap\n! H1 !! H2\n|-\n| a || b\n|}"),
        "<table class=\"wikitable\"><caption>Cap</caption>\
<tr><th>H1</th><th>H2</th></tr>\
<tr><td>a</td><td>b</td></tr></table>"
    );
}

#[test]
fn cell_with_attributes() {
    assert_eq!(
        render_inner("{|\n| class=\"c\" | data\n|}"),
        "<table><tr><td class=\"c\">data</td></tr></table>"
    );
}

#[test]
fn cell_pipe_without_equals_is_not_attributes() {
    // No '=' in the left part → the whole thing is content (with the pipe).
    assert_eq!(
        render_inner("{|\n| a | b\n|}"),
        "<table><tr><td>a | b</td></tr></table>"
    );
}

#[test]
fn header_cell_attributes() {
    assert_eq!(
        render_inner("{|\n! scope=\"col\" style=\"width:5em\" | Name\n|}"),
        "<table><tr><th style=\"width:5em\">Name</th></tr></table>"
    );
}

#[test]
fn links_inside_cells() {
    assert_eq!(
        render_inner("{|\n| [[Berlin]] || '''bold'''\n|}"),
        "<table><tr><td><a href=\"/wiki/Berlin\">Berlin</a></td><td><b>bold</b></td></tr></table>"
    );
}

#[test]
fn implicit_first_row_before_any_row_marker() {
    assert_eq!(
        render_inner("{|\n| a\n| b\n|}"),
        "<table><tr><td>a</td><td>b</td></tr></table>"
    );
}

#[test]
fn nested_table_in_cell() {
    let out = render_inner("{|\n| outer\n{|\n| inner\n|}\n|}");
    assert_eq!(
        out,
        "<table><tr><td>outer<table><tr><td>inner</td></tr></table></td></tr></table>"
    );
}

#[test]
fn row_attributes() {
    assert_eq!(
        render_inner("{|\n|- class=\"r\"\n| a\n|}"),
        "<table><tr class=\"r\"><td>a</td></tr></table>"
    );
}
