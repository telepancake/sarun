// The embedded brush shell (D9). When a box is launched with `-b`, the box's
// shell is brush-core/brush-parser running IN-PROCESS in the --inner shim, not
// /bin/sh. This is an EXPLICIT toggle: a construct brush cannot run is a
// VISIBLE error and a non-zero exit — never a silent downgrade to /bin/sh.
//
// What this buys (per D9): brush is what runs the box's top-level command, so
// the sh-storm a build's per-recipe `sh -c` would otherwise fork+exec is run
// in-process instead, AND brush emits SEMANTIC-PROVENANCE that raw FUSE can't
// recover — for each command it runs, the exact command string plus its
// parsed pipeline/redirect structure (a real step above pid+argv).
//
// Capture: brush and every binary it forks/execs (cc, ld, tr, …) inherit this
// process's fd 1/2, which we point at the box's FUSE sink files BEFORE building
// the shell. So all of their writes flow through the overlay and are recorded,
// exactly like ordinary capture mode — brush sits ABOVE FUSE, it does not
// replace it.
//
// BUILTIN STDOUT IS CAPTURED — VERIFIED (so we use the BashMode builtin set:
// echo/printf/test/[/let/source/declare etc. all run in-process). brush-core's
// `OpenFile::Stdout(std::io::Stdout)` write() does `f.write(buf)` to the real
// fd 1 (openfiles.rs), and we dup2 the box's FUSE sink onto fd 1 BEFORE building
// the shell — so a builtin writing to stdout writes to fd 1 == the sink == it is
// captured, exactly like a forked binary. There is NO capture reason to keep
// echo/printf/test external; the prior "ShMode preserves capture" claim was
// FALSE. (When a builtin's stdout is redirected — a pipe or `> file` — brush
// hands it a different OpenFile, also fd-backed and captured.)
//
// IN-PROCESS COREUTILS: on top of the BashMode shell builtins we fold in
// `brush_coreutils_builtins::bundled_commands()` (uutils/coreutils uumain
// adapters) as brush builtins — so `cat ls cp mv rm mkdir wc sort cut tr …` run
// IN-PROCESS too (no fork of /usr/bin/cat). Shell builtins WIN for overlapping
// names (we install coreutils FIRST, then let the BashMode set overwrite the
// overlaps), so brush's own `test`/`[`/`echo`/`printf`/`pwd`/`kill` stay shell
// builtins. A uutils uumain writes the PROCESS's real fd 1/2 (it ignores brush's
// in-memory OpenFiles), so the wrapper dup2's the context's stdout/stderr fd
// onto 1/2 around the call — see CoreutilWrapper below. shell_builder_common()
// is the one place all three shells (top-level box, nested sh, nested bash) get
// their builtins, so capture/coreutils/builtin-set policy lives in exactly one
// spot.
//
// brush↔PROCESS LINKAGE (D9, DONE — see capture.rs):
//   Every command brush fork/execs is a child of THIS --inner process (the brush
//   shell), so in the process FOREST every pipeline process's parent_id chain
//   passes through the brush --inner row. We exploit that for a faithful link:
//     • brush emits one FRAME_PROV per pipeline, IN EXECUTION ORDER, immediately
//       before running that pipeline (run_brush runs complete-commands one at a
//       time on the same persistent shell), carrying a 0-based `seq`.
//     • the engine inserts a brushprov row, then marks it as the box's CURRENT
//       pipeline; any process recorded while it is current whose ancestry reaches
//       the brush --inner row is stamped process.brush_pipeline_id = that row.
//   How the link is made — EXACT, race-free, semantic: each FRAME_PROV carries
//   the pipeline's literal WRITE-redirect TARGET paths (`> file`, `>>`, `&>`).
//   At box teardown the engine resolves each target file's LAST writer process
//   row and stamps it with that pipeline's brushprov id (guarded so it really is
//   a brush descendant in the forest). A pipeline's output file is written by
//   exactly that pipeline's process, so this needs NO clock/timing comparison —
//   which matters because a process row is only materialized at file *close*
//   (an async FUSE release), long after and out of order with its pipeline, so a
//   time-window scheme could not separate sub-jiffy-apart pipelines. Pipelines
//   that produce no write-redirect target are still recognizable as brush
//   children by forest ancestry but are not stamped to a SPECIFIC pipeline (the
//   per-pipeline column stays NULL for them). Two further linkage limitations,
//   stated honestly: (a) only LITERAL output targets link — a redirect target
//   needing expansion (`> $OUT`, `> a/*.o`, `> $(cmd)`) is skipped, since the
//   engine resolves the path offline, not in brush's expansion context; (b) the
//   target is matched as a box-ABSOLUTE path (`/root/x`), so a RELATIVE redirect
//   (`> out.txt`) — whose sqlar name is the cwd-resolved path — does not link in
//   this cut. Both are documented gaps, not silent mislinks.
//   Readers: discover::{proc_pipeline, pipeline_procs, brushprov(.processes)}.
//
// NESTED-shell EXECUTION (D9 follow-on — see brush_sh below):
// For -b brush boxes the runner shadows /bin/sh, /usr/bin/sh, /bin/bash and
// /usr/bin/bash with the engine binary itself. When any tool inside the box
// exec's `sh -c RECIPE` (make recipes, libc `system()`, configure scripts, …)
// it lands in `brush_sh`, which RUNS the recipe THROUGH embedded brush-core —
// not the real /bin/sh. There is NO real-shell fallback: D9's explicit-toggle
// rule applies — anything brush cannot parse or execute is a VISIBLE error
// (stderr message + non-zero exit), identical to how the top-level brush body
// already behaves. brush is NOT bash: bash-specific syntax (the constructs
// brush-core does not implement) fails here, by design.
//
// Each nested invocation parses the script, emits one `brush_prov_nested`
// record per pipeline over a fresh engine connection acquired via the FD
// broker (SARUN_BROKER — see runner::send_nested_prov; the box is resolved
// from the shim's pidfd /proc ancestry — the same path `register` uses for
// nested boxes), then runs the pipelines
// pipeline-by-pipeline on a fresh brush sh-mode shell built with the original
// invocation's cwd, $0 (the -c form's NAME or argv[0]'s basename) and the
// positional parameters ($1..$N).
//
// Capture: the nested brush-sh shim INHERITS fd 1/2 from its caller (typically
// make, which itself inherited the box's --inner brush's sinks). brush-core
// writes through whatever fd 1/2 it inherits, so all of the recipe's output
// and writes still flow through the existing capture path — there is no
// re-redirection needed here (and we deliberately do NOT touch fd 1/2 again,
// because the top-level inner_brush already did the right thing once).
//
// PROCESS LINKAGE for nested pipelines: every process a nested brush-sh
// invocation forks is a descendant of the top-level brush --inner (the
// brush-sh shim itself is a descendant of `make`, which is a descendant of
// the --inner). So the existing forest-ancestry guard in finalize_brush_links
// (capture.rs) accepts them too. We extend the engine to feed the nested
// pipelines' out_targets into the same brush_links bucket: a nested pipeline's
// literal `> file` writer gets stamped with the NESTED brushprov row's id,
// while the top-level pipeline that ran `make` keeps its own (typically
// targetless) row. Two pipelines never compete for the same literal target
// because each file is written by exactly one pipeline.
//
// PARSER MODE BY INVOCATION NAME (B): the nested shim `brush_sh` is reached as
// `sh`, `bash`, or `dash` (argv0 basename, see is_brush_sh_invocation). When it
// is `bash` we build the shell in BASH mode (sh_mode(false)) so bashisms work —
// `[[ … ]]` extended test, process substitution `<(…)/>(…)`, arrays,
// `${x//a/b}`, etc. — matching real /bin/bash. When it is `sh`/`dash` we keep
// sh_mode(true): faithful POSIX, where `[[` is just a command name and (absent a
// `[[` binary) fails visibly. The top-level box brush (run_brush/inner_brush)
// stands in for the box's default /bin/sh, so it stays sh_mode(true). Mode is the
// ONLY difference between the three; builtins come from shell_builder_common().
//
// Brush-core coverage (VERIFIED): in BOTH modes brush-core runs POSIX builtins
// (cd, export, set, [, test, printf, echo, shift, …), variable assignment +
// expansion, arithmetic, if/case/for/while/until control flow, functions, simple
// traps, here-docs/here-strings and the standard one-char flag set (-e/-u/-x/-o,
// set/unset of same). Bash-only constructs (`[[ … ]]`, `<(…)/>(…)`, arrays, …)
// work in BASH mode and surface as visible parse/exec errors in sh-mode — which
// is exactly the /bin/sh-vs-/bin/bash contract.

