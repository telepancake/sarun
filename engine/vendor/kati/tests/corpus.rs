// sarun: GNU-make compat baseline runner for the vendored upstream kati
// testcase corpus.
//
// For each `.mk` in `testcase/`:
//   1. Drop it into a tmpdir as `Makefile`.
//   2. Run system `make` once, capture combined stdout+stderr.
//   3. Run our `rkati` once, capture combined stdout+stderr.
//   4. Apply the same normalizations upstream's `run_test.go` does (path
//      stripping, GNU-make-version-skew rewrites, kati log line removal).
//   5. Compare. Identical = PASS.
//
// The corpus headers `# TODO(rust)`, `# TODO(all)`, etc. mark known-failure
// cases — we count them separately as XFAIL and don't fail the suite for them.
//
// Result: a single line at the end, `KATI_COMPAT_PASS=N/total` (plus an
// xfail count), printed to stdout. This is the hill-climb metric: every
// commit that fixes a kati semantics divergence should bump N.
//
// The runner is feature-gated (`kati-corpus`) because it needs the `rkati`
// binary built (also gated on the same feature) and runs system `make` —
// neither of which should happen during a normal engine build.

#![cfg(feature = "kati-corpus")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testcase")
}

fn rkati_path() -> PathBuf {
    let exe = env!("CARGO_BIN_EXE_rkati");
    PathBuf::from(exe)
}

// Match upstream run_test.go's regexes, applied in the same order.
struct Norm {
    re: regex_lite::Regex,
    replace: &'static str,
}

fn norm(pat: &str, replace: &'static str) -> Norm {
    Norm {
        re: regex_lite::Regex::new(pat).expect("normalization regex"),
        replace,
    }
}

