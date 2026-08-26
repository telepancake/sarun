// Embedded Make driven by Bumba's rkati fork. Kati parses and schedules the
// graph in-process; POSIX-shell recipes are executed by Bumba's Brush recipe
// executor without `/bin/sh`. Unsupported non-POSIX SHELL values remain an
// explicit passthrough to kati's ordinary fork/exec path. Structured graph,
// edge, output, activity, and variable events are emitted through EventSink.
//
// Kati retains a process-global FLAGS value for legacy standalone mode switches.
// Bumba parses every invocation independently and copies all execution-relevant
// flags into that invocation's Evaluator, so recursive and concurrent makes do
// not inherit the first make call's scheduling or error policy.
//
// NO-FALLBACK (D9): anything kati cannot parse/evaluate or execute is a VISIBLE
// error and a non-zero exit. We NEVER silently exec the real `make`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use bytes::{BufMut, BytesMut};
use kati::dep::{NamedDepNode, make_dep};
use kati::eval::{Evaluator, FrameType};
use kati::expr::Value;
use kati::loc::Loc;
use kati::symtab::{Symbol, intern, join_symbols};
use kati::var::{VarOrigin, Variable};
use parking_lot::Mutex;

/// True when this process carries a `make`/`gmake` multicall identity.
/// Program identity, not inherited environment, selects the multicall role;
/// recursive builds commonly sanitize their environment.
pub fn is_make_invocation() -> bool {
    let arg0 = std::env::args().next().unwrap_or_default();
    let base = std::path::Path::new(&arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(base, "make" | "gmake")
}

/// Translate the shell's `make` argv into the argv our vendored kati should parse.
/// We inject `--ninja` (a no-op in our direct-execute path — `FLAGS.generate_ninja`
/// is read only by the standalone main.rs/ninja.rs paths, never by run_kati — kept
/// for parity with the argv kati historically parsed), drop make-only flags kati
/// does not understand (e.g. -j is parsed by kati's own -j handling so we keep
/// numeric ones), and pass through -f/-C/targets/VAR=val. argv0 is kept as the
/// original program name (kati uses it only for subkati_args propagation).
/// Returns Err(msg) for a flag we deliberately refuse (visible, no fallback).
fn kati_argv(argv: &[String]) -> Result<Vec<OsString>, String> {
    let mut out: Vec<OsString> = Vec::new();
    // argv0 — kati needs *some* program name; its basename is irrelevant here.
    out.push(OsString::from(
        argv.first().cloned().unwrap_or_else(|| "make".into()),
    ));
    // Inert in our direct-execute path (see fn doc); kept for argv parity.
    out.push(OsString::from("--ninja"));

    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        match a.as_str() {
            // -f FILE / -C DIR / -I DIR: kati understands all three. Pass
            // through verbatim.
            "-f" | "-C" | "-I" => {
                out.push(OsString::from(a));
                if let Some(v) = argv.get(i + 1) {
                    out.push(OsString::from(v));
                }
                i += 2;
            }
            // Combined -fFILE / -CDIR / -IDIR.
            _ if a.starts_with("-f") || a.starts_with("-C") || a.starts_with("-I") => {
                out.push(OsString::from(a));
                i += 1;
            }
            // --include-dir=DIR (GNU make long form of -I).
            _ if a.starts_with("--include-dir") => {
                out.push(OsString::from(a));
                if a == "--include-dir" {
                    if let Some(v) = argv.get(i + 1) {
                        out.push(OsString::from(v));
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            // -jN parallelism: Kati's dependency scheduler uses this worker cap
            // and propagates it to recursive $(MAKE) through the jobserver.
            _ if a.starts_with("-j") => {
                out.push(OsString::from(a));
                i += 1;
            }
            // -lN attached load limit: advisory; kati accepts and ignores.
            _ if a.starts_with("-l") && a[2..].chars().all(|c| c.is_ascii_digit() || c == '.') => {
                out.push(OsString::from(a));
                i += 1;
            }
            // Short flags kati's flags.rs knows or that we handle above.
            // -s silent, -r no-builtin-rules, -R no-builtin-variables,
            // -w print-directory, -k keep-going, -n dry-run (GNU: print,
            // don't execute), -i ignore-errors (GNU). Refuse anything else.
            "-s" | "-r" | "-R" | "-w" | "-k" | "-n" | "-i" | "-B" | "-q" | "-t" | "-l" => {
                out.push(OsString::from(a));
                i += 1;
            }
            _ if a.starts_with("--") => {
                // Pass long flags kati's own parser will accept; if kati rejects
                // it, kati panics with "Unknown flag", which surfaces visibly.
                out.push(OsString::from(a));
                i += 1;
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                return Err(format!(
                    "bumba make: unsupported make flag {a:?} \
                     (embedded kati does not implement it; NO real-make fallback)"
                ));
            }
            // A bare token: a target name or a VAR=val assignment. kati's flags.rs
            // routes `=`-containing tokens to cl_vars and the rest to targets.
            _ => {
                out.push(OsString::from(a));
                i += 1;
            }
        }
    }
    Ok(out)
}

/// kati's bootstrap makefile (ported from upstream main.rs read_bootstrap_makefile).
/// Seeds GNU make's core C/C++/assembler variables and suffix rules, plus the
/// special MAKE/SHELL variables. Keep recipes expressed through COMPILE.* and
/// OUTPUT_OPTION: projects use those relations directly and via implicit rules.
/// Returns the parsed bootstrap stmts.
fn read_bootstrap_makefile(
    targets: &[Symbol],
    working_dir: &std::path::Path,
    no_builtin_rules: bool,
    no_builtin_variables: bool,
    makelevel: u32,
) -> anyhow::Result<Arc<Mutex<Vec<kati::stmt::Stmt>>>> {
    let mut bootstrap = BytesMut::new();
    if !no_builtin_variables {
        bootstrap.put_slice(b"CC?=cc\n");
        if cfg!(target_os = "macos") {
            bootstrap.put_slice(b"CXX?=c++\n");
        } else {
            bootstrap.put_slice(b"CXX?=g++\n");
        }
        bootstrap.put_slice(b"AR?=ar\n");
        bootstrap.put_slice(b"ARFLAGS?=-rv\n");
        bootstrap.put_slice(b"AS?=as\n");
        bootstrap.put_slice(b"CPP?=$(CC) -E\n");
        bootstrap.put_slice(b"COMPILE.c?=$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c\n");
        bootstrap.put_slice(b"COMPILE.cc?=$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c\n");
        bootstrap.put_slice(b"COMPILE.cpp?=$(COMPILE.cc)\n");
        bootstrap.put_slice(b"COMPILE.C?=$(COMPILE.cc)\n");
        bootstrap.put_slice(b"COMPILE.S?=$(CC) $(ASFLAGS) $(CPPFLAGS) $(TARGET_MACH) -c\n");
        bootstrap.put_slice(b"PREPROCESS.S?=$(CPP) $(CPPFLAGS)\n");
        bootstrap.put_slice(b"LINK.o?=$(CC) $(LDFLAGS) $(TARGET_ARCH)\n");
        bootstrap.put_slice(b"LINK.c?=$(CC) $(CFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)\n");
        bootstrap.put_slice(b"LINK.cc?=$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(LDFLAGS) $(TARGET_ARCH)\n");
        bootstrap.put_slice(b"LINK.cpp?=$(LINK.cc)\n");
        bootstrap.put_slice(b"LINK.C?=$(LINK.cc)\n");
        bootstrap.put_slice(b"OUTPUT_OPTION?=-o $@\n");
        bootstrap.put_slice(b"RM?=rm -f\n");
    }
    // Bumba: report GNU make 4.3 (matches our compat target); Makefiles
    // gated on `ifeq ($(MAKE_VERSION),4.x)` see what they expect.
    bootstrap.put_slice(b"MAKE_VERSION?=4.3\n");
    // Bumba: GNU make also reports the triple it was built for. Vendor/SDK
    // Makefiles derive arch- and OS-keyed paths from it (e.g. `include
    // mk/$(word 1,$(subst -, ,$(MAKE_HOST))).mk`) — with MAKE_HOST empty
    // those includes resolve to nothing and whole prerequisite lists vanish
    // SILENTLY (`-include mk/.mk` is not an error). `?=` mirrors GNU
    // origin precedence: an env-seeded value wins over this default.
    bootstrap.put_slice(format!("MAKE_HOST?={}-pc-linux-gnu\n", std::env::consts::ARCH).as_bytes());
    // Bumba: MAKELEVEL tracks recursion across sub-makes. The caller passes
    // the level from the make's OWN inherited environment (seed_env) — many
    // in-process makes share one process env, so reading std::env here gave
    // every nested $(MAKE) level 0.
    bootstrap.put_slice(format!("MAKELEVEL:={makelevel}\n").as_bytes());
    bootstrap.put_slice(b"KATI?=ckati\n");
    bootstrap.put_slice(b"SHELL=/bin/sh\n");
    // Bumba: GNU make 4.x advertises its optional features via the special
    // .FEATURES var; e.g. the Linux kernel's top Makefile gates on
    // `$(filter undefine,$(.FEATURES))` and bails out with "GNU Make >= 3.82
    // is required" if it's empty. Kati implements (or accepts as syntax)
    // each of these — undefine directive, target-specific vars, .ONESHELL,
    // .SECONDEXPANSION, .DELETE_ON_ERROR, else-if, shortest-stem pattern
    // matching, order-only prerequisites — so advertise them; anything kati
    // doesn't actually implement would surface as its own visible error
    // elsewhere, not silently no-op because of a missing .FEATURES token.
    // `output-sync` is the GNU 4.x capability marker used by Linux's top-level
    // Makefile. The embedded executor owns recipe output and coordinates its
    // emission on the make thread, so it has the required synchronization
    // boundary even though the default policy deliberately streams chunks
    // (equivalent to GNU make without an active -O mode).
    // `jobserver` is real here too: jobserver.rs advertises the host's
    // slip pool into MAKEFLAGS, so builds gating their parallel plumbing on
    // $(filter jobserver,$(.FEATURES)) get the coordinated path.
    bootstrap.put_slice(
        b".FEATURES?=target-specific order-only second-expansion else-if \
          shortest-stem undefine oneshell output-sync jobserver\n",
    );
    if !no_builtin_rules {
        bootstrap.put_slice(b".c.o:\n");
        bootstrap.put_slice(b"\t$(COMPILE.c) $(OUTPUT_OPTION) $<\n");
        bootstrap.put_slice(b".cc.o:\n");
        bootstrap.put_slice(b"\t$(COMPILE.cc) $(OUTPUT_OPTION) $<\n");
        bootstrap.put_slice(b".cpp.o:\n");
        bootstrap.put_slice(b"\t$(COMPILE.cpp) $(OUTPUT_OPTION) $<\n");
        bootstrap.put_slice(b".C.o:\n");
        bootstrap.put_slice(b"\t$(COMPILE.C) $(OUTPUT_OPTION) $<\n");
        bootstrap.put_slice(b".S.o:\n");
        bootstrap.put_slice(b"\t$(COMPILE.S) $(OUTPUT_OPTION) $<\n");
        bootstrap.put_slice(b".o:\n");
        bootstrap.put_slice(b"\t$(LINK.o) $^ $(LOADLIBES) $(LDLIBS) -o $@\n");
        bootstrap.put_slice(b".c:\n");
        bootstrap.put_slice(b"\t$(LINK.c) $^ $(LOADLIBES) $(LDLIBS) -o $@\n");
        bootstrap.put_slice(b".cc:\n");
        bootstrap.put_slice(b"\t$(LINK.cc) $^ $(LOADLIBES) $(LDLIBS) -o $@\n");
        bootstrap.put_slice(b".cpp:\n");
        bootstrap.put_slice(b"\t$(LINK.cpp) $^ $(LOADLIBES) $(LDLIBS) -o $@\n");
        bootstrap.put_slice(b".C:\n");
        bootstrap.put_slice(b"\t$(LINK.C) $^ $(LOADLIBES) $(LDLIBS) -o $@\n");
    }
    // Bumba: GNU make's $(MAKE) is the name make was invoked as (argv[0]) — no
    // -jN appended. Parallelism propagates via MAKEFLAGS, not MAKE itself.
    // Without this, sub-`$(MAKE)` recipes echoed verbatim by the parent (e.g.
    // `echo '... $(MAKE) ...'`) would print `make -j4`, diverging from gnu's
    // plain `make`. The FUSE shadow makes `make` route back to the host.
    bootstrap.put_slice(b"MAKE?=make\n");
    bootstrap.put_slice(b"MAKECMDGOALS?=");
    bootstrap.put(join_symbols(targets, b" "));
    bootstrap.put_u8(b'\n');
    // CURDIR is the make's logical working dir (the brush context's cwd / -C
    // target), NOT the host's process cwd — a Makefile computes srctree and
    // resolves `include`s against it (e.g. busyshell's Kbuild).
    bootstrap.put_slice(b"CURDIR:=");
    bootstrap.put_slice(working_dir.as_os_str().as_bytes());
    bootstrap.put_u8(b'\n');
    kati::parser::parse_buf(
        &bootstrap.freeze(),
        Loc {
            filename: intern("*bootstrap*"),
            line: 0,
        },
    )
}

/// Run kati end-to-end: bootstrap + command-line vars + parse the Makefile +
/// dependency analysis + EXECUTE the dep graph via kati's own executor
/// (kati::exec::exec). A port of upstream kati main.rs `run()`, but driving
/// kati's executor directly instead of generating a Ninja graph — Bumba
/// executes in-process and does not emit ninja. Returns Ok on success.
///
/// `remake_active` (in the returned RunKatiResult) means the makefile had at
/// least one required `include` of a file the same makefile has a rule for;
/// kati's executor builds the include target(s) first, then the caller re-execs
/// the process so the next invocation parses with the freshly-generated content
/// visible (GNU make's remake-the-makefile loop).
struct RunKatiResult {
    remake_active: bool,
    /// -q: true iff the main-goal exec found anything that WOULD run
    /// (drives GNU's exit-1 "not up to date" status).
    would_run: bool,
    /// OPTIONAL (-include) remake targets that did NOT materialize this
    /// pass — the re-run passes them back as `noremake` so they aren't
    /// attempted again (GNU proceeds without an unmakeable optional
    /// include instead of looping).
    failed_optional: Vec<Vec<u8>>,
}

fn run_kati(
    targets: &[Symbol],
    cl_vars: &[bytes::Bytes],
    makefile: &OsStr,
    working_dir: &std::path::Path,
    // The environment this make starts from. The shadow/main path passes the
    // process env (std::env); the in-process `make` builtin passes the brush
    // subshell's exported env (which carries the PARENT make's exports applied
    // via the recipe prefix). We never read std::env directly for the make's
    // variables here — many makes share one host process, so that would mix
    // their environments.
    seed_env: &[(std::ffi::OsString, std::ffi::OsString)],
    recipe_stdin: Option<std::sync::Arc<std::os::fd::OwnedFd>>,
    recipe_execution_context: Option<kati::fileutil::RecipeExecutionContext>,
    direct_recipe_output: bool,
    include_dirs: &[OsString],
    no_builtin_rules: bool,
    no_builtin_variables: bool,
    // Bumba: the long-form (`--foo`) flags this make was invoked with, verbatim
    // (see extract_long_flags). GNU make reflects command-line flags into
    // $(MAKEFLAGS) so a Makefile can detect them (e.g. the kernel top Makefile's
    // `$(filter --no-print-directory,$(MAKEFLAGS))` __sub-make guard). Kati
    // doesn't do this automatically — without it that filter never matches and
    // a self-recursing `$(MAKE)` rule spins forever.
    cmdline_flags: &[OsString],
    // Host jobserver words for this invocation only. These become logical
    // MAKEFLAGS and never mutate the environment shared by unrelated builds.
    jobserver_flags: Option<&str>,
    // GNU -n / -i for THIS make instance (a recursive in-process $(MAKE)
    // can't use the once-installed process-global FLAGS). dry_run applies to
    // the MAIN goals only — GNU -n still remakes included makefiles for real.
    dry_run: bool,
    ignore_errors: bool,
    // GNU --trace: per-target run/skip decisions, printed to stderr.
    trace: bool,
    // GNU -B (rebuild unconditionally) / -q (question probe). Both apply to
    // the MAIN goals only; makefile remaking stays real (and un-forced, or
    // -B would re-remake includes every pass and never converge).
    always_make: bool,
    question: bool,
    touch: bool,
    num_jobs: usize,
    jobs_explicit: bool,
    keep_going: bool,
    silent_mode: bool,
    // Optional includes known unmakeable from a previous remake pass —
    // don't queue them again.
    noremake: &std::collections::HashSet<Vec<u8>>,
) -> anyhow::Result<RunKatiResult> {
    let mut ev = Evaluator::new();
    ev.recipe_stdin = recipe_stdin;
    ev.recipe_execution_context = recipe_execution_context;
    ev.direct_recipe_output = direct_recipe_output;
    ev.ignore_errors = ignore_errors;
    ev.goal_trace = trace;
    ev.num_jobs = num_jobs.max(1);
    ev.jobs_explicit = jobs_explicit;
    ev.keep_going = keep_going;
    ev.silent_mode = silent_mode;
    if trace {
        kati::exec::emit_recipe_err(&format!(
            "--trace: make in {} (makefile {:?}), goals {:?}",
            working_dir.display(),
            makefile,
            targets.iter().map(|t| t.to_string()).collect::<Vec<_>>()
        ));
    }
    // Bumba: the Evaluator seeds working_dir from the process cwd; override it
    // with the caller's logical working dir. For the shadow path this equals the
    // process cwd; for the in-process builtin it's the make's dir resolved from
    // -C against the brush context's cwd (no process chdir).
    ev.working_dir = working_dir.to_path_buf();
    ev.include_dirs = include_dirs
        .iter()
        .map(|d| {
            let p = std::path::Path::new(d);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                working_dir.join(p)
            }
        })
        .collect();
    ev.start()?;

    // Bumba: GNU make's MAKEFILE_LIST has no leading space — the main
    // makefile is the very first word (matches rkati main.rs). The old
    // " name" form leaked an extra space into recipes that referenced
    // $(MAKEFILE_LIST).
    let mut makefile_list = BytesMut::new();
    makefile_list.put_slice(makefile.as_bytes());
    ev.set_global_var(
        intern("MAKEFILE_LIST"),
        Variable::with_simple_string(
            makefile_list.freeze(),
            VarOrigin::File,
            Some(ev.current_frame()),
            ev.loc.clone(),
        ),
        false,
        None,
    )?;
    for (k, v) in seed_env {
        let v = bytes::Bytes::from(v.as_bytes().to_vec());
        let val = Arc::new(Value::Literal(None, v.clone()));
        ev.set_global_var(
            intern(k.as_bytes().to_vec()),
            Variable::new_recursive(
                val,
                VarOrigin::Environment,
                Some(ev.current_frame()),
                None,
                v,
            ),
            false,
            None,
        )?;
    }
    // Bumba: $(MAKEFLAGS) is composed from three sources:
    //
    //  * The make's OWN inherited environment (seed_env) — this is how a
    //    parent make's flags reach a sub-make in GNU make. For the shadow
    //    path seed_env IS the process env; for the in-process builtin it's
    //    the brush subshell env carrying the PARENT make's export prefix
    //    (which now includes MAKEFLAGS, see below) — e.g. the Linux kernel's
    //    `MAKEFLAGS += --include-dir=$(abs_srctree)` arrives here, NOT in the
    //    shared process env.
    //
    //  * This invocation's host jobserver advertisement, if any. It is scoped
    //    data, never process-global environment, so the first build's `-jN`
    //    cannot contaminate later builds.
    //
    //  * This invocation's own long-form command-line flags (cmdline_flags),
    //    the way GNU make reflects argv into MAKEFLAGS for Makefiles that
    //    inspect it (see the cmdline_flags doc comment on run_kati). Without
    //    this, a Makefile's `$(filter --foo,$(MAKEFLAGS))` guard never
    //    matches even though --foo was passed on the command line.
    {
        // Merge the three flag sources SECTION-WISE: MAKEFLAGS is
        // `flag-words [-- var-definitions]` (GNU carries command-line
        // variable overrides after `--`, space-escaped) — naive word
        // appends would land flags inside the variable section.
        let seed_mf = seed_env
            .iter()
            .find(|(k, _)| k == "MAKEFLAGS")
            .map(|(_, v)| v.as_bytes().to_vec())
            .unwrap_or_default();
        let (mut fwords, mut fvars) = kati::flags::Flags::split_makeflags(&seed_mf);
        if let Some(jobserver_mf) = jobserver_flags {
            // An explicit -jN owns this invocation's pool. Cargo and other
            // hosts commonly leave their own jobserver in the process
            // environment; retaining both advertisements makes clients select
            // the first (foreign) FIFO and silently serialize when its tokens
            // are occupied. Replace the inherited parallel transport as one
            // section, while preserving unrelated inherited flags.
            let mut inherited = Vec::with_capacity(fwords.len());
            let mut index = 0usize;
            while index < fwords.len() {
                let word = &fwords[index];
                let parallel = word.as_ref() == b"-j"
                    || word.starts_with(b"-j")
                    || word.as_ref() == b"--jobs"
                    || word.starts_with(b"--jobs=")
                    || word.starts_with(b"--jobserver-auth=")
                    || word.starts_with(b"--jobserver-fds=");
                if parallel {
                    if word.as_ref() == b"--jobs"
                        && fwords.get(index + 1).is_some_and(|next| {
                            next.iter().all(|byte| byte.is_ascii_digit())
                        })
                    {
                        index += 1;
                    }
                } else {
                    inherited.push(word.clone());
                }
                index += 1;
            }
            fwords = inherited;
            let (job_words, job_vars) =
                kati::flags::Flags::split_makeflags(jobserver_mf.as_bytes());
            for t in job_words {
                if !fwords.contains(&t) {
                    fwords.push(t);
                }
            }
            for v in job_vars {
                if !fvars.contains(&v) {
                    fvars.push(v);
                }
            }
        }
        for f in cmdline_flags {
            let b = bytes::Bytes::copy_from_slice(f.as_bytes());
            if !fwords.contains(&b) {
                fwords.push(b);
            }
        }
        // THIS invocation's command-line variables propagate to sub-makes
        // through the `--` section, exactly like GNU (`make DESTDIR=/x
        // install` must mean DESTDIR=/x in every sub-make — losing it sent
        // `install -d` at un-prefixed system paths).
        fn command_line_var_name(def: &[u8]) -> &[u8] {
            let Some(eq) = def.iter().position(|&b| b == b'=') else {
                return def;
            };
            let end = if eq > 0 && b":+?!".contains(&def[eq - 1]) {
                eq - 1
            } else {
                eq
            };
            &def[..end]
        }
        fn command_line_var_accumulates(def: &[u8]) -> bool {
            let Some(eq) = def.iter().position(|&b| b == b'=') else {
                return false;
            };
            eq > 0 && matches!(def[eq - 1], b'+' | b'?')
        }
        for v in cl_vars {
            if command_line_var_accumulates(v) {
                // Repeated `NAME+=value` definitions are an ordered program,
                // not alternate spellings of one override. OpenWrt passes a
                // dozen LIBS+= expressions whose wildcards/conditionals must
                // remain deferred and replay in each Automake subdirectory.
                // Exact definitions already present in inherited MAKEFLAGS
                // are not appended again at every recursive level.
                if !fvars.contains(v) {
                    fvars.push(v.clone());
                }
            } else {
                // A recursive make's own `NAME=value` replaces the inherited
                // definition of NAME; retaining both grows MAKEFLAGS at every
                // level and, more importantly, lets a stale parent value win if a
                // consumer replays the list in a different order.
                let name = command_line_var_name(v);
                fvars.retain(|old| command_line_var_name(old) != name);
                fvars.push(v.clone());
            }
        }
        // GNU make keeps the command-line-variable portion of MAKEFLAGS in
        // MAKEOVERRIDES. A makefile can clear MAKEOVERRIDES before invoking a
        // recursive make to stop those overrides at that boundary. GCC does
        // this after passing a temporary CXX override into an intermediate
        // make, allowing the configured CXX in the next layer to take over.
        // Store the already MAKEFLAGS-quoted form; it is spliced back into
        // MAKEFLAGS after parsing, like GNU's special-variable relationship.
        let mut overrides = BytesMut::new();
        for v in &fvars {
            if !overrides.is_empty() {
                overrides.put_u8(b' ');
            }
            overrides.put_slice(&kati::flags::Flags::quote_for_makeflags(v));
        }
        let makeoverrides = intern(b"MAKEOVERRIDES".to_vec());
        if ev.lookup_var(makeoverrides)?.is_none() {
            let value = overrides.freeze();
            let val = Arc::new(Value::Literal(None, value.clone()));
            ev.set_global_var(
                makeoverrides,
                Variable::new_recursive(
                    val,
                    VarOrigin::Default,
                    Some(ev.current_frame()),
                    None,
                    value,
                ),
                false,
                None,
            )?;
        }
        let mut mf = BytesMut::new();
        for w in &fwords {
            if !mf.is_empty() {
                mf.put_u8(b' ');
            }
            mf.put_slice(w);
        }
        if !fvars.is_empty() {
            if !mf.is_empty() {
                mf.put_u8(b' ');
            }
            mf.put_slice(b"--");
            for v in &fvars {
                mf.put_u8(b' ');
                mf.put_slice(&kati::flags::Flags::quote_for_makeflags(v));
            }
        }
        if !mf.is_empty() {
            let v = mf.freeze();
            let val = Arc::new(Value::Literal(None, v.clone()));
            ev.set_global_var(
                intern(b"MAKEFLAGS".to_vec()),
                Variable::new_recursive(
                    val,
                    VarOrigin::Environment,
                    Some(ev.current_frame()),
                    None,
                    v,
                ),
                false,
                None,
            )?;
        }
    }

    // Bumba: this make's MAKELEVEL is whatever the seed env carried; a
    // recipe-spawned sub-make must see the NEXT level. We don't bump the process
    // env (that's a shared global write across concurrent in-process makes) —
    // the +1 is emitted into the export prefix below so children pick it up
    // through their subshell env.
    let makelevel = seed_env
        .iter()
        .find(|(k, _)| k == "MAKELEVEL")
        .and_then(|(_, v)| v.to_str())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let child_makelevel = makelevel + 1;
    ev.box_inherited_exports = seed_env
        .iter()
        .map(|(name, _)| intern(name.as_bytes().to_vec()))
        .collect();
    ev.box_child_makelevel = Some(child_makelevel);
    // Parse-time $(shell ...) must already see this make's inherited
    // environment. refresh_box_export_prefix is called again by each shell
    // expansion, so earlier export/unexport directives and assignments are
    // reflected at their exact point in the makefile.
    ev.refresh_box_export_prefix()?;
    let bootstrap_asts = read_bootstrap_makefile(
        targets,
        working_dir,
        no_builtin_rules,
        no_builtin_variables,
        makelevel,
    )?;
    {
        let _frame = ev.enter(
            FrameType::Phase,
            bytes::Bytes::from_static(b"*bootstrap*"),
            Loc::default(),
        );
        ev.in_bootstrap();
        ev.eval_stmts(&bootstrap_asts)?;
    }
    {
        let _frame = ev.enter(
            FrameType::Phase,
            bytes::Bytes::from_static(b"*command line*"),
            Loc::default(),
        );
        ev.in_command_line();
        for l in cl_vars {
            let asts = kati::parser::parse_buf(
                l,
                Loc {
                    filename: intern("*bootstrap*"),
                    line: 0,
                },
            )?;
            ev.eval_stmts(&asts)?;
        }
    }
    ev.in_toplevel_makefile();
    {
        let _eval_frame = ev.enter(
            FrameType::Phase,
            bytes::Bytes::from_static(b"*parse*"),
            Loc::default(),
        );
        let _file_frame = ev.enter(
            FrameType::Parse,
            bytes::Bytes::from(makefile.as_bytes().to_vec()),
            Loc::default(),
        );
        let Some(mk) = kati::file_cache::get_makefile(makefile, &ev.working_dir)? else {
            anyhow::bail!("makefile not found");
        };
        ev.eval_stmts(&mk.stmts)?;
    }

    // Changing MAKEOVERRIDES rewrites only MAKEFLAGS' command-variable tail;
    // ordinary flags (including makefile additions like `MAKEFLAGS += -rR`)
    // survive. Reconcile that GNU make relationship before recipes and
    // recursive makes see the exported value.
    {
        let makeflags = intern(b"MAKEFLAGS".to_vec());
        let makeoverrides = intern(b"MAKEOVERRIDES".to_vec());
        let current = ev.eval_var(makeflags)?;
        // MAKEOVERRIDES is substituted into MAKEFLAGS only while MAKEFLAGS
        // still contains its `--` command-variable slot. `override
        // MAKEFLAGS=` deliberately removes that slot; OpenWrt uses this at
        // package boundaries so its top-level `V=s` does not override an
        // unrelated package makefile's own `V` (Lua's version suffix).
        let has_override_slot = current
            .split(|byte| byte.is_ascii_whitespace())
            .any(|word| word == b"--");
        let (flag_words, _) = kati::flags::Flags::split_makeflags(&current);
        let overrides = ev.eval_var(makeoverrides)?;
        let mut reconciled = BytesMut::new();
        for word in flag_words {
            if !reconciled.is_empty() {
                reconciled.put_u8(b' ');
            }
            reconciled.put_slice(&word);
        }
        if has_override_slot && !overrides.is_empty() {
            if !reconciled.is_empty() {
                reconciled.put_u8(b' ');
            }
            reconciled.put_slice(b"-- ");
            reconciled.put_slice(&overrides);
        }
        let value = reconciled.freeze();
        ev.set_global_var(
            makeflags,
            Variable::with_simple_string(
                value,
                VarOrigin::File,
                Some(ev.current_frame()),
                ev.loc.clone(),
            ),
            false,
            None,
        )?;
    }

    // Bumba: GNU make's remake-the-makefile loop. Every included makefile is
    // itself a build target, including one that already existed while parsing.
    // Build those targets first; only request a reparse when the executor
    // actually ran something. This distinction is what lets a stale generated
    // config refresh while an up-to-date include proceeds to the real goals.
    let mut remake_targets: Vec<(Symbol, bool)> = Vec::new();
    {
        let pending = std::mem::take(&mut ev.pending_remake_includes);
        for (loc, name, required) in &pending {
            let sym = intern(name.as_bytes().to_vec());
            // A rule can produce the missing include either literally or via a
            // PATTERN target — the kernel regenerates include/config/auto.conf
            // through `%/auto.conf %/auto.conf.cmd: $(KCONFIG_CONFIG)`.
            let producible = ev.rules.iter().any(|r| {
                r.outputs.contains(&sym)
                    || r.output_patterns.iter().any(|p| {
                        kati::strutil::Pattern::new(bytes::Bytes::from(p.as_bytes().to_vec()))
                            .matches(&name.as_bytes())
                    })
            });
            let path = std::path::Path::new(name);
            let exists = if path.is_absolute() {
                kati::filesystem::exists(path).unwrap_or(false)
            } else {
                kati::filesystem::exists(working_dir.join(path)).unwrap_or(false)
            };
            if producible && (*required || !noremake.contains(name.as_bytes())) {
                remake_targets.push((sym, *required));
            } else if *required && !exists {
                let pat_str = String::from_utf8_lossy(name.as_bytes());
                eprintln!("{loc}: {pat_str}: No such file or directory");
                std::process::exit(2);
            }
            // A missing OPTIONAL include with no rule (or one already known
            // unmakeable): GNU tolerates it.
        }
    }
    // Materialize the final recipe environment after parsing. The same helper
    // is called immediately before every parse-time $(shell ...) expansion.
    ev.refresh_box_export_prefix()?;

    // GNU make restarts only when an included makefile itself changed, not
    // merely because some command in its dependency graph ran.  Autotools
    // makefiles deliberately have rebuild recipes which inspect generated
    // inputs and leave the makefile untouched when its embedded revision is
    // already current.  Treating "executor ran anything" as "makefile was
    // remade" makes those perfectly converged graphs restart forever.
    //
    // Match GNU's observable test: snapshot the included target's stat before
    // its graph runs and compare it afterwards.  A missing file becoming
    // present counts; an unchanged mtime/size does not.  Do not hash contents
    // here -- this path is exercised for every included dependency file in
    // large generated builds, and GNU itself uses filesystem timestamps.
    fn include_stamp(
        working_dir: &std::path::Path,
        sym: Symbol,
    ) -> Option<(std::time::SystemTime, u64)> {
        let bytes = sym.as_bytes();
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&bytes));
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            working_dir.join(path)
        };
        let meta = kati::filesystem::metadata(&abs).ok()?;
        Some((kati::filesystem::modified(&abs).ok()?, meta.len))
    }

    let before_stamps: std::collections::HashMap<Symbol, _> = remake_targets
        .iter()
        .map(|(sym, _)| (*sym, include_stamp(working_dir, *sym)))
        .collect();
    let mut successfully_checked = std::collections::HashSet::new();
    if !remake_targets.is_empty() {
        let remake_nodes = {
            let _frame = ev.enter(
                FrameType::Phase,
                bytes::Bytes::from_static(b"*remake dependency analysis*"),
                Loc::default(),
            );
            make_dep(
                &mut ev,
                remake_targets.iter().map(|(symbol, _)| *symbol).collect(),
            )?
        };
        emit_build_edges_kati(&remake_nodes);
        let _frame = ev.enter(
            FrameType::Phase,
            bytes::Bytes::from_static(b"*remake*"),
            Loc::default(),
        );
        let required: std::collections::HashSet<Symbol> = remake_targets
            .iter()
            .filter(|(_, req)| *req)
            .map(|(s, _)| *s)
            .collect();
        let (req_nodes, opt_nodes): (Vec<_>, Vec<_>) = remake_nodes
            .into_iter()
            .partition(|(s, _)| required.contains(s));
        if !req_nodes.is_empty() {
            kati::exec::exec(req_nodes, &mut ev)?;
            successfully_checked.extend(required.iter().copied());
        }
        for node in opt_nodes {
            let sym = node.0;
            // GNU tolerates a failed remake of an optional included makefile.
            if kati::exec::exec_opts(vec![node], &mut ev, true).is_ok() {
                successfully_checked.insert(sym);
            }
        }
    }
    let remake_active = successfully_checked
        .into_iter()
        .any(|sym| before_stamps.get(&sym).copied().flatten() != include_stamp(working_dir, sym));
    // Optional remake targets that did not materialize are reported back so
    // the re-run skips them (otherwise a permanently-unmakeable -include
    // would re-queue every pass until the depth cap).
    let failed_optional: Vec<Vec<u8>> = remake_targets
        .iter()
        .filter(|(sym, required)| {
            if *required {
                return false;
            }
            let b = sym.as_bytes();
            let p = std::path::Path::new(std::ffi::OsStr::from_bytes(&b));
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                working_dir.join(p)
            };
            !kati::filesystem::exists(abs).unwrap_or(false)
        })
        .map(|(sym, _)| sym.as_bytes().to_vec())
        .collect();
    if remake_active {
        ev.finish()?;
        return Ok(RunKatiResult {
            remake_active: true,
            failed_optional,
            would_run: false,
        });
    }

    let nodes = {
        let _frame = ev.enter(
            FrameType::Phase,
            bytes::Bytes::from_static(b"*dependency analysis*"),
            Loc::default(),
        );
        make_dep(&mut ev, targets.to_owned())?
    };
    // Emit provenance before execution so even skipped/up-to-date goals are
    // visible in the build-target pane.
    emit_build_edges_kati(&nodes);
    {
        let _frame = ev.enter(
            FrameType::Phase,
            bytes::Bytes::from_static(b"*execute*"),
            Loc::default(),
        );
        // GNU -n/-B/-q apply to the main goals only; makefile remakes above
        // were always real and unforced.
        ev.dry_run = dry_run;
        ev.always_make = always_make;
        ev.question = question;
        ev.touch = touch;
        kati::exec::exec(nodes, &mut ev)?;
        ev.dry_run = false;
        ev.always_make = false;
        ev.question = false;
        ev.touch = false;
    }
    let would_run = ev.question_would_run;
    ev.finish()?;
    Ok(RunKatiResult {
        remake_active: false,
        failed_optional,
        would_run,
    })
}

