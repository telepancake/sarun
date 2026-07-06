//! Magic variables (τ-resolved dates, page-name family, site vars),
//! DISPLAYTITLE/DEFAULTSORT swallowing, and behavior switches.

#[path = "preprocess_common/mod.rs"]
mod common;
use common::*;

// 2005-01-01T00:00:00Z and 2005-01-01T12:34:56Z, in micros.
const TS_MIDNIGHT: i64 = 1_104_537_600_000_000;
const TS_AFTERNOON: i64 = 1_104_582_896_000_000;

#[test]
fn current_date_follows_tau_not_wall_clock() {
    let s = MockStore::at(TS_MIDNIGHT);
    assert_eq!(xt(&s, "{{CURRENTYEAR}}"), "2005");
    assert_eq!(xt(&s, "{{CURRENTMONTH}}"), "01");
    assert_eq!(xt(&s, "{{CURRENTMONTH1}}"), "1");
    assert_eq!(xt(&s, "{{CURRENTMONTHNAME}}"), "January");
    assert_eq!(xt(&s, "{{CURRENTDAY}}"), "1");
    assert_eq!(xt(&s, "{{CURRENTDAY2}}"), "01");
    // 2005-01-01 was a Saturday → DOW 6, name Saturday.
    assert_eq!(xt(&s, "{{CURRENTDOW}}"), "6");
    assert_eq!(xt(&s, "{{CURRENTDAYNAME}}"), "Saturday");
}

#[test]
fn tau_change_changes_currentyear() {
    // The SAME text renders differently at two different τ — proving the
    // value comes from timestamp_micros, never the host clock.
    let s2005 = MockStore::at(TS_MIDNIGHT);
    let s2020 = MockStore::at(1_577_836_800_000_000); // 2020-01-01T00:00:00Z
    assert_eq!(xt(&s2005, "{{CURRENTYEAR}}"), "2005");
    assert_eq!(xt(&s2020, "{{CURRENTYEAR}}"), "2020");
}

#[test]
fn current_time_fields() {
    let s = MockStore::at(TS_AFTERNOON);
    assert_eq!(xt(&s, "{{CURRENTTIME}}"), "12:34");
    assert_eq!(xt(&s, "{{CURRENTHOUR}}"), "12");
    assert_eq!(xt(&s, "{{CURRENTTIMESTAMP}}"), "20050101123456");
}

#[test]
fn local_aliases_current() {
    let s = MockStore::at(TS_MIDNIGHT);
    assert_eq!(xt(&s, "{{LOCALYEAR}}"), "2005");
    assert_eq!(xt(&s, "{{LOCALMONTHNAME}}"), "January");
}

#[test]
fn pagename_family_from_render_title() {
    let s = MockStore::new();
    let t = title(10, "Infobox/doc");
    assert_eq!(ex_on(&s, &t, "{{PAGENAME}}").text, "Infobox/doc");
    assert_eq!(ex_on(&s, &t, "{{FULLPAGENAME}}").text, "Template:Infobox/doc");
    assert_eq!(ex_on(&s, &t, "{{NAMESPACE}}").text, "Template");
    assert_eq!(ex_on(&s, &t, "{{BASEPAGENAME}}").text, "Infobox");
    assert_eq!(ex_on(&s, &t, "{{SUBPAGENAME}}").text, "doc");
}

#[test]
fn namespace_variables_talk_subject() {
    let s = MockStore::new();
    let t = title(2, "Example"); // User namespace
    assert_eq!(ex_on(&s, &t, "{{NAMESPACE}}").text, "User");
    assert_eq!(ex_on(&s, &t, "{{TALKSPACE}}").text, "User talk");
    assert_eq!(ex_on(&s, &t, "{{SUBJECTSPACE}}").text, "User");
    // From a talk page, subject space maps back to the even namespace.
    let tt = title(3, "Example");
    assert_eq!(ex_on(&s, &tt, "{{SUBJECTSPACE}}").text, "User");
    assert_eq!(ex_on(&s, &tt, "{{TALKSPACE}}").text, "User talk");
}

#[test]
fn pagename_with_title_argument() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{PAGENAME:Template:Foo}}"), "Foo");
    assert_eq!(xt(&s, "{{NAMESPACE:File:Bar.png}}"), "File");
}

#[test]
fn sitename_and_empty_safe_server_vars() {
    let s = MockStore::new();
    assert_eq!(xt(&s, "{{SITENAME}}"), "Wikipedia");
    assert_eq!(xt(&s, "{{SERVER}}"), "");
    assert_eq!(xt(&s, "{{SCRIPTPATH}}"), "");
    assert_eq!(xt(&s, "{{REVISIONID}}"), "");
}

#[test]
fn displaytitle_and_defaultsort_are_swallowed_and_surfaced() {
    let s = MockStore::new();
    let out = ex(&s, "before{{DISPLAYTITLE:My Title}}{{DEFAULTSORT:Sortkey}}after");
    assert_eq!(out.text, "beforeafter");
    assert_eq!(out.display_title.as_deref(), Some("My Title"));
    assert_eq!(out.default_sort.as_deref(), Some("Sortkey"));
}

#[test]
fn behavior_switches_stripped_and_flagged() {
    let s = MockStore::new();
    let out = ex(&s, "a__NOTOC__b__NOEDITSECTION__c");
    assert_eq!(out.text, "abc");
    assert!(out.switches.no_toc);
    assert!(out.switches.no_editsection);
    assert!(!out.switches.force_toc);
}

#[test]
fn magic_word_shadows_are_not_templates() {
    // A page literally named "CURRENTYEAR" must NOT shadow the magic word.
    let mut s = MockStore::at(TS_MIDNIGHT);
    s.template("CURRENTYEAR", "SHOULD-NOT-APPEAR");
    assert_eq!(xt(&s, "{{CURRENTYEAR}}"), "2005");
}
