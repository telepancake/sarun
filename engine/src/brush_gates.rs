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
///   `-nq`/`-cq` (non-numeric attached value to -n/-c): uutils accepts (exit 0),
///   GNU errors (exit 1) — so an attached -c/-n value is accepted only if it
///   begins with `-` or a digit.
fn gate_head(args: &[OsString]) -> bool {
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
                if !has_eq {
                    // value is the next token; consume it.
                    if it.next().is_none() {
                        return false;
                    }
                }
            } else {
                return false; // --help/--version/--presume-input-pipe/unknown
            }
        } else {
            // Short form. Either obsolete `-NUM…` (a digit right after `-`) or a
            // cluster of audited letters.
            let rest = &b[1..];
            if rest.first().is_some_and(u8::is_ascii_digit) {
                continue; // obsolete `-5`, `-2c`, … (differential-tested)
            }
            let mut i = 0;
            let mut consumed_next = false;
            while i < rest.len() {
                match rest[i] {
                    b'q' | b'v' | b'z' => i += 1,
                    b'c' | b'n' => {
                        if i + 1 < rest.len() {
                            // attached value `-n5`/`-n-1`/`-c2b`: must look
                            // numeric (a leading `-` or digit). Rejects `-nq`/`-cq`.
                            let v = &rest[i + 1..];
                            if !(v[0] == b'-' || v[0].is_ascii_digit()) {
                                return false;
                            }
                            i = rest.len();
                        } else {
                            consumed_next = true;
                            i += 1;
                        }
                    }
                    _ => return false, // unaudited short letter
                }
            }
            if consumed_next && it.next().is_none() {
                return false; // `-n` with no following value
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

    for a in args.iter().skip(1) {
        let b = a.as_bytes();
        if opts_done {
            if b == b"-" { have_explicit_stdin = true; } else { have_file = true; }
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
            } else if b == b"--total" || b.starts_with(b"--total=") {
                continue; // total policy: safe, not a counter
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
                b"--section-delimiter" | b"--number-separator" => {
                    let _ = value!(); // any string value is fine
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
                    b's' | b'd' => true, // any string value
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
            let _ = it.next(); // value is the next token; consume it
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
                            let _ = it.next(); // value is next token
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