/// Walk the kati dep graph reachable from `roots` and ship one
/// `build_edges` control frame carrying {outs, ins, cmd} for every
/// distinct node — same shape Phase 1 emitted from the n2 graph. The
/// frame drives the UI's build target pane (ui.rs::build_edges_lines).
/// Mirrors the contract of `crate::runner::send_nested_prov` and
/// `control.rs::build_edges`.
///
/// `cmd` is the recipe TEMPLATE text joined with newlines (kati's
/// pre-evaluation form, e.g. `$(CC) -o $@ $<`). Evaluating cmds at
/// emit time would re-run `$(shell …)` side-effects, so we keep the
/// template; the UI labels it accurately. Phony targets carry an
/// empty cmd string.
fn emit_build_edges_kati(roots: &[NamedDepNode]) {
    use kati::dep::NamedDepNode as N;
    use std::collections::HashSet;

    let mut seen: HashSet<kati::symtab::Symbol> = HashSet::new();
    let mut edges: Vec<crate::BuildEdge> = Vec::new();

    fn visit(
        node: &N,
        seen: &mut HashSet<kati::symtab::Symbol>,
        edges: &mut Vec<crate::BuildEdge>,
    ) {
        let (sym, dep) = node;
        if !seen.insert(*sym) {
            return;
        }
        let guard = dep.lock();
        let outs: Vec<String> = std::iter::once(guard.output.to_string())
            .chain(guard.implicit_outputs.iter().map(|s| s.to_string()))
            .collect();
        let ins: Vec<String> = guard.actual_inputs.iter().map(|s| s.to_string()).collect();
        // Recipe text. Evaluating cmds at frame-emit time would re-run
        // `$(shell …)` and other macro side effects, so we reconstruct the
        // make-SOURCE form statically instead (Value::static_string: literal
        // bytes verbatim, variable/function refs rendered back to their `$(…)`
        // surface form, automatic vars as `$@`/`$<`). No evaluation, no side
        // effects — faithful to the literal command bytes, which is what the
        // provenance/UI panes want. Each recipe line is one cmd; join with \n.
        let cmd: String = guard
            .cmds
            .iter()
            .map(|c| c.static_string())
            .collect::<Vec<_>>()
            .join("\n");
        edges.push(crate::BuildEdge {
            outputs: outs,
            inputs: ins,
            command: Some(cmd),
        });
        // Walk children (deps + order-only). Phase 1's n2-graph walk
        // emitted every edge in the graph; mirror that by recursing.
        let deps = guard.deps.clone();
        let order_onlys = guard.order_onlys.clone();
        drop(guard);
        for d in &deps {
            visit(d, seen, edges);
        }
        for d in &order_onlys {
            visit(d, seen, edges);
        }
    }

    for r in roots {
        visit(r, &mut seen, &mut edges);
    }

    crate::event::emit(crate::Event::BuildGraph {
        tool: "make".into(),
        edges,
    });
}

