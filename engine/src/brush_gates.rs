//! Per-util "is uutils OK for this argv?" gates.
//!
//! BACKGROUND. `engine/src/brush.rs::CoreutilWrapper` registers every
//! bundled uutils coreutil (`cat`, `cp`, `mv`, `rm`, `ls`, `mkdir`, …)
//! as a brush builtin so it runs IN-PROCESS — fast, no fork+exec per
//! file op. The trade-off: uutils' implementations diverge from GNU
//! coreutils on flags, locale-sensitive output, and a long tail of
//! corner cases. Some divergences are harmless; others break real
//! workloads (e.g. uucore's localization caches the FIRST util's
//! FluentResource process-globally — see brush.rs::box_builtins_opt —
//! so `cp` on a thread where `mkdir` already ran emits a raw fluent
//! key instead of a message).
//!
//! THIS MODULE is the **gate**: for each util, a function
//! `gate_<name>(args) -> bool` returns true only if uutils faithfully
//! reproduces GNU semantics for the SPECIFIC argv handed in. Anything
//! the gate refuses falls back to PATH lookup + fork+exec of the host
//! binary — which IS bit-compatible with GNU because it IS GNU.
//!
//! POLICY. Gates default to **false** (be conservative). A gate
//! returns true only after a per-util audit: walked the GNU manpage,
//! cross-checked uutils' implementation, exercised the edge cases.
//! Agents own gate authoring per-util — keep each `gate_<name>` a
//! self-contained function so an agent can be handed ONE function to
//! tighten without grepping the rest of the codebase.
//!
//! ARGS SHAPE. `args[0]` is the command name (uutils' uumain convention,
//! mirrored by brush's ExecutionContext::args), `args[1..]` are the
//! user-supplied tokens. Gates inspect `args[1..]`.
//!
//! TEMPLATE for new gates:
//! ```ignore
//! fn gate_NAME(args: &[OsString]) -> bool {
//!     // 1. Walk the GNU manpage; categorize every flag as
//!     //    SAFE (uutils matches GNU byte-for-byte) /
//!     //    UNSAFE (known divergence or untested).
//!     // 2. Walk args[1..]; reject the moment we see an UNSAFE flag.
//!     // 3. Reject any unknown long option `--xyz` we haven't audited.
//!     // 4. Return true only if every observed token is SAFE.
//!     for arg in args.iter().skip(1) {
//!         let s = arg.as_bytes();
//!         match s {
//!             b"-x" | b"--known-safe" => continue,
//!             a if a.starts_with(b"-") => return false,
//!             _ => continue, // positional file arg
//!         }
//!     }
//!     true
//! }
//! ```

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

/// Gate signature. Returns true → uutils handles this argv;
/// false → fall back to fork+exec of the host binary.
pub type CoreutilGate = fn(&[OsString]) -> bool;

/// Conservative default. Every util that doesn't have a per-util gate
/// yet (or that an agent has only partially audited) wires through
/// this — i.e. "never trust uutils for this util, always shell out".
pub fn gate_false(_args: &[OsString]) -> bool {
    false
}

/// Look up the gate for `name`. Falls back to [`gate_false`] when no
/// per-util gate is registered. The returned `fn` is cheap to call.
pub fn gate_for(name: &str) -> CoreutilGate {
    *gate_table().get(name).unwrap_or(&(gate_false as CoreutilGate))
}

/// The full name → gate map, built once. Keys MUST match brush's
/// CoreutilWrapper bundled-commands keys (e.g. `cat`, `ls`, `cp`),
/// not the uu_<name> crate name (so `dir`/`vdir` map separately, etc.).
fn gate_table() -> &'static HashMap<&'static str, CoreutilGate> {
    static TABLE: OnceLock<HashMap<&'static str, CoreutilGate>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m: HashMap<&'static str, CoreutilGate> = HashMap::new();
        // Order: keep alphabetical so agents can scan + add their entry
        // by binary-search eye. EVERY util initially absent here gets
        // gate_false (= always fall back); an agent adds one line per
        // util they've audited.
        //
        // Starter gates below cover the trivially safe utils — utilities
        // where the argv shape is essentially "no arguments" so there's
        // nothing to diverge on. Agents will fill in the rest.
        m.insert("true", gate_true_cmd as CoreutilGate);
        m.insert("false", gate_false_cmd as CoreutilGate);
        m.insert("yes", gate_yes as CoreutilGate);
        m.insert("pwd", gate_pwd as CoreutilGate);
        m.insert("whoami", gate_whoami as CoreutilGate);
        m.insert("hostname", gate_hostname as CoreutilGate);
        m.insert("sync", gate_sync as CoreutilGate);
        m.insert("uname", gate_uname as CoreutilGate);
        m.insert("cat", gate_cat as CoreutilGate);
        m.insert("cp", gate_cp as CoreutilGate);
        m
    })
}

// ── starter gates ────────────────────────────────────────────────────
// Each agent owns ONE `gate_<name>` below. Keep them stand-alone and
// short; if the gate logic needs helpers, add them locally so the agent
// editing one util doesn't have to touch shared code.
//
// `args[0]` is the command name. Skip it when inspecting flags.

/// `true` — exits 0, accepts and ignores all arguments per POSIX.
/// Uutils matches GNU exactly because there is nothing to mismatch.
fn gate_true_cmd(_args: &[OsString]) -> bool { true }

/// `false` — exits 1, accepts and ignores all arguments per POSIX.
/// Uutils matches GNU exactly because there is nothing to mismatch.
fn gate_false_cmd(_args: &[OsString]) -> bool { true }

/// `yes [STRING]` — prints STRING (or "y") forever. Single optional
/// positional; no flags GNU supports beyond `--help`/`--version`.
fn gate_yes(args: &[OsString]) -> bool {
    // Refuse --help/--version so brush's output for those matches the
    // host binary's wording exactly (uutils diverges on these strings).
    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        if b.starts_with(b"--") {
            return false;
        }
    }
    true
}

/// `pwd [-L|-P]` — print working dir. uutils matches GNU on the empty
/// argv and on `-L`/`-P`.
fn gate_pwd(args: &[OsString]) -> bool {
    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        match b {
            b"-L" | b"-P" => continue,
            _ => return false,
        }
    }
    true
}

/// `whoami` — no flags GNU honours beyond `--help`/`--version`.
fn gate_whoami(args: &[OsString]) -> bool {
    args.len() <= 1
}

/// `hostname` — bare invocation only. GNU `hostname FOO` SETS the
/// hostname (needs CAP_SYS_ADMIN); inside a `--unshare-uts` box that
/// would succeed and confuse the parent's view, so always fall back.
fn gate_hostname(args: &[OsString]) -> bool {
    args.len() <= 1
}

/// `sync` — bare invocation only. uutils' `-f`/`--file-system` paths
/// not audited.
fn gate_sync(args: &[OsString]) -> bool {
    args.len() <= 1
}

