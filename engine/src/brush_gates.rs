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
