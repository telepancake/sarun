// Embedded Ninja driven by Bumba's n2 fork. The graph is loaded and scheduled
// in-process, recipes run through Brush instead of `/bin/sh`, and structured
// graph/edge/output events are emitted through EventSink.

/// True when this process carries the projected `ninja` identity.
/// The identity cannot depend on inherited environment: sanitized build
/// environments must keep executing the same projection.
pub fn is_ninja_invocation() -> bool {
    let arg0 = std::env::args().next().unwrap_or_default();
    let base = std::path::Path::new(&arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    base == "ninja"
}

/// Walk the loaded graph and emit ONE `build_edges` control message carrying
/// every edge: {outs, ins, cmd}. This captures the FULL parsed graph, including
/// up-to-date targets n2 will skip executing — the point of the frame. Phony
/// edges (cmdline == None) are included with cmd == null. A graph read error is
/// swallowed here because n2 reports the authoritative error during execution.
fn emit_build_edges(filename: &str) {
    let state = match n2::load::read(filename) {
        Ok(s) => s,
        Err(_) => return, // n2::run will report the load error to the user
    };
    let graph = &state.graph;
    let mut edges = Vec::new();
    for build in graph.builds.iter() {
        let outs: Vec<String> = build
            .outs()
            .iter()
            .map(|&id| graph.file(id).name.clone())
            .collect();
        let ins: Vec<String> = build
            .explicit_ins()
            .iter()
            .map(|&id| graph.file(id).name.clone())
            .collect();
        edges.push(crate::BuildEdge {
            outputs: outs,
            inputs: ins,
            command: build.cmdline.clone(),
        });
    }
    crate::event::emit(crate::Event::BuildGraph {
        tool: "ninja".into(),
        edges,
    });
}

/// The embedded-ninja entrypoint. `argv` is the FULL process argv (argv[0] is
/// `ninja`). Returns the process exit code.
pub fn n2_main(argv: &[String]) -> i32 {
    n2_main_with_executor(argv, crate::shell::ninja_executor)
}

pub fn n2_main_with_executor(argv: &[String], executor: n2::process::Executor) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    ninja_builtin_with_executor(argv, &cwd, std::io::stdout(), std::io::stderr(), executor)
}

/// In-process `ninja` brush builtin entry — the n2 analogue of katirun's
/// make_builtin. Dispatched when brush runs `ninja` (a recipe's `ninja`, or a
/// cmake/configure step), so it stays in THIS process. Drives n2 via the
/// already-`pub` in-memory entries (`load::read` + `run_state`) instead of
/// `n2::run::run()` (which reads the PROCESS argv).
///
/// Logical cwd: rather than chdir the process, the build dir is threaded into n2
/// as a thread-local (`n2::graph::set_cwd`) that every filesystem touch resolves
/// relative paths against (stat, build-file/depfile reads, .n2_db, rspfiles,
/// output dirs). The same dir is set as the recipe cwd (`BOX_RECIPE_CWD`) so a
/// recipe's commands run through brush at the build dir. A `-C <dir>` shifts the
/// build dir; relative `-C` joins onto `base_cwd` (the brush context's dir).
pub fn ninja_builtin(
    argv: &[String],
    base_cwd: &std::path::Path,
    out: impl std::io::Write,
    err: impl std::io::Write,
) -> i32 {
    ninja_builtin_with_executor(argv, base_cwd, out, err, crate::shell::ninja_executor)
}

pub fn ninja_builtin_with_executor(
    argv: &[String],
    base_cwd: &std::path::Path,
    mut out: impl std::io::Write,
    mut err: impl std::io::Write,
    executor: n2::process::Executor,
) -> i32 {
    n2::process::set_executor(executor);

    // Advertise the host-global slip pool into MAKEFLAGS for this build. ninja
    // is parallel by default (CPU count), overridable by -jN; that count is the
    // LOCAL runner cap (n2::jobserver::jobs_hint), while the machine-wide pool
    // does the real bounding. Idempotent: a ninja under a parallel make inherits
    // that make's advertisement and shares the same pool.
    crate::jobserver::advertise(
        crate::jobserver::explicit_jobs(argv).unwrap_or_else(crate::jobserver::cpu_count),
    );

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
    // without the process chdir'ing. n2 carries this onto each recipe's worker
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
