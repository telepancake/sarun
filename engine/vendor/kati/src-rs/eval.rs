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

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::io::BufWriter;
use std::os::unix::ffi::OsStringExt;
use std::sync::{Arc, LazyLock, Weak};

use anyhow::{Context, Result};
use bytes::{Buf, Bytes};
use memchr::{memchr, memchr2};
use parking_lot::Mutex;

use crate::expr::Evaluable;
use crate::expr::Value;
use crate::flags::FLAGS;
use crate::loc::Loc;
use crate::parser::{parse_assign_statement, parse_buf_no_stats};
use crate::rule::{Rule, is_pattern_rule};
use crate::stmt::{
    AssignOp, AssignStmt, CommandStmt, CondOp, ExportStmt, IfStmt, IncludeStmt, RuleSep, RuleStmt,
    Statement, UndefineStmt,
};
use crate::strutil::{is_space_byte, trim_leading_curdir, trim_right_space, word_scanner};
use crate::symtab::{ALLOW_RULES_SYM, KATI_READONLY_SYM, MAKEFILE_LIST, SHELL_SYM, Symbol, intern};
use crate::var::{Var, VarOrigin, Variable, Vars};
use crate::{collect_stats_with_slow_report, error, error_loc, file_cache, log, warn_loc};

/// Minimal JSON string escaping for the trace records (no serde dep here).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub enum RulesAllowed {
    Allowed,
    Warning,
    Error,
}

/// Whether `export` directives are allowed.
pub enum ExportAllowed {
    /// Export directives are allowed, the default.
    Allowed,
    /// Export directives result in warnings with the specified message.
    Warning(String),
    /// Export directives result in errors with the specified message.
    Error(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameType {
    Root,       // Root node. Exactly one of this exists.
    Phase,      // Markers for various phases of the execution.
    Parse,      // Initial evaluation pass: include, := variables, etc.
    Call,       // Evaluating the result of a function call
    FunCall,    // Evaluating a function call (not its result)
    Statement,  // Denotes individual statements for better location reporting
    Dependency, // Dependency analysis. += requires variable expansion here.
    Exec,       // Execution phase. Expansion of = and rule-specific variables.
    Ninja,      // Ninja file generation
}

#[derive(Debug)]
pub struct Frame {
    frame_type: FrameType,
    #[allow(dead_code)]
    parent: Option<Weak<Frame>>,
    name: Bytes,
    location: Option<Loc>,
    children: Mutex<Vec<Arc<Frame>>>,
}

impl Frame {
    fn new(
        frame_type: FrameType,
        parent: Option<Arc<Frame>>,
        loc: Option<Loc>,
        name: Bytes,
    ) -> Self {
        assert!(parent.is_none() == (frame_type == FrameType::Root));
        Self {
            frame_type,
            parent: parent.map(|p| Arc::downgrade(&p)),
            name,
            location: loc,
            children: Mutex::new(Vec::new()),
        }
    }

    fn add(&self, child: Arc<Frame>) {
        self.children.lock().push(child);
    }

    #[allow(dead_code)]
    fn print_json_trace(&self, tf: &mut dyn std::io::Write, indent: usize) -> Result<()> {
        if self.frame_type == FrameType::Root {
            return Ok(());
        }

        let indent_string = " ".repeat(indent);
        let mut desc = String::from_utf8_lossy(&self.name);
        if let Some(loc) = &self.location {
            desc = Cow::Owned(format!("{desc} @ {loc}"));
        }

        let parent = self.parent.clone().unwrap().upgrade();
        let comma = if parent
            .clone()
            .is_some_and(|p| p.frame_type == FrameType::Root)
        {
            ""
        } else {
            ","
        };
        writeln!(tf, "{indent_string}\"{desc}\"{comma}")?;
        if let Some(parent) = parent {
            parent.print_json_trace(tf, indent)?;
        }
        Ok(())
    }
}

pub struct ScopedFrame {
    stack: Arc<Mutex<Vec<Arc<Frame>>>>,
    frame: Option<Arc<Frame>>,
}

impl ScopedFrame {
    fn new(stack: Arc<Mutex<Vec<Arc<Frame>>>>, frame: Option<Arc<Frame>>) -> Self {
        if let Some(frame) = frame.clone() {
            let mut stack = stack.lock();
            stack.last().unwrap().add(frame.clone());
            stack.push(frame);
        }
        Self { stack, frame }
    }
    pub fn current(&self) -> Option<Arc<Frame>> {
        self.frame.clone()
    }
}

impl Drop for ScopedFrame {
    fn drop(&mut self) {
        if let Some(frame) = &self.frame {
            let mut stack = self.stack.lock();
            let last = stack.pop().unwrap();
            assert!(last.name == frame.name);
            assert!(last.location == frame.location);
        }
    }
}

#[derive(Default)]
struct IncludeGraphNode {
    includes: BTreeSet<Bytes>,
}

struct IncludeGraph {
    nodes: HashMap<Bytes, IncludeGraphNode>,
    include_stack: Vec<Arc<Frame>>,
}

impl IncludeGraph {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            include_stack: Vec::new(),
        }
    }

    fn dump_json(&self, tf: &mut dyn std::io::Write) -> Result<()> {
        writeln!(tf, "{{")?;
        write!(tf, "  \"include_graph\": [")?;
        let mut first_node = true;

        for (file, node) in &self.nodes {
            if first_node {
                first_node = false;
                writeln!(tf)?;
            } else {
                writeln!(tf, ",")?;
            }

            writeln!(tf, "    {{")?;
            // TODO(lberki): Quote all these strings properly
            writeln!(tf, "      \"file\": \"{}\",", String::from_utf8_lossy(file))?;
            write!(tf, "      \"includes\": [")?;
            let mut first_include = true;
            for include in &node.includes {
                if first_include {
                    first_include = false;
                    writeln!(tf)?;
                } else {
                    writeln!(tf, ",")?;
                }

                write!(tf, "        \"{}\"", String::from_utf8_lossy(include))?;
            }
            writeln!(tf, "\n      ]")?;
            write!(tf, "    }}")?;
        }
        writeln!(tf, "\n  ]")?;
        writeln!(tf, "}}")?;

        Ok(())
    }

