//! Template transclusion: argument substitution, inclusion tags, loop and
//! depth limits, missing templates, subst-irrelevance.

#[path = "preprocess_common/mod.rs"]
mod common;
use common::*;
use wikimak_wikitext::preprocess::expand;

#[test]
fn simple_transclusion() {
    let mut s = MockStore::new();
    s.template("Greeting", "Hello {{{1}}}!");
    assert_eq!(xt(&s, "{{Greeting|World}}"), "Hello World!");
}

#[test]
fn positional_and_named_args() {
    let mut s = MockStore::new();
    s.template("P", "{{{1}}}-{{{a}}}-{{{2}}}");
    assert_eq!(xt(&s, "{{P|one|a=named|two}}"), "one-named-two");
}

#[test]
fn positional_value_keeps_whitespace_named_is_trimmed() {
    let mut s = MockStore::new();
    s.template("Pos", "[{{{1}}}]");
    s.template("Nam", "[{{{k}}}]");
    assert_eq!(xt(&s, "{{Pos| spaced }}"), "[ spaced ]");
    assert_eq!(xt(&s, "{{Nam|k= spaced }}"), "[spaced]");
}

#[test]
fn arg_default_and_missing() {
    let mut s = MockStore::new();
    s.template("D", "{{{1|fallback}}}");
    s.template("M", "[{{{x}}}]");
    assert_eq!(xt(&s, "{{D}}"), "fallback");
    assert_eq!(xt(&s, "{{D|given}}"), "given");
    // No arg, no default → the literal triple-brace name survives.
    assert_eq!(xt(&s, "{{M}}"), "[{{{x}}}]");
}

#[test]
fn nested_templates_expand_inside_out() {
    let mut s = MockStore::new();
    s.template("Outer", "<{{Inner}}>");
    s.template("Inner", "IN");
    assert_eq!(xt(&s, "{{Outer}}"), "<IN>");
}

#[test]
fn arg_passed_through_to_child() {
    let mut s = MockStore::new();
    s.template("A", "{{B|x={{{1}}}}}");
    s.template("B", "<{{{x}}}>");
    assert_eq!(xt(&s, "{{A|VAL}}"), "<VAL>");
}

#[test]
fn includeonly_noinclude_on_transclusion() {
    let mut s = MockStore::new();
    s.template("T", "A<includeonly>B</includeonly><noinclude>C</noinclude>");
    // Transcluded: includeonly kept, noinclude dropped.
    assert_eq!(xt(&s, "{{T}}"), "AB");
    // Page view of the template itself: includeonly dropped, noinclude kept.
    let pv = ex_on(&s, &title(10, "T"), "A<includeonly>B</includeonly><noinclude>C</noinclude>");
    assert_eq!(pv.text, "AC");
}

#[test]
fn onlyinclude_restricts_transclusion() {
    let mut s = MockStore::new();
    let body = "X<onlyinclude>Y</onlyinclude>Z";
    s.template("O", body);
    // Transcluded: ONLY the onlyinclude section.
    assert_eq!(xt(&s, "{{O}}"), "Y");
    // Page view: tags stripped, everything shown.
    let pv = ex_on(&s, &title(10, "O"), body);
    assert_eq!(pv.text, "XYZ");
}

#[test]
fn onlyinclude_multiple_sections_concatenate() {
    let mut s = MockStore::new();
    s.template("O", "a<onlyinclude>1</onlyinclude>b<onlyinclude>2</onlyinclude>c");
    assert_eq!(xt(&s, "{{O}}"), "12");
}

#[test]
fn missing_template_is_red_link_and_counted() {
    let s = MockStore::new();
    let out = ex(&s, "before {{Nope}} after");
    assert_eq!(out.text, "before [[Template:Nope]] after");
    assert_eq!(out.misses.missing_templates, vec!["Template:Nope".to_string()]);
}

#[test]
fn missing_mainspace_transclusion_red_link() {
    let s = MockStore::new();
    let out = ex(&s, "{{:Missing Page}}");
    assert_eq!(out.text, "[[:Missing Page]]");
    assert_eq!(out.misses.missing_templates, vec!["Missing Page".to_string()]);
}

#[test]
fn template_loop_detected() {
    let mut s = MockStore::new();
    s.template("Loop", "start {{Loop}} end");
    let out = ex(&s, "{{Loop}}");
    assert!(
        out.text.contains("Template loop detected"),
        "got: {}",
        out.text
    );
    assert!(out.text.contains("[[Template:Loop]]"));
}

#[test]
fn mutual_loop_detected() {
    let mut s = MockStore::new();
    s.template("Ping", "{{Pong}}");
    s.template("Pong", "{{Ping}}");
    let out = ex(&s, "{{Ping}}");
    assert!(out.text.contains("Template loop detected"), "got: {}", out.text);
}

#[test]
fn depth_limit_stops_deep_recursion() {
    let mut s = MockStore::new();
    for i in 0..60 {
        s.template(&format!("T{i}"), &format!("{{{{T{}}}}}", i + 1));
    }
    s.template("T60", "END");
    let out = ex(&s, "{{T0}}");
    // The chain is cut by the depth guard before ever reaching END.
    assert!(!out.text.contains("END"), "got: {}", out.text);
    assert!(
        out.text.contains("depth limit exceeded"),
        "got: {}",
        out.text
    );
}

#[test]
fn subst_and_safesubst_are_transparent() {
    let mut s = MockStore::new();
    s.template("G", "Hi {{{1}}}");
    assert_eq!(xt(&s, "{{subst:G|there}}"), "Hi there");
    assert_eq!(xt(&s, "{{safesubst:G|you}}"), "Hi you");
}

#[test]
fn explicit_namespace_transclusion() {
    let mut s = MockStore::new();
    s.add(2, "Sig", "a userpage template");
    assert_eq!(xt(&s, "{{User:Sig}}"), "a userpage template");
}

#[test]
fn underscore_and_case_normalization_on_lookup() {
    let mut s = MockStore::new();
    s.template("Foo bar", "matched");
    // Lower-first + underscores must normalize to the stored key.
    assert_eq!(xt(&s, "{{foo_bar}}"), "matched");
}

#[test]
fn expanded_reports_misses_struct_default_when_clean() {
    let mut s = MockStore::new();
    s.template("Ok", "fine");
    let out = expand(&s, &title(0, "Test"), "{{Ok}}", &opts());
    assert!(out.misses.missing_templates.is_empty());
    assert!(out.misses.failed_invokes.is_empty());
}
