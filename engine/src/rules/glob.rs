// A faithful re-implementation of the wcmatch glob vocabulary used by the
// Python engine's `_glob_match` (wcmatch.glob.globmatch with GLOBSTAR | EXTGLOB
// | BRACE | DOTGLOB). The Python side and this side MUST agree on every
// (pattern, string) pair — that is what test_rules_parity_rs cross-checks.
//
// Semantics implemented:
//   *          run of NON-`/` chars (stays within one path segment)
//   ?          one NON-`/` char
//   **         a GLOBSTAR segment — crosses `/`; `**/` = any depth (incl. zero),
//              a trailing `/**` matches everything below, a bare `**` matches all
//   [..]       a bracket char class ([!..]/[^..] negation, a-z ranges)
//   @(a|b) !(x) +(p) *(p) ?(p)   EXTGLOB groups (| separates alternatives)
//   {a,b}      BRACE alternation (expanded up front; nesting + commas honoured)
//   DOTGLOB    a leading `*` DOES match a leading dot (no implicit hidden-skip)
//
// Matching is anchored (whole-string, like globmatch) and `*`/extglob never
// cross `/`; only a `**` globstar segment does.

/// Whole-string match of extended glob `pat` against `s`.
pub fn globmatch(pat: &str, s: &str) -> bool {
    for alt in expand_braces(pat) {
        if match_pattern(&alt, s) {
            return true;
        }
    }
    false
}

/// Whole-string match where the subject is plain TEXT, not a path: `*` and
/// `?` cross `/` like any other character. Implemented by mapping `/` in
/// both pattern and subject to a private sentinel byte (0x1f, never present
/// in either), which neutralises the matcher's path-segment special-casing
/// while keeping literal `/` in a pattern matching literal `/` in the text.
pub fn textmatch(pat: &str, s: &str) -> bool {
    const SENTINEL: char = '\u{1f}';
    globmatch(
        &pat.replace('/', &SENTINEL.to_string()),
        &s.replace('/', &SENTINEL.to_string()),
    )
}

// ── brace expansion ──────────────────────────────────────────────────────────
// {a,b}c → [ac, bc]; nested braces and multiple groups expand combinatorially.
// A `{` with no matching `}` or no top-level comma is treated literally (the
// wcmatch behaviour: an unbalanced/comma-less brace is not a brace group).
fn expand_braces(pat: &str) -> Vec<String> {
    let chars: Vec<char> = pat.chars().collect();
    // Find the first top-level brace group with a comma.
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '{' {
            if let Some((close, parts, has_comma)) = scan_group(&chars, i) {
                if has_comma {
                    let prefix: String = chars[..i].iter().collect();
                    let suffix: String = chars[close + 1..].iter().collect();
                    let mut out = vec![];
                    for p in parts {
                        // recursively expand each alternative + the suffix
                        for tail in expand_braces(&format!("{p}{suffix}")) {
                            out.push(format!("{prefix}{tail}"));
                        }
                    }
                    return out;
                }
            }
        }
        i += 1;
    }
    vec![pat.to_string()]
}

/// Scan a `{...}` group starting at `open`. Returns (close_index, top-level
/// comma-separated parts, had_comma). Honours nested braces and `(`-extglob
/// (commas inside extglob groups are NOT brace separators). None if unbalanced.
fn scan_group(chars: &[char], open: usize) -> Option<(usize, Vec<String>, bool)> {
    let mut depth_brace = 0i32;
    let mut depth_paren = 0i32;
    let mut parts: Vec<String> = vec![];
    let mut cur = String::new();
    let mut had_comma = false;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                cur.push(c);
                if i + 1 < chars.len() {
                    cur.push(chars[i + 1]);
                    i += 1;
                }
            }
            '{' => {
                depth_brace += 1;
                if depth_brace > 1 {
                    cur.push(c);
                }
            }
            '}' => {
                depth_brace -= 1;
                if depth_brace == 0 {
                    parts.push(std::mem::take(&mut cur));
                    return Some((i, parts, had_comma));
                }
                cur.push(c);
            }
            '(' => {
                depth_paren += 1;
                cur.push(c);
            }
            ')' => {
                if depth_paren > 0 {
                    depth_paren -= 1;
                }
                cur.push(c);
            }
            ',' if depth_brace == 1 && depth_paren == 0 => {
                had_comma = true;
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    None // unbalanced
}

// ── the core matcher ──────────────────────────────────────────────────────────
// Recursive backtracking over the pattern chars `p` (from `pi`) vs the string
// chars `s` (from `si`). Anchored: succeeds only when both are fully consumed.
fn match_pattern(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    m(&p, 0, &t, 0)
}

