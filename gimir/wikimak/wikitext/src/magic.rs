//! Magic words & variables ({{PAGENAME}}, {{CURRENTYEAR}}, {{SITENAME}},
//! {{ns:}}…) — τ-resolved: CURRENT* answers from
//! PageStore::timestamp_micros, never wall clock (plan §3.2).
//! OWNED BY: the preprocessor agent.
//!
//! Calendar math is done here (no chrono in this crate) so both the
//! magic date variables and {{#time}} share one civil-date derivation.

use crate::{SiteConfig, Title};

/// Broken-out UTC civil datetime. `dow` is 0=Sunday … 6=Saturday
/// (MediaWiki's {{CURRENTDOW}} convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
    pub dow: u32,
    /// Unix seconds — exposed for {{#time:U}} / {{CURRENTTIMESTAMP}} math.
    pub unix: i64,
}

/// Days since 1970-01-01 → (year, month 1-12, day 1-31). Hinnant's
/// civil_from_days; valid across the full i64 range we care about.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0,399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0,365]
    let mp = (5 * doy + 2) / 153; // [0,11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1,31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1,12]
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

/// Inverse of `civil_from_days`: (year, month, day) → days since epoch.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0,399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0,365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0,146096]
    era * 146_097 + doe - 719_468
}

pub(crate) fn civil_from_unix(secs: i64) -> Civil {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let dow = (((days % 7) + 4).rem_euclid(7)) as u32; // 1970-01-01 = Thursday
    Civil {
        year,
        month,
        day,
        hour: (rem / 3600) as u32,
        min: ((rem % 3600) / 60) as u32,
        sec: (rem % 60) as u32,
        dow,
        unix: secs,
    }
}

pub(crate) fn civil_from_micros(micros: i64) -> Civil {
    civil_from_unix(micros.div_euclid(1_000_000))
}

/// Build a Civil from explicit y/m/d h:m:s (used by {{#time}} arg parse).
pub(crate) fn civil_from_parts(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Civil {
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + (h as i64) * 3600 + (mi as i64) * 60 + s as i64;
    civil_from_unix(secs)
}

pub(crate) const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
pub(crate) const WEEKDAYS: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

pub(crate) fn month_name(m: u32) -> &'static str {
    MONTHS.get((m.max(1) - 1) as usize % 12).copied().unwrap_or("January")
}
pub(crate) fn weekday_name(dow: u32) -> &'static str {
    WEEKDAYS[(dow % 7) as usize]
}

/// ISO-8601 week-of-year (1-53). Used by {{CURRENTWEEK}}.
pub(crate) fn iso_week(c: &Civil) -> u32 {
    let days = days_from_civil(c.year, c.month, c.day);
    let iso_dow = (((days % 7) + 4).rem_euclid(7)) as i64; // 0=Sun
    let iso_dow = if iso_dow == 0 { 7 } else { iso_dow }; // 1=Mon..7=Sun
    let thursday = days - (iso_dow - 4); // Thursday decides the ISO year
    let (ty, _, _) = civil_from_days(thursday);
    let jan1 = days_from_civil(ty, 1, 1);
    ((thursday - jan1) / 7 + 1) as u32
}

/// The subject (even) namespace id paired with `ns`.
fn subject_ns(ns: i32) -> i32 {
    if ns < 0 {
        ns
    } else {
        ns & !1
    }
}
fn talk_ns(ns: i32) -> i32 {
    if ns < 0 {
        ns
    } else {
        subject_ns(ns) | 1
    }
}

fn ns_name(site: &SiteConfig, ns: i32) -> String {
    if ns == 0 {
        return String::new();
    }
    site.namespaces
        .get(&ns)
        .map(|n| {
            if !n.aliases.is_empty() {
                n.aliases[0].clone()
            } else {
                n.canonical.clone()
            }
        })
        .unwrap_or_default()
}

/// Resolve a magic *variable* (no colon, or a page-name variable with an
/// optional title argument). Returns None when `name` is not one we know,
/// so the caller can fall through to template transclusion.
///
/// `subject` is the Title the variable describes — the page for the
/// bare forms, or the parsed argument for the `{{PAGENAME:Foo}}` forms.
pub(crate) fn magic_variable(
    name: &str,
    subject: &Title,
    site: &SiteConfig,
    ts_micros: i64,
    has_arg: bool,
) -> Option<String> {
    let c = civil_from_micros(ts_micros);
    // CURRENT* and LOCAL* share values (no per-wiki timezone in the depot).
    let stripped = name.strip_prefix("CURRENT").or_else(|| name.strip_prefix("LOCAL"));
    if !has_arg {
        if let Some(rest) = stripped {
            if let Some(v) = date_variable(rest, &c) {
                return Some(v);
            }
        }
        match name {
            "SITENAME" => return Some(site.site_name.clone()),
            "SERVER" | "SERVERNAME" | "SCRIPTPATH" | "STYLEPATH" | "ARTICLEPATH" => {
                return Some(String::new())
            }
            "REVISIONID" | "REVISIONUSER" | "PAGEID" => return Some(String::new()),
            "REVISIONYEAR" => return Some(format!("{:04}", c.year)),
            "REVISIONMONTH" => return Some(format!("{:02}", c.month)),
            "REVISIONMONTH1" => return Some(c.month.to_string()),
            "REVISIONDAY" => return Some(c.day.to_string()),
            "REVISIONDAY2" => return Some(format!("{:02}", c.day)),
            "REVISIONTIMESTAMP" => return Some(timestamp14(&c)),
            "CONTENTLANGUAGE" | "CONTENTLANG" => return Some(site.lang.clone()),
            "DIRECTIONMARK" | "DIRMARK" => {
                return Some(if site.rtl { "\u{200f}".into() } else { "\u{200e}".into() })
            }
            "NUMBEROFARTICLES" | "NUMBEROFPAGES" | "NUMBEROFFILES" | "NUMBEROFUSERS"
            | "NUMBEROFEDITS" | "NUMBEROFADMINS" | "NUMBEROFACTIVEUSERS" => {
                return Some("0".into())
            }
            _ => {}
        }
    }
    // Page-name family: bare uses the render title, `:arg` uses `subject`.
    if let Some(v) = space_variable(name, subject, site) {
        return Some(v);
    }
    let (base, url) = match name.strip_suffix('E') {
        Some(b) if PAGE_VARS.contains(&b) => (b, true),
        _ => (name, false),
    };
    let raw = page_variable(base, subject, site)?;
    Some(if url { encode_title(&raw) } else { raw })
}

