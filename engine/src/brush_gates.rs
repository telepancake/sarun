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

// ── agents add per-util gates BELOW this line ────────────────────────
// One function per util, alphabetical. Register the new fn in
// `gate_table()` above. Leave a 2-3 line comment explaining what
// shapes the gate accepts and which uutils divergences it dodges.