use std::os::fd::AsRawFd;

use serde_json::json;
use serde_json::Value;

// ── shared shell builder (A + C) ─────────────────────────────────────────────
// All three brush shells (top-level box, nested sh, nested bash) install the
// SAME builtin policy. The ONLY per-call difference is the parser mode, passed
// as `sh_mode`. This is the single place capture/coreutils/builtin-set decisions
// live (see the module header).

use std::ffi::OsString;

/// A uutils coreutil (`fn(Vec<OsString>) -> i32`) wrapped as a brush-core
/// `SimpleCommand` so it runs IN-PROCESS as a brush builtin.
///
/// THE CORRECTNESS TRAP: a uutils `uumain` writes the PROCESS's real fd 1/2 —
/// it ignores brush's in-memory `OpenFiles`. So `execute` inspects the brush
/// `context`'s stdout/stderr OpenFile and, if it is backed by a real fd that is
/// NOT already 1/2 (a pipe stage, a `> file` redirect, …), dup2's that fd onto
/// 1/2 for the duration of the call, then restores the saved 1/2. If stdout is
/// the plain `Stdout` (fd 1 already → the FUSE sink) we call directly. If the
/// OpenFile is an in-memory `Stream` with no raw fd (`try_borrow_as_fd` errors)
/// uutils CANNOT be pointed at it, so we error visibly rather than silently
/// write to the wrong fd — that path does not arise for box capture (sinks and
/// pipes are all real fds) but we refuse it loudly if it ever does.
struct CoreutilWrapper;

impl CoreutilWrapper {
    /// Look up the coreutil fn for `name` (the registered builtin name) from the
    /// bundled table. Built once per call; the table is tiny (<100 fn ptrs).
    fn lookup(name: &str) -> Option<fn(Vec<OsString>) -> i32> {
        brush_coreutils_builtins::bundled_commands().get(name).copied()
    }
}

/// Redirect `target_fd` (1 or 2) to the raw fd backing `of` for the duration of
/// `run`, restoring the original afterwards. Returns the run's value. If `of`
/// already maps to `target_fd` (the common Stdout/Stderr → sink case) nothing is
/// dup'd. Returns Err with a visible message if `of` has no usable raw fd.
fn with_fd_redirected<R>(
    target_fd: i32,
    of: &brush_core::openfiles::OpenFile,
    run: impl FnOnce() -> R,
) -> Result<R, String> {
    // The raw fd brush would have uutils write to. Stream has none.
    let borrowed = of.try_borrow_as_fd().map_err(|_| {
        format!("coreutil output fd {target_fd} is an in-memory stream with no \
                 raw fd; cannot run uutils against it")
    })?;
    let src = borrowed.as_raw_fd();
    if src == target_fd {
        // Already the right fd (e.g. Stdout == fd 1 == the FUSE sink). No dup.
        return Ok(run());
    }
    // Save the current target fd, point it at `src`, run, restore.
    let saved = unsafe { libc::dup(target_fd) };
    if saved < 0 {
        return Err(format!("coreutil: dup(save fd {target_fd}) failed"));
    }
    if unsafe { libc::dup2(src, target_fd) } < 0 {
        unsafe { libc::close(saved); }
        return Err(format!("coreutil: dup2 onto fd {target_fd} failed"));
    }
    let r = run();
    unsafe {
        // Best-effort restore; flush is the uutils adapter's job (it does).
        libc::dup2(saved, target_fd);
        libc::close(saved);
    }
    Ok(r)
}

/// Shell out to the host binary for a coreutil — when its gate refuses the argv,
/// or when it is a separately-spawned (concurrent) pipeline stage where the
/// in-process uumain path's process-wide fd redirect would race the sibling
/// spawns.
///
/// We delegate to brush's OWN external-command composer, `compose_std_command`,
/// so a forked coreutil inherits the shell state EXACTLY like any other external
/// brush runs — there is one definition of "how a child sees the shell" and we
/// reuse it instead of re-deriving it piecemeal. That single function already
/// applies: the shell's LOGICAL cwd and exported environment (brush keeps both
/// logical — it never chdir's the process or mutates its env), std fd 0/1/2 plus
/// the command's injected fd>=3 redirections, argv[0], and the stray-fd close
/// hook. It is all child-local (no process-wide dup2), so it is safe on the
/// spawn_blocking pipeline worker, and it is the same battle-tested path every
/// external pipeline stage (cc, sed, …) already takes. `args` is argv WITHOUT
/// the leading command name (compose adds argv[0] itself). Returns the child's
/// exit code (low 8 bits); a compose or spawn failure returns 127.
fn run_coreutil_external<SE: brush_core::extensions::ShellExtensions>(
    context: &brush_core::commands::ExecutionContext<'_, SE>,
    name: &str,
    args: &[OsString],
) -> i32 {
    let mut cmd = match brush_core::commands::compose_std_command(
        context, name, name, args, /* empty_env = */ false,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("/bin/sh: line 1: {name}: {e}");
            return 127;
        }
    };
    match cmd.status() {
        Ok(s) => s.code().unwrap_or(127),
        Err(e) => {
            eprintln!("/bin/sh: line 1: {name}: command not found ({e})");
            127
        }
    }
}

impl brush_core::builtins::SimpleCommand for CoreutilWrapper {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: bundled uutils/coreutils builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        let Some(func) = CoreutilWrapper::lookup(&name) else {
            // Should not happen: we only register names present in the table.
            eprintln!("sarun-engine brush: coreutil '{name}' not in bundled table");
            return Ok(brush_core::results::ExecutionResult::new(127));
        };
        // argv as OsString. brush passes `args` INCLUDING the command name as
        // the first element (CommandArg "including the command itself" — see
        // brush_core::commands::ExecutionContext::args), which is exactly the
        // argv[0]=util-name uutils expects, so we do NOT prepend `name` again.
        // Defensive: if brush ever hands an empty list, seed argv[0]=name.
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        if argv.is_empty() { argv.push(OsString::from(&name)); }

        // sarun: per-util GATE. The bundled uutils diverges from GNU
        // coreutils on flags, locale-sensitive output, and a tail of
        // corner cases. crate::brush_gates::gate_for(name) returns a
        // per-util predicate: true means uutils faithfully handles
        // THIS specific argv → call uumain in-process; false (default)
        // → fall back to fork+exec of the host binary, which IS the
        // GNU implementation. Most utils start at gate_false until an
        // agent audits them and writes a tighter gate (see
        // brush_gates.rs's module-level docs).
        let gate = crate::brush_gates::gate_for(&name);

        // sarun: CONCURRENT-PIPELINE-STAGE guard. The in-process uumain path below
        // points the PROCESS's real fd 0/1/2 at the brush OpenFiles via
        // with_fd_redirected (a process-wide dup2). That is safe for a stand-alone
        // command, but NOT when this coreutil is a separately-spawned pipeline
        // stage: brush runs it on a tokio spawn_blocking worker CONCURRENTLY with
        // spawning the sibling stages, so a transient fd redirect leaks into a
        // just-forked sibling (the configure/make hang; see run_coreutil_external).
        // brush tells us this directly via context.spawned_pipeline_stage (true
        // only on the OwnedShell + spawn_blocking path) — the RIGHT signal, vs.
        // guessing from fd type, which would wrongly fire on a stand-alone command
        // that merely inherited a pipe as its stdout (e.g. the whole box piped into
        // another process). A concurrent stage takes the external host binary,
        // which wires the child's fds without mutating this process's fd table. We
        // OR with the gate so a pipeline stage shells out even if its gate is true.
        if context.spawned_pipeline_stage || !gate(&argv) {
            // compose_std_command adds argv[0] itself, so pass argv WITHOUT the
            // leading command name; it derives cwd/env/fds from the context.
            let code = run_coreutil_external(&context, &name, &argv[1..]);
            return Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8));
        }

        // In-process uumain path (gate said true AND not a concurrent pipeline
        // stage). Resolve the brush context's stdin/stdout/stderr OpenFiles so we
        // can point the PROCESS's real fd 0/1/2 at them around the uumain call — a
        // uutils uumain reads/writes the real fds, ignoring brush's OpenFiles.
        // Where the OpenFile already maps to the same fd (the common
        // Stdout==1 / Stderr==2 / Stdin==0 sink case) with_fd_redirected is a
        // no-op; where it is a File (`> file`) it dup2's around the call. This
        // path only handles 0/1/2 (uutils uses no other fds); pipe stages and any
        // exec-3<… cases already went external above with full fd inheritance.
        let in_ = context.try_fd(0);
        let out = context.try_fd(1);
        let err = context.try_fd(2);

        // Redirect fd N to `of` (if present) around `run`; map a redirect failure
        // to a visible message + exit 1. Returns the uumain exit code.
        fn step(
            fd: i32,
            of: &Option<brush_core::openfiles::OpenFile>,
            run: impl FnOnce() -> i32,
        ) -> i32 {
            match of {
                Some(of) => match with_fd_redirected(fd, of, run) {
                    Ok(c) => c,
                    Err(msg) => { eprintln!("sarun-engine brush: {msg}"); 1 }
                },
                // No entry for this fd → leave the process fd as-is.
                None => run(),
            }
        }

        // Nest fd 2 (outer) → fd 1 → fd 0 (inner) → uumain.
        let code = step(2, &err, || {
            step(1, &out, || {
                step(0, &in_, || func(argv))
            })
        });
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// `sarun` / `oaita` as in-box brush builtins (interactive use). There is no
/// `sarun` on PATH inside a box and no FUSE shadow under /usr/local: the
/// builtin re-execs the box inner runner process's OWN executable — the engine
/// binary, reachable as /proc/self/exe — so typing `sarun …` / `oaita …` in a
/// brush box shell works with nothing planted in the box filesystem. `oaita`
/// runs the engine's `oaita` subcommand. The child's std fds are wired from the
/// brush context's OpenFiles (the box's captured sinks, or a pipe/redirect),
/// child-local (no process-wide dup2), so it is pipeline-safe.
struct EngineSelfCommand;