const PAGE_VARS: [&str; 8] = [
    "PAGENAME",
    "FULLPAGENAME",
    "BASEPAGENAME",
    "SUBPAGENAME",
    "ROOTPAGENAME",
    "TALKPAGENAME",
    "SUBJECTPAGENAME",
    "ARTICLEPAGENAME",
];

fn page_variable(name: &str, t: &Title, site: &SiteConfig) -> Option<String> {
    let full = t.prefixed(site);
    Some(match name {
        "PAGENAME" => t.text.clone(),
        "FULLPAGENAME" => full,
        "BASEPAGENAME" => match t.text.rsplit_once('/') {
            Some((base, _)) => base.to_string(),
            None => t.text.clone(),
        },
        "SUBPAGENAME" => match t.text.rsplit_once('/') {
            Some((_, sub)) => sub.to_string(),
            None => t.text.clone(),
        },
        "ROOTPAGENAME" => t.text.split('/').next().unwrap_or(&t.text).to_string(),
        "TALKPAGENAME" => prefixed_in(site, talk_ns(t.ns), &t.text),
        "SUBJECTPAGENAME" | "ARTICLEPAGENAME" => prefixed_in(site, subject_ns(t.ns), &t.text),
        _ => return None,
    })
}

/// Space-namespace variables ({{TALKSPACE}}, {{NAMESPACE}}) resolved with
/// an optional title arg. Split out so `magic_variable` stays flat.
pub(crate) fn space_variable(name: &str, t: &Title, site: &SiteConfig) -> Option<String> {
    match name {
        "NAMESPACE" => Some(ns_name(site, t.ns)),
        "NAMESPACENUMBER" => Some(t.ns.to_string()),
        "TALKSPACE" => Some(ns_name(site, talk_ns(t.ns))),
        "SUBJECTSPACE" | "ARTICLESPACE" => Some(ns_name(site, subject_ns(t.ns))),
        _ => None,
    }
}

fn prefixed_in(site: &SiteConfig, ns: i32, text: &str) -> String {
    let p = ns_name(site, ns);
    if p.is_empty() {
        text.to_string()
    } else {
        format!("{p}:{text}")
    }
}

fn date_variable(rest: &str, c: &Civil) -> Option<String> {
    Some(match rest {
        "YEAR" => format!("{:04}", c.year),
        "MONTH" | "MONTH2" => format!("{:02}", c.month),
        "MONTH1" => c.month.to_string(),
        "MONTHNAME" | "MONTHNAMEGEN" => month_name(c.month).to_string(),
        "MONTHABBREV" => month_name(c.month)[..3].to_string(),
        "DAY" => c.day.to_string(),
        "DAY2" => format!("{:02}", c.day),
        "DAYNAME" => weekday_name(c.dow).to_string(),
        "DOW" => c.dow.to_string(),
        "TIME" => format!("{:02}:{:02}", c.hour, c.min),
        "HOUR" => format!("{:02}", c.hour),
        "WEEK" => format!("{:02}", iso_week(c)),
        "TIMESTAMP" => timestamp14(c),
        _ => return None,
    })
}

fn timestamp14(c: &Civil) -> String {
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        c.year, c.month, c.day, c.hour, c.min, c.sec
    )
}

/// The MediaWiki behavior switches we recognize (stripped from output;
/// TOC/section ones surface as flags via `BehaviorSwitches`).
pub(crate) const BEHAVIOR_SWITCHES: [&str; 20] = [
    "__NOTOC__",
    "__FORCETOC__",
    "__TOC__",
    "__NOEDITSECTION__",
    "__NEWSECTIONLINK__",
    "__NONEWSECTIONLINK__",
    "__NOGALLERY__",
    "__HIDDENCAT__",
    "__EXPECTUNUSEDCATEGORY__",
    "__NOCONTENTCONVERT__",
    "__NOCC__",
    "__NOTITLECONVERT__",
    "__NOTC__",
    "__INDEX__",
    "__NOINDEX__",
    "__STATICREDIRECT__",
    "__DISAMBIG__",
    "__EXPECTED_UNCONNECTED_PAGE__",
    "__ARCHIVEDTALK__",
    "__NOGLOBAL__",
];

/// Percent-encode a title for the `…E` (encoded) page-name variables:
/// spaces → underscores, then URL-encode the rest MediaWiki-style.
pub(crate) fn encode_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' => out.push('_'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b':' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
