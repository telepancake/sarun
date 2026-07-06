//! Parser functions (#if/#ifeq/#iferror/#ifexist/#switch/#titleparts/#tag)
//! and string/format transforms (lc/uc/pad/urlencode/ns/plural/formatnum).

#[path = "preprocess_common/mod.rs"]
mod common;
use common::*;
use wikimak_wikitext::preprocess::expand;

#[test]
fn if_selects_branch_on_emptiness() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{#if:x|yes|no}}"), "yes");
    assert_eq!(xt(&s, "{{#if:|yes|no}}"), "no");
    assert_eq!(xt(&s, "{{#if: |yes|no}}"), "no"); // whitespace-only = empty
    assert_eq!(xt(&s, "{{#if:x|only}}"), "only");
    assert_eq!(xt(&s, "{{#if:|only}}"), "");
}

#[test]
fn ifeq_string_and_numeric() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{#ifeq:cat|cat|same|diff}}"), "same");
    assert_eq!(xt(&s, "{{#ifeq:cat|dog|same|diff}}"), "diff");
    // Numeric equality: "1" == "1.0" == "01".
    assert_eq!(xt(&s, "{{#ifeq:1|1.0|eq|ne}}"), "eq");
    assert_eq!(xt(&s, "{{#ifeq:01|1|eq|ne}}"), "eq");
    // Non-numeric strings compare literally.
    assert_eq!(xt(&s, "{{#ifeq:a1|a1|eq|ne}}"), "eq");
}

#[test]
fn iferror_detects_error_markup() {
    let s = MockStore::new();
    // A real expr error inside the test triggers the `then` branch.
    assert_eq!(xt(&s, "{{#iferror:{{#expr:1/0}}|bad|good}}"), "bad");
    assert_eq!(xt(&s, "{{#iferror:plain|bad|good}}"), "good");
    // else defaults to the test string when omitted.
    assert_eq!(xt(&s, "{{#iferror:plain}}"), "plain");
}

#[test]
fn ifexist_consults_store() {
    let mut s = MockStore::new();
    s.add(0, "Real Page", "content");
    assert_eq!(xt(&s, "{{#ifexist:Real Page|here|gone}}"), "here");
    assert_eq!(xt(&s, "{{#ifexist:Ghost Page|here|gone}}"), "gone");
    // Namespaced existence.
    s.template("Exists", "x");
    assert_eq!(xt(&s, "{{#ifexist:Template:Exists|y|n}}"), "y");
}

#[test]
fn switch_basic_default_and_fallthrough() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{#switch:b|a=1|b=2|c=3}}"), "2");
    assert_eq!(xt(&s, "{{#switch:z|a=1|b=2|#default=none}}"), "none");
    // Fallthrough: bare cases share the next result.
    assert_eq!(xt(&s, "{{#switch:a|a|b=shared|c=3}}"), "shared");
    assert_eq!(xt(&s, "{{#switch:b|a|b=shared|c=3}}"), "shared");
    // Trailing bare value is the implicit default.
    assert_eq!(xt(&s, "{{#switch:q|a=1|fallback}}"), "fallback");
}

#[test]
fn switch_numeric_comparison() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{#switch:1.0|1=one|2=two}}"), "one");
}

#[test]
fn titleparts_slices_on_slash() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{#titleparts:A/B/C|1}}"), "A");
    assert_eq!(xt(&s, "{{#titleparts:A/B/C|2}}"), "A/B");
    assert_eq!(xt(&s, "{{#titleparts:A/B/C||2}}"), "B/C");
    assert_eq!(xt(&s, "{{#titleparts:A/B/C|-1}}"), "A/B");
    assert_eq!(xt(&s, "{{#titleparts:A/B/C}}"), "A/B/C");
}

#[test]
fn tag_emits_literal_markup() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{#tag:ref|a citation}}"), "<ref>a citation</ref>");
    assert_eq!(
        xt(&s, "{{#tag:ref|body|name=x|group=y}}"),
        "<ref name=\"x\" group=\"y\">body</ref>"
    );
}

