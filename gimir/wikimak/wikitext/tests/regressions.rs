//! Regression tests for adversarially-verified renderer defects. Each test
//! pins the fix for a specific crash / resource-exhaustion report so a
//! reintroduction fails loudly instead of taking down the render process.

#[path = "preprocess_common/mod.rs"]
mod common;
use common::*;

// ---------------------------------------------------------------------------
// Deeply nested braces/brackets/args must NOT overflow the native stack.
//
// The `Node` tree is recursive, so its derived `Clone` (the node walk clones
// subtrees) and `Drop` recurse once per nesting level; the node walk itself
// (expand_nodes → expand_template/expand_arg/expand_link → …) also recurses
// per level. Without a depth cap, a few thousand nested `{{` overflowed the
// stack and SIGABRT'd the whole process (the report reproduced it at ~5000).
// parse_nodes now caps tree depth (deeper delimiters stay literal), which
// bounds clone/drop/expand alike. These inputs are an order of magnitude
// past the old crash threshold; the assertion is simply that rendering
// RETURNS — a stack overflow aborts the test binary, it does not "fail".
// ---------------------------------------------------------------------------

const DEEP: usize = 50_000;

#[test]
fn deeply_nested_templates_do_not_overflow() {
    let s = MockStore::new();
    let input = "{{a".repeat(DEEP) + &"}}".repeat(DEEP);
    let out = xt(&s, &input);
    assert!(!out.is_empty(), "deep template nesting must render, not crash");
}

#[test]
fn deeply_nested_args_do_not_overflow() {
    let s = MockStore::new();
    let input = "{{{a".repeat(DEEP) + &"}}}".repeat(DEEP);
    let out = xt(&s, &input);
    assert!(!out.is_empty(), "deep arg nesting must render, not crash");
}

#[test]
fn deeply_nested_links_do_not_overflow() {
    let s = MockStore::new();
    let input = "[[a".repeat(DEEP) + &"]]".repeat(DEEP);
    let out = xt(&s, &input);
    assert!(!out.is_empty(), "deep link nesting must render, not crash");
}

// Ordinary shallow nesting is unaffected by the guard — a plausible real
// page nests only a handful of levels.
#[test]
fn shallow_nesting_still_expands() {
    let mut s = MockStore::new();
    s.template("Inner", "IN");
    s.template("Outer", "[{{Inner}}]");
    assert_eq!(xt(&s, "{{Outer}}"), "[IN]");
    // A modestly nested arg default resolves normally.
    assert_eq!(xt(&s, "{{{x|{{{y|deep}}}}}}"), "deep");
}

// ---------------------------------------------------------------------------
// {{padleft:}}/{{padright:}} width is capped at MediaWiki's 500-char limit.
//
// Uncapped, a tiny input like `{{padleft:x|400000000}}` built a 400 MB
// string (and `9999999999` requested ~10 GB → allocation-failure abort).
// MediaWiki caps the padded result at 500 chars, so the cap is both the
// memory/CPU fix and the correct output.
// ---------------------------------------------------------------------------

#[test]
fn padleft_width_is_capped_at_500() {
    let s = MockStore::new();
    // Formerly a 400 MB allocation; now capped to a 500-char result.
    let out = xt(&s, "{{padleft:x|400000000}}");
    assert_eq!(out.chars().count(), 500);
    assert!(out.ends_with('x'));
    assert!(out.starts_with('0'));

    // A width that formerly requested ~10 GB is likewise capped, not OOM.
    let huge = xt(&s, "{{padleft:x|9999999999}}");
    assert_eq!(huge.chars().count(), 500);

    // A width just over the cap clamps to exactly 500.
    assert_eq!(xt(&s, "{{padleft:x|1000}}").chars().count(), 500);
}

#[test]
fn padright_width_is_capped_at_500() {
    let s = MockStore::new();
    let out = xt(&s, "{{padright:xyz|1000|-}}");
    assert_eq!(out.chars().count(), 500);
    assert!(out.starts_with("xyz"));
    assert!(out.ends_with('-'));
}

// Sub-cap padding is unchanged by the fix.
#[test]
fn small_pad_unaffected() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{padleft:5|3}}"), "005");
    assert_eq!(xt(&s, "{{padright:5|3}}"), "500");
    // Already wider than the target width → returned as-is.
    assert_eq!(xt(&s, "{{padleft:foo|2}}"), "foo");
    // Exactly 500 is allowed (boundary).
    assert_eq!(xt(&s, "{{padleft:x|500}}").chars().count(), 500);
}

// ---------------------------------------------------------------------------
// protect_tags (nowiki/pre shielding) must scan the body in LINEAR time.
//
// It formerly probed for a tag opening at every character position with
// `find_ci_from(text, "<nowiki", i) == Some(i)`, and find_ci_from searches
// FORWARD to the end — so a large body with no nowiki/pre took O(len²) and a
// megabyte page hung for minutes. This renders a large tag-free body plus a
// real <nowiki> region: the assertion is that it returns (a quadratic scan
// would blow the test timeout) AND still shields the nowiki content.
// ---------------------------------------------------------------------------
#[test]
fn protect_tags_scan_is_linear() {
    let s = MockStore::new();
    // ~800 KB of tag-free text with a nowiki region in the middle. Quadratic
    // scanning would take minutes; linear scanning is milliseconds.
    let filler = "word ".repeat(160_000); // 800 KB, no '<'
    let input = format!("{filler}<nowiki>{{{{notatemplate}}}}</nowiki>{filler}");
    let out = xt(&s, &input);
    // The nowiki body is shielded verbatim (its braces are NOT expanded).
    assert!(out.contains("<nowiki>{{notatemplate}}</nowiki>"), "nowiki content must survive verbatim");
}
