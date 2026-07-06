//! Title normalization: namespace resolution (canonical + alias,
//! case-insensitive), underscore/whitespace collapse, first-letter case
//! rule, leading colon, fragment split.

mod common;
use common::*;
use wikimak_wikitext::Title;

#[test]
fn mainspace_first_letter_uppercased_underscores_spaced() {
    let t = Title::parse("foo_bar", &site());
    assert_eq!(t.ns, 0);
    assert_eq!(t.text, "Foo bar");
}

#[test]
fn whitespace_collapsed() {
    let t = Title::parse("  Foo   bar  ", &site());
    assert_eq!(t.text, "Foo bar");
}

#[test]
fn namespace_canonical_resolved() {
    let t = Title::parse("Template:Infobox", &site());
    assert_eq!(t.ns, 10);
    assert_eq!(t.text, "Infobox");
}

#[test]
fn namespace_alias_resolved_case_insensitive() {
    let t = Title::parse("image:photo.png", &site());
    assert_eq!(t.ns, 6);
    assert_eq!(t.text, "Photo.png");
}

#[test]
fn namespace_prefix_case_insensitive() {
    let t = Title::parse("CATEGORY:Birds", &site());
    assert_eq!(t.ns, 14);
    assert_eq!(t.text, "Birds");
}

#[test]
fn unknown_prefix_is_mainspace_with_colon() {
    let t = Title::parse("SomeThing:Value", &site());
    assert_eq!(t.ns, 0);
    assert_eq!(t.text, "SomeThing:Value");
}

#[test]
fn leading_colon_consumed_forces_mainspace_resolution() {
    let t = Title::parse(":Berlin", &site());
    assert_eq!(t.ns, 0);
    assert_eq!(t.text, "Berlin");
}

#[test]
fn leading_colon_still_resolves_namespace() {
    // Leading colon is a link-suppression marker, not a namespace override.
    let t = Title::parse(":Category:Birds", &site());
    assert_eq!(t.ns, 14);
    assert_eq!(t.text, "Birds");
}

#[test]
fn fragment_split_off_and_not_in_text() {
    let (t, frag) = Title::parse_parts("Berlin#History", &site());
    assert_eq!(t.text, "Berlin");
    assert_eq!(frag.as_deref(), Some("History"));
}

#[test]
fn fragment_underscores_normalized() {
    let (_t, frag) = Title::parse_parts("Foo#Early_life", &site());
    assert_eq!(frag.as_deref(), Some("Early life"));
}

#[test]
fn prefixed_roundtrips_namespace() {
    let t = Title::parse("template:navbox", &site());
    assert_eq!(t.prefixed(&site()), "Template:Navbox");
}

#[test]
fn project_alias_resolves() {
    let t = Title::parse("Project:About", &site());
    assert_eq!(t.ns, 4);
    assert_eq!(t.prefixed(&site()), "Wikipedia:About");
}