/// The long-form (`--foo`, `--foo=bar`) flags from a make invocation's
/// ORIGINAL argv, in order — used to fold this make's own command-line flags
/// into $(MAKEFLAGS) (see run_kati's cmdline_flags doc comment). Deliberately
/// reads the caller's real argv, not the synthesized kati_argv() (which
/// injects `--ninja`, a Bumba-internal detail no Makefile should observe).
fn extract_long_flags(argv: &[String]) -> Vec<OsString> {
    argv.iter()
        .skip(1)
        .filter(|a| a.starts_with("--"))
        .map(OsString::from)
        .collect()
}

/// Install brush as kati's in-process recipe runner so kati::exec::exec runs
/// every recipe IN-PROCESS via embedded brush (NO fork+exec of /bin/sh per
/// recipe). Merged stdout+stderr flow through brush's pipe machinery to the
/// `output_cb` kati provides; kati then routes them via `emit_recipe_output`
/// (process stdout for the shadow path, or an in-process builtin's logical
/// stdout when one set the thread-local sink).
///
/// Honors `SHELL := ...`: anything other than a /bin/sh-shaped path (sh, bash,
/// dash, ash, ksh, zsh) makes the runner decline (Passthrough) so kati's
/// exec.rs falls back to fork+exec — makefiles using SHELL=echo etc. still work
/// as gnu make / standalone rkati do. Process-global + idempotent (last wins);
/// safe to call from both the shadow entry and the builtin.
/// Batched makefile-variable-assignment records, shipped as `make_vars`
/// frames. Values are capped (a 100KB variable is not debugged whole) and
/// flushed every 256 records plus at each make's end.
pub(crate) static MAKEVAR_BUF: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());

