//! τ-driven calendar math and formatters. Scribunto's `os.time`,
//! `os.date`, and `mw.language:formatDate` must answer from the frame's
//! τ, never the wall clock (plan §3.2: CURRENT* == τ). All times are UTC
//! — the depot's τ is a unix instant and MediaWiki formats content-time
//! in the wiki's timezone, which for our purposes is UTC.

/// Broken-down UTC time. `wday` is 0=Sunday..6=Saturday (C `struct tm`);
/// `yday` is 0-based day-of-year (C convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
    pub wday: u32,
    pub yday: u32,
}

/// Days from civil date (Howard Hinnant's algorithm) — days since
/// 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if m > 2 { m as i64 - 3 } else { m as i64 + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Decompose a unix-seconds instant into UTC calendar fields.
pub fn civil_from_unix(secs: i64) -> Civil {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday (wday 4).
    let wday = (days.rem_euclid(7) + 4).rem_euclid(7) as u32;
    let yday = (days - days_from_civil(year, 1, 1)) as u32;
    Civil {
        year,
        month,
        day,
        hour: (rem / 3600) as u32,
        min: (rem % 3600 / 60) as u32,
        sec: (rem % 60) as u32,
        wday,
        yday,
    }
}

/// Inverse: build unix seconds from a broken-down UTC time (used by
/// `os.time{...}`). Fields out of range are normalized by the day math.
pub fn unix_from_fields(year: i64, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> i64 {
    days_from_civil(year, month, day) * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

fn ordinal_suffix(d: u32) -> &'static str {
    match (d % 100, d % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    }
}

/// C `strftime` subset for `os.date`. Supports the codes real modules
/// reach for; unknown `%x` pass through literally.
pub fn strftime(fmt: &str, c: &Civil) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&c.year.to_string()),
            Some('y') => out.push_str(&format!("{:02}", c.year.rem_euclid(100))),
            Some('m') => out.push_str(&format!("{:02}", c.month)),
            Some('d') => out.push_str(&format!("{:02}", c.day)),
            Some('e') => out.push_str(&format!("{:2}", c.day)),
            Some('H') => out.push_str(&format!("{:02}", c.hour)),
            Some('I') => {
                let h12 = if c.hour % 12 == 0 { 12 } else { c.hour % 12 };
                out.push_str(&format!("{h12:02}"));
            }
            Some('M') => out.push_str(&format!("{:02}", c.min)),
            Some('S') => out.push_str(&format!("{:02}", c.sec)),
            Some('p') => out.push_str(if c.hour < 12 { "AM" } else { "PM" }),
            Some('A') => out.push_str(WEEKDAYS[c.wday as usize]),
            Some('a') => out.push_str(&WEEKDAYS[c.wday as usize][..3]),
            Some('B') => out.push_str(MONTHS[(c.month - 1) as usize]),
            Some('b') | Some('h') => out.push_str(&MONTHS[(c.month - 1) as usize][..3]),
            Some('j') => out.push_str(&format!("{:03}", c.yday + 1)),
            Some('w') => out.push_str(&c.wday.to_string()),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// MediaWiki `#time` / PHP-`date`-style formatter, used by
/// `mw.language:formatDate`. Backslash escapes the next char; `"…"`
/// runs are literal. Supports the common Y y n m d j H i s D l F M N w
/// L t U codes; unknown letters pass through.
pub fn format_php_date(fmt: &str, c: &Civil, unix: i64) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '"' => {
                for n in chars.by_ref() {
                    if n == '"' {
                        break;
                    }
                    out.push(n);
                }
            }
            'Y' => out.push_str(&c.year.to_string()),
            'y' => out.push_str(&format!("{:02}", c.year.rem_euclid(100))),
            'n' => out.push_str(&c.month.to_string()),
            'm' => out.push_str(&format!("{:02}", c.month)),
            'F' => out.push_str(MONTHS[(c.month - 1) as usize]),
            'M' => out.push_str(&MONTHS[(c.month - 1) as usize][..3]),
            'j' => out.push_str(&c.day.to_string()),
            'd' => out.push_str(&format!("{:02}", c.day)),
            'S' => out.push_str(ordinal_suffix(c.day)),
            'l' => out.push_str(WEEKDAYS[c.wday as usize]),
            'D' => out.push_str(&WEEKDAYS[c.wday as usize][..3]),
            // ISO weekday 1=Mon..7=Sun for N; 0=Sun..6=Sat for w.
            'N' => out.push_str(&(if c.wday == 0 { 7 } else { c.wday }).to_string()),
            'w' => out.push_str(&c.wday.to_string()),
            'H' => out.push_str(&format!("{:02}", c.hour)),
            'G' => out.push_str(&c.hour.to_string()),
            'h' => {
                let h12 = if c.hour % 12 == 0 { 12 } else { c.hour % 12 };
                out.push_str(&format!("{h12:02}"));
            }
            'g' => {
                let h12 = if c.hour % 12 == 0 { 12 } else { c.hour % 12 };
                out.push_str(&h12.to_string());
            }
            'i' => out.push_str(&format!("{:02}", c.min)),
            's' => out.push_str(&format!("{:02}", c.sec)),
            'A' => out.push_str(if c.hour < 12 { "AM" } else { "PM" }),
            'a' => out.push_str(if c.hour < 12 { "am" } else { "pm" }),
            'L' => out.push(if is_leap(c.year) { '1' } else { '0' }),
            't' => out.push_str(&days_in_month(c.year, c.month).to_string()),
            'z' => out.push_str(&c.yday.to_string()),
            'U' => out.push_str(&unix.to_string()),
            other => out.push(other),
        }
    }
    out
}