    fn merge_tree_node(&mut self, frame: &Arc<Frame>) {
        if frame.frame_type == FrameType::Parse {
            self.nodes.entry(frame.name.clone()).or_default();

            if let Some(parent_frame) = self.include_stack.last()
                && let Some(parent_node) = self.nodes.get_mut(&parent_frame.name)
            {
                parent_node.includes.insert(frame.name.clone());
            }

            self.include_stack.push(frame.clone());
        }

        for child in &*frame.children.lock() {
            self.merge_tree_node(child);
        }

        if frame.frame_type == FrameType::Parse {
            self.include_stack.pop();
        }
    }
}

static USED_UNDEFINED_VARS: LazyLock<Mutex<HashSet<Symbol>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub struct Evaluator {
    pub rule_vars: HashMap<Symbol, Arc<Vars>>,
    /// sarun: accumulated `vpath PATTERN DIRS` directive entries, in
    /// makefile order — consumed by dep.rs's directory search.
    pub vpath_patterns: Vec<(Bytes, Vec<Bytes>)>,
    /// sarun: per-instance global variable bindings, indexed by Symbol id.
    /// Moved off the process-global symbol table (symtab `symbol_data`) so each
    /// Evaluator — i.e. each make invocation — owns its OWN global namespace.
    /// That is both the correct sub-make semantics (a sub-make starts fresh,
    /// inheriting only exported env) and the prerequisite for running multiple
    /// kati instances in one process without their variables colliding. The
    /// interner (name<->id) stays global in symtab; only the bindings are
    /// per-instance. Behind Arc<Mutex> so a ScopedGlobalVar can restore its
    /// saved binding on Drop without needing an &Evaluator.
    pub global_vars: Arc<Mutex<Vec<Option<Var>>>>,
    /// sarun: per-instance logical working directory. kati's path-resolving
    /// boundaries (makefile read, `include`, `$(wildcard)`/`$(abspath)`/
    /// `$(realpath)`/`$(file)`, the find emulator root) resolve relative paths
    /// against THIS instead of the process cwd, so an in-process `make` builtin
    /// can run against the brush shell's logical cwd without `chdir`-ing the
    /// process — and concurrent instances in different directories don't race
    /// on it. Seeded from the process cwd so standalone rkati is unchanged.
    pub working_dir: std::path::PathBuf,
    /// sarun: GNU make's `-I`/`--include-dir` search path. When `include`
    /// can't find a file relative to working_dir, it tries each of these
    /// directories in order (matching GNU make's include-search semantics).
    /// Populated from the command-line `-I` and from `--include-dir=` in
    /// MAKEFLAGS (the kernel build adds `--include-dir=$(abs_srctree)`).
    pub include_dirs: Vec<std::path::PathBuf>,
    pub rules: Vec<Rule>,
    pub exports: HashMap<Symbol, bool>,
    /// sarun: set when the makefile names `.EXPORT_ALL_VARIABLES` as a
    /// target. Causes every make-defined variable to be exported into
    /// recipe environments, mirroring GNU make's behavior.
    pub export_all_vars: bool,
    /// sarun: set when the makefile names `.ONESHELL` as a target.
    /// Recipe lines for each rule are concatenated and passed to a
    /// single shell invocation, so shell state (variables, cwd, set
    /// flags) persists across them.
    pub oneshell: bool,
    /// sarun: set when the makefile names `.DELETE_ON_ERROR` as a
    /// target. When a recipe exits with a non-zero status, kati
    /// removes the target's output file (mirrors GNU make).
    pub delete_on_error: bool,
    /// sarun: required `include` directives whose file didn't exist at
    /// parse time, paired with the source location of the directive.
    /// The remake-the-makefile loop in main.rs checks for rules
    /// producing these names and builds + re-parses if any apply.
    pub pending_remake_includes: Vec<(Loc, OsString)>,
    /// sarun: in the in-process box, exported make variables can't be staged
    /// into the process environment (`std::env::set_var`) — many makes share one
    /// engine process, so that global write is a data race (UB) AND leaks one
    /// make's exports into another. Instead the engine builds a non-echoed shell
    /// prefix (`export NAME='val'` / `unset NAME`, newline-terminated) here, and
    /// the executor (recipes) and `$(shell)` prepend it to the command run in the
    /// brush subshell, so exports reach children through the per-subshell env.
    /// Empty for the standalone rkati binary, which keeps the `std::env` path
    /// (one OS process per make, where that's correct).
    pub box_export_prefix: Bytes,
    symbols_for_eval: HashSet<Symbol>,

    in_rule: bool,
    pub current_scope: Option<Arc<Vars>>,

    pub loc: Option<Loc>,
    is_bootstrap: bool,
    is_commandline: bool,

    trace: bool,
    stack: Arc<Mutex<Vec<Arc<Frame>>>>,
    assignment_tracefile: Option<Box<dyn std::io::Write>>,

    pub avoid_io: bool,
    // This value tracks the nest level of make expressions. For
    // example, $(YYY) in $(XXX $(YYY)) is evaluated with depth==2.
    // This will be used to disallow $(shell) in other make constructs.
    pub eval_depth: i32,
    // Commands which should run at ninja-time (i.e., info, warning, and
    // error).
    pub delayed_output_commands: Vec<Bytes>,

    posix_sym: Symbol,
    is_posix: bool,

    /// Whether `export`/`unexport` directives are allowed.
    pub export_allowed: ExportAllowed,

    pub profiled_files: Vec<OsString>,

    pub is_evaluating_command: bool,
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl Evaluator {
    pub fn new() -> Self {
        let ev = Self {
            rule_vars: HashMap::new(),
            vpath_patterns: Vec::new(),
            global_vars: Arc::new(Mutex::new(Vec::new())),
            working_dir: std::env::current_dir().unwrap_or_default(),
            include_dirs: Vec::new(),
            rules: Vec::new(),
            exports: HashMap::new(),
            export_all_vars: false,
            oneshell: false,
            delete_on_error: false,
            pending_remake_includes: Vec::new(),
            box_export_prefix: Bytes::new(),
            symbols_for_eval: HashSet::new(),

            in_rule: false,
            current_scope: None,

            loc: None,
            is_bootstrap: false,
            is_commandline: false,

            trace: FLAGS.dump_variable_assignment_trace.is_some()
                || FLAGS.dump_include_graph.is_some(),
            stack: Arc::new(Mutex::new(vec![Arc::new(Frame::new(
                FrameType::Root,
                None,
                None,
                Bytes::from_static(b"*root*"),
            ))])),
            assignment_tracefile: None,

            avoid_io: false,
            eval_depth: 0,
            delayed_output_commands: Vec::new(),

            posix_sym: crate::symtab::intern(".POSIX"),
            is_posix: false,

            export_allowed: ExportAllowed::Allowed,

            profiled_files: Vec::new(),

            is_evaluating_command: false,
        };
        ev.seed_special_vars();
        ev
    }

    /// sarun: seed the builtin special variables that used to be installed
    /// process-globally by Symtab::new(). Now each Evaluator seeds them into
    /// its own per-instance global namespace, so behavior is identical for a
    /// single instance but instances no longer share these bindings.
    fn seed_special_vars(&self) {
        let _ = self.set_global_var(intern(".SHELLSTATUS"), Variable::new_shell_status_var(), false, None);
        let _ = self.set_global_var(
            intern(".VARIABLES"),
            Variable::new_variable_names(b".VARIABLES", true),
            false,
            None,
        );
        let _ = self.set_global_var(
            intern(".KATI_SYMBOLS"),
            Variable::new_variable_names(b".KATI_SYMBOLS", false),
            false,
            None,
        );
    }

    /// sarun: read a global variable binding (peek — no env-use tracking).
    pub fn peek_global_var(&self, sym: Symbol) -> Option<Var> {
        let store = self.global_vars.lock();
        store.get(sym.index())?.clone()
    }

    /// sarun: read a global variable binding, tracking env-var use (mirrors
    /// the old Symbol::get_global_var).
    pub fn get_global_var(&self, sym: Symbol) -> Option<Var> {
        let v = {
            let store = self.global_vars.lock();
            store.get(sym.index())?.clone()?
        };
        match v.read().origin() {
            VarOrigin::Environment | VarOrigin::EnvironmentOverride => {
                crate::var::USED_ENV_VARS.lock().insert(sym);
            }
            _ => {}
        }
        Some(v)
    }

    /// sarun: drop a binding (the `undefine` directive). Was
    /// Symbol::clear_global_var.
    pub fn clear_global_var(&self, sym: Symbol) {
        let mut store = self.global_vars.lock();
        let idx = sym.index();
        if idx < store.len() {
            store[idx] = None;
        }
    }

    /// sarun: assign a global variable, honoring make's precedence rules
    /// (readonly, command-line/override beats file, automatic is overwritable).
    /// Ported verbatim from the old Symtab::set_global_var.
    pub fn set_global_var(
        &self,
        sym: Symbol,
        var: Var,
        is_override: bool,
        readonly: Option<&mut bool>,
    ) -> Result<()> {
        let mut store = self.global_vars.lock();
        let idx = sym.index();
        if idx >= store.len() {
            store.resize(idx + 1, None);
        }
        let entry = store.get_mut(idx).unwrap();
        if let Some(orig) = entry {
            if orig.read().readonly {
                if let Some(readonly) = readonly {
                    *readonly = true;
                } else {
                    error!("*** cannot assign to readonly variable: {sym}");
                }
                return Ok(());
            } else if let Some(readonly) = readonly {
                *readonly = false;
            }
            let origin = orig.read().origin();
            if !is_override
                && (origin == VarOrigin::Override || origin == VarOrigin::EnvironmentOverride)
            {
                return Ok(());
            }
            if origin == VarOrigin::CommandLine && var.read().origin() == VarOrigin::File {
                return Ok(());
            }
            // sarun: $(eval) inside $(call) often does `1:=newval` to rebind the
            // call arg. Real make accepts the override; when the surrounding
            // ScopedGlobalVar later drops, the original Automatic binding is
            // restored.
            if origin == VarOrigin::Automatic {
                // fall through — overwrite the entry below.
            }
        }
        *entry = Some(var);
        Ok(())
    }

    /// sarun: enumerate per-instance global bindings passing `filter`, paired
    /// with their interned names. Feeds `.VARIABLES`/`.KATI_SYMBOLS` and
    /// `.EXPORT_ALL_VARIABLES`. Was the free fn symtab::get_symbol_names, which
    /// walked the global symbol_data; now walks this Evaluator's bindings.
    pub fn get_symbol_names<T: Fn(Var) -> bool>(&self, filter: T) -> Vec<(Symbol, Bytes)> {
        let store = self.global_vars.lock();
        store
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let var = slot.clone()?;
                if !filter(var) {
                    return None;
                }
                let sym = Symbol::from_index(idx)?;
                Some((sym, sym.as_bytes()))
            })
            .collect()
    }

    pub fn start(&mut self) -> Result<()> {
        let Some(filename) = &FLAGS.dump_variable_assignment_trace else {
            return Ok(());
        };

        // sarun: the trace is JSONL — one self-contained record per line,
        // written with a single write() call so concurrent in-process makes
        // (which all share stderr / the file) interleave at line granularity
        // instead of shredding a header/footer-framed document. A make that
        // never touches a traced variable emits NOTHING. Each record carries
        // \"make\" (the emitting make's working dir) to tell the streams apart.
        if filename == "-" {
            self.assignment_tracefile = Some(Box::new(std::io::stderr()));
        } else {
            // Append, don't truncate: every nested make re-opens the file.
            let f = std::fs::OpenOptions::new().create(true).append(true)
                .open(filename)?;
            self.assignment_tracefile = Some(Box::new(f));
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if let Some(tf) = self.assignment_tracefile.as_mut() {
            tf.flush()?;
        }
        Ok(())
    }

    /// One JSONL trace record, emitted with a single write for line-atomic
    /// interleaving across concurrent makes. `extra` fields come pre-escaped.
    fn trace_emit(&mut self, body: String) -> Result<()> {
        let make_dir = json_escape(&self.working_dir.to_string_lossy());
        let line = format!("{{\"make\": \"{make_dir}\", {body}}}\n");
        if let Some(tf) = self.assignment_tracefile.as_mut() {
            tf.write_all(line.as_bytes())?;
            tf.flush()?;
        }
        Ok(())
    }

    pub fn in_bootstrap(&mut self) {
        self.is_bootstrap = true;
        self.is_commandline = false;
    }

    pub fn in_command_line(&mut self) {
        self.is_bootstrap = false;
        self.is_commandline = true;
    }

    pub fn in_toplevel_makefile(&mut self) {
        self.is_bootstrap = false;
        self.is_commandline = false;
    }

    pub fn current_frame(&self) -> Arc<Frame> {
        self.stack.lock().last().unwrap().clone()
    }

    pub fn eval_rhs(
        &mut self,
        lhs: Symbol,
        rhs_v: Arc<Value>,
        orig_rhs: Bytes,
        op: AssignOp,
        is_override: bool,
    ) -> Result<(Var, bool)> {
        let (origin, current_frame) = if self.is_bootstrap {
            (VarOrigin::Default, None)
        } else if self.is_commandline {
            (VarOrigin::CommandLine, None)
        } else if is_override {
            (VarOrigin::Override, self.stack.lock().last().cloned())
        } else {
            (VarOrigin::File, self.stack.lock().last().cloned())
        };

        let result: Var;
        let prev: Option<Var>;
        let mut needs_assign = true;

        match op {
            AssignOp::ColonEq => {
                prev = self.peek_var_in_current_scope(lhs);
                result = Variable::with_simple_value(
                    origin,
                    current_frame,
                    self.loc.clone(),
                    self,
                    &rhs_v,
                )?;
            }
            AssignOp::Eq => {
                prev = self.peek_var_in_current_scope(lhs);
                result = Variable::new_recursive(
                    rhs_v,
                    origin,
                    current_frame,
                    self.loc.clone(),
                    orig_rhs,
                );
            }
            AssignOp::PlusEq => {
                prev = self.lookup_var_in_current_scope(lhs)?;
                if let Some(prev) = prev.clone() {
                    if prev.read().readonly {
                        error_loc!(
                            self.loc.as_ref(),
                            "*** cannot assign to readonly variable: {lhs}"
                        );
                    }
                    result = prev;
                    if result.read().immediate_eval() {
                        let buf = rhs_v.eval_to_buf(self)?;
                        result.write().append_str(&buf, self.current_frame())?;
                    } else {
                        result.write().append_var(
                            rhs_v,
                            self.current_frame(),
                            self.loc.as_ref(),
                        )?;
                    }
                    needs_assign = false;
                } else {
                    result = Variable::new_recursive(
                        rhs_v,
                        origin,
                        current_frame,
                        self.loc.clone(),
                        orig_rhs,
                    );
                }
            }
            AssignOp::QuestionEq => {
                prev = self.lookup_var_in_current_scope(lhs)?;
                if let Some(prev) = prev.clone() {
                    result = prev;
                    needs_assign = false;
                } else {
                    result = Variable::new_recursive(
                        rhs_v,
                        origin,
                        current_frame,
                        self.loc.clone(),
                        orig_rhs,
                    );
                }
            }
            AssignOp::BangEq => {
                prev = self.peek_var_in_current_scope(lhs);
                // sarun: `X != cmd` — run cmd through the shell at assign
                // time, store output as a simply-expanded value. Real make
                // converts internal newlines to spaces and strips trailing
                // whitespace, which is what shell_func_impl's
                // format_for_command_substitution does for us.
                let cmd_buf = rhs_v.eval_to_buf(self)?;
                let loc = self.loc.clone().unwrap_or_default();
                let shell = self.get_shell()?;
                let shellflag = self.get_shell_flag();
                let box_prefix = self.box_export_prefix.clone();
                let cwd = std::os::unix::ffi::OsStrExt::as_bytes(
                    self.working_dir.as_os_str()).to_vec();
                let (_exit, output, _fc) =
                    crate::func::shell_func_impl(&shell, shellflag, &cmd_buf, &loc,
                                                 &box_prefix, &cwd)?;
                result = Variable::with_simple_string(
                    output,
                    origin,
                    current_frame,
                    self.loc.clone(),
                );
            }
        }

        if let Some(prev) = prev {
            let prev = prev.read();
            prev.used(self, &lhs)?;
            if needs_assign && let Some(deprecated) = &prev.deprecated {
                result.write().deprecated = Some(deprecated.clone());
            }
        }

        Ok((result, needs_assign))
    }

    pub fn eval_assign(&mut self, stmt: &AssignStmt) -> Result<()> {
        self.loc = Some(stmt.loc());
        self.in_rule = false;
        let lhs = stmt.get_lhs_symbol(self)?;

        if lhs == *KATI_READONLY_SYM {
            let rhs = stmt.rhs.eval_to_buf(self)?;
            for name in word_scanner(&rhs) {
                let name = intern(rhs.slice_ref(name));
                let Some(var) = self.get_global_var(name) else {
                    error_loc!(self.loc.as_ref(), "*** unknown variable: {name}");
                };
                var.write().readonly = true;
            }
            return Ok(());
        }

        let is_override = stmt.directive.map(|v| v.is_override).unwrap_or(false);
        let (var, needs_assign) = self.eval_rhs(
            lhs,
            stmt.rhs.clone(),
            stmt.orig_rhs.clone(),
            stmt.op,
            is_override,
        )?;
        if needs_assign {
            let mut readonly = false;
            self.set_global_var(lhs, var.clone(), is_override, Some(&mut readonly))?;
            if readonly {
                error_loc!(
                    self.loc.as_ref(),
                    "*** cannot assign to readonly variable: {lhs}"
                );
            }
        }

        if stmt.is_final {
            var.write().readonly = true
        }
        self.trace_variable_assign(&lhs, &var)?;
        // sarun: feed the engine's assignment recorder — makefile-level
        // assignments only (bootstrap/command-line noise is engine plumbing).
        if !self.is_bootstrap && !self.is_commandline {
            let g = var.read();
            // the ASSIGNMENT site (stmt.loc), not the variable's original
            // definition loc — `+=` must point at the append, not the `:=`.
            let loc = stmt.loc().to_string();
            let val = g.string().unwrap_or(std::borrow::Cow::Borrowed(b""));
            let op = match stmt.op {
                AssignOp::Eq => "=",
                AssignOp::ColonEq => ":=",
                AssignOp::PlusEq => "+=",
                AssignOp::QuestionEq => "?=",
                AssignOp::BangEq => "!=",
            };
            crate::fileutil::report_var_assign(
                &lhs.as_bytes(), &loc, &val,
                self.working_dir.as_os_str().as_bytes(), &stmt.orig_rhs,
                op, crate::var::get_origin_str(g.origin()));
        }
        Ok(())
    }

    // With rule broken into
    //   <before_term> <term> <after_term>
    // parses <before_term> into Symbol instances until encountering ':'
    // Returns the remainder of <before_term>.
    pub fn parse_rule_targets(
        loc: &Loc,
        before_term: &Bytes,
    ) -> Result<(Bytes, Vec<Symbol>, bool)> {
        let Some(idx) = memchr(b':', before_term) else {
            error_loc!(Some(loc), "*** missing separator.");
        };
        let targets_string = before_term.slice(0..idx);
        let after = before_term.slice(idx + 1..);
        let mut pattern_rule_count = 0;
        let mut targets: Vec<Symbol> = Vec::new();
        for word in word_scanner(&targets_string) {
            let target = targets_string.slice_ref(trim_leading_curdir(word));
            targets.push(intern(target.clone()));
            if is_pattern_rule(&target) {
                pattern_rule_count += 1;
            }
        }
        // Check consistency: either all outputs are patterns or none.
        if pattern_rule_count > 0 && pattern_rule_count != targets.len() {
            error_loc!(
                Some(loc),
                "*** mixed implicit and normal rules: deprecated syntax"
            );
        }
        Ok((after, targets, pattern_rule_count > 0))
    }

    // Strip leading spaces and trailing spaces and colons.
    pub fn format_rule_error(before_term: &[u8]) -> String {
        let before_term = String::from_utf8_lossy(before_term).into_owned();
        if before_term.is_empty() {
            return before_term;
        }
        before_term
            .trim_ascii_start()
            .trim_end_matches(|c: char| c.is_ascii_whitespace() || c == ':')
            .to_string()
    }

    pub fn mark_vars_readonly(&mut self, vars_list: &Value) -> Result<()> {
        let vars_list_string = vars_list.eval_to_buf(self)?;
        for name in word_scanner(&vars_list_string) {
            let name = intern(vars_list_string.slice_ref(name));
            let Some(var) = self.current_scope.as_ref().unwrap().lookup(name) else {
                error_loc!(self.loc.as_ref(), "*** unknown variable: {name}");
            };
            var.write().readonly = true;
        }
        Ok(())
    }

    pub fn eval_rule_specific_assign(
        &mut self,
        targets: &[Symbol],
        stmt: &RuleStmt,
        after_targets: &Bytes,
        separator_pos: usize,
    ) -> Result<()> {
        let mut assign = parse_assign_statement(after_targets, separator_pos);
        // sarun: target-specific `export VAR := …`, `unexport VAR := …`,
        // and `private VAR := …`. Strip the leading directive(s) from the
        // LHS — kati used to intern e.g. "export VAR" as the variable
        // name. `private` is a no-op for the assignment value itself; it
        // means "don't inherit into prereq recipes", which we approximate
        // well enough by simply consuming the keyword (per-target scope
        // already isolates the var from siblings, and our prereq
        // inheritance is shallow).
        let mut tsv_exported = None;
        let mut lhs_trimmed = crate::strutil::trim_left_space(assign.lhs);
        loop {
            if let Some(rest) = lhs_trimmed.strip_prefix(b"export ") {
                tsv_exported = Some(true);
                lhs_trimmed = crate::strutil::trim_left_space(rest);
            } else if let Some(rest) = lhs_trimmed.strip_prefix(b"unexport ") {
                tsv_exported = Some(false);
                lhs_trimmed = crate::strutil::trim_left_space(rest);
            } else if let Some(rest) = lhs_trimmed.strip_prefix(b"private ") {
                lhs_trimmed = crate::strutil::trim_left_space(rest);
            } else {
                break;
            }
        }
        assign.lhs = lhs_trimmed;
        let var_sym = intern(after_targets.slice_ref(assign.lhs));
        let is_final = stmt.sep == RuleSep::FinalEq;
        for target in targets {
            let scope = self
                .rule_vars
                .entry(*target)
                .or_insert_with(|| Arc::new(Vars::new()))
                .clone();

            let rhs = if assign.rhs.is_empty() {
                stmt.rhs.clone()
            } else if let Some(stmt_rhs) = stmt.rhs.clone() {
                let sep = if stmt.sep == RuleSep::Semicolon {
                    b" ; "
                } else {
                    b" = "
                };
                Some(Arc::new(Value::List(
                    self.loc.clone(),
                    vec![
                        Arc::new(Value::Literal(None, after_targets.slice_ref(assign.rhs))),
                        Arc::new(Value::Literal(None, Bytes::from_static(sep))),
                        stmt_rhs,
                    ],
                )))
            } else {
                Some(Arc::new(Value::Literal(
                    None,
                    after_targets.slice_ref(assign.rhs),
                )))
            };

            self.current_scope = Some(scope);
            if var_sym == *KATI_READONLY_SYM {
                if let Some(rhs) = rhs {
                    self.mark_vars_readonly(&rhs)?;
                }
            } else {
                let (rhs_var, needs_assign) = self.eval_rhs(
                    var_sym,
                    rhs.unwrap(),
                    Bytes::from_static(b"*TODO*"),
                    assign.op,
                    false,
                )?;
                if needs_assign {
                    let mut readonly = false;
                    rhs_var.write().assign_op = Some(assign.op);
                    if let Some(exp) = tsv_exported {
                        rhs_var.write().exported = exp;
                    }
                    self.current_scope.as_ref().unwrap().assign(
                        var_sym,
                        rhs_var.clone(),
                        &mut readonly,
                    )?;
                    if readonly {
                        error_loc!(
                            self.loc.as_ref(),
                            "*** cannot assign to readonly variable: {var_sym}"
                        );
                    }
                }
                if is_final {
                    rhs_var.write().readonly = true;
                }
            }
            self.current_scope = None
        }
        Ok(())
    }

    pub fn eval_rule(&mut self, stmt: &RuleStmt) -> Result<()> {
        self.loc = Some(stmt.loc());
        self.in_rule = false;

        let before_term = stmt.lhs.eval_to_buf(self)?;
        // See semicolon.mk.
        if before_term.iter().all(|c| b" \t\n;".contains(c)) {
            if stmt.sep == RuleSep::Semicolon {
                error_loc!(self.loc.as_ref(), "*** missing rule before commands.");
            }
            return Ok(());
        }

        let (mut after_targets, targets, is_pattern_rule) =
            Evaluator::parse_rule_targets(self.loc.as_ref().unwrap(), &before_term)?;
        let is_double_colon = after_targets.starts_with(b":");
        if is_double_colon {
            after_targets.advance(1);
        }

        // Figure out if this is a rule-specific variable assignment.
        // It is an assignment when either after_targets contains an assignment token
        // or separator is an assignment token, but only if there is no ';' before the
        // first assignment token.
        let mut separator_pos = memchr2(b'=', b';', &after_targets);
        let separator = if let Some(separator_pos) = separator_pos {
            Some(after_targets[separator_pos])
        } else if stmt.sep == RuleSep::Eq || stmt.sep == RuleSep::FinalEq {
            separator_pos = Some(after_targets.len());
            Some(b'=')
        } else {
            None
        };

        // If variable name is not empty, we have rule- or target-specific
        // variable assignment.
        if separator == Some(b'=')
            && let Some(separator_pos) = separator_pos
            && separator_pos > 0
        {
            return self.eval_rule_specific_assign(&targets, stmt, &after_targets, separator_pos);
        }

        if separator_pos == Some(0) {
            // We used to make this a warning and otherwise accept it, but Make 4.1
            // calls this out as an error, so let's follow.
            error_loc!(self.loc.as_ref(), "*** empty variable name.");
        }

        let mut rule = Rule::new(self.loc.clone().unwrap(), is_double_colon);
        if is_pattern_rule {
            rule.output_patterns = targets;
        } else {
            rule.outputs = targets;
        }
        rule.parse_prerequisites(&after_targets, separator_pos, stmt)?;

        if stmt.sep == RuleSep::Semicolon {
            rule.cmds.push(stmt.rhs.clone().unwrap());
        }

        for o in &rule.outputs {
            if o == &self.posix_sym {
                self.is_posix = true;
            }
        }

        // sarun: GNU make expands shell wildcards in target and prerequisite
        // lists when the makefile is read — e.g. the Linux kernel's
        //   xen-hypercalls.h: … $(srctree)/include/xen/interface/xen*.h
        // Expand any word containing glob metacharacters against the logical
        // working dir; a pattern with no matches stays literal (GNU behavior).
        self.expand_rule_globs(&mut rule);

        log!("Rule: {:?}", rule);
        match self.get_allow_rules()? {
            RulesAllowed::Warning => {
                warn_loc!(
                    self.loc.as_ref(),
                    "warning: Rule not allowed here for target: {}",
                    Evaluator::format_rule_error(&before_term)
                );
            }
            RulesAllowed::Error => {
                error_loc!(
                    self.loc.as_ref(),
                    "*** Rule not allowed here for target: {}",
                    Evaluator::format_rule_error(&before_term),
                );
            }
            RulesAllowed::Allowed => {}
        }
        self.rules.push(rule);
        self.in_rule = true;
        Ok(())
    }

    /// GNU make wildcard expansion for rule target / prerequisite words (NOT
    /// %-patterns — those lists are left alone unless a word carries actual
    /// glob metacharacters). Matches replace the word in place, sorted (libc
    /// glob's default, same as GNU); a non-matching pattern stays literal.
    fn expand_rule_globs(&self, rule: &mut crate::rule::Rule) {
        fn has_meta(b: &[u8]) -> bool {
            b.iter().any(|c| matches!(c, b'*' | b'?' | b'['))
        }
        let expand = |list: &mut Vec<Symbol>| {
            if !list.iter().any(|s| has_meta(&s.as_bytes())) {
                return;
            }
            let mut out: Vec<Symbol> = Vec::with_capacity(list.len());
            for s in list.iter() {
                let b = s.as_bytes();
                if !has_meta(&b) {
                    out.push(*s);
                    continue;
                }
                let files = crate::fileutil::glob(b.clone(), &self.working_dir);
                match files.as_ref() {
                    Ok(v) if !v.is_empty() => {
                        for f in v {
                            out.push(intern(f.clone()));
                        }
                    }
                    _ => out.push(*s),
                }
            }
            *list = out;
        };
        expand(&mut rule.outputs);
        expand(&mut rule.inputs);
        expand(&mut rule.order_only_inputs);
    }

    pub fn eval_command(&mut self, stmt: &CommandStmt) -> Result<()> {
        self.loc = Some(stmt.loc());

        if !self.in_rule {
            let stmts = parse_buf_no_stats(&stmt.orig(), stmt.loc())?;
            let stmts = stmts.lock();
            for a in &*stmts {
                a.eval(self)?;
            }
            return Ok(());
        }

        let last_rule = self.rules.last_mut().unwrap();
        last_rule.cmds.push(stmt.expr.clone());
        if last_rule.cmd_loc.is_none() {
            last_rule.cmd_loc = Some(stmt.loc());
        }
        log!("Command: {:?}", stmt.expr);

        Ok(())
    }

    pub fn eval_if(&mut self, stmt: &IfStmt) -> Result<()> {
        self.loc = Some(stmt.loc());

        let is_true = match stmt.op {
            CondOp::Ifdef | CondOp::Ifndef => {
                let var_name = stmt.lhs.eval_to_buf(self)?;
                let lhs = trim_right_space(&var_name);
                if lhs.iter().any(is_space_byte) {
                    error_loc!(self.loc.as_ref(), "*** invalid syntax in conditional.");
                }
                let lhs = intern(var_name.slice_ref(lhs));
                if let Some(v) = self.lookup_var_in_current_scope(lhs)? {
                    let v = v.read();
                    v.used(self, &lhs)?;
                    v.string()?.is_empty() == (stmt.op == CondOp::Ifndef)
                } else {
                    stmt.op == CondOp::Ifndef
                }
            }
            CondOp::Ifeq | CondOp::Ifneq => {
                let lhs = stmt.lhs.eval_to_buf(self)?;
                let rhs = stmt
                    .rhs
                    .as_ref()
                    .map(|v| v.eval_to_buf(self))
                    .unwrap_or_else(|| Ok(Bytes::new()))?;
                (lhs == rhs) == (stmt.op == CondOp::Ifeq)
            }
        };

        let stmts = match is_true {
            true => &stmt.true_stmts,
            false => &stmt.false_stmts,
        };
        let stmts = stmts.lock();
        for a in stmts.iter() {
            log!("{:?}", a);
            a.eval(self)?;
        }
        Ok(())
    }

    pub fn do_include(&mut self, fname: &Bytes) -> Result<()> {
        let filename = OsString::from_vec(fname.to_vec());
        collect_stats_with_slow_report!("included makefiles", &filename);

        let Some(mk) = file_cache::get_makefile(&filename, &self.working_dir)? else {
            error_loc!(
                self.loc.as_ref(),
                "{} does not exist",
                filename.to_string_lossy()
            );
        };

        let v = fname.slice_ref(trim_leading_curdir(fname));
        if let Some(var_list) = self.lookup_var(*MAKEFILE_LIST)? {
            var_list.write().append_str(&v, self.current_frame())?;
        } else {
            self.set_global_var(
                *MAKEFILE_LIST,
                Variable::with_simple_string(
                    v,
                    VarOrigin::File,
                    Some(self.current_frame()),
                    self.loc.clone(),
                ),
                false,
                None,
            )?;
        }
        for stmt in mk.stmts.lock().iter() {
            log!("{stmt:?}");
            stmt.eval(self)?;
        }

        if !self.profiled_files.is_empty() {
            for mk in std::mem::take(&mut self.profiled_files) {
                STATS.mark_interesting(mk);
            }
        }
        Ok(())
    }

    pub fn eval_include(&mut self, stmt: &IncludeStmt) -> Result<()> {
        self.loc = Some(stmt.loc());
        self.in_rule = false;

        let pats = stmt.expr.eval_to_buf(self)?;
        for pat in word_scanner(&pats) {
            let pat = pats.slice_ref(pat);
            let mut files = crate::fileutil::glob(pat.clone(), &self.working_dir);

            // sarun: GNU make's -I / --include-dir search. When the file
            // isn't found relative to the working directory, try each
            // include_dir in order — the kernel build relies on this
            // (MAKEFLAGS += --include-dir=$(abs_srctree)) for sub-makes
            // whose working dir differs from the source tree.
            if !std::path::Path::new(std::ffi::OsStr::from_bytes(&pat)).is_absolute() {
                let missing = match files.as_ref() {
                    Err(_) => true,
                    Ok(v) => v.is_empty(),
                };
                if missing {
                    for idir in &self.include_dirs {
                        let try_files = crate::fileutil::glob(pat.clone(), idir);
                        let found = match try_files.as_ref() {
                            Err(_) => false,
                            Ok(v) => !v.is_empty(),
                        };
                        if found {
                            // glob() strips the base prefix, returning
                            // relative paths. Resolve them against the
                            // include_dir so do_include opens the right file
                            // (not working_dir).
                            let idir_bytes = idir.as_os_str().as_bytes();
                            if let Ok(v) = try_files.as_ref() {
                                files = std::sync::Arc::new(Ok(v.iter()
                                    .map(|f| {
                                        let mut abs = bytes::BytesMut::with_capacity(
                                            idir_bytes.len() + 1 + f.len(),
                                        );
                                        abs.extend_from_slice(idir_bytes);
                                        abs.extend_from_slice(b"/");
                                        abs.extend_from_slice(f);
                                        abs.freeze()
                                    })
                                    .collect()));
                            }
                            break;
                        }
                    }
                }
            }

            if stmt.should_exist {
                let missing = match files.as_ref() {
                    Err(_) => true,
                    Ok(v) => v.is_empty(),
                };
                if missing {
                    let loc = self.loc.clone().unwrap_or_default();
                    self.pending_remake_includes
                        .push((loc, OsString::from_vec(pat.to_vec())));
                    continue;
                }
            }
            let Ok(files) = files.as_ref() else {
                continue;
            };

            for fname in files {
                if !stmt.should_exist
                    && FLAGS
                        .ignore_optional_include_pattern
                        .as_ref()
                        .map(|p| p.matches(fname))
                        .unwrap_or(false)
                {
                    continue;
                }

                {
                    let _frame = self.enter(FrameType::Parse, fname.clone(), stmt.loc());
                    self.do_include(fname)
                        .with_context(|| format!("In file included from {}:", stmt.loc()))?;
                }
            }
        }

        Ok(())
    }

    /// `vpath PATTERN DIRS` appends a pattern-scoped search entry;
    /// `vpath PATTERN` clears that pattern's entries; bare `vpath` clears
    /// everything. DIRS split on colons and whitespace, like GNU.
    pub fn eval_vpath(&mut self, stmt: &crate::stmt::VpathStmt) -> Result<()> {
        self.loc = Some(stmt.loc());
        self.in_rule = false;
        let line = stmt.expr.eval_to_buf(self)?;
        let mut words = word_scanner(&line);
        let Some(pat) = words.next() else {
            self.vpath_patterns.clear();
            return Ok(());
        };
        let pat = line.slice_ref(pat);
        let mut dirs: Vec<Bytes> = vec![];
        for w in words {
            for part in w.split(|&b| b == b':').filter(|p| !p.is_empty()) {
                dirs.push(line.slice_ref(part));
            }
        }
        if dirs.is_empty() {
            self.vpath_patterns.retain(|(p, _)| p != &pat);
        } else {
            self.vpath_patterns.push((pat, dirs));
        }
        Ok(())
    }

    pub fn eval_undefine(&mut self, stmt: &UndefineStmt) -> Result<()> {
        self.loc = Some(stmt.loc());
        self.in_rule = false;
        let names = stmt.expr.eval_to_buf(self)?;
        for tok in word_scanner(&names) {
            let sym = intern(names.slice_ref(tok));
            // Mirrors set_global_var's command-line-override rule: a plain
            // `undefine` won't unset a command-line variable, but
            // `override undefine` will.
            if let Some(prev) = self.peek_global_var(sym)
                && !stmt.is_override
                && matches!(
                    prev.read().origin(),
                    VarOrigin::CommandLine | VarOrigin::Override
                )
            {
                continue;
            }
            self.clear_global_var(sym);
        }
        Ok(())
    }

    pub fn eval_export(&mut self, stmt: &ExportStmt) -> Result<()> {
        self.loc = Some(stmt.loc());
        self.in_rule = false;

        let exports = stmt.expr.eval_to_buf(self)?;
        for tok in word_scanner(&exports) {
            let equal_index = memchr(b'=', tok);
            let lhs;
            if equal_index == Some(0)
                || (equal_index == Some(1)
                    && (tok.starts_with(b":") || tok.starts_with(b"?") || tok.starts_with(b"+")))
            {
                // Do not export tokens after an assignment.
                break;
            } else if let Some(equal_index) = equal_index {
                let assign = parse_assign_statement(tok, equal_index);
                lhs = assign.lhs;
            } else {
                lhs = tok;
            }
            let sym = intern(exports.slice_ref(lhs));
            self.exports.insert(sym, stmt.is_export);

            let prefix = if stmt.is_export { "" } else { "un" };
            match &self.export_allowed {
                ExportAllowed::Allowed => {}
                ExportAllowed::Error(msg) => error_loc!(
                    self.loc.as_ref(),
                    "*** {sym}: {prefix}export is obsolete{msg}."
                ),
                ExportAllowed::Warning(msg) => warn_loc!(
                    self.loc.as_ref(),
                    "{sym}: {prefix}export has been deprecated{msg}."
                ),
            }
        }
        Ok(())
    }

    pub fn lookup_var_global(&self, name: Symbol) -> Option<Var> {
        let v = self.get_global_var(name);
        if v.is_none() {
            USED_UNDEFINED_VARS.lock().insert(name);
        }
        v
    }

    pub fn is_traced(&self, name: &Symbol) -> bool {
        if self.assignment_tracefile.is_none() {
            return false;
        }

        // trace every variable unless filtered
        if FLAGS.traced_variables_pattern.is_empty() {
            return true;
        }

        let name = name.as_bytes();
        for pat in FLAGS.traced_variables_pattern.iter() {
            if pat.matches(&name) {
                return true;
            }
        }
        false
    }

    pub fn trace_variable_lookup(
        &mut self,
        operation: &'static str,
        name: &Symbol,
        var: &Option<Var>,
    ) -> Result<()> {
        if !self.is_traced(name) {
            return Ok(());
        }
        let frame = {
            let f = self.current_frame();
            let mut desc = String::from_utf8_lossy(&f.name).into_owned();
            if let Some(loc) = &f.location {
                desc = format!("{desc} @ {loc}");
            }
            json_escape(&desc)
        };
        let body = format!(
            "\"name\": \"{name}\", \"op\": \"{operation}\", \
             \"defined\": {}, \"frame\": \"{frame}\"",
            var.is_some());
        self.trace_emit(body)
    }

    pub fn trace_variable_assign(&mut self, name: &Symbol, var: &Var) -> Result<()> {
        if !self.is_traced(name) {
            return Ok(());
        }
        let (loc, value) = {
            let g = var.read();
            let loc = g.loc().as_ref().map(|l| l.to_string()).unwrap_or_default();
            let val = g.string().unwrap_or(std::borrow::Cow::Borrowed(b"?"));
            // Truncate huge values: a single write() stays line-atomic on
            // pipes only up to PIPE_BUF, and nobody debugs a 100KB value by
            // reading it whole.
            let mut vs = String::from_utf8_lossy(&val).into_owned();
            if vs.len() > 2000 {
                vs.truncate(2000);
                vs.push_str("…");
            }
            (json_escape(&loc), json_escape(&vs))
        };
        let body = format!(
            "\"name\": \"{name}\", \"op\": \"assign\", \
             \"loc\": \"{loc}\", \"value\": \"{value}\"");
        self.trace_emit(body)
    }

    pub fn lookup_var_for_eval(&mut self, name: Symbol) -> Result<Option<Var>> {
        if let Some(var) = self.lookup_var(name)? {
            if self.symbols_for_eval.contains(&name) {
                error_loc!(
                    var.read().loc().as_ref(),
                    "*** Recursive variable \"{name}\" references itself (eventually)."
                );
            }
            self.symbols_for_eval.insert(name);
            return Ok(Some(var));
        }
        Ok(None)
    }

    pub fn var_eval_complete(&mut self, name: Symbol) {
        self.symbols_for_eval.remove(&name);
    }

    pub fn lookup_var(&mut self, name: Symbol) -> Result<Option<Var>> {
        let mut result = None;

        if let Some(current_scope) = &self.current_scope {
            result = current_scope.lookup(name);
        }

        if result.is_none() {
            result = self.lookup_var_global(name);
        }

        self.trace_variable_lookup("lookup", &name, &result)?;
        Ok(result)
    }

    pub fn peek_var(&self, name: Symbol) -> Option<Var> {
        let mut result = None;

        if let Some(current_scope) = &self.current_scope {
            result = current_scope.peek(name);
        }

        if result.is_none() {
            result = self.peek_global_var(name);
        }

        result
    }

    pub fn lookup_var_in_current_scope(&mut self, name: Symbol) -> Result<Option<Var>> {
        let result = if let Some(current_scope) = &self.current_scope {
            current_scope.lookup(name)
        } else {
            self.lookup_var_global(name)
        };

        self.trace_variable_lookup("scope lookup", &name, &result)?;
        Ok(result)
    }

    pub fn peek_var_in_current_scope(&self, name: Symbol) -> Option<Var> {
        if let Some(current_scope) = &self.current_scope {
            current_scope.peek(name)
        } else {
            self.peek_global_var(name)
        }
    }

    pub fn eval_var(&mut self, name: Symbol) -> Result<Bytes> {
        if let Some(var) = self.lookup_var(name)? {
            var.read().eval_to_buf(self)
        } else {
            Ok(Bytes::new())
        }
    }

    pub fn enter(&mut self, frame_type: FrameType, name: Bytes, loc: Loc) -> ScopedFrame {
        if !self.trace {
            return ScopedFrame::new(self.stack.clone(), None);
        }

        let parent = self.stack.lock().last().cloned();
        let frame = Frame::new(frame_type, parent, Some(loc), name);
        ScopedFrame::new(self.stack.clone(), Some(Arc::new(frame)))
    }

    pub fn get_shell(&mut self) -> Result<Bytes> {
        self.eval_var(*SHELL_SYM)
    }

    pub fn get_shell_flag(&self) -> &'static [u8] {
        if self.is_posix { b"-ec" } else { b"-c" }
    }

    fn get_allow_rules(&mut self) -> Result<RulesAllowed> {
        Ok(match self.eval_var(*ALLOW_RULES_SYM)?.as_ref() {
            b"warning" => RulesAllowed::Warning,
            b"error" => RulesAllowed::Error,
            _ => RulesAllowed::Allowed,
        })
    }

    pub fn dump_include_json(&self, filename: &OsStr) -> Result<()> {
        let mut graph = IncludeGraph::new();
        graph.merge_tree_node(self.stack.lock().first().unwrap());
        let mut w: Box<dyn std::io::Write> = if filename == OsStr::new("-") {
            Box::new(std::io::stdout())
        } else {
            let f = std::fs::File::create(filename)?;
            Box::new(BufWriter::new(f))
        };

        graph.dump_json(&mut w)?;
        Ok(())
    }

    pub fn used_undefined_vars() -> HashSet<Symbol> {
        USED_UNDEFINED_VARS.lock().clone()
    }
}
