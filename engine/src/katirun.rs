// Phase 2 — embedded `make`. In a -b brush box the runner shadows make/gmake
// (and /usr/bin/make, /bin/make) with the ENGINE binary (gated on SARUN_BRUSH_SH
// =1, exactly like the ninja/sh shadows). main() detects argv0 basename == make
// /gmake BEFORE its normal dispatch and lands here, which:
//   1. drives a vendored fork of kati (github.com/google/kati src-rs/) IN-PROCESS
//      to PARSE the box's Makefile and GENERATE a ninja graph;
//   2. hands that ninja graph — purely IN-MEMORY, via a memfd, NEVER a disk
//      build.ninja temp — to the already-embedded n2 (Phase 1) to EXECUTE;
//   3. routes every recipe through embedded brush in THIS process (Phase 1's
//      brush::n2_executor) — no /bin/sh fork, no engine re-exec;
//   4. emits a `build_edges` provenance frame for the generated graph (the same
//      frame/table/verb Phase 1's ninja path uses), capturing EVERY edge
//      including up-to-date targets n2 will skip.
//
// THE HANDOFF (user-mandated, no disk temp file): kati's generate_ninja writes
// the ninja to a FILE PATH (there is no in-memory-string API). We give it the
// path `/proc/self/fd/<memfd>` of an anonymous memfd_create(2) file, let kati
// write there, lseek to 0, read the bytes back into a String, and feed that to
// n2's in-memory loader (n2::load::read_from_content). The memfd auto-frees on
// close — nothing is written to the box filesystem, nothing to clean up.
//
// kati's FLAGS is a process-global LazyLock parsed from argv. We can't be argv0
// `make`-with-flags, so we synthesize the argv kati should parse (--ninja forced
// on, the box's -f/-C/targets/VAR=val translated through) and install it via the
// vendored `kati::flags::install_args` hook BEFORE the first FLAGS access.
//
// NO-FALLBACK (D9): anything kati cannot parse/evaluate, or n2 cannot run, is a
// VISIBLE error and a non-zero exit. We NEVER silently exec the real `make`.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use bytes::{BufMut, BytesMut};
use kati::dep::{NamedDepNode, make_dep};
use kati::eval::{Evaluator, FrameType};
use kati::expr::Value;
use kati::flags::FLAGS;
use kati::loc::Loc;
use kati::ninja::generate_ninja_to_path;
use kati::symtab::{Symbol, intern, join_symbols};
use kati::var::{VarOrigin, Variable};
use parking_lot::Mutex;
use serde_json::json;

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
/// We FORCE `--ninja` (kati must emit a graph, not execute), drop make-only flags
/// kati does not understand (e.g. -j is parsed by kati's own -j handling so we
/// keep numeric ones), and pass through -f/-C/targets/VAR=val. argv0 is kept as
/// the original program name (kati uses it only for subkati_args propagation).
/// Returns Err(msg) for a flag we deliberately refuse (visible, no fallback).
fn kati_argv(argv: &[String]) -> Result<Vec<OsString>, String> {
    let mut out: Vec<OsString> = Vec::new();
    // argv0 — kati needs *some* program name; its basename is irrelevant here.
    out.push(OsString::from(argv.first().cloned().unwrap_or_else(|| "make".into())));
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
fn read_bootstrap_makefile(targets: &[Symbol]) -> anyhow::Result<Arc<Mutex<Vec<kati::stmt::Stmt>>>> {
    let mut bootstrap = BytesMut::new();
    bootstrap.put_slice(b"CC?=cc\n");
    bootstrap.put_slice(b"CXX?=g++\n");
    bootstrap.put_slice(b"AR?=ar\n");
    bootstrap.put_slice(b"MAKE_VERSION?=4.2.1\n");
    bootstrap.put_slice(b"KATI?=ckati\n");
    bootstrap.put_slice(b"SHELL=/bin/sh\n");
    if !FLAGS.no_builtin_rules {
        bootstrap.put_slice(b".c.o:\n");
        bootstrap.put_slice(b"\t$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<\n");
        bootstrap.put_slice(b".cc.o:\n");
        bootstrap.put_slice(b"\t$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<\n");
    }
    bootstrap.put_slice(format!("MAKE?=make -j{}\n", FLAGS.num_jobs.max(1)).as_bytes());
    bootstrap.put_slice(b"MAKECMDGOALS?=");
    bootstrap.put(join_symbols(targets, b" "));
    bootstrap.put_u8(b'\n');
    bootstrap.put_slice(b"CURDIR:=");
    bootstrap.put_slice(std::env::current_dir()?.as_os_str().as_bytes());
    bootstrap.put_u8(b'\n');
    kati::parser::parse_buf(
        &bootstrap.freeze(),
        Loc { filename: intern("*bootstrap*"), line: 0 },
    )
}

/// Run kati: bootstrap + command-line vars + parse the Makefile + dependency
/// analysis + generate the ninja graph into `ninja_path` (our memfd path). This
/// is a faithful port of upstream kati main.rs `run()` restricted to the
/// generate-ninja branch (the only mode sarun uses). Returns Ok on success.
fn run_kati(targets: &[Symbol], cl_vars: &[bytes::Bytes], ninja_path: &OsStr) -> anyhow::Result<()> {
    let start_time = std::time::SystemTime::now();
    let mut ev = Evaluator::new();
    ev.start()?;

    // MAKEFILE_LIST + environment, like upstream.
    let mut makefile_list = BytesMut::new();
    makefile_list.put_u8(b' ');
    makefile_list.put_slice(FLAGS.makefile.lock().clone().unwrap().as_bytes());
    intern("MAKEFILE_LIST").set_global_var(
        Variable::with_simple_string(
            makefile_list.freeze(),
            VarOrigin::File,
            Some(ev.current_frame()),
            ev.loc.clone(),
        ),
        false,
        None,
    )?;
    for (k, v) in std::env::vars_os() {
        let v = bytes::Bytes::from(v.as_bytes().to_vec());
        let val = Arc::new(Value::Literal(None, v.clone()));
        intern(k.as_bytes().to_vec()).set_global_var(
            Variable::new_recursive(val, VarOrigin::Environment, Some(ev.current_frame()), None, v),
            false,
            None,
        )?;
    }

    let bootstrap_asts = read_bootstrap_makefile(targets)?;
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
        let makefile = FLAGS.makefile.lock().clone().unwrap();
        let _file_frame = ev.enter(FrameType::Parse, bytes::Bytes::from(makefile.as_bytes().to_vec()), Loc::default());
        let Some(mk) = kati::file_cache::get_makefile(&makefile)? else {
            anyhow::bail!("makefile not found: {}", makefile.to_string_lossy());
        };
        let stmts = mk.stmts.lock();
        for stmt in stmts.iter() {
            stmt.eval(&mut ev)?;
        }
    }

    let nodes: Vec<NamedDepNode>;
    {
        let _frame = ev.enter(FrameType::Phase, bytes::Bytes::from_static(b"*dependency analysis*"), Loc::default());
        nodes = make_dep(&mut ev, targets.to_owned())?;
    }

    {
        let _frame = ev.enter(FrameType::Phase, bytes::Bytes::from_static(b"*ninja generation*"), Loc::default());
        // sarun: emit ONLY the ninja, to our memfd path (no shell/stamp, no disk).
        generate_ninja_to_path(&nodes, &mut ev, start_time, ninja_path)?;
        ev.finish()?;
    }
    Ok(())
}