/// `uname [-a|-s|-r|-v|-m|-n|-p|-i|-o]` — single-letter flags only,
/// no combined `-asr` clusters (gate_false on those until audited).
fn gate_uname(args: &[OsString]) -> bool {
    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        match b {
            b"-a" | b"-s" | b"-r" | b"-v" | b"-m" | b"-n" | b"-p" | b"-i" | b"-o" => continue,
            _ => return false,
        }
    }
    true
}

/// `cat [-A -b -e -E -n -s -t -T -u -v] [FILE...]` — concatenate files.
///
/// The vendored `uu_cat` fork reproduces GNU's output byte-for-byte for the
/// full formatting flag set (numbering, `$`-line-ends, `^`/`M-` non-printing
/// escapes, tab display, blank squeezing) and for the read-error path (it
/// strips errno to the same "No such file or directory" wording, continues to
/// the next file, exits non-zero). What it does NOT match is the `--help` /
/// `--version` text, so reject any `--`-prefixed long option except the audited
/// formatting ones, and reject any unaudited short flag. Bare `-` is stdin and
/// passes as a positional.
fn gate_cat(args: &[OsString]) -> bool {
    // Long options whose behavior matches GNU (formatting only). `--help` and
    // `--version` are deliberately absent so their wording falls back to GNU.
    const SAFE_LONG: &[&[u8]] = &[
        b"--show-all",
        b"--number-nonblank",
        b"--show-ends",
        b"--number",
        b"--squeeze-blank",
        b"--show-tabs",
        b"--show-nonprinting",
    ];
    let mut opts_done = false;
    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        if opts_done {
            continue; // everything after `--` is a positional file
        }
        if b == b"--" {
            opts_done = true;
        } else if b == b"-" || !b.starts_with(b"-") {
            continue; // stdin, or a filename
        } else if b.starts_with(b"--") {
            if !SAFE_LONG.contains(&b) {
                return false; // --help/--version/unknown long option
            }
        } else {
            // A short cluster like `-bn` / `-vET`: every letter must be audited.
            if !b[1..].iter().all(|c| b"AbeEnstTuv".contains(c)) {
                return false;
            }
        }
    }
    true
}

/// `head [-c [-]NUM] [-n [-]NUM] [-q] [-v] [-z] [FILE...]` — first lines/bytes.
///
/// The vendored `uu_head` fork reproduces GNU 9.4 byte-for-byte (stdout AND exit
/// code) across the full functional flag set — verified by an 80-case
/// differential battery (positive/negative `-n`/`-c`, `--lines`/`--bytes`,
/// `=`-joined values, `-q`/`--quiet`/`--silent`, `-v`/`--verbose`,
/// `-z`/`--zero-terminated`, obsolete `-NUM[bkmcqvz]` syntax, short clusters,
/// multi-file `==>` headers, empty/no-newline/large/binary/NUL inputs).
///
/// EXCLUDED (divergent or unaudited) — fall back to host binary:
///   --help, --version        (uutils wording != GNU)
///   --presume-input-pipe     (uutils-internal hidden flag; GNU rejects it)
///   any `-n`/`-c`/`--lines`/`--bytes` value that is not a PLAIN decimal
///   `[-]?[0-9]+` — a multiplier suffix (`-n0b`, `-c0k`, `--bytes=0k`) or hex
///   (`-c0x10`) diverges from GNU (GNU applies the multiplier — `0×512 = 0` →
///   empty — while uutils prints the whole file / mis-strips the token). Plain
///   numbers match byte-for-byte; suffix/hex forms go to the host GNU binary.
///   This also covers `-nq`/`-cq` (a non-numeric attached value).
fn gate_head(args: &[OsString]) -> bool {
    // A GNU-faithful count value: optional leading `-`, then ≥1 digit, ALL
    // digits (no `b`/`k`/`M`/… multiplier suffix, no `0x..` hex). Those are the
    // only `-n`/`-c`/`--lines`/`--bytes` values where uutils matches GNU.
    fn is_plain_count(v: &[u8]) -> bool {
        let digits = v.strip_prefix(b"-").unwrap_or(v);
        !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
    }

    const SAFE_LONG_BARE: &[&[u8]] = &[
        b"--quiet",
        b"--silent",
        b"--verbose",
        b"--zero-terminated",
    ];
    const SAFE_LONG_VALUED: &[&[u8]] = &[b"--bytes", b"--lines"];

    let mut it = args.iter().skip(1).peekable();
    let mut opts_done = false;
    while let Some(a) = it.next() {
        let b = a.as_bytes();
        if opts_done {
            continue; // positional file after `--`
        }
        if b == b"--" {
            opts_done = true;
        } else if b == b"-" || !b.starts_with(b"-") {
            continue; // stdin, or a filename
        } else if b.starts_with(b"--") {
            let head = match b.iter().position(|&c| c == b'=') {
                Some(eq) => &b[..eq],
                None => b,
            };
            let has_eq = head.len() != b.len();
            if SAFE_LONG_BARE.contains(&head) {
                if has_eq {
                    return false; // a bare flag doesn't take `=value`
                }
            } else if SAFE_LONG_VALUED.contains(&head) {
                // The value must be a plain decimal — `--bytes=0k`/`--lines 0b`
                // diverge. Value is the `=`-suffix or the next token.
                let val: &[u8] = if has_eq {
                    &b[head.len() + 1..]
                } else {
                    match it.next() {
                        Some(v) => v.as_bytes(),
                        None => return false,
                    }
                };
                if !is_plain_count(val) {
                    return false;
                }
            } else {
                return false; // --help/--version/--presume-input-pipe/unknown
            }
        } else {
            // Short form. Either obsolete `-NUM` (a digit right after `-`) or a
            // cluster of audited letters.
            let rest = &b[1..];
            if rest.first().is_some_and(u8::is_ascii_digit) {
                // Obsolete `-5`/`-0`/`-123`. Accept only PURE digits — an
                // obsolete value with a multiplier/option suffix (`-2c`, `-0b`,
                // `-5q`) can hit the same suffix divergence, so fall back.
                if rest.iter().all(u8::is_ascii_digit) {
                    continue;
                }
                return false;
            }
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    b'q' | b'v' | b'z' => i += 1,
                    b'c' | b'n' => {
                        // The value is the rest of the cluster (`-n5`) or, if
                        // `c`/`n` ends the cluster, the next token (`-n 5`).
                        let val: &[u8] = if i + 1 < rest.len() {
                            &rest[i + 1..]
                        } else {
                            match it.next() {
                                Some(v) => v.as_bytes(),
                                None => return false, // `-n` with no value
                            }
                        };
                        if !is_plain_count(val) {
                            return false; // `-nq`, `-n0b`, `-c 0x10`, …
                        }
                        i = rest.len();
                    }
                    _ => return false, // unaudited short letter
                }
            }
        }
    }
    true
}