impl EngineSelfCommand {
    /// A `Stdio` duplicating the raw fd backing `of`, or None to inherit this
    /// process's fd (the common Stdout/Stderr → sink case). The dup is owned by
    /// the returned Stdio and closed when the child spawn consumes it.
    fn stdio_from(of: &Option<brush_core::openfiles::OpenFile>) -> Option<std::process::Stdio> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let of = of.as_ref()?;
        let bf = of.try_borrow_as_fd().ok()?;
        let dup = unsafe { libc::dup(bf.as_raw_fd()) };
        if dup < 0 { return None; }
        Some(unsafe { std::process::Stdio::from(OwnedFd::from_raw_fd(dup)) })
    }
}

impl brush_core::builtins::SimpleCommand for EngineSelfCommand {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: sarun in-box engine builtin\n"))
    }

    fn execute<SE: brush_core::extensions::ShellExtensions,
               I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let name = context.command_name.clone();
        // `args` includes the command name as argv[0]; drop it and rebuild the
        // engine argv. `oaita` becomes the engine's `oaita` subcommand.
        let mut argv: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();
        let rest = if argv.is_empty() { vec![] } else { argv.split_off(1) };
        let mut eargs: Vec<OsString> = Vec::new();
        if name == "oaita" { eargs.push(OsString::from("oaita")); }
        eargs.extend(rest);

        let mut cmd = std::process::Command::new("/proc/self/exe");
        cmd.args(&eargs);
        if let Some(s) = Self::stdio_from(&context.try_fd(0)) { cmd.stdin(s); }
        if let Some(s) = Self::stdio_from(&context.try_fd(1)) { cmd.stdout(s); }
        if let Some(s) = Self::stdio_from(&context.try_fd(2)) { cmd.stderr(s); }
        let code = match cmd.status() {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => { eprintln!("{name}: cannot exec engine: {e}"); 127 }
        };
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

/// The set of builtin registrations every box brush shell installs: bundled
/// coreutils FIRST, then the BashMode shell builtins OVERWRITE any overlapping
/// names (so brush's own echo/printf/test/[/pwd/kill/etc. win — they must stay
/// shell builtins). Coreutils fills in the file utilities brush has no builtin
/// for (cat ls cp mv rm mkdir wc sort cut tr od stat du df …). Finally
/// `sarun`/`oaita` are added so they resolve in-box without a PATH shadow.
fn box_builtins<SE: brush_core::extensions::ShellExtensions>()
    -> std::collections::HashMap<String, brush_core::builtins::Registration<SE>> {
    box_builtins_opt(true)
}

/// Same as [`box_builtins`] but lets the caller exclude bundled uutils
/// coreutils. The make-recipe path passes `bundle_coreutils=false` so
/// `cp`/`mkdir`/`ls`/etc. fall through to fork+exec of the host binary.
/// Reason: uutils localization caches each util's FluentResource in a
/// process-global `OnceLock` (see uucore::locale::UTIL_FLUENT); the
/// FIRST util to run on the engine wins that slot for the process's
/// lifetime, and every subsequent util's `translate!()` lookup against
/// keys missing from the cached bundle returns the raw key (e.g.
/// `cp-error-cannot-stat`). For interactive boxes (Phase 1, n2 ninja
/// recipes) we keep bundled coreutils for in-process speed; for make
/// recipes we trade speed for bash-compatible output. Standalone rkati
/// has no in-process shell so this divergence didn't exist there.
fn box_builtins_opt<SE: brush_core::extensions::ShellExtensions>(
    bundle_coreutils: bool,
) -> std::collections::HashMap<String, brush_core::builtins::Registration<SE>> {
    use brush_core::builtins::{builtin, simple_builtin};
    let mut m: std::collections::HashMap<String, brush_core::builtins::Registration<SE>>
        = std::collections::HashMap::new();
    if bundle_coreutils {
        // Coreutils first (lowest priority).
        for name in brush_coreutils_builtins::bundled_commands().keys() {
            m.insert(name.clone(), simple_builtin::<CoreutilWrapper, SE>());
        }
    }
    // BashMode shell builtins overwrite overlaps (highest priority).
    m.extend(brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode));
    // In-box engine entry points (no PATH shadow): `sarun` / `oaita` re-exec
    // the inner runner's own binary via /proc/self/exe.
    m.insert("sarun".to_string(), simple_builtin::<EngineSelfCommand, SE>());
    m.insert("oaita".to_string(), simple_builtin::<EngineSelfCommand, SE>());
    // In-process `find` (vendored find-only findutils fork). It is neither a
    // bundled coreutil nor a bash builtin, so it is always present and never
    // overwritten — registered regardless of `bundle_coreutils` (find does not
    // share the uucore Fluent localization cache that gated coreutils for make
    // recipes). It runs against the shell's logical I/O and logical cwd; see
    // crate::find_builtin.
    m.insert(
        "find".to_string(),
        simple_builtin::<crate::find_builtin::FindBuiltin, SE>(),
    );
    // In-process `xargs` (same vendored findutils fork). Like `find`, it is
    // neither a bundled coreutil nor a bash builtin, so it is always present and
    // registered regardless of `bundle_coreutils`. It reads the shell's logical
    // stdin, writes its logical stdout/stderr, and spawns its child commands in
    // the shell's logical cwd; see crate::xargs_builtin.
    m.insert(
        "xargs".to_string(),
        builtin::<crate::xargs_builtin::XargsBuiltin, SE>(),
    );
    // In-process exec-wrapper builtins (`env`, `printenv`). These are launcher
    // front-ends, not ports: they mutate the shell's LOGICAL launch state
    // (environment, cwd) on a cloned subshell and dispatch the residual command
    // through brush itself — so `env FOO=bar find .` runs `find` as the
    // in-process builtin with the modified logical state, no OS process needed.
    // Registered after the BashMode extend so they win over the uutils coreutil
    // wrappers of the same name; see crate::exec_wrappers.
    m.insert(
        "env".to_string(),
        builtin::<crate::exec_wrappers::EnvCommand, SE>(),
    );
    m.insert(
        "printenv".to_string(),
        builtin::<crate::exec_wrappers::PrintenvCommand, SE>(),
    );
    // Launch-state exec wrappers (`nice`/`setsid`/`nohup`): same clone-and-
    // dispatch mechanism, but they set a disposition only a real process can
    // carry (priority / session / SIGHUP) that materializes in the forked child
    // at exec; see crate::exec_wrappers and brush_core::LaunchState.
    m.insert(
        "nice".to_string(),
        builtin::<crate::exec_wrappers::NiceCommand, SE>(),
    );
    m.insert(
        "setsid".to_string(),
        builtin::<crate::exec_wrappers::SetsidCommand, SE>(),
    );
    m.insert(
        "nohup".to_string(),
        builtin::<crate::exec_wrappers::NohupCommand, SE>(),
    );
    m
}