/// Queue one variable-provenance row (shared by the make hook here and the
/// brush shell-assignment observer); flushes every 256 rows.
pub fn push_makevar(row: serde_json::Value) {
    let mut buf = MAKEVAR_BUF.lock();
    buf.push(row);
    if buf.len() >= 256 {
        drop(buf);
        flush_makevars();
    }
}

pub(crate) fn flush_makevars() {
    let rows: Vec<serde_json::Value> = std::mem::take(&mut *MAKEVAR_BUF.lock());
    if rows.is_empty() {
        return;
    }
    for fields in rows {
        crate::event::emit(crate::Event::VariableAssignment { fields });
    }
}

/// Variable tracing is opt-in so a large build pays nothing when unused.
pub fn vartrace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BUMBA_TRACE_VARS").is_some_and(|v| v == "1"))
}

/// Make functions whose `$(name …)` head is NOT a variable dereference. The
/// first argument of `call`/`value` IS one, so those record it.
const MAKE_FUNCS: &[&str] = &[
    "subst",
    "patsubst",
    "strip",
    "findstring",
    "filter",
    "filter-out",
    "sort",
    "word",
    "wordlist",
    "words",
    "firstword",
    "lastword",
    "dir",
    "notdir",
    "suffix",
    "basename",
    "addsuffix",
    "addprefix",
    "join",
    "wildcard",
    "realpath",
    "abspath",
    "if",
    "or",
    "and",
    "intcmp",
    "foreach",
    "file",
    "eval",
    "origin",
    "flavor",
    "shell",
    "guile",
    "error",
    "warning",
    "info",
    "let",
];

