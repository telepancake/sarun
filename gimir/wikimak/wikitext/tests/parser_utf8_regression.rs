//! Regressions for the tag-scanner UTF-8 boundary bugs (adversarial
//! review, 2026-07): `match_open`/`match_ext_tag` byte-sliced a `str` at
//! an ASCII tag-name length that could land inside a multibyte char
//! (whole-render panic on any `<` near a non-ASCII char), and
//! `read_to_close` indexed the original body with a `to_lowercase()`
//! offset (wrong slice on case-length-changing chars).

mod common;
use common::render_full as render_wikitext;

/// A bare `<` near a multibyte char must not panic the renderer.
#[test]
fn lt_near_multibyte_does_not_panic() {
    for input in [
        "hello < 中文文 world",
        "a<€€€ b",
        "x <ref中 y",           // `<` + tag-ish + multibyte at the boundary
        "<referencé/>",         // near-miss tag name with a multibyte tail
    ] {
        let out = render_wikitext(input);
        // No assertion on content — the point is it RETURNS, not panics.
        let _ = out;
    }
}

/// A ref whose body starts with a multibyte char, followed by
/// `<references/>`, renders a footnote without panicking.
#[test]
fn ref_body_multibyte_renders() {
    let out = render_wikitext("A<ref>€x</ref>\n<references/>");
    assert!(out.contains("cite_note-"), "expected a footnote note id, got: {out}");
    assert!(out.contains("€x"), "ref body text should survive: {out}");
}

/// `read_to_close` must not mis-slice a ref body containing a
/// case-length-changing char (Turkish dotted İ). The old code leaked a
/// stray `<` into the note body.
#[test]
fn ref_body_case_length_change_not_missliced() {
    let out = render_wikitext("A<ref>İz</ref> tail\n<references/>");
    // The reference item text is exactly "İz" — no leaked "<" from the
    // close tag, no swallowed "</ref>".
    assert!(out.contains("İz"), "body should contain İz: {out}");
    assert!(!out.contains("İz&lt;"), "stray '<' leaked into ref body: {out}");
    assert!(!out.contains("İz</ref"), "close tag swallowed into body: {out}");
}

/// Longer run of case-changing chars — the old bug swallowed the close
/// tag and trailing text entirely.
#[test]
fn ref_body_many_case_changing_chars() {
    let out = render_wikitext("A<ref>İİİİİİİİ</ref>B\n<references/>");
    assert!(out.contains("B"), "trailing text after </ref> must survive: {out}");
    assert!(!out.contains("İ</ref"), "close tag must not be swallowed: {out}");
}

/// A multibyte char (en-dash) inside an inline table cell must not panic the
/// separator splitter (`split_multi` walked bytes, then sliced `s[i..]`
/// mid-char). Found by the real-page corpus straightedge on de:Schach.
#[test]
fn inline_table_cell_with_endash_does_not_panic() {
    let out = render_wikitext("{|\n! a !! e2–e4 || Bauer zieht von e2 nach e4\n|}");
    assert!(out.contains("e2–e4"), "en-dash cell must render: {out}");
}
