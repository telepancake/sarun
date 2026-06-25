// Phase 2 — embedded `make`. In a -b brush box the runner shadows make/gmake
// (and /usr/bin/make, /bin/make) with the ENGINE binary (gated on SARUN_BRUSH_SH
// =1, exactly like the ninja/sh shadows). main() detects argv0 basename == make
// /gmake BEFORE its normal dispatch and lands here, which:
//   1. drives a vendored fork of kati (github.com/google/kati src-rs/) IN-PROCESS
//      to PARSE the box's Makefile, run dependency analysis, and EXECUTE the dep
//      graph via kati's OWN executor (src-rs/exec.rs) — sequential, declaration
//      order, mtime-based staleness, i.e. standalone rkati semantics. NO ninja
//      graph is generated and NO n2 is involved. (An earlier design had kati
//      emit a ninja graph in-memory and handed it to the embedded n2 to run;
//      that handoff is gone — kati executes directly now.)
//   2. routes every recipe through embedded brush in THIS process via the
//      `install_recipe_runner` hook — no /bin/sh fork, no engine re-exec —
//      unless SHELL is non-POSIX, in which case the runner declines
//      (Passthrough) and kati's exec.rs uses the classic fork+exec path.
//   3. emits a `build_edges` provenance frame for the dep graph (the same
//      frame/table/verb Phase 1's ninja path used), capturing EVERY edge
//      including up-to-date targets exec.rs will skip.
//
// kati's FLAGS is a process-global LazyLock parsed from argv. We can't be argv0
// `make`-with-flags, so we synthesize the argv kati should parse (the box's
// -f/-C/targets/VAR=val translated through) and install it via the vendored
// `kati::flags::install_args` hook BEFORE the first FLAGS access. (We still
// inject `--ninja` into that synthesized argv, but in THIS direct-execute path
// `FLAGS.generate_ninja` is inert — only the standalone main.rs/ninja.rs paths
// consult it, never run_kati — so forcing it is a harmless no-op.)
//
// NO-FALLBACK (D9): anything kati cannot parse/evaluate or execute is a VISIBLE
// error and a non-zero exit. We NEVER silently exec the real `make`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::Arc;

use bytes::{BufMut, BytesMut};
use kati::dep::{NamedDepNode, make_dep};
use kati::eval::{Evaluator, FrameType};
use kati::expr::Value;
use kati::flags::FLAGS;
use kati::loc::Loc;
use kati::symtab::{Symbol, intern, join_symbols};
use kati::var::{VarOrigin, Variable};
use parking_lot::Mutex;