/// Parse a timestamp STRING as `mw.language:formatDate` accepts one (PHP
/// `strtotime`/`wfTimestamp` territory). Not the full grammar — the subset
/// real modules feed formatDate: MediaWiki 14-digit stamps, ISO/`Y-M-D`
/// with optional time, `D Month Y` / `Month D, Y` / `Month Y` / bare year.
/// Returns unix seconds. `now` resolves to τ. None = unrecognized.
pub fn parse_timestamp(raw: &str, tau_secs: i64) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let low = s.to_ascii_lowercase();
    if low == "now" {
        return Some(tau_secs);
    }

    // MediaWiki 14-digit YYYYMMDDHHMMSS (and shorter all-digit prefixes).
    if s.len() >= 4 && s.bytes().all(|b| b.is_ascii_digit()) {
        if s.len() == 14 {
            let g = |a: usize, b: usize| s[a..b].parse::<i64>().ok();
            let (y, mo, d) = (g(0, 4)?, g(4, 6)? as u32, g(6, 8)? as u32);
            let (h, mi, se) = (g(8, 10)? as u32, g(10, 12)? as u32, g(12, 14)? as u32);
            return Some(unix_from_fields(y, mo, d, h, mi, se));
        }
        if s.len() == 4 {
            return Some(unix_from_fields(s.parse().ok()?, 1, 1, 0, 0, 0));
        }
    }

    // Split off a time part if present (after 'T' or a space).
    let (date_part, time_part) = split_date_time(s);
    let (mut h, mut mi, mut se) = (0u32, 0u32, 0u32);
    if let Some(tp) = time_part {
        let tp = tp.trim_end_matches('Z').trim();
        let mut it = tp.split(':');
        h = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
        mi = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        se = it.next().and_then(|x| x.split('.').next().unwrap_or("0").parse().ok()).unwrap_or(0);
    }

    // Numeric Y-M-D or Y/M/D.
    if let Some((y, mo, d)) = parse_ymd(date_part) {
        return Some(unix_from_fields(y, mo, d, h, mi, se));
    }
    // Month-name forms. A year-less form ("1 January", "1January") defaults
    // to τ's year, matching PHP strtotime (CS1 feeds these for validation).
    let default_year = civil_from_unix(tau_secs).year;
    if let Some((y, mo, d)) = parse_named(date_part, default_year) {
        return Some(unix_from_fields(y, mo, d, h, mi, se));
    }
    None
}

fn split_date_time(s: &str) -> (&str, Option<&str>) {
    if let Some(i) = s.find('T') {
        return (&s[..i], Some(&s[i + 1..]));
    }
    // A space separating a date from a HH:MM(:SS) time.
    if let Some(i) = s.find(' ') {
        let rest = &s[i + 1..];
        if rest.contains(':') {
            return (&s[..i], Some(rest));
        }
    }
    (s, None)
}

fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let sep = if s.contains('-') { '-' } else if s.contains('/') { '/' } else { return None };
    let mut it = s.split(sep);
    let y: i64 = it.next()?.trim().parse().ok()?;
    // Reject a leading day (D-M-Y) heuristically: a 4-digit first field is a
    // year; otherwise not our format (named parser handles D Month Y).
    if !(1..=9999).contains(&y) || s.split(sep).next()?.trim().len() < 3 {
        return None;
    }
    let mo: u32 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(1);
    let d: u32 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(1);
    if it.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, mo, d))
}

fn month_num(name: &str) -> Option<u32> {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return None;
    }
    MONTHS
        .iter()
        .position(|m| {
            let ml = m.to_ascii_lowercase();
            ml == n || ml.starts_with(&n) && n.len() >= 3
        })
        .map(|i| i as u32 + 1)
}