/// Build a box brush shell with the shared builtin policy. `sh_mode == true`
/// → faithful POSIX (/bin/sh, /bin/dash, top-level box); `false` → BASH mode
/// (bashisms enabled, for a nested `bash -c`). `shell_name`/`positional`/`cwd`
/// are optional ($0 / $1.. / working dir); None keeps brush-core defaults.
async fn build_box_shell(
    sh_mode: bool,
    shell_name: Option<String>,
    positional: Option<Vec<String>>,
    cwd: Option<std::path::PathBuf>,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    build_box_shell_opt(sh_mode, shell_name, positional, cwd, false).await
}

/// Same as build_box_shell but lets the caller mark the shell as interactive.
/// brush-core's builder propagates that into enable_command_history +
/// enable_job_control; without it the shell.history is None and
/// brush-interactive's reedline hinter panics with HistoryFeatureUnsupported
/// on every keystroke. The non-interactive callers use the wrapper above.
async fn build_box_shell_opt(
    sh_mode: bool,
    shell_name: Option<String>,
    positional: Option<Vec<String>>,
    cwd: Option<std::path::PathBuf>,
    interactive: bool,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    build_box_shell_full(sh_mode, shell_name, positional, cwd, interactive, true).await
}

async fn build_box_shell_full(
    sh_mode: bool,
    shell_name: Option<String>,
    positional: Option<Vec<String>>,
    cwd: Option<std::path::PathBuf>,
    interactive: bool,
    bundle_coreutils: bool,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    // bon's builder is typestate-typed (each setter changes the type), so we
    // can't conditionally chain. shell_name/shell_args/working_dir are all
    // Option fields whose bon setter accepts the inner value — passing None's
    // natural default ("", [], $PWD) reproduces brush-core's own unset default,
    // so this is equivalent to omitting the setter.
    let cwd = cwd.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    });
    brush_core::Shell::builder()
        .sh_mode(sh_mode)
        .interactive(interactive)
        .builtins(box_builtins_opt(bundle_coreutils))
        .shell_name(shell_name.unwrap_or_default())
        .shell_args(positional.unwrap_or_default())
        .working_dir(cwd)
        .build().await
}

/// Point this process's fd 1 and 2 at the box's FUSE stdout/stderr sink files,
/// so brush's own output and every binary it forks inherit captured fds. Returns
/// false (visibly) if the sinks can't be opened.
fn redirect_stdio_to_sinks() -> bool {
    let out = std::fs::OpenOptions::new().write(true).open("/.slopbox-stdout");
    let err = std::fs::OpenOptions::new().write(true).open("/.slopbox-stderr");
    let (out, err) = match (out, err) {
        (Ok(o), Ok(e)) => (o, e),
        _ => {
            eprintln!("sarun-engine inner: -b capture sinks unavailable");
            return false;
        }
    };
    unsafe {
        if libc::dup2(out.as_raw_fd(), 1) < 0 { return false; }
        if libc::dup2(err.as_raw_fd(), 2) < 0 { return false; }
    }
    // `out`/`err` drop here; the dup'd fd 1/2 keep the sinks open.
    true
}

/// Decide the script brush should run from the box's argv. We honor the
/// /bin/sh contract at the top level: `sh -c SCRIPT [name [args…]]` (and the
/// `bash`/`dash` aliases) hands SCRIPT to brush; anything else is treated as a
/// single simple command and reconstructed into a command string brush parses.
/// (This is the top-level /bin/sh-resolution point — see the module header.)
fn script_from_argv(cmd: &[String]) -> String {
    let base = std::path::Path::new(&cmd[0])
        .file_name().and_then(|s| s.to_str()).unwrap_or(&cmd[0]);
    if matches!(base, "sh" | "bash" | "dash" | "brush") {
        if let Some(pos) = cmd.iter().position(|a| a == "-c") {
            if let Some(script) = cmd.get(pos + 1) {
                return script.clone();
            }
        }
    }
    // Reconstruct a command string from argv, quoting any word that needs it so
    // brush re-parses it as the SAME simple command (no shell-meta surprises).
    cmd.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ")
}

/// Minimal single-quote shell escaping (POSIX): wrap in '…', escaping embedded
/// single quotes as '\''. Bare alnum/safe words pass through unquoted.
fn shell_quote(w: &str) -> String {
    let safe = !w.is_empty() && w.chars().all(|c|
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '=' | ':' | '+'));
    if safe { return w.to_string(); }
    let mut s = String::from("'");
    for c in w.chars() {
        if c == '\'' { s.push_str("'\\''"); } else { s.push(c); }
    }
    s.push('\'');
    s
}

/// Walk the parsed program and build one provenance JSON object per top-level
/// pipeline: the exact command string brush parsed plus its real structure —
/// pipeline stage count, the `!`-negation flag, and per-stage redirect counts /
/// command words. This is the genuine semantic context brush has (D9), NOT a
/// Makefile line. We also include the FULL serde-serialized AST under "ast" so
/// nothing in the structure is lost.
/// The per-pipeline provenance records for ONE complete-command (CompoundList).
/// Used to emit FRAME_PROV immediately before brush runs that complete-command,
/// so the engine's `current_pipeline` window matches real execution order.
pub(crate) fn complete_command_records(complete: &brush_parser::ast::CompoundList) -> Vec<Value> {
    use brush_parser::ast;
    let mut out = vec![];
    // CompleteCommand = CompoundList = Vec<CompoundListItem(AndOrList, sep)>.
    for item in &complete.0 {
        let andor = &item.0;
        // The first pipeline plus any && / || continuations.
        let mut pipelines: Vec<&ast::Pipeline> = vec![&andor.first];
        for cont in &andor.additional {
            match cont {
                ast::AndOr::And(p) | ast::AndOr::Or(p) => pipelines.push(p),
            }
        }
        for pl in pipelines {
            let stages: Vec<Value> = pl.seq.iter().map(stage_record).collect();
            out.push(json!({
                "cmd": pl.to_string(),
                "bang": pl.bang,
                "stages": pl.seq.len(),
                "stage_detail": stages,
                // The literal WRITE-redirect target paths this pipeline opens for
                // output (`>`, `>>`, `>|`, `&>`). The engine uses these as the
                // EXACT, race-free brush↔process link: the process that last wrote
                // such a file IS this pipeline's process. Words requiring expansion
                // (vars/globs/`$()`) are skipped — they can't be resolved here.
                "out_targets": pipeline_out_targets(pl),
            }));
        }
    }
    out
}

/// The literal WRITE-redirect target filenames a pipeline opens for output
/// (across all its stages). Only un-expanded literal filenames (`> /a/b`) are
/// returned; a target needing expansion is skipped (can't be resolved offline).
fn pipeline_out_targets(pl: &brush_parser::ast::Pipeline) -> Vec<String> {
    use brush_parser::ast::Command;
    let mut out = vec![];
    for cmd in &pl.seq {
        if let Command::Simple(s) = cmd {
            if let Some(p) = &s.prefix { collect_out_targets(&p.0, &mut out); }
            if let Some(suf) = &s.suffix { collect_out_targets(&suf.0, &mut out); }
        }
    }
    out
}

fn collect_out_targets(items: &[brush_parser::ast::CommandPrefixOrSuffixItem],
                       out: &mut Vec<String>) {
    use brush_parser::ast::CommandPrefixOrSuffixItem as It;
    use brush_parser::ast::{IoRedirect, IoFileRedirectKind as K, IoFileRedirectTarget as T};
    for it in items {
        let It::IoRedirect(io) = it else { continue };
        match io {
            IoRedirect::File(_, kind, T::Filename(w)) => {
                if matches!(kind, K::Write | K::Append | K::Clobber | K::ReadAndWrite) {
                    if let Some(p) = literal_word(w) { out.push(p); }
                }
            }
            IoRedirect::OutputAndError(w, _) => {
                if let Some(p) = literal_word(w) { out.push(p); }
            }
            _ => {}
        }
    }
}

/// A redirect target word as a literal path IF it needs no expansion (no $ ` *
/// ? [ ~ ); else None. The Word's Display is the source text brush parsed.
fn literal_word(w: &brush_parser::ast::Word) -> Option<String> {
    let s = w.to_string();
    if s.is_empty() || s.chars().any(|c| matches!(c, '$' | '`' | '*' | '?' | '[' | '~')) {
        return None;
    }
    Some(s)
}

fn scan_items(items: &[brush_parser::ast::CommandPrefixOrSuffixItem],
              words: &mut Vec<String>, redirects: &mut usize) {
    use brush_parser::ast::CommandPrefixOrSuffixItem as It;
    for it in items {
        match it {
            It::IoRedirect(_) => *redirects += 1,
            It::Word(w) => words.push(w.to_string()),
            It::AssignmentWord(_, w) => words.push(w.to_string()),
            _ => {}
        }
    }
}