fn normalize_quotes() -> Norm {
    // ` ' " plus U+2018 U+2019 (utf-8 e2 80 98 / e2 80 99)
    norm(r#"([`'"\x{2018}\x{2019}])"#, "\"")
}

fn make_norms() -> Vec<Norm> {
    vec![
        normalize_quotes(),
        norm(r"make(?:\[\d+\])?: (Entering|Leaving) directory[^\n]*\n", ""),
        norm(r"make(?:\[\d+\])?: ", ""),
        // sarun: real make prints `"X" is up to date.` after a
        // skip-recipe run; rkati doesn't, and we shouldn't pollute every
        // incremental build with that. Strip the whole line.
        norm(r#""[^"\n]+" is up to date\.\n"#, ""),
        norm(" recipe for target ", " commands for target "),
        norm(" recipe commences ", " commands commence "),
        norm(r"missing rule before recipe\.", "missing rule before commands."),
        norm(r" \(did you mean TAB instead of 8 spaces\?\)", ""),
        norm("Extraneous text after", "extraneous text after"),
        norm(r"\s+Stop\.", ""),
        norm(r#"Makefile:\d+: commands for target ".*?" failed\n"#, ""),
        norm(r"/bin/(ba)?sh: line 1: ", ""),
        // sarun: dash prefixes errors as `/bin/sh: N: cmd: ...` rather
        // than `/bin/sh: line N: cmd: ...`. Strip both forms so a
        // recipe's shell-error landing on either gets canonicalized.
        norm(r"/bin/(ba)?sh: \d+: ", ""),
        norm(
            r#"(: \S+: No such file or directory)\n\*\*\* No rule to make target "[^"]+"\."#,
            "$1",
        ),
        norm(r"\[\S+:\d+: ", "["),
        // The non-ninja branch of upstream adds normalizeMakeNinja too; we
        // always strip ninja warnings since we're never running ninja here.
        norm("ninja: warning: [^\n]+", ""),
    ]
}

fn kati_norms() -> Vec<Norm> {
    vec![
        normalize_quotes(),
        // sarun: now that rkati's $(MAKE) is just "make" (via arg0), any
        // $(MAKE) -f sub.mk recipe actually invokes the system make,
        // whose own Entering/Leaving and "make: " prefixes leak into
        // rkati's output. Apply the same strips the make-side does.
        norm(r"make(?:\[\d+\])?: (Entering|Leaving) directory[^\n]*\n", ""),
        norm(r"make(?:\[\d+\])?: ", ""),
        norm(r"\*kati\*[^\n]*", ""),
        norm(r"c?kati: ", ""),
        norm(r"/bin/(ba)?sh: line 1: ", ""),
        // sarun: dash form `/bin/sh: N: cmd: ...` (recipe shell-error
        // prefix when SHELL=/bin/sh-via-dash); strip the leading-digit
        // form too so make's and rkati's recipe errors normalize the
        // same way.
        norm(r"/bin/(ba)?sh: \d+: ", ""),
        norm(r"/bin/sh: ", ""),
        norm(r".*: warning for parse error in an unevaluated line: [^\n]*", ""),
        norm(r"([^\n ]+: )?FindEmulator: ", ""),
        norm(r" (\./+)+kati\.\S+", ""),
        norm(r" (\./+)+test\S+.json", ""),
        norm(
            r"(: )open (\S+): n(o such file or directory)\nNOTE:[^\n]*",
            "${1}${2}: N${3}",
        ),
        norm(r"Too many symbolic links encountered", "Too many levels of symbolic links"),
        norm(r" \(os error \d+\)", ""),
    ]
}

fn circular_re() -> regex_lite::Regex {
    regex_lite::Regex::new(r"(Circular .* dropped\.\n)").unwrap()
}

fn normalize(input: &str, norms: &[Norm]) -> String {
    // Upstream's normalize() splits out circular-dep messages, strips them
    // from the body, and prepends them — order-independent comparison for
    // those one liners. Match that.
    let circ = circular_re();
    let mut prefix = String::new();
    for cap in circ.captures_iter(input) {
        prefix.push_str(&cap[1]);
    }
    let body = circ.replace_all(input, "").into_owned();
    let mut out = prefix;
    out.push_str(&body);
    for n in norms {
        out = n.re.replace_all(&out, n.replace).into_owned();
    }
    out
}

// Returns Some(reason) if the testcase header marks it as TODO/expected-fail
// for the configuration we're running ("rust", default goal, non-ninja,
// non-gen-all). Markers with a "/testN" suffix apply only when that specific
// sub-test target is invoked — since we only run the default goal, those
// don't apply to us.
fn xfail_reason(src: &str) -> Option<String> {
    let todo_re = regex_lite::Regex::new(r"^# TODO(?:\(([-a-z|]+)(?:/([-a-z0-9|]+))?\))?")
        .unwrap();
    for line in src.lines() {
        if !line.starts_with("#!") && !line.starts_with("# TODO") {
            return None;
        }
        let Some(cap) = todo_re.captures(line) else {
            continue;
        };
        let subtest = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        if !subtest.is_empty() {
            // Sub-test-scoped TODO; we don't invoke sub-tests.
            continue;
        }
        let tags = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if tags.is_empty() {
            return Some(line.to_string());
        }
        let tags: HashSet<&str> = tags.split('|').collect();
        // We run "rust" (rkati), no --ninja, no --gen_all_targets.
        if tags.contains("rust") || tags.contains("all") {
            return Some(line.to_string());
        }
    }
    None
}

fn run_with_timeout(mut cmd: Command, dir: &Path) -> (Vec<u8>, Option<i32>) {
    // Merge stderr into stdout via a single OS pipe so the runner reads one
    // naturally-interleaved stream (mirrors what `2>&1` gives at the shell
    // and what Go's `cmd.CombinedOutput()` does upstream). Otherwise reading
    // stdout-then-stderr would compare apples to oranges any time make
    // routed info()/warn() and recipe-echo lines differently from rkati.
    let (read_pipe, write_pipe) = match os_pipe::pipe() {
        Ok(p) => p,
        Err(e) => return (format!("pipe: {e}").into_bytes(), None),
    };
    let stdout_w = match write_pipe.try_clone() {
        Ok(w) => w,
        Err(e) => return (format!("pipe clone: {e}").into_bytes(), None),
    };
    cmd.current_dir(dir)
        .stdout(stdout_w)
        .stderr(write_pipe)
        .env_remove("MAKEFLAGS")
        .env_remove("MAKELEVEL")
        .env("LC_ALL", "C");
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (format!("spawn failed: {e}").into_bytes(), None),
    };
    // Drop our copies of the write side so the read end sees EOF once the
    // child exits.
    drop(cmd);
    let mut read_pipe = read_pipe;

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::io::Read;
                let mut out = Vec::new();
                let _ = read_pipe.read_to_end(&mut out);
                return (out, status.code());
            }
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (b"<TIMEOUT>".to_vec(), None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return (format!("wait failed: {e}").into_bytes(), None),
        }
    }
}

