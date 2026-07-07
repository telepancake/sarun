//! Cite extension (`<ref>` / `<references>`) — browsing plan §3.5. Exact
//! input→HTML assertions for footnote numbering, named-ref reuse, groups,
//! auto-appended lists, nested markup, malformed input, and XSS escaping.
//! These pin real parser output end to end (through `render`), never a stub.

mod common;
use common::*;

#[test]
fn single_ref_defines_note_and_list() {
    assert_eq!(
        render_inner("Fact.<ref>Source A</ref>\n<references/>"),
        "<p>Fact.<sup class=\"reference\" id=\"cite_ref-1\">\
         <a href=\"#cite_note-1\">[1]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-1\">\
         <span class=\"mw-cite-backlink\"><a href=\"#cite_ref-1\">^</a></span> \
         <span class=\"reference-text\">Source A</span></li></ol>"
    );
}

#[test]
fn two_refs_number_sequentially() {
    assert_eq!(
        render_inner("A<ref>One</ref> B<ref>Two</ref>\n<references/>"),
        "<p>A<sup class=\"reference\" id=\"cite_ref-1\">\
         <a href=\"#cite_note-1\">[1]</a></sup> \
         B<sup class=\"reference\" id=\"cite_ref-2\">\
         <a href=\"#cite_note-2\">[2]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-1\">\
         <span class=\"mw-cite-backlink\"><a href=\"#cite_ref-1\">^</a></span> \
         <span class=\"reference-text\">One</span></li>\
         <li id=\"cite_note-2\">\
         <span class=\"mw-cite-backlink\"><a href=\"#cite_ref-2\">^</a></span> \
         <span class=\"reference-text\">Two</span></li></ol>"
    );
}

#[test]
fn named_ref_reuse_shares_number_and_backlinks() {
    // Define once, reuse twice: one note, three uses, back-links ^ a b c.
    assert_eq!(
        render_inner("A<ref name=\"s\">Shared</ref> B<ref name=\"s\"/> C<ref name=\"s\"/>\n<references/>"),
        "<p>A<sup class=\"reference\" id=\"cite_ref-1-0\">\
         <a href=\"#cite_note-1\">[1]</a></sup> \
         B<sup class=\"reference\" id=\"cite_ref-1-1\">\
         <a href=\"#cite_note-1\">[1]</a></sup> \
         C<sup class=\"reference\" id=\"cite_ref-1-2\">\
         <a href=\"#cite_note-1\">[1]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-1\">\
         <span class=\"mw-cite-backlink\">^ \
         <a href=\"#cite_ref-1-0\">a</a> \
         <a href=\"#cite_ref-1-1\">b</a> \
         <a href=\"#cite_ref-1-2\">c</a></span> \
         <span class=\"reference-text\">Shared</span></li></ol>"
    );
}

#[test]
fn reuse_before_definition_still_resolves() {
    // A name may be reused before it is defined; the definition's content
    // fills the note, and both occurrences share number 1.
    assert_eq!(
        render_inner("X<ref name=\"s\"/> Y<ref name=\"s\">Later</ref>\n<references/>"),
        "<p>X<sup class=\"reference\" id=\"cite_ref-1-0\">\
         <a href=\"#cite_note-1\">[1]</a></sup> \
         Y<sup class=\"reference\" id=\"cite_ref-1-1\">\
         <a href=\"#cite_note-1\">[1]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-1\">\
         <span class=\"mw-cite-backlink\">^ \
         <a href=\"#cite_ref-1-0\">a</a> \
         <a href=\"#cite_ref-1-1\">b</a></span> \
         <span class=\"reference-text\">Later</span></li></ol>"
    );
}

#[test]
fn group_refs_number_and_label_separately() {
    assert_eq!(
        render_inner("Note.<ref group=\"n\">Grouped</ref>\n<references group=\"n\"/>"),
        "<p>Note.<sup class=\"reference\" id=\"cite_ref-n-1\">\
         <a href=\"#cite_note-n-1\">[n 1]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-n-1\">\
         <span class=\"mw-cite-backlink\"><a href=\"#cite_ref-n-1\">^</a></span> \
         <span class=\"reference-text\">Grouped</span></li></ol>"
    );
}

#[test]
fn group_and_default_do_not_cross_contaminate() {
    // A default ref and a group ref each get number 1 in their own space,
    // with two separate lists (default list first — first-seen order).
    let out = render_inner(
        "D<ref>Def</ref> G<ref group=\"n\">Grp</ref>\n<references/>\n<references group=\"n\"/>",
    );
    assert!(
        out.contains("<sup class=\"reference\" id=\"cite_ref-1\"><a href=\"#cite_note-1\">[1]</a></sup>"),
        "default ref numbered 1: {out}"
    );
    assert!(
        out.contains("<sup class=\"reference\" id=\"cite_ref-n-1\"><a href=\"#cite_note-n-1\">[n 1]</a></sup>"),
        "group ref numbered [n 1]: {out}"
    );
    assert!(
        out.contains("<span class=\"reference-text\">Def</span>")
            && out.contains("<span class=\"reference-text\">Grp</span>"),
        "both notes rendered: {out}"
    );
}