/// Per-pipeline-stage detail: the command words (for a simple command) and the
/// redirect count brush parsed for that stage.
fn stage_record(cmd: &brush_parser::ast::Command) -> Value {
    use brush_parser::ast;
    match cmd {
        ast::Command::Simple(s) => {
            let mut words: Vec<String> = vec![];
            let mut redirects = 0usize;
            if let Some(p) = &s.prefix { scan_items(&p.0, &mut words, &mut redirects); }
            if let Some(w) = &s.word_or_name { words.push(w.to_string()); }
            if let Some(suf) = &s.suffix { scan_items(&suf.0, &mut words, &mut redirects); }
            json!({"kind": "simple", "words": words, "redirects": redirects})
        }
        ast::Command::Compound(_, redirs) => json!({
            "kind": "compound",
            "redirects": redirs.as_ref().map(|r| r.0.len()).unwrap_or(0),
            "text": cmd.to_string(),
        }),
        ast::Command::Function(_) => json!({"kind": "function", "text": cmd.to_string()}),
        ast::Command::ExtendedTest(..) => json!({"kind": "extended_test",
                                                 "text": cmd.to_string()}),
    }
}

/// Send a FRAME_PROV frame carrying one provenance JSON object over the box
/// channel. Best-effort: a blocked/closed channel must not abort the box.
fn send_prov(conn_fd: i32, rec: &Value) {
    let payload = serde_json::to_vec(rec).unwrap_or_default();
    let frame = crate::frames::encode(crate::frames::FRAME_PROV, &payload);
    unsafe { libc::write(conn_fd, frame.as_ptr().cast(), frame.len()); }
}

/// The brush-shell box body. Returns the box's exit code. Errors are VISIBLE
/// (printed to the captured stderr) and yield a non-zero exit — never a silent
/// /bin/sh fallback.
pub fn inner_brush(conn_fd: i32, cmd: Vec<String>) -> i32 {
    // 1. Capture wiring: sinks onto fd 1/2 (brush + its children write captured),
    //    then MUTE our own pid so the echo readback isn't re-recorded, and spawn
    //    the ECHO reader that replays captured bytes to the REAL fd 1/2 for live
    //    upward visibility (same contract as inner_capture). We must save the
    //    real fd 1/2 first — those are the terminal we echo back to.
    let real_out = unsafe { libc::dup(1) };
    let real_err = unsafe { libc::dup(2) };
    if !redirect_stdio_to_sinks() {
        return 127;
    }
    // MUTE our host pid: writes by us are echoed (live) but not RE-recorded.
    let pidfd = crate::runner::pidfd_open_pub(std::process::id() as i32);
    if pidfd >= 0 {
        crate::runner::send_frame_pub(
            conn_fd, &crate::frames::encode(crate::frames::FRAME_MUTE, &[]), Some(pidfd));
        unsafe { libc::close(pidfd); }
    }
    // ECHO reader: captured bytes → the saved real fd 1/2 (live). Stops on
    // ECHO_DONE / channel close.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    let rfd = conn_fd;
    let reader = std::thread::spawn(move || {
        let mut buf: Vec<u8> = vec![];
        let mut tmp = [0u8; 65536];
        loop {
            // recvmsg, not plain read, so a FRAME_CONN's SCM_RIGHTS fd
            // reaches the FD broker. One fd per recvmsg; we associate it
            // with the first FRAME_CONN in this batch.
            let mut got_fd: Option<i32> = None;
            let n = crate::runner::recv_box_frame_bytes_pub(
                rfd, &mut tmp, &mut got_fd);
            if n <= 0 { break; }
            buf.extend_from_slice(&tmp[..n as usize]);
            let (frames, used) = crate::frames::decode(&buf);
            buf.drain(..used);
            for (ft, payload) in frames {
                if ft == crate::frames::FRAME_CONN {
                    if let Some(fd) = got_fd.take() {
                        crate::runner::runner_broker_handoff_pub(fd);
                    }
                    continue;
                }
                if ft == crate::frames::FRAME_ECHO && !payload.is_empty() {
                    let realfd = if payload[0] == 1 { real_err } else { real_out };
                    unsafe { libc::write(realfd, payload[1..].as_ptr().cast(),
                                         payload.len() - 1); }
                } else if ft == crate::frames::FRAME_ECHO_DONE {
                    done2.store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
            if let Some(fd) = got_fd { unsafe { libc::close(fd); } }
        }
    });

    // 2. Run the box command THROUGH embedded brush. tokio current-thread runtime
    //    (brush's execution is async). Build the shell, parse, emit provenance,
    //    execute. A parse error or an execution Error is surfaced VISIBLY.
    let script = script_from_argv(&cmd);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().build();
    let code = match rt {
        Ok(rt) => rt.block_on(run_brush(conn_fd, script)),
        Err(e) => { eprintln!("sarun-engine inner: -b runtime: {e}"); 127 }
    };

    // 3. Teardown: sinks (fd 1/2) closed at process exit; wait for the reader to
    //    drain the captured tail, then UNMUTE and let the channel close (EOF =
    //    engine teardown). Mirrors inner_capture.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    // Closing our sink fds (1/2) lets the engine flush ECHO_DONE. Restore them to
    // the saved terminal fds so a late eprintln still surfaces.
    unsafe { libc::dup2(real_out, 1); libc::dup2(real_err, 2); }
    while !done.load(std::sync::atomic::Ordering::SeqCst)
        && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    crate::runner::send_frame_pub(
        conn_fd, &crate::frames::encode(crate::frames::FRAME_UNMUTE, &[]), None);
    let _ = reader;
    code
}

// ── brush-sh shim (D9 follow-on: NESTED shell IS brush) ──────────────────────
// When a -b box runs, runner::run shadows the box's /bin/sh, /usr/bin/sh,
// /bin/bash, /usr/bin/bash with the ENGINE binary and sets SARUN_BRUSH_SH=1.
// When the box's TOP-LEVEL command — or, more interestingly, a NESTED tool
// like `make` or a libc `system()` — exec's `/bin/sh -c RECIPE`, it lands
// HERE, and brush-core RUNS that recipe. No real-shell fallback exists.

/// True when this engine invocation should act as the brush-sh shim: the
/// SARUN_BRUSH_SH env flag is set AND argv[0]'s basename is a shell name. main()
/// checks this BEFORE its normal subcommand dispatch.
pub fn is_brush_sh_invocation() -> bool {
    if std::env::var("SARUN_BRUSH_SH").as_deref() != Ok("1") {
        return false;
    }
    let arg0 = std::env::args().next().unwrap_or_default();
    let base = std::path::Path::new(&arg0)
        .file_name().and_then(|s| s.to_str()).unwrap_or("");
    matches!(base, "sh" | "bash" | "dash")
}

/// The brush-sh shim entrypoint. `argv` is the FULL process argv (argv[0] is
/// the shell name we were invoked as). Parses the `-c` form (or a script-file
/// form), emits one nested-provenance message to the engine, then runs the
/// script through embedded brush-core. NO real-shell fallback: a construct
/// brush cannot run is a VISIBLE error and a non-zero exit.
pub fn brush_sh(argv: &[String]) -> i32 {
    if argv.is_empty() {
        eprintln!("sarun-engine brush-sh: empty argv");
        return 2;
    }
    let arg0 = &argv[0];
    let base = std::path::Path::new(arg0)
        .file_name().and_then(|s| s.to_str()).unwrap_or("sh").to_string();
    // We DELIBERATELY do not touch fd 1/2 here. The shim was exec'd by the
    // box's caller (typically make / a libc system()), whose fd 1/2 are the
    // top-level inner_brush's box-FUSE sinks — every byte brush-core (and any
    // child it forks/execs) writes flows through that existing capture path.
    // Re-redirecting here would double-record and stamp writes against the
    // wrong process row. The top-level inner_brush owns capture; we don't.

    // Parse the leading short flags. brush-core honors -e/-u/-x and `-o NAME`
    // (via set after build); -i/-l/--login are interactive/login forms we
    // deliberately do NOT support inside a box — error visibly.
    let mut idx = 1;
    let mut set_flags: Vec<String> = vec![];   // e.g. ["-e","-u","-x"]
    let mut set_o: Vec<String> = vec![];       // names from `-o NAME`
    let mut unset_o: Vec<String> = vec![];     // names from `+o NAME`
    let mut have_c = false;
    let mut interactive = false;
    while idx < argv.len() {
        let a = &argv[idx];
        if a == "--" { idx += 1; break; }
        if a == "-c" { have_c = true; idx += 1; break; }
        if a == "-o" || a == "+o" {
            let Some(name) = argv.get(idx + 1) else {
                eprintln!("sarun-engine brush-sh: {a} requires an option name");
                return 2;
            };
            if a == "-o" { set_o.push(name.clone()); }
            else { unset_o.push(name.clone()); }
            idx += 2; continue;
        }
        // -i / --interactive: drop the box into the brush-interactive REPL
        // (reedline-based: history, multi-line edit, completion). -l/--login
        // is still out of scope — the brush-sh shim is never executed as a
        // login shell inside a box (no /etc/profile chain).
        if a == "-i" || a == "--interactive" {
            interactive = true;
            idx += 1; continue;
        }
        if a == "-l" || a == "--login" {
            eprintln!("sarun-engine brush-sh: {a} not supported inside a brush box");
            return 2;
        }
        // A grouped short-flag bundle like -eux. Anything starting with '-' or
        // '+' (not a lone "-" stdin marker) we treat as flags; "-" or anything
        // else means operands begin here.
        if a == "-" { break; }
        if let Some(rest) = a.strip_prefix('-') {
            // Each char must be a known POSIX-ish flag.
            for c in rest.chars() {
                match c {
                    'e' | 'u' | 'x' | 'v' | 'f' | 'n' | 'h' | 'm' | 'b' | 'C' | 'a' =>
                        set_flags.push(format!("-{c}")),
                    'c' => { have_c = true; }
                    _ => {
                        eprintln!("sarun-engine brush-sh: unsupported flag -{c}");
                        return 2;
                    }
                }
            }
            idx += 1;
            if have_c { break; }  // -c terminates flag parse
            continue;
        }
        if let Some(rest) = a.strip_prefix('+') {
            for c in rest.chars() {
                match c {
                    'e' | 'u' | 'x' | 'v' | 'f' | 'n' | 'h' | 'm' | 'b' | 'C' | 'a' =>
                        set_flags.push(format!("+{c}")),
                    _ => {
                        eprintln!("sarun-engine brush-sh: unsupported flag +{c}");
                        return 2;
                    }
                }
            }
            idx += 1; continue;
        }
        // First non-flag operand: stop flag parsing here.
        break;
    }

    // Interactive REPL form: `sh -i` (no -c, no script path). Hand the
    // whole loop to brush-interactive's reedline backend. Set flags
    // (-e / -u / -o NAME ...) are applied to the shell before the loop
    // starts, same as the non-interactive paths below.
    if interactive && have_c == false
        && idx >= argv.len()  // no script path follows the flags
    {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("sarun-engine brush-sh: runtime: {e}");
                return 127;
            }
        };
        let bash_mode = base == "bash";
        return rt.block_on(run_brush_interactive(
            base.clone(), set_flags, set_o, unset_o, bash_mode));
    }

    // Discriminate forms.
    let (script_src, dollar0, positional): (String, String, Vec<String>);
    if have_c {
        // `sh [-flags] -c SCRIPT [name [args...]]`
        let Some(s) = argv.get(idx).cloned() else {
            eprintln!("sarun-engine brush-sh: -c requires a SCRIPT argument");
            return 2;
        };
        idx += 1;
        let name = argv.get(idx).cloned().unwrap_or(base.clone());
        let args = if idx < argv.len() { argv[idx + 1..].to_vec() } else { vec![] };
        script_src = s;
        dollar0 = name;
        positional = args;
    } else if let Some(path) = argv.get(idx).cloned() {
        // `sh [-flags] SCRIPT [args...]` — read SCRIPT from disk.
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sarun-engine brush-sh: cannot read script {path}: {e}");
                return 127;
            }
        };
        script_src = s;
        dollar0 = path.clone();
        positional = argv[idx + 1..].to_vec();
    } else {
        // No -c and no script-file. We refuse to enter an interactive REPL
        // inside a box (out of scope here).
        eprintln!("sarun-engine brush-sh: requires -c SCRIPT or a script path \
                   (interactive nested shell is out of scope inside a brush box)");
        return 2;
    }

    // Run the recipe through brush-core. We surface execution + parse errors
    // visibly; the recipe's exit code becomes ours. Per-pipeline provenance
    // is emitted by run_brush_script BEFORE each pipeline runs (matching the
    // top-level run_brush execution-order contract).
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
    let rt = match rt {
        Ok(rt) => rt,
        Err(e) => { eprintln!("sarun-engine brush-sh: runtime: {e}"); return 127; }
    };
    // PARSER MODE BY INVOCATION NAME (B): invoked as `bash` → BASH mode (bashisms
    // on, matching /bin/bash); `sh`/`dash` → faithful POSIX sh_mode.
    let bash_mode = base == "bash";
    rt.block_on(run_brush_script(script_src, dollar0, positional,
                                  set_flags, set_o, unset_o, bash_mode))
}

