/*
Copyright 2025 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{
    collections::HashMap,
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use anyhow::Result;
use bytes::Bytes;
use parking_lot::Mutex;

use crate::{
    command::CommandEvaluator,
    dep::{DepNode, NamedDepNode},
    error,
    eval::{Evaluator, FrameType},
    expr::Evaluable,
    fileutil::{RedirectStderr, get_timestamp, run_command, run_with_installed_runner},
    flags::FLAGS,
    symtab::Symbol,
    warn,
};

thread_local! {
    // sarun: when set, recipe stdout (and $(info)) is written HERE instead of
    // process stdout; RECIPE_ERR likewise for recipe-failure lines, warnings and
    // $(warning). An in-process `make` builtin sets these to writers over its
    // brush ExecutionContext's fd 1/2, so a recursive/nested make's output flows
    // up the brush pipe chain rather than escaping to the real terminal. Default
    // None → the shadow/standalone path uses process stdout/stderr exactly as
    // before. kati runs a make synchronously on one thread, so thread-local is
    // the correct scope (concurrent makes are on other threads).
    static RECIPE_OUT: std::cell::RefCell<Option<Box<dyn std::io::Write>>> =
        const { std::cell::RefCell::new(None) };
    static RECIPE_ERR: std::cell::RefCell<Option<Box<dyn std::io::Write>>> =
        const { std::cell::RefCell::new(None) };
}

/// sarun: install (or clear) the thread-local recipe-stdout sink, returning the
/// previous value so a nested make can save/restore it. Pass None to reset.
pub fn set_recipe_out(
    w: Option<Box<dyn std::io::Write>>,
) -> Option<Box<dyn std::io::Write>> {
    RECIPE_OUT.with(|c| std::mem::replace(&mut *c.borrow_mut(), w))
}

/// sarun: install (or clear) the thread-local recipe-stderr/diagnostics sink.
pub fn set_recipe_err(
    w: Option<Box<dyn std::io::Write>>,
) -> Option<Box<dyn std::io::Write>> {
    RECIPE_ERR.with(|c| std::mem::replace(&mut *c.borrow_mut(), w))
}

/// sarun: emit to the thread-local stdout sink if a make builtin installed one,
/// else to process stdout (unchanged default). Used for recipe stdout + $(info).
pub(crate) fn emit_recipe_output(output: &[u8]) {
    RECIPE_OUT.with(|c| {
        let mut slot = c.borrow_mut();
        if let Some(w) = slot.as_mut() {
            use std::io::Write;
            let _ = w.write_all(output);
            let _ = w.flush();
        } else {
            print!("{}", String::from_utf8_lossy(output));
        }
    });
}

/// sarun: emit a diagnostic line (recipe-failure, warning, $(warning)) to the
/// thread-local stderr sink if set, else process stderr. A trailing newline is
/// appended (callers pass an unterminated line, matching eprintln!).
pub fn emit_recipe_err(line: &str) {
    RECIPE_ERR.with(|c| {
        let mut slot = c.borrow_mut();
        if let Some(w) = slot.as_mut() {
            use std::io::Write;
            let _ = w.write_all(line.as_bytes());
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        } else {
            eprintln!("{line}");
        }
    });
}

/// sarun: a recipe failed. Propagated (instead of the old `std::process::exit`)
/// so the in-process make builtin doesn't kill the whole engine process — it
/// unwinds to make_main/make_builtin, which return `code`. The user-facing
/// `*** [target] Error N` line is emitted (via emit_recipe_err) before this is
/// returned, so callers must NOT re-print it.
#[derive(Debug, Clone, Copy)]
pub struct BuildFailed(pub i32);

impl std::fmt::Display for BuildFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "build failed (exit {})", self.0)
    }
}

impl std::error::Error for BuildFailed {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExecStatus {
    Processing,
    Timestamp(Option<SystemTime>),
}

impl PartialOrd for ExecStatus {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (ExecStatus::Processing, ExecStatus::Processing) => Some(std::cmp::Ordering::Equal),
            (ExecStatus::Processing, ExecStatus::Timestamp(Some(_))) => {
                Some(std::cmp::Ordering::Less)
            }
            (ExecStatus::Timestamp(None), ExecStatus::Timestamp(None)) => {
                Some(std::cmp::Ordering::Equal)
            }
            (ExecStatus::Timestamp(None), _) => Some(std::cmp::Ordering::Less),
            (_, ExecStatus::Timestamp(None)) => Some(std::cmp::Ordering::Greater),
            (ExecStatus::Timestamp(Some(a)), ExecStatus::Timestamp(Some(b))) => Some(a.cmp(b)),
            (ExecStatus::Timestamp(Some(_)), _) => Some(std::cmp::Ordering::Greater),
        }
    }
}

struct Executor<'a> {
    ce: CommandEvaluator<'a>,
    shell: Bytes,
    shellflag: &'static [u8],
    num_commands: u64,
    /// Suppress the "*** [target] Error N" banner (and no-rule notes) —
    /// used when remaking OPTIONAL includes, whose failures GNU swallows
    /// silently.
    quiet_failures: bool,
}

// ── Parallel scheduler ───────────────────────────────────────────────────────
//
// sarun: kati's executor walks the dep DAG and runs each target's recipe. The
// recursive `exec_node` below does that serially. The scheduler here is the
// SAME engine driven by a dependency-count ready-queue instead of recursion, so
// independent targets' recipes can run concurrently — in parallel brush
// subshells — bounded by the engine slip pool. The Evaluator is not Send, so the
// make thread keeps it and does ALL walking + command EVALUATION; only the
// concrete command strings are dispatched to worker threads.
//
// Conformance: every node is given its DFS POST-ORDER rank (the order the
// recursion would finish it), and ready nodes are selected lowest-rank-first.
// At cap=1 that reproduces the recursion's exact execution order — so the serial
// corpus is unaffected — while cap>1 fans out independent ready nodes.

#[derive(PartialEq, Eq)]
enum PState {
    Pending,
    Done,
}

/// A ready node, ordered for a MIN-heap by DFS post-order rank (ranks are unique,
/// so the Symbol never participates in the comparison — it need not be Ord).
#[derive(PartialEq, Eq)]
struct Ready {
    rank: usize,
    sym: Symbol,
}
impl Ord for Ready {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reversed so BinaryHeap (a max-heap) yields the lowest rank first.
        other.rank.cmp(&self.rank)
    }
}
impl PartialOrd for Ready {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct PNode {
    node: Arc<Mutex<DepNode>>,
    /// Build-deps: regular deps + order-only deps whose file is absent, by output.
    deps: Vec<Symbol>,
    dependents: Vec<Symbol>,
    unfinished: usize,
    /// DFS post-order rank (selection order; cap=1 ⇒ recursion order).
    rank: usize,
    /// Set once the node completes (its file's pre-run timestamp, matching
    /// exec_node's recorded ExecStatus).
    result: Option<ExecStatus>,
    state: PState,
}

/// A node's concrete recipe, ready to run on a worker (no Evaluator needed).
struct RunReq {
    output: Symbol,
    commands: Vec<crate::command::Command>,
    /// Target-specific exported vars (`target: export VAR := …`), applied to each
    /// command's subshell as a non-echoed `export` prefix. Pre-evaluated by the
    /// make thread (process env can't be used — parallel recipes would race it).
    exports: Vec<(Bytes, Bytes)>,
    /// The make's working_dir (path bytes), passed to the in-process runner so a
    /// recipe runs at the right cwd on its worker thread (explicit, not via a
    /// make-thread thread-local the worker wouldn't see).
    cwd: Bytes,
    /// sarun: the make's exported-variable prefix (`export NAME='val'` / `unset
    /// NAME` lines), applied to each command's subshell as a non-echoed prefix so
    /// the make's `export`s reach recipes (and recursive sub-makes) through the
    /// per-subshell env rather than a process-global `std::env` write. Empty for
    /// the standalone rkati path (Evaluator::box_export_prefix is unset there).
    box_prefix: Bytes,
    /// The pre-run output timestamp to record as this node's result on success.
    result_ts: ExecStatus,
}

/// Build the `export NAME='value'` prefix (newline-terminated) for a target's
/// exported vars, single-quote-escaping the values. Applied to each command's
/// run input but never echoed.
fn exports_prefix(exports: &[(Bytes, Bytes)]) -> Vec<u8> {
    let mut p = Vec::new();
    for (name, val) in exports {
        p.extend_from_slice(b"export ");
        p.extend_from_slice(name);
        p.extend_from_slice(b"='");
        for &b in val.iter() {
            if b == b'\'' {
                p.extend_from_slice(b"'\\''");
            } else {
                p.push(b);
            }
        }
        p.extend_from_slice(b"'\n");
    }
    p
}

/// What a worker produced running a node's recipe.
struct NodeRun {
    output: Symbol,
    ignored: Vec<(Symbol, i32)>,
    failure: Option<(Symbol, i32)>,
    result_ts: ExecStatus,
}

/// sarun: a worker→main message. Recipe output is streamed LIVE as `Chunk`s (so
/// a long-running or hung recipe's output appears immediately, not buffered
/// until the node finishes), then one `Done` carries the node's result. At cap 1
/// (serial) the byte order is identical to collecting-then-emitting, so the
/// rkati↔make corpus is unaffected; under -j output interleaves per chunk, like
/// GNU make without -O.
enum RunMsg {
    Chunk(Vec<u8>),
    Done(NodeRun, Option<u8>),
}

/// Run a node's concrete commands in sequence on a worker thread, capturing the
/// merged output. Stops at the first non-ignored failure. Needs no Evaluator.
fn run_node_commands(
    shell: &[u8],
    shellflag: &'static [u8],
    req: RunReq,
    emit: &mut dyn FnMut(&[u8]),
) -> NodeRun {
    let mut ignored = Vec::new();
    let mut failure = None;
    // The make's global export prefix (box mode) runs first, then the target's
    // own exported rule vars, then the command. Both are applied to the subshell
    // but never echoed.
    //
    // sarun: ONLY when SHELL is a POSIX sh — the prefix is `export …` shell
    // text. With a custom SHELL (e.g. the corpus's `SHELL=/usr/bin/printf`),
    // that text would be pasted into the command handed to the custom program
    // (visibly corrupting it); GNU make passes exports to such recipes through
    // the child ENVIRONMENT, which the fork path inherits from the process
    // anyway.
    let shell_base = std::path::Path::new(OsStr::from_bytes(shell))
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let posix_shell = matches!(shell_base, "sh" | "bash" | "dash" | "ash" | "ksh" | "zsh");
    let mut prefix = if posix_shell { req.box_prefix.to_vec() } else { Vec::new() };
    if posix_shell {
        prefix.extend_from_slice(&exports_prefix(&req.exports));
    }
    let cwd = req.cwd;
    // sarun: report this edge's run-state to the engine so the targets pane can
    // show only the targets currently building (and their wall time). Only edges
    // that actually run a recipe are reported — dry-run and command-less (phony /
    // up-to-date) nodes are correctly left un-started. `req.output` is the node's
    // primary output (== build_edges.outs[0], the engine's match key).
    let output = req.output;
    let report = !FLAGS.is_dry_run && !req.commands.is_empty();
    if report {
        crate::fileutil::report_edge(output.as_bytes().as_ref(),
                                     crate::fileutil::EdgePhase::Start, 0, b"");
    }
    // Keep the TAIL ~1KB of the recipe's merged output for the edge row's
    // excerpt — on failure the error text is at the end of the stream.
    let excerpt = std::cell::RefCell::new(Vec::<u8>::new());
    let mut emit = |b: &[u8]| {
        {
            let mut x = excerpt.borrow_mut();
            x.extend_from_slice(b);
            let overflow = x.len().saturating_sub(1024);
            if overflow > 0 { x.drain(..overflow); }
        }
        emit(b)
    };
    let emit = &mut emit as &mut dyn FnMut(&[u8]);
    for command in req.commands {
        if command.echo {
            // Echo LIVE, before running, so a hung command's line shows at once.
            emit(&command.cmd);
            emit(b"\n");
        }
        if FLAGS.is_dry_run {
            continue;
        }
        // In-process runner gets the exports prefix SEPARATELY (it applies it
        // to the recipe's shell without recording it as pipeline provenance);
        // the fork fallback prepends it to the input as before.
        let (ok, code) =
            if let Some(code) = run_with_installed_runner(
                shell, shellflag, &prefix, &command.cmd, &cwd,
                RedirectStderr::Stdout, emit)
            {
                (code == 0, code)
            } else {
                let run_input: Bytes = if prefix.is_empty() {
                    command.cmd.clone()
                } else {
                    let mut v = prefix.clone();
                    v.extend_from_slice(&command.cmd);
                    Bytes::from(v)
                };
                match run_command(shell, shellflag, &run_input, &cwd, RedirectStderr::Stdout) {
                    Ok((status, o)) => { emit(&o); (status.success(), status.code().unwrap_or(1)) }
                    Err(e) => { emit(format!("{e}\n").as_bytes()); (false, 1) }
                }
            };
        if !ok {
            if command.ignore_error {
                ignored.push((command.output, code));
            } else {
                failure = Some((command.output, code));
                break;
            }
        }
    }
    if report {
        let code = failure.map(|(_, c)| c).unwrap_or(0);
        crate::fileutil::report_edge(output.as_bytes().as_ref(),
                                     crate::fileutil::EdgePhase::Done, code,
                                     &excerpt.borrow());
    }
    NodeRun { output, ignored, failure, result_ts: req.result_ts }
}

impl<'a> Executor<'a> {
    fn new(ev: &'a mut Evaluator) -> Result<Self> {
        let shell = ev.get_shell()?;
        let shellflag = ev.get_shell_flag();
        Ok(Executor {
            ce: CommandEvaluator::new(ev)?,
            shell,
            shellflag,
            num_commands: 0,
            quiet_failures: false,
        })
    }


    /// Discover all reachable nodes in exec_node's child order (order-only deps
    /// whose file exists are skipped; a back-edge to an in-progress node is
    /// dropped, matching the recursion's circular-dependency drop), assigning
    /// each its DFS post-order rank and its build-deps.
    fn discover(
        &self,
        n: &Arc<Mutex<DepNode>>,
        graph: &mut HashMap<Symbol, PNode>,
        visiting: &mut std::collections::HashSet<Symbol>,
        rank: &mut usize,
    ) {
        let sym = n.lock().output;
        if graph.contains_key(&sym) {
            return;
        }
        visiting.insert(sym);
        let (order, deps) = {
            let g = n.lock();
            (g.order_onlys.clone(), g.deps.clone())
        };
        let mut build_deps: Vec<Symbol> = Vec::new();
        for (_, d) in order {
            let dout = d.lock().output;
            // sarun: resolve the existence probe against the make's logical
            // working_dir, NOT the process cwd — an in-process sub-make's cwd
            // differs (`make -C sub` in a box), and a same-named file in the
            // process cwd made the order-only prerequisite look already-built,
            // so it was silently never generated (its consumer then failed —
            // under -j immediately, serially whenever no other edge built it).
            let dbytes = dout.as_bytes();
            let dpath = Path::new(OsStr::from_bytes(&dbytes));
            let dpath = if dpath.is_absolute() {
                dpath.to_path_buf()
            } else {
                self.ce.ev.working_dir.join(dpath)
            };
            if std::fs::exists(&dpath).unwrap_or(false) {
                continue;
            }
            if visiting.contains(&dout) {
                // back-edge: drop it (matches exec_node's Processing-node drop),
                // emitting the same warning so output is identical to serial.
                warn!("Circular {sym} <- {dout} dependency dropped.");
                continue;
            }
            build_deps.push(dout);
            self.discover(&d, graph, visiting, rank);
        }
        for (_, d) in deps {
            let dout = d.lock().output;
            if visiting.contains(&dout) {
                warn!("Circular {sym} <- {dout} dependency dropped.");
                continue;
            }
            build_deps.push(dout);
            self.discover(&d, graph, visiting, rank);
        }
        let r = *rank;
        *rank += 1;
        visiting.remove(&sym);
        graph.insert(
            sym,
            PNode {
                node: n.clone(),
                deps: build_deps,
                dependents: Vec::new(),
                unfinished: 0,
                rank: r,
                result: None,
                state: PState::Pending,
            },
        );
    }

    /// Evaluate a ready node (make thread): compute its timestamp + up-to-date
    /// status from its now-complete deps, and either mark it done (no run) or
    /// produce its concrete recipe for dispatch. Mirrors exec_node's per-node
    /// logic. Returns (result timestamp, Some(run request) when commands run).
    fn prepare_node(
        &mut self,
        graph: &HashMap<Symbol, PNode>,
        sym: Symbol,
        missing: &mut std::collections::HashSet<Symbol>,
    ) -> Result<(ExecStatus, Option<RunReq>)> {
        let n = graph[&sym].node.clone();
        let output = sym;
        let output_str = output.as_bytes();
        let loc = n.lock().loc.clone();
        let _frame =
            self.ce
                .ev
                .enter(FrameType::Exec, output_str.clone(), loc.unwrap_or_default());
        let output_timestamp = get_timestamp(&output_str, &self.ce.ev.working_dir)?;
        let output_ts = ExecStatus::Timestamp(output_timestamp);
        let (has_rule, is_phony) = {
            let g = n.lock();
            (g.has_rule, g.is_phony)
        };
        if !has_rule && output_timestamp.is_none() && !is_phony {
            // GNU considers prerequisites LAZILY: a rule-less missing file
            // only errors when a CONSUMER is reached and it's still absent —
            // an earlier recipe may create it as a side effect (files built
            // by another rule without being its declared output). Defer: the
            // node completes normally; the consumer's prepare re-stats it.
            missing.insert(sym);
        }
        let mut latest = ExecStatus::Processing;
        for d in &graph[&sym].deps {
            if missing.contains(d) {
                // The dep had no rule and didn't exist when IT was prepared.
                // Re-stat now — everything scheduled before this consumer has
                // run, matching GNU's in-order consideration.
                let dts = get_timestamp(&d.as_bytes(), &self.ce.ev.working_dir)?;
                if let Some(ts) = dts {
                    missing.remove(d);
                    let r = ExecStatus::Timestamp(Some(ts));
                    if latest < r {
                        latest = r;
                    }
                    continue;
                }
                if !self.quiet_failures {
                    if let Some(dp) = graph.get(d) {
                        for note in &dp.node.lock().no_rule_notes {
                            crate::exec::emit_recipe_err(&format!(
                                "*kati*: note: {note}"));
                        }
                    }
                    // Diagnosis aid: name the RULE whose prerequisite list
                    // produced this target — a bad expansion (joined words,
                    // wrong subst) is otherwise untraceable in a big build.
                    // The `*kati*:` prefix is stripped by the corpus
                    // normalizers, so GNU-parity holds.
                    if let Some(loc) = n.lock().loc.clone() {
                        crate::exec::emit_recipe_err(&format!(
                            "*kati*: note: '{d}' comes from the prerequisite \
list of the rule for '{output}' at {loc}"));
                    }
                }
                error!(
                    "*** No rule to make target '{d}', needed by '{}'.",
                    String::from_utf8_lossy(&output_str)
                );
            }
            if let Some(r) = graph.get(d).and_then(|p| p.result) {
                if latest < r {
                    latest = r;
                }
            }
        }
        if output_ts >= latest && !is_phony {
            return Ok((output_ts, None));
        }
        let (commands, exports) = self.eval_node_commands(&n)?;
        if commands.is_empty() {
            return Ok((output_ts, None));
        }
        let cwd = Bytes::from(self.ce.ev.working_dir.as_os_str().as_bytes().to_vec());
        let box_prefix = self.ce.ev.box_export_prefix.clone();
        Ok((
            output_ts,
            Some(RunReq { output, commands, exports, cwd, box_prefix, result_ts: output_ts }),
        ))
    }

    /// Evaluate a node's recipe to concrete commands (+ its exported rule vars),
    /// applying .ONESHELL fusing. The make thread owns the Evaluator, so this is
    /// serial across nodes.
    fn eval_node_commands(
        &mut self,
        n: &Arc<Mutex<DepNode>>,
    ) -> Result<(Vec<crate::command::Command>, Vec<(Bytes, Bytes)>)> {
        let mut exports: Vec<(Bytes, Bytes)> = Vec::new();
        if let Some(rule_vars) = n.lock().rule_vars.clone() {
            let entries: Vec<(Symbol, crate::var::Var)> =
                rule_vars.0.lock().iter().map(|(s, v)| (*s, v.clone())).collect();
            for (symv, var) in entries {
                if !var.read().exported {
                    continue;
                }
                let val = var.read().eval_to_buf(self.ce.ev)?;
                exports.push((
                    Bytes::from(symv.as_bytes().to_vec()),
                    Bytes::from(val.to_vec()),
                ));
            }
        }
        let mut commands = self.ce.eval(n)?;
        if self.ce.ev.oneshell && commands.len() > 1 {
            use bytes::{BufMut, BytesMut};
            let mut combined = BytesMut::new();
            let first_echo = commands[0].echo;
            let first_ignore = commands[0].ignore_error;
            let first_output = commands[0].output;
            for (i, c) in commands.iter().enumerate() {
                if i > 0 {
                    combined.put_u8(b'\n');
                }
                combined.put_slice(&c.cmd);
            }
            commands = vec![crate::command::Command {
                output: first_output,
                cmd: combined.freeze(),
                echo: first_echo,
                ignore_error: first_ignore,
                force_no_subshell: false,
            }];
        }
        Ok((commands, exports))
    }

    /// Mark `sym` done with timestamp `ts`, then release any dependents whose
    /// last dep just completed (pushing them onto the ready heap by rank).
    fn finish_node(
        &self,
        graph: &mut HashMap<Symbol, PNode>,
        sym: Symbol,
        ts: ExecStatus,
        ready: &mut std::collections::BinaryHeap<Ready>,
        done_count: &mut usize,
    ) {
        {
            let p = graph.get_mut(&sym).unwrap();
            if p.state == PState::Done {
                return;
            }
            p.result = Some(ts);
            p.state = PState::Done;
        }
        *done_count += 1;
        let dependents = graph[&sym].dependents.clone();
        for dep in dependents {
            let now_ready = {
                let dp = graph.get_mut(&dep).unwrap();
                if dp.unfinished > 0 {
                    dp.unfinished -= 1;
                }
                dp.unfinished == 0 && dp.state != PState::Done
            };
            if now_ready {
                let r = graph[&dep].rank;
                ready.push(Ready { rank: r, sym: dep });
            }
        }
    }

    /// The dependency-count scheduler: evaluate ready nodes (lowest DFS-rank
    /// first), dispatch their recipes to worker threads bounded by `cap` and (if
    /// present) the engine slip pool, and complete dependents as runs finish.
    /// At cap=1 this is byte-identical in order to the recursive `exec_node`.
    fn exec_graph(
        &mut self,
        roots: Vec<NamedDepNode>,
        cap: usize,
        client: Option<crate::jobserver::Client>,
    ) -> Result<()> {
        use std::collections::{BinaryHeap, HashSet};

        let mut graph: HashMap<Symbol, PNode> = HashMap::new();
        let mut rank = 0usize;
        let mut visiting: HashSet<Symbol> = HashSet::new();
        for (_, root) in &roots {
            self.discover(root, &mut graph, &mut visiting, &mut rank);
        }
        let edges: Vec<(Symbol, Vec<Symbol>)> =
            graph.iter().map(|(s, p)| (*s, p.deps.clone())).collect();
        for (s, deps) in &edges {
            graph.get_mut(s).unwrap().unfinished = deps.len();
        }
        for (s, deps) in &edges {
            for d in deps {
                if let Some(dp) = graph.get_mut(d) {
                    dp.dependents.push(*s);
                }
            }
        }

        let mut ready: BinaryHeap<Ready> = BinaryHeap::new();
        for (s, p) in &graph {
            if p.unfinished == 0 {
                ready.push(Ready { rank: p.rank, sym: *s });
            }
        }
        let mut run_queue: BinaryHeap<Ready> = BinaryHeap::new();
        let mut reqs: HashMap<Symbol, RunReq> = HashMap::new();
        let (tx, rx) = std::sync::mpsc::channel::<RunMsg>();
        let mut running = 0usize;
        let mut failed: Option<i32> = None;
        let mut done_count = 0usize;
        let total = graph.len();

        // -k can also arrive as a makefile-level `MAKEFLAGS += -k`, which
        // lands in the MAKEFLAGS variable after FLAGS was parsed.
        // Rule-less nodes that didn't exist at their own prepare — resolved
        // (or errored) lazily at each consumer, GNU-style.
        let mut missing: std::collections::HashSet<Symbol> =
            std::collections::HashSet::new();
        let keep_going = FLAGS.is_keep_going
            || self
                .ce
                .ev
                .lookup_var(crate::symtab::intern("MAKEFLAGS"))
                .ok()
                .flatten()
                .and_then(|v| {
                    use crate::expr::Evaluable;
                    v.read().eval_to_buf(self.ce.ev).ok()
                })
                .is_some_and(|mf| crate::flags::Flags::makeflags_keep_going(&mf));
        // Nodes skipped because a (transitive) dependency failed under -k;
        // goal targets in here get the "not remade" notice at the end.
        let mut failed_set: HashSet<Symbol> = HashSet::new();
        loop {
            // 1. Evaluate ready nodes on the make thread (serial) → done or queued.
            while failed.is_none() || keep_going {
                let Some(Ready { sym, .. }) = ready.pop() else { break };
                let (ts, runreq) = self.prepare_node(&graph, sym, &mut missing)?;
                match runreq {
                    None => self.finish_node(&mut graph, sym, ts, &mut ready, &mut done_count),
                    Some(req) => {
                        self.num_commands += req.commands.len() as u64;
                        let r = graph[&sym].rank;
                        reqs.insert(sym, req);
                        run_queue.push(Ready { rank: r, sym });
                    }
                }
            }
            // 2. Dispatch queued recipes, bounded by cap and the slip pool.
            while running < cap {
                let Some(sym) = run_queue.peek().map(|r| r.sym) else { break };
                // First concurrent run uses the implicit token; the rest acquire
                // a slip from the shared pool (when one is advertised).
                let slip = if running == 0 {
                    None
                } else if let Some(c) = &client {
                    match c.try_acquire() {
                        Some(t) => Some(t),
                        None => break,
                    }
                } else {
                    None
                };
                run_queue.pop();
                let req = reqs.remove(&sym).unwrap();
                let shell = self.shell.clone();
                let shellflag = self.shellflag;
                let txc = tx.clone();
                // Large stack: a recipe may be a recursive sub-make whose kati
                // parse/eval recurses deeply on big Makefiles (busybox/kernel).
                // The default 2 MiB spawned-thread stack overflows there; ckati
                // relies on the 8 MiB main-thread stack. 64 MiB is ample.
                std::thread::Builder::new()
                    .stack_size(64 * 1024 * 1024)
                    .spawn(move || {
                        // Stream each output chunk to the main thread LIVE (it owns
                        // the RECIPE_OUT sink); then the final result.
                        let mut emit = |b: &[u8]| { let _ = txc.send(RunMsg::Chunk(b.to_vec())); };
                        let r = run_node_commands(&shell, shellflag, req, &mut emit);
                        let _ = txc.send(RunMsg::Done(r, slip));
                    })
                    .expect("spawn recipe worker");
                running += 1;
            }
            // 3. Termination.
            if running == 0 {
                if !ready.is_empty() || !run_queue.is_empty() {
                    continue;
                }
                // A goal target that is STILL rule-less and absent errors
                // even without a consumer (GNU: "No rule to make target").
                if failed.is_none() {
                    for (name, node) in &roots {
                        if missing.contains(name)
                            && get_timestamp(&name.as_bytes(),
                                             &self.ce.ev.working_dir)?.is_none()
                        {
                            if !self.quiet_failures {
                                for note in &node.lock().no_rule_notes {
                                    crate::exec::emit_recipe_err(&format!(
                                        "*kati*: note: {note}"));
                                }
                            }
                            error!("*** No rule to make target '{name}'");
                        }
                    }
                }
                if let Some(code) = failed {
                    if keep_going {
                        for (name, node) in &roots {
                            let _ = node;
                            if failed_set.contains(name) {
                                emit_recipe_err(&format!(
                                    "Target \"{}\" not remade because of errors.",
                                    String::from_utf8_lossy(&name.as_bytes())));
                            }
                        }
                    }
                    return Err(BuildFailed(code).into());
                }
                if done_count < total {
                    error!(
                        "*** Circular dependency detected -- {} target(s) unbuilt.",
                        total - done_count
                    );
                }
                break;
            }
            // 4. Drain worker messages: emit output chunks LIVE; a Done marks a
            // node finished (handle failure, release its slip). Chunks from
            // concurrent nodes interleave (like GNU make without -O); at cap 1
            // there's one worker, so the byte order is unchanged.
            let (run, slip) = match rx.recv().unwrap() {
                RunMsg::Chunk(b) => { emit_recipe_output(&b); continue; }
                RunMsg::Done(run, slip) => (run, slip),
            };
            running -= 1;
            if let (Some(c), Some(t)) = (&client, slip) {
                c.release(t);
            }
            for (o, c) in &run.ignored {
                emit_recipe_err(&format!("[{o}] Error {c} (ignored)"));
            }
            if let Some((o, code)) = run.failure {
                if !self.quiet_failures {
                    emit_recipe_err(&format!("*** [{o}] Error {code}"));
                }
                if self.ce.ev.delete_on_error {
                    let is_phony = graph[&run.output].node.lock().is_phony;
                    if !is_phony {
                        let out_bytes = o.as_bytes();
                        let path = self.ce.ev.working_dir.join(OsStr::from_bytes(&out_bytes));
                        if std::fs::exists(&path).unwrap_or(false) {
                            emit_recipe_err(&format!(
                                "*** Deleting file \"{}\"",
                                String::from_utf8_lossy(&out_bytes)
                            ));
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                failed = Some(2);
                if keep_going {
                    // -k: mark the failed node and everything depending on
                    // it as not-remade; keep building the rest.
                    let mut stack = vec![run.output];
                    while let Some(s) = stack.pop() {
                        if !failed_set.insert(s) {
                            continue;
                        }
                        let p = graph.get_mut(&s).unwrap();
                        if p.state != PState::Done {
                            p.state = PState::Done;
                            done_count += 1;
                        }
                        stack.extend(graph[&s].dependents.iter().copied());
                    }
                } else {
                    // Stop launching new work; drain in-flight runs, bail.
                    ready.clear();
                    run_queue.clear();
                }
            } else {
                self.finish_node(&mut graph, run.output, run.result_ts, &mut ready, &mut done_count);
            }
        }
        Ok(())
    }
}

pub fn exec(roots: Vec<NamedDepNode>, ev: &mut Evaluator) -> Result<()> {
    exec_opts(roots, ev, false)
}

/// exec with GNU's makefile-remake failure semantics available:
/// `quiet_failures` suppresses the "*** [target] Error N" banner and the
/// no-rule notes — callers remaking OPTIONAL includes swallow the Err too.
pub fn exec_opts(
    roots: Vec<NamedDepNode>, ev: &mut Evaluator, quiet_failures: bool,
) -> Result<()> {
    let not_parallel = ev.not_parallel;
    let mut executor = Executor::new(ev)?;
    executor.quiet_failures = quiet_failures;
    // One engine: a dependency-count scheduler with a worker cap. Parallel only
    // when -j>1 was explicitly requested; bounded by the engine slip pool when
    // one is advertised (sarun box), else by the local cap alone (standalone,
    // like make's own jobserver). No -j ⇒ cap 1 ⇒ serial, byte-identical order
    // to the old recursion — so the rkati↔make corpus is unaffected.
    let client = crate::jobserver::Client::from_env();
    // Parallel when -j>1 was requested OR a parent advertised a jobserver in
    // MAKEFLAGS (a sub-make inherits parallelism, like GNU make). Plain standalone
    // `make` with no jobserver stays serial (cap 1), so the corpus is unaffected.
    let parallel = (FLAGS.jobs_explicit || client.is_some())
        // .NOTPARALLEL: this make is serial regardless of -j/jobserver.
        && !not_parallel;
    let cap = if parallel { FLAGS.num_jobs.max(1) } else { 1 };
    let client = if cap > 1 { client } else { None };
    executor.exec_graph(roots.clone(), cap, client)?;
    // sarun: emit "Nothing to be done" only for roots whose rule has no
    // commands at all (or which had no rule).
    if executor.num_commands == 0 {
        for (sym, root) in roots {
            let node = root.lock();
            if node.cmds.is_empty() {
                println!("kati: Nothing to be done for `{sym}'.")
            }
        }
    }
    Ok(())
}