#[test]
fn ref_without_references_auto_appends_list_and_records_miss() {
    let out = render_out("Claim.<ref>Evidence</ref>");
    assert_eq!(
        out.html,
        "<div class=\"mw-parser-output\">\
         <p>Claim.<sup class=\"reference\" id=\"cite_ref-1\">\
         <a href=\"#cite_note-1\">[1]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-1\">\
         <span class=\"mw-cite-backlink\"><a href=\"#cite_ref-1\">^</a></span> \
         <span class=\"reference-text\">Evidence</span></li></ol></div>"
    );
    assert!(
        out.misses
            .failed_invokes
            .iter()
            .any(|m| m.contains("no <references/>")),
        "auto-append records a miss: {:?}",
        out.misses.failed_invokes
    );
}

#[test]
fn nested_markup_inside_ref_is_rendered() {
    assert_eq!(
        render_inner("X<ref>See ''Foo'' and [[Berlin]]</ref>\n<references/>"),
        "<p>X<sup class=\"reference\" id=\"cite_ref-1\">\
         <a href=\"#cite_note-1\">[1]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-1\">\
         <span class=\"mw-cite-backlink\"><a href=\"#cite_ref-1\">^</a></span> \
         <span class=\"reference-text\">See <i>Foo</i> and \
         <a href=\"/wiki/Berlin\">Berlin</a></span></li></ol>"
    );
}

#[test]
fn empty_ref_without_name_is_inline_error() {
    let out = render_out("Oops<ref></ref>");
    assert_eq!(
        out.html,
        "<div class=\"mw-parser-output\"><p>Oops\
         <span class=\"error mw-ext-cite-error\">Cite error: \
         &lt;ref&gt; with no content and no name</span></p></div>"
    );
    // No note was created ⇒ no auto-appended list.
    assert!(!out.html.contains("<ol class=\"references\">"), "{}", out.html);
    assert!(
        out.misses.failed_invokes.iter().any(|m| m == "cite: empty <ref>"),
        "{:?}",
        out.misses.failed_invokes
    );
}

#[test]
fn redefinition_keeps_first_content_and_records_miss() {
    // Same name, two bodies: first definition wins, second is a miss but
    // still a use (so the note back-links to both).
    let out = render_out(
        "A<ref name=\"d\">First</ref> B<ref name=\"d\">Second</ref>\n<references/>",
    );
    assert!(
        out.html.contains("<span class=\"reference-text\">First</span>"),
        "first definition wins: {}",
        out.html
    );
    assert!(!out.html.contains("Second"), "second body dropped: {}", out.html);
    assert!(
        out.misses
            .failed_invokes
            .iter()
            .any(|m| m.contains("redefinition of ref name \"d\"")),
        "{:?}",
        out.misses.failed_invokes
    );
}

#[test]
fn malformed_unclosed_ref_does_not_panic() {
    // No closing </ref>, no <references/>: must terminate, not loop/panic.
    let out = render_inner("Bad<ref>unclosed forever");
    assert!(
        out.contains("<sup class=\"reference\"") && out.contains("<ol class=\"references\">"),
        "{out}"
    );
    assert!(out.contains("unclosed forever"), "body preserved: {out}");
}

#[test]
fn nested_ref_inside_ref_does_not_recurse_or_panic() {
    // Inner <ref> is escaped by the sanitizer, never re-parsed as a ref.
    let out = render_inner("Q<ref>outer<ref>inner</ref>tail</ref> rest\n<references/>");
    assert!(out.contains("&lt;ref&gt;inner"), "inner ref escaped: {out}");
    assert!(out.contains("<ol class=\"references\">"), "{out}");
    // The stray </ref> and trailing text survive as body, no panic.
    assert!(out.contains("rest"), "{out}");
}

#[test]
fn xss_in_ref_body_is_escaped() {
    let out = render_inner("Danger<ref><script>alert('xss')</script></ref>\n<references/>");
    assert_eq!(
        out,
        "<p>Danger<sup class=\"reference\" id=\"cite_ref-1\">\
         <a href=\"#cite_note-1\">[1]</a></sup></p>\
         <ol class=\"references\">\
         <li id=\"cite_note-1\">\
         <span class=\"mw-cite-backlink\"><a href=\"#cite_ref-1\">^</a></span> \
         <span class=\"reference-text\">&lt;script&gt;alert('xss')&lt;/script&gt;\
         </span></li></ol>"
    );
    // Hard guarantee: no live <script> tag anywhere in the output.
    assert!(!out.contains("<script>"), "no unescaped script: {out}");
}

#[test]
fn list_defined_reference_supplies_content() {
    // <references> body defines the content for a name used in the article.
    let out = render_inner(
        "See.<ref name=\"b\"/>\n<references>\n<ref name=\"b\">Book body</ref>\n</references>",
    );
    assert!(
        out.contains("<span class=\"reference-text\">Book body</span>"),
        "LDR content filled: {out}"
    );
    assert!(
        out.contains("<sup class=\"reference\" id=\"cite_ref-1\"><a href=\"#cite_note-1\">[1]</a></sup>"),
        "inline marker present: {out}"
    );
}