/// Best-effort text scan of an UNEXPANDED rhs for the variables it
/// dereferences: `$(NAME)` / `${NAME}` (make), `$NAME` / `${NAME}` (shell).
/// Make function heads don't count (`$(call NAME,…)` / `$(value NAME)` count
/// their first argument instead); `$$` is a literal; automatic vars ($@ $< …)
/// have no name characters and fall out naturally. Function ARGUMENTS keep
/// being scanned, so `$(patsubst %.c,%.o,$(SRCS))` yields SRCS. Returns the
/// unique names in first-appearance order, space-joined — frugal, greppable.
pub fn extract_var_refs(rhs: &str) -> String {
    fn name_at(b: &[u8], i: usize) -> usize {
        let mut j = i;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'.' | b'-')) {
            j += 1;
        }
        j
    }
    let b = rhs.as_bytes();
    let mut refs: Vec<String> = vec![];
    let mut push = |s: &str| {
        if s.bytes().any(|c| c.is_ascii_alphabetic() || c == b'_') && !refs.iter().any(|r| r == s) {
            refs.push(s.to_string());
        }
    };
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        match b[i + 1] {
            b'$' => i += 2, // literal $
            b'(' | b'{' => {
                let start = i + 2;
                let end = name_at(b, start);
                let head = &rhs[start..end];
                if head == "call" || head == "value" {
                    // skip whitespace, the first argument is the deref
                    let mut k = end;
                    while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
                        k += 1;
                    }
                    let e2 = name_at(b, k);
                    push(&rhs[k..e2]);
                } else if !MAKE_FUNCS.contains(&head) {
                    push(head);
                }
                i = end.max(start); // keep scanning inside the parens
            }
            _ => {
                let end = name_at(b, i + 1);
                push(&rhs[i + 1..end]);
                i = end.max(i + 2);
            }
        }
    }
    let mut out = refs.join(" ");
    if out.len() > 1024 {
        out.truncate(1024);
    }
    out
}

/// Cap a recorded text field: a 100KB variable is not debugged whole.
pub fn cap_text(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut m = max;
        while m > 0 && !s.is_char_boundary(m) {
            m -= 1;
        }
        s.truncate(m);
        s.push('…');
    }
    s
}

fn install_var_recorder() {
    kati::fileutil::install_var_assign_hook(Arc::new(
        |name, loc, value, make_dir, rhs, op, origin| {
            if !vartrace_enabled() {
                return;
            }
            let v = cap_text(String::from_utf8_lossy(value).into_owned(), 4096);
            let rhs_s = cap_text(String::from_utf8_lossy(rhs).into_owned(), 1024);
            let refs = extract_var_refs(&rhs_s);
            // Compact flags: the assignment op, plus the variable's origin
            // when it isn't the ordinary makefile case ("file").
            let flags = match origin {
                "file" => op.to_string(),
                "environment" => format!("{op} env"),
                "environment override" => format!("{op} env!"),
                "command line" => format!("{op} cmd"),
                "override" => format!("{op} ovr"),
                "automatic" => format!("{op} auto"),
                other => format!("{op} {other}"),
            };
            // A builtin sub-make parses on the invoking recipe's thread, so
            // the recipe-edge / pipeline context links its assignments to the
            // spawning edge; a top-level (shadow-process) make has neither.
            let context = crate::event::current_context();
            let edge = context
                .edge
                .or_else(crate::shell::current_recipe_edge);
            push_makevar(serde_json::json!({
                "name": String::from_utf8_lossy(name),
                "loc": loc,
                "value": v,
                "make": String::from_utf8_lossy(make_dir),
                "rhs": rhs_s,
                "refs": refs,
                "edge": edge,
                "uid": context.correlation_id,
                "flags": flags,
            }));
        },
    ));
}

/// Stall visibility: ship the in-flight builtin/shell activity to the
/// the event sink every 30s and put a STALL line on
/// stderr once something runs 5+ minutes without completing — a silent
/// hang is a diagnosability bug. Idempotent; shared by the make entries
/// and every logical shell.
pub fn start_activity_reporting() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        kati::fileutil::start_stall_watchdog(
            std::env::var("BUMBA_STALL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
            Arc::new(|line| eprintln!("{line}")),
        );
        std::thread::Builder::new()
            .name("bumba-activity-feed".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    let snap = kati::fileutil::activity_snapshot();
                    if snap.is_empty() {
                        continue;
                    }
                    for (description, age_seconds) in snap {
                        crate::event::emit(crate::Event::Activity {
                            description,
                            age_seconds: age_seconds as f64,
                        });
                    }
                }
            })
            .ok();
    });
}

/// GNU `make -f -`: the makefile arrives on STDIN (automake's depfiles
/// bootstrap pipes a sed-filtered Makefile through `$MAKE -f - am--depfiles`).
/// Spool it to a private temp file and hand kati that path; the caller
/// passes the LOGICAL stdin (the builtin's fd 0) or the process stdin
/// (shadow path). Returns the temp path to use as the makefile.
fn spool_stdin_makefile(stdin: &mut dyn std::io::Read) -> std::io::Result<std::ffi::OsString> {
    use std::io::Write as _;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "bumba-stdin-makefile-{}-{n}.mk",
        std::process::id()
    ));
    let mut buf = Vec::new();
    stdin.read_to_end(&mut buf)?;
    let mut f = std::fs::File::create(&path)?;
    f.write_all(&buf)?;
    Ok(path.into_os_string())
}

