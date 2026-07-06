//! {{#time:…}} — format codes, explicit datetime parsing, and τ default.

#[path = "preprocess_common/mod.rs"]
mod common;
use common::*;

// 2005-01-01T12:34:56Z (Saturday) in micros.
const TS: i64 = 1_104_582_896_000_000;

fn t(s: &MockStore, fmt: &str, arg: &str) -> String {
    if arg.is_empty() {
        xt(s, &format!("{{{{#time:{fmt}}}}}"))
    } else {
        xt(s, &format!("{{{{#time:{fmt}|{arg}}}}}"))
    }
}

#[test]
fn default_source_is_tau() {
    let s = MockStore::at(TS);
    assert_eq!(t(&s, "Y", ""), "2005");
    assert_eq!(t(&s, "Y-m-d", ""), "2005-01-01");
    assert_eq!(t(&s, "H:i:s", ""), "12:34:56");
    // Saturday, DOW 6, ISO N=6.
    assert_eq!(t(&s, "l", ""), "Saturday");
    assert_eq!(t(&s, "D", ""), "Sat");
    assert_eq!(t(&s, "N", ""), "6");
    assert_eq!(t(&s, "w", ""), "6");
}

#[test]
fn explicit_iso_date() {
    let s = MockStore::at(TS);
    assert_eq!(t(&s, "Y-m-d", "2020-07-04"), "2020-07-04");
    // 2005-06-15 was a Wednesday.
    assert_eq!(t(&s, "l", "2005-06-15"), "Wednesday");
    assert_eq!(t(&s, "F j, Y", "2005-06-15"), "June 15, 2005");
}

#[test]
fn explicit_datetime_with_time() {
    let s = MockStore::at(TS);
    assert_eq!(t(&s, "H:i:s", "2005-06-15 08:09:07"), "08:09:07");
    assert_eq!(t(&s, "Y-m-d H:i", "2005-06-15T23:45:00Z"), "2005-06-15 23:45");
}

#[test]
fn fourteen_digit_timestamp() {
    let s = MockStore::at(TS);
    assert_eq!(t(&s, "Y-m-d H:i:s", "20051225133000"), "2005-12-25 13:30:00");
}

#[test]
fn twelve_hour_and_ampm() {
    let s = MockStore::at(TS);
    assert_eq!(t(&s, "g:i a", "2005-01-01 13:05:00"), "1:05 pm");
    assert_eq!(t(&s, "h:i A", "2005-01-01 00:07:00"), "12:07 AM");
    assert_eq!(t(&s, "g A", "2005-01-01 12:00:00"), "12 PM");
}

#[test]
fn month_and_day_number_padding() {
    let s = MockStore::at(TS);
    assert_eq!(t(&s, "n/j", "2005-03-05"), "3/5");
    assert_eq!(t(&s, "m/d", "2005-03-05"), "03/05");
    assert_eq!(t(&s, "M", "2005-03-05"), "Mar");
}

#[test]
fn literals_and_escapes() {
    let s = MockStore::at(TS);
    // Backslash escapes a format char; quotes make a literal run.
    assert_eq!(t(&s, "\\Y=Y", "2005-01-01"), "Y=2005");
    assert_eq!(t(&s, "\"year\" Y", "2005-01-01"), "year 2005");
}

#[test]
fn unix_and_leap_and_daysinmonth() {
    let s = MockStore::at(TS);
    assert_eq!(t(&s, "U", "1970-01-01 00:00:00"), "0");
    assert_eq!(t(&s, "L", "2004-01-01"), "1"); // 2004 leap
    assert_eq!(t(&s, "L", "2005-01-01"), "0");
    assert_eq!(t(&s, "t", "2005-02-01"), "28");
    assert_eq!(t(&s, "t", "2004-02-01"), "29");
}

#[test]
fn invalid_time_is_error_box() {
    let s = MockStore::at(TS);
    let out = t(&s, "Y", "not a date");
    assert!(out.contains("class=\"error\""), "got: {out}");
}
