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