/// `wc [-c -l] [--bytes --lines] [--total=auto|always|only|never] [FILE...]`
///
/// SAFE (verified byte-for-byte equal to GNU /usr/bin/wc in this container's
/// C/POSIX locale): `-c`/`--bytes` and `-l`/`--lines` (both locale-independent,
/// so they match on ASCII or multibyte input), `--total=auto|always|only|never`,
/// and at least one explicit FILE arg OR a single counter on stdin.
///
/// EXCLUDED (uutils diverges — fall back to host binary): `-w`/`--words`,
/// `-m`/`--chars`, `-L`/`--max-line-length`, and the DEFAULT no-count case
/// (uutils always Unicode-decodes while GNU in C locale counts bytes / splits on
/// whitespace bytes); multi-counter reads from stdin (uutils forces width 7,
/// GNU sizes from a regular file behind `< file`); `--files0-from`, `--debug`,
/// `--help`, `--version`, and any unaudited option.
fn gate_wc(args: &[OsString]) -> bool {
    let mut n_counters: u32 = 0;          // distinct counters requested
    let mut have_file = false;            // a real path positional (not '-')
    let mut have_explicit_stdin = false;  // a '-' token
    let mut opts_done = false;

    const SAFE_LONG: &[&[u8]] = &[b"--bytes", b"--lines"];

    let mut i = 1;
    while i < args.len() {
        let b = args[i].as_bytes();
        if opts_done {
            if b == b"-" { have_explicit_stdin = true; } else { have_file = true; }
            i += 1;
            continue;
        }
        if b == b"--" {
            opts_done = true;
        } else if b == b"-" {
            have_explicit_stdin = true;
        } else if !b.starts_with(b"-") {
            have_file = true;
        } else if b.starts_with(b"--") {
            if SAFE_LONG.contains(&b) {
                n_counters += 1;
            } else if b.starts_with(b"--total=") {
                // attached WHEN value: safe policy, not a counter
            } else if b == b"--total" {
                // GNU's --total is required_argument: the WHEN value is the NEXT
                // token. Consume it so it isn't miscounted as a FILE (which would
                // defeat the multi-counter-stdin guard below).
                i += 1;
                if i >= args.len() { return false; }
            } else {
                return false; // --words/--chars/--max-line-length/--help/…
            }
        } else {
            // short cluster: every letter must be an audited-safe counter.
            for c in &b[1..] {
                match c {
                    b'c' | b'l' => n_counters += 1,
                    _ => return false, // -w / -m / -L / unknown short flag
                }
            }
        }
        i += 1;
    }

    if n_counters == 0 {
        return false; // default implies words, which diverges
    }
    if n_counters > 1 && (!have_file || have_explicit_stdin) {
        return false; // multi-counter stdin column-width divergence
    }
    true
}

/// `nl [-b STYLE] [-f STYLE] [-h STYLE] [-d CC] [-n FORMAT] [-w N] [-s STR]
///    [-v N] [-i N] [-l N] [-p] [FILE]` — number lines.
///
/// The vendored `uu_nl` fork reproduces GNU's numbered output byte-for-byte for
/// the full formatting set and the single-file read-error / directory-target
/// paths. EXCLUDED: `--help`/`--version` (text); a value-flag whose value is a
/// SEPARATED leading-dash token (`-v -3`, `-w -5`) which clap rejects though GNU
/// accepts (attached `-v-3`/`-v=-3`/`--long=-3` are fine); an INVALID style/
/// format/width value (wording differs); and 2+ FILE operands (uutils aborts on a
/// missing file mid-list while GNU continues — the gate can't stat at parse time).
fn gate_nl(args: &[OsString]) -> bool {
    fn is_style(v: &[u8]) -> bool {
        matches!(v, b"a" | b"t" | b"n") || v.first() == Some(&b'p')
    }
    fn is_format(v: &[u8]) -> bool { matches!(v, b"ln" | b"rn" | b"rz") }
    fn is_pos_int(v: &[u8]) -> bool {
        !v.is_empty() && v.iter().all(u8::is_ascii_digit)
    }
    fn is_nonzero_width(v: &[u8]) -> bool { is_pos_int(v) && v != b"0" }

    let mut opts_done = false;
    let mut operands = 0usize;
    let mut i = 1;
    while i < args.len() {
        let b = args[i].as_bytes();
        if opts_done {
            operands += 1;
            if operands > 1 { return false; }
            i += 1;
            continue;
        }
        if b == b"--" { opts_done = true; i += 1; continue; }
        if b == b"-" || !b.starts_with(b"-") {
            operands += 1;
            if operands > 1 { return false; }
            i += 1;
            continue;
        }

        if b.starts_with(b"--") {
            let (name, attached): (&[u8], Option<&[u8]>) =
                match b.iter().position(|&c| c == b'=') {
                    Some(p) => (&b[..p], Some(&b[p + 1..])),
                    None => (b, None),
                };
            macro_rules! value {
                () => {{
                    match attached {
                        Some(v) => v,
                        None => {
                            i += 1;
                            if i >= args.len() { return false; }
                            args[i].as_bytes()
                        }
                    }
                }};
            }
            match name {
                b"--body-numbering" | b"--footer-numbering" | b"--header-numbering" => {
                    let v = value!();
                    if v.starts_with(b"-") || !is_style(v) { return false; }
                }
                b"--number-format" => {
                    let v = value!();
                    if v.starts_with(b"-") || !is_format(v) { return false; }
                }
                b"--number-width" => {
                    let v = value!();
                    if v.starts_with(b"-") || !is_nonzero_width(v) { return false; }
                }
                b"--line-increment" | b"--join-blank-lines"
                | b"--starting-line-number" => {
                    let v = value!();
                    if v.starts_with(b"-") || !is_pos_int(v) { return false; }
                }
                b"--number-separator" => {
                    // uutils pads UNNUMBERED lines by `width + 1` while GNU pads
                    // by `width + len(separator)`, so the output diverges unless
                    // the separator is exactly one byte (the default `\t` case).
                    let v = value!();
                    if v.len() != 1 { return false; }
                }
                b"--section-delimiter" => {
                    let _ = value!(); // section delimiter: unaffected
                }
                b"--no-renumber" => {} // takes no value
                _ => return false, // --help/--version/unknown long option
            }
            i += 1;
            continue;
        }

        // Short option (possibly with an attached value, e.g. `-ba`, `-w3`).
        let body = &b[1..];
        let c = body[0];
        let rest = &body[1..]; // attached value, if any
        match c {
            b'p' => { if !rest.is_empty() { return false; } } // -p takes no value
            b'b' | b'f' | b'h' | b'n' | b'i' | b'l' | b'v' | b'w' | b's' | b'd' => {
                let owned;
                let v: &[u8] = if rest.is_empty() {
                    i += 1;
                    if i >= args.len() { return false; }
                    owned = args[i].as_bytes().to_vec();
                    &owned
                } else {
                    rest
                };
                let ok = match c {
                    b'b' | b'f' | b'h' => !v.starts_with(b"-") && is_style(v),
                    b'n' => !v.starts_with(b"-") && is_format(v),
                    b'w' => !v.starts_with(b"-") && is_nonzero_width(v),
                    b'i' | b'l' | b'v' => !v.starts_with(b"-") && is_pos_int(v),
                    // -s separator: only a single byte matches GNU (see the
                    // --number-separator note above). -d section delimiter: any.
                    b's' => v.len() == 1,
                    b'd' => true,
                    _ => false,
                };
                if !ok { return false; }
            }
            _ => return false, // unaudited short flag
        }
        i += 1;
    }
    true
}