/// Ship one `brush_prov_nested` control message carrying one pipeline's
/// records (with `nested:true`) + this process's pidfd to the engine. Used by
/// run_brush_script per-pipeline so the engine sees provenance IN EXECUTION
/// ORDER even when the same recipe contains multiple `;`-separated commands.
fn send_nested_pipeline_records(records: Vec<Value>) {
    if records.is_empty() { return; }
    let msg = json!({"type": "brush_prov_nested", "records": records});
    crate::runner::send_nested_prov(format!("{msg}\n").as_bytes());
}

/// Build a brush sh-mode shell with the right $0/positional/cwd, apply the
/// post-build set/+set flags, parse, and execute the script. Mirrors run_brush
/// (which serves the top-level -b body) — same parse/execute discipline, same
/// visible-failure rule.
async fn run_brush_script(script: String, shell_name: String,
                          positional: Vec<String>,
                          set_flags: Vec<String>, set_o: Vec<String>,
                          unset_o: Vec<String>, bash_mode: bool) -> i32 {
    // The shim INHERITS cwd from execve; brush-core defaults to $PWD/getcwd()
    // when working_dir is unspecified, which matches that. We still pass it
    // explicitly to be defensive against any future builder default change.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    // sh_mode == !bash_mode: `sh`/`dash` → POSIX, `bash` → bashisms (B).
    let shell_res = build_box_shell(!bash_mode, Some(shell_name.clone()),
                                    Some(positional.clone()), Some(cwd)).await;
    let mut shell = match shell_res {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: brush init failed: {e}");
            return 127;
        }
    };
    // Apply -e/-u/-x/-o NAME (etc.) by running an explicit `set` command
    // inside the shell. Failures here are visible — we never silently drop a
    // -e flag and let a failing recipe continue.
    if !set_flags.is_empty() || !set_o.is_empty() || !unset_o.is_empty() {
        let mut set_cmd = String::from("set");
        for f in &set_flags { set_cmd.push(' '); set_cmd.push_str(f); }
        for n in &set_o    { set_cmd.push_str(" -o "); set_cmd.push_str(n); }
        for n in &unset_o  { set_cmd.push_str(" +o "); set_cmd.push_str(n); }
        let src = brush_core::SourceInfo {
            source: "<brush-sh flags>".into(),
            start: None,
        };
        let params0 = shell.default_exec_params();
        if let Err(e) = shell.run_string(set_cmd.clone(), &src, &params0).await {
            eprintln!("sarun-engine brush-sh: applying flags ({set_cmd}) failed: {e}");
            return 2;
        }
    }

    let prog = match shell.parse_string(script.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: cannot parse this script \
                       (NO /bin/sh fallback): {e}");
            return 2;
        }
    };
    let params = shell.default_exec_params();
    let mut last_code = 0i32;
    let mut seq = 0i64;
    for complete in prog.complete_commands {
        // Emit this complete-command's per-pipeline provenance BEFORE running
        // it, mirroring the top-level run_brush contract. We collect each
        // pipeline's records, tag with seq/spawn_ts/nested, ship one message.
        let spawn_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let mut recs = vec![];
        for mut rec in complete_command_records(&complete) {
            if let Value::Object(ref mut m) = rec {
                m.insert("seq".to_string(), json!(seq));
                m.insert("spawn_ts".to_string(), json!(spawn_ts));
                m.insert("nested".to_string(), json!(true));
            }
            recs.push(rec);
            seq += 1;
        }
        send_nested_pipeline_records(recs);
        let one = brush_parser::ast::Program { complete_commands: vec![complete] };
        match shell.run_program(one, &params).await {
            Ok(result) => last_code = u8::from(result.exit_code) as i32,
            Err(e) => {
                eprintln!("sarun-engine brush-sh: execution error \
                           (NO /bin/sh fallback): {e}");
                return 1;
            }
        }
    }
    last_code
}

