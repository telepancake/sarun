// n2/ninja embedded build (Phase 1). When a -b brush box runs `ninja`, the
// runner shadows /bin/ninja and /usr/bin/ninja with the ENGINE binary (gated on
// SARUN_BRUSH_SH=1, same as the /bin/sh shadow). main() detects that BEFORE its
// normal dispatch (argv[0] basename == "ninja" && SARUN_BRUSH_SH=1) and lands
// here, which:
//   1. installs the in-process recipe executor (brush::n2_executor) into the
//      vendored n2 — so n2 NEVER posix_spawns /bin/sh; every recipe runs through
//      embedded brush in THIS process (no fork, no engine re-exec);
//   2. loads the box's REAL build.ninja from the overlay (n2::load::read — no
//      temp file) and emits a `build_edges` provenance frame capturing EVERY
//      edge (outs/ins/cmd), INCLUDING up-to-date targets that never execute;
//   3. runs the vendored n2 (`n2::run::run`) which honours the box's ninja argv
//      (-f/-C/targets/…) and is FORCED to -j1 (serial) in embedded mode.
//
// build.ninja resolution mirrors n2's own default: -f FILE if present on the
// box argv, else "build.ninja" relative to -C dir (or cwd). We replicate that
// minimal arg scan ONLY to find the file for the build_edges read; the actual
// build re-parses identically inside n2::run.

use serde_json::json;

/// True when this engine invocation should act as the embedded-ninja entry:
/// SARUN_BRUSH_SH=1 (a -b brush box) AND argv[0]'s basename is `ninja`.
pub fn is_ninja_invocation() -> bool {
    if std::env::var("SARUN_BRUSH_SH").as_deref() != Ok("1") {
        return false;
    }
    let arg0 = std::env::args().next().unwrap_or_default();
    let base = std::path::Path::new(&arg0)
        .file_name().and_then(|s| s.to_str()).unwrap_or("");
    base == "ninja"
}

/// Find the build file the box's ninja argv selects, applying any `-C dir`
/// chdir first (n2 itself chdir's on -C; we must match so a relative -f and the
/// default "build.ninja" resolve identically). Returns the path to read for the
/// build_edges graph walk. Mutates cwd via -C exactly as n2 will.
fn resolve_build_file(argv: &[String]) -> String {
    let mut filename = String::from("build.ninja");
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-C" => {
                if let Some(dir) = argv.get(i + 1) {
                    let _ = std::env::set_current_dir(dir);
                }
                i += 2;
            }
            "-f" => {
                if let Some(f) = argv.get(i + 1) { filename = f.clone(); }
                i += 2;
            }
            _ => i += 1,
        }
    }
    filename
}

/// Walk the loaded graph and emit ONE `build_edges` control message carrying
/// every edge: {outs, ins, cmd}. This captures the FULL parsed graph, including
/// up-to-date targets n2 will skip executing — the point of the frame. Phony
/// edges (cmdline == None) are included with cmd == null. Best-effort: a read
/// error or unresolvable box is swallowed (the build proceeds regardless).
fn emit_build_edges(filename: &str) {
    let state = match n2::load::read(filename) {
        Ok(s) => s,
        Err(_) => return, // n2::run will report the load error to the user
    };
    let graph = &state.graph;
    let mut edges = vec![];
    for build in graph.builds.iter() {
        let outs: Vec<String> = build.outs()
            .iter().map(|&id| graph.file(id).name.clone()).collect();
        let ins: Vec<String> = build.explicit_ins()
            .iter().map(|&id| graph.file(id).name.clone()).collect();
        edges.push(json!({
            "outs": outs,
            "ins": ins,
            "cmd": build.cmdline.clone(),
        }));
    }
    let msg = json!({"type": "build_edges", "edges": edges});
    crate::runner::send_nested_prov(format!("{msg}\n").as_bytes());
}