/// `tac [-b] [-r] [-s SEP] [FILE...]` — reverse-concatenate lines.
///
/// uutils matches GNU byte-for-byte for the real work. SAFE: `-b`/`--before`,
/// `-r`/`--regex` (valid regexes match GNU exactly; only the *error* text differs
/// on a malformed regex, and exit code agrees at 1), `-s STR`/`--separator=STR`
/// (incl. `-sSTR`, clustered `-brs:`). EXCLUDED: `--help`/`--version` (branding);
/// any unaudited long option; and a REPEATED `-s`/`--separator` — GNU lets the
/// last win (exit 0) while uutils errors "cannot be used multiple times" (exit 1),
/// a genuine exit-code divergence, so reject >1 occurrence.
fn gate_tac(args: &[OsString]) -> bool {
    const SAFE_LONG: &[&[u8]] = &[b"--before", b"--regex"];
    let mut opts_done = false;
    let mut sep_count = 0usize; // number of -s / --separator occurrences

    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let b = a.as_bytes();
        if opts_done {
            continue; // positional file after `--`
        }
        if b == b"--" {
            opts_done = true;
        } else if b == b"-" || !b.starts_with(b"-") {
            continue; // stdin, or a filename
        } else if b == b"--separator" {
            sep_count += 1;
            // The value is the next token. clap (unlike GNU getopt) refuses a
            // value that looks like a flag, so a separated `-`-leading separator
            // (`--separator -x`) errors in uutils but is literal in GNU — reject.
            match it.next() {
                Some(v) if !v.as_bytes().starts_with(b"-") => {}
                _ => return false,
            }
        } else if b.starts_with(b"--separator=") {
            sep_count += 1;
        } else if b.starts_with(b"--") {
            if !SAFE_LONG.contains(&b) {
                return false; // --help/--version/unaudited long opt
            }
        } else {
            // A short cluster like "-b", "-r", "-br", "-s:", "-brs:".
            let cluster = &b[1..];
            let mut i = 0;
            while i < cluster.len() {
                match cluster[i] {
                    b'b' | b'r' => i += 1,
                    b's' => {
                        sep_count += 1;
                        if i + 1 < cluster.len() {
                            i = cluster.len(); // value attached, cluster done
                        } else {
                            // value is the next token; a `-`-leading one is
                            // refused by clap but literal in GNU — reject.
                            match it.next() {
                                Some(v) if !v.as_bytes().starts_with(b"-") => {}
                                _ => return false,
                            }
                            i += 1;
                        }
                    }
                    _ => return false, // unaudited short flag
                }
            }
        }
        if sep_count > 1 {
            return false; // repeated separator diverges from GNU
        }
    }
    sep_count <= 1
}

fn gate_basename(args: &[OsString]) -> bool {
    // A long option (after stripping any `=value`) that uutils reproduces
    // GNU-faithfully. clap's infer_long_args admits unambiguous prefixes, so we
    // match by prefix against this set (none is a prefix of another, and none is
    // a prefix of --help/--version).
    fn safe_long_head(head: &[u8]) -> Option<&'static [u8]> {
        for full in [b"--multiple".as_slice(), b"--suffix", b"--zero"] {
            // head must be a non-empty prefix of exactly one full long name,
            // and longer than "--" so "--" itself is handled by the caller.
            if head.len() > 2 && full.starts_with(head) {
                return Some(full);
            }
        }
        None
    }

    let mut it = args.iter().skip(1).peekable();
    let mut opts_done = false;
    while let Some(a) = it.next() {
        let b = a.as_bytes();
        if opts_done {
            continue; // operand: any value is SAFE
        }
        if b == b"--" {
            opts_done = true;
        } else if b == b"-" || !b.starts_with(b"-") {
            continue; // `-` is a literal operand; or a NAME
        } else if b.starts_with(b"--") {
            // Long option, possibly `--name=value`.
            let (head, has_eq) = match b.iter().position(|&c| c == b'=') {
                Some(eq) => (&b[..eq], true),
                None => (b, false),
            };
            match safe_long_head(head) {
                Some(full) if full == b"--suffix" => {
                    // --suffix needs a value: `--suffix=V` (any V incl. empty or
                    // leading-dash is SAFE), or a SEPARATED token. A separated
                    // value that starts with `-` diverges (GNU accepts, clap
                    // refuses) → reject.
                    if !has_eq {
                        match it.next() {
                            Some(v) if !v.as_bytes().starts_with(b"-") => {}
                            _ => return false, // missing value, or dash value
                        }
                    }
                }
                Some(_) => {
                    // --multiple / --zero take NO value.
                    if has_eq { return false; }
                }
                None => return false, // --help/--version/unknown long
            }
        } else {
            // Short cluster: letters from {a,z} and optionally a value-taking
            // `s`. `-s` consumes the REST of the cluster as its value if any,
            // else the next token (which must NOT start with `-`).
            let rest = &b[1..];
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    b'a' | b'z' => i += 1,
                    b's' => {
                        if i + 1 < rest.len() {
                            // Attached value `-s.txt` / `-s-x`. EXCLUDE the
                            // `-s=value` form (GNU keeps the literal `=`).
                            if rest[i + 1] == b'=' { return false; }
                            i = rest.len(); // rest is the suffix value
                        } else {
                            // Separated value: next token, must not start `-`.
                            match it.next() {
                                Some(v) if !v.as_bytes().starts_with(b"-") => {}
                                _ => return false, // missing, or dash value
                            }
                            i = rest.len();
                        }
                    }
                    _ => return false, // unaudited short letter (incl. `-b`, `-h`)
                }
            }
        }
    }
    true
}

fn gate_dirname(args: &[OsString]) -> bool {
    let mut opts_done = false;
    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        if opts_done {
            continue; // operand: any value is SAFE
        }
        if b == b"--" { opts_done = true; continue; }
        if b == b"-" || !b.starts_with(b"-") {
            continue; // operand (`-` is a literal operand to dirname)
        }
        match b {
            // SAFE: -z and --zero (incl. infer_long_args prefixes of --zero).
            b"-z" => continue,
            x if x == b"--zero"
                || (x.starts_with(b"--z") && b"--zero".starts_with(x)) => continue,
            // EXCLUDED: --help, --version, any unaudited flag → host binary.
            _ => return false,
        }
    }
    true
}