/// Brush-sh interactive REPL: `sh -i` / `bash -i` inside a -b box drops
/// into brush-interactive's reedline backend — multi-line editing,
/// history, completion, highlighting — all driven by the SAME brush-core
/// shell we use for `-c SCRIPT`, so semantics match script mode exactly.
/// Set-flags (`-e`/`-u`/`-o NAME`/...) are applied before the loop starts.
async fn run_brush_interactive(shell_name: String,
                               set_flags: Vec<String>, set_o: Vec<String>,
                               unset_o: Vec<String>, bash_mode: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    // Build the shell as INTERACTIVE from the start — brush-core's builder
    // wires up Option<History> and enables job-control during build(), and
    // both are checked there only. Setting shell.options_mut().interactive
    // afterwards leaves history=None, which makes brush-interactive's
    // reedline DefaultHinter panic on todo! the first keystroke.
    let shell_res = build_box_shell_opt(!bash_mode, Some(shell_name.clone()),
                                        Some(vec![]), Some(cwd),
                                        /*interactive=*/ true).await;
    let mut shell = match shell_res {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: brush init failed: {e}");
            return 127;
        }
    };
    // Apply set/+set flags exactly like the non-interactive paths.
    if !set_flags.is_empty() || !set_o.is_empty() || !unset_o.is_empty() {
        let mut set_cmd = String::from("set");
        for f in &set_flags { set_cmd.push(' '); set_cmd.push_str(f); }
        for n in &set_o    { set_cmd.push_str(" -o "); set_cmd.push_str(n); }
        for n in &unset_o  { set_cmd.push_str(" +o "); set_cmd.push_str(n); }
        let src = brush_core::SourceInfo {
            source: "<brush-sh flags>".into(), start: None,
        };
        let params0 = shell.default_exec_params();
        if let Err(e) = shell.run_string(set_cmd.clone(), &src, &params0).await {
            eprintln!("sarun-engine brush-sh: applying flags ({set_cmd}) failed: {e}");
            return 2;
        }
    }
    // Hand the shell off to brush-interactive. It needs an Arc<tokio::Mutex<>>
    // (ShellRef) because the reedline helpers (completer/validator/highlighter)
    // all clone the ref to query the shell from their callbacks.
    let shell_ref: brush_interactive::ShellRef = std::sync::Arc::new(
        tokio::sync::Mutex::new(shell));
    let ui_opts = brush_interactive::UIOptions::builder().build();
    let mut backend = match brush_interactive::ReedlineInputBackend::new(
        &ui_opts, &shell_ref) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: reedline init failed: {e}");
            return 127;
        }
    };
    let opts: brush_interactive::InteractiveOptions = (&ui_opts).into();
    let mut iash = match brush_interactive::InteractiveShell::new(
        &shell_ref, &mut backend, &opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sarun-engine brush-sh: interactive shell init failed: {e}");
            return 127;
        }
    };
    if let Err(e) = iash.run_interactively().await {
        eprintln!("sarun-engine brush-sh: interactive: {e}");
        return 1;
    }
    // Last exit code lives on the shell after the loop ends.
    let s = shell_ref.lock().await;
    i32::from(u8::from(s.last_exit_status()))
}

/// Build the brush shell, parse the script, emit one FRAME_PROV per pipeline,
/// then execute the WHOLE program through brush-core. No /bin/sh fallback:
///   - a parse error  → VISIBLE message, exit 2
///   - a fatal exec error (unsupported construct) → VISIBLE message, non-zero
async fn run_brush(conn_fd: i32, script: String) -> i32 {
    // sh-mode brush: POSIX-ish, closest to the /bin/sh the box would otherwise
    // get (the top-level body stands in for the box's default /bin/sh, so it is
    // sh_mode even though nested `bash -c` uses bash mode). The shared
    // build_box_shell() installs the BashMode shell builtins + bundled coreutils
    // (see module header) — without builtins brush-core ships an empty table, so
    // even POSIX builtins would surface as "command not found".
    let shell_res = build_box_shell(true, None, None, None).await;
    let mut shell = match shell_res {
        Ok(s) => s,
        Err(e) => { eprintln!("sarun-engine inner: -b brush init failed: {e}"); return 127; }
    };

    // Parse FIRST so we can (a) emit provenance and (b) turn a parse error into a
    // visible, non-zero result rather than a quiet fallback.
    let prog = match shell.parse_string(script.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sarun-engine inner: -b brush cannot parse this command \
                       (NO /bin/sh fallback): {e}");
            return 2;
        }
    };
    // Execute one complete-command at a time on the SAME persistent shell, so
    // shell state (vars, cwd, exit status, functions) carries across exactly as a
    // single run_program over the whole Program would — emitting each pipeline's
    // FRAME_PROV (carrying its parsed structure + literal output-redirect targets,
    // plus a `spawn_ts`/`seq` for ordering/diagnostics) BEFORE running it. The
    // engine makes the process↔pipeline link from those output targets at teardown
    // (see the header). We do our OWN error handling (no run_string auto-display,
    // no /bin/sh fallback) so an unsupported construct surfaces as a visible
    // message + non-zero.
    let params = shell.default_exec_params();
    let mut last_code = 0i32;
    let mut seq = 0i64;
    for complete in prog.complete_commands {
        let spawn_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        for mut rec in complete_command_records(&complete) {
            if let Value::Object(ref mut m) = rec {
                m.insert("seq".to_string(), json!(seq));
                m.insert("spawn_ts".to_string(), json!(spawn_ts));
            }
            send_prov(conn_fd, &rec);
            seq += 1;
        }
        let one = brush_parser::ast::Program { complete_commands: vec![complete] };
        match shell.run_program(one, &params).await {
            Ok(result) => last_code = u8::from(result.exit_code) as i32,
            Err(e) => {
                eprintln!("sarun-engine inner: -b brush execution error \
                           (NO /bin/sh fallback): {e}");
                return 1;
            }
        }
    }
    last_code
}

// ── n2/ninja in-process recipe executor (Phase 1) ────────────────────────────
// When a -b box runs `ninja` (shadow-bound to the engine, see runner.rs and
// crate::n2run), we embed the vendored n2 build scheduler IN-PROCESS and route
// each recipe THROUGH this executor instead of n2's posix_spawn(/bin/sh -c …).
// The byte/Termination contract is identical to n2's upstream run_command:
//   * stdin = /dev/null (recipes get no input)
//   * stdout+stderr MERGED into one pipe, fed to n2's output_cb
//   * exit 0 → Success, non-zero → Failure
// The recipe runs through the SAME embedded brush (BashMode builtins +
// in-process coreutils via build_box_shell) as every other -b recipe, so a
// `cp`/`sort`/pipeline runs in-process with NO /bin/sh fork and NO engine
// re-exec.
//
// Capture: the recipe's file writes go through the overlay (FUSE) exactly as
// usual — they are NOT on the fd 1/2 path. The recipe's stdout/stderr BYTES are
// teed: read off the pipe, handed to n2's output_cb AND written to the box's
// real FUSE stdout sink (fd 1, saved before we point brush's fd 1/2 at the
// pipe), so console output is still recorded like any captured run.

/// One shared multi-thread tokio runtime for all embedded-n2 recipes. n2's
/// scheduler calls the executor on its own std::thread (sync); we block_on the
/// async brush future against this runtime. A OnceLock so it is built once.
static N2_RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn n2_runtime() -> Option<&'static tokio::runtime::Runtime> {
    N2_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all().build()
            .expect("sarun-engine n2: tokio runtime")
    });
    N2_RT.get()
}

/// Strip a leading `/bin/sh -c '<recipe>'` (or sh/bash/dash by basename)
/// wrapper, returning the inner recipe; otherwise return the cmdline unchanged.
/// ninja `command =` lines are frequently `/bin/sh -c '<recipe>'`; we unwrap to
/// the inner recipe and run THAT through brush rather than nesting a shell
/// (model: script_from_argv's `sh -c` handling). A bare command line (no `sh -c`
/// wrapper) is returned unchanged and brush runs it directly.
///
/// We split the wrapper with a small POSIX-faithful word splitter (handling
/// '…' and "…" quoting + backslash escapes) ONLY to recognise the
/// `<shell> -c <script> [name…]` shape and recover the literal inner script.
/// If the prefix is not a recognised `sh -c`, the ORIGINAL cmdline is run as-is
/// (no re-quoting), so non-wrapped recipes are byte-identical.
/// Bash leniency for trailing `\` at end-of-input. `bash -c 'echo \'`
/// emits a literal backslash; brush-parser raises "unterminated escape
/// sequence" — its strict POSIX shape. Recipes generated by gnu make
/// from constructs like `$(call func, \)` deliver exactly that bare-
/// trailing-backslash shape. If `recipe` ends with an ODD number of
/// consecutive backslashes, append ONE more so the final `\` self-
/// quotes; brush then parses it and emits the same byte bash does.
/// Even-length trailing runs are untouched (they're already properly
/// escaped pairs).
fn double_trailing_backslash(mut recipe: String) -> String {
    let trailing = recipe.bytes().rev().take_while(|&b| b == b'\\').count();
    if trailing.is_multiple_of(2) {
        return recipe;
    }
    recipe.push('\\');
    recipe
}

