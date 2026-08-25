//! Tokenizer edge cases: brace disambiguation, links protecting pipes,
//! comment stripping (newline-eating), <nowiki>/<pre> protection, and
//! #REDIRECT parsing.

#[path = "preprocess_common/mod.rs"]
mod common;
use common::*;
use wikimak_wikitext::parse_redirect;

#[test]
fn brace_disambiguation_arg_vs_template() {
    let mut s = MockStore::new();
    s.template("T", "TPL");
    // {{{x}}} with no frame arg and no default → literal triple-brace.
    assert_eq!(xt(&s, "{{{x}}}"), "{{{x}}}");
    // {{T}} is a template.
    assert_eq!(xt(&s, "{{T}}"), "TPL");
    // Arg with default resolves.
    s.template("D", "{{{1|def}}}");
    assert_eq!(xt(&s, "{{D}}"), "def");
}

#[test]
fn pipe_inside_link_is_not_an_arg_separator() {
    let mut s = MockStore::new();
    // The [[a|b]] pipe must not split the template argument.
    s.template("Echo", "<{{{1}}}>");
    assert_eq!(xt(&s, "{{Echo|[[Page|label]]}}"), "<[[Page|label]]>");
}

#[test]
fn template_inside_link_expands() {
    let mut s = MockStore::new();
    s.template("Cap", "CAPTION");
    assert_eq!(xt(&s, "[[File:X.png|{{Cap}}]]"), "[[File:X.png|CAPTION]]");
}

#[test]
fn unmatched_open_brace_is_literal_but_inner_expands() {
    let mut s = MockStore::new();
    s.template("B", "BB");
    // Outer {{ never closes; the complete inner {{B}} still expands.
    assert_eq!(xt(&s, "{{foo|{{B}}"), "{{foo|BB");
}

#[test]
fn comment_stripped_inline() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "a<!-- hidden -->b"), "ab");
}

#[test]
fn comment_whole_line_eats_the_line() {
    let s = MockStore::new();
    // A comment that fills a line (with surrounding ws) removes the line.
    assert_eq!(xt(&s, "x\n <!-- c --> \ny"), "x\ny");
}

#[test]
fn unclosed_comment_removed_to_end() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "keep<!-- runs off"), "keep");
}

#[test]
fn nowiki_protects_braces_from_expansion() {
    let mut s = MockStore::new();
    s.template("T", "EXPANDED");
    // Inside <nowiki>, the template call is NOT expanded and survives verbatim.
    assert_eq!(
        xt(&s, "<nowiki>{{T}}</nowiki> {{T}}"),
        "<nowiki>{{T}}</nowiki> EXPANDED"
    );
}

#[test]
fn pre_protects_content() {
    let mut s = MockStore::new();
    s.template("T", "X");
    assert_eq!(xt(&s, "<pre>{{T}}</pre>"), "<pre>{{T}}</pre>");
}

#[test]
fn comment_inside_template_arg() {
    let mut s = MockStore::new();
    s.template("Echo", "[{{{1}}}]");
    assert_eq!(xt(&s, "{{Echo|a<!--x-->b}}"), "[ab]");
}

#[test]
fn redirect_english() {
    assert_eq!(
        parse_redirect("#REDIRECT [[Target Page]]"),
        Some("Target Page".to_string())
    );
    assert_eq!(
        parse_redirect("#redirect[[Other]]"),
        Some("Other".to_string())
    );
}

#[test]
fn redirect_strips_section_and_label() {
    assert_eq!(
        parse_redirect("#REDIRECT [[Page#Section]]"),
        Some("Page".to_string())
    );
    assert_eq!(
        parse_redirect("#REDIRECT [[Page|shown]]"),
        Some("Page".to_string())
    );
    assert_eq!(
        parse_redirect("#REDIRECT [[:Category:Foo]]"),
        Some("Category:Foo".to_string())
    );
}

#[test]
fn non_redirect_returns_none() {
    assert_eq!(parse_redirect("Just an article.\n#REDIRECT later"), None);
    assert_eq!(parse_redirect("#REDIRECT no link here"), None);
}

#[test]
fn expansion_inside_link_and_pf_together() {
    let mut s = MockStore::new();
    s.add(0, "Yes Page", "e");
    // #ifexist chooses a link target; the link pipe stays intact.
    assert_eq!(
        xt(&s, "[[{{#ifexist:Yes Page|Yes Page|Fallback}}|text]]"),
        "[[Yes Page|text]]"
    );
}