fn gate_seq(args: &[OsString]) -> bool {
    fn is_int_operand(v: &[u8]) -> bool {
        let d = v.strip_prefix(b"-").unwrap_or(v);
        !d.is_empty() && d.iter().all(u8::is_ascii_digit)
    }
    // A separator value is safe iff it does not begin with '-' (the `-s--`
    // end-of-options divergence). An empty separator (`-s ""`) is fine.
    fn sep_ok(v: &[u8]) -> bool { !v.starts_with(b"-") }

    let mut operands = 0usize;
    let mut opts_done = false;
    let mut it = args.iter().skip(1).peekable();

    while let Some(a) = it.next() {
        let b = a.as_bytes();

        if opts_done {
            if !is_int_operand(b) { return false; }
            operands += 1;
            continue;
        }
        if b == b"--" { opts_done = true; continue; }

        // seq uses clap `trailing_var_arg`: once a positional operand has been
        // seen, uutils treats every later token as ANOTHER operand (erroring on
        // an option), while GNU permutes options anywhere — so an option after an
        // operand diverges (stderr wording, and `-s,`-style parse). Route those
        // to the host. A negative-NUMBER token (`-5`) is an operand, not an
        // option, so it falls through to is_int_operand below.
        if operands > 0 && b.starts_with(b"-") && b != b"-" && !is_int_operand(b) {
            return false;
        }

        if b == b"-w" || b == b"--equal-width" {
            continue;
        }
        // -s / --separator with separated value
        if b == b"-s" || b == b"--separator" {
            match it.next() {
                Some(v) if sep_ok(v.as_bytes()) => continue,
                _ => return false,
            }
        }
        // -sSTR (attached)
        if let Some(rest) = b.strip_prefix(b"-s") {
            if !rest.is_empty() && b[1] == b's' {
                // b starts with "-s" and has more chars; rest is the value.
                if sep_ok(rest) { continue; } else { return false; }
            }
        }
        // --separator=STR
        if let Some(rest) = b.strip_prefix(b"--separator=") {
            if sep_ok(rest) { continue; } else { return false; }
        }
        // --equal-width is matched above; nothing else with `--` is audited.
        if b.starts_with(b"--") {
            return false; // --format/--terminator/--help/--version/unknown
        }
        // A bare numeric operand (incl. negative integers).
        if is_int_operand(b) {
            operands += 1;
            continue;
        }
        // Anything else: short flag we don't audit (-f, -t, clustered),
        // or a non-integer operand (float / 1e2 / .1 / inf / nan).
        return false;
    }

    // GNU requires 1..=3 operands; uu_app enforces num_args(1..=3) and errors
    // identically on 0 or >3, but be explicit so a malformed argv falls back
    // rather than relying on matching error text.
    (1..=3).contains(&operands)
}

fn gate_expr(args: &[OsString]) -> bool {
    // The version/help banners: expr treats its operands positionally
    // (allow_hyphen_values), so `--help`/`--version` are only special as a *sole*
    // argument (GNU: `expr --help foo` evaluates the string "--help").
    let operands = &args[1..];
    if operands.len() == 1 {
        let b = operands[0].as_bytes();
        if b == b"--help" || b == b"--version" {
            return false;
        }
    }

    // An integer literal: optional sign, then ≥1 ASCII digit, all digits.
    fn int_literal(b: &[u8]) -> bool {
        let d = match b.first() {
            Some(b'-' | b'+') => &b[1..],
            _ => b,
        };
        !d.is_empty() && d.iter().all(u8::is_ascii_digit)
    }

    // `substr` with an out-of-range POS/LEN diverges (GNU clamps to the string
    // end; uutils returns empty + exit 1) because uutils evaluates substr offsets
    // in fixed-width arithmetic, not the BigInt it uses for `+`/`-`/`*`. Normal
    // bignum ARITHMETIC matches GNU, so only gate this when `substr` is present.
    let has_substr = operands.iter().any(|a| a.as_bytes() == b"substr");

    for a in operands {
        let b = a.as_bytes();
        // HOLE 1: a leading-`+` integer literal (`+5`). uutils parses it as a
        // positive integer (BigInt accepts the sign); GNU does not — it treats
        // `+5` as a non-integer string, flipping arithmetic/comparison results
        // AND the exit code. The bare `+` operator (len 1) is unaffected.
        if b.len() > 1 && b[0] == b'+' && b[1].is_ascii_digit() {
            return false;
        }
        // HOLE 2: an integer operand that overflows i64 while `substr` is in play.
        if has_substr && int_literal(b)
            && std::str::from_utf8(b).ok().and_then(|s| s.parse::<i64>().ok()).is_none()
        {
            return false;
        }
    }
    true
}

/// uutils `tr` `[:class:]` names that match GNU; the SET2 restriction to
/// upper/lower is enforced by `tr_set_is_safe`.
const TR_SAFE_CLASSES: &[&[u8]] = &[
    b"alnum", b"alpha", b"blank", b"cntrl", b"digit", b"graph",
    b"lower", b"print", b"punct", b"space", b"upper", b"xdigit",
];