fn m(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    let mut pi = pi;
    let mut ti = ti;
    loop {
        if pi >= p.len() {
            return ti >= t.len();
        }
        let c = p[pi];
        // ── globstar: `**` occupying a whole segment crosses `/`. ──
        if c == '*' && pi + 1 < p.len() && p[pi + 1] == '*' && is_segment_globstar(p, pi) {
            // Determine the rest after the globstar, skipping a following `/`.
            let mut rest = pi + 2;
            let ate_slash = rest < p.len() && p[rest] == '/';
            if ate_slash {
                rest += 1;
            }
            // `**` (or `**/`) matches zero or more whole path segments.
            // Try consuming 0..=remaining segments of `t` from `ti`.
            // Case A: matches zero segments — rest matches from ti directly.
            if m(p, rest, t, ti) {
                return true;
            }
            // Case B: matches one-or-more segments: advance ti over segment
            // boundaries. `**` can also match within the current partial segment
            // start; emulate by trying every position that is either ti..end or
            // right after each '/'.
            let mut k = ti;
            while k < t.len() {
                if t[k] == '/' {
                    // after this slash, try matching the rest
                    if m(p, rest, t, k + 1) {
                        return true;
                    }
                    // also (for `**` not followed by `/`) allow rest to start
                    // mid-next-segment is handled by '*' logic; here only
                    // segment-aligned positions.
                }
                k += 1;
            }
            // If `**` was NOT followed by `/` (e.g. `a/**`), it also matches the
            // remainder of the string including the final segment with no
            // trailing slash — try every position.
            if !ate_slash {
                let mut k = ti;
                loop {
                    if m(p, rest, t, k) {
                        return true;
                    }
                    if k >= t.len() {
                        break;
                    }
                    k += 1;
                }
            }
            return false;
        }
        // ── extglob group: X(...) where X in @ ! + * ? ──
        if (c == '@' || c == '!' || c == '+' || c == '*' || c == '?')
            && pi + 1 < p.len()
            && p[pi + 1] == '('
        {
            if let Some((alts, after)) = parse_extglob(p, pi) {
                return match_extglob(c, &alts, p, after, t, ti);
            }
            // not a valid group: fall through to literal handling of c
        }
        match c {
            '*' => {
                // matches a run of NON-`/` chars (greedy w/ backtracking).
                // try zero-length first then extend up to the next '/'.
                let mut k = ti;
                loop {
                    if m(p, pi + 1, t, k) {
                        return true;
                    }
                    if k >= t.len() || t[k] == '/' {
                        return false;
                    }
                    k += 1;
                }
            }
            '?' => {
                if ti >= t.len() || t[ti] == '/' {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            '[' => {
                if let Some((matched, len, ok)) = match_class(p, pi, t, ti) {
                    if !ok || !matched {
                        return false;
                    }
                    pi += len;
                    ti += 1;
                } else {
                    // unbalanced '[' → literal
                    if ti >= t.len() || t[ti] != '[' {
                        return false;
                    }
                    pi += 1;
                    ti += 1;
                }
            }
            '\\' => {
                let lit = if pi + 1 < p.len() { p[pi + 1] } else { '\\' };
                if ti >= t.len() || t[ti] != lit {
                    return false;
                }
                pi += 2;
                ti += 1;
            }
            _ => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
}

/// Is the `**` at `pi` a standalone GLOBSTAR segment? wcmatch treats `**` as a
/// globstar only when it forms a whole segment: preceded by start-or-`/` and
/// followed by end-or-`/`. Otherwise `**` is two ordinary `*` (same-segment).
fn is_segment_globstar(p: &[char], pi: usize) -> bool {
    let before_ok = pi == 0 || p[pi - 1] == '/';
    let after = pi + 2;
    let after_ok = after >= p.len() || p[after] == '/';
    before_ok && after_ok
}

/// Parse an extglob group starting at `pi` (`X(`). Returns (alternatives, index
/// just past the closing `)`). Honours nested parens and `|` separators.
fn parse_extglob(p: &[char], pi: usize) -> Option<(Vec<String>, usize)> {
    let mut depth = 0i32;
    let mut alts: Vec<String> = vec![];
    let mut cur = String::new();
    let mut i = pi + 1; // at '('
    while i < p.len() {
        let c = p[i];
        match c {
            '\\' => {
                cur.push(c);
                if i + 1 < p.len() {
                    cur.push(p[i + 1]);
                    i += 1;
                }
            }
            '(' => {
                depth += 1;
                if depth > 1 {
                    cur.push(c);
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    alts.push(std::mem::take(&mut cur));
                    return Some((alts, i + 1));
                }
                cur.push(c);
            }
            '|' if depth == 1 => alts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
        i += 1;
    }
    None
}

/// Match an extglob `op(alts)` at the front of `t[ti..]`, then continue with the
/// pattern tail `p[after..]`. `op` ∈ @ ! + * ?.
fn match_extglob(
    op: char,
    alts: &[String],
    p: &[char],
    after: usize,
    t: &[char],
    ti: usize,
) -> bool {
    // Helper: does an alternative pattern match exactly `t[ti..end]` for some
    // `end`, returning all such ends? We brute-force end positions within the
    // current segment (extglob, like *, does not cross '/').
    let seg_end = {
        let mut e = ti;
        while e < t.len() && t[e] != '/' {
            e += 1;
        }
        e
    };
    // alt_ends(start): set of positions `e` (start<=e<=seg_end) such that one of
    // the alternatives matches t[start..e] exactly.
    let alt_ends = |start: usize| -> Vec<usize> {
        let mut ends = vec![];
        for a in alts {
            let ap: Vec<char> = a.chars().collect();
            for e in start..=seg_end {
                if m(&ap, 0, &t[start..e], 0) {
                    ends.push(e);
                }
            }
        }
        ends.sort_unstable();
        ends.dedup();
        ends
    };
    match op {
        '@' => {
            for e in alt_ends(ti) {
                if m(p, after, t, e) {
                    return true;
                }
            }
            false
        }
        '?' => {
            // zero or one
            if m(p, after, t, ti) {
                return true;
            }
            for e in alt_ends(ti) {
                if m(p, after, t, e) {
                    return true;
                }
            }
            false
        }
        '+' | '*' => {
            // one-or-more (+) / zero-or-more (*) repetitions. BFS over reachable
            // positions within the segment.
            let mut reachable = std::collections::BTreeSet::new();
            if op == '*' {
                reachable.insert(ti);
            }
            let mut frontier = vec![ti];
            // seed first repetition
            let mut seen = std::collections::BTreeSet::new();
            seen.insert(ti);
            // We want all positions reachable by >=1 (or >=0) reps.
            let mut positions = std::collections::BTreeSet::new();
            if op == '*' {
                positions.insert(ti);
            }
            while let Some(pos) = frontier.pop() {
                for e in alt_ends(pos) {
                    if e == pos {
                        continue;
                    } // avoid empty-match infinite loop
                    positions.insert(e);
                    if seen.insert(e) {
                        frontier.push(e);
                    }
                }
            }
            let _ = reachable;
            for pos in positions {
                if m(p, after, t, pos) {
                    return true;
                }
            }
            false
        }
        '!' => {
            // matches any string (within the segment) that does NOT match any
            // alternative as a WHOLE. Try every segment-internal end e; the
            // consumed span t[ti..e] must not equal any alternative.
            for e in ti..=seg_end {
                let span = &t[ti..e];
                let mut neg_matches = false;
                for a in alts {
                    let ap: Vec<char> = a.chars().collect();
                    if m(&ap, 0, span, 0) {
                        neg_matches = true;
                        break;
                    }
                }
                if !neg_matches && m(p, after, t, e) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Match a `[...]` bracket class at `p[pi]` against `t[ti]`. Returns
/// (matched, pattern_len_consumed, ok) or None if the class is unterminated.
fn match_class(p: &[char], pi: usize, t: &[char], ti: usize) -> Option<(bool, usize, bool)> {
    // find closing ']'
    let mut j = pi + 1;
    let negate = j < p.len() && (p[j] == '!' || p[j] == '^');
    if negate {
        j += 1;
    }
    // a ']' immediately after the (optional) negation is a literal member.
    let body_start = j;
    if j < p.len() && p[j] == ']' {
        j += 1;
    }
    while j < p.len() && p[j] != ']' {
        j += 1;
    }
    if j >= p.len() {
        return None;
    } // unterminated
    let close = j;
    if ti >= t.len() || t[ti] == '/' {
        return Some((false, close - pi + 1, true));
    }
    let ch = t[ti];
    let mut matched = false;
    let mut k = body_start;
    while k < close {
        // range a-z
        if k + 2 < close && p[k + 1] == '-' {
            let lo = p[k];
            let hi = p[k + 2];
            if lo <= ch && ch <= hi {
                matched = true;
            }
            k += 3;
        } else {
            if p[k] == ch {
                matched = true;
            }
            k += 1;
        }
    }
    if negate {
        matched = !matched;
    }
    Some((matched, close - pi + 1, true))
}