fn parse_named(s: &str, default_year: i64) -> Option<(i64, u32, u32)> {
    // Tokens split on spaces and commas: "1 January 2020", "January 1, 2020",
    // "January 2020", "Jan 2020".
    // Split on spaces/commas, THEN split any digit⇄letter boundary so
    // glued forms like "1January" or "January2020" tokenize ("1","January").
    let mut toks: Vec<String> = Vec::new();
    for raw in s.split(|c: char| c == ' ' || c == ',').filter(|t| !t.is_empty()) {
        let mut cur = String::new();
        let mut prev_digit: Option<bool> = None;
        for ch in raw.chars() {
            let is_d = ch.is_ascii_digit();
            if let Some(pd) = prev_digit {
                if pd != is_d {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            cur.push(ch);
            prev_digit = Some(is_d);
        }
        if !cur.is_empty() {
            toks.push(cur);
        }
    }
    if toks.is_empty() {
        return None;
    }
    let toks: Vec<&str> = toks.iter().map(|s| s.as_str()).collect();
    let (mut year, mut month, mut day) = (None, None, None);
    for t in &toks {
        if let Some(m) = month_num(t) {
            month = Some(m);
        } else if let Ok(n) = t.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<i64>() {
            if n >= 1000 {
                year = Some(n);
            } else if day.is_none() && (1..=31).contains(&n) {
                day = Some(n as u32);
            } else {
                year = Some(n);
            }
        }
    }
    let m = month?;
    let y = year.unwrap_or(default_year);
    Some((y, m, day.unwrap_or(1)))
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 30,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        let c = civil_from_unix(0);
        assert_eq!((c.year, c.month, c.day, c.wday), (1970, 1, 1, 4));
        // 2005-03-01 12:34:56 UTC == 1109680496.
        let c = civil_from_unix(1_109_680_496);
        assert_eq!((c.year, c.month, c.day), (2005, 3, 1));
        assert_eq!((c.hour, c.min, c.sec), (12, 34, 56));
        assert_eq!(c.wday, 2); // Tuesday
    }

    #[test]
    fn roundtrip() {
        let t = 1_109_680_496;
        let c = civil_from_unix(t);
        assert_eq!(unix_from_fields(c.year, c.month, c.day, c.hour, c.min, c.sec), t);
    }

    #[test]
    fn php_date_codes() {
        let c = civil_from_unix(1_109_680_496); // 2005-03-01 12:34:56 Tue
        assert_eq!(format_php_date("Y-m-d", &c, 0), "2005-03-01");
        assert_eq!(format_php_date("j F Y", &c, 0), "1 March 2005");
        assert_eq!(format_php_date("l", &c, 0), "Tuesday");
        assert_eq!(format_php_date("H:i:s", &c, 0), "12:34:56");
        assert_eq!(format_php_date("\\Y=Y", &c, 0), "Y=2005");
    }

    #[test]
    fn strftime_codes() {
        let c = civil_from_unix(1_109_680_496);
        assert_eq!(strftime("%Y-%m-%d", &c), "2005-03-01");
        assert_eq!(strftime("%A %B", &c), "Tuesday March");
        assert_eq!(strftime("%H:%M:%S", &c), "12:34:56");
    }

    #[test]
    fn timestamp_string_parsing() {
        // τ = 2005-03-01 12:34:56 UTC, used for `now` and year-less defaults.
        let tau = 1_109_680_496;
        let fmt = |raw: &str| parse_timestamp(raw, tau).map(|u| {
            let c = civil_from_unix(u);
            format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", c.year, c.month, c.day, c.hour, c.min, c.sec)
        });
        // Y-M-D (single and zero-padded), the form CS1's Configuration feeds.
        assert_eq!(fmt("2022-1-1").as_deref(), Some("2022-01-01 00:00:00"));
        assert_eq!(fmt("2022-03-09").as_deref(), Some("2022-03-09 00:00:00"));
        // ISO with time.
        assert_eq!(fmt("2022-03-09T05:06:07Z").as_deref(), Some("2022-03-09 05:06:07"));
        // MediaWiki 14-digit stamp and bare year.
        assert_eq!(fmt("20200215123000").as_deref(), Some("2020-02-15 12:30:00"));
        assert_eq!(fmt("1999").as_deref(), Some("1999-01-01 00:00:00"));
        // Month-name forms, including the glued "1January" and year-less
        // (defaults to τ's year).
        assert_eq!(fmt("1 January 2020").as_deref(), Some("2020-01-01 00:00:00"));
        assert_eq!(fmt("January 1, 2020").as_deref(), Some("2020-01-01 00:00:00"));
        assert_eq!(fmt("August 2017").as_deref(), Some("2017-08-01 00:00:00"));
        assert_eq!(fmt("1January").as_deref(), Some("2005-01-01 00:00:00"));
        assert_eq!(fmt("now").as_deref(), Some("2005-03-01 12:34:56"));
        // Unrecognized returns None (the caller surfaces "invalid timestamp").
        assert_eq!(parse_timestamp("not a date", tau), None);
    }
}