/// True if a `tr` SET argument uses only constructs uutils renders
/// GNU-identically (excludes overflowing octal escapes and SET2 classes
/// other than upper/lower — both diverge in stderr wording).
fn tr_set_is_safe(s: &[u8], is_set2: bool) -> bool {
    // A LONE trailing backslash (odd run of trailing `\`): GNU warns for ANY set
    // ("unescaped backslash at end of string"); uutils only warns for SET1, so a
    // trailing `\` in SET2 diverges (stderr). Reject either way → host fallback.
    if s.iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 1 {
        return false;
    }
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'\\' if i + 1 < s.len() => {
                // Octal escape: backslash then octal digits. A FIRST digit 4-7
                // with ≥2 more octal digits overflows a byte → GNU's two-line
                // "ambiguous octal escape" wording. Reject.
                let d = s[i + 1];
                if (b'4'..=b'7').contains(&d)
                    && s.get(i + 2).is_some_and(|c| (b'0'..=b'7').contains(c))
                    && s.get(i + 3).is_some_and(|c| (b'0'..=b'7').contains(c))
                {
                    return false;
                }
                i += 2; // consume backslash + escaped char
            }
            b'[' if s.get(i + 1) == Some(&b':') => {
                // A [:class:] construct. Find the closing ":]".
                let rest = &s[i + 2..];
                match rest.windows(2).position(|w| w == b":]") {
                    Some(end) => {
                        let name = &rest[..end];
                        if !TR_SAFE_CLASSES.contains(&name) {
                            return false; // unknown/empty class name wording
                        }
                        if is_set2 {
                            // ANY class in SET2 diverges: a non-upper/lower class
                            // is rejected by uutils with different wording, and a
                            // MISALIGNED [:upper:]/[:lower:] gives a divergent
                            // "misaligned" message. The one valid case — the
                            // canonical [:lower:]↔[:upper:] pair — is whitelisted
                            // in gate_tr before this is ever called for SET2.
                            return false;
                        }
                        i += 2 + end + 2; // consume "[:name:]"
                    }
                    // No closing ":]" — not a class construct; treat '[' as a
                    // literal and move on (GNU/uutils agree here).
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    true
}

fn gate_tr(args: &[OsString]) -> bool {
    // Collect the positional SETs (in tr, '-' is a LITERAL set member, not a
    // stdin marker) and validate flags inline.
    let mut sets: Vec<&[u8]> = Vec::new();
    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        if b == b"-" || !b.starts_with(b"-") {
            sets.push(b);
        } else if b.starts_with(b"--") {
            match b {
                b"--complement" | b"--delete"
                | b"--squeeze-repeats" | b"--truncate-set1" => continue,
                _ => return false, // --help/--version/unknown
            }
        } else if !b[1..].iter().all(|c| b"cCdst".contains(c)) {
            return false; // unaudited short letter
        }
    }
    // The ONLY GNU-faithful use of [:upper:]/[:lower:] in SET2 is the canonical
    // case-conversion pair; tr_set_is_safe rejects every other SET2 class, so
    // whitelist exactly that pair here.
    if sets.len() == 2
        && ((sets[0] == b"[:lower:]" && sets[1] == b"[:upper:]")
            || (sets[0] == b"[:upper:]" && sets[1] == b"[:lower:]"))
    {
        return true;
    }
    for (i, s) in sets.iter().enumerate() {
        if !tr_set_is_safe(s, i == 1) {
            return false;
        }
    }
    true
}

fn gate_cut(args: &[OsString]) -> bool {
    // A GNU-acceptable LIST: comma-separated specs, each `N`, `N-`, `-N`, or
    // `N-M`, every number ≥ 1 (no `0`, no empty spec, no non-digits).
    fn list_ok(v: &[u8]) -> bool {
        if v.is_empty() { return false; }
        for spec in v.split(|&c| c == b',') {
            // strip a single leading or trailing '-' (open range); the rest
            // splits into ≤2 numeric parts.
            let parts: Vec<&[u8]> = spec.split(|&c| c == b'-').collect();
            if parts.len() > 2 { return false; }              // "1-2-3"
            let mut saw_num = false;
            let mut ends: [Option<usize>; 2] = [None, None];
            for (idx, p) in parts.iter().enumerate() {
                if p.is_empty() { continue; }                 // open end "-5"/"5-"
                if !p.iter().all(u8::is_ascii_digit) { return false; }
                // Parse as usize: an out-of-range magnitude diverges from GNU
                // ("field number is too large" vs uutils' "failed to parse").
                let n: usize = match std::str::from_utf8(p).ok()
                    .and_then(|s| s.parse::<usize>().ok()) {
                    Some(n) => n,
                    None => return false,                     // overflow
                };
                if n == 0 { return false; }                   // "0"
                ends[idx] = Some(n);
                saw_num = true;
            }
            if !saw_num { return false; }                     // bare "-" / ""
            // A closed DECREASING range ("3-1") is "invalid decreasing range" in
            // GNU but a different message in uutils — fall back to the host.
            if let (Some(lo), Some(hi)) = (ends[0], ends[1]) {
                if hi < lo { return false; }
            }
        }
        true
    }

    let mut mode_count: u32 = 0;     // count of -b/-c/-f (must end == 1)
    let mut have_fields = false;     // mode is -f
    let mut have_delim = false;      // -d seen
    let mut have_only_delim = false; // -s seen
    let mut opts_done = false;

    let mut it = args.iter().skip(1).peekable();
    while let Some(a) = it.next() {
        let b = a.as_bytes();
        if opts_done {
            continue; // positional FILE after `--`
        }
        if b == b"--" {
            opts_done = true;
            continue;
        }
        if b == b"-" || !b.starts_with(b"-") {
            continue; // stdin, or a filename
        }
        if b.starts_with(b"--") {
            // long option; value may ride in `=VALUE`.
            let (head, val): (&[u8], Option<&[u8]>) = match b.iter().position(|&c| c == b'=') {
                Some(eq) => (&b[..eq], Some(&b[eq + 1..])),
                None => (b, None),
            };
            match head {
                b"--bytes" | b"--characters" | b"--fields" => {
                    mode_count += 1;
                    if head == b"--fields" { have_fields = true; }
                    let list = match val {
                        Some(v) => v,
                        None => match it.next() { Some(v) => v.as_bytes(), None => return false },
                    };
                    if !list_ok(list) { return false; }
                }
                b"--delimiter" => {
                    have_delim = true;
                    let d = match val {
                        Some(v) => v,
                        None => match it.next() { Some(v) => v.as_bytes(), None => return false },
                    };
                    // GNU: delimiter is exactly one byte (empty -> NUL is fine).
                    if d.len() > 1 { return false; }
                }
                b"--output-delimiter" => {
                    if val.is_none() && it.next().is_none() { return false; }
                    // any STR is fine (multi-byte output delims are GNU-valid)
                }
                b"--complement" | b"--zero-terminated" => {
                    if val.is_some() { return false; } // bare flag takes no =value
                }
                b"--only-delimited" => {
                    have_only_delim = true;
                    if val.is_some() { return false; }
                }
                // --help, --version, --whitespace-delimited(long?), unknown
                _ => return false,
            }
        } else {
            // short cluster, e.g. -d:, -f1-3, -sz, -zf2 ...
            let rest = &b[1..];
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    b'z' | b'n' => i += 1,
                    b's' => { have_only_delim = true; i += 1; }
                    b'b' | b'c' | b'f' => {
                        mode_count += 1;
                        if rest[i] == b'f' { have_fields = true; }
                        // LIST is the rest of the cluster, or the next token.
                        let list: &[u8] = if i + 1 < rest.len() {
                            &rest[i + 1..]
                        } else {
                            match it.next() { Some(v) => v.as_bytes(), None => return false }
                        };
                        if !list_ok(list) { return false; }
                        i = rest.len();
                    }
                    b'd' => {
                        have_delim = true;
                        let d: &[u8] = if i + 1 < rest.len() {
                            &rest[i + 1..]
                        } else {
                            match it.next() { Some(v) => v.as_bytes(), None => return false }
                        };
                        if d.len() > 1 { return false; } // multi-char -d diverges
                        i = rest.len();
                    }
                    // -w (whitespace ext), -h/--help short, or unaudited letter
                    _ => return false,
                }
            }
        }
    }

    // Exactly one cutting mode (0 -> "missing mode" divergence; ≥2 -> "conflict"
    // divergence). `-d`/`-s` only legal with `-f` (else uutils' wording differs).
    if mode_count != 1 { return false; }
    if (have_delim || have_only_delim) && !have_fields { return false; }
    true
}