fn unwrap_sh_c(cmdline: &str) -> String {
    let words = split_words(cmdline);
    if words.len() >= 3 {
        let base = std::path::Path::new(&words[0])
            .file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(base, "sh" | "bash" | "dash" | "brush") && words[1] == "-c" {
            return words[2].clone();
        }
    }
    cmdline.to_string()
}

/// Minimal POSIX word splitter: splits on unquoted whitespace, honouring single
/// quotes (literal), double quotes (backslash escapes \" \\ \$ \`), and bare
/// backslash escapes. Sufficient to recover the inner script of `sh -c '…'`.
fn split_words(s: &str) -> Vec<String> {
    let mut words = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' if !in_word => {}
            ' ' | '\t' | '\n' => { words.push(std::mem::take(&mut cur)); in_word = false; }
            '\'' => {
                in_word = true;
                for q in chars.by_ref() { if q == '\'' { break; } cur.push(q); }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    if q == '"' { break; }
                    if q == '\\' {
                        if let Some(&n) = chars.peek() {
                            if matches!(n, '"' | '\\' | '$' | '`') { cur.push(chars.next().unwrap()); continue; }
                        }
                    }
                    cur.push(q);
                }
            }
            '\\' => { in_word = true; if let Some(n) = chars.next() { cur.push(n); } }
            _ => { in_word = true; cur.push(c); }
        }
    }
    if in_word { words.push(cur); }
    words
}

/// Run ONE n2 recipe `cmdline` through embedded brush, merging stdout+stderr
/// into `output_cb` (n2's contract) while teeing those bytes to the box FUSE
/// stdout sink for capture. Returns the recipe's exit code.
pub fn run_recipe_in_process(cmdline: &str, output_cb: &mut dyn FnMut(&[u8])) -> i32 {
    run_recipe_in_process_opt(cmdline, output_cb, true)
}

/// Same as [`run_recipe_in_process`] but lets the caller skip bundled
/// uutils coreutils so `cp`/`mkdir`/`ls`/etc. fork+exec the host binary.
/// kati's make path passes `bundle_coreutils=false` because uutils
/// localization caches each util's FluentResource in a process-global
/// OnceLock that gets poisoned by the FIRST util to run (see
/// box_builtins_opt for the full rationale). Phase 1 (n2 ninja) keeps
/// `bundle_coreutils=true` — interactive boxes value the in-process
/// speed of bundled coreutils over byte-perfect bash compatibility.
pub fn run_recipe_in_process_opt(
    cmdline: &str,
    output_cb: &mut dyn FnMut(&[u8]),
    bundle_coreutils: bool,
) -> i32 {
    let recipe = double_trailing_backslash(unwrap_sh_c(cmdline));
    let Some(rt) = n2_runtime() else {
        output_cb(b"sarun-engine n2: no tokio runtime\n");
        return 127;
    };
    // The single merged stdout+stderr pipe (matches n2's posix path: one pipe,
    // both fds dup'd onto it). brush's fd 1 and fd 2 both point at the write end.
    let (mut reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => { output_cb(format!("sarun-engine n2: pipe: {e}\n").as_bytes()); return 127; }
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let recipe_owned = recipe.clone();
    // Run brush on a worker thread so the calling (n2 scheduler) thread can drain
    // the pipe concurrently — a finite pipe buffer would otherwise deadlock.
    let exec = std::thread::spawn(move || {
        rt.block_on(async move {
            let mut shell = match build_box_shell_full(
                true, None, None, Some(cwd), false, bundle_coreutils,
            ).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("sarun-engine n2: brush init failed: {e}");
                    return 127;
                }
            };
            // Point brush's fd 1 AND fd 2 at the SAME pipe write end (merged).
            // PipeWriter is fd-backed, so a uutils coreutil's CoreutilWrapper
            // dup2's that fd onto the process 1/2 around its uumain call too —
            // builtins, coreutils and any forked binary all land on the pipe.
            let w2 = match writer.try_clone() {
                Ok(w) => w,
                Err(e) => { eprintln!("sarun-engine n2: pipe clone: {e}"); return 127; }
            };
            shell.open_files_mut().set_fd(1, brush_core::openfiles::OpenFile::from(writer));
            shell.open_files_mut().set_fd(2, brush_core::openfiles::OpenFile::from(w2));
            let prog = match shell.parse_string(recipe_owned.clone()) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("sarun-engine n2: cannot parse recipe \
                               (NO /bin/sh fallback): {e}");
                    return 2;
                }
            };
            let params = shell.default_exec_params();
            match shell.run_program(prog, &params).await {
                Ok(result) => u8::from(result.exit_code) as i32,
                Err(e) => {
                    eprintln!("sarun-engine n2: recipe execution error \
                               (NO /bin/sh fallback): {e}");
                    1
                }
            }
            // shell (and its PipeWriter clones) drop here → write end closed →
            // the drain loop below sees EOF.
        })
    });

    // sarun: drain the merged pipe into n2's output_cb. n2 itself
    // writes the bytes back out to the user's terminal as the recipe
    // runs; we used to ALSO tee directly to a saved fd-1 dup of the
    // FUSE sink, which made every byte of recipe output appear twice
    // in the user's terminal (visible diff against real make for any
    // multi-line recipe).
    //
    // sarun bash-compat shim: brush emits errors as `error: <msg>` (its
    // own format from `writeln!(stderr, "error: {e}")` in brush-core
    // interp.rs). Bash emits `/bin/bash: line N: <msg>`. Both are
    // stripped by the corpus runner's kati_norms (`/bin/(ba)?sh: ` →
    // ""), but only if the prefix matches. Rewrite the brush form to a
    // bash-shaped one ON THE FLY so the user-visible output AND the
    // corpus comparator see the same shape standalone rkati does. We
    // process line-by-line to keep the substitution unambiguous; tail
    // bytes without a trailing newline are buffered until either a
    // newline arrives or the pipe closes.
    let mut buf = [0u8; 4 << 10];
    let mut line_buf: Vec<u8> = Vec::new();
    loop {
        let n = match std::io::Read::read(&mut reader, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            line_buf.push(b);
            if b == b'\n' {
                emit_bash_compat(&line_buf, output_cb);
                line_buf.clear();
            }
        }
    }
    if !line_buf.is_empty() {
        emit_bash_compat(&line_buf, output_cb);
    }
    exec.join().unwrap_or(127)
}

/// One line of brush output → bash-shaped form on `output_cb`.
/// Currently rewrites the `error: command not found: NAME` shape that
/// brush emits when builtin/PATH lookup fails into `/bin/sh: line 1:
/// NAME: command not found` — what bash would emit and what
/// `kati_norms()` strips down to `NAME: command not found`. Other
/// `error: <msg>` lines pass through (rare, mostly internal brush
/// errors that don't appear in standard recipes).
fn emit_bash_compat(line: &[u8], output_cb: &mut dyn FnMut(&[u8])) {
    const NF: &[u8] = b"error: command not found: ";
    if let Some(rest) = line.strip_prefix(NF) {
        let name = rest.strip_suffix(b"\n").unwrap_or(rest);
        let mut out = Vec::with_capacity(line.len() + 24);
        out.extend_from_slice(b"/bin/sh: line 1: ");
        out.extend_from_slice(name);
        out.extend_from_slice(b": command not found\n");
        output_cb(&out);
        return;
    }
    output_cb(line);
}

/// The bare-fn executor installed into the vendored n2 (process::set_executor).
/// Maps the recipe exit code → n2::process::Termination, matching upstream:
/// 0 → Success, anything else → Failure (a brush recipe has no signal path, so
/// Interrupted is not produced here — n2's own SIGINT handling is suppressed in
/// embedded mode anyway).
pub fn n2_executor(cmdline: &str, output_cb: &mut dyn FnMut(&[u8])) -> n2::process::Termination {
    if run_recipe_in_process(cmdline, output_cb) == 0 {
        n2::process::Termination::Success
    } else {
        n2::process::Termination::Failure
    }
}