/// True when this engine invocation should act as the embedded-make entry:
/// SARUN_BRUSH_SH=1 (a -b brush box) AND argv[0]'s basename is `make`/`gmake`.
pub fn is_make_invocation() -> bool {
    if std::env::var("SARUN_BRUSH_SH").as_deref() != Ok("1") {
        return false;
    }
    let arg0 = std::env::args().next().unwrap_or_default();
    let base = std::path::Path::new(&arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(base, "make" | "gmake")
}

/// Translate the box's `make` argv into the argv our vendored kati should parse.
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
    out.push(OsString::from(argv.first().cloned().unwrap_or_else(|| "make".into())));
    // Inert in our direct-execute path (see fn doc); kept for argv parity.
    out.push(OsString::from("--ninja"));

    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        match a.as_str() {
            // -f FILE / -C DIR: kati understands both (it reads -f and -C). Pass
            // through verbatim (kati's -C does the chdir; we also chdir below so
            // the build_edges + n2 cwd match).
            "-f" | "-C" => {
                out.push(OsString::from(a));
                if let Some(v) = argv.get(i + 1) {
                    out.push(OsString::from(v));
                }
                i += 2;
            }
            // Combined -fFILE / -CDIR.
            _ if a.starts_with("-f") || a.starts_with("-C") => {
                out.push(OsString::from(a));
                i += 1;
            }
            // -jN parallelism: kati parses -j (used only to seed $(MAKE)); n2 runs
            // serial anyway under the in-process executor. Pass numeric forms.
            _ if a.starts_with("-j") => {
                out.push(OsString::from(a));
                i += 1;
            }
            // -s silent / -k keep-going-ish flags kati doesn't model: drop quietly
            // is WRONG per D9. We accept the handful kati's flags.rs knows (-s),
            // and refuse anything else that looks like an unknown dash-flag.
            "-s" => {
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
                    "sarun-engine make: unsupported make flag {a:?} \
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
/// Seeds CC/CXX/AR/MAKE/SHELL and the builtin .c.o/.cc.o suffix rules so ordinary
/// Makefiles relying on implicit rules work. Returns the parsed bootstrap stmts.
fn read_bootstrap_makefile(
    targets: &[Symbol],
    working_dir: &std::path::Path,
) -> anyhow::Result<Arc<Mutex<Vec<kati::stmt::Stmt>>>> {
    let mut bootstrap = BytesMut::new();
    bootstrap.put_slice(b"CC?=cc\n");
    bootstrap.put_slice(b"CXX?=g++\n");
    bootstrap.put_slice(b"AR?=ar\n");
    // sarun: report GNU make 4.3 (matches our compat target); Makefiles
    // gated on `ifeq ($(MAKE_VERSION),4.x)` see what they expect.
    bootstrap.put_slice(b"MAKE_VERSION?=4.3\n");
    // sarun: MAKELEVEL tracks recursion across sub-makes. Initialize
    // from env (default 0) so $(MAKELEVEL) is defined for the top
    // makefile; the bump-for-children happens in the recipe-runner.
    {
        let level = std::env::var("MAKELEVEL")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        bootstrap.put_slice(format!("MAKELEVEL:={level}\n").as_bytes());
    }
    bootstrap.put_slice(b"KATI?=ckati\n");
    bootstrap.put_slice(b"SHELL=/bin/sh\n");
    if !FLAGS.no_builtin_rules {
        bootstrap.put_slice(b".c.o:\n");
        bootstrap.put_slice(b"\t$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<\n");
        bootstrap.put_slice(b".cc.o:\n");
        bootstrap.put_slice(b"\t$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<\n");
    }
    // sarun: GNU make's $(MAKE) is the name make was invoked as (argv[0]) — no
    // -jN appended. Parallelism propagates via MAKEFLAGS, not MAKE itself.
    // Without this, sub-`$(MAKE)` recipes echoed verbatim by the parent (e.g.
    // `echo '... $(MAKE) ...'`) would print `make -j4`, diverging from gnu's
    // plain `make`. The FUSE shadow makes `make` route back to the engine.
    bootstrap.put_slice(b"MAKE?=make\n");
    bootstrap.put_slice(b"MAKECMDGOALS?=");
    bootstrap.put(join_symbols(targets, b" "));
    bootstrap.put_u8(b'\n');
    // CURDIR is the make's logical working dir (the brush context's cwd / -C
    // target), NOT the engine's process cwd — a Makefile computes srctree and
    // resolves `include`s against it (e.g. busybox's Kbuild).
    bootstrap.put_slice(b"CURDIR:=");
    bootstrap.put_slice(working_dir.as_os_str().as_bytes());
    bootstrap.put_u8(b'\n');
    kati::parser::parse_buf(
        &bootstrap.freeze(),
        Loc { filename: intern("*bootstrap*"), line: 0 },
    )
}

/// Run kati end-to-end: bootstrap + command-line vars + parse the Makefile +
/// dependency analysis + EXECUTE the dep graph via kati's own executor
/// (kati::exec::exec). A port of upstream kati main.rs `run()`, but driving
/// kati's executor directly instead of generating a ninja graph — sarun
/// executes in-process and does not emit ninja. Returns Ok on success.
///
/// `remake_active` (in the returned RunKatiResult) means the makefile had at
/// least one required `include` of a file the same makefile has a rule for;
/// kati's executor builds the include target(s) first, then the caller re-execs
/// the engine so the next invocation parses with the freshly-generated content
/// visible (GNU make's remake-the-makefile loop).
struct RunKatiResult {
    remake_active: bool,
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
    // variables here — many makes share one engine process, so that would mix
    // their environments.
    seed_env: &[(std::ffi::OsString, std::ffi::OsString)],
) -> anyhow::Result<RunKatiResult> {
    let mut ev = Evaluator::new();
    // sarun: the Evaluator seeds working_dir from the process cwd; override it
    // with the caller's logical working dir. For the shadow path this equals the
    // process cwd; for the in-process builtin it's the make's dir resolved from
    // -C against the brush context's cwd (no process chdir).
    ev.working_dir = working_dir.to_path_buf();
    ev.start()?;

    // sarun: GNU make's MAKEFILE_LIST has no leading space — the main
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
            Variable::new_recursive(val, VarOrigin::Environment, Some(ev.current_frame()), None, v),
            false,
            None,
        )?;
    }
    // MAKEFLAGS is the jobserver's channel: jobserver::advertise() wrote the
    // current `--jobserver-auth=…` into the PROCESS env just before this call
    // (in make_builtin), which is AFTER seed_env was captured. Pull the live
    // value so $(MAKEFLAGS) — and any sub-make that inherits it — sees the
    // jobserver. This is the one var that legitimately rides std::env (the
    // jobserver's existing design); a single read of a near-constant value.
    if let Some(mf) = std::env::var_os("MAKEFLAGS") {
        let v = bytes::Bytes::from(mf.as_bytes().to_vec());
        let val = Arc::new(Value::Literal(None, v.clone()));
        ev.set_global_var(
            intern(b"MAKEFLAGS".to_vec()),
            Variable::new_recursive(val, VarOrigin::Environment, Some(ev.current_frame()), None, v),
            false,
            None,
        )?;
    }

    let bootstrap_asts = read_bootstrap_makefile(targets, working_dir)?;
    // sarun: this make's MAKELEVEL is whatever the seed env carried; a
    // recipe-spawned sub-make must see the NEXT level. We don't bump the process
    // env (that's a shared global write across concurrent in-process makes) —
    // the +1 is emitted into the export prefix below so children pick it up
    // through their subshell env.
    let child_makelevel = seed_env
        .iter()
        .find(|(k, _)| k == "MAKELEVEL")
        .and_then(|(_, v)| v.to_str())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        + 1;
    {
        let _frame = ev.enter(FrameType::Phase, bytes::Bytes::from_static(b"*bootstrap*"), Loc::default());
        ev.in_bootstrap();
        for stmt in bootstrap_asts.lock().iter() {
            stmt.eval(&mut ev)?;
        }
    }
    {
        let _frame = ev.enter(FrameType::Phase, bytes::Bytes::from_static(b"*command line*"), Loc::default());
        ev.in_command_line();
        for l in cl_vars {
            let asts = kati::parser::parse_buf(l, Loc { filename: intern("*bootstrap*"), line: 0 })?;
            let asts = asts.lock();
            for a in asts.iter() {
                a.eval(&mut ev)?;
            }
        }
    }
    ev.in_toplevel_makefile();
    {
        let _eval_frame = ev.enter(FrameType::Phase, bytes::Bytes::from_static(b"*parse*"), Loc::default());
        let _file_frame = ev.enter(FrameType::Parse, bytes::Bytes::from(makefile.as_bytes().to_vec()), Loc::default());
        let Some(mk) = kati::file_cache::get_makefile(makefile, &ev.working_dir)? else {
            anyhow::bail!("makefile not found");
        };
        let stmts = mk.stmts.lock();
        for stmt in stmts.iter() {
            stmt.eval(&mut ev)?;
        }
    }

    // sarun: GNU make's remake-the-makefile loop. After parse, if a
    // required `include` named a file that didn't exist at parse time
    // AND a rule for it is in ev.rules, build THOSE targets first;
    // make_main will then re-exec the engine so the second parse sees
    // the freshly-generated content. If no rule applies, raise the
    // canonical error and exit.
    let mut remake_targets: Vec<Symbol> = Vec::new();
    {
        let pending = std::mem::take(&mut ev.pending_remake_includes);
        for (loc, name) in &pending {
            let sym = intern(name.as_bytes().to_vec());
            if ev.rules.iter().any(|r| r.outputs.contains(&sym)) {
                remake_targets.push(sym);
            } else {
                let pat_str = String::from_utf8_lossy(name.as_bytes());
                eprintln!("{loc}: {pat_str}: No such file or directory");
                std::process::exit(2);
            }
        }
    }
    let remake_active = !remake_targets.is_empty();

    let nodes: Vec<NamedDepNode>;
    {
        let _frame = ev.enter(FrameType::Phase, bytes::Bytes::from_static(b"*dependency analysis*"), Loc::default());
        // When remaking, only build the include targets in this
        // invocation; the user's real targets get built in the
        // re-exec'd process.
        let dep_targets = if remake_active {
            remake_targets.clone()
        } else {
            targets.to_owned()
        };
        nodes = make_dep(&mut ev, dep_targets)?;
    }

    // sarun: build the make's exported environment as a non-echoed shell prefix
    // (`export NAME='val'` / `unset NAME`) rather than staging it into the
    // process env. In a box, every recipe — and every recursive `$(MAKE)` — runs
    // in-process through a brush subshell; many of those makes share ONE engine
    // process, so a `std::env::set_var` here would (a) be a data race against
    // sibling makes building their own subshells and (b) leak one make's exports
    // into another. exec.rs prepends this prefix to each recipe's subshell and
    // func.rs prepends it to `$(shell)`, so exports reach children through the
    // per-subshell env instead. The standalone rkati binary leaves this empty and
    // keeps the std::env path (one OS process per make, where that's correct).
    if ev.export_all_vars {
        let all = ev.get_symbol_names(|v| {
            !matches!(
                v.read().origin(),
                kati::var::VarOrigin::Default | kati::var::VarOrigin::Automatic
            )
        });
        for (sym, _) in all {
            ev.exports.entry(sym).or_insert(true);
        }
    }
    fn emit_export(prefix: &mut Vec<u8>, name: &[u8], value: &[u8]) {
        prefix.extend_from_slice(b"export ");
        prefix.extend_from_slice(name);
        prefix.extend_from_slice(b"='");
        for &b in value {
            if b == b'\'' {
                prefix.extend_from_slice(b"'\\''");
            } else {
                prefix.push(b);
            }
        }
        prefix.extend_from_slice(b"'\n");
    }
    let mut prefix: Vec<u8> = Vec::new();
    // MAKELEVEL is exported to children at the NEXT level (computed above from
    // the seed env, never from a process-global bump).
    emit_export(&mut prefix, b"MAKELEVEL", child_makelevel.to_string().as_bytes());
    // GNU make exports ENVIRONMENT-origin variables to children by default. The
    // recipe subshell inherits the engine process env, but a NESTED make's
    // environment additions (its parent's exports, carried in via seed_env) are
    // NOT in that process env — so re-export the make's inherited env here, with
    // current values, so recipes and recursive sub-makes see them. Skip the
    // shell-managed vars brush maintains itself (PWD/OLDPWD/SHLVL/_) and
    // MAKELEVEL (emitted above), and skip anything the makefile explicitly
    // `unexport`ed.
    // MAKEFLAGS is make-managed and is also the jobserver's advertisement
    // channel (jobserver::advertise writes it into the process env so forked
    // tools like `gcc -flto=jobserver` inherit it). Don't re-export the stale
    // seed value here — that would clobber the advertised one in recipes.
    const SHELL_MANAGED: &[&[u8]] = &[b"PWD", b"OLDPWD", b"SHLVL", b"_", b"MAKELEVEL", b"MAKEFLAGS"];
    fn is_sh_name(n: &[u8]) -> bool {
        !n.is_empty()
            && (n[0].is_ascii_alphabetic() || n[0] == b'_')
            && n.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
    }
    for (k, _) in seed_env {
        let kb = k.as_bytes();
        // Names that aren't valid shell identifiers (e.g. exported bash
        // functions like `BASH_FUNC_x%%`) can't be set via `export NAME=` and
        // would break the prefix — the subshell already inherits them anyway.
        if SHELL_MANAGED.contains(&kb) || !is_sh_name(kb) {
            continue;
        }
        let sym = intern(kb.to_vec());
        if ev.exports.get(&sym) == Some(&false) {
            continue; // explicitly unexported
        }
        let value = if let Some(v) = ev.lookup_var(sym)? {
            use kati::expr::Evaluable;
            v.read().eval_to_buf(&mut ev)?
        } else {
            bytes::Bytes::new()
        };
        emit_export(&mut prefix, kb, &value);
    }
    // Explicitly `export`ed make variables (override any env-origin value above)
    // and explicit `unexport`s.
    for (name, export) in ev.exports.clone() {
        let nb = name.as_bytes();
        if export {
            let value = if let Some(v) = ev.lookup_var(name)? {
                use kati::expr::Evaluable;
                v.read().eval_to_buf(&mut ev)?
            } else {
                bytes::Bytes::new()
            };
            emit_export(&mut prefix, &nb, &value);
        } else {
            prefix.extend_from_slice(b"unset ");
            prefix.extend_from_slice(&nb);
            prefix.push(b'\n');
        }
    }
    ev.box_export_prefix = bytes::Bytes::from(prefix);

    // sarun: emit the build_edges provenance frame BEFORE exec so the
    // UI's build target pane is populated immediately, even for
    // up-to-date targets that exec.rs will skip. Walk the kati dep
    // graph reachable from `nodes` and ship one edge per node — same
    // shape Phase 1 ninja's emit_build_edges produced. Without this
    // the build target pane is empty in -b boxes (regression from
    // ripping out the n2 path).
    emit_build_edges_kati(&nodes);

    {
        // sarun: drive kati's OWN executor (src-rs/exec.rs) on the dep
        // graph directly — NO ninja generation, NO n2. Recipes run
        // sequentially, in declaration order, with mtime-based
        // staleness — i.e. exactly the standalone rkati semantics, so
        // box-mode now passes the same corpus tests rkati does. The
        // shell call inside exec.rs is intercepted by the
        // install_recipe_runner hook we set in make_main and routed
        // through embedded brush in-process — no fork+exec to a
        // shadowed /bin/sh.
        let _frame = ev.enter(
            FrameType::Phase,
            bytes::Bytes::from_static(b"*execute*"),
            Loc::default(),
        );
        kati::exec::exec(nodes, &mut ev)?;
        ev.finish()?;
    }
    Ok(RunKatiResult { remake_active })
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
    let mut edges: Vec<serde_json::Value> = Vec::new();

    fn visit(
        node: &N,
        seen: &mut HashSet<kati::symtab::Symbol>,
        edges: &mut Vec<serde_json::Value>,
    ) {
        let (sym, dep) = node;
        if !seen.insert(*sym) {
            return;
        }
        let guard = dep.lock();
        let outs: Vec<String> = std::iter::once(guard.output.to_string())
            .chain(guard.implicit_outputs.iter().map(|s| s.to_string()))
            .collect();
        let ins: Vec<String> = guard
            .actual_inputs
            .iter()
            .map(|s| s.to_string())
            .collect();
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
        edges.push(serde_json::json!({
            "outs": outs,
            "ins": ins,
            "cmd": cmd,
        }));
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

    let msg = serde_json::json!({"type": "build_edges", "edges": edges});
    crate::runner::send_nested_prov(format!("{msg}\n").as_bytes());
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
fn install_make_recipe_runner() {
    kati::fileutil::install_recipe_runner(Arc::new(|shell, _shellflag, cmd, cwd, redirect_stderr, output_cb| {
        use kati::fileutil::RecipeRunnerDecision;
        let shell_base = std::path::Path::new(std::ffi::OsStr::from_bytes(shell))
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let posix_shell = matches!(shell_base, "sh" | "bash" | "dash" | "ash" | "ksh" | "zsh");
        if !posix_shell {
            return RecipeRunnerDecision::Passthrough;
        }
        let s = std::str::from_utf8(cmd)
            .map(std::borrow::Cow::Borrowed)
            .unwrap_or_else(|_| String::from_utf8_lossy(cmd));
        // The recipe cwd is threaded EXPLICITLY from kati (the make's working_dir)
        // rather than read from a make-thread thread-local — under -j the recipe
        // runs on a worker thread that wouldn't see it. Set it for THIS worker
        // thread around the run (save/restore so nested makes nest cleanly).
        let cwd_path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(cwd));
        let prev = crate::brush::set_box_recipe_cwd(Some(cwd_path));
        // bundle_coreutils=false: see brush::box_builtins_opt. uutils
        // localization caches each util's FluentResource in a process-
        // global OnceLock; the first util to run owns it, every later
        // util's translate!() returns the raw key (e.g. cp's
        // `cp-error-cannot-stat`). For make recipes we accept the
        // fork+exec overhead in exchange for bash-compatible stderr.
        // Map kati's stderr disposition to brush's fd-2 handling: recipes
        // (RedirectStderr::Stdout) merge stderr into the captured output; a
        // $(shell ...) (RedirectStderr::None) keeps stderr on the box's real
        // fd 2 (terminal/sink) and captures only stdout; DevNull discards it.
        let stderr_mode = match redirect_stderr {
            kati::fileutil::RedirectStderr::Stdout => crate::brush::RecipeStderr::Merge,
            kati::fileutil::RedirectStderr::None => crate::brush::RecipeStderr::Inherit,
            kati::fileutil::RedirectStderr::DevNull => crate::brush::RecipeStderr::Null,
        };
        let code = crate::brush::run_recipe_in_process_opt(&s, output_cb, false, stderr_mode);
        crate::brush::set_box_recipe_cwd(prev);
        RecipeRunnerDecision::Ran { code }
    }));
}

/// The embedded-make entrypoint. `argv` is the FULL process argv (argv[0] is
/// `make`/`gmake`). Returns the process exit code.
pub fn make_main(argv: &[String]) -> i32 {
    install_make_recipe_runner();

    // 2. Recognized make pseudo-actions BEFORE kati's flags parser sees them
    //    (kati panics on anything it doesn't recognize, e.g. --version). The
    //    box's FUSE shadow on /usr/bin/make loops `$(shell make --version | ...)`
    //    style probes back into THIS engine; emit a gnu-make-shaped version
    //    banner so makefiles that grep `Make ([0-9])` extract a sane MAKEVER.
    //    Done before -C handling so a recipe like `make -C sub --version`
    //    short-circuits without a chdir.
    for a in argv.iter().skip(1) {
        if a == "--version" || a == "-v" {
            println!("GNU Make 4.3");
            println!("Built for x86_64-pc-linux-gnu");
            println!("Copyright (C) 1988-2020 Free Software Foundation, Inc.");
            println!("License GPLv3+: GNU GPL version 3 or later <http://gnu.org/licenses/gpl.html>");
            println!("This is free software: you are free to change and redistribute it.");
            println!("There is NO WARRANTY, to the extent permitted by law.");
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
                if let Some(d) = argv.get(i + 1) { let _ = std::env::set_current_dir(d); }
                i += 2;
            } else if let Some(d) = argv[i].strip_prefix("-C") {
                if !d.is_empty() { let _ = std::env::set_current_dir(d); }
                i += 1;
            } else { i += 1; }
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
        Err(msg) => { eprintln!("{msg}"); return 2; }
    };
    let _ = kati::flags::install_args(kargv.clone());
    let flags = kati::flags::Flags::from_args(kargv);

    // 4. kati needs a makefile; if none was given on the argv, discover the
    //    default like real make/kati (GNUmakefile / makefile / Makefile).
    let makefile: OsString = match flags.makefile.lock().clone() {
        Some(m) => m,
        None => {
            let mut found = None;
            for cand in ["GNUmakefile", "makefile", "Makefile"] {
                if std::fs::metadata(cand).is_ok() {
                    found = Some(OsString::from(cand));
                    break;
                }
            }
            match found {
                Some(m) => m,
                None => {
                    eprintln!("sarun-engine make: no makefile found (and none given with -f)");
                    return 2;
                }
            }
        }
    };

    // 4b. If `-f <file>` named a missing makefile, emit gnu-shaped error
    //     output and exit 2 so a recipe like `$(MAKE) -f missing.mk` running
    //     under the box's FUSE-shadowed `make` prints what gnu make would.
    //     Standalone rkati's recursive-make recipe resolves $(MAKE) through
    //     PATH and lands on /usr/bin/make (gnu) for the sub-make, so the
    //     standalone corpus runner only ever sees gnu's framing for this
    //     case — and the corpus comparator was written against gnu. Without
    //     this, box-mode submake_basic diverges from the standalone pass set.
    //
    //     We DO NOT emit Entering/Leaving directory messages because that's
    //     gated on MAKELEVEL > 0 + `--print-directory`; the simpler
    //     "submake/basic.mk: No such file or directory" + "No rule to make
    //     target" pair is what survives the corpus runner's make[N]:
    //     Entering/Leaving strip anyway.
    {
        if std::fs::metadata(&makefile).is_err() {
            let display = makefile.to_string_lossy();
            // No " Stop." suffix: rkati standalone doesn't emit it, so
            // kati_norms doesn't strip it; gnu does emit it but make_norms
            // strips it. Matching rkati's no-Stop form is what makes box ↔ gnu
            // (post-norms) line up.
            eprintln!("make: {display}: No such file or directory");
            eprintln!("make: *** No rule to make target '{display}'.");
            return 2;
        }
    }

    // 5. Run kati end-to-end: parse → dep graph → execute. kati's own
    //    executor (src-rs/exec.rs) walks the dep graph sequentially in
    //    declaration order, uses real mtime for staleness, and would
    //    normally fork+exec /bin/sh per recipe. We installed
    //    brush::run_recipe_in_process as kati's in-process runner above,
    //    so every recipe stays in this process. NO ninja generation, NO
    //    n2 — the box pipeline is byte-identical to standalone rkati on
    //    corpus tests.
    let targets: Vec<Symbol> = flags.targets.clone();
    let cl_vars: Vec<bytes::Bytes> = flags.cl_vars.clone();
    // sarun: shadow/main() path — working dir is the process cwd (already
    // chdir'd for -C above), so this matches the Evaluator's own default.
    let shadow_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    // Shadow/main() path is one OS process per make — seed from the process env.
    let seed_env: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        std::env::vars_os().collect();
    let run_result = match run_kati(&targets, &cl_vars, &makefile, &shadow_cwd, &seed_env) {
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
    let code = 0;

    if run_result.remake_active && code == 0 {
        // sarun: remake-the-makefile loop completed building the
        // generated includes. Re-exec the engine binary with the same
        // argv so the second invocation parses the makefile with the
        // freshly-generated content visible (matches GNU make's
        // self-re-exec). Capped via SARUN_KATI_REMAKE_DEPTH.
        let depth: u32 = std::env::var("SARUN_KATI_REMAKE_DEPTH")
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
        cmd.env("SARUN_KATI_REMAKE_DEPTH", (depth + 1).to_string());
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
/// in THIS process at the right directory instead of re-exec'ing the engine.
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
    recipe_out: Box<dyn std::io::Write>,
    recipe_err: Box<dyn std::io::Write>,
) -> i32 {
    install_make_recipe_runner();

    // make pseudo-actions handled before kati's flag parser (which panics on
    // unknown flags). --version short-circuits to the gnu-shaped banner.
    for a in argv.iter().skip(1) {
        if a == "--version" || a == "-v" {
            let _ = writeln!(out, "GNU Make 4.3");
            let _ = writeln!(out, "Built for x86_64-pc-linux-gnu");
            let _ = writeln!(out, "Copyright (C) 1988-2020 Free Software Foundation, Inc.");
            let _ = writeln!(out, "License GPLv3+: GNU GPL version 3 or later <http://gnu.org/licenses/gpl.html>");
            let _ = writeln!(out, "This is free software: you are free to change and redistribute it.");
            let _ = writeln!(out, "There is NO WARRANTY, to the extent permitted by law.");
            return 0;
        }
    }

    // Per-build jobserver: an explicit -jN on make enables parallelism and
    // advertises the engine-global slip pool into MAKEFLAGS, so this build's
    // recipes — and any `gcc -flto=jobserver` / sub-make they fork — draw from
    // the one machine-wide pool. Plain `make` (no -j) is serial like GNU make and
    // advertises nothing, leaving a nested `ninja` free to parallelize on its own.
    // advertise() is idempotent — a recursive sub-make inherits the pool.
    if let Some(n) = crate::jobserver::explicit_jobs(argv) {
        if n > 1 {
            crate::jobserver::advertise(n);
        }
    }

    let kargv = match kati_argv(argv) {
        Ok(v) => v,
        Err(msg) => {
            let _ = writeln!(err, "{msg}");
            return 2;
        }
    };
    let _ = kati::flags::install_args(kargv.clone());
    let flags = kati::flags::Flags::from_args(kargv);

    // Resolve -C against the context cwd (NO process chdir). flags.working_dir
    // is kati's parsed -C value.
    let mut working_dir = base_cwd.to_path_buf();
    if let Some(c) = &flags.working_dir {
        let p = std::path::Path::new(c);
        working_dir = if p.is_absolute() { p.to_path_buf() } else { working_dir.join(p) };
    }

    // Makefile: explicit -f, else discover GNUmakefile/makefile/Makefile in the
    // working dir. Stored as the name kati interns (relative); the fs read
    // resolves it against working_dir (Evaluator.working_dir / file_cache).
    let makefile: OsString = match flags.makefile.lock().clone() {
        Some(m) => m,
        None => {
            let mut found = None;
            for cand in ["GNUmakefile", "makefile", "Makefile"] {
                if std::fs::metadata(working_dir.join(cand)).is_ok() {
                    found = Some(OsString::from(cand));
                    break;
                }
            }
            match found {
                Some(m) => m,
                None => {
                    let _ = writeln!(
                        err,
                        "sarun-engine make: no makefile found (and none given with -f)"
                    );
                    return 2;
                }
            }
        }
    };
    if std::fs::metadata(working_dir.join(&makefile)).is_err() {
        let display = makefile.to_string_lossy();
        let _ = writeln!(err, "make: {display}: No such file or directory");
        let _ = writeln!(err, "make: *** No rule to make target '{display}'.");
        return 2;
    }

    // Route recipe stdout to the context's fd 1 and recipes' cwd to working_dir
    // for the duration of THIS make; save/restore so a nested recursive $(MAKE)
    // (which lands here again, on its own brush worker thread) nests cleanly.
    let prev_out = kati::exec::set_recipe_out(Some(recipe_out));
    let prev_err = kati::exec::set_recipe_err(Some(recipe_err));
    let prev_cwd = crate::brush::set_box_recipe_cwd(Some(working_dir.clone()));

    let targets: Vec<Symbol> = flags.targets.clone();
    let cl_vars: Vec<bytes::Bytes> = flags.cl_vars.clone();

    // Each `$(MAKE)` is logically a fresh make PROCESS — it must see the current
    // filesystem, not a snapshot another make took earlier. But unlike the
    // standalone rkati binary (one OS process per make), every in-process make
    // in a box shares ONE process and ONE set of process-global caches: the glob
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
    // remake_active; the shadow path re-execs the engine to re-parse with the
    // generated content visible, but a builtin can't re-exec the brush process.
    // Instead we drop the makefile cache (so the regenerated include is re-read)
    // and re-run kati, up to a small cap — matching SARUN_KATI_REMAKE_DEPTH.
    let mut result = run_kati(&targets, &cl_vars, &makefile, &working_dir, seed_env);
    let mut remake_depth = 0u32;
    while matches!(&result, Ok(r) if r.remake_active) && remake_depth < 5 {
        remake_depth += 1;
        // Drop BOTH caches: the makefile cache (so the regenerated include is
        // re-parsed) AND the glob cache (eval_include probes existence via
        // glob(); the first parse cached the missing include as absent, which
        // would otherwise make the re-parse believe it's still missing and loop
        // forever).
        kati::file_cache::clear();
        kati::fileutil::clear_glob_cache();
        result = run_kati(&targets, &cl_vars, &makefile, &working_dir, seed_env);
    }

    kati::exec::set_recipe_out(prev_out);
    kati::exec::set_recipe_err(prev_err);
    crate::brush::set_box_recipe_cwd(prev_cwd);

    match result {
        Ok(r) => {
            if r.remake_active {
                let _ = writeln!(err, "*** kati: remake-the-makefile loop exceeded 5 iterations");
                return 2;
            }
            let _ = out.flush();
            0
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