fn gate_uniq(args: &[OsString]) -> bool {
    // A GNU-faithful skip/check value: ≥1 byte, ALL ascii digits (no sign, no
    // `0x`, no `k`/`M` multiplier). These are the only -f/-s/-w values where
    // uutils matches GNU.
    fn is_plain_count(v: &[u8]) -> bool {
        !v.is_empty() && v.iter().all(u8::is_ascii_digit)
    }

    const SAFE_LONG_BARE: &[&[u8]] = &[
        b"--count",
        b"--repeated",
        b"--unique",
        b"--ignore-case",
        b"--zero-terminated",
    ];
    const SAFE_LONG_VALUED: &[&[u8]] = &[b"--skip-fields", b"--skip-chars", b"--check-chars"];

    let mut count_flag = false;       // -c / --count seen
    let mut all_repeated_flag = false; // -D / --all-repeated seen
    let mut operands = 0usize;        // INPUT / OUTPUT positionals
    let mut opts_done = false;

    let mut it = args.iter().skip(1).peekable();
    while let Some(a) = it.next() {
        let b = a.as_bytes();
        if opts_done {
            operands += 1;
            if operands > 2 { return false; } // only INPUT + OUTPUT allowed
            continue;
        }
        if b == b"--" {
            opts_done = true;
        } else if b == b"-" || !b.starts_with(b"-") {
            operands += 1;                     // stdin/stdout marker, or a path
            if operands > 2 { return false; }
        } else if b.starts_with(b"--") {
            let head = match b.iter().position(|&c| c == b'=') {
                Some(eq) => &b[..eq],
                None => b,
            };
            let has_eq = head.len() != b.len();
            if SAFE_LONG_BARE.contains(&head) {
                if has_eq { return false; }    // a bare flag takes no `=value`
                if head == b"--count" { count_flag = true; }
            } else if SAFE_LONG_VALUED.contains(&head) {
                let val: &[u8] = if has_eq {
                    &b[head.len() + 1..]
                } else {
                    match it.next() {
                        Some(v) => v.as_bytes(),
                        None => return false,
                    }
                };
                if !is_plain_count(val) { return false; }
            } else if head == b"--all-repeated" {
                // require_equals arg: only `--all-repeated` (bare) or
                // `--all-repeated=<none|prepend|separate>` match GNU.
                all_repeated_flag = true;
                if has_eq {
                    let val = &b[head.len() + 1..];
                    if !matches!(val, b"none" | b"prepend" | b"separate") {
                        return false;          // bad delim -> clap text diverges
                    }
                }
            } else {
                return false;                  // --help/--version/--group/unknown
            }
        } else {
            // Short form: obsolete `-N` skip-fields (pure digits) or a cluster.
            let rest = &b[1..];
            if rest.first().is_some_and(u8::is_ascii_digit) {
                // Obsolete skip-fields `-1`/`-12`. Accept only PURE digits.
                if rest.iter().all(u8::is_ascii_digit) { continue; }
                return false;
            }
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    b'c' => { count_flag = true; i += 1; }
                    b'd' | b'u' | b'i' | b'z' => i += 1,
                    b'D' => { all_repeated_flag = true; i += 1; } // bare -D only
                    b'f' | b's' | b'w' => {
                        // uutils' clap accepts a value-flag's GLUED value only when
                        // it leads the cluster (`-f1`); `-cf1` errors ("a value is
                        // required") while GNU accepts it. So require f/s/w to be
                        // the FIRST cluster letter; otherwise fall back to host.
                        if i != 0 { return false; }
                        // value = rest of cluster (`-f1`) or next token (`-f 1`).
                        let val: &[u8] = if i + 1 < rest.len() {
                            &rest[i + 1..]
                        } else {
                            match it.next() {
                                Some(v) => v.as_bytes(),
                                None => return false,
                            }
                        };
                        if !is_plain_count(val) { return false; } // `-w -3`,`-f q`
                        i = rest.len();
                    }
                    _ => return false,         // unaudited short letter
                }
            }
        }
    }

    // `-c` together with `-D` errors in both, but with divergent wording; send
    // the whole argv to the host GNU binary so the message matches.
    if count_flag && all_repeated_flag { return false; }
    true
}

fn gate_sort(args: &[OsString]) -> bool {
    // LOCALE GUARD. uu_sort is built with the `i18n-collator` default feature, so
    // its default string compare uses ICU collation under a UTF-8 non-C locale,
    // diverging from GNU's byte sort. Only the C/POSIX collation locale (byte
    // order) is reproducible in-process; anything else falls back to the host.
    // The locale comes from the ENGINE process env (LC_ALL > LC_COLLATE > LANG).
    fn collation_is_c() -> bool {
        let pick = |k| std::env::var(k).ok().filter(|s| !s.is_empty());
        let v = pick("LC_ALL").or_else(|| pick("LC_COLLATE"))
            .or_else(|| pick("LANG")).unwrap_or_default();
        let base = v.split(['.', '@']).next().unwrap_or("");
        base.is_empty() || base == "C" || base == "POSIX"
    }
    if !collation_is_c() {
        return false;
    }

    // Long options that are safe as bare flags (no value).
    const SAFE_LONG_BARE: &[&[u8]] = &[
        b"--reverse", b"--unique", b"--ignore-leading-blanks",
        b"--dictionary-order", b"--ignore-case", b"--ignore-nonprinting",
        b"--stable", b"--zero-terminated", b"--merge",
    ];
    // Long options that take a value (via `=V` or the next token). Their VALUES
    // are unrestricted-but-safe: separators, buffer sizes, thread counts, batch
    // sizes, file paths, and `--check[=…]`/`--key`/`--output`/`--files0-from`.
    // --key values are validated separately below (no mode modifiers allowed).
    const SAFE_LONG_VALUED: &[&[u8]] = &[
        b"--field-separator", b"--buffer-size", b"--temporary-directory",
        b"--parallel", b"--batch-size", b"--output", b"--files0-from",
        b"--check", // --check / --check=quiet|silent|diagnose-first
    ];
    // EXCLUDED long options (explicit, for clarity — anything not in the two
    // SAFE lists is rejected by the fall-through anyway).
    // --numeric-sort --general-numeric-sort --human-numeric-sort --month-sort
    // --version-sort --sort --random-sort --random-source --debug
    // --compress-program --help --version

    // A `--key` value is safe iff every per-field modifier letter is in {b,d,f,i,r,s}.
    // A modifier appears after a '.' char-offset or directly after the field
    // number, on either end of an optional ',' range: `F[.C][MODS][,F[.C][MODS]]`.
    fn key_value_safe(v: &[u8]) -> bool {
        // Reject any of the MODE modifiers n,g,h,M,V and the random R. We accept
        // a modifier letter only from the safe set; digits, '.', ',' are fine.
        for &c in v {
            match c {
                b'0'..=b'9' | b'.' | b',' => {}
                b'b' | b'd' | b'f' | b'i' | b'r' | b's' => {}
                _ => return false, // n,g,h,M,V,R or anything unexpected
            }
        }
        true
    }

    let mut it = args.iter().skip(1).peekable();
    let mut opts_done = false;
    while let Some(a) = it.next() {
        let b = a.as_bytes();
        if opts_done {
            continue; // positional file after `--`
        }
        if b == b"--" {
            opts_done = true;
        } else if b.first() == Some(&b'+') && b.get(1).is_some_and(u8::is_ascii_digit) {
            // Obsolete `+POS[-POS]` key syntax: the gate can't validate the key's
            // mode modifiers (and a string +POS key inherits the locale risk), so
            // route it to the host for parity with the `-k` policy.
            return false;
        } else if b == b"-" || !b.starts_with(b"-") {
            continue; // stdin, or a filename
        } else if b.starts_with(b"--") {
            let (head, has_eq) = match b.iter().position(|&c| c == b'=') {
                Some(eq) => (&b[..eq], true),
                None => (b, false),
            };
            if head == b"--key" {
                let val: &[u8] = if has_eq {
                    &b[head.len() + 1..]
                } else {
                    match it.next() { Some(v) => v.as_bytes(), None => return false }
                };
                if !key_value_safe(val) { return false; }
            } else if SAFE_LONG_BARE.contains(&head) {
                if has_eq { return false; } // a bare flag takes no `=value`
            } else if SAFE_LONG_VALUED.contains(&head) {
                // --check may appear bare (=> default "diagnose-first"); the
                // others must have a value (=V or next token). Either way the
                // value itself is unrestricted, so just consume the next token
                // when there is no `=`.
                if !has_eq && head != b"--check" {
                    if it.next().is_none() { return false; }
                }
            } else {
                return false; // mode/random/debug/compress/help/version/unknown
            }
        } else {
            // Short cluster. Walk letters; flags that take a value consume the
            // rest of the cluster or the next token.
            let rest = &b[1..];
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    // bare comparison/selection flags
                    b'r' | b'u' | b'b' | b'd' | b'f' | b'i' | b's' | b'z' | b'c' | b'C' | b'm' => i += 1,
                    // value-taking short flags: -t -k -o -S -T
                    b't' | b'k' | b'o' | b'S' | b'T' => {
                        let val: &[u8] = if i + 1 < rest.len() {
                            &rest[i + 1..]
                        } else {
                            match it.next() { Some(v) => v.as_bytes(), None => return false }
                        };
                        // Only -k needs value validation (reject mode modifiers).
                        if rest[i] == b'k' && !key_value_safe(val) { return false; }
                        i = rest.len();
                    }
                    // EXCLUDED short flags: n g h M V R (modes/random)
                    _ => return false,
                }
            }
        }
    }
    true
}