/// Walk the n2 graph and emit ONE `build_edges` control frame carrying every
/// edge {outs, ins, cmd} — including up-to-date targets n2 will skip. Identical
/// shape to Phase 1's ninja emit_build_edges, so the same reader/table apply.
fn emit_build_edges(graph: &n2::graph::Graph) {
    let mut edges = vec![];
    for build in graph.builds.iter() {
        let outs: Vec<String> = build.outs().iter().map(|&id| graph.file(id).name.clone()).collect();
        let ins: Vec<String> = build.explicit_ins().iter().map(|&id| graph.file(id).name.clone()).collect();
        edges.push(json!({"outs": outs, "ins": ins, "cmd": build.cmdline.clone()}));
    }
    let msg = json!({"type": "build_edges", "edges": edges});
    crate::runner::send_nested_prov(format!("{msg}\n").as_bytes());
}

/// The embedded-make entrypoint. `argv` is the FULL process argv (argv[0] is
/// `make`/`gmake`). Returns the process exit code.
pub fn make_main(argv: &[String]) -> i32 {
    // 1. Install the in-process recipe executor so n2 runs recipes through brush
    //    (NEVER posix_spawns /bin/sh). Shared with Phase 1. Idempotent.
    n2::process::set_executor(crate::brush::n2_executor);

    // 2. Honour `-C dir` ourselves up front so kati's chdir, the makefile lookup
    //    and n2's cwd all agree (kati also chdir's on -C; set_current_dir is
    //    idempotent for the same dir). kati's working_dir is applied inside its
    //    flags parse, but doing it here keeps emit_build_edges/n2 consistent.
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

    // 3. Synthesize + install the kati argv (forces --ninja). MUST happen before
    //    any FLAGS access. A refused flag is a visible error, no fallback.
    let kargv = match kati_argv(argv) {
        Ok(v) => v,
        Err(msg) => { eprintln!("{msg}"); return 2; }
    };
    if kati::flags::install_args(kargv).is_err() {
        eprintln!("sarun-engine make: kati flags already initialized (internal error)");
        return 2;
    }

    // 4. kati needs a makefile; if none was given on the argv, discover the
    //    default like real make/kati (GNUmakefile / makefile / Makefile).
    if FLAGS.makefile.lock().is_none() {
        let mut mf = FLAGS.makefile.lock();
        for cand in ["GNUmakefile", "makefile", "Makefile"] {
            if std::fs::metadata(cand).is_ok() { *mf = Some(OsString::from(cand)); break; }
        }
        if mf.is_none() {
            drop(mf);
            eprintln!("sarun-engine make: no makefile found (and none given with -f)");
            return 2;
        }
    }

    // 5. Create the memfd the ninja graph is written into, and hand kati its
    //    /proc/self/fd/<memfd> path. NO disk temp file (user-mandated).
    // SAFETY: memfd_create with a valid NUL name + MFD_CLOEXEC.
    let memfd = unsafe { libc::memfd_create(c"sarun-make-ninja".as_ptr(), libc::MFD_CLOEXEC) };
    if memfd < 0 {
        eprintln!("sarun-engine make: memfd_create failed: {}", std::io::Error::last_os_error());
        return 127;
    }
    let ninja_path = OsString::from(format!("/proc/self/fd/{memfd}"));

    // 6. Run kati → ninja graph into the memfd.
    let targets: Vec<Symbol> = FLAGS.targets.clone();
    let cl_vars: Vec<bytes::Bytes> = FLAGS.cl_vars.clone();
    if let Err(e) = run_kati(&targets, &cl_vars, &ninja_path) {
        for cause in e.chain() {
            eprintln!("{cause}");
        }
        // SAFETY: closing our owned memfd.
        unsafe { libc::close(memfd); }
        return 1;
    }

    // 7. Read the generated ninja back out of the memfd (lseek 0 + read all).
    //    We wrap the fd in a File for convenience; into_raw_fd keeps it open for
    //    the close below (File::from_raw_fd would otherwise close on drop).
    let ninja_src = {
        // SAFETY: memfd is a live owned fd we created.
        let mut f = unsafe { std::fs::File::from_raw_fd(memfd) };
        let mut buf = Vec::new();
        let read_res = f.seek(std::io::SeekFrom::Start(0)).and_then(|_| f.read_to_end(&mut buf));
        // f drops here → closes memfd → the in-memory file is freed (no cleanup).
        match read_res {
            Ok(_) => buf,
            Err(e) => {
                eprintln!("sarun-engine make: reading generated ninja from memfd failed: {e}");
                return 1;
            }
        }
    };

    // 8. Parse the ninja IN MEMORY into a runnable n2 State (memfd-backed db, no
    //    .n2_db on disk), emit build_edges, then run n2 serial through brush.
    let state = match n2::load::read_from_content("<kati-ninja>", ninja_src) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sarun-engine make: n2 could not load the kati-generated ninja: {e:#}");
            return 1;
        }
    };
    emit_build_edges(&state.graph);

    // n2's targets are NAMES; the box's make targets are the same names kati put
    // in the graph as outputs. Pass them through so `make foo` builds foo's edge.
    let n2_targets: Vec<String> = targets.iter().map(|s| String::from_utf8_lossy(&s.as_bytes()).into_owned()).collect();
    match n2::run::run_state(state, &n2_targets) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sarun-engine make: build failed: {e:#}");
            1
        }
    }
}