#[test]
fn lc_uc_first_case() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{lc:HELLO World}}"), "hello world");
    assert_eq!(xt(&s, "{{uc:hello World}}"), "HELLO WORLD");
    assert_eq!(xt(&s, "{{lcfirst:Hello}}"), "hello");
    assert_eq!(xt(&s, "{{ucfirst:hello}}"), "Hello");
}

#[test]
fn padleft_padright() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{padleft:7|3|0}}"), "007");
    assert_eq!(xt(&s, "{{padleft:bat|5}}"), "00bat");
    assert_eq!(xt(&s, "{{padright:bat|5|xy}}"), "batxy");
    // Already at/over width → unchanged.
    assert_eq!(xt(&s, "{{padleft:longvalue|3}}"), "longvalue");
}

#[test]
fn urlencode_modes() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{urlencode:a b}}"), "a+b");
    assert_eq!(xt(&s, "{{urlencode:a b|WIKI}}"), "a_b");
    assert_eq!(xt(&s, "{{urlencode:a b|PATH}}"), "a%20b");
    assert_eq!(xt(&s, "{{urlencode:a&b}}"), "a%26b");
}

#[test]
fn ns_maps_id_and_name() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{ns:0}}"), "");
    assert_eq!(xt(&s, "{{ns:10}}"), "Template");
    assert_eq!(xt(&s, "{{ns:6}}"), "File");
    // Name/alias input resolves to canonical.
    assert_eq!(xt(&s, "{{ns:Image}}"), "File");
    assert_eq!(xt(&s, "{{ns:template}}"), "Template");
}

#[test]
fn plural_english_rule() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{plural:1|cat|cats}}"), "cat");
    assert_eq!(xt(&s, "{{plural:2|cat|cats}}"), "cats");
    assert_eq!(xt(&s, "{{plural:0|cat|cats}}"), "cats");
}

#[test]
fn formatnum_grouping_and_reverse() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{formatnum:1234567}}"), "1,234,567");
    assert_eq!(xt(&s, "{{formatnum:1234.5}}"), "1,234.5");
    assert_eq!(xt(&s, "{{formatnum:1,234,567|R}}"), "1234567");
}

#[test]
fn nested_parser_functions() {
    let s = MockStore::new();
    // #if condition built from #ifeq.
    assert_eq!(
        xt(&s, "{{#if:{{#ifeq:a|a|1|}}|matched|nomatch}}"),
        "matched"
    );
}

#[test]
fn invoke_without_engine_is_error_and_miss() {
    let s = MockStore::new();
    let out = expand(&s, &title(0, "Test"), "{{#invoke:Foo|bar}}", &opts());
    assert!(out.text.contains("class=\"error\""), "got: {}", out.text);
    assert_eq!(out.misses.failed_invokes, vec!["Module:Foo#bar".to_string()]);
}

#[test]
fn invoke_with_engine_passes_args_and_output() {
    use wikimak_wikitext::RenderOptions;
    let s = MockStore::new();
    let inv = FnInvoker(|module: &str, function: &str, frame: &wikimak_wikitext::Frame| {
        // Prove args and identifiers cross the boundary.
        let a1 = frame.args.get("1").cloned().unwrap_or_default();
        let named = frame.args.get("k").cloned().unwrap_or_default();
        Ok(format!("{module}/{function}:{a1}:{named}"))
    });
    let o = RenderOptions {
        invoker: Some(&inv),
        ..Default::default()
    };
    let out = expand(&s, &title(0, "Test"), "{{#invoke:Str|len|hello|k=v}}", &o);
    assert_eq!(out.text, "Str/len:hello:v");
    assert!(out.misses.failed_invokes.is_empty());
}

#[test]
fn invoke_error_renders_box_and_counts_miss() {
    use wikimak_wikitext::RenderOptions;
    let s = MockStore::new();
    let inv = FnInvoker(|_m: &str, _f: &str, _fr: &wikimak_wikitext::Frame| {
        Err("boom".to_string())
    });
    let o = RenderOptions {
        invoker: Some(&inv),
        ..Default::default()
    };
    let out = expand(&s, &title(0, "Test"), "{{#invoke:M|f}}", &o);
    assert!(out.text.contains("class=\"error\""), "got: {}", out.text);
    assert!(out.text.contains("boom"));
    assert_eq!(out.misses.failed_invokes, vec!["Module:M#f".to_string()]);
}