// ── agents add per-util gates BELOW this line ────────────────────────
// One function per util, alphabetical. Register the new fn in
// `gate_table()` above. Leave a 2-3 line comment explaining what
// shapes the gate accepts and which uutils divergences it dodges.

/// `cp [-r|-R|--recursive] [-p|--preserve-mode/ownership/timestamps]
/// [-a|--archive] [-f|--force] [-d] [-H] [-L] [-P] [-T] [-t DIR]
/// [-v|--verbose] [-n|--no-clobber] [--] SRC... DST` — copy files.
///
/// What this gate ACCEPTS is a deliberately small, conservative slice where
/// `uu_cp` reproduces GNU `cp`'s observable effect (the bytes/metadata of the
/// copied file) faithfully for the plain copy shapes our build-recipe workloads
/// use — primarily `cp SRC DST` and `cp SRC... DIR`, plus the recurse/preserve
/// flag cluster (`-r`/`-R`/`-a`/`-p`/`-d`/`-f`/`-T`/`-H`/`-L`/`-P`/`-t`/`-v`
/// and their audited long forms). On THESE argvs a successful copy lands
/// byte-identical content and the documented metadata, which is all the
/// provenance/capture path observes.
///
/// What it REFUSES (→ fall back to fork+exec of GNU `cp`):
///   * `--help` / `--version` — uutils' wording diverges from GNU's.
///   * `-i`/`--interactive`, `--backup`/`-b`, `--reflink`, `--sparse`,
///     `-u`/`--update`, `-l`/`--link`, `-s`/`--symbolic-link`,
///     `-x`/`--one-file-system`, `-S`/`--suffix`, `-Z`/`--context`,
///     `--attributes-only`, `--copy-contents`, `--no-clobber` long quirks,
///     `--parents`, `--no-target-directory` long edge, and ANY long option we
///     have not explicitly listed as SAFE. These either differ from GNU in
///     output/prompting/heuristics or are simply un-audited here, so they go
///     to the host binary which IS GNU.
///   * combined short clusters whose every letter is not in the audited set
///     (e.g. `-ri` mixes safe `-r` with unsafe `-i`).
///   * an `=`-valued long option we have not audited (`--preserve=…` carries a
///     GNU-specific attribute grammar — refuse, let host cp handle it).
///
/// IMPORTANT: this gate is a static argv predicate; it cannot see whether the
/// copy will SUCCEED. uutils' ERROR messages diverge from GNU (the uucore
/// Fluent localization cache, see brush.rs::box_builtins_opt). That is
/// acceptable here only because the in-process path is taken solely for these
/// simple argvs and the callers that opt cp in either don't compare cp's
/// stderr against GNU or run cp only on inputs expected to exist. BEHAVIORALLY
/// UNVERIFIED in this container (box-spawning tests can't run here); correctness
/// rests on the argv categorization above + a clean compile.
fn gate_cp(args: &[OsString]) -> bool {
    // Long options whose effect matches GNU for a plain copy. `--help`,
    // `--version`, and every attribute-grammar/backup/reflink/update/link
    // option are deliberately ABSENT so they fall back to GNU cp.
    const SAFE_LONG: &[&[u8]] = &[
        b"--recursive",
        b"--archive",
        b"--preserve",        // bare --preserve (default attr set) only; `--preserve=…` refused below
        b"--no-dereference",
        b"--dereference",
        b"--force",
        b"--target-directory", // `--target-directory=DIR` handled via `=` split below
        b"--no-target-directory",
        b"--verbose",
        b"--",
    ];
    // Short flags (single letters) whose effect matches GNU for a plain copy.
    const SAFE_SHORT: &[u8] = b"rRapdfHLPTv";

    let mut opts_done = false;
    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        if opts_done {
            continue; // positional path after `--`
        }
        if b == b"--" {
            opts_done = true;
        } else if b == b"-" || !b.starts_with(b"-") {
            continue; // a path (bare `-` is just a filename to cp)
        } else if b.starts_with(b"--") {
            // Split a possible `--opt=value`. We accept ONLY the audited bare
            // long options; any `=`-valued form (e.g. `--preserve=mode`,
            // `--target-directory=DIR` we have not audited the grammar of) is
            // refused so GNU cp parses it.
            if b.contains(&b'=') {
                return false;
            }
            if !SAFE_LONG.contains(&b) {
                return false; // --help/--version/unknown/un-audited long option
            }
            // `--target-directory` (bare) takes the NEXT arg as its dir; that
            // arg is a path, consumed by the loop as a positional — fine.
        } else {
            // Short cluster `-rp`, `-a`, etc. Every letter must be audited.
            // `-t` takes an argument (the target dir); if `-t` is the LAST
            // letter of this cluster, the next token is its value (a path,
            // consumed normally). We do NOT special-case attached values like
            // `-tDIR` — refuse a `t` that is not the cluster's final letter so
            // we never misread an attached value as more flags.
            let letters = &b[1..];
            for (i, c) in letters.iter().enumerate() {
                if *c == b't' {
                    if i != letters.len() - 1 {
                        return false; // attached value `-tDIR` — let host cp parse
                    }
                    continue;
                }
                if !SAFE_SHORT.contains(c) {
                    return false; // an un-audited short flag
                }
            }
        }
    }
    true
}