fn install_make_recipe_runner() {
    install_var_recorder();
    start_activity_reporting();
    kati::fileutil::install_recipe_runner(Arc::new(
        |shell, _shellflag, prefix, cmd, cwd, stdin, redirect_stderr, direct_output,
         execution_context, output_cb| {
            use kati::fileutil::RecipeRunnerDecision;
            let posix_shell = kati::fileutil::is_posix_shell_command(shell);
            if !posix_shell {
                return RecipeRunnerDecision::Passthrough;
            }
            let s = std::str::from_utf8(cmd)
                .map(std::borrow::Cow::Borrowed)
                .unwrap_or_else(|_| String::from_utf8_lossy(cmd));
            let p = String::from_utf8_lossy(prefix);
            // The recipe cwd is threaded EXPLICITLY from kati (the make's working_dir)
            // rather than read from a make-thread thread-local — under -j the recipe
            // runs on a worker thread that wouldn't see it. Set it for THIS worker
            // thread around the run (save/restore so nested makes nest cleanly).
            let cwd_path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(cwd));
            let prev = crate::shell::set_recipe_cwd(Some(cwd_path));
            // Map kati's stderr disposition to brush's fd-2 handling: recipes
            // (RedirectStderr::Stdout) merge stderr into the captured output; a
            // $(shell ...) (RedirectStderr::None) keeps stderr on the shell's real
            // fd 2 (terminal/sink) and captures only stdout; DevNull discards it.
            let stderr_mode = match redirect_stderr {
                kati::fileutil::RedirectStderr::Stdout => crate::shell::RecipeStderr::Merge,
                kati::fileutil::RedirectStderr::None => crate::shell::RecipeStderr::Inherit,
                kati::fileutil::RedirectStderr::DevNull => crate::shell::RecipeStderr::Null,
            };
            let stdin = stdin.and_then(|fd| fd.try_clone().ok());
            let logical_io = execution_context.and_then(|context| {
                context
                    .as_ref()
                    .downcast_ref::<crate::shell::LogicalRecipeIo>()
            });
            let code = if direct_output {
                crate::shell::run_recipe_direct(&p, &s, stderr_mode, stdin)
            } else if let Some(io) = logical_io
                && std::env::var_os("BUMBA_TARGET_LOG_DIR").is_none()
            {
                crate::shell::run_recipe_inherited(&p, &s, io, stderr_mode, stdin)
            } else {
                crate::shell::run_recipe(&p, &s, output_cb, stderr_mode, stdin)
            };
            crate::shell::set_recipe_cwd(prev);
            RecipeRunnerDecision::Ran { code }
        },
    ));
    // Report each node's recipe run-state to the host, keyed by the node's
    // primary output (== the build_edges row's outs[0]), so the targets pane
    // shows only the targets currently building and their wall time.
    kati::fileutil::install_edge_reporter(Arc::new(
        |output: &[u8], phase, code, excerpt: &[u8]| {
            write_target_log(output, phase, excerpt);
            if phase == kati::fileutil::EdgePhase::Output {
                return;
            }
            let out = String::from_utf8_lossy(output);
            // Tag this worker thread with the edge whose recipe is about to run
            // (cleared on Done): every pipeline the recipe spawns records
            // `edge_out`, the exact edge → pipeline causal link the UI's
            // cross-navigation follows.
            crate::shell::set_recipe_edge(match phase {
                kati::fileutil::EdgePhase::Start => Some(out.to_string()),
                kati::fileutil::EdgePhase::Done => None,
                kati::fileutil::EdgePhase::Output => unreachable!(),
            });
            let ex = String::from_utf8_lossy(excerpt);
            if phase == kati::fileutil::EdgePhase::Done && code != 0 && !ex.trim().is_empty() {
                eprintln!("target {out} failed:\n{}", ex.trim_end());
            }
            match phase {
                kati::fileutil::EdgePhase::Start => crate::event::emit(
                    crate::Event::EdgeStarted { output: out.into_owned(), command: None },
                ),
                kati::fileutil::EdgePhase::Done => crate::event::emit(
                    crate::Event::EdgeFinished { output: out.into_owned(), code },
                ),
                kati::fileutil::EdgePhase::Output => unreachable!(),
            }
        },
    ));
}

fn write_target_log(output: &[u8], phase: kati::fileutil::EdgePhase, bytes: &[u8]) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::io::Write;
    use std::sync::OnceLock;

    static LOGS: OnceLock<Mutex<std::collections::HashMap<Vec<u8>, std::fs::File>>> =
        OnceLock::new();
    let Some(directory) = std::env::var_os("BUMBA_TARGET_LOG_DIR") else {
        return;
    };
    let logs = LOGS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut logs = logs.lock();
    match phase {
        kati::fileutil::EdgePhase::Start => {
            let directory = std::path::PathBuf::from(directory);
            if std::fs::create_dir_all(&directory).is_err() {
                return;
            }
            let mut hasher = DefaultHasher::new();
            output.hash(&mut hasher);
            let label = String::from_utf8_lossy(output);
            let basename = label
                .rsplit('/')
                .next()
                .unwrap_or("target")
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                        character
                    } else {
                        '_'
                    }
                })
                .take(80)
                .collect::<String>();
            let path = directory.join(format!("{basename}-{:016x}.log", hasher.finish()));
            if let Ok(mut file) = std::fs::File::create(path) {
                let _ = writeln!(file, "target: {label}");
                logs.insert(output.to_vec(), file);
            }
        }
        kati::fileutil::EdgePhase::Output => {
            if let Some(file) = logs.get_mut(output) {
                let _ = file.write_all(bytes);
                let _ = file.flush();
            }
        }
        kati::fileutil::EdgePhase::Done => {
            if let Some(file) = logs.remove(output) {
                let _ = file.sync_all();
            }
        }
    }
}

/// GNU-shaped `make --help` text for the embedded make: the options the
/// embedded kati actually understands (anything else errors visibly, per the
/// no-fallback rule). Printed by both the shadow and builtin entries.
const MAKE_HELP: &str = "\
Usage: make [options] [target] ...\n\
Options (supported by Bumba's embedded make):\n\
  -C DIRECTORY                Change to DIRECTORY before doing anything.\n\
  -f FILE                     Read FILE as a makefile.\n\
  -j [N]                      Allow N jobs at once.\n\
  -I DIRECTORY, --include-dir=DIRECTORY\n\
                              Search DIRECTORY for included makefiles.\n\
  -k                          Keep going when some targets can't be made.\n\
  -n                          Don't actually run any recipe; just print them.\n\
  -t, --touch                 Touch out-of-date targets instead of remaking.\n\
  -s, --silent, --quiet       Don't echo recipes.\n\
  -r, --no-builtin-rules      Disable the built-in implicit rules.\n\
  -R, --no-builtin-variables  Disable the built-in variable settings.\n\
  -w, --print-directory / --no-print-directory\n\
                              Print (or not) the working directory.\n\
  -v, --version               Print the version number and exit.\n\
  -h, --help                  Print this message and exit.\n\
  NAME=VALUE                  Set variable NAME to VALUE.\n\
Diagnostics:\n\
  --dump_variable_assignment_trace=-  --variable_assignment_trace_filter=NAME\n\
                              Trace every assignment/lookup of NAME (stderr).\n\
\n\
This is Bumba's embedded make (rkati); unsupported GNU make flags fail\n\
visibly rather than falling back to a real make.\n";