#[derive(Default)]
struct Tally {
    pass: usize,
    fail: usize,
    xfail_unexpected_pass: usize,
    xfail: usize,
    skipped: usize,
    xpass_names: Vec<String>,
    xfail_names: Vec<String>,
}

#[test]
fn corpus_pass_rate() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        panic!("testcase corpus missing at {}", dir.display());
    }
    let rkati = rkati_path();
    if !rkati.exists() {
        panic!("rkati binary not at expected location: {}", rkati.display());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "mk")
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut tally = Tally::default();
    let mut failures = Vec::new();
    let only = std::env::var("KATI_CORPUS_ONLY").ok();
    // sarun: parallel-conformance mode. When set, each case is ALSO run with
    // rkati -j4 and its output compared (order-independent) to the rkati -j1 run.
    // make's contract is that -jN produces the same result as -j1, so this
    // validates the parallel scheduler against the same conformance the corpus
    // already pins for serial. Recipe output interleaves under -j, so the compare
    // is on sorted lines, not raw order.
    let parallel = std::env::var("KATI_CORPUS_PARALLEL").is_ok();
    let mut par_pass = 0usize;
    let mut par_fail = 0usize;
    let mut par_mismatch: Vec<String> = Vec::new();

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(filter) = &only {
            if !name.contains(filter.as_str()) {
                tally.skipped += 1;
                continue;
            }
        }

        let src = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(_) => {
                tally.skipped += 1;
                continue;
            }
        };
        let xfail = xfail_reason(&src);

        // Use a single tmpdir for both runs so absolute-path output (e.g.
        // $(abspath), $(realpath), $(CURDIR)) matches between make and rkati.
        let workdir = tempfile::tempdir().expect("tempdir");
        std::fs::copy(entry.path(), workdir.path().join("Makefile"))
            .expect("copy Makefile");
        // Some testcases reference auxiliary files in sibling subdirectories
        // of testcase/ (e.g. `$(MAKE) -f submake/basic.mk`). Symlink each
        // such subdir into the tmpdir only when the .mk actually mentions
        // it — otherwise tests that walk the filesystem (find, wildcard)
        // start seeing files they shouldn't.
        for sub in &["submake", "dump", "tools"] {
            if !src.contains(sub) {
                continue;
            }
            let from = dir.join(sub);
            if from.is_dir() {
                let _ = std::os::unix::fs::symlink(&from, workdir.path().join(sub));
            }
        }

        let mut mk_cmd = Command::new("make");
        // Push SHELL=/bin/bash via MAKEFLAGS — POSIX-mode make ignores
        // SHELL from the OS env, so we need it as a command-line var,
        // and MAKEFLAGS is the canonical way to do that without leaking
        // the assignment into $(MAKE)'s expansion.
        mk_cmd.env("MAKEFLAGS", "SHELL=/bin/bash");
        let (mk_out, _) = run_with_timeout(mk_cmd, workdir.path());

        // Wipe everything except the Makefile and the staged symlinks so
        // rkati starts from the same state make did. Symlinks we put there
        // (submake/, dump/, tools/) are inputs, not artifacts — leave them.
        for e in std::fs::read_dir(workdir.path()).unwrap().flatten() {
            if e.file_name() == "Makefile" {
                continue;
            }
            if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }

        let mut rk_cmd = Command::new(&rkati);
        // sarun: mirror what the production FUSE shadow does to argv[0].
        // When a box's /usr/bin/make is overlaid to serve the engine
        // binary, the kernel still passes argv[0]="make" (because that's
        // what the shell put in the array), so kati's MAKE/MAKEFLAGS etc.
        // end up as "make". The corpus runner has no FUSE; just set
        // argv[0] directly via Command::arg0 to land on the same state.
        std::os::unix::process::CommandExt::arg0(&mut rk_cmd, "make");
        // SHELL=/bin/bash rides MAKEFLAGS for the same reason as the
        // make side: command-line vars in MAKEFLAGS apply but don't
        // leak into $(MAKE)'s expansion. --use_find_emulator can't ride
        // MAKEFLAGS (kati only consumes key=value); we just drop it
        // — find calls shell out, slower but identical output.
        rk_cmd.env("MAKEFLAGS", "SHELL=/bin/bash");
        let (rk_out, _) = run_with_timeout(rk_cmd, workdir.path());

        // sarun: $(MAKE) now matches between sides for free — rkati's
        // arg0="make" + SHELL=/bin/bash riding MAKEFLAGS leaves
        // subkati_args = ["make"], so $(MAKE) expands to just "make",
        // same as real make. No prefix-stripping required.

        let mk_norm = normalize(&String::from_utf8_lossy(&mk_out), &make_norms());
        let rk_norm = normalize(&String::from_utf8_lossy(&rk_out), &kati_norms());

        if std::env::var("KATI_CORPUS_DEBUG").is_ok() {
            eprintln!("=== {name} ===");
            eprintln!("--- mk raw ---\n{}", String::from_utf8_lossy(&mk_out));
            eprintln!("--- mk norm ---\n{mk_norm}");
            eprintln!("--- rk raw ---\n{}", String::from_utf8_lossy(&rk_out));
            eprintln!("--- rk norm ---\n{rk_norm}");
        }

        // sarun: parallel-conformance — re-run with -j4 from the same initial
        // state and assert order-independent output equivalence to the -j1 run.
        // Only meaningful for cases rkati matches make on (the serial baseline);
        // an xfail case has no trustworthy baseline to compare against.
        if parallel && xfail.is_none() {
            // Wipe the -j1 rkati artifacts, leaving the Makefile + staged symlinks.
            for e in std::fs::read_dir(workdir.path()).unwrap().flatten() {
                if e.file_name() == "Makefile" {
                    continue;
                }
                if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(&p);
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
            let mut pj_cmd = Command::new(&rkati);
            std::os::unix::process::CommandExt::arg0(&mut pj_cmd, "make");
            pj_cmd.env("MAKEFLAGS", "SHELL=/bin/bash");
            pj_cmd.arg("-j4");
            let (pj_out, _) = run_with_timeout(pj_cmd, workdir.path());
            let pj_norm = normalize(&String::from_utf8_lossy(&pj_out), &kati_norms());
            // Output interleaves under -j, so compare on sorted lines. No -jN
            // normalization: $(MAKE) must NOT embed the job count (it rides
            // MAKEFLAGS, like GNU make), so a sub-make recipe echoes identically
            // at -j1 and -j4 — this pass is what proves that.
            let norm_par = |s: &str| {
                let mut v: Vec<&str> = s.lines().collect();
                v.sort_unstable();
                v.join("\n")
            };
            if norm_par(&rk_norm) == norm_par(&pj_norm) {
                par_pass += 1;
            } else {
                par_fail += 1;
                if par_mismatch.len() < 25 {
                    par_mismatch.push(name.clone());
                }
            }
        }

        let matched = mk_norm == rk_norm;
        match (matched, xfail.is_some()) {
            (true, false) => tally.pass += 1,
            (true, true) => {
                tally.xfail_unexpected_pass += 1;
                tally.xpass_names.push(name.clone());
            }
            (false, false) => {
                tally.fail += 1;
                if failures.len() < 25 {
                    failures.push(name.clone());
                }
            }
            (false, true) => {
                tally.xfail += 1;
                tally.xfail_names.push(name.clone());
            }
        }
    }

    let total = tally.pass + tally.fail + tally.xfail + tally.xfail_unexpected_pass;
    println!();
    println!("KATI_COMPAT_PASS={}/{}", tally.pass, total);
    println!(
        "    fail={} xfail={} xpass={} skipped={}",
        tally.fail, tally.xfail, tally.xfail_unexpected_pass, tally.skipped
    );
    if parallel {
        println!(
            "KATI_PARALLEL_EQUIV={}/{} (rkati -j4 output == -j1, order-independent)",
            par_pass,
            par_pass + par_fail
        );
        if !par_mismatch.is_empty() {
            println!("    parallel mismatches (up to 25):");
            for f in &par_mismatch {
                println!("        {f}");
            }
        }
    }
    if !failures.is_empty() {
        println!("    first failing (up to 25):");
        for f in &failures {
            println!("        {f}");
        }
    }
    if !tally.xpass_names.is_empty() {
        println!("    xpass (drop the TODO marker to count as pass):");
        for f in &tally.xpass_names {
            println!("        {f}");
        }
    }
    if std::env::var("KATI_CORPUS_SHOW_XFAIL").is_ok() && !tally.xfail_names.is_empty() {
        println!("    xfail:");
        for f in &tally.xfail_names {
            println!("        {f}");
        }
    }

    // Parallel conformance is a HARD gate when enabled (KATI_CORPUS_PARALLEL=1):
    // every case rkati matches make on serially must produce identical output at
    // -j4 (order-independent). This is how parallel-scheduler conformance is
    // covered — transitively, rkati -j4 == rkati -j1 == make.
    if parallel {
        assert_eq!(
            par_fail, 0,
            "parallel conformance regression: {} case(s) where rkati -j4 != -j1: {:?}",
            par_fail, par_mismatch
        );
    }

    // sarun: the serial corpus is a HARD gate — every case not marked
    // `# TODO` must match GNU make byte-for-byte (post-normalization), so a
    // kati fix pinned by a testcase cannot silently regress. Runs filtered
    // with KATI_CORPUS_ONLY skip this (a partial run has no meaningful tally).
    if only.is_none() {
        assert_eq!(
            tally.fail, 0,
            "kati compat regression: {} non-TODO case(s) diverge from GNU make: {:?}",
            tally.fail, failures
        );
        // An unexpected PASS is stale bookkeeping: the case's `# TODO` header
        // no longer reflects reality, and the behavior it pins could regress
        // invisibly. Un-TODO the case (or fix the header) to re-arm it.
        assert_eq!(
            tally.xfail_unexpected_pass, 0,
            "stale TODO header(s): {} case(s) now PASS but are still marked \
             expected-fail — remove their `# TODO` so they're enforced: {:?}",
            tally.xfail_unexpected_pass, tally.xpass_names
        );
    }

    // Optional stricter floor (e.g. CI ratcheting): KATI_COMPAT_MIN=N.
    if let Ok(min) = std::env::var("KATI_COMPAT_MIN") {
        let min: usize = min.parse().expect("KATI_COMPAT_MIN must be integer");
        assert!(
            tally.pass >= min,
            "kati compat regression: {} < {} (KATI_COMPAT_MIN)",
            tally.pass,
            min
        );
    }
}
