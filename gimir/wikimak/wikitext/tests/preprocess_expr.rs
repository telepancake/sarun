//! {{#expr:…}} numeric grammar: operators, precedence, functions,
//! MediaWiki numeric formatting, and error handling.

#[path = "preprocess_common/mod.rs"]
mod common;
use common::*;

fn expr(s: &MockStore, e: &str) -> String {
    xt(s, &format!("{{{{#expr:{e}}}}}"))
}

#[test]
fn arithmetic_and_formatting() {
    let s = MockStore::new();
    assert_eq!(expr(&s, "2+2"), "4");
    assert_eq!(expr(&s, "10 - 3 * 2"), "4");
    assert_eq!(expr(&s, "10/4"), "2.5");
    assert_eq!(expr(&s, "10/3"), "3.3333333333333");
    assert_eq!(expr(&s, "2^10"), "1024");
    assert_eq!(expr(&s, "(1+2)*3"), "9");
}

#[test]
fn integer_results_have_no_decimal_point() {
    let s = MockStore::new();
    assert_eq!(expr(&s, "6/2"), "3");
    assert_eq!(expr(&s, "0.1 + 0.2"), "0.3");
}

#[test]
fn mod_div_fmod() {
    let s = MockStore::new();
    assert_eq!(expr(&s, "7 mod 3"), "1");
    assert_eq!(expr(&s, "8 mod 3"), "2");
    assert_eq!(expr(&s, "10 div 4"), "2.5");
    assert_eq!(expr(&s, "10.5 fmod 3"), "1.5");
}

#[test]
fn functions() {
    let s = MockStore::new();
    assert_eq!(expr(&s, "abs(-5)"), "5");
    assert_eq!(expr(&s, "ceil 4.2"), "5");
    assert_eq!(expr(&s, "floor 4.8"), "4");
    assert_eq!(expr(&s, "trunc 4.8"), "4");
    assert_eq!(expr(&s, "3.14159 round 2"), "3.14");
    assert_eq!(expr(&s, "exp 0"), "1");
    assert_eq!(expr(&s, "ln 1"), "0");
}

#[test]
fn round_negative_half_away_from_zero() {
    let s = MockStore::new();
    assert_eq!(expr(&s, "2.5 round 0"), "3");
    assert_eq!(expr(&s, "-2.5 round 0"), "-3");
    assert_eq!(expr(&s, "1234.5678 round 2"), "1234.57");
}

#[test]
fn comparisons_and_booleans() {
    let s = MockStore::new();
    assert_eq!(expr(&s, "5 > 3"), "1");
    assert_eq!(expr(&s, "5 < 3"), "0");
    assert_eq!(expr(&s, "4 = 4"), "1");
    assert_eq!(expr(&s, "4 <> 5"), "1");
    assert_eq!(expr(&s, "4 != 4"), "0");
    assert_eq!(expr(&s, "3 >= 3"), "1");
    assert_eq!(expr(&s, "2 <= 1"), "0");
    assert_eq!(expr(&s, "1 and 0"), "0");
    assert_eq!(expr(&s, "1 or 0"), "1");
    assert_eq!(expr(&s, "not 0"), "1");
    assert_eq!(expr(&s, "not 5"), "0");
}

#[test]
fn unary_binds_tighter_than_power() {
    // MediaWiki: -2^2 == 4 (unary minus binds tighter than ^).
    let s = MockStore::new();
    assert_eq!(expr(&s, "-2^2"), "4");
}

#[test]
fn constants() {
    let s = MockStore::new();
    // pi to 14 significant digits.
    assert_eq!(expr(&s, "pi"), "3.1415926535898");
    assert_eq!(expr(&s, "e"), "2.718281828459");
    assert_eq!(expr(&s, "2 * pi > 6"), "1");
}

#[test]
fn scientific_notation_number() {
    let s = MockStore::new();
    assert_eq!(expr(&s, "1e3"), "1000");
    assert_eq!(expr(&s, "1.5e2 + 1"), "151");
}

#[test]
fn division_by_zero_is_an_error_box() {
    let s = MockStore::new();
    let out = expr(&s, "1/0");
    assert!(out.contains("class=\"error\""), "got: {out}");
    assert!(out.contains("Division by zero"), "got: {out}");
}

#[test]
fn expr_used_as_condition_in_ifexpr() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{#ifexpr:2+2=4|yes|no}}"), "yes");
    assert_eq!(xt(&s, "{{#ifexpr:1 > 5|yes|no}}"), "no");
}

#[test]
fn ifexpr_error_renders_error_box() {
    let s = MockStore::new();
    let out = xt(&s, "{{#ifexpr:1/0|a|b}}");
    assert!(out.contains("class=\"error\""), "got: {out}");
}