/// The embedded-make entrypoint. `argv` is the FULL process argv (argv[0] is
/// `make`/`gmake`). Returns the process exit code.
pub fn make_main(argv: &[String]) -> i32 {
    install_make_recipe_runner();
    use std::os::fd::AsFd as _;
    let recipe_stdin = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .map(std::sync::Arc::new);

    // 2. Recognized make pseudo-actions BEFORE kati's flags parser sees them
    //    (kati panics on anything it doesn't recognize, e.g. --version). The
    //    shell's FUSE shadow on /usr/bin/make loops `$(shell make --version | ...)`
    //    style probes back into THIS process; emit a gnu-make-shaped version
    //    banner so makefiles that grep `Make ([0-9])` extract a sane MAKEVER.
    //    Done before -C handling so a recipe like `make -C sub --version`
    //    short-circuits without a chdir.
    for a in argv.iter().skip(1) {
        if a == "--version" || a == "-v" {
            println!("GNU Make 4.3");
            println!("Built for {}-pc-linux-gnu", std::env::consts::ARCH);
            println!("Copyright (C) 1988-2020 Free Software Foundation, Inc.");
            println!(
                "License GPLv3+: GNU GPL version 3 or later <http://gnu.org/licenses/gpl.html>"
            );
            println!("This is free software: you are free to change and redistribute it.");
            println!("There is NO WARRANTY, to the extent permitted by law.");
            return 0;
        }
        if a == "--help" || a == "-h" {
            print!("{MAKE_HELP}");
            return 0;
        }
    }

    // 3. Honour `-C dir` ourselves up front so kati's chdir and the makefile
    //    lookup agree (kati also chdir's on -C; set_current_dir is idempotent
    //    for the same dir).
    {
        let mut i = 1;
        while i < argv.len() {
            if argv[i] == "-C" {
                if let Some(d) = argv.get(i + 1) {
                    let _ = std::env::set_current_dir(d);
                }
                i += 2;
            } else if let Some(d) = argv[i].strip_prefix("-C") {
                if !d.is_empty() {
                    let _ = std::env::set_current_dir(d);
                }
                i += 1;
            } else {
                i += 1;
            }
        }
    }

    // 3. Synthesize the kati argv (forces --ninja). Install it into the global
    //    FLAGS for the immutable mode-switches; this is idempotent — a repeated
    //    install in the same process (a second in-process make) is tolerated,
    //    the mode-switches are identical. The PER-INSTANCE inputs (makefile,
    //    targets, cl_vars, working_dir) come from a LOCAL Flags parsed from the
    //    same argv, so multiple makes sharing one process don't collide on them.
    //    A refused flag is a visible error, no fallback.
    let kargv = match kati_argv(argv) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };
    let _ = kati::flags::install_args(kargv.clone());
    let flags = kati::flags::Flags::from_args(kargv);

    // 4. kati needs a makefile; if none was given on the argv, discover the
    //    default like real make/kati (GNUmakefile / makefile / Makefile).
    let makefile: OsString = match flags.makefile.lock().clone() {
        // `-f -`: the makefile arrives on stdin (automake depfiles bootstrap).
        Some(m) if m == "-" => match spool_stdin_makefile(&mut std::io::stdin()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bumba make: cannot read makefile from stdin: {e}");
                return 2;
            }
        },
        Some(m) => m,
        None => {
            let mut found = None;
            for cand in ["GNUmakefile", "makefile", "Makefile"] {
                if kati::filesystem::metadata(cand).is_ok() {
                    found = Some(OsString::from(cand));
                    break;
                }
            }
            match found {
                Some(m) => m,
                None => {
                    eprintln!("bumba make: no makefile found (and none given with -f)");
                    return 2;
                }
            }
        }
    };

    // 4b. If `-f <file>` named a missing makefile, emit gnu-shaped error
    //     output and exit 2 so a recipe like `$(MAKE) -f missing.mk` running
    //     under the shell's FUSE-shadowed `make` prints what gnu make would.
    //     Standalone rkati's recursive-make recipe resolves $(MAKE) through
    //     PATH and lands on /usr/bin/make (gnu) for the sub-make, so the
    //     standalone corpus runner only ever sees gnu's framing for this
    //     case — and the corpus comparator was written against gnu. Without
    //     this, embedded mode submake_basic diverges from the standalone pass set.
    //
    //     We DO NOT emit Entering/Leaving directory messages because that's
    //     gated on MAKELEVEL > 0 + `--print-directory`; the simpler
    //     "submake/basic.mk: No such file or directory" + "No rule to make
    //     target" pair is what survives the corpus runner's make[N]:
    //     Entering/Leaving strip anyway.
    {
        if kati::filesystem::metadata(&makefile).is_err() {
            let display = makefile.to_string_lossy();
            // No " Stop." suffix: rkati standalone doesn't emit it, so
            // kati_norms doesn't strip it; gnu does emit it but make_norms
            // strips it. Matching rkati's no-Stop form is what makes Bumba ↔ GNU
            // (post-norms) line up.
            eprintln!("make: {display}: No such file or directory");
            eprintln!("make: *** No rule to make target '{display}'.");
            return 2;
        }
    }

    // 5. Run kati end-to-end: parse → dependency graph → bounded parallel
    //    execution. The scheduler uses real mtimes and persistent recipe
    //    workers; Brush executes POSIX recipes without `/bin/sh`. NO ninja
    //    generation and NO n2 are involved in this path.
    let targets: Vec<Symbol> = flags.targets.clone();
    let cl_vars: Vec<bytes::Bytes> = flags.cl_vars.clone();
    let include_dirs: Vec<OsString> = flags.include_dirs.clone();
    // Bumba: shadow/main() path — working dir is the process cwd (already
    // chdir'd for -C above), so this matches the Evaluator's own default.
    let shadow_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    // Shadow/main() path is one OS process per make — seed from the process env.
    let seed_env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let cmdline_flags = extract_long_flags(argv);
    let parallel_advertisement =
        crate::jobserver::explicit_jobs(argv).map(crate::jobserver::advertisement);
    let literal_external_before = crate::shell::literal_external_launches();
    // Optional includes a previous remake pass could not build (colon-joined,
    // via the re-exec env) — skip re-queueing them this pass.
    let noremake: std::collections::HashSet<Vec<u8>> = std::env::var_os("BUMBA_KATI_NOREMAKE")
        .map(|v| {
            std::os::unix::ffi::OsStrExt::as_bytes(v.as_os_str())
                .split(|&b| b == b':')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_vec())
                .collect()
        })
        .unwrap_or_default();
    let run_result = match run_kati(
        &targets,
        &cl_vars,
        &makefile,
        &shadow_cwd,
        &seed_env,
        recipe_stdin,
        None,
        // Standalone recipes can write straight to the process descriptors.
        // Target logging is an explicit request for per-edge capture, so keep
        // the captured path authoritative when it is enabled.
        std::env::var_os("BUMBA_TARGET_LOG_DIR").is_none(),
        &include_dirs,
        flags.no_builtin_rules,
        flags.no_builtin_variables,
        &cmdline_flags,
        parallel_advertisement.as_ref().map(|value| value.flags()),
        flags.is_dry_run,
        flags.is_ignore_errors,
        flags.is_trace,
        flags.is_always_make,
        flags.is_question,
        flags.is_touch,
        flags.num_jobs,
        flags.jobs_explicit,
        flags.is_keep_going,
        flags.is_silent_mode,
        &noremake,
    ) {
        Ok(r) => r,
        Err(e) => {
            // Recipe failure already printed its `*** [target] Error N`; just
            // surface the code (was std::process::exit(2) inside exec).
            if let Some(bf) = e.downcast_ref::<kati::exec::BuildFailed>() {
                return bf.0;
            }
            for cause in e.chain() {
                eprintln!("{cause}");
            }
            return 1;
        }
    };
    flush_makevars();
    if std::env::var_os("BUMBA_SCHED_STATS").is_some() {
        eprintln!(
            "bumba-recipe-stats literal_external={}",
            crate::shell::literal_external_launches().saturating_sub(literal_external_before),
        );
    }
    // GNU -q: silent probe — exit 1 when something would be rebuilt.
    if flags.is_question && run_result.would_run {
        return 1;
    }
    let code = 0;

    if run_result.remake_active && code == 0 {
        // Bumba: remake-the-makefile loop completed building the
        // generated includes. Re-exec the host binary with the same
        // argv so the second invocation parses the makefile with the
        // freshly-generated content visible (matches GNU make's
        // self-re-exec). Capped via BUMBA_KATI_REMAKE_DEPTH.
        let depth: u32 = std::env::var("BUMBA_KATI_REMAKE_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if depth >= 5 {
            eprintln!("*** kati: remake-the-makefile loop exceeded 5 iterations");
            return 2;
        }
        let argv_os: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let argv0 = argv_os.first().cloned().unwrap_or_default();
        let exe = std::env::current_exe().unwrap_or_else(|_| argv0.clone().into());
        let mut cmd = std::process::Command::new(&exe);
        std::os::unix::process::CommandExt::arg0(&mut cmd, &argv0);
        cmd.args(argv_os.iter().skip(1));
        cmd.env("BUMBA_KATI_REMAKE_DEPTH", (depth + 1).to_string());
        // Carry forward optional includes that failed to remake, unioned
        // with the ones we inherited, so the next pass skips them.
        if !run_result.failed_optional.is_empty() || !noremake.is_empty() {
            let mut all: Vec<Vec<u8>> = noremake.iter().cloned().collect();
            for f in &run_result.failed_optional {
                if !all.contains(f) {
                    all.push(f.clone());
                }
            }
            let joined = all.join(&b':');
            cmd.env("BUMBA_KATI_NOREMAKE", std::ffi::OsStr::from_bytes(&joined));
        }
        let err = std::os::unix::process::CommandExt::exec(&mut cmd);
        eprintln!("*** kati: failed to re-exec for remake: {err}");
        return 2;
    }
    code
}

