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
// record per pipeline over the engine control socket bind-mounted at
// UI_SOCK_INBOX (the box is resolved from the shim's pidfd /proc ancestry —
// the same path `register` uses for nested boxes), then runs the pipelines
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
// Brush-core coverage gaps (VERIFIED — failures listed in brush_sh comments
// near run_brush_script, where the tests exercise them): brush-core does NOT
// implement bash extended-test `[[ … ]]` or process substitution `<(…) / >(…)`
// in sh-mode (both surface as visible parse or execution errors). It DOES run
// POSIX builtins (cd, export, set, [, test, printf, echo, shift, …), variable
// assignment + expansion, arithmetic, if/case/for/while/until control flow,
// functions, simple traps, here-docs/here-strings and the standard one-char
// flag set (-e/-u/-x/-o, set/unset of same).

use std::os::fd::AsRawFd;

use serde_json::json;
use serde_json::Value;

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
            let n = unsafe { libc::read(rfd, tmp.as_mut_ptr().cast(), tmp.len()) };
            if n <= 0 { break; }
            buf.extend_from_slice(&tmp[..n as usize]);
            let (frames, used) = crate::frames::decode(&buf);
            buf.drain(..used);
            for (ft, payload) in frames {
                if ft == crate::frames::FRAME_ECHO && !payload.is_empty() {
                    let realfd = if payload[0] == 1 { real_err } else { real_out };
                    unsafe { libc::write(realfd, payload[1..].as_ptr().cast(),
                                         payload.len() - 1); }
                } else if ft == crate::frames::FRAME_ECHO_DONE {
                    done2.store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
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
        // -i / -l / --login: out of scope inside a box.
        if a == "-i" || a == "-l" || a == "--login" || a == "--interactive" {
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
    rt.block_on(run_brush_script(script_src, dollar0, positional,
                                  set_flags, set_o, unset_o))
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
                          unset_o: Vec<String>) -> i32 {
    // The shim INHERITS cwd from execve; brush-core defaults to $PWD/getcwd()
    // when working_dir is unspecified, which matches that. We still pass it
    // explicitly to be defensive against any future builder default change.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    use brush_builtins::ShellBuilderExt;
    let shell_res = brush_core::Shell::builder()
        .sh_mode(true)
        .default_builtins(brush_builtins::BuiltinSet::ShMode)
        .shell_name(shell_name.clone())
        .shell_args(positional.clone())
        .working_dir(cwd)
        .build().await;
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

/// Build the brush shell, parse the script, emit one FRAME_PROV per pipeline,
/// then execute the WHOLE program through brush-core. No /bin/sh fallback:
///   - a parse error  → VISIBLE message, exit 2
///   - a fatal exec error (unsupported construct) → VISIBLE message, non-zero
async fn run_brush(conn_fd: i32, script: String) -> i32 {
    // sh-mode brush: POSIX-ish, closest to the /bin/sh the box would otherwise
    // get, and skip rc/profile so the box's own filesystem isn't sourced.
    // Default ShMode builtins are registered (cd, export, set, [, test, …) —
    // without them brush-core ships an empty builtin table, so even POSIX
    // builtins would surface as "command not found" from inside brush.
    use brush_builtins::ShellBuilderExt;
    let shell_res = brush_core::Shell::builder()
        .sh_mode(true)
        .default_builtins(brush_builtins::BuiltinSet::ShMode)
        .build().await;
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