/// The embedded-ninja entrypoint. `argv` is the FULL process argv (argv[0] is
/// `ninja`). Returns the process exit code.
pub fn n2_main(argv: &[String]) -> i32 {
    // 1. Install the in-process executor so n2 runs recipes through brush and
    //    NEVER posix_spawns /bin/sh. Idempotent (OnceLock).
    n2::process::set_executor(crate::brush::n2_executor);

    // 2. Emit build_edges from the REAL build.ninja in the overlay. resolve_*
    //    applies -C so the path resolves exactly as n2::run will (n2 re-applies
    //    -C itself; set_current_dir is idempotent for the same dir).
    let build_file = resolve_build_file(&argv[1..]);
    emit_build_edges(&build_file);

    // 3. Run the vendored n2. It reads the box's ninja argv from std::env::args
    //    (we ARE the ninja process), forces -j1 in embedded mode (run.rs), and
    //    suppresses its own SIGINT handler (signal.rs). A non-zero / Err result
    //    is the build failure exit.
    match n2::run::run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ninja (sarun embedded n2): {e:#}");
            1
        }
    }
}

/// In-process `ninja` brush builtin entry — the n2 analogue of katirun's
/// make_builtin. Dispatched when brush runs `ninja` (a recipe's `ninja`, or a
/// cmake/configure step), so it stays in THIS process. Drives n2 via the
/// already-`pub` in-memory entries (`load::read` + `run_state`) instead of
/// `n2::run::run()` (which reads the PROCESS argv).
///
/// Logical cwd: rather than chdir the engine, the build dir is threaded into n2
/// as a thread-local (`n2::graph::set_cwd`) that every filesystem touch resolves
/// relative paths against (stat, build-file/depfile reads, .n2_db, rspfiles,
/// output dirs). The same dir is set as the recipe cwd (`BOX_RECIPE_CWD`) so a
/// recipe's commands run through brush at the build dir. A `-C <dir>` shifts the
/// build dir; relative `-C` joins onto `base_cwd` (the brush context's dir).
pub fn ninja_builtin(
    argv: &[String],
    base_cwd: &std::path::Path,
    mut out: impl std::io::Write,
    mut err: impl std::io::Write,
) -> i32 {
    n2::process::set_executor(crate::brush::n2_executor);

    // Per-build jobserver. ninja is parallel by default (CPU count), overridable
    // by -jN. `ensure` is idempotent: if a parallel `make` already created the
    // pool, this ninja inherits and shares it rather than opening a second one.
    crate::jobserver::request_jobs(
        crate::jobserver::explicit_jobs(argv).unwrap_or_else(crate::jobserver::cpu_count),
    );
    crate::jobserver::ensure();

    let mut build_file = String::from("build.ninja");
    let mut targets: Vec<String> = Vec::new();
    // The logical build dir, shifted by each `-C` (relative ones chain, exactly
    // as real ninja applies repeated -C left to right).
    let mut build_dir = base_cwd.to_path_buf();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--version" => {
                // CMake gates on a Ninja version; match n2's fake_ninja_compat.
                let _ = writeln!(out, "1.10.2");
                return 0;
            }
            "-f" => {
                if let Some(f) = argv.get(i + 1) {
                    build_file = f.clone();
                }
                i += 2;
            }
            "-C" => {
                if let Some(dir) = argv.get(i + 1) {
                    build_dir = build_dir.join(dir);
                }
                i += 2;
            }
            // Flags that take a value: skip the value too (best-effort).
            "-j" | "-k" | "-l" | "-d" | "-t" | "-w" => i += 2,
            s if s.starts_with('-') => i += 1,
            s => {
                targets.push(s.to_string());
                i += 1;
            }
        }
    }

    // Thread the build dir into n2 so every filesystem touch (stat, build-file
    // and depfile reads, .n2_db, rspfiles, output dirs) resolves against it
    // without the engine chdir'ing. n2 carries this onto each recipe's worker
    // thread, and n2_executor derives the recipe's run cwd from it. Saved and
    // restored so a nested or sibling build is unaffected.
    let prev_cwd = n2::graph::set_cwd(Some(build_dir.clone()));

    // build_file resolves against build_dir via n2's now-cwd-aware reads, so
    // pass it as-is (relative or absolute).
    emit_build_edges(&build_file);

    let result = match n2::load::read(&build_file) {
        Ok(state) => n2::run::run_state(state, &targets),
        Err(e) => Err(e),
    };

    n2::graph::set_cwd(prev_cwd);

    let code = match result {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "ninja: {e:#}");
            1
        }
    };
    let _ = out.flush();
    code
}