/// In-process `make`/`gmake` brush builtin entry. Unlike `make_main` (the
/// shadow/process path), this runs make WITHOUT mutating process state: the
/// working dir comes from the brush ExecutionContext (resolved with -C, no
/// chdir) and recipe output is routed to the context's fd 1 — so a recursive
/// `$(MAKE)` in a recipe, or `make` invoked by a configure/cmake script, stays
/// in THIS process at the right directory instead of re-exec'ing the host.
///
/// `base_cwd` is the brush shell's logical cwd; `out`/`err` are its fd 1/2;
/// `recipe_out` is a second handle on fd 1 used as the recipe-output sink.
pub fn make_builtin(
    argv: &[String],
    base_cwd: &std::path::Path,
    // The brush subshell's exported env, captured by MakeBuiltin::execute. This
    // carries the PARENT make's exports (applied to the subshell via the recipe
    // prefix), so a recursive `$(MAKE)` inherits them WITHOUT any make ever
    // touching the shared process env. NOT std::env — concurrent in-process makes
    // would race on that.
    seed_env: &[(std::ffi::OsString, std::ffi::OsString)],
    mut out: impl std::io::Write,
    mut err: impl std::io::Write,
    recipe_out: brush_core::openfiles::OpenFile,
    recipe_err: brush_core::openfiles::OpenFile,
    // The builtin's logical stdin (fd 0), for `make -f -`.
    mut stdin: Option<brush_core::openfiles::OpenFile>,
    filesystem: Option<Arc<dyn kati::filesystem::FileSystemProvider>>,
) -> i32 {
    install_make_recipe_runner();

    let recipe_stdin = stdin
        .as_ref()
        .and_then(|input| input.try_borrow_as_fd().ok()?.try_clone_to_owned().ok())
        .map(std::sync::Arc::new);

    // make pseudo-actions handled before kati's flag parser (which panics on
    // unknown flags). --version short-circuits to the gnu-shaped banner.
    for a in argv.iter().skip(1) {
        if a == "--version" || a == "-v" {
            let _ = writeln!(out, "GNU Make 4.3");
            let _ = writeln!(out, "Built for {}-pc-linux-gnu", std::env::consts::ARCH);
            let _ = writeln!(
                out,
                "Copyright (C) 1988-2020 Free Software Foundation, Inc."
            );
            let _ = writeln!(
                out,
                "License GPLv3+: GNU GPL version 3 or later <http://gnu.org/licenses/gpl.html>"
            );
            let _ = writeln!(
                out,
                "This is free software: you are free to change and redistribute it."
            );
            let _ = writeln!(out, "There is NO WARRANTY, to the extent permitted by law.");
            return 0;
        }
        if a == "--help" || a == "-h" {
            let _ = write!(out, "{MAKE_HELP}");
            return 0;
        }
    }

    // An explicit -jN creates (or joins a configured host) jobserver for this
    // top-level build. Recursive sub-makes normally have no explicit -j and
    // inherit these words through their logical MAKEFLAGS instead, so sibling
    // recursion remains bounded by one shared pool.
    let jobserver_advertisement =
        crate::jobserver::explicit_jobs(argv).map(crate::jobserver::advertisement);

    let kargv = match kati_argv(argv) {
        Ok(v) => v,
        Err(msg) => {
            let _ = writeln!(err, "{msg}");
            return 2;
        }
    };
    let _ = kati::flags::install_args(kargv.clone());
    // This make's real inherited MAKEFLAGS rides seed_env (the subshell env
    // carrying the parent make's export prefix), not the shared host process
    // environment. Parse it BEFORE argv so this invocation's `obj=child`
    // overrides a propagated `obj=parent`, exactly as GNU make does.
    let inherited_makeflags = seed_env
        .iter()
        .find(|(k, _)| k == "MAKEFLAGS")
        .map(|(_, value)| value.as_bytes());
    let flags = kati::flags::Flags::from_args_with_makeflags(kargv, inherited_makeflags);

    // Resolve -C against the context cwd (NO process chdir). flags.working_dir
    // is kati's parsed -C value.
    let mut working_dir = base_cwd.to_path_buf();
    if let Some(c) = &flags.working_dir {
        let p = std::path::Path::new(c);
        working_dir = if p.is_absolute() {
            p.to_path_buf()
        } else {
            working_dir.join(p)
        };
    }

    // Native filesystem access is the standalone default. Embedders can scope
    // an alternate read capability to this make invocation.
    let _filesystem = filesystem.map(kati::filesystem::install_file_system_provider);

    // Makefile: explicit -f, else discover GNUmakefile/makefile/Makefile in the
    // working dir. Stored as the name kati interns (relative); the fs read
    // resolves it against working_dir (Evaluator.working_dir / file_cache).
    let makefile: OsString = match flags.makefile.lock().clone() {
        // `-f -`: the makefile arrives on the builtin's LOGICAL stdin.
        Some(m) if m == "-" => {
            let spooled = match stdin.as_mut() {
                Some(inp) => spool_stdin_makefile(inp),
                None => Err(std::io::Error::other("no stdin available")),
            };
            match spooled {
                Ok(p) => p,
                Err(e) => {
                    let _ = writeln!(
                        err,
                        "bumba make: cannot read makefile from stdin: {e}"
                    );
                    return 2;
                }
            }
        }
        Some(m) => m,
        None => {
            let mut found = None;
            for cand in ["GNUmakefile", "makefile", "Makefile"] {
                if kati::filesystem::metadata(working_dir.join(cand)).is_ok() {
                    found = Some(OsString::from(cand));
                    break;
                }
            }
            match found {
                Some(m) => m,
                None => {
                    let _ = writeln!(
                        err,
                        "bumba make: no makefile found (and none given with -f)"
                    );
                    return 2;
                }
            }
        }
    };
    if kati::filesystem::metadata(working_dir.join(&makefile)).is_err() {
        let display = makefile.to_string_lossy();
        let _ = writeln!(err, "make: {display}: No such file or directory");
        let _ = writeln!(err, "make: *** No rule to make target '{display}'.");
        return 2;
    }

    // Route recipe stdout to the context's fd 1 and recipes' cwd to working_dir
    // for the duration of THIS make; save/restore so a nested recursive $(MAKE)
    // (which lands here again, on its own brush worker thread) nests cleanly.
    let recipe_execution_context: kati::fileutil::RecipeExecutionContext = Arc::new(
        crate::shell::LogicalRecipeIo {
            stdout: recipe_out.clone(),
            stderr: recipe_err.clone(),
        },
    );
    let prev_out = kati::exec::set_recipe_out(Some(Box::new(recipe_out)));
    let prev_err = kati::exec::set_recipe_err(Some(Box::new(recipe_err)));
    let prev_cwd = crate::shell::set_recipe_cwd(Some(working_dir.clone()));

    let targets: Vec<Symbol> = flags.targets.clone();
    let cl_vars: Vec<bytes::Bytes> = flags.cl_vars.clone();
    let include_dirs: Vec<OsString> = flags.include_dirs.clone();

    // Each `$(MAKE)` is logically a fresh make PROCESS — it must see the current
    // filesystem, not a snapshot another make took earlier. But unlike the
    // standalone rkati binary (one OS process per make), every in-process make
    // in a host shares ONE process and ONE set of process-global caches: the glob
    // cache (kati::fileutil) and the parsed-makefile cache (kati::file_cache).
    // Those caches outlive each make invocation, so a stale entry leaks across
    // makes. Concretely: `make defconfig` runs before `.config` exists, so the
    // top makefile's `-include .config` globs it as ABSENT and caches that; the
    // later build (and its per-directory sub-makes) then read the stale "missing"
    // and every `obj-$(CONFIG_*)` collapses to empty → empty lib.a archives →
    // link fails with hundreds of undefined `*_main` symbols. (busybox; the
    // failure is deterministic at -j1 and intermittent under -j as the shared
    // caches also race between concurrent sub-makes.) Drop both at entry so this
    // make starts from a clean, current view — matching GNU make's per-process
    // filesystem caching.
    kati::file_cache::clear();
    kati::fileutil::clear_glob_cache();

    // GNU make's remake-the-makefile loop, IN-PROCESS. run_kati builds any
    // required `include` targets that didn't exist at parse time and reports
    // remake_active; the multicall path re-enters Make to parse with the
    // generated content visible, but a builtin can't re-exec the brush process.
    // Instead we drop the makefile cache (so the regenerated include is re-read)
    // and re-run kati, up to a small cap — matching BUMBA_KATI_REMAKE_DEPTH.
    let cmdline_flags = extract_long_flags(argv);
    let mut noremake: std::collections::HashSet<Vec<u8>> = Default::default();
    let mut result = run_kati(
        &targets,
        &cl_vars,
        &makefile,
        &working_dir,
        seed_env,
        recipe_stdin.clone(),
        Some(recipe_execution_context.clone()),
        false,
        &include_dirs,
        flags.no_builtin_rules,
        flags.no_builtin_variables,
        &cmdline_flags,
        jobserver_advertisement.as_ref().map(|value| value.flags()),
        flags.is_dry_run,
        flags.is_ignore_errors,
        flags.is_trace,
        flags.is_always_make,
        flags.is_question,
        flags.is_touch,
        flags.num_jobs,
        flags.jobs_explicit,
        flags.is_keep_going,
        flags.is_silent_mode,
        &noremake,
    );
    let mut remake_depth = 0u32;
    while matches!(&result, Ok(r) if r.remake_active) && remake_depth < 5 {
        remake_depth += 1;
        if let Ok(r) = &result {
            // Optional includes that failed to remake are skipped on the
            // re-run instead of re-queued forever.
            noremake.extend(r.failed_optional.iter().cloned());
        }
        // Drop BOTH caches: the makefile cache (so the regenerated include is
        // re-parsed) AND the glob cache (eval_include probes existence via
        // glob(); the first parse cached the missing include as absent, which
        // would otherwise make the re-parse believe it's still missing and loop
        // forever).
        kati::file_cache::clear();
        kati::fileutil::clear_glob_cache();
        result = run_kati(
            &targets,
            &cl_vars,
            &makefile,
            &working_dir,
            seed_env,
            recipe_stdin.clone(),
            Some(recipe_execution_context.clone()),
            false,
            &include_dirs,
            flags.no_builtin_rules,
            flags.no_builtin_variables,
            &cmdline_flags,
            jobserver_advertisement.as_ref().map(|value| value.flags()),
            flags.is_dry_run,
            flags.is_ignore_errors,
            flags.is_trace,
            flags.is_always_make,
            flags.is_question,
            flags.is_touch,
            flags.num_jobs,
            flags.jobs_explicit,
            flags.is_keep_going,
            flags.is_silent_mode,
            &noremake,
        );
    }

    kati::exec::set_recipe_out(prev_out);
    kati::exec::set_recipe_err(prev_err);
    crate::shell::set_recipe_cwd(prev_cwd);
    flush_makevars();

    match result {
        Ok(r) => {
            if r.remake_active {
                let _ = writeln!(
                    err,
                    "*** kati: remake-the-makefile loop exceeded 5 iterations"
                );
                return 2;
            }
            let _ = out.flush();
            // GNU -q: silent probe — exit 1 when something would be rebuilt.
            if flags.is_question && r.would_run {
                1
            } else {
                0
            }
        }
        Err(e) => {
            // A recipe failure already emitted its `*** [target] Error N` line
            // (routed to fd 2); just surface the exit code, don't re-print.
            if let Some(bf) = e.downcast_ref::<kati::exec::BuildFailed>() {
                return bf.0;
            }
            for cause in e.chain() {
                let _ = writeln!(err, "{cause}");
            }
            1
        }
    }
}
